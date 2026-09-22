"""Google News RSS engine.

Independent news index that requires no API key. Hits the public RSS endpoint
at https://news.google.com/rss/search?q=... which returns up-to-the-minute
news articles with structured <pubDate> tags.

Every item links to a `news.google.com/rss/articles/CBM…` redirect blob, not to
the publisher. Handing those out as result URLs quietly disabled everything
that reads a hostname: `include_domains` / `exclude_domains` and the category
filters saw "news.google.com" for every item, and the same story found by a web
engine never merged with it, so rank fusion could not reward the agreement. So
the engine resolves the top results to publisher URLs itself (`gnews.py` replays
the RPC Google's own client uses), inside a fixed time budget; an item that
does not resolve in time keeps its redirect link, which `fetch` still follows.
The outlet name is appended to each title in parentheses either way.

Not part of the default pool: it answers ANY query — a programming question
included — with ten headlines. It joins for `category="news"` and for
`freshness="day"|"week"` (see `settings.fresh_engines`).
"""

from __future__ import annotations

import asyncio
import html as html_lib
import re
import xml.etree.ElementTree as ET
from datetime import UTC, datetime
from email.utils import parsedate_to_datetime
from urllib.parse import quote_plus

from ..config import settings
from ..gnews import is_google_news_url, resolve_google_news_url
from .base import (
    Engine,
    SearchFilters,
    SearchResult,
    augment_query_with_operators,
    detect_query_region,
    region_to_google_news_ceid,
    region_to_google_params,
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
        dt = dt.replace(tzinfo=UTC)
    now = datetime.now(UTC)
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


# Resolving one link downloads a ~600 KB article shell, so this is bounded three
# ways: how many at once, how long in total, and how many failures in a row
# before concluding the RPC is down and leaving the rest alone.
_RESOLVE_CONCURRENCY = 4
_RESOLVE_BUDGET_SECONDS = 4.0
_RESOLVE_MAX_FAILURES = 3


async def _resolve_publisher_urls(results: list[SearchResult]) -> None:
    """Rewrite redirect blobs to publisher URLs, in place, best effort."""
    gate = asyncio.Semaphore(_RESOLVE_CONCURRENCY)
    failures = 0

    async def one(result: SearchResult) -> None:
        nonlocal failures
        async with gate:
            if failures >= _RESOLVE_MAX_FAILURES:
                return
            resolved = await resolve_google_news_url(result.url)
        if resolved:
            failures = 0
            result.url = resolved
        else:
            failures += 1

    tasks = [asyncio.create_task(one(r)) for r in results if is_google_news_url(r.url)]
    if not tasks:
        return
    _, late = await asyncio.wait(tasks, timeout=_RESOLVE_BUDGET_SECONDS)
    for task in late:
        task.cancel()
    await asyncio.gather(*tasks, return_exceptions=True)


class GoogleNewsEngine(Engine):
    """Google News RSS — independent news index, no API key, structured dates."""

    name = "googlenews"
    description = "Google News RSS: headlines with exact publish dates, edition-scoped by region."
    needs_browser = False  # plain RSS over HTTP, no JS
    # RSS feed: an empty/malformed parse is genuinely empty, so don't waste a
    # Playwright render trying to "recover" it (see Engine.search fallback).
    supports_browser_fallback = False
    categories = frozenset({"news"})

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
        # The edition comes from settings.region (default 'us-en' -> the
        # English-language US edition), like every other region-aware engine.
        # Hardcoding US:en here made the engine silently return an EMPTY feed
        # for any query the US edition does not index — a Chinese query got 0
        # items where the zh-Hans edition had 35 — and the empty feed then read
        # as "possible IP block" in the aggregator's diagnostics.
        # ...and the edition follows the query's script when the configured one
        # could not serve it at all (see detect_query_region).
        region = detect_query_region(query, settings.region)
        hl, gl = region_to_google_params(region)
        ceid = region_to_google_news_ceid(region)
        return (
            f"https://news.google.com/rss/search?q={quote_plus(q)}"
            # ceid is built from validated alpha components ('US:en',
            # 'CN:zh-Hans'), so its ':' needs no percent-encoding.
            f"&hl={hl}&gl={gl}&ceid={ceid}"
        )

    async def _raw_results(self, query, max_results, filters=None, diagnostics=None):
        results, html = await super()._raw_results(query, max_results, filters, diagnostics)
        # Only as many as can be returned. `site:` already scoped the feed
        # server-side when the caller gave domains, so the head of the list is
        # the part the host-based filters in `finalize_results` need to read.
        await _resolve_publisher_urls(results[:max_results])
        return results, html

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
