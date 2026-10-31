from __future__ import annotations

import asyncio
import hashlib
import json
import logging
import re
import time
import weakref
from collections.abc import Collection
from dataclasses import asdict
from datetime import UTC, datetime
from typing import Any, Literal
from urllib.parse import urlparse, urlsplit

from rapidfuzz import fuzz

from .browser import (
    BROWSER_INSTALL_HINT,
    BrowserLaunchError,
    BrowserUnavailableError,
    brief_error,
)
from .browser import pool as browser_pool
from .cache import cache
from .coherence import (  # noqa: F401
    # These two moved to coherence.py, which the Bing engine imports too; they
    # stay importable from here for evals/ranking/replay.py and the tests.
    _is_cjk,
    _lead_query_terms,
    bucket_coherence,
    is_witness,
    looks_like_decoy,
)
from .config import settings
from .engines import (
    ENGINES,
    Category,
    SearchFilters,
    SearchResult,
    category_group,
    get_engine,
)
from .engines.base import (
    EngineSignInPending,
    classify_source,
    detect_query_region,
    in_agent_lane,
    is_lasting,
)
from .health import SILENT_THRESHOLD, engine_health
from .ratelimit import RateLimiter

log = logging.getLogger(__name__)
search_limiter = RateLimiter(settings.rate_limit_per_minute)

# Engines that publish a stricter limit than our global default get their own
# bucket. GDELT, for one, answers 429 with "limit requests to one every 5
# seconds" — the default 30/min would walk straight into that.
for _name, _engine in ENGINES.items():
    if _engine.rate_limit_per_minute is not None:
        search_limiter.configure(
            _name, _engine.rate_limit_per_minute, _engine.rate_limit_burst
        )


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


def _is_exclusive(category: str | None) -> bool:
    """Whether `category`'s specialists replace the default web pool.

    Checks the FULL token before the group, so exclusivity can later be
    declared per sub-group (an `image.*` sub-group that should augment) without
    reopening this. A plain `category in _EXCLUSIVE_CATEGORIES` test silently
    dropped every dotted image/dataset token back into the augmenting branch,
    re-admitting the four web engines that exclusivity exists to keep out.
    """
    if not category:
        return False
    return (
        category in _EXCLUSIVE_CATEGORIES
        or category_group(category) in _EXCLUSIVE_CATEGORIES
    )


def engines_for_category(
    category: str | None, exclude: list[str] | None = None
) -> list[str]:
    """Engines that natively index `category`, in registry order.

    The default pool is three general web engines; they can only honour a
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
    if "." not in category:
        picks = _round_robin_by_subgroup(category, picks)
    limit = settings.category_engine_limit
    if limit >= 0:
        dropped = picks[limit:]
        picks = picks[:limit]
        if dropped:
            # Say so rather than silently truncating — an operator wondering
            # why arxiv never runs needs this in the log, not in the source.
            log.info(
                "category %r: using %s, over category_engine_limit=%d (skipped %s; "
                "name one explicitly with engines=[...] or ask for its sub-group)",
                category, picks, limit, dropped,
            )
    return picks


def _round_robin_by_subgroup(group: str, names: list[str]) -> list[str]:
    """Interleave a group's engines across its sub-groups before the cap bites.

    Registry order alone spends the whole `category_engine_limit` budget on
    whichever sub-group happens to sit first. `category="paper"` was the worked
    example: registry order gave `arxiv, openalex, crossref` and dropped
    `pubmed` entirely — while two of the three slots went to OpenAlex and
    Crossref, both DOI indexes with heavy overlap. Interleaving spends the same
    three round trips on three genuinely different corpora (a preprint server,
    a works index, a biomedical index) and, as a side effect, un-kills the
    engine four separate docs had been advertising for a category it could
    never run in.

    Only applies to a BARE group: a caller who asked for `paper.biomed` wants
    that sub-group's engines in registry order, not a spread.

    An engine that serves several sub-groups is bucketed under its
    alphabetically first one, so it is counted exactly once and the result stays
    deterministic.
    """
    prefix = group + "."
    buckets: dict[str, list[str]] = {}
    for name in names:
        subs = sorted(
            token[len(prefix):]
            for token in ENGINES[name].categories
            if token.startswith(prefix)
        )
        buckets.setdefault(subs[0] if subs else "", []).append(name)
    ordered: list[str] = []
    while any(buckets.values()):
        for bucket in buckets.values():
            if bucket:
                ordered.append(bucket.pop(0))
    return ordered


def _normalize_url(url: str) -> str:
    if url.startswith("//"):
        url = "https:" + url
    return url.split("#", 1)[0].rstrip("/")


def _url_key(url: str) -> str:
    """The identity of a page for rank fusion. A key — NEVER a URL to emit.

    `_normalize_url` alone let `http://arxiv.org/abs/1512.03385` (OpenAlex) and
    `https://arxiv.org/abs/1512.03385` (two web engines) score as two results:
    the agreement RRF exists to reward was split, and both copies were printed.
    So the key ignores what cannot change which page it is: scheme, a leading
    `www.`, host case, a default port, the fragment, a trailing slash.

    It stops there on purpose. `_canonical_host` also folds ccTLDs, which is
    right for a fuzzy title-dedup signal and wrong for identity —
    amazon.co.uk/dp/X and amazon.com/dp/X are different pages. Tracking
    parameters stay too: stripping them was measured and rejected (README).

    The result's `url` is always a real one, `_normalize_url(r.url)`. Replaying
    the capture with this key printed as the URL regressed two cases outright.
    """
    normalized = _normalize_url(url)
    try:
        parts = urlsplit(normalized)
        host = (parts.hostname or "").lower()
        port = parts.port
    except ValueError:
        return normalized
    if not host:
        return normalized
    key = host.removeprefix("www.")
    if port and port not in (80, 443):
        key += f":{port}"
    key += parts.path.rstrip("/")
    if parts.query:
        key += "?" + parts.query
    return key


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

    Numeric guard: two same-host titles whose digit-tokens BOTH exist and differ (e.g. "Python
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
            # Only when BOTH titles carry digits, though: "[1512.03385] Deep
            # Residual Learning..." and the same title without the arXiv id are
            # one paper (abs vs pdf), and comparing [] to a list of digits
            # called every such pair "distinct".
            if k_nums and t_nums and k_nums != t_nums:
                continue
            if fuzz.token_set_ratio(t, kt) >= 92:
                is_dup = True
                break
        if not is_dup:
            keep.append(it)
            keep_keys.append((host, t_nums, t))
    return keep


def _lead_snippet(query: str, results: list[dict]) -> str | None:
    """Pick an honest extractive lead from the top-3 results.

    Requires the snippet to contain >=2 query terms and be >=80 chars — short
    enough to skip filler titles, long enough to actually answer something.
    Prefixed with the host so the model sees the source inline. NOT an LLM
    answer; if no snippet qualifies we return None and the renderer skips the
    lead block entirely.

    Term tokenization is CJK-aware (see ``_lead_query_terms``).
    """
    # A record that a direct-answer engine looked up (see Engine.direct_answer)
    # and that won the top rank IS the answer, and it rarely echoes the words
    # of the question ("1万日元等于多少人民币" is answered by "10000 JPY =
    # 425.70 CNY"), so it is taken as the lead without the term test.
    if results and any(
        name in ENGINES and ENGINES[name].answers_directly(query)
        for name in results[0].get("engines") or []
    ):
        sn = (results[0].get("snippet") or "").strip()
        if sn:
            host = (urlparse(results[0].get("url", "")).hostname or "").removeprefix("www.")
            return f"According to {host}: {sn}"
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
    off_topic = False
    for name in sorted(set(gated) | set(fallback)):
        reason = gated.get(name, "gated")
        via = fallback.get(name)
        if reason == "browser_unavailable":
            browser_missing = True
            desc = f"{name} needed a browser render that is unavailable"
        elif reason == "off_topic":
            off_topic = True
            desc = (
                f"{name} answered with results that match only the first word of "
                "the query; they were discarded"
            )
        else:
            real_gates = True
            desc = f"{name} was {reason}-gated"
        if via:
            desc += f" → served via {via}"
        elif reason not in ("browser_unavailable", "off_topic"):
            desc += " (no results)"
        parts.append(desc)
    hint = "; ".join(parts) + "."
    if real_gates:
        hint += (
            " Configure a proxy (admin UI / SEARCH_MCP_PROXY) to route through "
            "a non-blocked IP, or rely on the keyless default engines."
        )
    if off_topic:
        # Not a wall, so none of the wall remedies apply — saying "configure a
        # proxy" here would send someone to fix a network that is fine.
        hint += (
            " Discarded results are not a block and a proxy will not change them; "
            "the remaining engines answered normally."
        )
    if browser_missing:
        hint += " " + BROWSER_INSTALL_HINT
    return hint


def _query_language(query: str) -> str:
    """Language code implied by the query's SCRIPT ("zh", "ja", ...), or "".

    The configured region is deliberately not consulted: an operator in
    `cn-zh` typing an English query wants the English web.
    """
    return detect_query_region(query, "").partition("-")[2]


def claimants(query: str) -> list[str]:
    """Record engines that say they can answer `query`, in registry order.

    Consulted only when the caller named no category: with one, routing is
    the category's. Capped by `settings.claim_engine_limit`, so a question
    that matches several registries ("latest version of X" with no ecosystem
    named) spends a bounded number of extra requests.
    """
    if not settings.auto_route_enabled or not query.strip():
        return []
    picks: list[str] = []
    for name, engine in ENGINES.items():
        try:
            # claims() first: it is a regex, while is_available() can read a
            # file (a sign-in engine's stored credential) on every search.
            if engine.claims(query) and engine.is_available():
                picks.append(name)
        except Exception:  # noqa: BLE001 - a claim test must never break a search
            log.warning("engine %s: claims() raised", name, exc_info=True)
    limit = settings.claim_engine_limit
    return picks[:limit] if limit >= 0 else picks


def _nominal_pool(query: str, category: str | None, freshness: str | None) -> list[str]:
    """The engines this search is entitled to, before health is considered.

    The cache is keyed on THIS list. Keying on what actually ran would make the
    key wobble with the breaker: the same question would miss whenever an
    engine went from benched to probing and back.
    """
    if _is_exclusive(category):
        # For these, the general web pool is noise rather than coverage: a web
        # engine cannot return an image file or a dataset record, so mixing it
        # in only crowds out the sources that can. Falls back to the default
        # pool if no specialist is available.
        return engines_for_category(category) or list(settings.default_engines)
    pool = list(settings.default_engines)
    if category:
        pool.extend(engines_for_category(category, exclude=pool))
    else:
        pool.extend(name for name in claimants(query) if name not in pool)
    extras: list[str] = []
    if freshness in ("day", "week"):
        extras.extend(settings.fresh_engines)
    extras.extend(settings.locale_engines.get(_query_language(query), []))
    pool.extend(name for name in dict.fromkeys(extras) if name not in pool)
    return pool


def _usable_reserve(name: str) -> bool:
    """Whether a reserve engine can stand in right now, without asking it."""
    if engine_health.is_open(name):
        return False
    try:
        engine = get_engine(name)
    except ValueError:
        return False
    # getattr: tests substitute duck-typed engine stubs (see `_max_token_wait`).
    is_available = getattr(engine, "is_available", None)
    if is_available is not None and not is_available():
        return False
    # A missing browser is never held against an engine someone asked for — the
    # install hint has to keep appearing. A reserve nobody asked for is
    # different: picking one that cannot run just adds that hint to a search
    # that was otherwise fine.
    return not (getattr(engine, "needs_browser", False) and browser_pool.known_unavailable)


def _active_pool(nominal: list[str]) -> tuple[list[str], dict[str, dict[str, Any]]]:
    """`(engines to query, benched)`: the nominal pool minus open circuits.

    Reserves are seated only while fewer than `min_healthy_engines` general
    engines remain. The shipped default pool is one wider than that, so it
    loses one engine before a reserve is seated, and each further loss seats
    one. If nothing general is left at all the breaker
    is ignored for this search: trying three doubtful engines beats answering
    with none.
    """
    if not settings.health_enabled:
        return nominal, {}
    benched: dict[str, dict[str, Any]] = {}
    for name in nominal:
        state = engine_health.describe(name)
        if state is not None:
            benched[name] = {**state, "substitute": None}
    if not benched:
        return nominal, {}

    active = [name for name in nominal if name not in benched]
    healthy = sum(1 for name in active if _is_guarded(name))
    waiting = [name for name in benched if _is_guarded(name)]
    for name in settings.reserve_engines:
        if healthy >= settings.min_healthy_engines or not waiting:
            break
        if name in nominal or not _usable_reserve(name):
            continue
        active.append(name)
        healthy += 1
        benched[waiting.pop(0)]["substitute"] = name
    if healthy == 0 and any(_is_guarded(name) for name in nominal):
        return nominal, {}
    return active, benched


def default_pool_now() -> list[str]:
    """The engines a plain search would ask right now: the default pool with
    benched engines out and reserves in."""
    return _active_pool(list(settings.default_engines))[0]


# --- the agent lane ---------------------------------------------------------
#
# An agent engine (`Engine.agent_search`) searches with a language model on the
# operator's own sign-in. It used to run only when a call named it, and naming
# it switched the keyless pool, its breaker and its routing off for that call.
# The lane runs the signed-in ones beside the pool instead: asked at the same
# moment, fused into the same ranking, and never allowed to hold the keyless
# results back past `agent_lane_wait_seconds`.

# Words in SEARCH_MCP_AGENT_ENGINES that name no agent engine, warned about once.
_warned_lane_tokens: set[str] = set()


def _configured_lane() -> list[str]:
    """The agent engines SEARCH_MCP_AGENT_ENGINES names, before sign-in and health."""
    tokens = settings.agent_engine_tokens()
    if "off" in tokens:
        return []
    names: list[str] = []
    # getattr: tests substitute duck-typed engine stubs for the registry.
    for token in tokens:
        if token == "auto":
            names.extend(
                name
                for name, engine in ENGINES.items()
                if getattr(engine, "agent_search", False)
                and getattr(engine, "agent_lane_auto", False)
            )
        elif token in ENGINES and getattr(ENGINES[token], "agent_search", False):
            names.append(token)
        elif token not in _warned_lane_tokens:
            _warned_lane_tokens.add(token)
            log.warning("SEARCH_MCP_AGENT_ENGINES: %r is not an agent engine; ignored", token)
    return list(dict.fromkeys(names))


def _lane_nominal(nominal: list[str]) -> list[str]:
    """The agent lane this search is entitled to: configured, signed in, and
    not already in the pool. The cache key includes it, so like `nominal` it
    is decided before the breaker is consulted."""
    lane: list[str] = []
    for name in _configured_lane():
        if name in nominal:
            continue
        try:
            # Reads the stored sign-in; it never opens a sign-in page.
            if get_engine(name).is_available():
                lane.append(name)
        except Exception:  # noqa: BLE001 - a broken sign-in file must not break search
            log.warning("engine %s: is_available() raised", name, exc_info=True)
    return lane


def _benched_for(name: str) -> str:
    """The breaker's reason for benching `name` right now, or ""."""
    state = engine_health.describe(name)
    return str(state.get("reason") or "") if state else ""


def _lane_names(stored: Any, lane_nominal: list[str]) -> list[str]:
    """The names in a cache row's lane list that this search's lane holds."""
    return [name for name in stored if name in lane_nominal] if isinstance(stored, list) else []


def _seated(lane_nominal: list[str]) -> list[str]:
    """The lane engines asked right now: the nominal lane minus open circuits."""
    if not settings.health_enabled:
        return list(lane_nominal)
    return [name for name in lane_nominal if not engine_health.is_open(name)]


def agent_lane_now() -> list[str]:
    """The agent engines a plain search would ask right now."""
    return _seated(_lane_nominal(list(settings.default_engines)))


_LaneOutcome = tuple[str, list[SearchResult] | Exception, dict[str, Any]]


async def _run_lane(
    name: str, query: str, n: int, filters: SearchFilters
) -> _LaneOutcome:
    """Ask one agent engine. Never raises.

    Its diagnostics stay in a dict of its own: an engine that finishes after
    the search returned must not write into a payload already handed back.
    """
    lane_diag: dict[str, Any] = {}
    # Each lane task runs in its own copy of the context, so this marks only
    # this engine's search.
    in_agent_lane.set(True)
    try:
        engine = get_engine(name)
    except ValueError as e:
        return name, e, lane_diag
    # No waiting for a token: the limit protects the operator's plan, and an
    # engine out of tokens sits this search out instead of holding it.
    if not await search_limiter.acquire(name, max_wait=0):
        lane_diag["rate_limited"] = True
        return name, [], lane_diag
    try:
        results = await engine.search(query, n, filters, diagnostics=lane_diag)
    except Exception as e:
        log.warning("agent engine %s failed: %s", name, brief_error(e))
        return name, e, lane_diag
    # Judged once, here: whether this answer echoes the query, which is what
    # lets it end the search's waits (see `_lane_found`).
    lane_diag["witness"] = bool(results) and is_witness(query, results)
    return name, results, lane_diag


async def _await_lane(
    tasks: dict[asyncio.Future[_LaneOutcome], str],
    started: float,
    answered: bool,
    rescue: asyncio.Future[tuple[list[SearchResult], str | None]] | None = None,
) -> tuple[list[_LaneOutcome], set[asyncio.Future[_LaneOutcome]], float]:
    """`(finished, still running, seconds since started)` once the lane's wait
    has passed.

    The wait counts from `started`, when the keyless engines were asked too,
    so the keyless fan-out's own time is not added to it, and once those have
    answered it ends `agent_lane_grace_seconds` after them at the latest. When the keyless
    engines found nothing, the lane may be all there is, and the wait
    stretches to the search deadline; once the early `rescue` running beside
    it has brought results back, the lane is down to its ordinary wait again.
    """
    if not tasks:
        return [], set(), 0.0
    ordinary = settings.agent_lane_wait_seconds
    if answered:
        # The keyless results are in: the lane gets its grace beyond them,
        # within its own wait.
        wait = min(ordinary, time.monotonic() - started + settings.agent_lane_grace_seconds)
    else:
        wait = max(ordinary, settings.search_deadline_seconds)
    # What finished while the keyless fan-out ran is in, however long that took.
    pending = {task for task in tasks if not task.done()}
    while pending:
        if rescue is not None and rescue.done():
            if not rescue.cancelled() and rescue.exception() is None and rescue.result()[0]:
                wait = min(wait, ordinary)
            rescue = None
        if _lane_found(tasks):
            # One lane engine's answer is an answer: the others get the
            # ordinary wait, not the stretched one (and never a longer one).
            wait = min(wait, ordinary)
        remaining = started + wait - time.monotonic()
        timeout: float | None = remaining
        if remaining <= 0:
            if rescue is None or rescue.done() or _lane_found(tasks):
                break
            # The search will wait for the early rescue anyway; a lane answer
            # that arrives meanwhile belongs in it.
            timeout = None
        watched = pending | ({rescue} if rescue is not None else set())
        done, _ = await asyncio.wait(
            watched, timeout=timeout, return_when=asyncio.FIRST_COMPLETED
        )
        # On a timeout the loop goes round again: the wait has passed, and the
        # top of the loop decides whether the early rescue still holds it open.
        pending -= done
    pending = {task for task in pending if not task.done()}
    order = {name: i for i, name in enumerate(tasks.values())}
    finished = sorted(
        (task.result() for task in tasks if task not in pending), key=lambda o: order[o[0]]
    )
    return finished, pending, time.monotonic() - started


def _lane_found(lane: Collection[asyncio.Future[_LaneOutcome]]) -> bool:
    """Whether an agent engine of the lane has answered with results that echo
    the query (`_run_lane` judges that once). Only such an answer may stop the
    search waiting for others: a bucket the off-topic guard would drop is no
    answer."""
    return any(
        task.done() and not task.cancelled() and task.result()[2].get("witness")
        for task in lane
    )


def _settle_lane_suspects(
    named: list[tuple[str, list[SearchResult]]],
    suspects: list[str],
    lane: Collection[str],
    *,
    witness: bool = False,
) -> tuple[list[tuple[str, list[SearchResult]]], list[str], list[str]]:
    """`(named, off-topic, doubted)`: the one rule for lane buckets the
    off-topic guard could not judge, applied once every bucket of the run is
    in (after the rescue on the main path, on each answer in the late merge).

    `suspects` are what the guard left unconfirmed. A lane suspect is never
    handed to the rescue (that pass is the keyless pool's, and no request is
    spent judging an agent engine) and never becomes a keyless gate. With a
    `witness` that arrived after the guard ran (a rescue bucket or a late
    answer that echoes the query), it is off-topic. Otherwise it is left out
    ("doubted") while a web bucket that is not itself a suspect remains to
    answer with, and served when the lane's answer is all there is, as a
    keyless suspect is. Records from specialist sources do not count as such a
    bucket: one looked-up record does not stand in for the lane's pages.
    """
    lane_suspects = [name for name in suspects if name in lane]
    if not lane_suspects:
        return named, [], []
    if witness:
        off_topic, doubted = lane_suspects, []
    else:
        solid = any(
            bucket
            for name, bucket in named
            if name not in lane and name not in suspects and _is_guarded(name)
        )
        off_topic, doubted = [], (lane_suspects if solid else [])
    drop = set(off_topic) | set(doubted)
    return [(n, b) for n, b in named if n not in drop], off_topic, doubted


def _take_lane_verdicts(diagnostics: dict[str, Any], lane: list[str]) -> list[str]:
    """Move the off-topic guard's verdicts on lane engines out of the shared
    `diagnostics["gated"]`, and return the lane engines it dropped."""
    gated = diagnostics.get("gated") or {}
    dropped = [name for name in lane if gated.get(name) == "off_topic"]
    for name in dropped:
        del gated[name]
    return dropped


# The breaker's reason for a lane engine benched by a lasting failure. A cache
# row is not shortened for an engine benched with it (see `lane_missing`).
LANE_REFUSED = "refused"


def _record_lane_health(
    name: str, outcome: list[SearchResult] | Exception, reason: str | None = None
) -> None:
    """Tell the breaker how an agent engine did.

    Stricter than `_record_health` where it counts: a lasting failure (a spent
    usage window, a refused or revoked sign-in, a model the service will not
    serve; see `is_lasting`) benches it at once as `LANE_REFUSED`, since the
    next search does not repair any of those and every attempt draws on the
    operator's plan. Anything else, a timeout, a reset, a token endpoint that
    was down, gets the ordinary two strikes. An empty answer is not held
    against it.
    """
    if not settings.health_enabled:
        return
    if isinstance(outcome, Exception):
        if is_lasting(outcome):
            engine_health.record_failure(name, reason or LANE_REFUSED, threshold=1)
        else:
            engine_health.record_failure(name, reason or "error")
    elif reason:
        engine_health.record_failure(name, reason)
    elif outcome:
        engine_health.record_success(name)


def _lane_hint(
    pending: list[str],
    errors: dict[str, str],
    rate_limited: list[str],
    *,
    waited: float,
    merges_later: bool,
    off_topic: list[str] | None = None,
    doubted: list[str] | None = None,
) -> str:
    """The "Agent lane" note. Called after the breaker heard about the run,
    so it can say whether a failed engine sits out the next searches."""
    parts: list[str] = []
    if off_topic:
        parts.append(
            f"The answer from {', '.join(off_topic)} barely mentioned the query beyond its "
            "first word while other engines' results did, so it was left out."
        )
    if doubted:
        parts.append(
            f"The answer from {', '.join(doubted)} looked off-topic and nothing in this "
            "search could confirm it either way, so it was left out in favour of the "
            "keyless results."
        )
    if pending:
        who = ", ".join(pending)
        parts.append(
            f"{who} had not answered within {waited:.1f} s, so the results above do not "
            "include it."
        )
        if merges_later:
            parts.append(
                "It keeps searching in the background and its results are merged into the "
                "cached result set for this search: repeat the same call in a little while "
                "to get them."
            )
    for name, err in errors.items():
        after = (
            "sits out the next searches"
            if settings.health_enabled and engine_health.is_open(name)
            else "is asked again on the next search"
        )
        parts.append(f"{name} failed ({err}) and {after}; the keyless results are unaffected.")
    if rate_limited:
        parts.append(
            f"{', '.join(rate_limited)} sat this search out: its per-minute limit, which "
            "protects the account's usage, was spent."
        )
    return " ".join(parts)


# Agent engines still searching when their search returned. Held here so the
# event loop keeps a reference until each one's results are merged.
_lane_background: set[asyncio.Task[None]] = set()
# Which late merge may still write each cache row: a search that writes the
# row afresh takes it away from any merge an earlier search left running, so
# old buckets never overwrite a newer answer.
_lane_rows: dict[str, object] = {}
# Serialises taking a row and writing it, per event loop (a lock is bound to
# the loop it first waits on).
_row_locks: weakref.WeakKeyDictionary[asyncio.AbstractEventLoop, asyncio.Lock] = (
    weakref.WeakKeyDictionary()
)


def _row_lock() -> asyncio.Lock:
    loop = asyncio.get_running_loop()
    lock = _row_locks.get(loop)
    if lock is None:
        lock = _row_locks[loop] = asyncio.Lock()
    return lock


def _late_ceiling(names: list[str]) -> float:
    """Ceiling on the background wait for the lane engines `names`, against
    one that never returns.

    An engine's timeout (SEARCH_MCP_CODEX_TIMEOUT,
    SEARCH_MCP_ANTIGRAVITY_TIMEOUT, 60 s by default) bounds each request, and
    one search can make several in a row: a token refresh, the model
    catalogue, the search, a fallback. Four of the longest pending engine's
    timeout and a margin, so an operator who allows a slow engine more time
    keeps its answer: 270 s with the defaults."""
    timeouts = [float(getattr(settings, f"{name}_timeout", 60.0)) for name in names] or [60.0]
    return 4 * max(timeouts) + 30.0


def _merge_lane_later(
    pending: set[asyncio.Future[_LaneOutcome]],
    *,
    query: str,
    n: int,
    category: str | None,
    named: list[tuple[str, list[SearchResult]]],
    keyless: list[str],
    asked: list[str],
    cache_key: str,
    meta: dict[str, Any],
    missing: list[str],
    written_at: int | None,
    pending_names: list[str],
) -> None:
    owner = object()
    _lane_rows[cache_key] = owner
    task = asyncio.ensure_future(
        _fold_late_lane(
            pending,
            query=query,
            n=n,
            category=category,
            named=list(named),
            keyless=list(keyless),
            asked=list(asked),
            cache_key=cache_key,
            meta=dict(meta),
            missing=list(missing),
            written_at=written_at,
            ceiling=_late_ceiling(pending_names),
            owner=owner,
        )
    )
    _lane_background.add(task)
    task.add_done_callback(_lane_background.discard)


async def _fold_late_lane(
    pending: set[asyncio.Future[_LaneOutcome]],
    *,
    query: str,
    n: int,
    category: str | None,
    named: list[tuple[str, list[SearchResult]]],
    keyless: list[str],
    asked: list[str],
    cache_key: str,
    meta: dict[str, Any],
    missing: list[str],
    written_at: int | None,
    ceiling: float,
    owner: object,
) -> None:
    """Merge agent engines that answered after their search returned into
    that search's cached result set. Never raises.

    `named` is what the search merged, as `(engine, bucket)`. Each late bucket
    goes through the same off-topic guard (`_drop_decoy_buckets`) together
    with it: it can be dropped as a decoy itself, or be the witness that drops
    a web engine the guard could not judge before for want of one. The row is
    rewritten each time an engine answers, which also settles its place on
    `lane_missing`, and the merge stops once a newer search has rewritten the
    row.
    """
    deadline = time.monotonic() + ceiling
    missing_now = set(missing)
    try:
        while pending:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                break
            done, pending = await asyncio.wait(
                pending, timeout=remaining, return_when=asyncio.FIRST_COMPLETED
            )
            changed = False
            for task in done:
                name, outcome, lane_diag = task.result()
                if isinstance(outcome, Exception) or lane_diag.get("rate_limited"):
                    _record_lane_health(name, outcome)
                    if isinstance(outcome, Exception):
                        # Asked and failed: listed as the main path lists it.
                        asked.append(name)
                        changed = True
                        if is_lasting(outcome):
                            # A lasting state, as on the main path: the row
                            # keeps its full lifetime.
                            missing_now.discard(name)
                    continue
                missing_now.discard(name)
                asked.append(name)
                changed = True
                if not outcome:
                    _record_lane_health(name, outcome)
                    continue
                verdict: dict[str, Any] = {}
                kept, unjudged = _drop_decoy_buckets(
                    query, [*named, (name, outcome)], verdict
                )
                lane_names = [n_ for n_ in asked if n_ not in keyless]
                dropped_lane = _take_lane_verdicts(verdict, lane_names)
                kept, _, doubted = _settle_lane_suspects(kept, unjudged, lane_names)
                if name not in dropped_lane and name not in doubted:
                    _record_lane_health(name, outcome)
                for dropped in dropped_lane:
                    # The late answer itself, or a lane engine served earlier
                    # that it has now shown to be a decoy.
                    _record_lane_health(dropped, outcome, "off_topic")
                left = [n_ for n_ in dropped_lane + doubted
                        if n_ not in (meta.get("lane_left_out") or [])]  # fmt: skip
                if left:
                    # Left out, as on the main path, and recorded on the row.
                    meta = {**meta, "lane_left_out": [*(meta.get("lane_left_out") or []), *left]}
                cleared = list(verdict.get("gated") or {})
                named = kept
                if cleared:
                    meta = _with_off_topic(meta, cleared)
                    # The row now rests on fewer keyless indexes; if too few,
                    # it is degraded like any thinned pool's.
                    gates = {n_: info["reason"] for n_, info in meta["gated_engines"].items()}
                    _, standing = _general_census(keyless, named, {}, {"gated": gates})
                    if standing < settings.min_healthy_engines:
                        meta = {**meta, "degraded": True}
            if not changed:
                continue
            merged = _merge([bucket for _, bucket in named], n, category, query)
            if not merged:
                continue
            stored = {**meta, "engines": asked}
            stored.pop("lane_missing", None)
            if missing_now:
                stored["lane_missing"] = sorted(missing_now)
            async with _row_lock():
                if _lane_rows.get(cache_key) is not owner:
                    break
                # The keyless results are as old as the search that found
                # them; adding a lane answer does not make them newer.
                await cache.put_search(
                    cache_key, query, asked, merged, stored, created=written_at
                )
                written_at = written_at or int(time.time())
    except Exception:  # noqa: BLE001 - nobody is waiting on this to fail
        log.warning("merging the agent lane's late results failed", exc_info=True)
    finally:
        for task in pending:
            task.cancel()
        if _lane_rows.get(cache_key) is owner:
            del _lane_rows[cache_key]


def _with_off_topic(meta: dict[str, Any], names: list[str]) -> dict[str, Any]:
    """`meta` with the keyless engines `names` recorded as off-topic gates,
    and the breaker told. (`_drop_decoy_buckets` has logged them.)"""
    stored = meta.get("gated_engines") or {}
    gated = {name: info.get("reason") or "gated" for name, info in stored.items()}
    fallback = {name: info["fallback"] for name, info in stored.items() if info.get("fallback")}
    for name in names:
        gated[name] = "off_topic"
        if settings.health_enabled:
            engine_health.record_failure(name, "off_topic")
    gated_engines, gated_hint = _gate_provenance(gated, fallback)
    return {**meta, "gated_engines": gated_engines, "gated_hint": gated_hint}


def _gate_provenance(
    gated: dict[str, str], fallback: dict[str, str]
) -> tuple[dict[str, dict[str, Any]], str]:
    """`(gated_engines, gated_hint)`: which engines hit a wall and how each was
    covered, in the shape the payload and the cache row carry."""
    gated_engines = {
        name: {"reason": gated.get(name, "gated"), "fallback": fallback.get(name)}
        for name in sorted(set(gated) | set(fallback))
    }
    return gated_engines, (_gate_hint(gated, fallback) if gated_engines else "")


# Raised by an engine without saying anything about the engine itself.
_NOT_THE_ENGINES_FAULT = (ValueError, BrowserUnavailableError, BrowserLaunchError)


def _record_health(
    ran: list[str],
    named: list[tuple[str, list[SearchResult]]],
    raised: dict[str, Exception],
    diagnostics: dict[str, Any],
) -> None:
    """Tell the breaker how each engine that ran did.

    Called after the off-topic guard, so `off_topic` counts. Four outcomes are
    deliberately NOT failures: a missing local browser (the machine's problem,
    and the install hint must keep appearing), a browser that did not start (a
    local network stall, which the pool's own cooldown already retries), a
    rate-limit skip (ours), and a ValueError (an unknown engine name or a
    missing API key — configuration, which no amount of waiting repairs).
    """
    if not settings.health_enabled:
        return
    gated = diagnostics.get("gated") or {}
    skipped = set(diagnostics.get("rate_limited") or [])
    refused = diagnostics.get("http_status") or {}
    sizes = {name: len(bucket) for name, bucket in named}
    raw = diagnostics.get("raw_per_engine") or {}
    peer_found_something = {
        name: any(size and other != name and _is_guarded(other) for other, size in sizes.items())
        for name in ran
    }
    for name in ran:
        if name in skipped:
            continue
        if name in raised:
            if not isinstance(raised[name], _NOT_THE_ENGINES_FAULT):
                engine_health.record_failure(name, "error")
            continue
        reason = gated.get(name)
        if reason == "browser_unavailable":
            continue
        if reason:
            engine_health.record_failure(name, reason)
        elif name in refused:
            engine_health.record_failure(name, f"http_{refused[name]}")
        elif raw.get(name, sizes.get(name, 0)) > 0:
            engine_health.record_success(name)
        elif _is_guarded(name) and peer_found_something[name]:
            engine_health.record_failure(name, "silent", threshold=SILENT_THRESHOLD)


def _general_census(
    ran: list[str],
    named: list[tuple[str, list[SearchResult]]],
    raised: dict[str, Exception],
    diagnostics: dict[str, Any],
) -> tuple[int, int]:
    """`(lost, standing)` among the general web engines of this run: how many
    fell over (raised, walled, off-topic, refused) and how many returned
    results that were kept."""
    gated = diagnostics.get("gated") or {}
    refused = diagnostics.get("http_status") or {}
    kept = {name for name, bucket in named if bucket}
    lost = standing = 0
    for name in ran:
        if not _is_guarded(name):
            continue
        failed = name in gated or name in refused
        if name in raised:
            failed = not isinstance(raised[name], ValueError)
        if failed:
            lost += 1
        elif name in kept:
            standing += 1
    return lost, standing


# A result set produced while the pool was too thin, or without an agent engine
# the search was keyed on for a passing reason (still being merged in, out of
# tokens, a timeout, or benched after one), is replayed for an hour at most,
# however long the configured TTL is: both describe a pool that recovers.
_DEGRADED_CACHE_TTL = 3600


def _too_degraded_to_replay(meta: dict[str, Any]) -> bool:
    if not (meta.get("degraded") or meta.get("lane_missing")):
        return False
    return time.time() - float(meta.get("cached_at") or 0) > _DEGRADED_CACHE_TTL


def _benched_hint(benched: dict[str, dict[str, Any]]) -> str:
    parts = []
    for name in sorted(benched):
        info = benched[name]
        minutes = max(1, round(info["retry_in_seconds"] / 60))
        desc = f"{name} is benched ({info['reason']}; retried in ~{minutes} min)"
        if info.get("substitute"):
            desc += f", {info['substitute']} is standing in"
        parts.append(desc)
    return (
        "; ".join(parts) + ". A benched engine failed recently and was not asked this "
        "time. Name it in `engines=` to force an attempt."
    )


def _is_guarded(name: str) -> bool:
    """Whether the off-topic guard applies to this engine: web indexes only.

    A specialist is exempt because it legitimately does not echo the query —
    SEC EDGAR answers "NVDA risk factors" with filing titles that contain
    neither word (coherence 0.0 in the capture, and every one of them right).
    A single-site catalogue is exempt for the same reason. getattr, because
    tests substitute duck-typed engine stubs.
    """
    try:
        engine = get_engine(name)
    except ValueError:
        return False
    return not getattr(engine, "categories", None) and not getattr(engine, "single_site", False)


def _drop_decoy_buckets(
    query: str,
    named: list[tuple[str, list[SearchResult]]],
    diagnostics: dict[str, Any],
) -> tuple[list[tuple[str, list[SearchResult]]], list[str]]:
    """Remove web-engine buckets that answer only the query's first word.

    Returns `(kept, unconfirmed)`. A suspect bucket is dropped only when
    another bucket in the same run is a WITNESS — coherent enough to prove that
    pages about this query do echo its words. Without one, the metric itself is
    in doubt (an English query answered in Japanese echoes nothing, and is not
    a decoy), so the suspects are kept and returned as `unconfirmed` for the
    caller to settle if a rescue bucket later turns up as the witness.

    Always judged against the caller's query, never the operator-augmented one
    an engine actually sent.
    """
    if not settings.coherence_guard_enabled:
        return named, []
    suspects = [
        name for name, bucket in named if _is_guarded(name) and looks_like_decoy(query, bucket)
    ]
    if not suspects:
        return named, []
    if not any(name not in suspects and is_witness(query, bucket) for name, bucket in named):
        return named, suspects
    _mark_off_topic(suspects, diagnostics)
    return [(name, bucket) for name, bucket in named if name not in suspects], []


def _mark_off_topic(names: list[str], diagnostics: dict[str, Any]) -> None:
    gated = diagnostics.setdefault("gated", {})
    for name in names:
        log.info("engine %s returned an off-topic bucket; dropped", name)
        gated[name] = "off_topic"


def _needs_rescue(
    merged: list[dict[str, Any]],
    errors: dict[str, str],
    diagnostics: dict[str, Any],
    category: str | None = None,
    *,
    seek_witness: bool = False,
    lost_general: int = 0,
    healthy_general: int = 0,
) -> bool:
    """Decide whether the keyless rescue pass should run.

    Triggers only when the run is empty, or sparse (<=3 — the SAME threshold
    the empty-engine and filter hints use, so a run is never simultaneously
    "sparse enough to warn about" and "too healthy to rescue") AND
    demonstrably unhealthy: an engine errored, hit a gate, or silently
    returned zero. A healthy niche query that legitimately yields a few
    results must NOT trigger network work — the normal-path latency guarantee.

    NEVER for an exclusive category. `settings.rescue_engines` is the general
    web pool, and the whole reason `image` and `dataset` REPLACE that pool is
    that a web engine cannot return an image file or a dataset record. Rescuing
    into it hands back exactly the wrong media type — HTML pages that
    `fetch(inline=True)` cannot render — under a header saying the search
    succeeded. Reporting the empty run is the honest answer, and the sparse and
    empty-engine hints already do that.
    """
    if not settings.rescue_enabled:
        return False
    if _is_exclusive(category):
        return False
    if len(merged) == 0:
        return True
    if seek_witness:
        # ONE engine came back off-topic and nothing in the run could confirm
        # or clear it. A second opinion is worth a rescue even when the result
        # count looks healthy — ten decoys are not ten results.
        return True
    if lost_general and healthy_general < settings.min_healthy_engines:
        # General engines dropped out DURING this run (walled, errored,
        # off-topic) and too few are left standing. The result count can look
        # fine — one surviving engine returns ten results — while the answer
        # rests on a single index.
        return True
    if len(merged) > 3:
        return False
    raw = diagnostics.get("raw_per_engine", {})
    return bool(errors) or bool(diagnostics.get("gated")) or any(
        count == 0 for count in raw.values()
    )


# The race hedges one slow candidate with one more, not with the whole list:
# the reserves answer in 1 to 3 s, so a one-second stagger with no cap would
# have every rescue reach the browser-rendered `brave` and the quota-limited
# `youcom` on an ordinary slow day. A candidate stops counting against the cap
# once it has run past its share of the pass (see `hold` in `_rescue`).
_RESCUE_IN_FLIGHT = 2
# A candidate's place is held for this share of `rescue_timeout`: 4 s of the
# default 10, past the 1 to 3 s a slow reserve takes and early enough to leave
# a fast third candidate time to answer.
_RESCUE_HOLD_SHARE = 0.4


async def _rescue(
    query: str,
    n: int,
    filters: SearchFilters,
    engine_names: list[str],
    diagnostics: dict[str, Any],
) -> tuple[list[SearchResult], str | None]:
    """One bounded keyless recovery pass via ``settings.reserve_engines`` then
    ``settings.rescue_engines``, skipping any that already ran or are benched.

    A staggered race, first usable answer wins. The first candidate is asked
    at once and each next one ``settings.rescue_stagger_seconds`` later, or as
    soon as the one before it has failed, with at most ``_RESCUE_IN_FLIGHT``
    running at once; the winner cancels whatever is still running. The whole
    pass, rate-limiter waits included, fits in ``settings.rescue_timeout``.
    It used to ask one candidate at a time in an equal slice each, so a slow
    first candidate spent its slice before a fast second one was asked; a
    race asks the fast one after one stagger. Calls the
    engines directly, never re-entering ``aggregate_search``, and the candidate
    list excludes engines the caller already ran, so there is no recursion and
    no self-rescue.

    Each candidate runs with a PRIVATE diagnostics dict: rescue probes must
    never leak into the caller-facing per-engine stats (``empty_engines`` /
    ``gated_engines`` describe engines the caller asked for). The attempt
    summary — including any gates the probes hit — lives under
    ``diagnostics["rescue"]``. Returns ``(results, served_by)``; never raises.
    """
    candidates = [
        name
        for name in dict.fromkeys([*settings.reserve_engines, *settings.rescue_engines])
        if name not in engine_names and _usable_reserve(name)
    ]
    if not candidates:
        return [], None
    attempted: list[str] = []
    info: dict[str, Any] = {"attempted": attempted}
    diagnostics["rescue"] = info

    async def probe(name: str) -> list[SearchResult]:
        """The candidate's results if they are usable, else [] (and the
        failure recorded)."""
        try:
            engine = get_engine(name)
        except ValueError:
            return []
        rescue_diag: dict[str, Any] = {}
        try:
            if not await search_limiter.acquire(name, max_wait=_max_token_wait(engine)):
                # Rescue is already a bounded, best-effort recovery attempt;
                # queueing for a token would starve the other candidates.
                return []
            results = await engine.search(query, n, filters, diagnostics=rescue_diag)
        except Exception as e:
            log.warning("rescue engine %s failed: %s", name, e)
            if not isinstance(e, ValueError):
                _probe_failed(name, "error")
            return []
        if rescue_diag.get("gated"):
            info.setdefault("gated", {}).update(rescue_diag["gated"])
            reason = rescue_diag["gated"].get(name)
            if reason and reason != "browser_unavailable":
                _probe_failed(name, reason)
        if (
            results
            and settings.coherence_guard_enabled
            and _is_guarded(name)
            and looks_like_decoy(query, results)
        ):
            # A weak rescue candidate was measured at 0.0-0.2 coherence on a
            # bad day. Recovering INTO a decoy would be worse than not recovering.
            info.setdefault("gated", {})[name] = "off_topic"
            _probe_failed(name, "off_topic")
            return []
        return results

    loop = asyncio.get_running_loop()
    deadline = loop.time() + settings.rescue_timeout
    waiting = list(candidates)
    running: dict[asyncio.Future[list[SearchResult]], str] = {}
    started_at: dict[asyncio.Future[list[SearchResult]], float] = {}
    # How long a candidate holds its place among the `_RESCUE_IN_FLIGHT`. One
    # still running after that keeps running, and can still win, but no longer
    # keeps the next candidate waiting: two hung reserves must not starve a
    # fast third, and a reserve that is merely slow (2 to 3 s) must not let the
    # race run on to `brave` and `youcom` either.
    hold = max(2 * settings.rescue_stagger_seconds, settings.rescue_timeout * _RESCUE_HOLD_SHARE)
    next_start = loop.time()
    try:
        while waiting or running:
            now = loop.time()
            if now >= deadline:
                break
            holding = [task for task in running if now - started_at[task] < hold]
            room = len(holding) < _RESCUE_IN_FLIGHT
            if waiting and room and (not running or now >= next_start):
                name = waiting.pop(0)
                attempted.append(name)
                task = asyncio.ensure_future(probe(name))
                running[task] = name
                started_at[task] = now
                next_start = now + settings.rescue_stagger_seconds
                continue
            timeout = deadline - now
            if waiting and room:
                timeout = min(timeout, max(0.0, next_start - now))
            elif waiting:
                free_at = min(started_at[task] + hold for task in holding)
                timeout = min(timeout, max(0.0, free_at - now))
            done, _ = await asyncio.wait(
                running, timeout=timeout, return_when=asyncio.FIRST_COMPLETED
            )
            # Two answers in the same tick: the earlier candidate wins.
            for task in sorted(done, key=lambda t: candidates.index(running[t])):
                name = running.pop(task)
                results = task.result()
                if results:
                    info["served_by"] = name
                    info["results"] = len(results)
                    if settings.health_enabled:
                        engine_health.record_success(name)
                    return results, name
            if done:
                # A candidate failed: its successor need not wait out the stagger.
                next_start = loop.time()
        end = loop.time()
        for task, name in running.items():
            info.setdefault("timeouts", []).append(name)
            # Only a candidate that had its share of the pass and still did
            # not answer has shown anything; one started just before the end
            # has not.
            if end - started_at[task] >= hold:
                _probe_failed(name, "timeout")
        return [], None
    finally:
        for task in running:
            task.cancel()
            # Owned until its cancellation has run (a browser render tears
            # down for a while), without the search waiting for it.
            _cancelled_probes.add(task)
            task.add_done_callback(_cancelled_probes.discard)


# Rescue candidates cancelled by a winner or the deadline, still unwinding.
_cancelled_probes: set[asyncio.Future[list[SearchResult]]] = set()


def _probe_failed(name: str, reason: str) -> None:
    # A rescue candidate that fails is benched like any other engine, so the
    # next degraded search does not spend its rescue budget on it again.
    if settings.health_enabled:
        engine_health.record_failure(name, reason)


USAGE_NOTE = (
    "Snippets locate sources; they are not the source. Read dates, amounts, rules and "
    "other details from the page itself with fetch or research before relying on them."
)


def _annotate(results: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Per-result provenance that is derived, not stored: runs on fresh results
    and on cache hits alike, so a row cached by an older version still gets it."""
    for rec in results:
        kind = classify_source(rec.get("url") or "")
        if kind:
            rec["source_type"] = kind
        if "date_source" not in rec:
            # A pre-0.12 cache row. Whether its date was structured is no longer
            # knowable, so claim the weaker of the two.
            rec["date_source"] = "snippet" if rec.get("published_age") else "none"
    return results


def _iso_utc(epoch: float) -> str:
    return datetime.fromtimestamp(epoch, UTC).strftime("%Y-%m-%dT%H:%M:%SZ")


def _freshness_signals(
    results: list[dict[str, Any]], freshness: str | None, retrieved: float
) -> dict[str, Any]:
    """The facts a reader needs to judge whether an answer is CURRENT.

    Search results almost never carry a date — measured at 0-1 in 10 — and the
    `freshness` filter keeps undated results rather than dropping them, so
    "I asked for this week" silently became "these are from this week". This
    says what is actually known.
    """
    dated = sum(1 for r in results if r.get("date_source") in ("structured", "snippet"))
    out: dict[str, Any] = {
        "retrieved_at": _iso_utc(retrieved),
        "dated_results": dated,
        "usage_note": USAGE_NOTE,
    }
    undated = len(results) - dated
    if freshness and results and undated * 2 >= len(results):
        out["freshness_note"] = (
            f"{undated} of {len(results)} results carry no verifiable date, and "
            f'freshness="{freshness}" keeps undated results, so being listed here does not '
            "show that a result is recent. Fetch the page and check its publish date "
            "before treating it as current."
        )
    return out


# How long a cached answer may stand in for a fresh one, when the caller said
# recency matters. The default TTL is seven days, and a seven-day-old answer to
# a `freshness="day"` query is wrong by construction: the results it holds were
# each under a day old WHEN CACHED.
_FRESHNESS_CACHE_TTL = {"day": 3600, "week": 6 * 3600, "month": 24 * 3600}
_NEWS_CACHE_TTL = 6 * 3600
# Caps for the categories whose records go stale on their own clock, whatever
# the query said about freshness. A forecast is reissued hourly and the ECB
# fixes its rates once a working day, so a week-old cached answer to "上海明天
# 天气" or "100 usd to cny" would be wrong while looking exact; a registry's
# current version or a package's advisories move on the scale of hours to
# days. Keyed by group or full token; the full token wins.
_CATEGORY_CACHE_TTL = {
    "weather": 3600,
    "finance.fx": 3600,
    "software": 6 * 3600,
    "security": 6 * 3600,
}


def _read_ttl(
    freshness: str | None, category: str | None, max_age_seconds: int | None
) -> int | None:
    """The tightest of: the caller's `max_age_seconds`, the freshness window's
    TTL, the news cap and the category's own cap. None means "use the
    configured default"."""
    limits = [max_age_seconds, _FRESHNESS_CACHE_TTL.get(freshness or "")]
    if category_group(category) == "news":
        limits.append(_NEWS_CACHE_TTL)
    if category:
        cap = _CATEGORY_CACHE_TTL.get(category, _CATEGORY_CACHE_TTL.get(category_group(category)))
        limits.append(cap)
    known = [limit for limit in limits if limit is not None]
    return min(known) if known else None


def _key(query: str, engines: list[str], max_results: int, filters: SearchFilters) -> str:
    """Cache key for one search. Four arguments, and tests patch it as such.

    Region, SafeSearch and Accept-Language are read from settings rather than
    passed in: they change what every engine returns, and were missing, so a
    server restarted with SEARCH_MCP_REGION=jp-ja served a week of cached
    us-en answers. `v` is the key's own version — bump it to orphan every
    existing row. 2: rows written before the off-topic guard can hold merged
    decoy results for up to the 7-day TTL.
    """
    raw = json.dumps(
        {
            "v": 2,
            "q": query,
            "e": sorted(engines),
            "n": max_results,
            "f": asdict(filters),
            "r": settings.region,
            "s": settings.safesearch,
            "l": settings.accept_language,
        },
        ensure_ascii=False,
        sort_keys=True,
    )
    return hashlib.sha256(raw.encode("utf-8")).hexdigest()


def _absorb(rec: dict[str, Any], r: SearchResult) -> None:
    """Fold another sighting of the same URL into its representative record.

    FIELD-wise, not record-wise. The best value for each field lives in a
    different bucket, and picking one record wholesale — the one with the
    longest snippet — threw the others away. Concretely: googlenews, gdelt,
    arxiv and crossref supply an exact `published_age` from a structured
    source, an HTML scraper supplies a longer snippet, the scraper won the
    whole record, and the ranked-output loop below then dropped the
    now-empty `published_age` from the payload entirely. That is the one
    field `search`'s docstring tells the model to rely on for freshness.
    """
    if len(r.snippet) > len(rec.get("snippet") or ""):
        rec["snippet"] = r.snippet
    if not (rec.get("title") or "").strip() and r.title.strip():
        rec["title"] = r.title
    # Sightings now share a key across schemes. Hand back the https one: it is
    # the link that works without a redirect, whichever engine came first.
    if rec.get("url", "").startswith("http://"):
        sighted = _normalize_url(r.url)
        if sighted.startswith("https://"):
            rec["url"] = sighted
    # A date from a structured source (RSS pubDate, an API field) beats one
    # scraped out of snippet prose. Among equals, the first sighting wins.
    confident = _is_confident(r)
    if r.published_age and (
        not rec.get("published_age")
        or (confident and not rec.get(_AGE_CONFIDENT))
    ):
        rec["published_age"] = r.published_age
        rec[_AGE_CONFIDENT] = confident


# Internal-only marker carried on the representative dict while merging, so
# `_absorb` can prefer a trusted date over a scraped one. `to_dict()`
# deliberately omits `published_age_confident`, and it must not reach output.
_AGE_CONFIDENT = "_published_age_confident"


def _is_confident(r: Any) -> bool:
    """Whether this result's `published_age` came from a structured source.

    getattr, not attribute access: tests substitute duck-typed result stubs
    that predate the flag, and a missing attribute means "not known to be
    trusted" — the same convention `_max_token_wait` uses for engine stubs.
    """
    return bool(getattr(r, "published_age_confident", False))


# RRF's damping constant, from Cormack et al. 2009. Measured on a 14-query set
# against real engine output: moving it anywhere between 5 and 60 changed MRR
# by under 0.01, because ranks are already correlated across engines. Left at
# the literature value — there is no evidence here for a different one.
_RRF_K = 60.0

# How much a result from an engine that NATIVELY indexes the requested category
# counts, relative to a general web engine.
#
# Without this, `category=` barely affected the ORDER of results. A specialist
# is usually the only source returning a given document, so its hit scored
# 1/61 while three general engines agreeing on a blog post about the topic
# scored 3/61 and won: `category="finance.filings"` put NVIDIA's actual 10-K
# fourth, behind commentary about it. Measured (hit@1 / hit@3 / MRR against the
# one result a knowledgeable person would call correct, 14 queries):
#
#     weight 1.0 (before)   6/14   9/14   0.605
#     weight 2.0            8/14  13/14   0.747    6 improved, 0 regressed
#
# 2.0 is the point where one native hit ties two general engines agreeing — a
# rule that can be stated, rather than a constant fitted to this set. Higher
# values nudged MRR up but cost hit@3, and by 3.0 they let ANY native result
# outrank a consensus one: the correct arXiv URL for "attention is all you
# need" fell from rank 1 to rank 18, behind other papers arXiv returned first.
_NATIVE_CATEGORY_WEIGHT = 2.0

# How much the FIRST result of a `direct_answer` engine counts when its
# category was requested. Such a result is a record looked up by the query
# (PyPI's current release, the ECB rate, today's forecast), not a page ranked
# by relevance, and the caller asked for exactly that kind of record. The rule:
# one looked-up record outranks the whole three-engine default pool agreeing on
# a page ABOUT it (4/60 < 5/60), because the page's snippet is a crawl-time
# summary and the record is the publisher's own dated value. Measured
# 2026-09-21 on the eleven category probes in the channel work: with 2.0 the
# PyPI record for "latest fastapi version" ranked below three snippets of the
# PyPI project page, and Open-Meteo's numbers ranked third behind two weather
# portals' stale snippets; with 5.0 both lead. Only rank 0 gets this, so an
# engine that returns several records (OSV's advisories, Wikidata's entity
# candidates) leads with its best one and interleaves the rest as native hits.
_DIRECT_ANSWER_WEIGHT = 5.0


def _direct_answer_engines(category: str | None, query: str) -> frozenset[str]:
    """Names of the engines whose first result is a record answering `query`.

    With a category, those declaring it; without one, those that claimed the
    question (see `claimants`), since a claim is exactly the statement that
    the first result will be the record.
    """
    if not category:
        return frozenset(
            name for name in claimants(query) if ENGINES[name].answers_directly(query)
        )
    return frozenset(
        name
        for name, engine in ENGINES.items()
        if category in engine.categories and engine.answers_directly(query)
    )


def _native_engines(category: str | None, query: str = "") -> frozenset[str]:
    """Names of the engines that declare `category`.

    Accepts either level of the token: an engine declaring `paper.biomed` also
    declares `paper`, so both resolve here without the caller splitting
    anything — the same test `engines_for_category` uses. Without a category,
    the engines that claimed the query count as native: they were seated for
    it.
    """
    if not category:
        return frozenset(claimants(query))
    return frozenset(
        name for name, engine in ENGINES.items() if category in engine.categories
    )


def _merge(
    buckets: list[list[SearchResult]],
    max_results: int,
    category: str | None = None,
    query: str = "",
) -> list[dict[str, Any]]:
    """Weighted reciprocal-rank fusion across engines.

    A URL appearing high in several engines wins, except that when the caller
    named a `category`, the engines that natively index it count double (see
    `_NATIVE_CATEGORY_WEIGHT`). Still no per-result scoring magic: a lexical
    query/title overlap bonus was tried on the same measurement set and made
    every configuration worse (MRR 0.747 -> 0.645), so rank and engine
    agreement remain the only signals.
    """
    k = _RRF_K
    native = _native_engines(category, query)
    direct = _direct_answer_engines(category, query)
    scores: dict[str, float] = {}
    representative: dict[str, dict[str, Any]] = {}
    engines_for: dict[str, list[str]] = {}

    for bucket in buckets:
        # Buckets are per engine, so position 0 is that engine's first result.
        # (`finalize_results` stamps ranks from 1, so `r.rank` is not tested.)
        for position, r in enumerate(bucket):
            url = _normalize_url(r.url)
            if not url:
                continue
            key = _url_key(url)
            record = position == 0 and r.engine in direct
            if record:
                weight = _DIRECT_ANSWER_WEIGHT
            elif r.engine in native:
                weight = _NATIVE_CATEGORY_WEIGHT
            else:
                weight = 1.0
            scores[key] = scores.get(key, 0.0) + weight / (k + r.rank)
            engines_for.setdefault(key, []).append(r.engine)
            rec = representative.get(key)
            if rec is None:
                rec = r.to_dict()
                rec["url"] = url
                rec[_AGE_CONFIDENT] = _is_confident(r)
                representative[key] = rec
            else:
                _absorb(rec, r)
                if record and r.title.strip():
                    # The record's title states the value ("fastapi 0.141.1
                    # on PyPI"); the page title a web engine saw does not.
                    rec["title"] = r.title

    ranked = sorted(scores.items(), key=lambda kv: kv[1], reverse=True)
    out_full = []
    for key, score in ranked:
        rec = representative[key]
        rec["engines"] = sorted(set(engines_for[key]))
        rec["score"] = round(score, 5)
        rec.pop("rank", None)
        rec.pop("engine", None)
        # Where the date came from, in words a reader can weigh: a feed's
        # pubDate or an API field ("structured"), a date-looking phrase lifted
        # out of snippet prose ("snippet"), or nowhere ("none" — which means
        # unknown age, not recent). Written here because the marker it is
        # derived from is internal and is dropped on the next line.
        if not rec.get("published_age"):
            rec["date_source"] = "none"
        else:
            rec["date_source"] = "structured" if rec.get(_AGE_CONFIDENT) else "snippet"
        rec.pop(_AGE_CONFIDENT, None)
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


# Upper bound on `max_results`. Engines cap out around 10-20 results each, so
# anything past this is duplicate noise bought with real latency — and the
# number is also the per-engine budget, so it multiplies across the fan-out.
_MAX_RESULTS = 50


async def _fan_out(
    run: Any,
    engine_names: list[str],
    diagnostics: dict[str, Any],
    lane: Collection[asyncio.Future[_LaneOutcome]] = (),
) -> list[tuple[str, list[SearchResult] | Exception]]:
    """Query every engine in parallel, bounded by `settings.search_deadline_seconds`.

    The deadline starts when the fan-out starts. When it passes and at least
    one engine has answered with results, the engines still running are
    cancelled and listed in `diagnostics["timed_out"]`; their slot in the
    answer is an error, so the breaker sees it and the caller reads it. When
    nothing has answered yet the wait continues, because an empty answer
    delivered on time is worth less than a late one, unless an agent engine
    of the `lane` running beside the fan-out has found results: then the
    search has its answer, and the stragglers are cut like any other.
    """
    deadline = settings.search_deadline_seconds
    tasks = {asyncio.ensure_future(run(name)): name for name in engine_names}
    if not tasks:
        return []
    if deadline <= 0:
        return list(await asyncio.gather(*tasks))
    done, pending = await asyncio.wait(tasks, timeout=deadline)
    if pending:
        answered = any(
            isinstance(t.result()[1], list) and t.result()[1] for t in done if not t.cancelled()
        )
        if not answered:
            lane_open = {task for task in lane if not task.done()}
            while pending and not _lane_found(lane):
                more, _ = await asyncio.wait(
                    pending | lane_open, return_when=asyncio.FIRST_COMPLETED
                )
                lane_open -= more
                finished = more & pending
                done |= finished
                pending -= finished
            if pending:
                # Cut because a lane answer settled the search, not because
                # another keyless engine had answered (see `aggregate_search`).
                diagnostics["cut_by_lane"] = sorted(tasks[t] for t in pending)
    results: list[tuple[str, list[SearchResult] | Exception]] = []
    for task in pending:
        task.cancel()
    if pending:
        # A cancelled engine may be inside a browser render whose teardown
        # takes seconds; give it one and let the rest finish on its own.
        # Measured 2026-09-22: waiting for the teardown made a 10 s deadline
        # a 15 s search.
        await asyncio.wait(pending, timeout=1.0)
        late = sorted(tasks[t] for t in pending)
        diagnostics["timed_out"] = late
        log.info("search deadline %.1fs passed; cancelled %s", deadline, late)
    for task in done:
        results.append(task.result())
    for task in pending:
        results.append(
            (tasks[task], TimeoutError(f"no answer within the {deadline:g}s search deadline"))
        )
    order = {name: i for i, name in enumerate(engine_names)}
    results.sort(key=lambda item: order[item[0]])
    return results


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
    # `nominal` is what this search is entitled to and what the cache is keyed
    # on; `engine_names` is what actually gets asked. They differ only for the
    # automatic pool, and only while the breaker has something benched: naming
    # engines, or an exclusive category, is an instruction and is followed.
    benched: dict[str, dict[str, Any]] = {}
    auto_pool = not engines and not _is_exclusive(category)
    if engines:
        nominal = list(engines)
        engine_names = nominal
    else:
        nominal = _nominal_pool(query, category, freshness)
        engine_names, benched = _active_pool(nominal) if auto_pool else (nominal, {})
    # The agent lane joins the automatic pool only: naming engines is an
    # instruction, and an image or dataset search wants records, not pages.
    lane_nominal = _lane_nominal(nominal) if auto_pool else []
    lane = _seated(lane_nominal)
    routed = (
        [name for name in engine_names if name not in settings.default_engines]
        if auto_pool and not category
        else []
    )
    routed = [name for name in routed if name in claimants(query)]
    # `or` would have turned an explicit 0 into the default — a caller who
    # asked for nothing got ten results and no indication anything was ignored.
    # `None` still means "use the configured default"; a number is clamped into
    # a range the fan-out can actually honour, since every engine is queried
    # for `n` and a four-figure request buys duplicate noise, not recall.
    n = (
        settings.max_results_per_engine
        if max_results is None
        else max(1, min(int(max_results), _MAX_RESULTS))
    )
    # ONE place normalizes the category token, and it is here. Routing above
    # uses the caller's raw token (so `paper.biomed` reaches only the engines
    # that declare it), while everything downstream — the post-filter branches,
    # `finalize_results`, and the eleven engines that special-case
    # `category == "pdf"` — only ever sees a bare group. Splitting inside the
    # post-filter instead would have made "category may be dotted" an invariant
    # that exactly one file honoured.
    filters = SearchFilters(
        freshness=freshness,
        include_domains=list(include_domains) if include_domains else [],
        exclude_domains=list(exclude_domains) if exclude_domains else [],
        category=category_group(category),
        category_token=category,
        include_text=include_text,
        exclude_text=exclude_text,
    )
    cache_key = _key(query, nominal + lane_nominal, n, filters)

    # Read-bypass and cache-WRITE are decoupled. `use_cache` gates BOTH the read
    # and the write; `max_age_seconds` only tightens the read TTL. So a caller
    # passing max_age_seconds=0 (force-refresh) still writes the fresh result
    # back — caching is never silently disabled by a freshness request.
    #   max_age_seconds is None  -> read with the server default TTL.
    #   max_age_seconds == 0     -> always a read miss (force-refresh).
    #   max_age_seconds > 0      -> read only if the row is younger than that.
    if use_cache and max_age_seconds != 0:
        cached = await cache.get_search(
            cache_key, max_age_seconds=_read_ttl(freshness, category, max_age_seconds)
        )
        if cached and _too_degraded_to_replay(cached[1]):
            cached = None
        if cached:
            hit, meta = cached
            # A4: recompute lead_snippet from the cached results so the rendered
            # markdown keeps its '> **Lead:**' block. filter_diagnostics can't be
            # rebuilt from results alone (it needs the per-engine raw/drop tallies
            # that only exist on a fresh run), so it is intentionally fresh-only.
            payload = {
                "query": query,
                # The engines that produced THESE results, which is not
                # necessarily the pool a fresh run would use right now.
                "engines": meta.get("engines") or nominal + lane_nominal,
                "cached": True,
                "results": _annotate(hit),
                "lead_snippet": _lead_snippet(query, hit),
            }
            cached_at = float(meta.get("cached_at") or time.time())
            payload.update(_freshness_signals(hit, freshness, cached_at))
            payload["cache_key"] = cache_key
            payload["cache_age_seconds"] = max(0, int(time.time() - cached_at))
            if meta.get("benched_engines"):
                payload["benched_engines"] = meta["benched_engines"]
                payload["benched_hint"] = meta.get("benched_hint") or ""
            # Provenance, unlike run statistics, describes the RESULTS — and the
            # results are exactly what we just replayed. A set that only exists
            # because a captcha-walled engine was rescued by a reserve has to say so
            # every time it is served, or the second identical query inside the
            # 7-day TTL silently re-labels a recovered set as a normal one.
            gated = meta.get("gated_engines")
            if gated:
                payload["gated_engines"] = gated
                payload["gated_hint"] = meta.get("gated_hint") or ""
            if meta.get("rescued_via"):
                payload["rescued_via"] = meta["rescued_via"]
            if meta.get("auto_routed"):
                payload["auto_routed"] = meta["auto_routed"]
            missing = _lane_names(meta.get("lane_missing"), lane_nominal)
            left_out = _lane_names(meta.get("lane_left_out"), lane_nominal)
            # Only what is known of this replay: which lane engine it lacks.
            replay: dict[str, Any] = {}
            notes: list[str] = []
            if missing:
                replay["missing"] = missing
                notes.append(
                    f"This result set was cached without {', '.join(missing)}, which had "
                    "failed, sat out or not answered yet. It is replayed for an hour at "
                    "most; pass max_age_hours=0 to search again now."
                )
            if left_out:
                replay["left_out"] = left_out
                notes.append(
                    f"The answer from {', '.join(left_out)} was left out of this result set "
                    "as off-topic or unconfirmed."
                )
            if replay:
                payload["agent_lane"] = replay
                payload["agent_lane_hint"] = " ".join(notes)
            return payload

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
            log.warning("engine %s failed: %s", name, brief_error(e))
            return name, e

    # The agent lane starts with the keyless fan-out, not after it.
    lane_started = time.monotonic()
    lane_tasks = {
        asyncio.ensure_future(_run_lane(name, query, n, filters)): name for name in lane
    }
    early_rescue: asyncio.Future[tuple[list[SearchResult], str | None]] | None = None
    # Until the still-running lane engines are handed to the late merge or
    # cancelled below, an exception (or the caller cancelling) must not leave
    # them, or an early rescue, running unowned.
    try:
        results = await _fan_out(run, engine_names, diagnostics, lane_tasks)
        answered = any(isinstance(res, list) and res for _, res in results)
        if (
            not answered
            and any(not task.done() for task in lane_tasks)
            and not _lane_found(lane_tasks)
            and settings.rescue_enabled
            and not _is_exclusive(category)
        ):
            # Nothing keyless answered and the lane is still searching. The
            # rescue such a run nearly always needs starts now, beside the
            # lane wait rather than after it, and is cancelled below if the
            # lane's answer turns out to be enough.
            early_rescue = asyncio.ensure_future(
                _rescue(query, n, filters, engine_names, diagnostics)
            )
        lane_done, lane_pending, lane_waited = await _await_lane(
            lane_tasks, lane_started, answered, early_rescue
        )
        named: list[tuple[str, list[SearchResult]]] = []
        errors: dict[str, str] = {}
        raised: dict[str, Exception] = {}
        for name, res in results:
            if isinstance(res, Exception):
                errors[name] = brief_error(res)
                raised[name] = res
            else:
                named.append((name, res))
        # Lane outcomes join the run's buckets, so the off-topic guard and the
        # merge treat them like any web engine (and they can be the witness that
        # exposes a keyless decoy). Their failures are reported in `agent_lane`,
        # not `errors`: nobody named them, and the advice `errors` produces is
        # about the engines the caller chose.
        lane_errors: dict[str, str] = {}
        lane_raised: dict[str, Exception] = {}
        lane_rate_limited: list[str] = []
        for name, res, lane_diag in lane_done:
            if lane_diag.get("rate_limited"):
                lane_rate_limited.append(name)
            elif isinstance(res, Exception):
                lane_errors[name] = brief_error(res)
                lane_raised[name] = res
            else:
                named.append((name, res))
        still_searching = {lane_tasks[task] for task in lane_pending}
        lane_pending_names = [name for name in lane if name in still_searching]

        # Before the merge, not after: RRF has no notion of a bad bucket, and would
        # interleave a decoy's ten results at ranks 3, 6, 9 of the answer.
        named, unconfirmed = _drop_decoy_buckets(query, named, diagnostics)
        # The guard's verdicts on lane buckets are the lane's to report and
        # never the keyless pool's: they leave the shared `gated` here, so the
        # rescue, the census and the gate hint see keyless engines only.
        lane_off_topic = _take_lane_verdicts(diagnostics, lane)
        # Lane suspects are settled after the rescue (`_settle_lane_suspects`);
        # the rescue's own decisions see keyless suspects only.
        lane_suspects = [name for name in unconfirmed if name in lane]
        unconfirmed = [name for name in unconfirmed if name not in lane]
        buckets = [bucket for _, bucket in named]
        merged = _merge(buckets, n, category, query)

        # Keyless rescue: one bounded recovery attempt when the run came back
        # empty or nearly-empty with demonstrably unhealthy engines. Rescue
        # results join the RRF merge (any partial default results keep their
        # weight and attribution stays honest via each result's `engines`), and
        # they flow into the cache write below like any other result — a repeat
        # query within TTL should not re-pay the rescue.
        rescued_via: str | None = None
        rescue_pair: list[tuple[str, list[SearchResult]]] = []
        # Stragglers the fan-out cut short because a lane answer settled the
        # search do not call for a rescue: the search has its answer. They did
        # run past the deadline, so the breaker still hears of it as of any
        # engine cut there. Engines cut by the ordinary deadline, walled,
        # refused or failed still call for breadth.
        cut_by_lane = set(diagnostics.get("cut_by_lane") or ())
        lost, standing = _general_census(
            engine_names,
            named,
            {k: v for k, v in raised.items() if k not in cut_by_lane},
            diagnostics,
        )
        # An early rescue that has already come back is used whatever the
        # lane brought: its requests are spent, and its index adds breadth.
        early_ready = (
            early_rescue is not None
            and early_rescue.done()
            and not early_rescue.cancelled()
            and bool(early_rescue.result()[0])
        )
        if early_ready or _needs_rescue(
            merged,
            {k: v for k, v in errors.items() if k not in cut_by_lane},
            diagnostics,
            category,
            seek_witness=len(unconfirmed) == 1,
            # Only the automatic pool promises breadth. Someone who named two
            # engines and lost one asked for two engines, not for three indexes.
            lost_general=lost if auto_pool else 0,
            healthy_general=standing,
        ):
            if early_rescue is not None:
                rescue_bucket, rescued_via = await early_rescue
            else:
                rescue_bucket, rescued_via = await _rescue(
                    query, n, filters, engine_names, diagnostics
                )
            if rescue_bucket:
                if unconfirmed and is_witness(query, rescue_bucket):
                    # The second opinion is in, and it echoes the query: the
                    # suspects were decoys after all.
                    _mark_off_topic(unconfirmed, diagnostics)
                    named = [(name, bucket) for name, bucket in named if name not in unconfirmed]
                    buckets = [bucket for _, bucket in named]
                buckets.append(rescue_bucket)
                if rescued_via:
                    rescue_pair = [(rescued_via, rescue_bucket)]
                merged = _merge(buckets, n, category, query)
                # Attribute the recovery to the gated engines so the gate hint
                # reads "was captcha-gated → served via <reserve>" instead of the
                # misleading "(no results)".
                if rescued_via:
                    fb = diagnostics.setdefault("fallback", {})
                    for name in diagnostics.get("gated", {}):
                        fb.setdefault(name, rescued_via)
        elif early_rescue is not None:
            early_rescue.cancel()

        # Every bucket of the run is in: settle the lane suspects, with the
        # rescue bucket as the witness or the solid answer they may lack.
        rescue_witness = bool(rescue_pair) and is_witness(query, rescue_pair[0][1])
        settled, late_off_topic, doubted = _settle_lane_suspects(
            named + rescue_pair, lane_suspects + unconfirmed, lane, witness=rescue_witness
        )
        if late_off_topic or doubted:
            named = [pair for pair in settled if pair not in rescue_pair]
            buckets = [bucket for _, bucket in settled]
            merged = _merge(buckets, n, category, query)
        lane_off_topic += late_off_topic
        lane_left_out = lane_off_topic + doubted

        # Lane buckets are left out: an engine that legitimately found nothing
        # must not take a "silent" strike because a language model's search
        # found pages no keyless index has.
        _record_health(
            engine_names, [(n, b) for n, b in named if n not in lane], raised, diagnostics
        )
        for name, res, lane_diag in lane_done:
            # A doubted answer is no evidence either way.
            if not lane_diag.get("rate_limited") and name not in doubted:
                _record_lane_health(name, res, "off_topic" if name in lane_off_topic else None)
        if rescued_via and _is_guarded(rescued_via):
            standing += 1
        # An answer resting on too few indexes is served, and cached — but not for
        # a week: the point of the breaker is that the pool recovers.
        degraded = auto_pool and standing < settings.min_healthy_engines
        benched_hint = _benched_hint(benched) if benched else ""

        # Gate/fallback provenance is computed BEFORE the cache write so it can be
        # stored alongside the results it describes and replayed on a later hit.
        gated = diagnostics.get("gated") or {}
        gated_engines, gated_hint = _gate_provenance(gated, diagnostics.get("fallback") or {})

        # A named engine waiting on its sign-in page did not get to answer; a
        # cached run without it would be replayed after the approval.
        sign_in_pending = any(isinstance(e, EngineSignInPending) for e in raised.values())
        # What was asked and has answered: the keyless engines, then the agent
        # engines that finished in time. One still searching is not listed yet; it
        # joins the row when its results are merged in.
        asked = engine_names + [
            name for name in lane if name not in still_searching and name not in lane_rate_limited
        ]
        meta: dict[str, Any] = {}
        if gated_engines:
            meta["gated_engines"] = gated_engines
            meta["gated_hint"] = gated_hint
        if rescued_via:
            meta["rescued_via"] = rescued_via
        if asked != nominal + lane_nominal:
            meta["engines"] = asked
        if benched:
            meta["benched_engines"] = benched
            meta["benched_hint"] = benched_hint
        if degraded:
            meta["degraded"] = True
        if routed:
            meta["auto_routed"] = routed
        lane_benched = [name for name in lane_nominal if name not in lane]
        # The lane engines the cache key promises and these results lack for a
        # passing reason: still searching, out of tokens, a timeout or reset,
        # or benched after one. One refused for its plan or its sign-in, or
        # benched for that, is a lasting state; marking those too would turn
        # every cached search into an hourly re-run for as long as a plan
        # stays spent.
        lane_missing = [
            name
            for name in lane_nominal
            if name in still_searching
            or name in lane_rate_limited
            or (name in lane_raised and not is_lasting(lane_raised[name]))
            or (name in lane_benched and _benched_for(name) != LANE_REFUSED)
        ]
        if lane_missing:
            meta["lane_missing"] = lane_missing
        if lane_left_out:
            # So a replay can say that a lane engine was asked and its answer
            # left out, rather than pass the set off as including its view.
            meta["lane_left_out"] = lane_left_out
        written_at = int(time.time())
        if use_cache and merged and not sign_in_pending:
            if lane_nominal or cache_key in _lane_rows:
                # A row this run writes is the newest answer: a late merge an
                # earlier search left running must not overwrite it. That holds
                # for any search with the same key, a call naming the very
                # engines an automatic search asked included. A run that writes
                # nothing leaves the merge its row. The row lock makes taking
                # the row and writing it one step, so the merge cannot slip a
                # write in between; a search with no lane and no merge for its
                # key has nothing to take and skips it.
                async with _row_lock():
                    _lane_rows.pop(cache_key, None)
                    await cache.put_search(cache_key, query, asked, merged, meta or None)
            else:
                await cache.put_search(cache_key, query, asked, merged, meta or None)
        stored_as = cache_key if use_cache and merged and not sign_in_pending else None
    except BaseException:
        # Cancelling a finished task is a no-op, so all of them will do.
        for task in lane_tasks:
            task.cancel()
        if early_rescue is not None:
            early_rescue.cancel()
        raise
    merges_later = bool(lane_pending) and use_cache and not sign_in_pending
    if merges_later:
        # Not cancelled: the request is already spent, and its answer makes
        # the next identical search better. Started after the write above, so
        # the late merge always lands on top of it.
        _merge_lane_later(
            lane_pending,
            query=query,
            n=n,
            category=category,
            named=named + rescue_pair,
            keyless=engine_names + [name for name, _ in rescue_pair],
            asked=asked,
            cache_key=cache_key,
            meta=meta,
            missing=lane_missing,
            written_at=written_at if stored_as else None,
            pending_names=lane_pending_names,
        )
    else:
        # Without the cache there is no row to merge a late answer into, so
        # nothing is gained by letting it finish.
        for task in lane_pending:
            task.cancel()

    payload: dict[str, Any] = {
        "query": query,
        "engines": asked,
        "cached": False,
        "results": _annotate(merged),
        "lead_snippet": _lead_snippet(query, merged),
        "errors": errors or None,
    }
    payload.update(_freshness_signals(merged, freshness, time.time()))
    if stored_as:
        # Present only when the row exists: it is the handle of the
        # `cache://search/{query_hash}` resource, and a handle to nothing is a lie.
        payload["cache_key"] = stored_as
    if rescued_via:
        payload["rescued_via"] = rescued_via
    # Record sources seated because they claimed the question (no category
    # was named). Listed so a reader can see why `engines` holds more than
    # the default pool, and which of them found a record.
    if routed:
        payload["auto_routed"] = routed
    timed_out = diagnostics.get("timed_out") or []
    if timed_out:
        payload["timed_out_engines"] = timed_out
        payload["timed_out_hint"] = (
            f"{', '.join(timed_out)} had not answered within the "
            f"{settings.search_deadline_seconds:g}s search deadline and were cancelled; "
            "the results above come from the engines that did answer."
        )
    # Engines that were NOT asked, because they failed recently. `engines`
    # above lists what ran; without this a benched default would simply vanish.
    if benched:
        payload["benched_engines"] = benched
        payload["benched_hint"] = benched_hint
    if lane_nominal:
        lane_report: dict[str, Any] = {
            "engines": lane,
            "answered": {name: len(bucket) for name, bucket in named if name in lane},
        }
        if lane_pending_names:
            lane_report["pending"] = lane_pending_names
            lane_report["merges_into_cache"] = merges_later
        if lane_errors:
            lane_report["errors"] = lane_errors
        if lane_rate_limited:
            lane_report["rate_limited"] = lane_rate_limited
        if lane_benched:
            lane_report["benched"] = lane_benched
        if lane_left_out:
            lane_report["left_out"] = lane_left_out
        payload["agent_lane"] = lane_report
        lane_hint = _lane_hint(
            lane_pending_names,
            lane_errors,
            lane_rate_limited,
            waited=lane_waited,
            merges_later=merges_later,
            off_topic=lane_off_topic,
            doubted=doubted,
        )
        if lane_hint:
            payload["agent_lane_hint"] = lane_hint

    # Engines the rate limiter refused a token to. Recorded since the limiter
    # was added but never read, so a source silently vanished from a search it
    # was listed in: `gdelt` (6/min, max_wait 1.0s) drops out of a second news
    # query with no error, no `empty_engines` entry (it never reached
    # `engine.search`, so it has no `raw_per_engine` row either) and no hint,
    # while still appearing in `payload["engines"]`. Reported unconditionally —
    # the key is absent unless an engine was actually skipped, and losing a
    # source matters whether or not the remaining ones found plenty.
    rate_limited = diagnostics.get("rate_limited") or []
    if rate_limited:
        payload["rate_limited_engines"] = sorted(rate_limited)
        payload["rate_limited_hint"] = (
            f"{', '.join(sorted(rate_limited))} did not run: no rate-limit token was "
            "available within the wait this engine allows. This is throttling and says "
            "nothing about whether results exist. Retry in a minute for that source's "
            "coverage."
        )

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
    # caller learns WHY an engine returned nothing — and whether a rescue
    # covered it — instead of seeing a silent gap.
    if gated_engines:
        payload["gated_engines"] = gated_engines
        payload["gated_hint"] = gated_hint

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
                f"{detail}: the source refused the request, which is different from "
                "finding no matches. 429 means back off and retry later; "
                "5xx means the source is down."
            )
        if empty:
            hints.append(
                f"{', '.join(empty)} returned 0 results with no error and no "
                "CAPTCHA/consent wall detected. This may be a silent IP block or a "
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
