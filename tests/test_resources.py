"""Resource template + cache max_age_seconds override.

Uses an isolated cache file via a monkeypatched settings.cache_path so we
don't trample the user's real cache.
"""
from __future__ import annotations

import time

import pytest

pytestmark = pytest.mark.asyncio


@pytest.fixture
def isolated_cache(tmp_path, monkeypatch):
    """Force the singleton cache to use a tmp sqlite file for this test."""
    from search_mcp import cache as cache_mod

    fresh = cache_mod.Cache()
    fresh._path = str(tmp_path / "test_cache.sqlite")
    monkeypatch.setattr(cache_mod, "cache", fresh)
    # The server module imported `cache` at module load — patch there too.
    from search_mcp import server as server_mod
    monkeypatch.setattr(server_mod, "cache", fresh)
    return fresh


async def test_resource_template_registered():
    from search_mcp.server import mcp
    templates = await mcp.list_resource_templates()
    assert len(templates) == 2
    by_uri = {t.uriTemplate: t for t in templates}
    page_uri = next(u for u in by_uri if "cache://page/" in u)
    search_uri = next(u for u in by_uri if "cache://search/" in u)
    assert by_uri[page_uri].title == "Cached page"
    assert by_uri[search_uri].title == "Cached search result"


async def test_cached_search_resource_returns_json(isolated_cache):
    """The cache://search/{query_hash} template should round-trip a known key."""
    import json

    from search_mcp.server import mcp

    query_hash = "deadbeef"
    rows = [{"url": "https://x.example", "title": "X", "snippet": "s"}]
    await isolated_cache.put_search(query_hash, "q", ["duckduckgo"], rows)

    contents = await mcp.read_resource(f"cache://search/{query_hash}")
    items = list(contents)
    body = "".join(getattr(c, "content", "") or "" for c in items)
    parsed = json.loads(body)
    assert parsed == rows


async def test_cached_search_resource_misses_raise(isolated_cache):
    from search_mcp.server import mcp
    with pytest.raises(Exception):
        await mcp.read_resource("cache://search/no-such-hash")


async def test_cached_page_resource_returns_content(isolated_cache):
    from urllib.parse import quote
    from search_mcp.server import mcp
    url = "https://example.com/article"
    await isolated_cache.put_page(url, "Example", "Hello world body")

    encoded = quote(url, safe="")
    contents = await mcp.read_resource(f"cache://page/{encoded}")
    # read_resource returns an iterable of ReadResourceContents
    items = list(contents)
    assert items, "should return at least one content item"
    body = "".join(getattr(c, "content", "") or "" for c in items)
    assert "Hello world body" in body


async def test_cached_page_resource_misses_raise(isolated_cache):
    from urllib.parse import quote
    from search_mcp.server import mcp
    with pytest.raises(Exception):
        await mcp.read_resource(
            f"cache://page/{quote('https://nope.example.com/', safe='')}",
        )


async def test_get_page_max_age_seconds_override(isolated_cache):
    """Old entry should miss when max_age_seconds is shorter than its age."""
    url = "https://example.com/old"
    await isolated_cache.put_page(url, "old", "old body")

    # Manually rewind the fetched timestamp to 1 hour ago.
    import aiosqlite
    one_hour_ago = int(time.time()) - 3600
    conn = await aiosqlite.connect(isolated_cache._path)
    try:
        await conn.execute("UPDATE pages SET fetched=? WHERE url=?", (one_hour_ago, url))
        await conn.commit()
    finally:
        await conn.close()

    # default TTL = 7d, so this should still hit
    fresh_default = await isolated_cache.get_page(url)
    assert fresh_default is not None

    # 30 minutes max -> should miss
    miss = await isolated_cache.get_page(url, max_age_seconds=30 * 60)
    assert miss is None

    # 2 hours max -> should still hit
    hit = await isolated_cache.get_page(url, max_age_seconds=2 * 3600)
    assert hit is not None


async def test_cache_search_invalid_fts_surfaces_clean_hint(isolated_cache):
    """A malformed FTS query (`a AND`) must return a clean empty result with a
    'invalid syntax' hint — no raw OperationalError / SQL text leaks (#13)."""
    from search_mcp.server import cache_search

    out = await cache_search("a AND", format="markdown")
    assert isinstance(out, str)
    assert "invalid" in out.lower()
    assert "AND" in out  # the friendly explanation mentions the operator
    # Must NOT leak SQLite internals.
    lower = out.lower()
    assert "operationalerror" not in lower
    assert "fts5" not in lower or "phrases" in lower  # only the friendly mention
    assert "syntax error" not in lower


async def test_cache_search_unterminated_quote_hint(isolated_cache):
    from search_mcp.server import cache_search

    out = await cache_search('"unterminated', format="markdown")
    assert isinstance(out, str)
    assert "quote" in out.lower()
    assert "operationalerror" not in out.lower()


async def test_cache_search_valid_query_empty_cache_uses_normal_message(isolated_cache):
    """A *valid* query that simply has no matches keeps the populate-the-cache
    message, NOT the invalid-syntax hint."""
    from search_mcp.server import cache_search

    out = await cache_search("zebra", format="markdown")
    assert isinstance(out, str)
    assert "No cached pages match" in out
    assert "invalid" not in out.lower()


async def test_cache_search_json_format_returns_list_on_bad_query(isolated_cache):
    """json format keeps its list contract even for a malformed query — empty
    list, never a raised exception."""
    from search_mcp.server import cache_search

    out = await cache_search("a AND", format="json")
    assert out == []


async def test_invalid_fts_hint_helper():
    """Unit-test the detector directly so the heuristic is pinned."""
    from search_mcp.server import _invalid_fts_hint

    assert _invalid_fts_hint("a AND") is not None
    assert _invalid_fts_hint("AND") is not None
    assert _invalid_fts_hint("NOT term") is not None
    assert _invalid_fts_hint('"open phrase') is not None
    assert _invalid_fts_hint("(a OR b") is not None
    assert _invalid_fts_hint("a AND OR b") is not None
    # Valid queries -> None.
    assert _invalid_fts_hint("hello world") is None
    assert _invalid_fts_hint("cats AND dogs") is None
    assert _invalid_fts_hint('"exact phrase"') is None
    assert _invalid_fts_hint("(a OR b) c") is None
    assert _invalid_fts_hint("") is None
    assert _invalid_fts_hint("   ") is None


async def test_get_search_max_age_seconds_override(isolated_cache):
    """Same TTL override semantics for the search cache."""
    key = "abc123"
    await isolated_cache.put_search(
        key, "query", ["duckduckgo"], [{"url": "https://x", "title": "x", "snippet": "s"}],
    )
    import aiosqlite
    one_hour_ago = int(time.time()) - 3600
    conn = await aiosqlite.connect(isolated_cache._path)
    try:
        await conn.execute(
            "UPDATE search_cache SET created=? WHERE cache_key=?",
            (one_hour_ago, key),
        )
        await conn.commit()
    finally:
        await conn.close()

    assert await isolated_cache.get_search(key) is not None
    assert await isolated_cache.get_search(key, max_age_seconds=10 * 60) is None
    assert await isolated_cache.get_search(key, max_age_seconds=2 * 3600) is not None
