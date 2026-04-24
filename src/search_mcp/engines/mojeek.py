from urllib.parse import quote_plus

from .base import Engine, SearchResult, parse_html, text_of


class MojeekEngine(Engine):
    """Independent search index, no JS gating, no API key, stable HTML."""

    name = "mojeek"
    needs_browser = False

    def build_url(self, query: str, max_results: int) -> str:
        return f"https://www.mojeek.com/search?q={quote_plus(query)}"

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
