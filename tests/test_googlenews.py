"""Tests for the Google News RSS engine.

Note: Google News returns news.google.com/articles/CBM... redirect URLs, NOT
direct publisher URLs. The fetcher resolves the redirect later; for engine
output we just preserve the news.google.com link.
"""

import os

import pytest

NETWORK = os.environ.get("SEARCH_MCP_TEST_NETWORK") == "1"
skip_offline = pytest.mark.skipif(
    not NETWORK, reason="set SEARCH_MCP_TEST_NETWORK=1 to run"
)


SAMPLE_RSS = """<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0">
  <channel>
    <title>"AI" - Google News</title>
    <link>https://news.google.com/search?q=AI</link>
    <description>Google News</description>
    <item>
      <title>OpenAI announces new model</title>
      <link>https://news.google.com/articles/CBMiAbcdef?oc=5</link>
      <guid isPermaLink="false">CBMiAbcdef</guid>
      <pubDate>Tue, 28 Apr 2026 15:30:00 GMT</pubDate>
      <description>&lt;a href="https://news.google.com/x"&gt;OpenAI announces new model&lt;/a&gt;&amp;nbsp;&amp;nbsp;&lt;font color="#6f6f6f"&gt;Reuters&lt;/font&gt;</description>
      <source url="https://www.reuters.com">Reuters</source>
    </item>
    <item>
      <title>Anthropic releases Claude update</title>
      <link>https://news.google.com/articles/CBMiXyz123?oc=5</link>
      <guid isPermaLink="false">CBMiXyz123</guid>
      <pubDate>Tue, 28 Apr 2026 12:00:00 GMT</pubDate>
      <description>Latest Claude model brings improvements.</description>
      <source url="https://www.theverge.com">The Verge</source>
    </item>
  </channel>
</rss>
"""


def test_parse_offline_rss():
    """Hand-crafted RSS parses into SearchResult objects with proper titles
    and news.google.com URLs."""
    from search_mcp.engines.googlenews import GoogleNewsEngine

    engine = GoogleNewsEngine()
    results = engine.parse(SAMPLE_RSS)

    assert len(results) >= 1
    first = results[0]
    assert first.title  # non-empty
    assert "news.google.com" in first.url  # redirect URL, not direct publisher
    # Source should be appended to title in parens.
    assert "(Reuters)" in first.title
    # Description HTML/entities should be stripped.
    assert "<" not in first.snippet
    assert "&nbsp;" not in first.snippet
    assert first.engine == "googlenews"


def test_parse_handles_garbage():
    from search_mcp.engines.googlenews import GoogleNewsEngine

    engine = GoogleNewsEngine()
    assert engine.parse("") == []
    assert engine.parse("<not><valid xml") == []


def test_build_url_includes_freshness_and_locale():
    from search_mcp.engines.base import SearchFilters
    from search_mcp.engines.googlenews import GoogleNewsEngine

    engine = GoogleNewsEngine()
    url = engine.build_url("AI", 5, SearchFilters(freshness="day"))
    assert url.startswith("https://news.google.com/rss/search?q=")
    assert "when%3A1d" in url
    assert "ceid=US:en" in url
    assert "hl=en-US" in url


def test_engine_registered():
    from search_mcp.engines import ENGINES, get_engine

    assert "googlenews" in ENGINES
    assert get_engine("googlenews").name == "googlenews"


@skip_offline
@pytest.mark.asyncio
async def test_googlenews_live_search():
    """Live test: query the real RSS endpoint and assert hits, or skip on 0."""
    from search_mcp.aggregator import aggregate_search

    out = await aggregate_search(
        "AI news today",
        engines=["googlenews"],
        max_results=5,
        use_cache=False,
    )
    if not out["results"]:
        pytest.skip("Google News RSS returned no results (transient)")
    assert len(out["results"]) > 0
    # All result URLs should be news.google.com redirects at this stage.
    assert any("news.google.com" in r["url"] for r in out["results"])


# ---------------------------------------------------------------------------
# Publisher-URL resolution (offline: the resolver is stubbed)
# ---------------------------------------------------------------------------


def _feed(n: int) -> str:
    items = "".join(
        f"<item><title>Story {i}</title>"
        f"<link>https://news.google.com/rss/articles/CBM{i}</link>"
        f"<source>Outlet {i}</source></item>"
        for i in range(n)
    )
    return f"<rss><channel>{items}</channel></rss>"


def _wire_resolver(monkeypatch, resolver, feed: str):
    from unittest.mock import AsyncMock

    from search_mcp.engines import googlenews as gn

    engine = gn.GoogleNewsEngine()
    monkeypatch.setattr(engine, "_fetch", AsyncMock(return_value=feed))
    monkeypatch.setattr(gn, "resolve_google_news_url", resolver)
    return engine, gn


async def test_results_carry_publisher_urls_so_host_filters_can_see_them(monkeypatch):
    from search_mcp.engines.base import SearchFilters

    async def resolver(url):
        i = int(url.rsplit("CBM", 1)[1])
        return f"https://{'reuters.com' if i % 2 == 0 else 'example.net'}/story/{i}"

    engine, _ = _wire_resolver(monkeypatch, resolver, _feed(6))

    everything = await engine.search("q", 10)
    assert all("news.google.com" not in r.url for r in everything)

    # Before this, every item's host was news.google.com and this matched nothing.
    only_reuters = await engine.search("q", 10, SearchFilters(include_domains=["reuters.com"]))
    assert [r.url for r in only_reuters] == [
        "https://reuters.com/story/0",
        "https://reuters.com/story/2",
        "https://reuters.com/story/4",
    ]


async def test_resolution_is_bounded_in_parallelism(monkeypatch):
    import asyncio

    live = peak = 0

    async def resolver(url):
        nonlocal live, peak
        live += 1
        peak = max(peak, live)
        await asyncio.sleep(0.01)
        live -= 1
        return url.replace("news.google.com", "publisher.example")

    engine, gn = _wire_resolver(monkeypatch, resolver, _feed(10))
    await engine.search("q", 10)
    assert peak == gn._RESOLVE_CONCURRENCY


async def test_only_the_results_that_can_be_returned_are_resolved(monkeypatch):
    seen = []

    async def resolver(url):
        seen.append(url)  # and resolve nothing

    engine, _ = _wire_resolver(monkeypatch, resolver, _feed(10))
    monkeypatch.setattr("search_mcp.engines.googlenews._RESOLVE_MAX_FAILURES", 99)
    await engine.search("q", 3)
    assert len(seen) == 3


async def test_a_slow_resolver_costs_the_budget_and_keeps_the_redirect_links(monkeypatch):
    import asyncio
    import time

    async def resolver(url):
        await asyncio.sleep(30)
        return "https://never.example/"

    engine, gn = _wire_resolver(monkeypatch, resolver, _feed(5))
    monkeypatch.setattr(gn, "_RESOLVE_BUDGET_SECONDS", 0.05)

    started = time.monotonic()
    results = await engine.search("q", 10)

    assert time.monotonic() - started < 2
    assert len(results) == 5
    assert all("news.google.com" in r.url for r in results)


async def test_resolution_gives_up_after_consecutive_failures(monkeypatch):
    calls = 0

    async def resolver(url):
        nonlocal calls
        calls += 1  # and resolve nothing

    engine, gn = _wire_resolver(monkeypatch, resolver, _feed(10))
    monkeypatch.setattr(gn, "_RESOLVE_CONCURRENCY", 1)

    results = await engine.search("q", 10)

    assert calls == gn._RESOLVE_MAX_FAILURES
    assert len(results) == 10
