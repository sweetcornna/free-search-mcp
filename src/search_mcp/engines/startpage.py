from urllib.parse import quote_plus

from .base import Engine, SearchResult, parse_html, text_of


class StartpageEngine(Engine):
    name = "startpage"
    needs_browser = True
    wait_selector = ".result a[aria-label='link']"

    def build_url(self, query: str, max_results: int) -> str:
        return (
            f"https://www.startpage.com/sp/search?query={quote_plus(query)}"
            "&cat=web&pl=opensearch&language=english"
        )

    def parse(self, html: str) -> list[SearchResult]:
        # Startpage uses dynamic emotion-css class names that change every load,
        # so anchor selection by aria-label="link" is the only stable hook.
        tree = parse_html(html)
        results: list[SearchResult] = []
        seen: set[str] = set()
        for div in tree.css(".result"):
            link = div.css_first('a[aria-label="link"]')
            if not link:
                continue
            url = link.attributes.get("href") or ""
            if not url or url in seen:
                continue
            seen.add(url)
            title = text_of(link)
            descs = [
                text_of(d)
                for d in div.css(".description")
                if text_of(d) and "{" not in text_of(d)
            ]
            snippet = max(descs, key=len, default="")
            if not title:
                continue
            results.append(SearchResult(title=title, url=url, snippet=snippet, engine=self.name, rank=0))
        return results
