import asyncio
import base64
import binascii
import logging
import time
import warnings
from dataclasses import dataclass
from typing import Any
from urllib.parse import parse_qs, quote_plus, urlparse

from curl_cffi.requests import AsyncSession

from ..coherence import looks_like_decoy
from ..config import settings
from ..httpfetch import IMPERSONATE
from ..net import curl_proxy_kwargs
from .base import (
    Engine,
    SearchFilters,
    SearchResult,
    _region_to_bing_market,
    augment_query_with_operators,
    extract_date_hint,
    parse_html,
    safesearch_param,
    text_of,
)

log = logging.getLogger(__name__)

# Bing sets a `__Host-` cookie. Seeding a session from a plain dict makes
# curl_cffi normalise it (host-only, path "/") and warn that it did — once per
# search, about a rewrite that is exactly what the cookie prefix requires.
warnings.filterwarnings("ignore", message=r"`host` changed to True", module=r"curl_cffi\..*")

# Every organic Bing result now arrives as a click-tracking redirect
# (`www.bing.com/ck/a?…&u=a1<base64url>&…`) rather than as the target URL. Left
# alone, that breaks three things at once: `_host()` reports "www.bing.com", so
# `include_domains` / `exclude_domains` and every host-based `category` filter
# silently discard ALL of this engine's results; the same page found by another
# engine never dedupes against it, so RRF cannot reward the agreement; and the
# caller is handed an opaque blob instead of a link. The `u` parameter is the
# target, base64url-encoded behind a two-character tag ("a1" in practice).


def resolve_bing_url(raw_url: str) -> str:
    """Unwrap a bing.com/ck/a click-tracking redirect to the publisher URL.

    Leaving the wrapper in place is not cosmetic. The blob is unique per SERP
    impression, so it defeats both the URL-keyed RRF merge and _dedup_by_title
    in the aggregator: the same page found by Bing and by DuckDuckGo is scored
    as two different results and both are emitted, spending the caller's
    max_results on duplicates. It also hands the model a link that says nothing
    about the publisher and cannot be judged for relevance without fetching it.

    Only ck/a URLs are touched, and only a decoded absolute http(s) URL is
    accepted: `u=` is an ordinary parameter name that other sites use for their
    own purposes, and rewriting one of those to whatever its value happens to
    base64-decode into would corrupt a perfectly good link. Anything else is
    returned unchanged — a working redirect beats dropping the result.
    """
    if "bing.com/ck/a" not in raw_url:
        return raw_url
    encoded = parse_qs(urlparse(raw_url).query).get("u", [""])[0]
    # Two-character type tag, then the payload.
    if len(encoded) < 3:
        return raw_url
    payload = encoded[2:]
    # base64url -> base64, re-padded to a multiple of 4.
    padded = payload.replace("-", "+").replace("_", "/")
    padded += "=" * (-len(padded) % 4)
    try:
        decoded = base64.b64decode(padded).decode("utf-8")
    except (binascii.Error, UnicodeDecodeError, ValueError):
        return raw_url
    return decoded if decoded.startswith(("http://", "https://")) else raw_url


# ---------------------------------------------------------------------------
# The session Bing expects
# ---------------------------------------------------------------------------
#
# A Bing SERP request that arrives with no cookies is not refused. It is
# answered — HTTP 200, ten `li.b_algo` results, valid markup — with results
# that match only the FIRST token of the query. "rust ownership borrowing"
# comes back as the Steam page for the game Rust. Nothing structural can tell
# that page from a real one, which is why it sat in the default pool for weeks
# (it is already in the 2026-08-29 ranking capture) before anyone saw it.
#
# What was measured on 2026-09-21, one fresh query per variant because Bing
# caches per query server-side and a repeated query replays the first answer:
#
#     as shipped: `?q=…&count=10`, no cookies            decoy   0/10 on topic
#     cookies only / `form=QBRE` only / mkt / headers    decoy
#     home-page cookies + `form=QBRE`, and NO `count=`   real    9-10/10
#     the same, plus `count=10`                          decoy
#     the same, plus `first=11` / `mkt=` / `filters=`    real
#
# So three things carry weight: a cookie jar minted by a GET of the home page,
# `form=QBRE` (the form id of a query typed into the results page), and the
# ABSENCE of `count=`, which no browser sends. The jar is reusable across
# queries and across sessions, so it is minted once and shared.
#
# This is best effort, and the code below is written as if it will stop working
# — because request shapes like this do. The durable protection is the
# whole-bucket coherence check: here (re-mint and retry once) and again in the
# aggregator, which drops a bucket that is still a decoy and says so.

_WARM_URL = "https://www4.bing.com/"
_JAR_TTL = 1800.0
# A failed warm-up is remembered briefly, so a Bing outage costs one extra
# request a minute rather than one per search.
_WARM_FAIL_TTL = 60.0


@dataclass
class _Jar:
    cookies: dict[str, str]
    minted_at: float
    generation: int

    def fresh(self, now: float) -> bool:
        return now - self.minted_at < (_JAR_TTL if self.cookies else _WARM_FAIL_TTL)


# Keyed by proxy egress: cookies minted from one exit IP and replayed from
# another are exactly the inconsistency this exists to avoid.
_JARS: dict[str, _Jar] = {}
# Single-flight, the way gnews.py does it. Deliberately not an asyncio.Lock: a
# module-level lock binds to the first event loop that touches it, and pytest
# runs every test on a new one.
_MINTING: dict[str, asyncio.Future] = {}
_generation = 0


def _reset_jar() -> None:
    """Forget every minted session. For tests."""
    global _generation
    _JARS.clear()
    _MINTING.clear()
    _generation = 0


def _egress_key(proxy_kwargs: dict[str, Any]) -> str:
    return str(proxy_kwargs.get("proxy") or "")


async def _warm_cookies(impersonate: str, proxy_kwargs: dict[str, Any]) -> dict[str, str]:
    """GET the home page and return the cookies it set. Never raises: with no
    jar the search still runs, and the coherence check judges what comes back."""
    try:
        async with AsyncSession(
            impersonate=impersonate,
            timeout=settings.request_timeout,
            allow_redirects=True,
            headers={
                "Accept-Language": settings.accept_language,
                "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            },
            **proxy_kwargs,
        ) as client:
            await client.get(_WARM_URL)
            return {c.name: c.value for c in client.cookies.jar if c.value is not None}
    except Exception as exc:
        log.debug("bing session warm-up failed: %s", exc)
        return {}


async def _session(impersonate: str, proxy_kwargs: dict[str, Any]) -> _Jar:
    """The shared jar for this egress, minting it if absent or expired."""
    global _generation
    key = _egress_key(proxy_kwargs)
    jar = _JARS.get(key)
    if jar is not None and jar.fresh(time.monotonic()):
        return jar

    minting = _MINTING.get(key)
    if minting is not None:
        return await minting

    fut: asyncio.Future = asyncio.get_running_loop().create_future()
    _MINTING[key] = fut
    try:
        cookies = await _warm_cookies(impersonate, proxy_kwargs)
        _generation += 1
        jar = _Jar(cookies=cookies, minted_at=time.monotonic(), generation=_generation)
        _JARS[key] = jar
        if not fut.done():
            fut.set_result(jar)
        return jar
    except BaseException:
        # Settle the waiters rather than orphan them; see gnews.py.
        if not fut.done():
            fut.set_result(_Jar(cookies={}, minted_at=time.monotonic(), generation=_generation))
        raise
    finally:
        _MINTING.pop(key, None)


def _invalidate(proxy_kwargs: dict[str, Any], generation: int) -> None:
    """Drop the jar — but only the one the caller was actually served with.

    Four searches that all drew a decoy from generation 7 must produce ONE
    re-mint, not four: the first drops generation 7, the other three find
    generation 8 already in place and leave it alone."""
    key = _egress_key(proxy_kwargs)
    jar = _JARS.get(key)
    if jar is not None and jar.generation == generation:
        _JARS.pop(key, None)


# Bing's documented freshness filter values.
_BING_FRESHNESS = {
    "day": 'ex1:"ez1"',
    "week": 'ex1:"ez2"',
    "month": 'ex1:"ez3"',
    "year": 'ex1:"ez4"',
}


class BingEngine(Engine):
    name = "bing"
    description = "Microsoft Bing web results over plain HTTP: broad index, no browser needed."
    # The www4 edge serves real organic results over plain HTTP in about two
    # seconds (one warm-up GET per half hour, then one request per search), so
    # we try HTTP FIRST and only pay for a Playwright render when parse() comes
    # back empty (a real gate) via the inherited supports_browser_fallback.
    # wait_selector still applies to the fallback render.
    needs_browser = False
    # Match the actual result item; #b_results is the empty container that
    # exists immediately and would short-circuit the wait.
    wait_selector = "li.b_algo"
    # Ask Bing as Edge — Microsoft's own browser is the client its SERP is
    # built and tested against, and the identity the warmed cookie jar was
    # issued to. It does not by itself avoid the decoy page (measured: neither
    # Edge nor Chrome does without the jar); it keeps the fingerprint coherent.
    impersonate = "edge"

    # No search() override: a raise (e.g. a www4 non-200 under
    # fetch_strategy="http") lands in the aggregator's per-engine `errors` map —
    # visible, and enough to trigger the rescue pass — instead of being
    # swallowed into a fake "silent zero" the empty-engine hint would then
    # mislabel as "no error". What IS overridden is the fetch-and-parse seam,
    # because Bing's failure mode is a page that parses perfectly.

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        # www.bing.com aggressively challenges headless clients ("something went
        # wrong" page). The www4 edge serves the same index without that gate.
        #
        # `form=QBRE` is load-bearing and `count=` is poison — see the table at
        # the top of this module. (`form=QBLH` is a different, lighter layout
        # with no `.b_algo` at all.) More than ten results come from a second
        # page in `_raw_results`, never from `count=`. If this shape stops
        # working, the full set a browser sends is
        # `form=QBRE&sp=-1&lq=0&pq=<query>&sc=<n>-<len>&qs=n&sk=&cvid=<32 hex>`;
        # none of the extras changed the outcome when measured.
        filetype = None
        if filters and filters.category == "pdf":
            filetype = "pdf"
        q = augment_query_with_operators(
            query,
            include_domains=filters.include_domains if filters else None,
            exclude_domains=filters.exclude_domains if filters else None,
            filetype=filetype,
        )
        url = f"https://www4.bing.com/search?q={quote_plus(q)}&form=QBRE"
        if filters and filters.freshness:
            url += f"&filters={quote_plus(_BING_FRESHNESS[filters.freshness])}"
        # SafeSearch: adlt=strict|moderate|off maps 1:1 to our setting.
        adlt = safesearch_param(self.name)
        if adlt is not None:
            url += f"&adlt={adlt}"
        # Region -> Bing market code, e.g. us-en -> en-US, uk-en -> en-GB.
        if settings.region:
            url += f"&mkt={quote_plus(_region_to_bing_market(settings.region))}"
        return url

    async def _http_get(self, url: str, *, cookies: dict[str, str] | None = None) -> str:
        if cookies is None:
            jar = await _session(self.impersonate or IMPERSONATE, curl_proxy_kwargs(self.name))
            cookies = jar.cookies or None
        return await super()._http_get(url, cookies=cookies)

    async def _raw_results(
        self,
        query: str,
        max_results: int,
        filters: SearchFilters | None = None,
        diagnostics: dict[str, Any] | None = None,
    ) -> tuple[list[SearchResult], str]:
        proxy_kwargs = curl_proxy_kwargs(self.name)
        served_by = _JARS.get(_egress_key(proxy_kwargs))
        results, html = await super()._raw_results(query, max_results, filters, diagnostics)

        if looks_like_decoy(query, results):
            # The session went stale, or never took. Re-mint and ask once more.
            # If that is a decoy too, return it as it is: the aggregator runs
            # the same check, drops the bucket and reports why, and doing that
            # in one place keeps the reporting honest.
            jar = served_by or _JARS.get(_egress_key(proxy_kwargs))
            if jar is not None:
                _invalidate(proxy_kwargs, jar.generation)
            log.info("bing returned a decoy page for %r; re-minting the session", query)
            results, html = await super()._raw_results(query, max_results, filters, diagnostics)
            if looks_like_decoy(query, results):
                return results, html

        if max_results > 10 and results:
            results = await self._with_second_page(query, max_results, filters, results)
        return results, html

    async def _with_second_page(
        self,
        query: str,
        max_results: int,
        filters: SearchFilters | None,
        first_page: list[SearchResult],
    ) -> list[SearchResult]:
        """Results 11-20, by pagination — the way a person gets them.

        `first=11` keeps the real-results behaviour (measured: 14/14 on topic,
        two overlapping page one). A failure here costs the extra results and
        nothing else."""
        try:
            html = await self._fetch(self.build_url(query, max_results, filters) + "&first=11")
            more = self.parse(html)
        except Exception as exc:
            log.debug("bing second page failed: %s", exc)
            return first_page
        seen = {r.url for r in first_page}
        return first_page + [r for r in more if r.url not in seen]

    def parse(self, html: str) -> list[SearchResult]:
        tree = parse_html(html)
        results: list[SearchResult] = []
        seen: set[str] = set()
        # Guard against a SERP repeating a URL (and against a future markup
        # change re-introducing a double match): a duplicate inside one bucket
        # scores twice in the aggregator's RRF merge, inflating this engine's
        # weight, and eats a slot in the max_results budget.
        for li in tree.css("li.b_algo"):
            link = li.css_first("h2 a")
            if not link:
                continue
            url = resolve_bing_url(link.attributes.get("href", ""))
            title = text_of(link)
            snippet_node = (
                li.css_first(".b_caption p")
                or li.css_first(".b_lineclamp4")
                or li.css_first(".b_lineclamp2")
                or li.css_first(".b_paractl")
            )
            snippet = text_of(snippet_node)
            if not url or not title or url in seen:
                continue
            seen.add(url)
            result = SearchResult(title=title, url=url, snippet=snippet, engine=self.name, rank=0)
            hint = extract_date_hint(snippet) or extract_date_hint(title)
            if hint:
                result.published_age = hint
            results.append(result)
        return results
