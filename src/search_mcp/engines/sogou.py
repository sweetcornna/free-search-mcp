"""搜狗搜索 (sogou.com) — Chinese web search, HTML scrape, no API key.

Notable and deliberately not hidden: **Sogou returns redirect URLs**, not
target URLs. Every result href is `/link?url=<opaque encrypted blob>`, and the
blob cannot be decoded client-side — only followed. So the engine emits the
absolute Sogou redirect link, which:

  * works fine for reading (`fetch` follows redirects), and
  * means every result shares the host `www.sogou.com`, so host-based
    `category` filtering will discard them all.

Resolving each link would cost one extra HTTP round trip per result, which is
not a price the aggregator's parallel fan-out budget should pay for an opt-in
engine. If you need real target URLs, `baidu` and `so360` provide them
directly.

Best-effort by nature — see so360.py.
"""

from __future__ import annotations

from urllib.parse import quote_plus, urljoin

from ..config import settings
from .base import Engine, SearchFilters, SearchResult, extract_date_hint, parse_html, text_of

_ORIGIN = "https://www.sogou.com"
_ENDPOINT = f"{_ORIGIN}/web"

# Sogou's "sort by time" filter lives in the `tsn` parameter (days back).
_FRESHNESS = {"day": "1", "week": "2", "month": "3", "year": "4"}


class SogouEngine(Engine):
    """搜狗 web results (keyless HTML scrape, redirect URLs)."""

    name = "sogou"

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        params = [f"query={quote_plus(query)}"]
        if filters and filters.freshness:
            token = _FRESHNESS.get(filters.freshness)
            if token:
                params.append(f"tsn={token}")
        if settings.safesearch != "off":
            params.append("cl=3")
        return f"{_ENDPOINT}?{'&'.join(params)}"

    def parse(self, html: str) -> list[SearchResult]:
        if not html:
            return []
        tree = parse_html(html)
        results: list[SearchResult] = []
        seen: set[str] = set()

        for heading in tree.css("h3.vr-title"):
            anchor = heading.css_first("a")
            if anchor is None:
                continue
            href = (anchor.attributes.get("href") or "").strip()
            title = text_of(anchor)
            # Sogou pads the page with in-page widgets whose links are
            # `javascript:void(0)` or relative re-query links; only the real
            # results use the /link? redirect or an absolute URL.
            if not title or not href or href.startswith("javascript:"):
                continue
            if not (href.startswith("/link?") or href.startswith("http")):
                continue
            url = urljoin(_ORIGIN, href)
            if url in seen:
                continue
            seen.add(url)

            results.append(
                SearchResult(
                    title=title,
                    url=url,
                    snippet=self._snippet(heading),
                    engine=self.name,
                    rank=0,
                    published_age=extract_date_hint(self._snippet(heading)),
                )
            )
        return results

    @staticmethod
    def _snippet(heading) -> str:
        """Find the summary that belongs to this heading.

        Sogou nests results inconsistently — the snippet may be a sibling of
        the <h3> or live one or two levels up — so walk a bounded number of
        ancestors rather than assuming one shape.
        """
        node = heading.parent
        for _ in range(3):
            if node is None:
                return ""
            body = node.css_first("div.text-layout, div.fz-mid, p.str-info, div.space-txt")
            if body is not None:
                return text_of(body)
            node = node.parent
        return ""
