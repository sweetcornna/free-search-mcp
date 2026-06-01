"""Offline unit tests for browser.py proxy + transient-retry pure helpers.

No real browser / Playwright is started here: we only exercise the small pure
helpers (`_launch_proxy_kwargs`, `_is_transient_nav_error`) that were factored
out precisely so the proxy and retry logic can be tested without launching
Chromium.
"""
from __future__ import annotations

import pytest

from search_mcp import browser, net


# --- _launch_proxy_kwargs ----------------------------------------------------


def test_launch_proxy_kwargs_includes_proxy_when_configured(monkeypatch):
    proxy = {"server": "http://h:1"}
    monkeypatch.setattr(net, "playwright_proxy", lambda: proxy)
    # browser.py imports the symbol by name (`from .net import playwright_proxy`),
    # so patch the binding the module actually calls.
    monkeypatch.setattr(browser, "playwright_proxy", lambda: proxy)

    kwargs = browser._launch_proxy_kwargs()
    assert kwargs == {"proxy": proxy}


def test_launch_proxy_kwargs_empty_when_no_proxy(monkeypatch):
    monkeypatch.setattr(browser, "playwright_proxy", lambda: None)
    kwargs = browser._launch_proxy_kwargs()
    assert kwargs == {}
    assert "proxy" not in kwargs  # never pass proxy=None into the launch


# --- _is_transient_nav_error -------------------------------------------------


@pytest.mark.parametrize(
    "marker",
    [
        "ERR_TUNNEL_CONNECTION_FAILED",
        "ERR_TIMED_OUT",
        "ERR_CONNECTION_RESET",
        "ERR_CONNECTION_CLOSED",
        "ERR_NETWORK_CHANGED",
    ],
)
def test_is_transient_nav_error_true_for_markers(marker):
    msg = f"page.goto: net::{marker} at https://example.com/"
    assert browser._is_transient_nav_error(msg) is True


@pytest.mark.parametrize(
    "msg",
    [
        "",
        "net::ERR_NAME_NOT_RESOLVED at https://example.com/",
        "Timeout 30000ms exceeded waiting for selector",
        "some unrelated error string",
    ],
)
def test_is_transient_nav_error_false_otherwise(msg):
    assert browser._is_transient_nav_error(msg) is False
