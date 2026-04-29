"""Google News RSS engine.

Independent news index that requires no API key. Hits the public RSS endpoint
at https://news.google.com/rss/search?q=... which returns up-to-the-minute
news articles with structured <pubDate> tags.

Caveat: result URLs are news.google.com/articles/CBM... redirects, NOT direct
publisher URLs. Decoding the wrapped URL is non-trivial (Google encodes it
inside a base64-ish blob with anti-bot signing). For now we return the
news.google.com link as-is; the fetcher follows the redirect and the final
publisher URL surfaces as FetchResult.url. The original outlet name is
appended to each title in parentheses (parsed from the <source> RSS element)
so the LLM can identify the publisher without parsing the URL.
"""

from __future__ import annotations

import html as html_lib
import re
import xml.etree.ElementTree as ET
from urllib.parse import quote_plus

from .base import (
    Engine,
    SearchFilters,
    SearchResult,
    augment_query_with_operators,
)


# Google News supports a `when:` query operator: when:1d, when:7d, when:1m, when:1y.
_GN_FRESHNESS = {"day": "1d", "week": "7d", "month": "1m", "year": "1y"}

_TAG_RE = re.compile(r"<[^>]+>")


def _strip_html(s: str) -> str:
    """Remove tags and decode entities from an RSS description blob."""
    if not s:
        return ""
    no_tags = _TAG_RE.sub(" ", s)
    decoded = html_lib.unescape(no_tags)
    return " ".join(decoded.split())


class GoogleNewsEngine(Engine):
    """Google News RSS — independent news index, no API key, structured dates."""

    name = "googlenews"
    needs_browser = False  # plain RSS over HTTP, no JS

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        q = augment_query_with_operators(
            query,
            include_domains=filters.include_domains if filters else None,
            exclude_domains=filters.exclude_domains if filters else None,
        )
        if filters and filters.freshness:
            q = f"{q} when:{_GN_FRESHNESS[filters.freshness]}"
        # hl=en-US&gl=US&ceid=US:en gives us the English-language US edition.
        return (
            f"https://news.google.com/rss/search?q={quote_plus(q)}"
            "&hl=en-US&gl=US&ceid=US:en"
        )

    def parse(self, html: str) -> list[SearchResult]:
        results: list[SearchResult] = []
        if not html:
            return results
        try:
            root = ET.fromstring(html)
        except ET.ParseError:
            return results

        # RSS 2.0: items live at /rss/channel/item — but use .//item for
        # robustness against minor structural drift.
        for item in root.iter("item"):
            title_el = item.find("title")
            link_el = item.find("link")
            desc_el = item.find("description")
            source_el = item.find("source")

            title = (title_el.text or "").strip() if title_el is not None else ""
            url = (link_el.text or "").strip() if link_el is not None else ""
            if not title or not url:
                continue

            source = ""
            if source_el is not None and source_el.text:
                source = source_el.text.strip()

            display_title = f"{title} ({source})" if source else title
            snippet = _strip_html(desc_el.text) if desc_el is not None else ""

            results.append(
                SearchResult(
                    title=display_title,
                    url=url,
                    snippet=snippet,
                    engine=self.name,
                    rank=0,
                )
            )
        return results
