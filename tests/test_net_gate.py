"""Tests for the proxy helpers (net.py) + gate detection (base.detect_gate).

Offline; SEARCH_MCP_CONFIG_DIR points at a tmp dir so the proxy (read via
keystore) never touches the real config.
"""
from __future__ import annotations

import pytest

from search_mcp import keystore, net
from search_mcp.engines.base import detect_gate


@pytest.fixture(autouse=True)
def _tmp_config(tmp_path, monkeypatch):
    monkeypatch.setenv("SEARCH_MCP_CONFIG_DIR", str(tmp_path))
    monkeypatch.delenv("SEARCH_MCP_PROXY", raising=False)
    monkeypatch.delenv("SEARCH_MCP_PROXY_ENGINES", raising=False)
    keystore._reset_cache()
    yield
    keystore._reset_cache()


# --- proxy ------------------------------------------------------------------


def test_no_proxy_by_default():
    assert net.proxy_url() is None
    assert net.proxy_for("google") is None
    assert net.curl_proxy_kwargs("google") == {}
    assert net.playwright_proxy() is None


def test_proxy_from_keystore():
    keystore.set_secrets({"proxy": "http://127.0.0.1:8888"})
    assert net.proxy_url() == "http://127.0.0.1:8888"
    assert net.curl_proxy_kwargs() == {"proxy": "http://127.0.0.1:8888"}


def test_env_overrides_keystore_proxy(monkeypatch):
    keystore.set_secrets({"proxy": "http://from-file:1"})
    monkeypatch.setenv("SEARCH_MCP_PROXY", "http://from-env:2")
    assert net.proxy_url() == "http://from-env:2"


def test_proxy_scope_limits_to_listed_engines():
    keystore.set_secrets({"proxy": "http://p:1", "proxy_engines": "google, bing zhihu"})
    assert net.proxy_for("google") == "http://p:1"
    assert net.proxy_for("bing") == "http://p:1"
    assert net.proxy_for("mojeek") is None          # not in scope
    assert net.proxy_for(None) == "http://p:1"       # global (browser/fetch) still proxied
    assert net.curl_proxy_kwargs("mojeek") == {}


def test_playwright_proxy_parses_credentials():
    keystore.set_secrets({"proxy": "http://user:pass@host.example:3128"})
    pp = net.playwright_proxy()
    assert pp == {
        "server": "http://host.example:3128",
        "username": "user",
        "password": "pass",
    }


def test_playwright_proxy_socks_no_creds():
    keystore.set_secrets({"proxy": "socks5://10.0.0.1:1080"})
    assert net.playwright_proxy() == {"server": "socks5://10.0.0.1:1080"}


# --- gate detection ----------------------------------------------------------


def test_detect_gate_none_on_empty_and_normal():
    assert detect_gate("") is None
    assert detect_gate("<html><body><div class='g'>real result</div></body></html>") is None


@pytest.mark.parametrize(
    "html,expected",
    [
        ("<html>… /sorry/index?continue=… unusual traffic …</html>", "captcha"),
        ("<form action='/recaptcha/api'>g-recaptcha</form>", "captcha"),
        ("<a href='https://consent.google.com/...'>before you continue</a>", "consent"),
        ("<div class='SignFlow'>请登录知乎</div>", "login"),
    ],
)
def test_detect_gate_classifies(html, expected):
    assert detect_gate(html) == expected


def test_detect_gate_priority_captcha_over_login():
    # A page with both markers reports the higher-priority one (captcha).
    html = "unusual traffic … please log in to continue"
    assert detect_gate(html) == "captcha"
