from __future__ import annotations

import asyncio
import hashlib
import json
import logging
import re
from dataclasses import asdict
from typing import Any, Literal
from urllib.parse import urlparse

from rapidfuzz import fuzz

from .browser import BROWSER_INSTALL_HINT
from .cache import cache
from .config import settings
from .engines import ENGINES, Category, Engine, SearchFilters, SearchResult, get_engine
from .ratelimit import RateLimiter

log = logging.getLogger(__name__)
search_limiter = RateLimiter(settings.rate_limit_per_minute)

# Engines that publish a stricter limit than our global default get their own
# bucket. GDELT, for one, answers 429 with "limit requests to one every 5
# seconds" — the default 30/min would walk straight into that.
for _name, _engine in ENGINES.items():
    if _engine.rate_limit_per_minute is not None:
        search_limiter.configure(_name, _engine.rate_limit_per_minute)


def _max_token_wait(engine: Any) -> float | None:
    """Seconds this engine is willing to queue for a rate-limit token.

    getattr rather than attribute access: tests substitute duck-typed engine
    stubs that don't inherit from `Engine`, and a missing attribute should mean
    "wait as long as needed" (the historical behavior), not a crash.
    """
    return getattr(engine, "rate_limit_max_wait", None)


# Categories whose specialist engines REPLACE the default pool rather than
# augment it — see aggregate_search.
_EXCLUSIVE_CATEGORIES = frozenset({"image", "dataset"})


def engines_for_category(
    category: str | None, exclude: list[str] | None = None
) -> list[str]:
    """Engines that natively index `category`, in registry order.

    The default pool is four general web engines; they can only honour a
    `category` by discarding results whose hostname isn't on a whitelist. An
    engine that declares the category actually searches it, so pull those in
    when the caller asked for a category but not for specific engines.

    Capped by `settings.category_engine_limit`: every added engine is another
    round trip on the critical path, and the specialist sources are ordered
    best-first in the registry.
    """
    if not category:
        return []
    already = set(exclude or ())
    picks = [
        name
        for name, engine in ENGINES.items()
        if category in engine.categories
        and name not in already
        and engine.is_available()
    ]
    limit = settings.category_engine_limit
    if limit >= 0:
        dropped = picks[limit:]
        picks = picks[:limit]
        if dropped:
            # Say so rather than silently truncating — an operator wondering
            # why arxiv never runs needs this in the log, not in the source.
            log.info(
                "category %r: using %s, over category_engine_limit=%d (skipped %s)",
                category, picks, limit, dropped,
            )
    return picks


def _normalize_url(url: str) -> str:
    if url.startswith("//"):
        url = "https:" + url
    return url.split("#", 1)[0].rstrip("/")


_HOST_PREFIXES = ("www.", "m.", "amp.", "mobile.")
# Country-coded TLDs we collapse to ".com" so bbc.co.uk and bbc.com look the
# same to the dedup pass. We never strip generic TLDs (.com, .org, .net) —
# only the country variants that syndicators reuse.
_TLD_NORMALIZE = (".co.uk", ".co.jp", ".com.au", ".co.in")


def _canonical_host(url: str) -> str:
    """Strip mobile/AMP prefixes and collapse country-TLDs to a single key.

    Doesn't change the URL we keep — just used as a dedup signal alongside
    title fuzzy match.
    """
    h = (urlparse(url).hostname or "").lower()
    for p in _HOST_PREFIXES:
        if h.startswith(p):
            h = h[len(p):]
            break
    for tld in _TLD_NORMALIZE:
        if h.endswith(tld):
            h = h[: -len(tld)] + ".com"
            break
    return h


_NUM_RE = re.compile(r"\d+")


def _dedup_by_title(items: list[dict]) -> list[dict]:
    """Remove near-duplicate titles on the same canonical host.

    Catches the cases URL-only dedup misses: bbc.com/news/x vs bbc.co.uk/news/x,
    and amp.example.com/x vs www.example.com/x where the two URLs differ but
    point at the same story. Different hosts with the same title (e.g. wire
    stories on Reuters and AP) are kept — those are legitimately distinct
    sources.

    Numeric guard: two same-host titles whose digit-tokens differ (e.g. "Python
    3.13 released" vs "Python 3.12 released", "iPhone 15" vs "iPhone 14", "...25
    basis points" vs "...50 basis points") are kept as DISTINCT, because the
    fuzzy ratio alone scores those >=92 and would silently drop a real, separate
    result. Version/year/quantity differences are meaningful, not syndication
    noise.
    """
    keep: list[dict] = []
    # (canonical_host, digit_tokens, lowered_title) per kept item, computed
    # once — the inner loop otherwise re-parses every kept URL and re-scans
    # every kept title for each new candidate (O(n²) urlparse/regex calls).
    keep_keys: list[tuple[str, list[str], str]] = []
    for it in items:
        t = (it.get("title") or "").lower().strip()
        if not t:
            keep.append(it)
            keep_keys.append(("", [], ""))
            continue
        host = _canonical_host(it.get("url", ""))
        t_nums = _NUM_RE.findall(t)
        is_dup = False
        for k_host, k_nums, kt in keep_keys:
            if not kt or k_host != host:
                continue
            # Distinct digit-tokens => distinct results; never collapse them.
            if k_nums != t_nums:
                continue
            if fuzz.token_set_ratio(t, kt) >= 92:
                is_dup = True
                break
        if not is_dup:
            keep.append(it)
            keep_keys.append((host, t_nums, t))
    return keep


def _is_cjk(c: str) -> bool:
    o = ord(c)
    return (
        0x4E00 <= o <= 0x9FFF       # CJK unified ideographs
        or 0x3040 <= o <= 0x30FF    # Japanese hiragana/katakana
        or 0xAC00 <= o <= 0xD7A3    # Korean hangul syllables
    )


def _lead_query_terms(query: str) -> set[str]:
    """Tokenize a query for snippet-substring matching.

    Pure-ASCII tokens: keep when len > 3 (skip "the", "vs", "of"...).
    CJK tokens: extract char-bigrams ("模型架构" -> {"模型","型架","架构"})
    so we still match when the snippet splits the term into "模型" and
    "架构" separately rather than emitting the whole 4-char run.
    Mixed-script tokens are included as-is when they contain a length-3+ ASCII
    portion or any CJK at all.
    """
    terms: set[str] = set()
    for tok in query.split():
        cjk_chars = [c for c in tok if _is_cjk(c)]
        if len(cjk_chars) >= 2:
            for i in range(len(cjk_chars) - 1):
                terms.add(cjk_chars[i] + cjk_chars[i + 1])
        elif len(cjk_chars) == 1:
            # Single CJK char alone is too generic; skip.
            pass
        elif len(tok) > 3:
            terms.add(tok.lower())
    return terms


def _lead_snippet(query: str, results: list[dict]) -> str | None:
    """Pick an honest extractive lead from the top-3 results.

    Requires the snippet to contain >=2 query terms and be >=80 chars — short
    enough to skip filler titles, long enough to actually answer something.
    Prefixed with the host so the model sees the source inline. NOT an LLM
    answer; if no snippet qualifies we return None and the renderer skips the
    lead block entirely.

    Term tokenization is CJK-aware (see ``_lead_query_terms``).
    """
    qterms = _lead_query_terms(query)
    if not qterms:
        return None
    for r in results[:3]:
        sn = (r.get("snippet") or "").strip()
        if not sn or len(sn) < 80:
            continue
        sn_lower = sn.lower()
        hits = sum(1 for t in qterms if t in sn_lower)
        # Single-term queries (e.g. "python", "ai") can never satisfy hits>=2,
        # so cap the requirement at the number of terms we actually have.
        if hits >= min(2, len(qterms)):
            host = (urlparse(r.get("url", "")).hostname or "")
            if host.startswith("www."):
                host = host[4:]
            # GoogleNews items carry an opaque news.google.com redirect URL, but
            # the real outlet is appended to the title as "(Reuters)". Attribute
            # the lead to that outlet instead of "news.google.com", which is
            # never the actual source.
            if host == "news.google.com":
                outlet = _outlet_from_gnews_title(r.get("title", ""))
                if outlet:
                    host = outlet
            return f"According to {host}: {sn}"
    return None


# GoogleNews display titles end with the outlet in parens: "Headline (Reuters)".
_GNEWS_OUTLET_RE = re.compile(r"\(([^()]+)\)\s*$")


def _outlet_from_gnews_title(title: str) -> str:
    """Extract the trailing "(Outlet)" name a GoogleNews title carries, or ""."""
    m = _GNEWS_OUTLET_RE.search(title or "")
    return m.group(1).strip() if m else ""


# Human-readable labels for the drop-reason keys we surface to the LLM.
# Kept here (not in base) so the rendering text stays close to the aggregator
# that emits it.
_DROP_REASON_LABEL: dict[str, str] = {
    "include_domains": "include_domains",
    "exclude_domains": "exclude_domains",
    "include_text": "include_text",
    "exclude_text": "exclude_text",
    "category_paper": "category=paper",
    "category_forum": "category=forum",
    "category_github": "category=github",
    "category_news": "category=news",
    "category_pdf": "category=pdf",
    "category_blog": "category=blog",
}


def _filter_hint(drops: dict[str, int], raw_total: int, kept_total: int) -> str:
    """One-sentence actionable explanation for a sparse result set.

    Names the single highest-dropping filter so the LLM knows which knob is
    most worth relaxing.
    """
    if not drops:
        # Nothing was dropped client-side — the engines themselves returned
        # almost nothing, so widening filters won't help.
        return (
            f"Engines returned only {raw_total} raw results (none dropped by filters). "
            "Try a broader query or different engines."
        )
    top_reason, top_n = max(drops.items(), key=lambda kv: kv[1])
    label = _DROP_REASON_LABEL.get(top_reason, top_reason)
    dropped_total = sum(drops.values())
    return (
        f"Filters dropped {dropped_total} of {raw_total} raw results "
        f"(kept {kept_total}). Most were excluded by {label}. "
        "Try widening or removing one filter."
    )


def _gate_hint(gated: dict[str, str], fallback: dict[str, str]) -> str:
    """One-line explanation of which engines were gated (CAPTCHA/consent/login,
    or a missing browser) and how each was handled (rescue, or nothing).

    Remedies are per-cause: proxy advice only when a real gate (a remote wall)
    was hit; the canonical install hint only when a browser was missing —
    telling someone to configure a proxy for a missing binary is noise.
    """
    parts: list[str] = []
    browser_missing = False
    real_gates = False
    for name in sorted(set(gated) | set(fallback)):
        reason = gated.get(name, "gated")
        via = fallback.get(name)
        if reason == "browser_unavailable":
            browser_missing = True
            desc = f"{name} needed a browser render that is unavailable"
        else:
            real_gates = True
            desc = f"{name} was {reason}-gated"
        if via:
            desc += f" → served via {via}"
        elif reason != "browser_unavailable":
            desc += " (no results)"
        parts.append(desc)
    hint = "; ".join(parts) + "."
    if real_gates:
        hint += (
            " Configure a proxy (admin UI / SEARCH_MCP_PROXY) to route through "
            "a non-blocked IP, or rely on the keyless default engines."
        )
    if browser_missing:
        hint += " " + BROWSER_INSTALL_HINT
    return hint


def _needs_rescue(
    merged: list[dict[str, Any]], errors: dict[str, str], diagnostics: dict[str, Any]
) -> bool:
    """Decide whether the keyless rescue pass should run.

    Triggers only when the run is empty, or sparse (<=3 — the SAME threshold
    the empty-engine and filter hints use, so a run is never simultaneously
    "sparse enough to warn about" and "too healthy to rescue") AND
    demonstrably unhealthy: an engine errored, hit a gate, or silently
    returned zero. A healthy niche query that legitimately yields a few
    results must NOT trigger network work — the normal-path latency guarantee.
    """
    if not settings.rescue_enabled:
        return False
    if len(merged) == 0:
        return True
    if len(merged) > 3:
        return False
    raw = diagnostics.get("raw_per_engine", {})
    return bool(errors) or bool(diagnostics.get("gated")) or any(
        count == 0 for count in raw.values()
    )


async def _rescue(
    query: str,
    n: int,
    filters: SearchFilters,
    engine_names: list[str],
    diagnostics: dict[str, Any],
) -> tuple[list[SearchResult], str | None]:
    """One bounded keyless recovery pass via ``settings.rescue_engines``.

    Sequential, first engine that yields results wins. Each candidate gets an
    equal slice of ``settings.rescue_timeout`` (rate-limiter wait included),
    so a slow first candidate (searx races public instances) can never starve
    a fast later one. Calls the engines directly — never re-enters
    ``aggregate_search`` — and the candidate list excludes engines the caller
    already ran, so there is no recursion and no self-rescue.

    Each candidate runs with a PRIVATE diagnostics dict: rescue probes must
    never leak into the caller-facing per-engine stats (``empty_engines`` /
    ``gated_engines`` describe engines the caller asked for). The attempt
    summary — including any gates the probes hit — lives under
    ``diagnostics["rescue"]``. Returns ``(results, served_by)``; never raises.
    """
    candidates = [e for e in settings.rescue_engines if e not in engine_names]
    if not candidates:
        return [], None
    attempted: list[str] = []
    info: dict[str, Any] = {"attempted": attempted}
    diagnostics["rescue"] = info
    per_candidate = settings.rescue_timeout / len(candidates)

    for name in candidates:
        attempted.append(name)
        try:
            engine = get_engine(name)
        except ValueError:
            continue
        rescue_diag: dict[str, Any] = {}

        async def _one(
            name: str = name,
            engine: Engine = engine,
            rescue_diag: dict[str, Any] = rescue_diag,
        ) -> list[SearchResult]:
            if not await search_limiter.acquire(name, max_wait=_max_token_wait(engine)):
                # Rescue is already a bounded, best-effort recovery attempt;
                # burning its timeout budget queueing for a token would starve
                # the remaining candidates.
                return []
            return await engine.search(query, n, filters, diagnostics=rescue_diag)

        try:
            results = await asyncio.wait_for(_one(), timeout=per_candidate)
        except TimeoutError:
            info.setdefault("timeouts", []).append(name)
            continue
        except Exception as e:
            log.warning("rescue engine %s failed: %s", name, e)
            continue
        if rescue_diag.get("gated"):
            info.setdefault("gated", {}).update(rescue_diag["gated"])
        if results:
            info["served_by"] = name
            info["results"] = len(results)
            return results, name
    return [], None


def _key(query: str, engines: list[str], max_results: int, filters: SearchFilters) -> str:
    raw = json.dumps(
        {
            "q": query,
            "e": sorted(engines),
            "n": max_results,
            "f": asdict(filters),
        },
        ensure_ascii=False,
        sort_keys=True,
    )
    return hashlib.sha256(raw.encode("utf-8")).hexdigest()


def _merge(buckets: list[list[SearchResult]], max_results: int) -> list[dict[str, Any]]:
    """Reciprocal-rank-fusion across engines: same URL appearing high in multiple
    engines wins. Stable, no scoring magic, well-known IR technique."""
    k = 60.0
    scores: dict[str, float] = {}
    representative: dict[str, dict[str, Any]] = {}
    engines_for: dict[str, list[str]] = {}

    for bucket in buckets:
        for r in bucket:
            url = _normalize_url(r.url)
            if not url:
                continue
            scores[url] = scores.get(url, 0.0) + 1.0 / (k + r.rank)
            engines_for.setdefault(url, []).append(r.engine)
            if url not in representative or len(r.snippet) > len(
                representative[url].get("snippet", "")
            ):
                representative[url] = r.to_dict()
                representative[url]["url"] = url

    ranked = sorted(scores.items(), key=lambda kv: kv[1], reverse=True)
    out_full = []
    for url, score in ranked:
        rec = representative[url]
        rec["engines"] = sorted(set(engines_for[url]))
        rec["score"] = round(score, 5)
        rec.pop("rank", None)
        rec.pop("engine", None)
        # `published_age` (when present) flows through automatically via
        # SearchResult.to_dict(); we drop the empty-string default so the
        # field is absent from output rather than noisy.
        if not rec.get("published_age"):
            rec.pop("published_age", None)
        out_full.append(rec)
    # URL-keyed RRF already collapsed exact-URL dupes; this second pass kills
    # the cross-host syndication and AMP/mobile variants the URL key misses.
    # Dedup over the FULL ranked list BEFORE slicing so a title-duplicate inside
    # the top-N is backfilled by the next unique result instead of leaving the
    # caller short of max_results (#7).
    return _dedup_by_title(out_full)[:max_results]


async def aggregate_search(
    query: str,
    engines: list[str] | None = None,
    max_results: int | None = None,
    use_cache: bool = True,
    *,
    max_age_seconds: int | None = None,
    freshness: Literal["day", "week", "month", "year"] | None = None,
    include_domains: list[str] | None = None,
    exclude_domains: list[str] | None = None,
    category: Category | None = None,
    include_text: str | None = None,
    exclude_text: str | None = None,
) -> dict[str, Any]:
    if engines:
        engine_names = list(engines)
    elif category in _EXCLUSIVE_CATEGORIES:
        # For these, the general web pool is noise rather than coverage: a web
        # engine cannot return an image file or a dataset record, so mixing it
        # in only crowds out the sources that can. Falls back to the default
        # pool if no specialist is available.
        engine_names = engines_for_category(category) or list(settings.default_engines)
    else:
        engine_names = list(settings.default_engines)
        engine_names.extend(engines_for_category(category, exclude=engine_names))
    n = max_results or settings.max_results_per_engine
    filters = SearchFilters(
        freshness=freshness,
        include_domains=list(include_domains) if include_domains else [],
        exclude_domains=list(exclude_domains) if exclude_domains else [],
        category=category,
        include_text=include_text,
        exclude_text=exclude_text,
    )
    cache_key = _key(query, engine_names, n, filters)

    # Read-bypass and cache-WRITE are decoupled. `use_cache` gates BOTH the read
    # and the write; `max_age_seconds` only tightens the read TTL. So a caller
    # passing max_age_seconds=0 (force-refresh) still writes the fresh result
    # back — caching is never silently disabled by a freshness request.
    #   max_age_seconds is None  -> read with the server default TTL.
    #   max_age_seconds == 0     -> always a read miss (force-refresh).
    #   max_age_seconds > 0      -> read only if the row is younger than that.
    if use_cache and max_age_seconds != 0:
        hit = await cache.get_search(cache_key, max_age_seconds=max_age_seconds)
        if hit:
            # A4: recompute lead_snippet from the cached results so the rendered
            # markdown keeps its '> **Lead:**' block. filter_diagnostics can't be
            # rebuilt from results alone (it needs the per-engine raw/drop tallies
            # that only exist on a fresh run), so it is intentionally fresh-only.
            return {
                "query": query,
                "engines": engine_names,
                "cached": True,
                "results": hit,
                "lead_snippet": _lead_snippet(query, hit),
            }

    # Shared accumulator the engines populate with raw/filtered counts, per-reason
    # drop tallies, and gate/fallback signals. Always built (cheap dict writes) so
    # gates (CAPTCHA/consent/login) are captured even on unfiltered queries.
    diagnostics: dict[str, Any] = {}

    async def run(name: str) -> tuple[str, list[SearchResult] | Exception]:
        try:
            engine = get_engine(name)
        except ValueError as e:
            return name, e
        if not await search_limiter.acquire(name, max_wait=_max_token_wait(engine)):
            # Strictly-limited source with no token to spare. Skipping keeps
            # the parallel fan-out at the speed of the other engines; say so
            # in diagnostics so an empty slot doesn't read as "found nothing".
            log.info("engine %s skipped: rate limit token unavailable", name)
            diagnostics.setdefault("rate_limited", []).append(name)
            return name, []
        try:
            return name, await engine.search(query, n, filters, diagnostics=diagnostics)
        except Exception as e:
            log.warning("engine %s failed: %s", name, e)
            return name, e

    results = await asyncio.gather(*(run(n) for n in engine_names))
    buckets: list[list[SearchResult]] = []
    errors: dict[str, str] = {}
    for name, res in results:
        if isinstance(res, Exception):
            errors[name] = str(res)
        else:
            buckets.append(res)

    merged = _merge(buckets, n)

    # Keyless rescue: one bounded recovery attempt when the run came back
    # empty or nearly-empty with demonstrably unhealthy engines. Rescue
    # results join the RRF merge (any partial default results keep their
    # weight and attribution stays honest via each result's `engines`), and
    # they flow into the cache write below like any other result — a repeat
    # query within TTL should not re-pay the rescue.
    rescued_via: str | None = None
    if _needs_rescue(merged, errors, diagnostics):
        rescue_bucket, rescued_via = await _rescue(
            query, n, filters, engine_names, diagnostics
        )
        if rescue_bucket:
            buckets.append(rescue_bucket)
            merged = _merge(buckets, n)
            # Attribute the recovery to the gated engines so the gate hint
            # reads "was captcha-gated → served via searx" instead of the
            # misleading "(no results)".
            if rescued_via:
                fb = diagnostics.setdefault("fallback", {})
                for name in diagnostics.get("gated", {}):
                    fb.setdefault(name, rescued_via)

    if use_cache and merged:
        await cache.put_search(cache_key, query, engine_names, merged)

    payload: dict[str, Any] = {
        "query": query,
        "engines": engine_names,
        "cached": False,
        "results": merged,
        "lead_snippet": _lead_snippet(query, merged),
        "errors": errors or None,
    }
    if rescued_via:
        payload["rescued_via"] = rescued_via

    # Surface filter diagnostics ONLY when (a) the user actually set a filter,
    # AND (b) the final result set is sparse. Otherwise omit the field entirely
    # so happy-path output stays clean.
    if not filters.is_empty() and len(merged) <= 3:
        raw_per_engine = diagnostics.get("raw_per_engine", {})
        after_per_engine = diagnostics.get("after_filter_per_engine", {})
        drops = diagnostics.get("drops_by_reason", {})
        raw_total = sum(raw_per_engine.values())
        payload["filter_diagnostics"] = {
            "raw_per_engine": raw_per_engine,
            "after_filter_per_engine": after_per_engine,
            "drops_by_reason": drops,
            "hint": _filter_hint(drops, raw_total, len(merged)),
        }

    # Surface gate/fallback signals (CAPTCHA / consent / login walls) so the
    # caller learns WHY an engine returned nothing — and whether a searx
    # fallback covered it — instead of seeing a silent gap.
    gated = diagnostics.get("gated") or {}
    fallback = diagnostics.get("fallback") or {}
    if gated or fallback:
        payload["gated_engines"] = {
            name: {"reason": gated.get(name, "gated"), "fallback": fallback.get(name)}
            for name in sorted(set(gated) | set(fallback))
        }
        payload["gated_hint"] = _gate_hint(gated, fallback)

    # Engines that returned 0 raw results with no exception and no detected
    # gate — the silent failure mode (IP block, markup drift) that otherwise
    # leaves no trace at all. Same sparseness threshold as filter_diagnostics
    # so a healthy response with one quiet engine stays clean.
    if len(merged) <= 3:
        # An engine whose HTTP call was refused (429/5xx) is NOT silent — it
        # told us exactly what happened and the keyless-JSON never-raise rule
        # swallowed it. Report those by status instead of sending the user off
        # to configure a proxy for what is usually a rate limit.
        http_status = diagnostics.get("http_status") or {}
        zero = [
            name
            for name, count in diagnostics.get("raw_per_engine", {}).items()
            if count == 0 and name not in gated and name not in errors
        ]
        refused = sorted(n for n in zero if n in http_status)
        empty = sorted(n for n in zero if n not in http_status)
        hints: list[str] = []
        if refused:
            detail = ", ".join(f"{n} (HTTP {http_status[n]})" for n in refused)
            hints.append(
                f"{detail} — the source refused the request rather than "
                "returning no matches. 429 means back off and retry later; "
                "5xx means the source is down."
            )
        if empty:
            hints.append(
                f"{', '.join(empty)} returned 0 results with no error and no "
                "CAPTCHA/consent wall detected — possible silent IP block or a "
                "markup change. If this persists, configure a proxy (admin UI / "
                "SEARCH_MCP_PROXY) or pick different engines via `engines=`."
            )
        if refused:
            payload["refused_engines"] = {n: http_status[n] for n in refused}
        if empty:
            payload["empty_engines"] = empty
        if hints:
            payload["empty_hint"] = " ".join(hints)

    return payload


def list_engines() -> list[str]:
    return list(ENGINES.keys())
