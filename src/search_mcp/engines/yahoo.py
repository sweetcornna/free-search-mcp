"""Yahoo Search (search.yahoo.com) — Bing's web index, HTML scrape, no API key.

Yahoo licenses Bing's organic results but serves them from its own front end,
so this is a second route to that index with failure modes of its own. The one
that matters: Bing answers a cookieless request with an off-topic decoy page
(see engines/bing.py), and Yahoo did not — every query tried on 2026-10-31
came back on topic, including "人工智能 最新进展", which returned Chinese
sources (Sina, 36kr). About seven organic results a page.

Organic results are `div.algo` rows inside `ol.searchCenterMiddle`. The title
is the row's `h3`: the anchor also wraps a favicon, the site name and a
breadcrumb, so its own text is not the title. The snippet is `div.compText`
and leads with Yahoo's date ("Jul 23, 2025 · ", "1 day ago · "), which
`extract_date_hint` reads.

Every href is a click-tracking redirect:

    https://r.search.yahoo.com/_ylt=…;_ylu=…/RV=2/RE=…/RO=10/RU=https%3a%2f%2fexample.com%2fpage/RK=2/RS=…

`RU=` is the target, percent-encoded once. Its own slashes arrive as `%2f`, so
the segment ends at the next literal "/", and it is decoded EXACTLY once: a
target that itself contains `%2B` arrives as `%252B`, and a second decode would
turn a real `.../C%2B%2B` link into `C++` and break it (the DuckDuckGo
unwrapper documents the same trap).

Ads are `div.ads` rows in `ol.searchCenterTopAds` / `searchCenterBottomAds`,
linking through `r.search.yahoo.com/rdclks/…` to bing.com/aclick. `div.algo`
never matches them, and the checks below drop anything that still looks like
an ad click or a Yahoo page (search verticals, Scout, the "Your Privacy
Choices" link to guce.yahoo.com).

URL parameters, measured against the unfiltered ranking:

* freshness is `btf=d|w|m`, what the page's "Past day/week/month" links send.
  `age=1w` on its own is ignored, and so is `btf=y`: there is no year option,
  so year is left to the post-filter.
* `site:`, `-site:` and `filetype:pdf` in the query are honoured.
* `n=` is ignored (seven results whatever is asked for), and `vm=r|i|p`, the
  SafeSearch values Yahoo has used, returned identical results for a borderline
  query, so neither is sent.

The `_bv` handshake. A request with no cookies is answered with a 307 to
`/_bv/v.gif?orig=…` (or at once with an empty HTTP 500). Each hop sets a
`YBV` cookie, valid for 60 s while the check is in progress and for a day once
it is satisfied, and the last hop redirects back to the search. curl follows
that within one request, but the handshake does not always complete: from a
fresh session per search, only 3 of 8 got results and the rest ended in the
empty 500. Once it has completed, the same cookies are accepted for other
queries with no redirect at all (5 of 5, about 0.6 s each). So, as with Bing's
jar, the cookies from one completed handshake are kept per proxy egress and
sent with every search, and a fresh handshake (up to three GETs in one
session, see `_HANDSHAKE_ATTEMPTS`) runs only when there is no jar or the jar
is rejected. Failed handshakes grew more frequent as this machine sent more
requests (about a hundred in half an hour of probing), and a day-long token
was once turned away straight after it was issued, so Yahoo is scoring the
client as well as the cookie: the jar also keeps that count down.

Best-effort by nature: this parses a page meant for humans, and the selectors
will need revisiting whenever Yahoo reshuffles its markup. An unparseable page
yields `[]` rather than an error, per the keyless-engine contract.
"""

from __future__ import annotations

import logging
import re
import time
from dataclasses import dataclass
from typing import Any
from urllib.parse import quote_plus, unquote, urlparse

from curl_cffi.requests import AsyncSession as CurlAsyncSession
from curl_cffi.requests.exceptions import RequestException, Timeout

from ..config import settings
from ..httpfetch import IMPERSONATE
from ..net import curl_proxy_kwargs
from .base import (
    Engine,
    SearchFilters,
    SearchResult,
    augment_query_with_operators,
    extract_date_hint,
    host_matches,
    inline_text,
    parse_html,
)

log = logging.getLogger(__name__)

_ENDPOINT = "https://search.yahoo.com/search"

# The values the page's own time filter links send. No year: `btf=y` is ignored.
_FRESHNESS = {"day": "d", "week": "w", "month": "m"}

_REDIRECT_HOST = "r.search.yahoo.com"
_RU_RE = re.compile(r"/RU=([^/]+)")
# Ad clicks go through r.search.yahoo.com/rdclks/… (and, in older markup,
# /cbclk…) rather than the organic `/_ylt=…` form. Checked on the raw href,
# because the ad's RU= target can be the advertiser's own page.
_AD_CLICK_MARKERS = ("/rdclks/", "/cbclk")

# Yahoo's own pages: search verticals, Scout, consent and privacy. Matched by
# suffix, so images.search.yahoo.com counts. Other yahoo.com hosts are kept —
# a finance.yahoo.com article that Bing ranks organically is a real page.
_INTERNAL_HOSTS = (
    "search.yahoo.com",
    "scout.yahoo.com",
    "shopping.yahoo.com",
    "guce.yahoo.com",
    "consent.yahoo.com",
    "login.yahoo.com",
    "legal.yahoo.com",
    "policies.yahoo.com",
)


def resolve_yahoo_url(href: str) -> str:
    """Unwrap an r.search.yahoo.com click redirect to the target URL.

    Anything that is not such a redirect is returned as it is. A redirect with
    no usable `RU=` yields "" so the caller skips it rather than emit Yahoo's
    tracking link as a result.
    """
    href = (href or "").strip()
    if href.startswith("//"):
        href = "https:" + href
    try:
        host = (urlparse(href).hostname or "").lower()
    except ValueError:
        return ""
    if host != _REDIRECT_HOST:
        return href
    m = _RU_RE.search(href)
    if not m:
        return ""
    target = unquote(m.group(1))
    return target if target.startswith(("http://", "https://")) else ""


def _is_dropped(url: str) -> bool:
    """A Yahoo page or an ad click, never a result."""
    try:
        parsed = urlparse(url)
    except ValueError:
        return True
    host = (parsed.hostname or "").lower()
    if host_matches(host, _INTERNAL_HOSTS):
        return True
    # What Yahoo's ads unwrap to: Bing Ads' own click redirect.
    return host_matches(host, ("bing.com",)) and parsed.path.startswith("/aclick")


# ---------------------------------------------------------------------------
# The cookie jar (see "The `_bv` handshake" above)
# ---------------------------------------------------------------------------

# The YBV cookie a completed handshake ends with is issued for a day
# (Max-Age=86400; the in-progress one only for 60 s), so half of that is safe.
# A jar Yahoo stops accepting is replaced on the spot (see `_http_get`), so the
# TTL only bounds how long an unused jar is trusted.
_JAR_TTL = 12 * 3600.0

# GETs per handshake, all in one session. Measured from a fresh session after
# two failures: the third attempt got results in 4 of 6 runs, whether or not
# it waited 1.5 s first, so it is repetition in the session that counts, not
# time. A re-mint after a rejected jar gets one fewer, because the rejected
# request has already spent a second or so.
_HANDSHAKE_ATTEMPTS = 3
_REMINT_ATTEMPTS = 2


@dataclass
class _Jar:
    cookies: dict[str, str]
    minted_at: float

    def fresh(self, now: float) -> bool:
        return now - self.minted_at < _JAR_TTL


# Keyed by proxy egress: cookies minted from one exit IP and replayed from
# another are exactly the inconsistency a bot check looks for.
_JARS: dict[str, _Jar] = {}


def _reset_jar() -> None:
    """Forget every minted jar. For tests."""
    _JARS.clear()


def _egress_key(proxy_kwargs: dict[str, Any]) -> str:
    return str(proxy_kwargs.get("proxy") or "")


class YahooEngine(Engine):
    """Yahoo web results (keyless HTML scrape of Bing's index)."""

    name = "yahoo"
    description = (
        "Yahoo Search: Bing's index via Yahoo's own front end; a keyless backup when bing is gated."
    )
    # Yahoo's EU consent interstitial, which a European egress gets instead.
    gate_markers = (("consent", ("consent.yahoo.com", "collectconsent")),)

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        # The operators are the only way these filters reach the engine: there
        # are no dedicated URL parameters for them.
        filetype = "pdf" if filters and filters.category == "pdf" else None
        q = augment_query_with_operators(
            query,
            include_domains=filters.include_domains if filters else None,
            exclude_domains=filters.exclude_domains if filters else None,
            filetype=filetype,
        )
        url = f"{_ENDPOINT}?p={quote_plus(q)}&ei=UTF-8"
        if filters and filters.freshness:
            token = _FRESHNESS.get(filters.freshness)
            if token:
                url += f"&btf={token}"
        return url

    async def _http_get(
        self, url: str, *, cookies: dict[str, str] | None = None, retry: bool = True
    ) -> str:
        if cookies is not None:
            return await super()._http_get(url, cookies=cookies, retry=retry)
        proxy_kwargs = curl_proxy_kwargs(self.name)
        key = _egress_key(proxy_kwargs)
        jar = _JARS.get(key)
        if jar is not None and jar.fresh(time.monotonic()):
            try:
                # One request: a rejected jar is answered with an empty 500,
                # which the generic retry would only repeat; the handshake
                # below is the answer to it.
                return await super()._http_get(url, cookies=jar.cookies, retry=False)
            except Timeout:
                raise  # slow, not rejected: a handshake would only be slower
            except RequestException as exc:
                # A rejected jar is answered the way a fresh client is: an
                # empty HTTP 500. Drop it (unless a concurrent search already
                # replaced it) and mint a new one in this same search.
                log.info("yahoo cookie jar rejected (%s); running the handshake again", exc)
                if _JARS.get(key) is jar:
                    _JARS.pop(key, None)
                return await self._handshake(url, proxy_kwargs, attempts=_REMINT_ATTEMPTS)
        return await self._handshake(url, proxy_kwargs, attempts=_HANDSHAKE_ATTEMPTS)

    async def _handshake(self, url: str, proxy_kwargs: dict[str, Any], *, attempts: int) -> str:
        """GET `url` from a session of our own, keeping the cookies it ends with.

        A plain curl_cffi session rather than the shared one, because the
        point is to read back the cookies the `_bv` hops set (sessions.py says
        the same of Bing's warm-up). Each further attempt in the same
        session sends the `YBV` cookie the previous one ended with, which is
        what eventually gets it through (see `_HANDSHAKE_ATTEMPTS`).
        """
        async with CurlAsyncSession(
            impersonate=self.impersonate or IMPERSONATE,
            timeout=settings.request_timeout,
            allow_redirects=True,
            headers={
                "Accept-Language": settings.accept_language,
                "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            },
            **proxy_kwargs,
        ) as client:
            status = None
            for _ in range(max(1, attempts)):
                resp = await client.get(url)
                status = resp.status_code
                if status in (403, 429):
                    # A refusal, not the bot check's empty 500: asking again
                    # at once only adds to what Yahoo is counting.
                    break
                if status == 200:
                    cookies = {c.name: c.value for c in client.cookies.jar if c.value is not None}
                    if cookies:
                        _JARS[_egress_key(proxy_kwargs)] = _Jar(
                            cookies=cookies, minted_at=time.monotonic()
                        )
                    return resp.text
        # Yahoo turns a handshake away with an empty HTTP 500 ("INKApi
        # Error"), which reads as a server fault. Say what it is. Still a
        # RequestException, so `_fetch` hands the URL to the browser under
        # fetch_strategy="auto" exactly as it would for any HTTP failure.
        raise RequestException(
            f"yahoo: Yahoo's bot check did not let this request through (its cookie "
            f"handshake ended in HTTP {status}); the other engines are unaffected."
        )

    def parse(self, html: str) -> list[SearchResult]:
        if not html:
            return []
        tree = parse_html(html)
        results: list[SearchResult] = []
        seen: set[str] = set()

        for row in tree.css("div.algo"):
            anchor = row.css_first("div.compTitle a") or row.css_first("h3 a")
            if anchor is None:
                continue
            href = anchor.attributes.get("href") or ""
            if any(marker in href for marker in _AD_CLICK_MARKERS):
                continue
            url = resolve_yahoo_url(href)
            title = inline_text(row.css_first("h3"))
            if not title or not url.startswith(("http://", "https://")):
                continue
            if _is_dropped(url) or url in seen:
                continue
            seen.add(url)

            snippet = inline_text(row.css_first("div.compText"))
            results.append(
                SearchResult(
                    title=title,
                    url=url,
                    snippet=snippet,
                    engine=self.name,
                    rank=0,
                    # Scraped from arbitrary snippet text, so NOT confident —
                    # a page merely mentioning a date must not be dropped by
                    # the freshness filter.
                    published_age=extract_date_hint(snippet),
                )
            )
        return results
