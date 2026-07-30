"""360 搜索 (so.com) — Chinese web search, HTML scrape, no API key.

Complements `baidu`: 360 is the second-largest Chinese index and routinely
surfaces different results for the same query.

Results live in `li.res-list`, and unlike Sogou the anchors carry direct
target URLs rather than redirect wrappers, so no unwrapping is needed.

Best-effort by nature: this parses a page meant for humans, and the selectors
will need revisiting whenever 360 reshuffles its markup. An unparseable page
yields `[]` rather than an error, per the keyless-engine contract.
"""

from __future__ import annotations

from urllib.parse import quote_plus

from ..config import settings
from .base import Engine, SearchFilters, SearchResult, extract_date_hint, parse_html, text_of

_ENDPOINT = "https://www.so.com/s"

# 360's freshness parameter, "advanced time" ranges.
_FRESHNESS = {"day": "d", "week": "w", "month": "m", "year": "y"}


class So360Engine(Engine):
    """360 搜索 web results (keyless HTML scrape)."""

    name = "so360"

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        # 360 pages in tens; asking for more than 50 just returns the same page.
        n = max(10, min(max_results, 50))
        params = [f"q={quote_plus(query)}", f"rn={n}"]
        if filters and filters.freshness:
            token = _FRESHNESS.get(filters.freshness)
            if token:
                params.append(f"adv_t={token}")
        if settings.safesearch != "off":
            params.append("secure=1")
        return f"{_ENDPOINT}?{'&'.join(params)}"

    def parse(self, html: str) -> list[SearchResult]:
        if not html:
            return []
        tree = parse_html(html)
        results: list[SearchResult] = []
        seen: set[str] = set()

        for item in tree.css("li.res-list"):
            anchor = item.css_first("h3 a")
            if anchor is None:
                continue
            # data-mdurl carries the canonical target when 360 wraps the href
            # for click tracking; plain href is the common case.
            url = (
                anchor.attributes.get("data-mdurl")
                or anchor.attributes.get("href")
                or ""
            ).strip()
            title = text_of(anchor)
            if not title or not url.startswith("http"):
                continue
            if url in seen:
                continue
            seen.add(url)

            body = item.css_first("p.res-desc") or item.css_first("div.res-comm-con")
            snippet = text_of(body) if body is not None else ""
            results.append(
                SearchResult(
                    title=title,
                    url=url,
                    snippet=snippet,
                    engine=self.name,
                    rank=0,
                    # Scraped from arbitrary snippet text, so NOT confident —
                    # a page merely mentioning a date must not be dropped by
                    # the freshness filter.
                    published_age=extract_date_hint(snippet),
                )
            )
        return results
