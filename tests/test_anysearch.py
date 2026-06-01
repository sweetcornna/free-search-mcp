"""AnySearchEngine tests.

The engine hits a JSON POST REST API (https://api.anysearch.com/v1/search),
so unit tests mock the AsyncSession HTTP path rather than touch the network.
A single live-network test is gated on ``SEARCH_MCP_TEST_NETWORK=1``.
"""
from __future__ import annotations

import os
from unittest.mock import AsyncMock, MagicMock

import pytest

from search_mcp.engines.anysearch import AnySearchEngine
from search_mcp.engines.base import SearchFilters

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.

NETWORK = os.environ.get("SEARCH_MCP_TEST_NETWORK") == "1"
skip_offline = pytest.mark.skipif(
    not NETWORK, reason="set SEARCH_MCP_TEST_NETWORK=1 to run"
)


# A representative 200 payload in the REAL live shape: results nested under
# ``data.results`` (with the top-level code/message envelope the live API
# returns). The first item has a longer ``content`` than ``description``
# (content should win); the second has a longer ``description`` (description
# should win) and a ``published_at`` to derive published_age from.
_OK_PAYLOAD = {
    "code": 0,
    "message": "success",
    "data": {
        "results": [
            {
                "title": "Result A",
                "url": "https://example.com/a",
                "description": "short desc",
                "content": "a much longer content body that should be chosen as snippet",
                "score": 0.9,
                "quality_score": 0.8,
                "signal_scores": {"relevance": 0.75},
                "published_at": "2024-02-06T00:00:00Z",
            },
            {
                "title": "Result B with Second keyword",
                "url": "https://example.com/b",
                "description": "a longer description that should be chosen over content",
                "content": "tiny",
                "score": 0.7,
                "published_at": "2025-11-30T12:34:56Z",
            },
        ],
    },
}


# Flat fallback shape — older/alternate {"results": [...]} the engine also
# tolerates so an API surface change can't silently zero us out.
_FLAT_PAYLOAD = {
    "results": [
        {"title": "Flat", "url": "https://example.com/flat", "content": "body"},
    ],
}


# ---------------------------------------------------------------------------
# Wiring
# ---------------------------------------------------------------------------


def test_engine_basic_attributes():
    e = AnySearchEngine()
    assert e.name == "anysearch"
    assert e.needs_browser is False
    # JSON API: don't waste a Playwright re-render on an empty/malformed body.
    assert e.supports_browser_fallback is False


def test_build_url_is_stable_endpoint():
    e = AnySearchEngine()
    url = e.build_url("hello world", 10)
    assert url == "https://api.anysearch.com/v1/search"
    # Same regardless of query/filters — the body carries those.
    assert e.build_url("other", 5, SearchFilters(freshness="week")) == url


def test_parse_never_raises_and_returns_empty():
    # parse() is unused on the POST path but the ABC requires it; it must
    # never raise and always return [].
    e = AnySearchEngine()
    assert e.parse("") == []
    assert e.parse("<html>garbage</html>") == []
    assert e.parse("not json at all {{{") == []


# ---------------------------------------------------------------------------
# search() — mocked HTTP/JSON layer
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
    monkeypatch.setattr("search_mcp.engines.anysearch.AsyncSession", cm_factory)


async def test_search_parses_results(monkeypatch):
    e = AnySearchEngine()
    _patch_session(
        monkeypatch, lambda *a, **kw: _mock_session_returning(200, _OK_PAYLOAD)
    )

    out = await e.search("hello", max_results=10)
    assert len(out) == 2

    a, b = out
    assert a.title == "Result A"
    assert a.url == "https://example.com/a"
    # content was longer than description -> snippet is content.
    assert a.snippet == "a much longer content body that should be chosen as snippet"
    # published_age is the YYYY-MM-DD portion of published_at.
    assert a.published_age == "2024-02-06"

    # description was longer than content -> snippet is description.
    assert b.snippet == "a longer description that should be chosen over content"
    assert b.published_age == "2025-11-30"


async def test_search_parses_flat_fallback_shape(monkeypatch):
    # A flat {"results": [...]} (no data envelope) must still parse.
    e = AnySearchEngine()
    _patch_session(
        monkeypatch, lambda *a, **kw: _mock_session_returning(200, _FLAT_PAYLOAD)
    )
    out = await e.search("hello", max_results=10)
    assert [r.url for r in out] == ["https://example.com/flat"]


async def test_search_sets_rank_and_engine(monkeypatch):
    e = AnySearchEngine()
    _patch_session(
        monkeypatch, lambda *a, **kw: _mock_session_returning(200, _OK_PAYLOAD)
    )

    out = await e.search("hello", max_results=10)
    assert [r.rank for r in out] == [1, 2]
    assert all(r.engine == "anysearch" for r in out)


async def test_search_non_200_returns_empty(monkeypatch):
    e = AnySearchEngine()
    _patch_session(
        monkeypatch,
        lambda *a, **kw: _mock_session_returning(429, None, text="Too Many Requests"),
    )

    out = await e.search("hello", max_results=10)
    assert out == []


async def test_search_malformed_json_returns_empty(monkeypatch):
    e = AnySearchEngine()
    _patch_session(
        monkeypatch,
        lambda *a, **kw: _mock_session_returning(200, ValueError("bad json"), text="<<<"),
    )

    out = await e.search("hello", max_results=10)
    assert out == []


async def test_search_request_exception_returns_empty(monkeypatch):
    e = AnySearchEngine()

    def factory(*a, **kw):
        raise RuntimeError("boom")

    _patch_session(monkeypatch, factory)
    out = await e.search("hello", max_results=10)
    assert out == []


async def test_search_exclude_text_filter_drops_result(monkeypatch):
    e = AnySearchEngine()
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
    e = AnySearchEngine()
    _patch_session(
        monkeypatch, lambda *a, **kw: _mock_session_returning(200, _OK_PAYLOAD)
    )

    out = await e.search("hello", max_results=1)
    assert len(out) == 1
    assert out[0].rank == 1


async def test_search_clamps_max_results_in_body(monkeypatch):
    e = AnySearchEngine()
    captured: dict = {}

    def factory(*a, **kw):
        cm = _mock_session_returning(200, {"results": []})

        async def _post(url, json=None, **_):
            captured["json"] = json
            resp = MagicMock()
            resp.status_code = 200
            resp.json = MagicMock(return_value={"results": []})
            resp.text = ""
            return resp

        # Replace the session's .post so we can capture the body sent.
        async def _aenter(*_a, **_kw):
            session = MagicMock()
            session.post = AsyncMock(side_effect=_post)
            return session

        cm.__aenter__ = AsyncMock(side_effect=_aenter)
        return cm

    _patch_session(monkeypatch, factory)

    await e.search("hello", max_results=9999)
    assert captured["json"]["max_results"] == 100
    assert captured["json"]["query"] == "hello"


async def test_search_empty_results_list(monkeypatch):
    e = AnySearchEngine()
    _patch_session(
        monkeypatch, lambda *a, **kw: _mock_session_returning(200, {"results": []})
    )
    out = await e.search("hello", max_results=10)
    assert out == []


async def test_search_skips_items_missing_title_or_url(monkeypatch):
    e = AnySearchEngine()
    payload = {
        "results": [
            {"title": "", "url": "https://example.com/no-title"},
            {"title": "No URL", "url": ""},
            {"title": "Good", "url": "https://example.com/good", "content": "body"},
        ]
    }
    _patch_session(monkeypatch, lambda *a, **kw: _mock_session_returning(200, payload))

    out = await e.search("hello", max_results=10)
    assert [r.url for r in out] == ["https://example.com/good"]


# ---------------------------------------------------------------------------
# Live network test
# ---------------------------------------------------------------------------


@skip_offline
async def test_live_anysearch_returns_results():
    e = AnySearchEngine()
    out = await e.search("python language", 5)
    if not out:
        pytest.skip("AnySearch endpoint unreachable or returned nothing")
    assert out[0].url.startswith("http")
    assert all(r.engine == "anysearch" for r in out)
