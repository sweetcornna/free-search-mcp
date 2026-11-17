"""Offline performance/lifecycle checks beyond the audit's runtime regressions."""
from __future__ import annotations

import asyncio
import threading
import time

import anyio
import pytest

from search_mcp import browser, browser_profile, oauth, server
from search_mcp.cache import Cache
from search_mcp.config import settings


@pytest.fixture
async def cache(monkeypatch):
    monkeypatch.setattr(settings, "cache_max_mb", 1)
    monkeypatch.setattr(settings, "cache_ttl_seconds", 10**9)
    instance = Cache()
    conn = await instance._conn()
    await instance._maintain_task
    try:
        yield instance, conn
    finally:
        await instance.close()


async def test_skewed_search_rows_fit_cap_and_keep_newest(cache):
    instance, conn = cache
    now = int(time.time())
    await conn.executemany(
        "INSERT INTO search_cache(cache_key, query, engines, results, created) "
        "VALUES (?, 'q', 'fake', ?, ?)",
        [(str(i), "x" * (1000 if i < 25 else 250_000), now - 30 + i) for i in range(30)],
    )
    await conn.commit()
    statements = []
    await conn.set_trace_callback(statements.append)
    await instance._maintain(conn)
    assert instance._db_size() <= 1024 * 1024
    async with conn.execute("SELECT cache_key FROM search_cache ORDER BY created") as cur:
        keys = [int(row[0]) for row in await cur.fetchall()]
    assert keys and keys == list(range(keys[0], 30))
    vacuums = statements.count("VACUUM")
    assert vacuums <= 1
    await instance._maintain(conn)
    assert statements.count("VACUUM") == vacuums


async def test_mixed_cache_keeps_newest_in_both_tables(cache):
    instance, conn = cache
    now = int(time.time())
    for table, key, stamp in (("pages", "url", "fetched"),
                              ("search_cache", "cache_key", "created")):
        if table == "pages":
            sql = f"INSERT INTO {table}({key}, content, {stamp}) VALUES (?, ?, ?)"
        else:
            sql = (f"INSERT INTO {table}({key}, results, {stamp}, query, engines) "
                   "VALUES (?, ?, ?, 'q', 'fake')")
        await conn.executemany(sql, [(str(i), "x" * 50_000, now - 30 + i) for i in range(30)])
    await conn.commit()
    await instance._maintain(conn)
    assert instance._db_size() <= 1024 * 1024
    for table, key, stamp in (("pages", "url", "fetched"),
                              ("search_cache", "cache_key", "created")):
        async with conn.execute(f"SELECT {key} FROM {table} ORDER BY {stamp}") as cur:
            keys = [int(row[0]) for row in await cur.fetchall()]
        assert 0 < len(keys) < 30
        assert keys == list(range(keys[0], 30))


async def test_page_eviction_stops_before_deleting_a_small_search(cache):
    instance, conn = cache
    now = int(time.time())
    await conn.executemany(
        "INSERT INTO pages(url, content, fetched) VALUES (?, ?, ?)",
        [(str(i), "x" * 600_000, now - 2 + i) for i in range(2)],
    )
    await conn.commit()
    await instance.put_search("newest", "q", ["fake"], [{"snippet": "small"}])
    assert instance._db_size() > 1024 * 1024
    await instance._maintain(conn)
    assert instance._db_size() <= 1024 * 1024
    assert await instance.get_search("newest") is not None
    async with conn.execute("SELECT url FROM pages") as cur:
        assert await cur.fetchall() == [("1",)]


async def test_wal_growth_alone_never_evicts_or_vacuums(cache):
    instance, conn = cache
    await conn.execute("PRAGMA wal_autocheckpoint=0")
    for _ in range(20):
        await instance.put_search("same", "q", ["fake"], [{"snippet": "x" * 80_000}])
    assert instance._db_size() > 1024 * 1024
    statements = []
    await conn.set_trace_callback(statements.append)
    await instance._maintain(conn)
    assert await instance.get_search("same") is not None
    assert instance._db_size() <= 1024 * 1024
    assert "VACUUM" not in statements


async def test_no_reclaimable_pages_means_no_repeated_vacuum(cache, monkeypatch):
    instance, conn = cache
    # An untruncatable WAL (e.g. another reader) is not reclaimable DB storage.
    monkeypatch.setattr(instance, "_db_size", lambda: 2 * 1024 * 1024)
    statements = []
    await conn.set_trace_callback(statements.append)
    await instance._maintain(conn)
    await instance._maintain(conn)
    assert "VACUUM" not in statements


@pytest.mark.parametrize("stage", ["start", "script"])
async def test_cancelled_initialization_closes_owned_resources(monkeypatch, stage):
    entered = asyncio.Event()
    closed = []

    class Context:
        async def add_init_script(self, script):
            entered.set()
            await asyncio.Event().wait()

        async def close(self):
            closed.append("context")

    class Driver:
        async def stop(self):
            closed.append("driver")

    class Manager:
        async def start(self):
            if stage == "start":
                entered.set()
                await asyncio.Event().wait()
            return Driver()

        async def __aexit__(self, *args):
            closed.append("manager")

    async def launch(*args):
        return Context()

    monkeypatch.setattr(browser, "async_playwright", Manager)
    monkeypatch.setattr(browser, "_launch_context", launch)
    pool = browser.BrowserPool()
    task = asyncio.create_task(pool._ensure())
    try:
        await asyncio.wait_for(entered.wait(), 2)
        task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await task
        assert closed == (["manager"] if stage == "start" else ["context", "driver"])
        assert pool._ctx is None and pool._playwright is None and pool._lease is None
        assert not pool._lock.locked()
    finally:
        await pool.shutdown()


async def test_cancelled_login_reclaims_private_profile_without_loop_callback(monkeypatch):
    entered, release, reclaimed = threading.Event(), threading.Event(), threading.Event()
    real_acquire = browser_profile.acquire_private
    leases = []

    def acquire(path):
        lease = real_acquire(path)
        leases.append(lease)
        real_release = lease.release

        def reclaim():
            real_release()
            reclaimed.set()

        lease.release = reclaim
        entered.set()
        release.wait(5)
        return lease

    class Manager:
        async def start(self):
            pytest.fail("cancelled profile acquisition must not launch a browser")

        async def __aexit__(self, *args):
            pass

    monkeypatch.setattr(browser_profile, "acquire_private", acquire)
    monkeypatch.setattr(browser, "async_playwright", Manager)
    task = asyncio.create_task(browser.BrowserPool().login("https://example.invalid"))
    try:
        assert await asyncio.to_thread(entered.wait, 2)
        task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await asyncio.wait_for(task, 1)
        release.set()
        # Intentionally block this loop: reclamation must live in the worker,
        # not in a completion callback that cannot run once asyncio.run exits.
        assert reclaimed.wait(2)
        assert not leases[0].path.exists()
    finally:
        release.set()
        for lease in leases:
            lease.release()


async def test_cancelled_published_lease_finishes_release_under_anyio(monkeypatch):
    released = []
    real_to_thread = asyncio.to_thread

    class Lease:
        def release(self):
            released.append(True)

    monkeypatch.setattr(browser_profile, "acquire", lambda path: Lease())
    with anyio.CancelScope() as scope:
        async def to_thread(work, *args):
            # Deliver cancellation after publication, before the loop receives
            # the result; make cleanup's await an explicit cancellation point.
            if work.__name__ == "work":
                await real_to_thread(work, *args)
                scope.cancel()
            await anyio.lowlevel.checkpoint()
            return await real_to_thread(work, *args)

        monkeypatch.setattr(asyncio, "to_thread", to_thread)
        await browser_profile.acquire_async(settings.cache_dir)
    assert released == [True]


async def test_cancelled_lifespan_still_drains_refreshes_and_closes(monkeypatch):
    closed = []

    async def noop():
        pass

    def cleanup(name):
        async def run():
            await anyio.lowlevel.checkpoint()
            closed.append(name)
        return run

    monkeypatch.setattr(server, "prewarm", noop)
    monkeypatch.setattr(server, "_report_agent_problem", noop)
    monkeypatch.setattr(oauth, "finish_refreshes", cleanup("refreshes"))
    monkeypatch.setattr(server.pool, "shutdown", cleanup("browser"))
    monkeypatch.setattr(server.cache, "close", cleanup("cache"))
    with anyio.CancelScope() as scope:
        async with server._lifespan(server.mcp):
            scope.cancel()
    assert closed == ["refreshes", "browser", "cache"]


def test_run_does_not_create_cleanup_loops(monkeypatch):
    runs, closed = [], []

    async def noop():
        pass

    async def refreshes():
        closed.append(("refreshes", asyncio.get_running_loop()))

    async def shutdown():
        closed.append(("browser", asyncio.get_running_loop()))

    async def close():
        closed.append(("cache", asyncio.get_running_loop()))

    def run(**kwargs):
        async def serve():
            runs.append(asyncio.get_running_loop())
            async with server._lifespan(server.mcp):
                pass
        asyncio.run(serve())

    monkeypatch.setattr(server, "prewarm", noop)
    monkeypatch.setattr(server, "_report_agent_problem", noop)
    monkeypatch.setattr(oauth, "finish_refreshes", refreshes)
    monkeypatch.setattr(server.pool, "shutdown", shutdown)
    monkeypatch.setattr(server.cache, "close", close)
    monkeypatch.setattr(server.downloads, "purge_expired", lambda: 0)
    monkeypatch.setattr(server.mcp, "run", run)
    server.run()
    assert closed == [(name, runs[0]) for name in ("refreshes", "browser", "cache")]
