"""BraveApiEngine tests (official keyed Brave Search API).

The engine hits a JSON GET REST API
(https://api.search.brave.com/res/v1/web/search) with a subscription token,
so unit tests monkeypatch ``get_secret`` (to inject a fake key) and mock the
AsyncSession HTTP path rather than touch the network. A single live-network
test is gated on ``SEARCH_MCP_TEST_NETWORK=1`` and needs a real key.
"""
from __future__ import annotations

import os
from unittest.mock import AsyncMock, MagicMock

import pytest

from search_mcp.engines.base import SearchFilters
from search_mcp.engines.brave_api import BraveApiEngine

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.

NETWORK = os.environ.get("SEARCH_MCP_TEST_NETWORK") == "1"
skip_offline = pytest.mark.skipif(
    not NETWORK, reason="set SEARCH_MCP_TEST_NETWORK=1 to run"
)


# A representative 200 payload in the Brave web-search shape. The first item
# carries an ISO ``page_age`` (-> YYYY-MM-DD wins) plus <strong> highlight tags
# in both title and description (must be stripped). The second omits page_age
# but has a free-text ``age`` we derive a hint from, and a "Second" keyword in
# its title for the exclude_text filter test.
_OK_PAYLOAD = {
    "web": {
        "results": [
            {
                "title": "Result <strong>A</strong>",
                "url": "https://example.com/a",
                "description": "a <strong>matched</strong> snippet body",
                "page_age": "2024-02-06T00:00:00Z",
                "age": "2 years ago",
            },
            {
                "title": "Result B with Second keyword",
                "url": "https://example.com/b",
                "description": "plain description, no tags",
                "age": "2026-05-15",
            },
        ]
    }
}

# Payload missing the "web" envelope entirely -> genuinely empty.
_NO_WEB_PAYLOAD = {"query": {"original": "hello"}}


# ---------------------------------------------------------------------------
# Wiring
# ---------------------------------------------------------------------------


def test_engine_basic_attributes():
    e = BraveApiEngine()
    assert e.name == "brave_api"
    assert e.needs_browser is False
    # JSON API: don't waste a Playwright re-render on an empty/malformed body.
    assert e.supports_browser_fallback is False


def test_build_url_is_stable_endpoint_with_query():
    e = BraveApiEngine()
    url = e.build_url("hello world", 10)
    assert url.startswith("https://api.search.brave.com/res/v1/web/search?q=")
    assert "hello" in url
    # No secret ever appears in the cache key.
    assert "brave_api_key" not in url.lower()
    assert "subscription" not in url.lower()


def test_parse_never_raises_and_returns_empty():
    # parse() is unused on the GET path but the ABC requires it; it must never
    # raise and always return [].
    e = BraveApiEngine()
    assert e.parse("") == []
    assert e.parse("<html>garbage</html>") == []
    assert e.parse("not json at all {{{") == []


# ---------------------------------------------------------------------------
# search() — mocked HTTP/JSON layer
# ---------------------------------------------------------------------------


def _mock_session_returning(status: int, payload, text: str = "", captured: dict | None = None):
    """Build an AsyncSession-shaped context manager whose .get() returns one
    response. ``payload`` may be a value (used by .json()) or an Exception
    instance to simulate malformed JSON (json() then raises it). When
    ``captured`` is given, the request kwargs (params/headers) are recorded."""
    response = MagicMock()
    response.status_code = status
    response.text = text
    if isinstance(payload, Exception):
        response.json = MagicMock(side_effect=payload)
    else:
        response.json = MagicMock(return_value=payload)

    async def _get(url, **kwargs):
        if captured is not None:
            captured["url"] = url
            captured.update(kwargs)
        return response

    session = MagicMock()
    session.get = AsyncMock(side_effect=_get)
    cm = MagicMock()
    cm.__aenter__ = AsyncMock(return_value=session)
    cm.__aexit__ = AsyncMock(return_value=None)
    if captured is not None:
        # Record the AsyncSession(**kwargs) the engine constructs, so the test
        # can assert the auth header is present.
        captured["_session_factory_called"] = True
    return cm


def _patch_key(monkeypatch, value: str | None):
    monkeypatch.setattr(
        "search_mcp.engines.brave_api.get_secret", lambda field: value
    )


def _patch_session(monkeypatch, cm_factory):
    monkeypatch.setattr("search_mcp.engines.brave_api.AsyncSession", cm_factory)


async def test_search_parses_results(monkeypatch):
    e = BraveApiEngine()
    _patch_key(monkeypatch, "FAKE")
    _patch_session(
        monkeypatch, lambda *a, **kw: _mock_session_returning(200, _OK_PAYLOAD)
    )

    out = await e.search("hello", max_results=10)
    assert len(out) == 2

    a, b = out
    # <strong> tags stripped from title + snippet.
    assert a.title == "Result A"
    assert a.url == "https://example.com/a"
    assert a.snippet == "a matched snippet body"
    # page_age ISO -> YYYY-MM-DD wins over the "age" free-text.
    assert a.published_age == "2024-02-06"

    assert b.title == "Result B with Second keyword"
    assert b.snippet == "plain description, no tags"
    # No page_age -> derived from the free-text "age" ISO date hint.
    assert b.published_age == "2026-05-15"


async def test_search_sets_rank_and_engine(monkeypatch):
    e = BraveApiEngine()
    _patch_key(monkeypatch, "FAKE")
    _patch_session(
        monkeypatch, lambda *a, **kw: _mock_session_returning(200, _OK_PAYLOAD)
    )

    out = await e.search("hello", max_results=10)
    assert [r.rank for r in out] == [1, 2]
    assert all(r.engine == "brave_api" for r in out)


async def test_search_sends_auth_header_and_params(monkeypatch):
    e = BraveApiEngine()
    _patch_key(monkeypatch, "SECRET-TOKEN")
    captured: dict = {}

    def factory(*a, **kw):
        # Capture the AsyncSession(**kw) headers the engine constructs.
        captured["session_kwargs"] = kw
        return _mock_session_returning(200, {"web": {"results": []}}, captured=captured)

    _patch_session(monkeypatch, factory)
    await e.search("hello", max_results=9999)

    # Auth token rides in the X-Subscription-Token header (never the URL).
    headers = captured["session_kwargs"]["headers"]
    assert headers["X-Subscription-Token"] == "SECRET-TOKEN"
    assert headers["Accept"] == "application/json"
    # count clamped to the provider max of 20.
    assert captured["params"]["count"] == 20
    assert captured["params"]["q"] == "hello"


async def test_search_missing_key_raises_value_error(monkeypatch):
    e = BraveApiEngine()
    _patch_key(monkeypatch, None)
    # Session must never even be constructed when the key is missing.
    _patch_session(
        monkeypatch,
        lambda *a, **kw: (_ for _ in ()).throw(AssertionError("should not fetch")),
    )

    with pytest.raises(ValueError) as exc:
        await e.search("hello", max_results=10)
    msg = str(exc.value)
    assert "brave_api" in msg
    assert "brave_api_key" in msg


async def test_search_quota_error_raises(monkeypatch):
    # A 429 (quota/rate limit) surfaces an actionable error instead of a silent
    # empty so the caller learns WHY a configured engine returned nothing.
    e = BraveApiEngine()
    _patch_key(monkeypatch, "FAKE")
    _patch_session(
        monkeypatch,
        lambda *a, **kw: _mock_session_returning(429, None, text="Too Many Requests"),
    )

    with pytest.raises(ValueError, match="429"):
        await e.search("hello", max_results=10)


async def test_search_server_error_returns_empty(monkeypatch):
    # A non-auth non-200 (e.g. 500) still degrades to empty (transient), no raise.
    e = BraveApiEngine()
    _patch_key(monkeypatch, "FAKE")
    _patch_session(
        monkeypatch,
        lambda *a, **kw: _mock_session_returning(500, None, text="Server Error"),
    )

    assert await e.search("hello", max_results=10) == []


async def test_search_malformed_json_returns_empty(monkeypatch):
    e = BraveApiEngine()
    _patch_key(monkeypatch, "FAKE")
    _patch_session(
        monkeypatch,
        lambda *a, **kw: _mock_session_returning(200, ValueError("bad json"), text="<<<"),
    )

    out = await e.search("hello", max_results=10)
    assert out == []


async def test_search_missing_web_envelope_returns_empty(monkeypatch):
    e = BraveApiEngine()
    _patch_key(monkeypatch, "FAKE")
    _patch_session(
        monkeypatch, lambda *a, **kw: _mock_session_returning(200, _NO_WEB_PAYLOAD)
    )
    out = await e.search("hello", max_results=10)
    assert out == []


async def test_search_request_exception_returns_empty(monkeypatch):
    e = BraveApiEngine()
    _patch_key(monkeypatch, "FAKE")

    def factory(*a, **kw):
        raise RuntimeError("boom")

    _patch_session(monkeypatch, factory)
    out = await e.search("hello", max_results=10)
    assert out == []


async def test_search_exclude_text_filter_drops_result(monkeypatch):
    e = BraveApiEngine()
    _patch_key(monkeypatch, "FAKE")
    _patch_session(
        monkeypatch, lambda *a, **kw: _mock_session_returning(200, _OK_PAYLOAD)
    )

    out = await e.search(
        "hello",
        max_results=10,
        filters=SearchFilters(exclude_text="Second"),
    )
    urls = {r.url for r in out}
    # "Second keyword" lives in Result B's title -> dropped.
    assert "https://example.com/b" not in urls
    assert "https://example.com/a" in urls
    # Ranks recomputed after the drop.
    assert [r.rank for r in out] == [1]


async def test_search_truncates_to_max_results(monkeypatch):
    e = BraveApiEngine()
    _patch_key(monkeypatch, "FAKE")
    _patch_session(
        monkeypatch, lambda *a, **kw: _mock_session_returning(200, _OK_PAYLOAD)
    )

    out = await e.search("hello", max_results=1)
    assert len(out) == 1
    assert out[0].rank == 1


async def test_search_skips_items_missing_title_or_url(monkeypatch):
    e = BraveApiEngine()
    _patch_key(monkeypatch, "FAKE")
    payload = {
        "web": {
            "results": [
                {"title": "", "url": "https://example.com/no-title"},
                {"title": "No URL", "url": ""},
                {"title": "Good", "url": "https://example.com/good",
                 "description": "body"},
            ]
        }
    }
    _patch_session(monkeypatch, lambda *a, **kw: _mock_session_returning(200, payload))

    out = await e.search("hello", max_results=10)
    assert [r.url for r in out] == ["https://example.com/good"]


# ---------------------------------------------------------------------------
# Live network test (requires a real key)
# ---------------------------------------------------------------------------


@skip_offline
async def test_live_brave_api_returns_results():
    from search_mcp.keystore import get_secret

    if not get_secret("brave_api_key"):
        pytest.skip("set SEARCH_MCP_BRAVE_API_KEY to run the live test")
    e = BraveApiEngine()
    out = await e.search("python language", 5)
    if not out:
        pytest.skip("Brave API unreachable or returned nothing")
    assert out[0].url.startswith("http")
    assert all(r.engine == "brave_api" for r in out)
