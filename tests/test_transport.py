"""Transport selection and the HTTP DNS-rebinding guard.

These stay offline on purpose: `conftest._hermetic_dns` redirects every
name lookup, so binding a real port here would fight the suite's own
hermeticity. The live HTTP path is covered by starting the server manually
(see README); what is pinned here is the logic this repo owns — which
transport gets selected, and which hosts/origins the guard admits.
"""

from __future__ import annotations

import pytest

from search_mcp.config import settings
from search_mcp.server import _http_security, run

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.


@pytest.fixture
def captured_run(monkeypatch):
    """Replace MCPServer.run so `run()` can be exercised without a server."""
    from search_mcp import server as server_mod

    calls: list[dict] = []
    monkeypatch.setattr(server_mod.mcp, "run", lambda **kw: calls.append(kw))

    # `run()`'s finally block tears down the browser pool and the cache. Stub
    # both — they are coroutine functions, and conftest's autouse fixtures
    # await the real ones, so a sync stub would break teardown for the whole
    # session rather than just this test.
    async def _noop() -> None:
        return None

    monkeypatch.setattr(server_mod.pool, "shutdown", _noop)
    monkeypatch.setattr(server_mod.cache, "close", _noop)
    return calls


# ---------------------------------------------------------------------------
# Transport selection
# ---------------------------------------------------------------------------


def test_defaults_to_stdio(captured_run):
    run()
    assert captured_run == [{}], "stdio must stay the zero-config default"


def test_cli_argument_selects_streamable_http(captured_run):
    run(transport="streamable-http", host="127.0.0.1", port=9123, path="/mcp")
    (kwargs,) = captured_run
    assert kwargs["transport"] == "streamable-http"
    assert kwargs["host"] == "127.0.0.1"
    assert kwargs["port"] == 9123
    assert kwargs["streamable_http_path"] == "/mcp"


def test_env_setting_selects_transport_when_cli_is_silent(captured_run, monkeypatch):
    monkeypatch.setattr(settings, "transport", "streamable-http")
    monkeypatch.setattr(settings, "http_port", 4321)
    run()
    (kwargs,) = captured_run
    assert kwargs["transport"] == "streamable-http"
    assert kwargs["port"] == 4321


def test_cli_argument_beats_env_setting(captured_run, monkeypatch):
    monkeypatch.setattr(settings, "http_port", 4321)
    monkeypatch.setattr(settings, "transport", "streamable-http")
    run(port=5555)
    (kwargs,) = captured_run
    assert kwargs["port"] == 5555


def test_port_zero_is_honoured_not_treated_as_missing(captured_run, monkeypatch):
    """`port=0` means "pick a free port" — it must not fall through to the
    default the way a `port or settings.http_port` would."""
    monkeypatch.setattr(settings, "transport", "streamable-http")
    monkeypatch.setattr(settings, "http_port", 8000)
    run(port=0)
    (kwargs,) = captured_run
    assert kwargs["port"] == 0


def test_http_transport_always_installs_the_security_guard(captured_run):
    run(transport="streamable-http")
    (kwargs,) = captured_run
    assert kwargs["transport_security"].enable_dns_rebinding_protection is True


# ---------------------------------------------------------------------------
# DNS-rebinding guard
# ---------------------------------------------------------------------------


def test_guard_admits_every_loopback_spelling():
    s = _http_security("127.0.0.1", 8000)
    assert "127.0.0.1:8000" in s.allowed_hosts
    assert "localhost:8000" in s.allowed_hosts
    assert "[::1]:8000" in s.allowed_hosts
    assert "http://localhost:8000" in s.allowed_origins


def test_guard_rejects_unlisted_origins():
    s = _http_security("127.0.0.1", 8000)
    assert "http://evil.example.com" not in s.allowed_origins
    # A different port on the same host is a different origin.
    assert "http://127.0.0.1:9999" not in s.allowed_origins


def test_guard_admits_an_explicit_bind_host():
    s = _http_security("search.internal", 8080)
    assert "search.internal:8080" in s.allowed_hosts
    assert "http://search.internal:8080" in s.allowed_origins


def test_wildcard_bind_does_not_become_an_allowed_host():
    """Binding 0.0.0.0 means "all interfaces", not "any Host header is fine"."""
    s = _http_security("0.0.0.0", 8000)  # noqa: S104 - asserting on config, not binding
    assert "0.0.0.0:8000" not in s.allowed_hosts
    assert s.allowed_hosts == ["127.0.0.1:8000", "localhost:8000", "[::1]:8000"]


@pytest.mark.parametrize("configured", ["https://a.example https://b.example",
                                        "https://a.example,https://b.example"])
def test_extra_origins_accept_space_or_comma_separation(monkeypatch, configured):
    monkeypatch.setattr(settings, "http_allowed_origins", configured)
    s = _http_security("127.0.0.1", 8000)
    assert "https://a.example" in s.allowed_origins
    assert "https://b.example" in s.allowed_origins
