from urllib.parse import quote_plus

from .base import Engine, SearchResult, parse_html, text_of


class BraveEngine(Engine):
    name = "brave"
    needs_browser = True
    wait_selector = "#results, .snippet"

    def build_url(self, query: str, max_results: int) -> str:
        return f"https://search.brave.com/search?q={quote_plus(query)}&source=web"

    def parse(self, html: str) -> list[SearchResult]:
        tree = parse_html(html)
        results: list[SearchResult] = []
        for snip in tree.css('div.snippet[data-type="web"], #results .snippet'):
            link = snip.css_first("a")
            if not link:
                continue
            url = link.attributes.get("href", "")
            title_node = (
                snip.css_first(".title")
                or snip.css_first(".snippet-title")
                or link.css_first(".title")
            )
            title = text_of(title_node) or text_of(link)
            snippet = text_of(
                snip.css_first(".snippet-description")
                or snip.css_first(".snippet-content")
                or snip.css_first(".snippet-content-summary")
            )
            if not url or not title or url.startswith("#"):
                continue
            results.append(SearchResult(title=title, url=url, snippet=snippet, engine=self.name, rank=0))
        return results
