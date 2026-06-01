"""GoogleCSEEngine tests.

The engine hits the Google Custom Search JSON API
(GET https://www.googleapis.com/customsearch/v1), so unit tests mock the
AsyncSession HTTP path rather than touch the network, and monkeypatch
``get_secret`` so no real key is needed. A single live-network test is gated
on ``SEARCH_MCP_TEST_NETWORK=1``.
"""
from __future__ import annotations

import os
from unittest.mock import AsyncMock, MagicMock

import pytest

from search_mcp.engines.base import SearchFilters
from search_mcp.engines.google_cse import GoogleCSEEngine

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.

NETWORK = os.environ.get("SEARCH_MCP_TEST_NETWORK") == "1"
skip_offline = pytest.mark.skipif(
    not NETWORK, reason="set SEARCH_MCP_TEST_NETWORK=1 to run"
)


# A representative 200 payload in the real CSE shape: results under ``items``.
# Result A carries <strong>/<em> highlight tags (must be stripped) and an
# ISO-ish date in the snippet (extract_date_hint should normalise it). Result B
# carries a "Second" keyword in its title (used by the exclude_text test).
_OK_PAYLOAD = {
    "kind": "customsearch#search",
    "items": [
        {
            "title": "Result <strong>A</strong>",
            "link": "https://example.com/a",
            "snippet": "Published 2024-02-06. A <em>snippet</em> about the topic.",
            "displayLink": "example.com",
        },
        {
            "title": "Result B with Second keyword",
            "link": "https://example.com/b",
            "snippet": "Another snippet, nothing date-like here.",
            "displayLink": "example.com",
        },
    ],
}


# ---------------------------------------------------------------------------
# Wiring
# ---------------------------------------------------------------------------


def _fake_key(field: str):
    """Default get_secret stand-in: both required fields resolve to a value."""
    return "FAKE"


def _patch_secret(monkeypatch, fn=_fake_key):
    monkeypatch.setattr("search_mcp.engines.google_cse.get_secret", fn)


def _mock_session_returning(status: int, payload, text: str = ""):
    """Build an AsyncSession-shaped context manager whose .get() returns one
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
    session.get = AsyncMock(return_value=response)
    cm = MagicMock()
    cm.__aenter__ = AsyncMock(return_value=session)
    cm.__aexit__ = AsyncMock(return_value=None)
    return cm


def _patch_session(monkeypatch, cm_factory):
    monkeypatch.setattr("search_mcp.engines.google_cse.AsyncSession", cm_factory)


def test_engine_basic_attributes():
    e = GoogleCSEEngine()
    assert e.name == "google_cse"
    assert e.needs_browser is False
    # JSON API: don't waste a Playwright re-render on an empty/malformed body.
    assert e.supports_browser_fallback is False


def test_build_url_is_stable_endpoint():
    e = GoogleCSEEngine()
    url = e.build_url("hello world", 10)
    assert url == "https://www.googleapis.com/customsearch/v1?q=hello+world"
    # No secret (key/cx) leaks into the cache key.
    assert "FAKE" not in url
    assert "key=" not in url and "cx=" not in url


def test_parse_never_raises_and_returns_empty():
    e = GoogleCSEEngine()
    assert e.parse("") == []
    assert e.parse("<html>garbage</html>") == []
    assert e.parse("not json at all {{{") == []


# ---------------------------------------------------------------------------
# search() — mocked HTTP/JSON layer
# ---------------------------------------------------------------------------


async def test_search_parses_results(monkeypatch):
    e = GoogleCSEEngine()
    _patch_secret(monkeypatch)
    _patch_session(
        monkeypatch, lambda *a, **kw: _mock_session_returning(200, _OK_PAYLOAD)
    )

    out = await e.search("hello", max_results=10)
    assert len(out) == 2

    a, b = out
    # Highlight tags stripped from title/snippet.
    assert a.title == "Result A"
    assert a.url == "https://example.com/a"
    assert a.snippet == "Published 2024-02-06. A snippet about the topic."
    # published_age derived from the snippet's ISO date.
    assert a.published_age == "2024-02-06"

    assert b.title == "Result B with Second keyword"
    assert b.url == "https://example.com/b"
    # No date-like text -> empty published_age.
    assert b.published_age == ""


async def test_search_sets_rank_and_engine(monkeypatch):
    e = GoogleCSEEngine()
    _patch_secret(monkeypatch)
    _patch_session(
        monkeypatch, lambda *a, **kw: _mock_session_returning(200, _OK_PAYLOAD)
    )

    out = await e.search("hello", max_results=10)
    assert [r.rank for r in out] == [1, 2]
    assert all(r.engine == "google_cse" for r in out)


async def test_search_non_200_returns_empty(monkeypatch):
    e = GoogleCSEEngine()
    _patch_secret(monkeypatch)
    _patch_session(
        monkeypatch,
        lambda *a, **kw: _mock_session_returning(403, None, text="Forbidden"),
    )

    out = await e.search("hello", max_results=10)
    assert out == []


async def test_search_malformed_json_returns_empty(monkeypatch):
    e = GoogleCSEEngine()
    _patch_secret(monkeypatch)
    _patch_session(
        monkeypatch,
        lambda *a, **kw: _mock_session_returning(200, ValueError("bad json"), text="<<<"),
    )

    out = await e.search("hello", max_results=10)
    assert out == []


async def test_search_no_items_key_returns_empty(monkeypatch):
    # Zero results -> CSE omits the "items" key entirely.
    e = GoogleCSEEngine()
    _patch_secret(monkeypatch)
    _patch_session(
        monkeypatch,
        lambda *a, **kw: _mock_session_returning(200, {"kind": "customsearch#search"}),
    )
    out = await e.search("hello", max_results=10)
    assert out == []


async def test_search_request_exception_returns_empty(monkeypatch):
    e = GoogleCSEEngine()
    _patch_secret(monkeypatch)

    def factory(*a, **kw):
        raise RuntimeError("boom")

    _patch_session(monkeypatch, factory)
    out = await e.search("hello", max_results=10)
    assert out == []


# --- the two missing-key cases (the whole point of this engine) ------------


async def test_search_missing_api_key_raises(monkeypatch):
    e = GoogleCSEEngine()
    # api_key missing, cx present.
    _patch_secret(
        monkeypatch,
        lambda field: None if field == "google_cse_api_key" else "CX",
    )
    with pytest.raises(ValueError, match="google_cse not configured"):
        await e.search("hello", max_results=10)


async def test_search_missing_cx_raises(monkeypatch):
    e = GoogleCSEEngine()
    # api_key present, cx missing.
    _patch_secret(
        monkeypatch,
        lambda field: "KEY" if field == "google_cse_api_key" else None,
    )
    with pytest.raises(ValueError, match="google_cse not configured"):
        await e.search("hello", max_results=10)


async def test_search_both_missing_raises(monkeypatch):
    e = GoogleCSEEngine()
    _patch_secret(monkeypatch, lambda field: None)
    with pytest.raises(ValueError, match="google_cse not configured"):
        await e.search("hello", max_results=10)


async def test_search_exclude_text_filter_drops_result(monkeypatch):
    e = GoogleCSEEngine()
    _patch_secret(monkeypatch)
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


async def test_search_clamps_num_to_10(monkeypatch):
    e = GoogleCSEEngine()
    _patch_secret(monkeypatch)
    captured: dict = {}

    def factory(*a, **kw):
        async def _get(url, params=None, **_):
            captured["params"] = params
            resp = MagicMock()
            resp.status_code = 200
            resp.json = MagicMock(return_value={"items": []})
            resp.text = ""
            return resp

        session = MagicMock()
        session.get = AsyncMock(side_effect=_get)
        cm = MagicMock()
        cm.__aenter__ = AsyncMock(return_value=session)
        cm.__aexit__ = AsyncMock(return_value=None)
        return cm

    _patch_session(monkeypatch, factory)

    await e.search("hello", max_results=9999)
    assert captured["params"]["num"] == 10
    assert captured["params"]["q"] == "hello"
    # Key/cx are sent as params (not logged anywhere).
    assert captured["params"]["key"] == "FAKE"
    assert captured["params"]["cx"] == "FAKE"


async def test_search_freshness_sets_date_restrict(monkeypatch):
    e = GoogleCSEEngine()
    _patch_secret(monkeypatch)
    captured: dict = {}

    def factory(*a, **kw):
        async def _get(url, params=None, **_):
            captured["params"] = params
            resp = MagicMock()
            resp.status_code = 200
            resp.json = MagicMock(return_value={"items": []})
            resp.text = ""
            return resp

        session = MagicMock()
        session.get = AsyncMock(side_effect=_get)
        cm = MagicMock()
        cm.__aenter__ = AsyncMock(return_value=session)
        cm.__aexit__ = AsyncMock(return_value=None)
        return cm

    _patch_session(monkeypatch, factory)

    await e.search("hello", max_results=5, filters=SearchFilters(freshness="week"))
    assert captured["params"]["dateRestrict"] == "w1"


async def test_search_truncates_to_max_results(monkeypatch):
    e = GoogleCSEEngine()
    _patch_secret(monkeypatch)
    _patch_session(
        monkeypatch, lambda *a, **kw: _mock_session_returning(200, _OK_PAYLOAD)
    )

    out = await e.search("hello", max_results=1)
    assert len(out) == 1
    assert out[0].rank == 1


async def test_search_skips_items_missing_title_or_link(monkeypatch):
    e = GoogleCSEEngine()
    _patch_secret(monkeypatch)
    payload = {
        "items": [
            {"title": "", "link": "https://example.com/no-title", "snippet": "x"},
            {"title": "No link", "link": "", "snippet": "x"},
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
async def test_live_google_cse_returns_results():
    e = GoogleCSEEngine()
    out = await e.search("python language", 5)
    if not out:
        pytest.skip("Google CSE unreachable or returned nothing")
    assert out[0].url.startswith("http")
    assert all(r.engine == "google_cse" for r in out)
