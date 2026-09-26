"""Each pytest-asyncio test gets a fresh event loop, but the BrowserPool
caches a Playwright instance bound to the loop where it was first created.
Shutting it down between tests keeps the pool from carrying a dead loop into
the next test.
"""
import os
import socket

import pytest

# Which tools exist is decided when `search_mcp.server` is imported, which is
# before any fixture runs. Pinned here, at conftest import, so a developer with
# an answer backend or a narrowed tool list in the shell (or in ./.env, which a
# real variable outranks) still tests the shipped eleven.
os.environ["SEARCH_MCP_AGENT_BACKEND"] = "off"
os.environ["SEARCH_MCP_TOOLS"] = ""
os.environ["SEARCH_MCP_TOOL_CALL_BUDGET"] = "0"


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
def _restore_search_mcp_env():
    """Undo whatever a test wrote into ``os.environ`` under ``SEARCH_MCP_``.

    ``load_env_file_into_environ`` writes to ``os.environ`` directly, which is
    its job, and ``monkeypatch.delenv(name, raising=False)`` records nothing to
    restore when the variable was absent to begin with. So a test of the loader
    left ``SEARCH_MCP_SERPER_API_KEY=from-dotenv`` behind for the rest of the
    process. Offline that was invisible, because ``_hermetic_config`` clears
    provider keys before every test. In a live run it does not, and the fake
    key reached Serper, which answered 403.
    """
    before = {k: v for k, v in os.environ.items() if k.startswith("SEARCH_MCP_")}
    yield
    for name in [k for k in os.environ if k.startswith("SEARCH_MCP_")]:
        if name not in before:
            del os.environ[name]
    os.environ.update(before)


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
    # A developer signed in to the Codex CLI must not have it linked or read.
    monkeypatch.setenv("CODEX_HOME", str(tmp_path_factory.mktemp("codex-home")))
    for var in (
        "SEARCH_MCP_ALLOW_PRIVATE_HOSTS",
        "SEARCH_MCP_SSRF_RESOLVE_ADDRESSES",
        "SEARCH_MCP_PROXY",
        "SEARCH_MCP_PROXY_ENGINES",
        "SEARCH_MCP_DOCUMENT_ROOT",
        "SEARCH_MCP_CACHE_DIR",
        "SEARCH_MCP_DOWNLOAD_ENABLED",
        "SEARCH_MCP_DOWNLOAD_DIR",
    ):
        monkeypatch.delenv(var, raising=False)
    # The answer agent is off in the shipped defaults. A developer who runs it
    # day to day has these in the shell, and the suite must not inherit a
    # backend, a model endpoint or a narrowed tool list from them.
    for var in [v for v in os.environ if v.startswith("SEARCH_MCP_AGENT_")] + [
        "SEARCH_MCP_TOOLS",
        "SEARCH_MCP_TOOL_CALL_BUDGET",
    ]:
        monkeypatch.delenv(var, raising=False)

    from search_mcp import keystore
    from search_mcp.config import Settings, settings

    # Same argument, for API keys. This project's claim is that everything works
    # with none configured, and a developer with SEARCH_MCP_SERPER_API_KEY in
    # their shell would be running a different product from CI's: the opt-in
    # line of `engines`, the "not configured" errors and the routing of
    # `github_code` all change. The live suite keeps them — that is the only
    # place a real key is meant to be exercised.
    if os.environ.get("SEARCH_MCP_TEST_NETWORK") != "1":
        for provider in keystore.PROVIDERS:
            for provider_field in provider.fields:
                monkeypatch.delenv(keystore._env_name(provider_field.key), raising=False)

    keystore._reset_cache()
    # `settings` is a module-level singleton built at import time, so clearing
    # the env is not enough — reset the fields that were already read from it.
    for name in (
        "allow_private_hosts",
        "ssrf_resolve_addresses",
        "document_root",
        "download_enabled",
        "download_dir",
        "tools",
        "tool_call_budget",
        *(f for f in Settings.model_fields if f.startswith("agent_")),
    ):
        monkeypatch.setattr(settings, name, Settings.model_fields[name].default)
    monkeypatch.setattr(
        settings, "cache_dir", tmp_path_factory.mktemp("download-cache")
    )
    # The one shipped default the suite does not run with: naming `codex`
    # unsigned would open a real browser on a developer's desktop. The tests
    # of that path switch it back on against a fake browser.
    monkeypatch.setattr(settings, "codex_auto_signin", False)
    # The singleton captures its database path when modules are imported during
    # collection, before this fixture can replace cache_dir. Point it at the
    # per-test cache root too so no test attempts to write the developer's
    # configured cache path (which may also be read-only in CI sandboxes).
    from search_mcp.cache import cache

    monkeypatch.setattr(cache, "_path", str(settings.cache_path()))

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


@pytest.fixture(autouse=True)
def _reset_bing_session():
    """The Bing engine shares one warmed cookie jar per proxy egress for half
    an hour. Carried across tests it would let one test's stubbed session
    answer for the next, and hide whether a test mints one at all."""
    from search_mcp.engines.bing import _reset_jar
    _reset_jar()
    yield
    _reset_jar()


@pytest.fixture(autouse=True)
def _reset_engine_health():
    """The circuit breaker is process-wide on purpose. Across tests that means
    one test's stubbed captcha would bench `searx` for every test after it."""
    from search_mcp.health import engine_health
    engine_health.reset()
    yield
    engine_health.reset()
