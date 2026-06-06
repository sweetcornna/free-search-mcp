"""Admin Network/Proxy card + zhihu browser-login tests — fully offline.

Driven through Starlette's TestClient (no uvicorn, no real browser). Each test
points SEARCH_MCP_CONFIG_DIR at a tmp dir so the real ~/.config is never
touched, and resets the keystore hot-reload cache between cases.
"""

from __future__ import annotations

import sys
import types

import pytest
from starlette.testclient import TestClient
from unittest.mock import AsyncMock

from search_mcp import keystore
from search_mcp.admin import app


@pytest.fixture(autouse=True)
def _tmp_config(tmp_path, monkeypatch):
    monkeypatch.setenv("SEARCH_MCP_CONFIG_DIR", str(tmp_path))
    keystore._reset_cache()
    yield
    keystore._reset_cache()


@pytest.fixture
def client():
    with TestClient(app) as c:
        yield c


def _ensure_pool_module(monkeypatch):
    """Make ``search_mcp.browser.pool`` importable & monkeypatchable.

    The browser package is installed by a separate part and may be absent here,
    so stub a minimal package/module tree when needed. Returns the pool module.
    """
    try:
        from search_mcp.browser import pool  # noqa: F401

        from search_mcp.browser import pool as real_pool

        return real_pool
    except Exception:
        pkg_name = "search_mcp.browser"
        mod_name = "search_mcp.browser.pool"
        browser_pkg = types.ModuleType(pkg_name)
        browser_pkg.__path__ = []  # mark as a package
        pool_mod = types.ModuleType(mod_name)
        pool_mod.login = AsyncMock(return_value=True)
        browser_pkg.pool = pool_mod
        monkeypatch.setitem(sys.modules, pkg_name, browser_pkg)
        monkeypatch.setitem(sys.modules, mod_name, pool_mod)
        return pool_mod


# --- Network / Proxy card ---------------------------------------------------


def test_index_renders_proxy_card(client):
    body = client.get("/").text
    assert "Proxy" in body
    # The proxy input is wired with data-key="proxy" so /api/save persists it.
    assert 'data-key="proxy"' in body
    # And it is a masked (password) input — never a plain text field.
    assert 'type="password" data-key="proxy"' in body


def test_proxy_value_not_echoed_in_page(client):
    keystore.set_secrets({"proxy": "http://user:s3cr3t@10.0.0.1:8080"})
    keystore._reset_cache()
    body = client.get("/").text
    # The stored secret must never be rendered back into the HTML.
    assert "s3cr3t" not in body
    assert "10.0.0.1" not in body
    # The card still flips to the configured badge.
    assert "Proxy" in body
    assert "Configured" in body


def test_save_persists_proxy(client):
    resp = client.post("/api/save", json={"proxy": "http://127.0.0.1:9"})
    assert resp.status_code == 200
    assert resp.json()["ok"] is True
    assert resp.json()["status"]["__network__"] is True

    keystore._reset_cache()
    assert keystore.get_secret("proxy") == "http://127.0.0.1:9"


def test_save_persists_proxy_engines(client):
    resp = client.post("/api/save", json={"proxy_engines": "zhihu google"})
    assert resp.status_code == 200
    assert resp.json()["ok"] is True

    keystore._reset_cache()
    assert keystore.get_secret("proxy_engines") == "zhihu google"


def test_clear_proxy_updates_network_status(client):
    keystore.set_secrets({"proxy": "http://127.0.0.1:9"})
    keystore._reset_cache()

    resp = client.post("/api/clear", json={"field": "proxy"})

    assert resp.status_code == 200
    assert resp.json()["ok"] is True
    assert resp.json()["status"]["__network__"] is False


# --- zhihu browser login ----------------------------------------------------


def test_login_zhihu_ok(client, monkeypatch):
    pool_mod = _ensure_pool_module(monkeypatch)
    login_mock = AsyncMock(return_value=True)
    monkeypatch.setattr(pool_mod, "login", login_mock)

    resp = client.post("/api/login/zhihu")
    assert resp.status_code == 200
    assert resp.json() == {"ok": True, "error": None}
    login_mock.assert_awaited_once_with("https://www.zhihu.com")


def test_login_unknown_provider(client):
    resp = client.post("/api/login/nope")
    assert resp.status_code == 404
    assert resp.json()["ok"] is False
