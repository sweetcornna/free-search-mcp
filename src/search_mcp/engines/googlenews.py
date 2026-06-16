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
from datetime import datetime, timezone
from email.utils import parsedate_to_datetime
from urllib.parse import quote_plus

from .base import (
    Engine,
    SearchFilters,
    SearchResult,
    augment_query_with_operators,
)


def _format_pubdate(raw: str | None) -> str:
    """Convert RSS RFC-2822 pubDate ('Tue, 28 Apr 2026 15:30:00 GMT') into
    either a relative phrase ('2 days ago') for recent items or an ISO date
    ('2026-04-28') for older ones. Returns "" on parse failure."""
    if not raw:
        return ""
    try:
        dt = parsedate_to_datetime(raw)
    except (TypeError, ValueError):
        return ""
    if dt is None:
        return ""
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    now = datetime.now(timezone.utc)
    delta = now - dt
    secs = delta.total_seconds()
    if secs < 0:
        return dt.strftime("%Y-%m-%d")
    if secs < 3600:
        m = max(1, int(secs // 60))
        return f"{m} minute{'s' if m != 1 else ''} ago"
    if secs < 86400:
        h = int(secs // 3600)
        return f"{h} hour{'s' if h != 1 else ''} ago"
    if secs < 86400 * 14:
        d = int(secs // 86400)
        return f"{d} day{'s' if d != 1 else ''} ago"
    return dt.strftime("%Y-%m-%d")


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
    # RSS feed: an empty/malformed parse is genuinely empty, so don't waste a
    # Playwright render trying to "recover" it (see Engine.search fallback).
    supports_browser_fallback = False

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
            pubdate_el = item.find("pubDate")

            title = (title_el.text or "").strip() if title_el is not None else ""
            url = (link_el.text or "").strip() if link_el is not None else ""
            if not title or not url:
                continue

            source = ""
            if source_el is not None and source_el.text:
                source = source_el.text.strip()

            display_title = f"{title} ({source})" if source else title
            snippet = _strip_html(desc_el.text) if desc_el is not None else ""
            published_age = _format_pubdate(pubdate_el.text if pubdate_el is not None else None)

            results.append(
                SearchResult(
                    title=display_title,
                    url=url,
                    snippet=snippet,
                    engine=self.name,
                    rank=0,
                    published_age=published_age,
                    # RSS <pubDate> is an exact, structured publish time.
                    published_age_confident=bool(published_age),
                )
            )
        return results
