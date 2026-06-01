from __future__ import annotations

import asyncio
import hashlib
import json
import logging
from dataclasses import asdict
from typing import Any, Literal
from urllib.parse import urlparse

from rapidfuzz import fuzz

from .cache import cache
from .config import settings
from .engines import ENGINES, SearchFilters, SearchResult, get_engine
from .ratelimit import RateLimiter

log = logging.getLogger(__name__)
search_limiter = RateLimiter(settings.rate_limit_per_minute)


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


def _dedup_by_title(items: list[dict]) -> list[dict]:
    """Remove near-duplicate titles on the same canonical host.

    Catches the cases URL-only dedup misses: bbc.com/news/x vs bbc.co.uk/news/x,
    and amp.example.com/x vs www.example.com/x where the two URLs differ but
    point at the same story. Different hosts with the same title (e.g. wire
    stories on Reuters and AP) are kept — those are legitimately distinct
    sources.
    """
    keep: list[dict] = []
    for it in items:
        t = (it.get("title") or "").lower().strip()
        if not t:
            keep.append(it)
            continue
        host = _canonical_host(it.get("url", ""))
        is_dup = any(
            fuzz.token_set_ratio(t, (k.get("title") or "").lower()) >= 92
            and _canonical_host(k.get("url", "")) == host
            for k in keep
        )
        if not is_dup:
            keep.append(it)
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
            return f"According to {host}: {sn}"
    return None


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
            if url not in representative or len(r.snippet) > len(representative[url].get("snippet", "")):
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
    category: Literal["news", "pdf", "github", "paper", "forum", "blog"] | None = None,
    include_text: str | None = None,
    exclude_text: str | None = None,
) -> dict[str, Any]:
    engine_names = list(engines) if engines else list(settings.default_engines)
    # Smart routing: news category benefits enormously from the Google News RSS
    # engine. If the user didn't explicitly choose engines and asked for news,
    # add googlenews to the pool — RSS items have structured pubDates and the
    # index is overwhelmingly news outlets.
    if category == "news" and engines is None and "googlenews" not in engine_names:
        engine_names.append("googlenews")
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

    # Shared accumulator the engines populate with raw/filtered counts and
    # per-reason drop tallies. Only built when filters are non-default —
    # diagnostics are pure overhead on the happy path.
    diagnostics: dict[str, Any] | None = None if filters.is_empty() else {}

    async def run(name: str) -> tuple[str, list[SearchResult] | Exception]:
        try:
            engine = get_engine(name)
        except ValueError as e:
            return name, e
        await search_limiter.acquire(name)
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

    # Surface diagnostics ONLY when (a) the user actually set a filter, AND
    # (b) the final result set is sparse. Otherwise omit the field entirely
    # so happy-path output stays clean.
    if diagnostics is not None and len(merged) <= 3:
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

    return payload


def list_engines() -> list[str]:
    return list(ENGINES.keys())
