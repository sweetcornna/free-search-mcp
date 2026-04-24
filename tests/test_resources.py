"""Resource template + cache max_age_seconds override.

Uses an isolated cache file via a monkeypatched settings.cache_path so we
don't trample the user's real cache.
"""
from __future__ import annotations

import time
from pathlib import Path

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
    assert len(templates) == 1
    t = templates[0]
    assert "cache://page/" in t.uriTemplate
    assert t.title == "Cached page"


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
