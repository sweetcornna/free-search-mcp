"""Offline tests: opt-in proxy reaches the curl_cffi AsyncSession ctor.

We use AnySearchEngine as the representative HTTP/JSON engine (keyless, so no
key wiring is needed). The engine constructs ``AsyncSession(..., **curl_proxy_kwargs(self.name))``;
these tests patch the AsyncSession symbol with a factory that captures its
ctor kwargs, then assert the proxy kwarg is present iff a proxy is configured.

Opt-in invariant:
  * With SEARCH_MCP_PROXY set, the ctor receives ``proxy="http://p:1"``.
  * With NO proxy configured, the ctor receives NO ``proxy`` key at all
    (byte-for-byte unchanged behaviour).

No network is touched: the captured session's .post() yields a canned 200 with
an empty results list.
"""
from __future__ import annotations

from unittest.mock import AsyncMock, MagicMock

import pytest

from search_mcp import keystore
from search_mcp.engines.anysearch import AnySearchEngine

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.


@pytest.fixture
def isolated_config(monkeypatch, tmp_path):
    """Point the keystore at an empty temp config dir + reset its mtime cache,
    so neither a real ~/.config/search-mcp/config.json nor a stale in-process
    cache can leak a proxy value into these tests."""
    monkeypatch.setenv("SEARCH_MCP_CONFIG_DIR", str(tmp_path))
    keystore._reset_cache()
    yield
    keystore._reset_cache()


def _patch_capturing_session(monkeypatch, captured: dict):
    """Patch anysearch.AsyncSession with a factory that records its ctor kwargs
    into ``captured`` and returns a ctx-manager whose .post() yields a canned
    200 with an empty results list."""

    def factory(*args, **kwargs):
        captured.clear()
        captured.update(kwargs)

        response = MagicMock()
        response.status_code = 200
        response.json = MagicMock(return_value={"data": {"results": []}})
        response.text = ""

        session = MagicMock()
        session.post = AsyncMock(return_value=response)

        cm = MagicMock()
        cm.__aenter__ = AsyncMock(return_value=session)
        cm.__aexit__ = AsyncMock(return_value=None)
        return cm

    monkeypatch.setattr("search_mcp.engines.anysearch.AsyncSession", factory)


async def test_proxy_reaches_session_ctor_when_configured(
    monkeypatch, isolated_config
):
    """case A: with SEARCH_MCP_PROXY set, the AsyncSession ctor sees proxy=..."""
    monkeypatch.setenv("SEARCH_MCP_PROXY", "http://p:1")
    captured: dict = {}
    _patch_capturing_session(monkeypatch, captured)

    await AnySearchEngine().search("x", 3)

    assert captured.get("proxy") == "http://p:1"


async def test_no_proxy_key_when_unconfigured(monkeypatch, isolated_config):
    """case B: with NO proxy configured, the ctor gets NO proxy key (opt-in:
    byte-for-byte unchanged)."""
    monkeypatch.delenv("SEARCH_MCP_PROXY", raising=False)
    captured: dict = {}
    _patch_capturing_session(monkeypatch, captured)

    await AnySearchEngine().search("x", 3)

    assert "proxy" not in captured
