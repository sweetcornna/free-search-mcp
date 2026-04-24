from urllib.parse import parse_qs, quote_plus, unquote, urlparse

from .base import Engine, SearchResult, parse_html, text_of


def _unwrap(url: str) -> str:
    if not url:
        return url
    if url.startswith("//"):
        url = "https:" + url
    parsed = urlparse(url)
    if "duckduckgo.com" in parsed.netloc and parsed.path.startswith("/l/"):
        qs = parse_qs(parsed.query)
        if "uddg" in qs:
            return unquote(qs["uddg"][0])
    return url


class DuckDuckGoEngine(Engine):
    name = "duckduckgo"
    needs_browser = False

    def build_url(self, query: str, max_results: int) -> str:
        return f"https://html.duckduckgo.com/html/?q={quote_plus(query)}"

    def parse(self, html: str) -> list[SearchResult]:
        tree = parse_html(html)
        results: list[SearchResult] = []
        for div in tree.css("div.result, div.web-result"):
            classes = div.attributes.get("class") or ""
            # Skip the obvious ad rows DDG injects; their URL is also a y.js redirect.
            if "result--ad" in classes or "result--sponsored" in classes:
                continue
            link = div.css_first("a.result__a")
            if not link:
                continue
            href = link.attributes.get("href", "")
            url = _unwrap(href)
            if "duckduckgo.com/y.js" in url or "ad_provider=" in url:
                continue
            title = text_of(link)
            snippet = text_of(div.css_first(".result__snippet"))
            if not url or not title:
                continue
            results.append(SearchResult(title=title, url=url, snippet=snippet, engine=self.name, rank=0))
        return results
