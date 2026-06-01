"""Cache internals: single long-lived connection, WAL + busy_timeout,
race-free initialization, and FTS5 MATCH hardening.

All tests use an isolated tmp sqlite file (never the user's real cache) by
constructing a fresh Cache() and pointing its _path at tmp_path.
"""
from __future__ import annotations

import asyncio

import aiosqlite
import pytest

pytestmark = pytest.mark.asyncio


@pytest.fixture
def fresh_cache(tmp_path, monkeypatch):
    """A brand-new Cache pointed at an isolated tmp sqlite file.

    Also redirect settings.cache_dir so settings.cache_path() (used by the
    Cache constructor) never touches the user's real ~/.cache dir.
    """
    from search_mcp import cache as cache_mod
    from search_mcp.config import settings

    monkeypatch.setattr(settings, "cache_dir", tmp_path)
    c = cache_mod.Cache()
    c._path = str(tmp_path / "test_cache.sqlite")
    return c


# --- #15: single long-lived connection, WAL, busy_timeout, race-free init ---


async def test_conn_is_reused_single_connection(fresh_cache):
    """_conn() must hand back the *same* connection object every time."""
    a = await fresh_cache._conn()
    b = await fresh_cache._conn()
    assert a is b
    assert fresh_cache._conn_obj is a
    await fresh_cache.close()


async def test_journal_mode_is_wal(fresh_cache):
    conn = await fresh_cache._conn()
    cur = await conn.execute("PRAGMA journal_mode")
    row = await cur.fetchone()
    assert row is not None
    assert str(row[0]).lower() == "wal"
    await fresh_cache.close()


async def test_busy_timeout_is_set(fresh_cache):
    conn = await fresh_cache._conn()
    cur = await conn.execute("PRAGMA busy_timeout")
    row = await cur.fetchone()
    assert row is not None
    assert int(row[0]) == 5000
    await fresh_cache.close()


async def test_concurrent_first_access_initializes_once(fresh_cache):
    """Many coroutines hitting _conn() for the first time concurrently must
    all share ONE connection and the schema must be initialized exactly once
    (no race between the read and the write of the init flag)."""
    conns = await asyncio.gather(*[fresh_cache._conn() for _ in range(20)])
    first = conns[0]
    assert all(c is first for c in conns)

    # Schema present exactly once and queryable on the shared connection.
    cur = await first.execute(
        "SELECT name FROM sqlite_master WHERE type='table' AND name='pages'",
    )
    assert await cur.fetchone() is not None
    await fresh_cache.close()


async def test_concurrent_put_page_all_succeed(fresh_cache):
    """Concurrent writes through the single connection must all land."""
    urls = [f"https://example.com/{i}" for i in range(25)]
    await asyncio.gather(
        *[fresh_cache.put_page(u, f"t{i}", f"body {i}") for i, u in enumerate(urls)]
    )
    for i, u in enumerate(urls):
        page = await fresh_cache.get_page(u)
        assert page is not None
        assert page["content"] == f"body {i}"
    await fresh_cache.close()


async def test_external_writer_visible_through_wal(fresh_cache):
    """A second connection (mirroring what test_resources.py does to rewind
    timestamps) must be able to write and have it seen by the long-lived
    connection — i.e. WAL doesn't strand the long-lived reader."""
    url = "https://example.com/wal"
    await fresh_cache.put_page(url, "t", "original")

    conn = await aiosqlite.connect(fresh_cache._path)
    try:
        await conn.execute("PRAGMA busy_timeout=5000")
        await conn.execute(
            "UPDATE pages SET content=? WHERE url=?", ("rewritten", url)
        )
        await conn.commit()
    finally:
        await conn.close()

    page = await fresh_cache.get_page(url)
    assert page is not None
    assert page["content"] == "rewritten"
    await fresh_cache.close()


async def test_close_is_idempotent(fresh_cache):
    await fresh_cache._conn()
    await fresh_cache.close()
    assert fresh_cache._conn_obj is None
    # second close must not raise
    await fresh_cache.close()
    # and the cache must still be usable afterwards (re-opens lazily)
    await fresh_cache.put_page("https://x", "t", "b")
    assert (await fresh_cache.get_page("https://x"))["content"] == "b"
    await fresh_cache.close()


# --- #13: FTS5 MATCH hardening — malformed input returns [], never raises ---


@pytest.mark.parametrize(
    "bad_query",
    [
        "a AND",          # trailing operator
        '"',              # unbalanced quote
        "title:val",      # column filter on a column that isn't queryable
        "AND OR NOT",     # bare operators
        "foo(",           # unbalanced paren
        "NEAR(",          # malformed NEAR
        "*",              # bare prefix token
    ],
)
async def test_search_pages_malformed_query_returns_empty(fresh_cache, bad_query):
    # Seed a row so the FTS index is non-empty (proves we don't just get []
    # because the table is empty).
    await fresh_cache.put_page("https://example.com/a", "Hello", "the quick brown fox")
    result = await fresh_cache.search_pages(bad_query)
    assert result == []
    await fresh_cache.close()


async def test_search_pages_valid_query_still_works(fresh_cache):
    """The try/except must not swallow legitimate matches."""
    await fresh_cache.put_page("https://example.com/a", "Hello", "the quick brown fox")
    await fresh_cache.put_page("https://example.com/b", "Other", "lazy dog sleeps")
    hits = await fresh_cache.search_pages("quick")
    assert len(hits) == 1
    assert hits[0]["url"] == "https://example.com/a"
    await fresh_cache.close()
