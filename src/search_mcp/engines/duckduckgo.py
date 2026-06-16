from urllib.parse import parse_qs, quote_plus, urlparse

from ..config import settings
from .base import (
    Engine,
    SearchFilters,
    SearchResult,
    augment_query_with_operators,
    extract_date_hint,
    parse_html,
    safesearch_param,
    text_of,
)


_DDG_FRESHNESS = {"day": "d", "week": "w", "month": "m", "year": "y"}


def _unwrap(url: str) -> str:
    if not url:
        return url
    if url.startswith("//"):
        url = "https:" + url
    parsed = urlparse(url)
    if "duckduckgo.com" in parsed.netloc and parsed.path.startswith("/l/"):
        qs = parse_qs(parsed.query)
        if "uddg" in qs:
            # parse_qs already percent-decodes the value once. Decoding again
            # would corrupt targets that legitimately contain literal %xx — e.g.
            # .../wiki/C%2B%2B (a real "C++" wiki URL) would become C++ and 404.
            return qs["uddg"][0]
    return url


class DuckDuckGoEngine(Engine):
    name = "duckduckgo"
    needs_browser = False

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        # Inline operators that DDG honors well: site:, -site:, filetype:
        filetype = None
        if filters and filters.category == "pdf":
            filetype = "pdf"
        q = augment_query_with_operators(
            query,
            include_domains=filters.include_domains if filters else None,
            exclude_domains=filters.exclude_domains if filters else None,
            filetype=filetype,
        )
        url = f"https://html.duckduckgo.com/html/?q={quote_plus(q)}"
        if filters and filters.freshness:
            url += f"&df={_DDG_FRESHNESS[filters.freshness]}"
        # SafeSearch: kp=1 strict, kp=-1 moderate, kp=-2 off.
        kp = safesearch_param(self.name)
        if kp is not None:
            url += f"&kp={kp}"
        # Region: DDG's kl param already uses our 'cc-lang' token form (us-en).
        if settings.region:
            url += f"&kl={quote_plus(settings.region)}"
        return url

    def parse(self, html: str) -> list[SearchResult]:
        tree = parse_html(html)
        results: list[SearchResult] = []
        seen: set[str] = set()
        # Every organic row carries class "result" (alongside "web-result" etc.);
        # selecting on "div.result" alone matches each row EXACTLY ONCE. The old
        # "div.result, div.web-result" comma selector matched the same div twice
        # (it has both classes), emitting every result twice and doubling DDG's
        # weight in the RRF merge. The seen-set is a belt-and-braces guard against
        # any future markup that re-introduces a double match.
        for div in tree.css("div.result"):
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
            if not url or not title or url in seen:
                continue
            seen.add(url)
            result = SearchResult(title=title, url=url, snippet=snippet, engine=self.name, rank=0)
            hint = extract_date_hint(snippet) or extract_date_hint(title)
            if hint:
                result.published_age = hint
            results.append(result)
        return results
