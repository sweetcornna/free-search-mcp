"""TavilyEngine tests.

Tavily is a KEYED JSON POST API (https://api.tavily.com/search), so unit tests
mock both the keystore (``get_secret`` -> a fake key) and the AsyncSession HTTP
path rather than touch the network or read a real config. A single
live-network test is gated on ``SEARCH_MCP_TEST_NETWORK=1`` (and a real key).
"""
from __future__ import annotations

import os
from unittest.mock import AsyncMock, MagicMock

import pytest

from search_mcp.engines.base import SearchFilters
from search_mcp.engines.tavily import TavilyEngine

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.

NETWORK = os.environ.get("SEARCH_MCP_TEST_NETWORK") == "1"
skip_offline = pytest.mark.skipif(
    not NETWORK, reason="set SEARCH_MCP_TEST_NETWORK=1 to run"
)


# A representative 200 payload in the live Tavily shape. The first item has a
# ``published_date`` (ISO) to derive published_age from and <strong> highlight
# tags that must be stripped; the second carries a ``Second`` keyword in its
# title (for the exclude_text test) and no published_date (-> "").
_OK_PAYLOAD = {
    "answer": None,
    "query": "hello",
    "results": [
        {
            "title": "Result <strong>A</strong>",
            "url": "https://example.com/a",
            "content": "an <em>answer</em> oriented snippet body",
            "score": 0.97,
            "published_date": "2024-02-06T00:00:00Z",
        },
        {
            "title": "Result B with Second keyword",
            "url": "https://example.com/b",
            "content": "another snippet",
            "score": 0.72,
        },
    ],
}


# ---------------------------------------------------------------------------
# Wiring
# ---------------------------------------------------------------------------


def test_engine_basic_attributes():
    e = TavilyEngine()
    assert e.name == "tavily"
    assert e.needs_browser is False
    # JSON API: don't waste a Playwright re-render on an empty/malformed body.
    assert e.supports_browser_fallback is False


def test_build_url_is_stable_endpoint():
    e = TavilyEngine()
    url = e.build_url("hello world", 10)
    assert url == "https://api.tavily.com/search"
    # Same regardless of query/filters — the body carries those (and never the
    # secret), keeping the cache key stable and key-free.
    assert e.build_url("other", 5, SearchFilters(freshness="week")) == url


def test_parse_never_raises_and_returns_empty():
    # parse() is unused on the POST path but the ABC requires it; it must never
    # raise and always return [].
    e = TavilyEngine()
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


def _patch_key(monkeypatch, key: str | None = "FAKE"):
    monkeypatch.setattr(
        "search_mcp.engines.tavily.get_secret", lambda f: key
    )


def _patch_session(monkeypatch, cm_factory):
    monkeypatch.setattr("search_mcp.engines.tavily.AsyncSession", cm_factory)


async def test_search_parses_results(monkeypatch):
    e = TavilyEngine()
    _patch_key(monkeypatch)
    _patch_session(
        monkeypatch, lambda *a, **kw: _mock_session_returning(200, _OK_PAYLOAD)
    )

    out = await e.search("hello", max_results=10)
    assert len(out) == 2

    a, b = out
    # <strong> highlight tags stripped from the title.
    assert a.title == "Result A"
    assert a.url == "https://example.com/a"
    # snippet = content, with <em> tags stripped.
    assert a.snippet == "an answer oriented snippet body"
    # published_age is the YYYY-MM-DD portion of published_date.
    assert a.published_age == "2024-02-06"

    # No published_date -> "".
    assert b.published_age == ""


async def test_search_sets_rank_and_engine(monkeypatch):
    e = TavilyEngine()
    _patch_key(monkeypatch)
    _patch_session(
        monkeypatch, lambda *a, **kw: _mock_session_returning(200, _OK_PAYLOAD)
    )

    out = await e.search("hello", max_results=10)
    assert [r.rank for r in out] == [1, 2]
    assert all(r.engine == "tavily" for r in out)


async def test_search_missing_key_raises(monkeypatch):
    e = TavilyEngine()
    _patch_key(monkeypatch, None)
    # Even with a working session, a missing key must raise before any request.
    _patch_session(
        monkeypatch, lambda *a, **kw: _mock_session_returning(200, _OK_PAYLOAD)
    )

    with pytest.raises(ValueError) as ei:
        await e.search("hello", max_results=10)
    # Actionable hint mentions the field and how to configure it.
    msg = str(ei.value)
    assert "tavily_api_key" in msg
    assert "SEARCH_MCP_TAVILY_API_KEY" in msg


async def test_search_rejected_key_raises(monkeypatch):
    # A 401 (rejected key) surfaces an actionable error instead of a silent empty.
    e = TavilyEngine()
    _patch_key(monkeypatch)
    _patch_session(
        monkeypatch,
        lambda *a, **kw: _mock_session_returning(401, None, text="Unauthorized"),
    )

    with pytest.raises(ValueError, match="rejected"):
        await e.search("hello", max_results=10)


async def test_search_server_error_returns_empty(monkeypatch):
    # A non-auth non-200 (e.g. 500) still degrades to empty (transient), no raise.
    e = TavilyEngine()
    _patch_key(monkeypatch)
    _patch_session(
        monkeypatch,
        lambda *a, **kw: _mock_session_returning(500, None, text="Server Error"),
    )

    assert await e.search("hello", max_results=10) == []


async def test_search_malformed_json_returns_empty(monkeypatch):
    e = TavilyEngine()
    _patch_key(monkeypatch)
    _patch_session(
        monkeypatch,
        lambda *a, **kw: _mock_session_returning(200, ValueError("bad json"), text="<<<"),
    )

    out = await e.search("hello", max_results=10)
    assert out == []


async def test_search_request_exception_returns_empty(monkeypatch):
    e = TavilyEngine()
    _patch_key(monkeypatch)

    def factory(*a, **kw):
        raise RuntimeError("boom")

    _patch_session(monkeypatch, factory)
    out = await e.search("hello", max_results=10)
    assert out == []


async def test_search_exclude_text_filter_drops_result(monkeypatch):
    e = TavilyEngine()
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
    # "Second keyword" lives in Result B's title -> dropped.
    assert "https://example.com/b" not in urls
    assert "https://example.com/a" in urls
    # Ranks recomputed after the drop.
    assert [r.rank for r in out] == [1]


async def test_search_truncates_to_max_results(monkeypatch):
    e = TavilyEngine()
    _patch_key(monkeypatch)
    _patch_session(
        monkeypatch, lambda *a, **kw: _mock_session_returning(200, _OK_PAYLOAD)
    )

    out = await e.search("hello", max_results=1)
    assert len(out) == 1
    assert out[0].rank == 1


async def test_search_clamps_max_results_and_sends_body(monkeypatch):
    e = TavilyEngine()
    _patch_key(monkeypatch, "SECRETKEY")
    captured: dict = {}

    def factory(*a, **kw):
        # Tavily auth lives in the Authorization header passed to the session
        # ctor, NOT in the body — capture it so we can assert the real contract.
        captured["headers"] = kw.get("headers")

        async def _post(url, json=None, **_):
            captured["json"] = json
            resp = MagicMock()
            resp.status_code = 200
            resp.json = MagicMock(return_value={"results": []})
            resp.text = ""
            return resp

        async def _aenter(*_a, **_kw):
            session = MagicMock()
            session.post = AsyncMock(side_effect=_post)
            return session

        cm = MagicMock()
        cm.__aenter__ = AsyncMock(side_effect=_aenter)
        cm.__aexit__ = AsyncMock(return_value=None)
        return cm

    _patch_session(monkeypatch, factory)

    await e.search(
        "hello",
        max_results=9999,
        filters=SearchFilters(
            include_domains=["example.com"], exclude_domains=["spam.com"]
        ),
    )
    body = captured["json"]
    # Auth is a Bearer header, and the key must NOT leak into the body.
    assert captured["headers"]["Authorization"] == "Bearer SECRETKEY"
    assert "api_key" not in body
    # Clamped to Tavily's 1..20 range.
    assert body["max_results"] == 20
    assert body["query"] == "hello"
    assert body["search_depth"] == "basic"
    assert body["include_answer"] is False
    assert body["include_raw_content"] is False
    # Native domain filters forwarded.
    assert body["include_domains"] == ["example.com"]
    assert body["exclude_domains"] == ["spam.com"]


async def test_search_min_clamp(monkeypatch):
    e = TavilyEngine()
    _patch_key(monkeypatch)
    captured: dict = {}

    def factory(*a, **kw):
        async def _post(url, json=None, **_):
            captured["json"] = json
            resp = MagicMock()
            resp.status_code = 200
            resp.json = MagicMock(return_value={"results": []})
            resp.text = ""
            return resp

        async def _aenter(*_a, **_kw):
            session = MagicMock()
            session.post = AsyncMock(side_effect=_post)
            return session

        cm = MagicMock()
        cm.__aenter__ = AsyncMock(side_effect=_aenter)
        cm.__aexit__ = AsyncMock(return_value=None)
        return cm

    _patch_session(monkeypatch, factory)

    await e.search("hello", max_results=0)
    assert captured["json"]["max_results"] == 1
    # No domain filters -> keys absent from the body.
    assert "include_domains" not in captured["json"]
    assert "exclude_domains" not in captured["json"]


async def test_search_empty_results_list(monkeypatch):
    e = TavilyEngine()
    _patch_key(monkeypatch)
    _patch_session(
        monkeypatch, lambda *a, **kw: _mock_session_returning(200, {"results": []})
    )
    out = await e.search("hello", max_results=10)
    assert out == []


async def test_search_skips_items_missing_title_or_url(monkeypatch):
    e = TavilyEngine()
    _patch_key(monkeypatch)
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
# Live network test (needs a real key in SEARCH_MCP_TAVILY_API_KEY)
# ---------------------------------------------------------------------------


@skip_offline
async def test_live_tavily_returns_results():
    e = TavilyEngine()
    try:
        out = await e.search("python language", 5)
    except ValueError:
        pytest.skip("no Tavily key configured")
    if not out:
        pytest.skip("Tavily endpoint unreachable or returned nothing")
    assert out[0].url.startswith("http")
    assert all(r.engine == "tavily" for r in out)
