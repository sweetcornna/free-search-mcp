"""A stalled browser launch costs one failed request, not the whole process (#19).

Before the bound, a driver start or launch that never returned held
BrowserPool's lock forever, and every later fetch queued behind it until the
MCP client timed out. These tests stand in for Playwright with stubs that hang.
"""

from __future__ import annotations

import asyncio

import pytest
from curl_cffi.requests.exceptions import RequestException

from search_mcp import browser as browser_mod
from search_mcp.browser import (
    BrowserLaunchError,
    BrowserPool,
    BrowserUnavailableError,
)
from search_mcp.config import settings
from search_mcp.engines.base import Engine

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.


class _Playwright:
    """A driver whose launch hangs (``hang=True``) or fails like a missing binary."""

    def __init__(self, *, hang: bool):
        self.hang = hang
        self.stopped = False
        self.launches = 0
        self.chromium = self

    async def launch_persistent_context(self, **kwargs):
        self.launches += 1
        if self.hang:
            await asyncio.Event().wait()
        raise Exception("BrowserType.launch_persistent_context: Executable doesn't exist at /x")

    async def stop(self):
        self.stopped = True


class _AsyncPlaywright:
    """Stands in for ``async_playwright()``; counts driver starts."""

    def __init__(
        self, *, hang_start: bool = False, hang_launch: bool = True, hang_exit: bool = False
    ):
        self.hang_start = hang_start
        self.hang_launch = hang_launch
        self.hang_exit = hang_exit
        self.starts = 0
        self.exits = 0
        self.drivers: list[_Playwright] = []

    def __call__(self):
        return self

    async def start(self):
        self.starts += 1
        if self.hang_start:
            await asyncio.Event().wait()
        driver = _Playwright(hang=self.hang_launch)
        self.drivers.append(driver)
        return driver

    async def __aexit__(self, *exc):
        # What Playwright.stop() calls, and the only way to stop a driver whose
        # start() never returned.
        self.exits += 1
        if self.hang_exit:
            await asyncio.Event().wait()


@pytest.fixture
def fast_timeouts(monkeypatch, tmp_path):
    monkeypatch.setattr(settings, "cache_dir", tmp_path)
    monkeypatch.setattr(settings, "browser_launch_timeout", 0.2)
    monkeypatch.setattr(settings, "browser_launch_cooldown", 60.0)


async def test_stalled_launch_times_out_and_releases_the_lock(monkeypatch, fast_timeouts):
    fake = _AsyncPlaywright()
    monkeypatch.setattr(browser_mod, "async_playwright", fake)
    pool = BrowserPool()

    with pytest.raises(BrowserLaunchError) as exc:
        await asyncio.wait_for(pool.fetch_html("https://example.com"), timeout=5)
    assert "did not start within" in str(exc.value)
    assert "install" not in str(exc.value).lower()
    # The stalled driver was stopped, not leaked.
    assert fake.drivers[0].stopped
    assert pool._playwright is None and pool._ctx is None
    assert not pool._lock.locked()
    # A missing binary is not what happened, so engines needing the browser
    # are not skipped as if it were.
    assert not pool.known_unavailable

    # The next call fails at once from the cooldown, without a new driver.
    with pytest.raises(BrowserLaunchError):
        await asyncio.wait_for(pool.fetch_html("https://example.com"), timeout=0.1)
    assert fake.starts == 1


async def test_stalled_driver_start_times_out(monkeypatch, fast_timeouts):
    fake = _AsyncPlaywright(hang_start=True)
    monkeypatch.setattr(browser_mod, "async_playwright", fake)
    pool = BrowserPool()

    with pytest.raises(BrowserLaunchError):
        await asyncio.wait_for(pool.fetch_html("https://example.com"), timeout=5)
    assert not pool._lock.locked()
    # start() never returned a handle, so the manager is what gets stopped.
    assert fake.exits == 1


async def test_concurrent_fetches_do_not_queue_forever(monkeypatch, fast_timeouts):
    monkeypatch.setattr(browser_mod, "async_playwright", _AsyncPlaywright())
    pool = BrowserPool()

    results = await asyncio.wait_for(
        asyncio.gather(
            *(pool.fetch_html(f"https://example.com/{i}") for i in range(4)),
            return_exceptions=True,
        ),
        timeout=5,
    )
    assert all(isinstance(r, BrowserLaunchError) for r in results)


async def test_cooldown_expires_and_the_launch_is_retried(monkeypatch, fast_timeouts):
    monkeypatch.setattr(settings, "browser_launch_cooldown", 0.0)
    fake = _AsyncPlaywright()
    monkeypatch.setattr(browser_mod, "async_playwright", fake)
    pool = BrowserPool()

    for _ in range(2):
        with pytest.raises(BrowserLaunchError):
            await asyncio.wait_for(pool.fetch_html("https://example.com"), timeout=5)
    assert fake.starts == 2


async def test_shutdown_clears_the_cooldown(monkeypatch, fast_timeouts):
    fake = _AsyncPlaywright()
    monkeypatch.setattr(browser_mod, "async_playwright", fake)
    pool = BrowserPool()

    with pytest.raises(BrowserLaunchError):
        await asyncio.wait_for(pool.fetch_html("https://example.com"), timeout=5)
    await pool.shutdown()
    with pytest.raises(BrowserLaunchError):
        await asyncio.wait_for(pool.fetch_html("https://example.com"), timeout=5)
    assert fake.starts == 2


async def test_missing_binary_still_raises_the_install_hint(monkeypatch, fast_timeouts):
    fake = _AsyncPlaywright(hang_launch=False)
    monkeypatch.setattr(browser_mod, "async_playwright", fake)
    pool = BrowserPool()

    with pytest.raises(BrowserUnavailableError) as exc:
        await asyncio.wait_for(pool.fetch_html("https://example.com"), timeout=5)
    assert "playwright install chromium" in str(exc.value)
    assert pool.known_unavailable
    assert fake.drivers[0].stopped


class _NoResultsEngine(Engine):
    name = "dummy"

    def build_url(self, query, max_results, filters=None):
        return "https://dummy.example/search"

    def parse(self, html):
        return []


async def _stalled(*args, **kwargs):
    raise BrowserLaunchError("The browser did not start within 30s.")


async def test_engine_fallback_survives_a_stalled_launch(monkeypatch):
    monkeypatch.setattr("search_mcp.engines.base.pool.fetch_html", _stalled)
    engine = _NoResultsEngine()

    async def _fetch(url):
        return "<html><body>plain empty page</body></html>"

    monkeypatch.setattr(engine, "_fetch", _fetch)

    diag: dict = {}
    assert await engine.search("anything", 5, diagnostics=diag) == []
    # A stall is not a missing browser, so no install-hint gate is recorded.
    assert "dummy" not in diag.get("gated", {})


async def test_http_error_surfaces_when_the_fallback_launch_stalls(monkeypatch):
    monkeypatch.setattr("search_mcp.engines.base.pool.fetch_html", _stalled)
    engine = _NoResultsEngine()

    async def _http_get(url):
        raise RequestException("HTTP 503 from upstream")

    monkeypatch.setattr(engine, "_http_get", _http_get)

    with pytest.raises(RequestException, match="HTTP 503"):
        await engine._fetch("https://dummy.example/search")


def test_tool_layers_treat_a_stall_as_an_expected_failure():
    from search_mcp import agent, server

    assert BrowserLaunchError in server._ANTICIPATED
    assert BrowserLaunchError in agent._TOOL_FAILURES


async def test_login_gives_up_on_a_stalled_launch_and_a_stalled_stop(monkeypatch, fast_timeouts):
    monkeypatch.setattr(browser_mod, "_TEARDOWN_TIMEOUT", 0.1)
    fake = _AsyncPlaywright(hang_exit=True)
    monkeypatch.setattr(browser_mod, "async_playwright", fake)

    ok = await asyncio.wait_for(BrowserPool().login("https://example.com"), timeout=5)
    assert ok is False
    assert fake.exits == 1


def test_browser_trouble_does_not_bench_the_engine():
    """A stall or a missing browser is the machine's problem. Counting it would
    bench browser engines for the breaker's 10 minutes, long after the pool's
    own 20-second cooldown let the browser start again."""
    from search_mcp.aggregator import _record_health
    from search_mcp.health import engine_health

    raised = {
        "startpage": BrowserLaunchError("did not start"),
        "brave": BrowserUnavailableError("not installed"),
        "zhihu": RuntimeError("boom"),
    }
    for _ in range(3):
        _record_health(list(raised), [], raised, {})
    assert not engine_health.is_open("startpage")
    assert not engine_health.is_open("brave")
    assert engine_health.is_open("zhihu")
