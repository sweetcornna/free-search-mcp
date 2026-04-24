from urllib.parse import quote_plus

from .base import (
    Engine,
    SearchFilters,
    SearchResult,
    augment_query_with_operators,
    parse_html,
    text_of,
)


# Mojeek's `since=` (relative) uses suffix-encoded durations.
_MOJEEK_FRESHNESS = {"day": "1d", "week": "7d", "month": "31d", "year": "365d"}


class MojeekEngine(Engine):
    """Independent search index, no JS gating, no API key, stable HTML."""

    name = "mojeek"
    needs_browser = False

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        filetype = None
        if filters and filters.category == "pdf":
            filetype = "pdf"
        q = augment_query_with_operators(
            query,
            include_domains=filters.include_domains if filters else None,
            exclude_domains=filters.exclude_domains if filters else None,
            filetype=filetype,
        )
        url = f"https://www.mojeek.com/search?q={quote_plus(q)}"
        if filters and filters.freshness:
            url += f"&since={_MOJEEK_FRESHNESS[filters.freshness]}"
        return url

    def parse(self, html: str) -> list[SearchResult]:
        tree = parse_html(html)
        results: list[SearchResult] = []
        for li in tree.css("ul.results-standard li"):
            link = li.css_first("h2 a.title")
            if not link:
                continue
            url = link.attributes.get("href", "")
            title = text_of(link)
            snippet = text_of(li.css_first("p.s"))
            if not url or not title:
                continue
            results.append(SearchResult(title=title, url=url, snippet=snippet, engine=self.name, rank=0))
        return results
