"""The browser no longer borrows the user's Chrome or grows without bound.

Every MCP client starts a server, and every server used to launch the
installed Google Chrome on one shared ``browser_profile``. On macOS the system
then counted the headless instance as the user's Chrome, so with theirs closed
the Dock icon reopened a windowless browser and Chrome seemed to hang; and the
shared directory, written by many browsers at once, reached 1.5 GB of caches.
"""

from __future__ import annotations

import json
import subprocess
import sys
import textwrap
from pathlib import Path

import pytest

from search_mcp import browser as browser_mod
from search_mcp import browser_profile
from search_mcp.browser import BrowserPool
from search_mcp.config import settings


def _profile(root, name="Default"):
    p = root / name
    p.mkdir(parents=True)
    (p / "Preferences").write_text("{}")
    return p


def test_prune_keeps_state_and_drops_caches(tmp_path):
    default = _profile(tmp_path)
    other = _profile(tmp_path, "Profile 1")
    for prof in (default, other):
        for cache in ("Cache", "Code Cache", "GPUCache", "Service Worker"):
            (prof / cache / "x").mkdir(parents=True)
        (prof / "Cookies").write_text("c")
        (prof / "Local Storage").mkdir()
    (tmp_path / "GraphiteDawnCache").mkdir()
    (tmp_path / "optimization_guide_model_store").mkdir()
    (tmp_path / "BrowserMetrics-spare.pma").write_text("m")
    (tmp_path / "Local State").write_text("{}")

    browser_profile.prune_caches(tmp_path)

    for prof in (default, other):
        assert sorted(p.name for p in prof.iterdir()) == [
            "Cookies",
            "Local Storage",
            "Preferences",
        ]
    assert sorted(p.name for p in tmp_path.iterdir()) == [
        "Default",
        "Local State",
        "Profile 1",
    ]


def test_one_process_holds_the_shared_profile(tmp_path):
    first = browser_profile.acquire(tmp_path)
    second = browser_profile.acquire(tmp_path)
    try:
        assert first.shared and first.path == tmp_path / "browser_profile"
        assert not second.shared
        assert second.path.parent == tmp_path / "browser_profiles"
        assert second.path.is_dir()
    finally:
        second.release()
        first.release()
    # A throwaway profile goes with its lease; the shared lock is free again.
    assert not second.path.exists()
    assert list((tmp_path / "browser_profiles").iterdir()) == []
    again = browser_profile.acquire(tmp_path)
    assert again.shared
    again.release()


def test_shared_profile_is_pruned_when_leased(tmp_path):
    default = _profile(tmp_path / "browser_profile")
    (default / "Code Cache" / "js").mkdir(parents=True)
    (default / "Cookies").write_text("c")
    lease = browser_profile.acquire(tmp_path)
    lease.release()
    assert not (default / "Code Cache").exists()
    assert (default / "Cookies").read_text() == "c"


def test_profile_of_a_killed_process_is_swept(tmp_path):
    """The OS lock dies with its process, so the next one deletes the leftovers."""
    script = textwrap.dedent(
        f"""
        import os, sys
        from pathlib import Path
        from search_mcp import browser_profile
        lease = browser_profile.acquire_private(Path({str(tmp_path)!r}))
        (lease.path / "Default").mkdir()
        print(lease.path, flush=True)
        os._exit(0)  # killed: no release
        """
    )
    out = subprocess.run(
        [sys.executable, "-c", script], capture_output=True, text=True, check=True
    )
    leftover = Path(out.stdout.strip())
    assert leftover.is_dir()

    lease = browser_profile.acquire_private(tmp_path)
    try:
        assert not leftover.exists()
        assert not leftover.with_name(leftover.name + ".lock").exists()
    finally:
        lease.release()


def test_live_private_profile_is_not_swept(tmp_path):
    held = browser_profile.acquire_private(tmp_path)
    other = browser_profile.acquire_private(tmp_path)
    try:
        assert held.path.is_dir()
        assert held.path != other.path
    finally:
        other.release()
        held.release()


def test_login_cookies_merge(tmp_path):
    base = [{"name": "a", "domain": ".x.com", "path": "/", "value": "old"}]
    new = [
        {"name": "a", "domain": ".x.com", "path": "/", "value": "new"},
        {"name": "z_c0", "domain": ".x.com", "path": "/", "value": "tok"},
    ]
    browser_profile.write_login_cookies(tmp_path, base, new)
    got = browser_profile.read_login_cookies(tmp_path)
    assert {c["name"]: c["value"] for c in got} == {"a": "new", "z_c0": "tok"}
    if sys.platform != "win32":
        assert (tmp_path / browser_profile.LOGIN_COOKIES).stat().st_mode & 0o077 == 0


def test_login_cookies_unreadable_is_empty(tmp_path):
    assert browser_profile.read_login_cookies(tmp_path) == []
    (tmp_path / browser_profile.LOGIN_COOKIES).write_text("{not json")
    assert browser_profile.read_login_cookies(tmp_path) == []


# --- the pool ---------------------------------------------------------------


class _Page:
    def set_default_timeout(self, ms):
        pass

    async def close(self):
        pass


class _Ctx:
    def __init__(self):
        self.added: list[list[dict]] = []

    async def add_init_script(self, script):
        pass

    async def new_page(self):
        return _Page()

    async def add_cookies(self, cookies):
        self.added.append(cookies)

    async def close(self):
        pass


class _Driver:
    def __init__(self, *, chrome_ok: bool = True):
        self.calls: list[dict] = []
        self.chrome_ok = chrome_ok
        self.chromium = self
        self.ctx = _Ctx()

    async def launch_persistent_context(self, **kwargs):
        self.calls.append(kwargs)
        if "channel" in kwargs and not self.chrome_ok:
            raise Exception("Chromium distribution 'chrome' is not found")
        return self.ctx

    async def stop(self):
        pass


@pytest.fixture
def driver(monkeypatch, tmp_path):
    monkeypatch.setattr(settings, "cache_dir", tmp_path)
    drv = _Driver()

    class _Manager:
        async def start(self):
            return drv

        async def __aexit__(self, *exc):
            pass

    monkeypatch.setattr(browser_mod, "async_playwright", lambda: _Manager())
    return drv


async def test_default_launch_is_bundled_chromium(driver, tmp_path):
    pool = BrowserPool()
    await pool._ensure()
    await pool.shutdown()
    (call,) = driver.calls
    assert "channel" not in call
    assert call["user_data_dir"] == str(tmp_path / "browser_profile")
    assert f"--disk-cache-size={settings.browser_disk_cache_bytes}" in call["args"]


async def test_installed_chrome_is_opt_in(driver, monkeypatch):
    monkeypatch.setattr(settings, "browser_channel", "chrome")
    driver.chrome_ok = False
    pool = BrowserPool()
    await pool._ensure()
    await pool.shutdown()
    assert [c.get("channel") for c in driver.calls] == ["chrome", None]


async def test_second_pool_gets_a_throwaway_profile(driver, tmp_path):
    first, second = BrowserPool(), BrowserPool()
    await first._ensure()
    await second._ensure()
    private = driver.calls[1]["user_data_dir"]
    assert driver.calls[0]["user_data_dir"] == str(tmp_path / "browser_profile")
    assert private.startswith(str(tmp_path / "browser_profiles"))
    await second.shutdown()
    await first.shutdown()
    assert list((tmp_path / "browser_profiles").iterdir()) == []


async def test_pool_adds_login_cookies_once_per_change(driver, tmp_path):
    pool = BrowserPool()
    async with pool.page():
        pass
    assert driver.ctx.added == []

    cookie = {"name": "z_c0", "domain": ".zhihu.com", "path": "/", "value": "1"}
    browser_profile.write_login_cookies(tmp_path, [], [cookie])
    async with pool.page():
        pass
    async with pool.page():
        pass
    assert driver.ctx.added == [[cookie]]
    await pool.shutdown()


async def test_login_runs_in_its_own_profile_and_saves_cookies(driver, tmp_path):
    cookie = {"name": "z_c0", "domain": ".zhihu.com", "path": "/", "value": "tok"}

    class _LoginPage(_Page):
        def is_closed(self):
            return False

        async def goto(self, url, **kw):
            pass

    async def new_page():
        return _LoginPage()

    async def cookies():
        return [cookie]

    driver.ctx.new_page = new_page
    driver.ctx.cookies = cookies

    server = BrowserPool()
    await server._ensure()  # a server holds the shared profile meanwhile
    assert await BrowserPool().login("https://www.zhihu.com", timeout_ms=1000)
    login_call = driver.calls[1]
    assert login_call["headless"] is False
    assert login_call["user_data_dir"].startswith(str(tmp_path / "browser_profiles"))
    saved = json.loads((tmp_path / browser_profile.LOGIN_COOKIES).read_text())
    assert saved == [cookie]
    # The throwaway login profile is gone.
    assert list((tmp_path / "browser_profiles").iterdir()) == []
    await server.shutdown()


@pytest.mark.parametrize("platform", ["linux", "darwin"])
async def test_missing_bundled_chromium_falls_back_except_on_macos(
    driver, monkeypatch, platform
):
    """An installed Chrome stands in for a missing bundled Chromium only where
    it runs as a separate instance; on macOS the install hint is the answer."""
    from search_mcp.browser import BrowserUnavailableError

    monkeypatch.setattr(browser_mod.sys, "platform", platform)
    real = driver.launch_persistent_context

    async def launch(**kwargs):
        if "channel" not in kwargs:
            driver.calls.append(kwargs)
            raise Exception(
                "BrowserType.launch_persistent_context: Executable doesn't exist at /x"
            )
        return await real(**kwargs)

    driver.launch_persistent_context = launch
    pool = BrowserPool()
    if platform == "darwin":
        with pytest.raises(BrowserUnavailableError):
            await pool._ensure()
        assert [c.get("channel") for c in driver.calls] == [None]
    else:
        await pool._ensure()
        assert [c.get("channel") for c in driver.calls] == [None, "chrome"]
    await pool.shutdown()
