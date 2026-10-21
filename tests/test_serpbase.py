"""SerpBaseEngine tests.

The engine hits a JSON POST REST API (https://api.serpbase.dev/google/search)
behind an API key, so unit tests mock both the keystore (a fake key) and the
AsyncSession HTTP path rather than touch the network. A single live-network
test is gated on ``SEARCH_MCP_TEST_NETWORK=1`` (and needs a real key).

The API's quirk is the reason half of this file exists: it answers **HTTP 200**
for failures, and success is the envelope's ``status == 0``. A bad key is a 200
whose body says ``{"status": 1001}``, so the tests pin that mapping — otherwise
every search would come back empty with no explanation.
"""
from __future__ import annotations

import os
from unittest.mock import AsyncMock, MagicMock

import pytest

from search_mcp.engines.base import EngineKeyError, SearchFilters
from search_mcp.engines.serpbase import SerpBaseEngine

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.

NETWORK = os.environ.get("SEARCH_MCP_TEST_NETWORK") == "1"
skip_offline = pytest.mark.skipif(
    not NETWORK, reason="set SEARCH_MCP_TEST_NETWORK=1 to run"
)


# A representative success envelope. The second item carries a <strong>-
# highlighted term that must be stripped; the third a "Second" keyword used by
# the exclude_text filter test.
_OK_PAYLOAD = {
    "status": 0,
    "request_id": "req_abc123",
    "elapsed_ms": 812,
    "credits_charged": 1,
    "organic": [
        {
            "title": "Result A",
            "link": "https://example.com/a",
            "snippet": "first snippet body",
            "position": 1,
        },
        {
            "title": "Result <strong>B</strong>",
            "link": "https://example.com/b",
            "snippet": "a highlighted <em>result</em>",
            "position": 2,
        },
        {
            "title": "Result C with Second keyword",
            "link": "https://example.com/c",
            "snippet": "third body",
            "position": 3,
        },
    ],
}


# ---------------------------------------------------------------------------
# Wiring
# ---------------------------------------------------------------------------


def test_engine_basic_attributes():
    e = SerpBaseEngine()
    assert e.name == "serpbase"
    assert e.needs_browser is False
    # JSON API: don't waste a Playwright re-render on an empty/malformed body.
    assert e.supports_browser_fallback is False


def test_build_url_is_stable_endpoint():
    e = SerpBaseEngine()
    url = e.build_url("hello world", 10)
    assert url == "https://api.serpbase.dev/google/search"
    # Same regardless of query/filters — the body carries those, and the key
    # never appears in the cache key.
    assert e.build_url("other", 5, SearchFilters(freshness="week")) == url


def test_parse_never_raises_and_returns_empty():
    # parse() is unused on the POST path but the ABC requires it; it must
    # never raise and always return [].
    e = SerpBaseEngine()
    assert e.parse("") == []
    assert e.parse("<html>garbage</html>") == []
    assert e.parse("not json at all {{{") == []


# ---------------------------------------------------------------------------
# search() — mocked keystore + HTTP/JSON layer
# ---------------------------------------------------------------------------


def _mock_session_returning(status: int, payload, text: str = ""):
    """Build an AsyncSession-shaped context manager whose .post() returns one
    response. ``payload`` may be a value (used by .json()) or an Exception
    instance to simulate malformed JSON (json() then raises it)."""
    response = MagicMock()
    response.status_code = status
    response.text = text
    if isinstance(payload, Exception):
        response.json = MagicMock(side_effect=payload)
    else:
        response.json = MagicMock(return_value=payload)

    session = MagicMock()
    session.post = AsyncMock(return_value=response)
    cm = MagicMock()
    cm.__aenter__ = AsyncMock(return_value=session)
    cm.__aexit__ = AsyncMock(return_value=None)
    return cm


def _patch_session(monkeypatch, cm_factory):
    monkeypatch.setattr("search_mcp.engines.serpbase.AsyncSession", cm_factory)


def _patch_key(monkeypatch, key: str | None = "FAKE"):
    monkeypatch.setattr("search_mcp.engines.serpbase.get_secret", lambda f: key)


async def test_search_parses_results(monkeypatch):
    e = SerpBaseEngine()
    _patch_key(monkeypatch)
    _patch_session(
        monkeypatch, lambda *a, **kw: _mock_session_returning(200, _OK_PAYLOAD)
    )

    out = await e.search("hello", max_results=10)
    assert len(out) == 3

    a, b, _c = out
    assert a.title == "Result A"
    assert a.url == "https://example.com/a"
    assert a.snippet == "first snippet body"
    # This product path carries no date field.
    assert a.published_age == ""

    # <strong>/<em> highlight tags stripped from title and snippet.
    assert b.title == "Result B"
    assert b.snippet == "a highlighted result"


async def test_search_sets_rank_and_engine(monkeypatch):
    e = SerpBaseEngine()
    _patch_key(monkeypatch)
    _patch_session(
        monkeypatch, lambda *a, **kw: _mock_session_returning(200, _OK_PAYLOAD)
    )

    out = await e.search("hello", max_results=10)
    assert [r.rank for r in out] == [1, 2, 3]
    assert all(r.engine == "serpbase" for r in out)


async def test_search_missing_key_raises(monkeypatch):
    e = SerpBaseEngine()
    _patch_key(monkeypatch, None)
    # Even with a working HTTP layer, a missing key must raise before any call.
    _patch_session(
        monkeypatch, lambda *a, **kw: _mock_session_returning(200, _OK_PAYLOAD)
    )

    with pytest.raises(ValueError) as exc:
        await e.search("hello", max_results=10)
    msg = str(exc.value)
    # Actionable hint names the field, the env var, and does not ask the user.
    assert "serpbase not configured" in msg
    assert "serpbase_api_key" in msg
    assert "SEARCH_MCP_SERPBASE_API_KEY" in msg
    assert "Do not ask the user for an API key" in msg


# --- the envelope: HTTP 200 is not success ---------------------------------


async def test_search_maps_1001_envelope_to_key_error(monkeypatch):
    # A rejected key arrives as HTTP 200 + {"status": 1001}. Before this
    # mapping the engine read it as an empty result set, silently.
    e = SerpBaseEngine()
    _patch_key(monkeypatch)
    _patch_session(
        monkeypatch,
        lambda *a, **kw: _mock_session_returning(
            200, {"status": 1001, "request_id": "req_x", "error": "unauthorized"}
        ),
    )

    with pytest.raises(EngineKeyError, match="status 1001"):
        await e.search("hello", max_results=10)


async def test_search_maps_1020_envelope_to_credits_error(monkeypatch):
    e = SerpBaseEngine()
    _patch_key(monkeypatch)
    _patch_session(
        monkeypatch,
        lambda *a, **kw: _mock_session_returning(
            200, {"status": 1020, "error": "insufficient credits"}
        ),
    )

    with pytest.raises(EngineKeyError, match="out of credits"):
        await e.search("hello", max_results=10)


async def test_search_maps_1029_envelope_to_rate_limit_error(monkeypatch):
    e = SerpBaseEngine()
    _patch_key(monkeypatch)
    _patch_session(
        monkeypatch,
        lambda *a, **kw: _mock_session_returning(200, {"status": 1029}),
    )

    with pytest.raises(EngineKeyError, match="rate limit"):
        await e.search("hello", max_results=10)


async def test_search_maps_1003_envelope_to_forbidden_error(monkeypatch):
    e = SerpBaseEngine()
    _patch_key(monkeypatch)
    _patch_session(
        monkeypatch,
        lambda *a, **kw: _mock_session_returning(200, {"status": 1003}),
    )

    with pytest.raises(EngineKeyError, match="status 1003"):
        await e.search("hello", max_results=10)


@pytest.mark.parametrize("code", [1000, 1500, 1502, 1503, 1504])
async def test_search_transient_envelope_returns_empty(monkeypatch, code):
    # Invalid-request and transient upstream codes are not auth problems: they
    # degrade to empty so one flaky endpoint can't poison the aggregator.
    e = SerpBaseEngine()
    _patch_key(monkeypatch)
    _patch_session(
        monkeypatch,
        lambda *a, **kw: _mock_session_returning(200, {"status": code}),
    )

    assert await e.search("hello", max_results=10) == []


async def test_search_rejected_key_raises(monkeypatch):
    # If the gateway ever does use a real HTTP status, it is still mapped.
    e = SerpBaseEngine()
    _patch_key(monkeypatch)
    _patch_session(
        monkeypatch,
        lambda *a, **kw: _mock_session_returning(403, None, text="Forbidden"),
    )

    with pytest.raises(ValueError, match="rejected"):
        await e.search("hello", max_results=10)


async def test_search_server_error_returns_empty(monkeypatch):
    # A non-auth non-200 (e.g. 500) still degrades to empty (transient), no raise.
    e = SerpBaseEngine()
    _patch_key(monkeypatch)
    _patch_session(
        monkeypatch,
        lambda *a, **kw: _mock_session_returning(500, None, text="Server Error"),
    )

    assert await e.search("hello", max_results=10) == []


async def test_search_malformed_json_returns_empty(monkeypatch):
    e = SerpBaseEngine()
    _patch_key(monkeypatch)
    _patch_session(
        monkeypatch,
        lambda *a, **kw: _mock_session_returning(200, ValueError("bad json"), text="<<<"),
    )

    assert await e.search("hello", max_results=10) == []


async def test_search_request_exception_returns_empty(monkeypatch):
    e = SerpBaseEngine()
    _patch_key(monkeypatch)

    def factory(*a, **kw):
        raise RuntimeError("boom")

    _patch_session(monkeypatch, factory)
    assert await e.search("hello", max_results=10) == []


# --- request shape ---------------------------------------------------------


async def test_search_posts_with_header_auth_and_body(monkeypatch):
    e = SerpBaseEngine()
    _patch_key(monkeypatch)
    captured: dict = {}

    def factory(*a, **kw):
        captured["headers"] = kw.get("headers")
        cm = MagicMock()

        async def _post(url, json=None, **_):
            captured["method"] = "POST"
            captured["url"] = url
            captured["json"] = json
            resp = MagicMock()
            resp.status_code = 200
            resp.json = MagicMock(return_value={"status": 0, "organic": []})
            resp.text = ""
            return resp

        async def _aenter(*_a, **_kw):
            session = MagicMock()
            session.post = AsyncMock(side_effect=_post)
            return session

        cm.__aenter__ = AsyncMock(side_effect=_aenter)
        cm.__aexit__ = AsyncMock(return_value=None)
        return cm

    _patch_session(monkeypatch, factory)

    await e.search("hello", max_results=9999)
    # num clamped into the API's page-size band, never in the URL.
    assert captured["json"]["num"] == 100
    assert captured["json"]["q"] == "hello"
    assert captured["json"]["gl"] == "us"
    assert captured["json"]["hl"] == "en"
    assert captured["url"] == "https://api.serpbase.dev/google/search"
    assert captured["method"] == "POST"
    # The key travels in X-API-Key; query-param keys are rejected by the API.
    assert captured["headers"]["X-API-Key"] == "FAKE"
    assert captured["headers"]["Content-Type"] == "application/json"


def test_search_never_puts_the_key_in_the_url(monkeypatch):
    e = SerpBaseEngine()
    url = e.build_url("hello", 10)
    assert "api_key" not in url
    assert "FAKE" not in url


# --- result handling -------------------------------------------------------


async def test_search_exclude_text_filter_drops_result(monkeypatch):
    e = SerpBaseEngine()
    _patch_key(monkeypatch)
    _patch_session(
        monkeypatch, lambda *a, **kw: _mock_session_returning(200, _OK_PAYLOAD)
    )

    out = await e.search(
        "hello", max_results=10, filters=SearchFilters(exclude_text="Second")
    )
    urls = {r.url for r in out}
    # "Second keyword" lives in Result C's title -> dropped.
    assert "https://example.com/c" not in urls
    assert "https://example.com/a" in urls
    # Ranks recomputed after the drop.
    assert [r.rank for r in out] == [1, 2]


async def test_search_truncates_to_max_results(monkeypatch):
    e = SerpBaseEngine()
    _patch_key(monkeypatch)
    _patch_session(
        monkeypatch, lambda *a, **kw: _mock_session_returning(200, _OK_PAYLOAD)
    )

    out = await e.search("hello", max_results=1)
    assert len(out) == 1
    assert out[0].rank == 1


async def test_search_empty_organic_list(monkeypatch):
    e = SerpBaseEngine()
    _patch_key(monkeypatch)
    _patch_session(
        monkeypatch,
        lambda *a, **kw: _mock_session_returning(200, {"status": 0, "organic": []}),
    )
    assert await e.search("hello", max_results=10) == []


async def test_search_missing_organic_key(monkeypatch):
    e = SerpBaseEngine()
    _patch_key(monkeypatch)
    # Success envelope with no organic key at all -> empty, never raises.
    _patch_session(
        monkeypatch,
        lambda *a, **kw: _mock_session_returning(200, {"status": 0}),
    )
    assert await e.search("hello", max_results=10) == []


async def test_search_skips_items_missing_title_or_url(monkeypatch):
    e = SerpBaseEngine()
    _patch_key(monkeypatch)
    payload = {
        "status": 0,
        "organic": [
            {"title": "", "link": "https://example.com/no-title"},
            {"title": "No URL", "link": ""},
            {"title": "Good", "link": "https://example.com/good", "snippet": "body"},
        ],
    }
    _patch_session(monkeypatch, lambda *a, **kw: _mock_session_returning(200, payload))

    out = await e.search("hello", max_results=10)
    assert [r.url for r in out] == ["https://example.com/good"]


# ---------------------------------------------------------------------------
# Live network test
# ---------------------------------------------------------------------------


@skip_offline
async def test_live_serpbase_returns_results():
    # An opt-in engine. The live suite has to pass on a machine with no keys at
    # all — that is the configuration the project promises works.
    from search_mcp.keystore import get_secret

    if not get_secret("serpbase_api_key"):
        pytest.skip("opt-in engine: no serpbase_api_key configured")

    e = SerpBaseEngine()
    try:
        out = await e.search("python language", 10)
    except EngineKeyError as exc:
        # The key on this machine, not the adapter: SerpBase refused it.
        pytest.skip(f"the configured serpbase_api_key does not work: {str(exc)[:90]}")
    if not out:
        pytest.skip("SerpBase endpoint unreachable, unkeyed, or returned nothing")
    assert out[0].url.startswith("http")
    assert all(r.engine == "serpbase" for r in out)
