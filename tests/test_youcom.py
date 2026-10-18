"""YouComEngine tests.

The engine calls a keyless JSON GET endpoint, so unit tests mock the
AsyncSession layer. One live test runs with ``SEARCH_MCP_TEST_NETWORK=1``; it
spends one of the roughly 100 daily searches the keyless tier allows per IP.
"""

from __future__ import annotations

import os
from unittest.mock import AsyncMock, MagicMock
from urllib.parse import parse_qs, urlsplit

import pytest

from search_mcp.engines import ENGINES
from search_mcp.engines.base import SearchFilters
from search_mcp.engines.youcom import _SNIPPET_CAP, YouComEngine

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.

NETWORK = os.environ.get("SEARCH_MCP_TEST_NETWORK") == "1"

# The live shape, trimmed (measured 2026-10-18).
_OK_PAYLOAD = {
    "results": {
        "web": [
            {
                "url": "https://docs.python.org/3/library/asyncio-task.html",
                "title": "Coroutines and tasks",
                "description": "Timeout context managers can be safely nested.",
                "snippets": ["a longer passage that must not win over description"],
                "page_age": "2026-10-17T08:20:26",
                "favicon_url": "https://you.com/favicon?domain=docs.python.org",
            },
            {
                "url": "https://superfastpython.com/asyncio-timeout/",
                "title": "asyncio.timeout() To Wait and Cancel Tasks",
                "description": "",
                "snippets": ["The asyncio.timeout() context manager", "second"],
            },
        ]
    },
    "metadata": {"query": "python asyncio timeout", "latency": 0.76},
}


def _session(status: int, payload):
    """An AsyncSession stand-in whose .get() returns one response. ``payload``
    may be an Exception, which .json() then raises."""
    response = MagicMock()
    response.status_code = status
    if isinstance(payload, Exception):
        response.json = MagicMock(side_effect=payload)
    else:
        response.json = MagicMock(return_value=payload)
    session = MagicMock()
    session.get = AsyncMock(return_value=response)
    cm = MagicMock()
    cm.__aenter__ = AsyncMock(return_value=session)
    cm.__aexit__ = AsyncMock(return_value=None)
    cm.session = session
    return cm


def _patch(monkeypatch, cm):
    monkeypatch.setattr("search_mcp.engines.youcom.AsyncSession", lambda *a, **kw: cm)
    return cm.session


def _params(url: str) -> dict[str, str]:
    return {k: v[0] for k, v in parse_qs(urlsplit(url).query).items()}


def test_registered_opt_in_and_keyless():
    from search_mcp.config import settings

    assert isinstance(ENGINES["youcom"], YouComEngine)
    assert "youcom" not in settings.default_engines
    e = ENGINES["youcom"]
    assert e.needs_browser is False
    assert e.supports_browser_fallback is False


def test_build_url_carries_query_count_and_operators():
    e = YouComEngine()
    url = e.build_url("tokio", 10)
    assert url.startswith("https://api.you.com/v1/agents/search?")
    assert _params(url) == {"query": "tokio", "count": "10"}

    filtered = e.build_url(
        "tokio",
        99,
        SearchFilters(include_domains=["github.com"], exclude_domains=["reddit.com"]),
    )
    assert _params(filtered) == {
        "query": "tokio site:github.com -site:reddit.com",
        "count": "50",
    }
    assert _params(e.build_url("attention", 5, SearchFilters(category="pdf")))["query"] == (
        "attention filetype:pdf"
    )


def test_parse_is_unused_and_never_raises():
    assert YouComEngine().parse("<html>anything</html>") == []


async def test_search_maps_results(monkeypatch):
    session = _patch(monkeypatch, _session(200, _OK_PAYLOAD))

    out = await YouComEngine().search("python asyncio timeout", max_results=10)

    assert [r.url for r in out] == [
        "https://docs.python.org/3/library/asyncio-task.html",
        "https://superfastpython.com/asyncio-timeout/",
    ]
    a, b = out
    assert a.snippet == "Timeout context managers can be safely nested."
    assert a.published_age == "2026-10-17"
    assert a.published_age_confident is True
    # An empty description falls back to the first passage only.
    assert b.snippet == "The asyncio.timeout() context manager"
    assert b.published_age == ""
    assert [r.rank for r in out] == [1, 2]
    assert all(r.engine == "youcom" for r in out)
    assert _params(session.get.call_args.args[0])["query"] == "python asyncio timeout"


async def test_long_passage_snippet_is_capped(monkeypatch):
    payload = {
        "results": {
            "web": [
                {"url": "https://example.com/a", "title": "A", "snippets": ["x" * 2000]},
            ]
        }
    }
    _patch(monkeypatch, _session(200, payload))
    (only,) = await YouComEngine().search("q", max_results=5)
    assert len(only.snippet) <= _SNIPPET_CAP + 2
    assert only.snippet.endswith("…")


async def test_truncates_to_max_results(monkeypatch):
    web = [
        {"url": f"https://example.com/{i}", "title": f"Result {i}", "description": "d"}
        for i in range(8)
    ]
    _patch(monkeypatch, _session(200, {"results": {"web": web}}))
    out = await YouComEngine().search("q", max_results=3)
    assert [r.rank for r in out] == [1, 2, 3]


async def test_skips_items_without_title_or_http_url(monkeypatch):
    web = [
        {"url": "https://example.com/ok", "title": "OK"},
        {"url": "", "title": "no url"},
        {"url": "javascript:alert(1)", "title": "not http"},
        {"url": "https://example.com/untitled", "title": "   "},
        "not a dict",
    ]
    _patch(monkeypatch, _session(200, {"results": {"web": web}}))
    out = await YouComEngine().search("q", max_results=10)
    assert [r.url for r in out] == ["https://example.com/ok"]


@pytest.mark.parametrize(
    "payload",
    [ValueError("not json"), None, [], {"results": None}, {"results": {"web": "x"}}],
)
async def test_malformed_payload_is_empty(monkeypatch, payload):
    _patch(monkeypatch, _session(200, payload))
    assert await YouComEngine().search("q", max_results=5) == []


async def test_refusal_is_empty_and_reported(monkeypatch):
    _patch(monkeypatch, _session(429, {"error": "rate limited"}))
    diag: dict = {}
    assert await YouComEngine().search("q", max_results=5, diagnostics=diag) == []
    assert diag["http_status"]["youcom"] == 429


async def test_network_error_is_empty(monkeypatch):
    from curl_cffi.requests.exceptions import RequestException

    cm = _session(200, _OK_PAYLOAD)
    cm.session.get = AsyncMock(side_effect=RequestException("connection reset"))
    _patch(monkeypatch, cm)
    assert await YouComEngine().search("q", max_results=5) == []


@pytest.mark.skipif(not NETWORK, reason="set SEARCH_MCP_TEST_NETWORK=1 to run")
async def test_live_search_returns_results():
    out = await YouComEngine().search("python asyncio", max_results=5)
    assert out, "the keyless tier answered with nothing (quota spent?)"
    assert all(r.url.startswith("http") and r.title for r in out)
