"""Caching-correctness regression tests for the max_age_hours plumbing.

Covers:
  #3  search(max_age_hours=N) must WRITE the cache (re-running engines but never
      persisting defeats caching and contradicts the docstring).
  #20 the resolved engine list / cache key must include news-category routing
      so a `category="news"` search hits the same key it wrote.
  A4  cache-hit payloads must still carry lead_snippet so the rendered markdown
      keeps its '> **Lead:**' block.
  #14 research(max_age_hours=N) must thread the TTL into the search portion
      (non-zero values were ignored, getting the full 7-day TTL).

These talk to aggregate_search / the server tools with mocked engines so no
network is hit. We count engine `.search` calls to prove cache behavior.
"""
from __future__ import annotations

import time

import pytest

pytestmark = pytest.mark.asyncio


@pytest.fixture
def isolated_cache(tmp_path, monkeypatch):
    """Point the singleton cache (and every module that imported it) at a tmp db."""
    from search_mcp import aggregator as agg_mod
    from search_mcp import cache as cache_mod
    from search_mcp import server as server_mod

    fresh = cache_mod.Cache()
    fresh._path = str(tmp_path / "test_cache.sqlite")
    monkeypatch.setattr(cache_mod, "cache", fresh)
    monkeypatch.setattr(agg_mod, "cache", fresh)
    monkeypatch.setattr(server_mod, "cache", fresh)
    return fresh


class _FakeResult:
    """Minimal stand-in for engines.SearchResult with a to_dict()."""

    def __init__(self, url: str, title: str, snippet: str, rank: int, engine: str):
        self.url = url
        self.title = title
        self.snippet = snippet
        self.rank = rank
        self.engine = engine
        self.published_age = ""

    def to_dict(self) -> dict:
        return {
            "url": self.url,
            "title": self.title,
            "snippet": self.snippet,
            "rank": self.rank,
            "engine": self.engine,
            "published_age": self.published_age,
        }


@pytest.fixture
def counting_engines(monkeypatch):
    """Patch get_engine so every engine returns a deterministic result list and
    increments a shared call counter. Returns the counter dict."""
    from search_mcp import aggregator as agg_mod

    calls = {"n": 0, "by_engine": {}}

    class _FakeEngine:
        def __init__(self, name: str):
            self.name = name

        async def search(self, query, n, filters=None, diagnostics=None):
            calls["n"] += 1
            calls["by_engine"][self.name] = calls["by_engine"].get(self.name, 0) + 1
            # Long snippet that hits >=2 query terms so lead_snippet qualifies.
            return [
                _FakeResult(
                    url=f"https://{self.name}.example/result",
                    title="Python Async Tutorial Guide",
                    snippet=(
                        "This long article explains python async programming and "
                        "covers tutorial material across coroutines and event loops."
                    ),
                    rank=0,
                    engine=self.name,
                )
            ]

    def fake_get_engine(name: str):
        return _FakeEngine(name)

    monkeypatch.setattr(agg_mod, "get_engine", fake_get_engine)
    # Defang the rate limiter so tests don't sleep.
    async def _noop_acquire(name):
        return None

    monkeypatch.setattr(agg_mod.search_limiter, "acquire", _noop_acquire)
    return calls


# ---------------------------------------------------------------------------
# #3 — max_age_hours must still WRITE the cache
# ---------------------------------------------------------------------------


async def test_max_age_hours_writes_cache(isolated_cache, counting_engines):
    """search(query, max_age_hours=24) -> a row is persisted, and a second call
    within TTL is served from cache without re-running engines."""
    from search_mcp.server import search

    q = "python async tutorial guide"
    first = await search(q, engines=["duckduckgo"], max_age_hours=24, format="json")
    assert first["cached"] is False
    assert counting_engines["n"] == 1  # engines ran once

    # A row must have been persisted.
    from search_mcp.aggregator import _key
    from search_mcp.engines import SearchFilters
    key = _key(q, ["duckduckgo"], 10, SearchFilters())
    persisted = await isolated_cache.get_search(key)
    assert persisted is not None, "max_age_hours search must still write the cache"

    # Second call within TTL -> served from cache, engines NOT re-run.
    second = await search(q, engines=["duckduckgo"], max_age_hours=24, format="json")
    assert second["cached"] is True
    assert counting_engines["n"] == 1, "second call within TTL must not re-run engines"


async def test_max_age_zero_force_refresh_still_writes(isolated_cache, counting_engines):
    """max_age_hours=0 forces a fresh engine run but MUST keep writing the cache."""
    from search_mcp.server import search

    q = "python async tutorial guide"
    await search(q, engines=["duckduckgo"], max_age_hours=0, format="json")
    assert counting_engines["n"] == 1

    from search_mcp.aggregator import _key
    from search_mcp.engines import SearchFilters
    key = _key(q, ["duckduckgo"], 10, SearchFilters())
    assert await isolated_cache.get_search(key) is not None

    # Second forced refresh re-runs engines but a normal cached read would hit.
    await search(q, engines=["duckduckgo"], max_age_hours=0, format="json")
    assert counting_engines["n"] == 2  # force-refresh always re-runs


# ---------------------------------------------------------------------------
# #20 — fresh vs stale + news-routing key consistency
# ---------------------------------------------------------------------------


async def test_fresh_within_max_age_is_cached(isolated_cache, counting_engines):
    from search_mcp.server import search

    q = "python async tutorial guide"
    await search(q, engines=["duckduckgo"], format="json")  # seed cache
    assert counting_engines["n"] == 1
    out = await search(q, engines=["duckduckgo"], max_age_hours=24, format="json")
    assert out["cached"] is True
    assert counting_engines["n"] == 1


async def test_stale_beyond_max_age_refetches(isolated_cache, counting_engines):
    from search_mcp.server import search

    q = "python async tutorial guide"
    await search(q, engines=["duckduckgo"], format="json")  # seed cache
    assert counting_engines["n"] == 1

    # Rewind the cached row to 2h ago.
    import aiosqlite
    two_hours_ago = int(time.time()) - 2 * 3600
    conn = await aiosqlite.connect(isolated_cache._path)
    try:
        await conn.execute("UPDATE search_cache SET created=?", (two_hours_ago,))
        await conn.commit()
    finally:
        await conn.close()

    out = await search(q, engines=["duckduckgo"], max_age_hours=1, format="json")
    assert out["cached"] is False, "row older than max_age must be a miss"
    assert counting_engines["n"] == 2  # engines re-ran


async def test_news_routing_key_is_consistent(isolated_cache, counting_engines):
    """A category='news' search (engines=None) writes a key that includes
    googlenews routing, and a second identical call hits THAT key — no drift."""
    from search_mcp.server import search

    q = "python async tutorial guide"
    first = await search(q, category="news", max_age_hours=24, format="json")
    assert first["cached"] is False
    # googlenews routing appended to the default engine set.
    assert "googlenews" in first["engines"]
    n_after_first = counting_engines["n"]
    assert n_after_first > 0

    second = await search(q, category="news", max_age_hours=24, format="json")
    assert second["cached"] is True, "news search must hit the key it wrote"
    assert counting_engines["n"] == n_after_first, "no re-run -> key didn't drift"


# ---------------------------------------------------------------------------
# A4 — cache-hit markdown keeps the Lead block
# ---------------------------------------------------------------------------


async def test_cache_hit_markdown_keeps_lead_block(isolated_cache, counting_engines):
    from search_mcp.server import search

    q = "python async tutorial guide"
    md1 = await search(q, engines=["duckduckgo"], format="markdown")
    assert "> **Lead:**" in md1  # fresh path has the lead

    md2 = await search(q, engines=["duckduckgo"], format="markdown")
    assert "_(from cache)_" in md2
    assert "> **Lead:**" in md2, "cached markdown must still carry the Lead block"


async def test_cache_hit_json_has_lead_snippet(isolated_cache, counting_engines):
    from search_mcp.server import search

    q = "python async tutorial guide"
    await search(q, engines=["duckduckgo"], format="json")
    cached = await search(q, engines=["duckduckgo"], format="json")
    assert cached["cached"] is True
    assert cached.get("lead_snippet"), "cache hit must recompute lead_snippet"


# ---------------------------------------------------------------------------
# #14 — research threads max_age_hours into the search portion
# ---------------------------------------------------------------------------


async def test_research_nonzero_max_age_refetches_stale_search(
    isolated_cache, counting_engines, monkeypatch
):
    """research(max_age_hours=1) over a 2h-old search row must re-run engines,
    not silently reuse the row under the 7-day default TTL."""
    from search_mcp import research as research_mod
    from search_mcp.server import research as research_tool

    # Stub fetch_many so we don't hit the network for page bodies.
    async def fake_fetch_many(urls, render="auto", **kw):
        return [{"url": u, "error": "stubbed"} for u in urls]

    monkeypatch.setattr(research_mod, "fetch_many", fake_fetch_many)

    q = "python async tutorial guide"
    # Seed the search cache via a plain aggregate_search run.
    from search_mcp.aggregator import aggregate_search
    await aggregate_search(q, engines=["duckduckgo"], max_results=6)
    seeded_calls = counting_engines["n"]
    assert seeded_calls == 1

    # Rewind the seeded search row to 2h ago.
    import aiosqlite
    two_hours_ago = int(time.time()) - 2 * 3600
    conn = await aiosqlite.connect(isolated_cache._path)
    try:
        await conn.execute("UPDATE search_cache SET created=?", (two_hours_ago,))
        await conn.commit()
    finally:
        await conn.close()

    await research_tool(
        q, depth=3, engines=["duckduckgo"], fetch=False, max_age_hours=1, format="json"
    )
    assert counting_engines["n"] > seeded_calls, (
        "research with max_age_hours=1 must treat a 2h-old row as stale and re-run engines"
    )
