from urllib.parse import quote_plus

from .base import (
    Engine,
    SearchFilters,
    SearchResult,
    augment_query_with_operators,
    parse_html,
    text_of,
)


# Baidu's `gpc=stf=<from>,<to>|stftype=1` for explicit ranges, but the simpler
# documented `gpc` time-window keys map cleanly to LLM freshness buckets.
# We rely on inline operators + client-side post-filter for everything else.


class BaiduEngine(Engine):
    name = "baidu"
    needs_browser = False

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        rn = min(max(max_results, 10), 50)
        filetype = None
        if filters and filters.category == "pdf":
            filetype = "pdf"
        q = augment_query_with_operators(
            query,
            include_domains=filters.include_domains if filters else None,
            exclude_domains=filters.exclude_domains if filters else None,
            filetype=filetype,
        )
        # Baidu has no reliable freshness URL parameter for the public HTML
        # endpoint (the gpc=stf=… token requires unix timestamps and is
        # session-bound). Skip and let the client-side filter handle it.
        return f"https://www.baidu.com/s?wd={quote_plus(q)}&rn={rn}"

    def parse(self, html: str) -> list[SearchResult]:
        tree = parse_html(html)
        results: list[SearchResult] = []
        for div in tree.css("div.result.c-container, div.result-op.c-container"):
            link = div.css_first("h3.t a") or div.css_first("h3 a")
            if not link:
                continue
            url = link.attributes.get("href", "")
            title = text_of(link)
            snippet = text_of(
                div.css_first(".c-abstract")
                or div.css_first('[class*="content-right"]')
                or div.css_first('[class*="abstract"]')
            )
            if not url or not title:
                continue
            results.append(SearchResult(title=title, url=url, snippet=snippet, engine=self.name, rank=0))
        return results
