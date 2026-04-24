import asyncio

from urllib.parse import quote_plus

from ..browser import pool
from .base import (
    Engine,
    SearchFilters,
    SearchResult,
    augment_query_with_operators,
    parse_html,
    text_of,
)


# Bing's documented freshness filter values.
_BING_FRESHNESS = {"day": "ex1:\"ez1\"", "week": "ex1:\"ez2\"", "month": "ex1:\"ez3\"", "year": "ex1:\"ez4\""}


class BingEngine(Engine):
    name = "bing"
    # Bing serves a JS interstitial/captcha to non-browser clients on many UAs,
    # so we always render via Playwright. Cost ≈ +1s per first cold call.
    needs_browser = True
    # Match the actual result item; #b_results is the empty container that
    # exists immediately and would short-circuit the wait.
    wait_selector = "li.b_algo"

    _warmup_lock = asyncio.Lock()
    _warmed = False

    async def search(self, query: str, max_results: int, filters: SearchFilters | None = None):
        if not BingEngine._warmed:
            async with BingEngine._warmup_lock:
                if not BingEngine._warmed:
                    await pool.warmup("https://www4.bing.com/")
                    BingEngine._warmed = True
        return await super().search(query, max_results, filters)

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
            results.append(SearchResult(title=title, url=url, snippet=snippet, engine=self.name, rank=0))
        return results
