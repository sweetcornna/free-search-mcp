from urllib.parse import quote_plus

from .base import (
    Engine,
    SearchFilters,
    SearchResult,
    augment_query_with_operators,
    extract_date_hint,
    parse_html,
    text_of,
)


# Startpage uses Google-style `with_date=` semantics on the `sp/search` endpoint.
_STARTPAGE_FRESHNESS = {"day": "d", "week": "w", "month": "m", "year": "y"}


class StartpageEngine(Engine):
    name = "startpage"
    needs_browser = True
    wait_selector = ".result a[aria-label='link']"

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
        url = (
            f"https://www.startpage.com/sp/search?query={quote_plus(q)}"
            "&cat=web&pl=opensearch&language=english"
        )
        if filters and filters.freshness:
            url += f"&with_date={_STARTPAGE_FRESHNESS[filters.freshness]}"
        return url

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
            result = SearchResult(title=title, url=url, snippet=snippet, engine=self.name, rank=0)
            hint = extract_date_hint(snippet) or extract_date_hint(title)
            if hint:
                result.published_age = hint
            results.append(result)
        return results
