from typing import Any
from urllib.parse import quote_plus

from ..config import settings
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


# Bing's documented freshness filter values.
_BING_FRESHNESS = {"day": "ex1:\"ez1\"", "week": "ex1:\"ez2\"", "month": "ex1:\"ez3\"", "year": "ex1:\"ez4\""}


class BingEngine(Engine):
    name = "bing"
    # The www4 edge serves 10 real organic results over plain HTTP in ~0.3s
    # (verified), so we try HTTP FIRST and only pay for a Playwright render when
    # parse() comes back empty (a real gate) via the inherited
    # supports_browser_fallback. This is ~50x faster than the old always-browser
    # path on the common case. wait_selector still applies to the fallback render.
    needs_browser = False
    # Match the actual result item; #b_results is the empty container that
    # exists immediately and would short-circuit the wait.
    wait_selector = "li.b_algo"

    async def search(
        self,
        query: str,
        max_results: int,
        filters: SearchFilters | None = None,
        diagnostics: dict[str, Any] | None = None,
    ) -> list[SearchResult]:
        """Scrape Bing (HTTP-first, browser fallback), then fall back to SearXNG
        when the provider gated us.

        ``base.search()`` proxies the fetch (HTTP, then a Playwright render if
        the HTTP body parsed empty) and records any gate into
        ``diagnostics["gated"][self.name]``. An empty result set almost always
        means a CAPTCHA/consent gate, so we recover keylessly via the SearXNG
        meta-search (it proxies Google/Bing). Fallback results keep
        ``engine="searx"`` for honest attribution. Never raises.
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
        # www.bing.com aggressively challenges headless clients ("something went
        # wrong" page). The www4 edge serves the same index without that gate.
        # form=QBLH switches to a lighter JS-rendered layout missing .b_algo,
        # so we omit it.
        count = min(max(max_results, 10), 50)
        filetype = None
        if filters and filters.category == "pdf":
            filetype = "pdf"
        q = augment_query_with_operators(
            query,
            include_domains=filters.include_domains if filters else None,
            exclude_domains=filters.exclude_domains if filters else None,
            filetype=filetype,
        )
        url = f"https://www4.bing.com/search?q={quote_plus(q)}&count={count}"
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

    def parse(self, html: str) -> list[SearchResult]:
        tree = parse_html(html)
        results: list[SearchResult] = []
        for li in tree.css("li.b_algo"):
            link = li.css_first("h2 a")
            if not link:
                continue
            url = link.attributes.get("href", "")
            title = text_of(link)
            snippet_node = (
                li.css_first(".b_caption p")
                or li.css_first(".b_lineclamp4")
                or li.css_first(".b_lineclamp2")
                or li.css_first(".b_paractl")
            )
            snippet = text_of(snippet_node)
            if not url or not title:
                continue
            result = SearchResult(title=title, url=url, snippet=snippet, engine=self.name, rank=0)
            hint = extract_date_hint(snippet) or extract_date_hint(title)
            if hint:
                result.published_age = hint
            results.append(result)
        return results
