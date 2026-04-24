import asyncio

from urllib.parse import quote_plus

from ..browser import pool
from .base import Engine, SearchResult, parse_html, text_of


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

    async def search(self, query: str, max_results: int):
        if not BingEngine._warmed:
            async with BingEngine._warmup_lock:
                if not BingEngine._warmed:
                    await pool.warmup("https://www4.bing.com/")
                    BingEngine._warmed = True
        return await super().search(query, max_results)

    def build_url(self, query: str, max_results: int) -> str:
        # www.bing.com aggressively challenges headless clients ("something went
        # wrong" page). The www4 edge serves the same index without that gate.
        # form=QBLH switches to a lighter JS-rendered layout missing .b_algo,
        # so we omit it.
        count = min(max(max_results, 10), 50)
        return f"https://www4.bing.com/search?q={quote_plus(query)}&count={count}"

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
