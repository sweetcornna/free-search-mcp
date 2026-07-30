"""Each pytest-asyncio test gets a fresh event loop, but the BrowserPool
caches a Playwright instance bound to the loop where it was first created.
Shutting it down between tests keeps the pool from carrying a dead loop into
the next test.
"""
import os
import socket

import pytest


@pytest.fixture(autouse=True)
async def _reset_browser_pool():
    yield
    from search_mcp.browser import pool
    await pool.shutdown()


@pytest.fixture(autouse=True)
async def _close_global_cache():
    """Cache maintenance is fire-and-forget; await/cancel it via close() so a
    pending task never outlives the test's event loop (aiosqlite's worker
    would then warn about call_soon_threadsafe on a closed loop)."""
    yield
    from search_mcp.cache import cache
    await cache.close()


@pytest.fixture(autouse=True)
def _hermetic_config(tmp_path_factory, monkeypatch):
    """Assert against the SHIPPED defaults, not the developer's machine.

    ``config.load_all_env_files()`` deliberately merges ``./.env`` and
    ``<config_dir>/.env`` into ``os.environ`` at import time, which is right at
    runtime and wrong for a test suite: a single
    ``SEARCH_MCP_ALLOW_PRIVATE_HOSTS=true`` in a personal config file disarms
    the guard under test, so 26 SSRF cases fail on that machine and pass
    everywhere else. A suite whose result depends on who runs it cannot be used
    to review a change to the thing it covers.

    The proxy matters for the same reason now that the guard consults it: a
    developer with ``SEARCH_MCP_PROXY`` set would skip the resolve-and-check
    layer everywhere and never know.
    """
    monkeypatch.setenv("SEARCH_MCP_CONFIG_DIR", str(tmp_path_factory.mktemp("cfg")))
    for var in (
        "SEARCH_MCP_ALLOW_PRIVATE_HOSTS",
        "SEARCH_MCP_SSRF_RESOLVE_ADDRESSES",
        "SEARCH_MCP_PROXY",
        "SEARCH_MCP_PROXY_ENGINES",
        "SEARCH_MCP_DOCUMENT_ROOT",
        "SEARCH_MCP_DOWNLOAD_DIR",
    ):
        monkeypatch.delenv(var, raising=False)

    from search_mcp import keystore
    from search_mcp.config import Settings, settings

    keystore._reset_cache()
    # `settings` is a module-level singleton built at import time, so clearing
    # the env is not enough — reset the fields that were already read from it.
    for name in ("allow_private_hosts", "ssrf_resolve_addresses",
                 "document_root", "download_dir"):
        monkeypatch.setattr(settings, name, Settings.model_fields[name].default)

    # The fake-IP verdict is memoised per process; tests re-stub the resolver,
    # so a verdict from one test must not decide the next.
    from search_mcp.url_safety import reset_resolver_detection
    reset_resolver_detection()
    yield
    reset_resolver_detection()
    keystore._reset_cache()


@pytest.fixture(autouse=True)
def _hermetic_dns(monkeypatch):
    """Offline suite must never depend on the machine's real resolver: some
    environments (the CC sandbox, DNS-filtering VPNs) resolve public hosts to
    the reserved 198.18.x range, which the SSRF guard correctly blocks and
    would make DNS-touching tests flake. Every hostname resolves to a fixed
    public IP; tests that need specific resolutions monkeypatch over this.
    Live runs (SEARCH_MCP_TEST_NETWORK=1) keep the real resolver."""
    if os.environ.get("SEARCH_MCP_TEST_NETWORK"):
        yield
        return

    def _resolver(host, port, *args, **kwargs):
        return [
            (
                socket.AF_INET,
                socket.SOCK_STREAM,
                socket.IPPROTO_TCP,
                "",
                ("93.184.216.34", port or 0),
            )
        ]

    monkeypatch.setattr(socket, "getaddrinfo", _resolver)
    yield


@pytest.fixture(autouse=True)
def _clear_dns_ok_cache():
    """The SSRF guard memoizes successful (host, port) validations for a short
    TTL. Tests re-stub the resolver per test, so a memo carried across tests
    would leak the previous stub's verdict into the next test."""
    from search_mcp.url_safety import clear_dns_cache
    clear_dns_cache()
    yield
    clear_dns_cache()


@pytest.fixture(autouse=True)
def _disable_rescue(monkeypatch):
    """The offline suite must never hit the network. The aggregation-level
    rescue (searx/bing) fires whenever a test stubs an engine into returning
    nothing, which would send REAL requests from an otherwise-offline test.
    test_rescue.py re-enables it against a fully stubbed engine registry."""
    from search_mcp.config import settings
    monkeypatch.setattr(settings, "rescue_enabled", False)
