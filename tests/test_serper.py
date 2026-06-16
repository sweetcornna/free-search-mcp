"""SerperEngine tests.

The engine hits a JSON POST REST API (https://google.serper.dev/search) behind
an API key, so unit tests mock both the keystore (a fake key) and the
AsyncSession HTTP path rather than touch the network. A single live-network
test is gated on ``SEARCH_MCP_TEST_NETWORK=1`` (and needs a real key).
"""
from __future__ import annotations

import os
from unittest.mock import AsyncMock, MagicMock

import pytest

from search_mcp.engines.base import SearchFilters
from search_mcp.engines.serper import SerperEngine

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.

NETWORK = os.environ.get("SEARCH_MCP_TEST_NETWORK") == "1"
skip_offline = pytest.mark.skipif(
    not NETWORK, reason="set SEARCH_MCP_TEST_NETWORK=1 to run"
)


# A representative 200 payload in Serper's live shape. The second item carries
# a free-text ``date`` ("Jan 5, 2024") that extract_date_hint normalises, and a
# <strong>-highlighted term that must be stripped. The third holds a "Second"
# keyword in its title (used by the exclude_text filter test).
_OK_PAYLOAD = {
    "searchParameters": {"q": "hello", "type": "search"},
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
            "snippet": "a dated <em>result</em>",
            "date": "Jan 5, 2024",
            "position": 2,
        },
        {
            "title": "Result C with Second keyword",
            "link": "https://example.com/c",
            "snippet": "third body",
            "date": "2 days ago",
            "position": 3,
        },
    ],
}


# ---------------------------------------------------------------------------
# Wiring
# ---------------------------------------------------------------------------


def test_engine_basic_attributes():
    e = SerperEngine()
    assert e.name == "serper"
    assert e.needs_browser is False
    # JSON API: don't waste a Playwright re-render on an empty/malformed body.
    assert e.supports_browser_fallback is False


def test_build_url_is_stable_endpoint():
    e = SerperEngine()
    url = e.build_url("hello world", 10)
    assert url == "https://google.serper.dev/search"
    # Same regardless of query/filters — the body carries those, and the key
    # never appears in the cache key.
    assert e.build_url("other", 5, SearchFilters(freshness="week")) == url


def test_parse_never_raises_and_returns_empty():
    # parse() is unused on the POST path but the ABC requires it; it must
    # never raise and always return [].
    e = SerperEngine()
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
    monkeypatch.setattr("search_mcp.engines.serper.AsyncSession", cm_factory)


def _patch_key(monkeypatch, key: str | None = "FAKE"):
    monkeypatch.setattr("search_mcp.engines.serper.get_secret", lambda f: key)


async def test_search_parses_results(monkeypatch):
    e = SerperEngine()
    _patch_key(monkeypatch)
    _patch_session(
        monkeypatch, lambda *a, **kw: _mock_session_returning(200, _OK_PAYLOAD)
    )

    out = await e.search("hello", max_results=10)
    assert len(out) == 3

    a, b, c = out
    assert a.title == "Result A"
    assert a.url == "https://example.com/a"
    assert a.snippet == "first snippet body"
    assert a.published_age == ""

    # <strong>/<em> highlight tags stripped from title and snippet.
    assert b.title == "Result B"
    assert b.snippet == "a dated result"
    # "Jan 5, 2024" -> normalised ISO date.
    assert b.published_age == "2024-01-05"

    # Relative phrase passed through by extract_date_hint.
    assert c.published_age == "2 days ago"


async def test_search_sets_rank_and_engine(monkeypatch):
    e = SerperEngine()
    _patch_key(monkeypatch)
    _patch_session(
        monkeypatch, lambda *a, **kw: _mock_session_returning(200, _OK_PAYLOAD)
    )

    out = await e.search("hello", max_results=10)
    assert [r.rank for r in out] == [1, 2, 3]
    assert all(r.engine == "serper" for r in out)


async def test_search_missing_key_raises(monkeypatch):
    e = SerperEngine()
    _patch_key(monkeypatch, None)
    # Even with a working HTTP layer, a missing key must raise before any call.
    _patch_session(
        monkeypatch, lambda *a, **kw: _mock_session_returning(200, _OK_PAYLOAD)
    )

    with pytest.raises(ValueError) as exc:
        await e.search("hello", max_results=10)
    # Actionable hint mentions the field and how to set it.
    msg = str(exc.value)
    assert "serper_api_key" in msg
    assert "SEARCH_MCP_SERPER_API_KEY" in msg


async def test_search_rejected_key_raises(monkeypatch):
    # A 403 (rejected key) surfaces an actionable error instead of a silent empty.
    e = SerperEngine()
    _patch_key(monkeypatch)
    _patch_session(
        monkeypatch,
        lambda *a, **kw: _mock_session_returning(403, None, text="Forbidden"),
    )

    with pytest.raises(ValueError, match="rejected"):
        await e.search("hello", max_results=10)


async def test_search_server_error_returns_empty(monkeypatch):
    # A non-auth non-200 (e.g. 500) still degrades to empty (transient), no raise.
    e = SerperEngine()
    _patch_key(monkeypatch)
    _patch_session(
        monkeypatch,
        lambda *a, **kw: _mock_session_returning(500, None, text="Server Error"),
    )

    assert await e.search("hello", max_results=10) == []


async def test_search_malformed_json_returns_empty(monkeypatch):
    e = SerperEngine()
    _patch_key(monkeypatch)
    _patch_session(
        monkeypatch,
        lambda *a, **kw: _mock_session_returning(200, ValueError("bad json"), text="<<<"),
    )

    out = await e.search("hello", max_results=10)
    assert out == []


async def test_search_request_exception_returns_empty(monkeypatch):
    e = SerperEngine()
    _patch_key(monkeypatch)

    def factory(*a, **kw):
        raise RuntimeError("boom")

    _patch_session(monkeypatch, factory)
    out = await e.search("hello", max_results=10)
    assert out == []


async def test_search_exclude_text_filter_drops_result(monkeypatch):
    e = SerperEngine()
    _patch_key(monkeypatch)
    _patch_session(
        monkeypatch, lambda *a, **kw: _mock_session_returning(200, _OK_PAYLOAD)
    )

    out = await e.search(
        "hello",
        max_results=10,
        filters=SearchFilters(exclude_text="Second"),
    )
    urls = {r.url for r in out}
    # "Second keyword" lives in Result C's title -> dropped.
    assert "https://example.com/c" not in urls
    assert "https://example.com/a" in urls
    # Ranks recomputed after the drop.
    assert [r.rank for r in out] == [1, 2]


async def test_search_truncates_to_max_results(monkeypatch):
    e = SerperEngine()
    _patch_key(monkeypatch)
    _patch_session(
        monkeypatch, lambda *a, **kw: _mock_session_returning(200, _OK_PAYLOAD)
    )

    out = await e.search("hello", max_results=1)
    assert len(out) == 1
    assert out[0].rank == 1


async def test_search_clamps_num_and_sets_headers_and_body(monkeypatch):
    e = SerperEngine()
    _patch_key(monkeypatch)
    captured: dict = {}

    def factory(*a, **kw):
        captured["headers"] = kw.get("headers")
        cm = MagicMock()

        async def _post(url, json=None, **_):
            captured["url"] = url
            captured["json"] = json
            resp = MagicMock()
            resp.status_code = 200
            resp.json = MagicMock(return_value={"organic": []})
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

    await e.search("hello", max_results=9999, filters=SearchFilters(freshness="week"))
    # num clamped into Serper's page-size band.
    assert captured["json"]["num"] == 20
    assert captured["json"]["q"] == "hello"
    assert captured["json"]["gl"] == "us"
    assert captured["json"]["hl"] == "en"
    # freshness -> tbs=qdr:w
    assert captured["json"]["tbs"] == "qdr:w"
    assert captured["url"] == "https://google.serper.dev/search"
    # Auth header carries the key; Content-Type set for JSON.
    assert captured["headers"]["X-API-KEY"] == "FAKE"
    assert captured["headers"]["Content-Type"] == "application/json"


async def test_search_num_floor(monkeypatch):
    e = SerperEngine()
    _patch_key(monkeypatch)
    captured: dict = {}

    def factory(*a, **kw):
        cm = MagicMock()

        async def _post(url, json=None, **_):
            captured["json"] = json
            resp = MagicMock()
            resp.status_code = 200
            resp.json = MagicMock(return_value={"organic": []})
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

    await e.search("hello", max_results=3)
    # num floored to the minimum page size.
    assert captured["json"]["num"] == 10


async def test_search_empty_results_list(monkeypatch):
    e = SerperEngine()
    _patch_key(monkeypatch)
    _patch_session(
        monkeypatch, lambda *a, **kw: _mock_session_returning(200, {"organic": []})
    )
    out = await e.search("hello", max_results=10)
    assert out == []


async def test_search_missing_organic_key(monkeypatch):
    e = SerperEngine()
    _patch_key(monkeypatch)
    # No "organic" key at all -> empty, never raises.
    _patch_session(
        monkeypatch, lambda *a, **kw: _mock_session_returning(200, {"answerBox": {}})
    )
    out = await e.search("hello", max_results=10)
    assert out == []


async def test_search_skips_items_missing_title_or_url(monkeypatch):
    e = SerperEngine()
    _patch_key(monkeypatch)
    payload = {
        "organic": [
            {"title": "", "link": "https://example.com/no-title"},
            {"title": "No URL", "link": ""},
            {"title": "Good", "link": "https://example.com/good", "snippet": "body"},
        ]
    }
    _patch_session(monkeypatch, lambda *a, **kw: _mock_session_returning(200, payload))

    out = await e.search("hello", max_results=10)
    assert [r.url for r in out] == ["https://example.com/good"]


# ---------------------------------------------------------------------------
# Live network test
# ---------------------------------------------------------------------------


@skip_offline
async def test_live_serper_returns_results():
    # Requires a real serper_api_key (env or config file).
    e = SerperEngine()
    out = await e.search("python language", 10)
    if not out:
        pytest.skip("Serper endpoint unreachable, unkeyed, or returned nothing")
    assert out[0].url.startswith("http")
    assert all(r.engine == "serper" for r in out)
