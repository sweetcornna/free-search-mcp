"""Keyless Google web SERP scraper.

Hits the public ``https://www.google.com/search`` HTML endpoint anonymously
(no API key, no credentials) and scrapes the organic result blocks. Same
family as ``baidu.py`` / ``duckduckgo.py`` — plain HTTP first, parse the
returned markup.

Caveats:
  * Google frequently serves a JS/consent shell (or a "before you continue"
    interstitial) to plain HTTP clients. When that happens ``parse()`` finds
    no organic blocks and returns ``[]``, which trips the base-class
    Playwright browser fallback (``supports_browser_fallback`` left True) to
    re-render the page and recover. We therefore deliberately do NOT set
    ``needs_browser`` — the HTTP path works often enough to be worth trying.
  * Organic result URLs are sometimes wrapped as ``/url?q=<target>&...``
    redirects; we unwrap the ``q`` parameter back to the real destination.
  * The SERP markup is unstable: class names rotate. We use several
    fallback selectors and never raise on unexpected input.
"""

from __future__ import annotations

from typing import Any
from urllib.parse import parse_qs, quote_plus, urlparse

from ..config import settings
from .base import (
    Engine,
    SearchFilters,
    SearchResult,
    augment_query_with_operators,
    extract_date_hint,
    parse_html,
    text_of,
)


# Google's `tbs=qdr:<x>` time-window keys map cleanly to our freshness buckets.
_GOOGLE_FRESHNESS = {"day": "d", "week": "w", "month": "m", "year": "y"}

# Hrefs that are internal Google navigation, not organic destinations.
_INTERNAL_HREF_PREFIXES = (
    "/search",
    "/setprefs",
    "/preferences",
    "/advanced_search",
    "https://www.google.com/search",
    "https://google.com/search",
    "https://maps.google.",
    "https://accounts.google.",
    "https://support.google.",
    "https://policies.google.",
    "#",
    "javascript:",
)


def _unwrap(href: str) -> str:
    """Turn a Google ``/url?q=<target>&...`` redirect into its real target.

    Google sometimes wraps organic links in a redirect of the form
    ``/url?q=https%3A%2F%2Fexample.com%2F&sa=...``. ``parse_qs`` already
    percent-decodes the ``q`` value once, so we return it verbatim. Anything
    that isn't a recognised ``/url?`` wrapper is returned unchanged.
    """
    if not href:
        return href
    if href.startswith("/url?") or href.startswith("/url?q="):
        qs = parse_qs(urlparse(href).query)
        if "q" in qs and qs["q"]:
            return qs["q"][0]
    return href


def _is_internal(href: str) -> bool:
    if not href:
        return True
    return href.startswith(_INTERNAL_HREF_PREFIXES)


class GoogleEngine(Engine):
    """Keyless Google web SERP scraper — anonymous HTTP, browser fallback."""

    name = "google"
    needs_browser = False
    # Leave supports_browser_fallback = True (the base default): Google often
    # serves a JS/consent shell to plain HTTP, parse()==[] then recovers via a
    # Playwright render.

    async def search(
        self,
        query: str,
        max_results: int,
        filters: SearchFilters | None = None,
        diagnostics: dict[str, Any] | None = None,
    ) -> list[SearchResult]:
        """Scrape Google, falling back to SearXNG when the provider gated us.

        ``base.search()`` already proxies the fetch and records the gate reason
        into ``diagnostics["gated"][self.name]``. An empty result set almost
        always means Google served a CAPTCHA/consent shell, so we recover
        keylessly via the working SearXNG meta-search (which itself proxies
        Google/Bing). Fallback results keep ``engine="searx"`` for honest
        attribution. Never raises.
        """
        results = await super().search(
            query, max_results, filters, diagnostics=diagnostics
        )
        if results:
            return results
        # Empty almost always means a CAPTCHA/consent gate. Recover keyless via
        # the working SearXNG meta-search (it proxies Google/Bing results).
        from .searx import SearxEngine

        fb = await SearxEngine().search(query, max_results, filters)
        if fb and diagnostics is not None:
            diagnostics.setdefault("gated", {}).setdefault(self.name, "gated")
            diagnostics.setdefault("fallback", {})[self.name] = "searx"
        return fb

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        # Google clamps `num` hard; ask for a little headroom over the budget
        # in the 10..20 band so dedup/filtering still leaves enough hits.
        num = min(max(max_results, 10), 20)
        filetype = None
        if filters and filters.category == "pdf":
            filetype = "pdf"
        q = augment_query_with_operators(
            query,
            include_domains=filters.include_domains if filters else None,
            exclude_domains=filters.exclude_domains if filters else None,
            filetype=filetype,
        )
        url = f"https://www.google.com/search?q={quote_plus(q)}&num={num}&hl=en&gl=us"
        if filters and filters.freshness:
            url += f"&tbs=qdr:{_GOOGLE_FRESHNESS[filters.freshness]}"
        # SafeSearch: Google is NOT in base's safesearch table, so read the
        # setting directly. strict/moderate => safe=active; off => omit.
        if settings.safesearch in ("strict", "moderate"):
            url += "&safe=active"
        return url

    def parse(self, html: str) -> list[SearchResult]:
        tree = parse_html(html)
        results: list[SearchResult] = []
        seen: set[str] = set()
        for block in tree.css("div.g, div[data-hveid]"):
            classes = block.attributes.get("class") or ""
            # Skip ad rows. Google marks sponsored blocks several ways.
            if "uEierd" in classes:
                continue
            if block.css_first("div[data-text-ad]") is not None:
                continue

            # Organic result = an <a href> wrapping (or containing) an <h3>.
            link = block.css_first("a:has(h3)") or block.css_first("a[href]")
            if link is None:
                continue
            h3 = link.css_first("h3") or block.css_first("h3")
            if h3 is None:
                continue

            href = link.attributes.get("href", "") or ""
            if _is_internal(href):
                continue
            url = _unwrap(href)
            if not url or _is_internal(url):
                continue

            title = text_of(h3)
            if not title:
                continue

            # Dedup by url — Google repeats the same result across stacked
            # data-hveid containers (sitelinks, "people also ask", etc.).
            if url in seen:
                continue
            seen.add(url)

            snippet = text_of(
                block.css_first("div.VwiC3b")
                or block.css_first("div[data-sncf]")
                or block.css_first("[data-content-feature] span")
                or _longest_text_div(block)
            )

            result = SearchResult(
                title=title, url=url, snippet=snippet, engine=self.name, rank=0
            )
            hint = extract_date_hint(snippet) or extract_date_hint(title)
            if hint:
                result.published_age = hint
            results.append(result)
        return results


def _longest_text_div(block):
    """Fallback snippet source: the descendant ``div`` holding the most text.

    Used when none of the known description selectors match (Google rotates
    class names). Returns ``None`` when the block has no usable text div, in
    which case ``text_of`` yields ``""``.
    """
    best = None
    best_len = 0
    for div in block.css("div"):
        # Skip divs that contain the title anchor — we want the description.
        if div.css_first("h3") is not None:
            continue
        t = text_of(div)
        if len(t) > best_len:
            best_len = len(t)
            best = div
    return best
