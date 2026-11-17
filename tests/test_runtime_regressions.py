"""Offline runtime boundary and lifecycle regressions from the backend audit."""
from __future__ import annotations

import asyncio
import json
import sqlite3
import threading

import aiosqlite
import httpx
import pytest

from search_mcp import admin, browser, browser_profile, keystore, oauth, server
from search_mcp.cache import _SCHEMA, Cache
from search_mcp.config import settings


@pytest.mark.parametrize(
    "headers",
    [
        {"Origin": "https://untrusted.invalid", "Content-Type": "text/plain"},
        {"Host": "untrusted.invalid:8765", "Content-Type": "application/json"},
        {"Sec-Fetch-Site": "cross-site", "Content-Type": "application/json"},
    ],
)
async def test_admin_rejects_cross_site_secret_writes(headers):
    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=admin.app), base_url="http://127.0.0.1:8765"
    ) as client:
        response = await client.post(
            "/api/save", content='{"proxy":"http://untrusted.invalid:8080"}', headers=headers
        )
    assert response.status_code in (400, 403)
    assert keystore.get_secret("proxy") is None


async def test_admin_same_origin_json_write_still_works():
    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=admin.app), base_url="http://127.0.0.1:8765"
    ) as client:
        response = await client.post(
            "/api/save", json={"serper_api_key": "fake-key"},
            headers={"Origin": "http://127.0.0.1:8765"},
        )
    assert response.status_code == 200
    assert keystore.get_secret("serper_api_key") == "fake-key"


@pytest.mark.parametrize("value", [None, [], 1, "not-a-map"])
def test_bad_secret_envelope_is_an_empty_store(value):
    keystore.config_path().write_text(json.dumps({"secrets": value}))
    keystore._reset_cache()
    assert keystore.get_secret("serper_api_key") is None
    assert keystore.all_secrets() == {}


class _Client:
    async def __aenter__(self):
        return self

    async def __aexit__(self, *args):
        pass


def _old_credential():
    oauth.save(oauth.Credential(
        provider="codex", access_token="fake-old", refresh_token="fake-r1",
        expires_at=0, account_id="fake-account",
    ))


def _pending_refresh(monkeypatch):
    entered, release, returned = asyncio.Event(), asyncio.Event(), asyncio.Event()

    async def request(*args):
        entered.set()
        await release.wait()
        returned.set()
        return {"access_token": "fake-new", "refresh_token": "fake-r2", "expires_in": 3600}

    monkeypatch.setattr(oauth, "http_client", lambda *args, **kwargs: _Client())
    monkeypatch.setattr(oauth, "_token_request", request)
    return entered, release, returned


async def test_logout_wins_over_a_refresh_already_in_flight(monkeypatch):
    _old_credential()
    entered, release, _ = _pending_refresh(monkeypatch)
    task = asyncio.create_task(oauth.credential("codex"))
    await entered.wait()
    oauth.sign_out("codex")
    release.set()
    await asyncio.gather(task, return_exceptions=True)
    assert oauth.load("codex") is None


async def test_replacement_signin_wins_over_an_old_refresh(monkeypatch):
    _old_credential()
    entered, release, _ = _pending_refresh(monkeypatch)
    task = asyncio.create_task(oauth.credential("codex"))
    await entered.wait()
    oauth.save(oauth.Credential(provider="codex", access_token="replacement", account_id="new"))
    release.set()
    await asyncio.gather(task, return_exceptions=True)
    assert oauth.load("codex").access_token == "replacement"


async def test_refresh_survives_cancellation_until_rotated_tokens_are_saved(monkeypatch):
    _old_credential()
    entered, release, returned = _pending_refresh(monkeypatch)
    saved = asyncio.Event()
    real_save = oauth._save_record

    def save(cred):
        real_save(cred)
        if cred.refresh_token == "fake-r2":
            saved.set()

    monkeypatch.setattr(oauth, "_save_record", save)
    task = asyncio.create_task(oauth.credential("codex"))
    await entered.wait()
    task.cancel()
    await asyncio.gather(task, return_exceptions=True)
    release.set()
    try:
        await asyncio.wait_for(returned.wait(), 1)
        await asyncio.wait_for(saved.wait(), 1)
        assert oauth.load("codex").refresh_token == "fake-r2"
    finally:
        # Drain any detached refresh before the fixture switches config dirs.
        for pending in list(getattr(oauth, "_refresh_tasks", ())):
            await asyncio.gather(pending, return_exceptions=True)


async def test_logout_cancels_the_admin_pending_signin(monkeypatch):
    waiting, release = asyncio.Event(), asyncio.Event()

    async def login(provider_id, *, on_url, **kwargs):
        on_url("https://example.invalid/authorize")
        waiting.set()
        await release.wait()
        _old_credential()

    monkeypatch.setattr(oauth, "login", login)
    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=admin.app), base_url="http://127.0.0.1:8765"
    ) as client:
        await client.post("/api/oauth/codex/start")
        await waiting.wait()
        job = admin._oauth_jobs["codex"]["task"]
        await client.post("/api/oauth/codex/logout")
        release.set()
        await asyncio.gather(job, return_exceptions=True)
    assert oauth.load("codex") is None


async def test_independent_cache_connections_migrate_once(tmp_path, monkeypatch):
    path = tmp_path / "legacy.sqlite"
    with sqlite3.connect(path) as seed:
        seed.executescript(_SCHEMA.replace("    meta      TEXT,\n", ""))
    conns = [await aiosqlite.connect(path) for _ in range(2)]
    ready = 0
    both = asyncio.Event()
    original = aiosqlite.Connection.execute

    async def execute(self, sql, *args, **kwargs):
        nonlocal ready
        # Force the old unprotected check-then-ALTER interleaving. A proper
        # migration transaction holds the write lock and cannot interleave.
        if sql == "ALTER TABLE search_cache ADD COLUMN meta TEXT" and not self.in_transaction:
            ready += 1
            if ready == 2:
                both.set()
            await both.wait()
        return await original(self, sql, *args, **kwargs)

    async def migrate(conn):
        await Cache._migrate(conn)
        await conn.commit()

    monkeypatch.setattr(aiosqlite.Connection, "execute", execute)
    try:
        outcomes = await asyncio.gather(*(migrate(c) for c in conns), return_exceptions=True)
        assert outcomes == [None, None]
    finally:
        await asyncio.gather(*(c.close() for c in conns))


async def test_search_only_cache_obeys_size_cap(monkeypatch):
    monkeypatch.setattr(settings, "cache_max_mb", 1)
    monkeypatch.setattr(settings, "cache_ttl_seconds", 10**9)
    cache = Cache()
    try:
        conn = await cache._conn()
        await cache._maintain_task
        for index in range(30):
            await cache.put_search(str(index), "q", ["fake"], [{"snippet": "x" * 50_000}])
        await cache._maintain(conn)
        assert cache._db_size() <= 1024 * 1024
    finally:
        await cache.close()


class _Driver:
    def __init__(self):
        self.stopped = False

    async def stop(self):
        self.stopped = True


class _Context:
    async def add_init_script(self, script):
        pass

    async def close(self):
        pass


async def test_cancelled_launch_stops_the_driver_before_retry(monkeypatch):
    entered = asyncio.Event()
    drivers = []

    class Manager:
        async def start(self):
            driver = _Driver()
            drivers.append(driver)
            return driver

        async def __aexit__(self, *args):
            pass

    async def launch(driver, kwargs):
        if len(drivers) == 1:
            entered.set()
            await asyncio.Event().wait()
        return _Context()

    monkeypatch.setattr(browser, "async_playwright", Manager)
    monkeypatch.setattr(browser, "_launch_context", launch)
    pool = browser.BrowserPool()
    try:
        task = asyncio.create_task(pool._ensure())
        await entered.wait()
        task.cancel()
        await asyncio.gather(task, return_exceptions=True)
        await pool._ensure()
    finally:
        await pool.shutdown()
    assert all(driver.stopped for driver in drivers)


async def test_cancelled_profile_acquisition_releases_its_lock(monkeypatch):
    entered, release, done = threading.Event(), threading.Event(), threading.Event()
    real_acquire = browser_profile.acquire
    leases = []

    def acquire(path):
        lease = real_acquire(path)
        leases.append(lease)
        entered.set()
        release.wait(2)
        done.set()
        return lease

    monkeypatch.setattr(browser_profile, "acquire", acquire)
    pool = browser.BrowserPool()
    task = asyncio.create_task(pool._ensure())
    try:
        assert await asyncio.to_thread(entered.wait, 2)
        task.cancel()
        await asyncio.gather(task, return_exceptions=True)
        release.set()
        assert await asyncio.to_thread(done.wait, 2)
        # A queue round-trip lets the worker's completion callback run.
        await asyncio.to_thread(lambda: None)
        await pool.shutdown()
        another = real_acquire(settings.cache_dir)
        try:
            assert another.shared
        finally:
            another.release()
    finally:
        release.set()
        for lease in leases:
            lease.release()


async def test_lifespan_closes_resources_on_the_owner_loop(monkeypatch):
    owner = asyncio.get_running_loop()
    closed = []

    async def noop():
        pass

    async def shutdown():
        assert asyncio.get_running_loop() is owner
        closed.append("browser")

    async def close():
        assert asyncio.get_running_loop() is owner
        closed.append("cache")

    monkeypatch.setattr(server, "prewarm", noop)
    monkeypatch.setattr(server, "_report_agent_problem", noop)
    monkeypatch.setattr(server.pool, "shutdown", shutdown)
    monkeypatch.setattr(server.cache, "close", close)
    async with server._lifespan(server.mcp):
        pass
    assert closed == ["browser", "cache"]
