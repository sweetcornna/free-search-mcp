from urllib.parse import quote_plus

from .base import Engine, SearchResult, parse_html, text_of


class BaiduEngine(Engine):
    name = "baidu"
    needs_browser = False

    def build_url(self, query: str, max_results: int) -> str:
        rn = min(max(max_results, 10), 50)
        return f"https://www.baidu.com/s?wd={quote_plus(query)}&rn={rn}"

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
