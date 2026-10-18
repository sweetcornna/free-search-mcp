"""YouComEngine tests.

The engine calls the you-search tool on You.com's keyless MCP profile with one
JSON-RPC POST, so unit tests mock the AsyncSession layer. One live test runs
with ``SEARCH_MCP_TEST_NETWORK=1``; it spends one of the 100 daily searches the
free profile allows.
"""

from __future__ import annotations

import json
import os
from unittest.mock import AsyncMock, MagicMock

import pytest

from search_mcp.config import settings
from search_mcp.engines import ENGINES
from search_mcp.engines.base import SearchFilters
from search_mcp.engines.youcom import _SNIPPET_CAP, YouComEngine, YouComError

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.

NETWORK = os.environ.get("SEARCH_MCP_TEST_NETWORK") == "1"

# The search JSON the tool returns as text, trimmed from a live answer
# (2026-10-18).
_SEARCH = {
    "results": {
        "web": [
            {
                "url": "https://tokio.rs/",
                "title": "Tokio - An asynchronous Rust runtime",
                "description": "Tokio is a runtime for writing reliable asynchronous applications.",
                "snippets": ["a longer passage that must not win over description"],
                "page_age": "2021-07-30T00:00:00",
            },
            {
                "url": "https://github.com/tokio-rs/tokio",
                "title": "tokio-rs/tokio",
                "description": "",
                "snippets": ["A runtime for writing reliable asynchronous applications", "2"],
            },
        ],
        "news": [
            {
                "url": "https://news.example.com/tokio-1-50",
                "title": "Tokio 1.50 released",
                "description": "The release adds ...",
                "page_age": "2026-10-16T09:00:00",
            }
        ],
    },
    "metadata": {"query": "tokio runtime"},
}


def _result(payload, *, is_error: bool = False) -> dict:
    text = payload if isinstance(payload, str) else json.dumps(payload)
    result = {"content": [{"type": "text", "text": text}]}
    if is_error:
        result["isError"] = True
    return {"jsonrpc": "2.0", "id": 1, "result": result}


def _sse(*messages: dict) -> str:
    return "".join(f"event: message\ndata: {json.dumps(m)}\n\n" for m in messages)


_NOTICE = {
    "jsonrpc": "2.0",
    "method": "notifications/message",
    "params": {"level": "info", "data": "Search successful"},
}


def _session(status: int = 200, body: str = "", *, exc: Exception | None = None):
    response = MagicMock()
    response.status_code = status
    response.text = body
    session = MagicMock()
    session.post = AsyncMock(side_effect=exc) if exc else AsyncMock(return_value=response)
    cm = MagicMock()
    cm.__aenter__ = AsyncMock(return_value=session)
    cm.__aexit__ = AsyncMock(return_value=None)
    cm.session = session
    return cm


def _patch(monkeypatch, cm):
    monkeypatch.setattr("search_mcp.engines.youcom.AsyncSession", lambda *a, **kw: cm)
    return cm.session


def _call(engine, query="q", n=10, filters=None) -> tuple[dict, dict]:
    params = engine._request(query, n, filters)["params"]
    assert params["name"] == "you-search"
    return params["arguments"], params["_meta"]["com.you.com/search"]


def test_registered_opt_in_and_keyless():
    assert isinstance(ENGINES["youcom"], YouComEngine)
    assert "youcom" not in settings.default_engines
    e = ENGINES["youcom"]
    assert e.needs_browser is False
    assert e.supports_browser_fallback is False
    assert e.build_url("q", 5) == "https://api.you.com/mcp?profile=free"


def test_request_carries_query_count_safesearch_and_freshness(monkeypatch):
    monkeypatch.setattr(settings, "safesearch", "strict")
    args, _ = _call(YouComEngine(), "tokio", 99, SearchFilters(freshness="week"))
    assert args == {
        "query": "tokio",
        "count": 50,
        "extraction": "none",
        "safesearch": "strict",
        "freshness": "week",
    }
    args, _ = _call(YouComEngine(), "attention", 5, SearchFilters(category="pdf"))
    assert args["query"] == "attention filetype:pdf"
    assert "freshness" not in args


@pytest.mark.parametrize(
    "region, expected",
    [
        ("us-en", {"country": "US", "language": "EN"}),
        ("uk-en", {"country": "GB", "language": "EN-GB"}),
        ("jp-ja", {"country": "JP", "language": "JA"}),
        ("tw-zh", {"country": "TW", "language": "ZH-HANT"}),
        ("cn-zh", {"country": "CN", "language": "ZH-HANS"}),
        ("br-pt", {"country": "BR", "language": "PT-BR"}),
        ("wt-wt", {}),
        ("", {}),
    ],
)
def test_region_becomes_country_and_language(monkeypatch, region, expected):
    monkeypatch.setattr(settings, "region", region)
    _, meta = _call(YouComEngine())
    assert {k: meta[k] for k in ("country", "language") if k in meta} == expected


def test_domain_filters_never_send_include_and_exclude_together():
    e = YouComEngine()
    args, meta = _call(e, filters=SearchFilters(include_domains=["github.com"]))
    assert meta["include_domains"] == ["github.com"] and "exclude_domains" not in args

    args, meta = _call(e, filters=SearchFilters(exclude_domains=["reddit.com"]))
    assert args["exclude_domains"] == ["reddit.com"] and "include_domains" not in meta

    both = SearchFilters(include_domains=["github.com"], exclude_domains=["reddit.com"])
    args, meta = _call(e, filters=both)
    assert meta["include_domains"] == ["github.com"] and "exclude_domains" not in args

    # An inline site: is an include to the tool.
    args, _ = _call(e, "tokio site:github.com", filters=SearchFilters(exclude_domains=["x.com"]))
    assert "exclude_domains" not in args


async def test_maps_web_and_news_from_an_event_stream(monkeypatch):
    session = _patch(monkeypatch, _session(200, _sse(_NOTICE, _result(_SEARCH))))

    out = await YouComEngine().search("tokio runtime", max_results=10)

    assert [r.url for r in out] == [
        "https://tokio.rs/",
        "https://github.com/tokio-rs/tokio",
        "https://news.example.com/tokio-1-50",
    ]
    web, web2, news = out
    assert web.snippet.startswith("Tokio is a runtime")
    # A web page_age is shown but not trusted as a publication date.
    assert web.published_age == "2021-07-30" and web.published_age_confident is False
    # An empty description falls back to the first passage only.
    assert web2.snippet == "A runtime for writing reliable asynchronous applications"
    assert news.published_age == "2026-10-16" and news.published_age_confident is True
    assert [r.rank for r in out] == [1, 2, 3]
    assert all(r.engine == "youcom" for r in out)
    body = session.post.call_args.kwargs["json"]
    assert body["method"] == "tools/call"
    assert body["params"]["arguments"]["query"] == "tokio runtime"


async def test_accepts_a_plain_json_reply(monkeypatch):
    _patch(monkeypatch, _session(200, json.dumps(_result(_SEARCH))))
    assert len(await YouComEngine().search("q", max_results=10)) == 3


async def test_long_passage_snippet_is_capped(monkeypatch):
    search = {"results": {"web": [{"url": "https://e.com/a", "title": "A", "snippets": ["x" * 2000]}]}}
    _patch(monkeypatch, _session(200, _sse(_result(search))))
    (only,) = await YouComEngine().search("q", max_results=5)
    assert len(only.snippet) <= _SNIPPET_CAP + 2
    assert only.snippet.endswith("…")


async def test_truncates_to_max_results(monkeypatch):
    web = [{"url": f"https://e.com/{i}", "title": f"R {i}", "description": "d"} for i in range(8)]
    _patch(monkeypatch, _session(200, _sse(_result({"results": {"web": web}}))))
    out = await YouComEngine().search("q", max_results=3)
    assert [r.rank for r in out] == [1, 2, 3]


async def test_skips_items_without_title_or_http_url(monkeypatch):
    web = [
        {"url": "https://e.com/ok", "title": "OK"},
        {"url": "", "title": "no url"},
        {"url": "javascript:alert(1)", "title": "not http"},
        {"url": "https://e.com/untitled", "title": "   "},
        "not a dict",
    ]
    _patch(monkeypatch, _session(200, _sse(_result({"results": {"web": web}}))))
    out = await YouComEngine().search("q", max_results=10)
    assert [r.url for r in out] == ["https://e.com/ok"]


async def test_a_tool_error_is_raised_with_its_text(monkeypatch):
    message = "Error: daily limit of 100 free searches reached"
    _patch(monkeypatch, _session(200, _sse(_result(message, is_error=True))))
    with pytest.raises(YouComError, match="daily limit"):
        await YouComEngine().search("q", max_results=5)


async def test_a_json_rpc_error_is_raised(monkeypatch):
    reply = {"jsonrpc": "2.0", "id": 1, "error": {"code": -32602, "message": "Invalid params"}}
    _patch(monkeypatch, _session(200, _sse(reply)))
    with pytest.raises(YouComError, match="Invalid params"):
        await YouComEngine().search("q", max_results=5)


async def test_refusal_is_empty_and_reported(monkeypatch):
    _patch(monkeypatch, _session(429, "Too Many Requests"))
    diag: dict = {}
    assert await YouComEngine().search("q", max_results=5, diagnostics=diag) == []
    assert diag["http_status"]["youcom"] == 429


@pytest.mark.parametrize(
    "body",
    [
        "",
        "not json",
        _sse(_NOTICE),
        _sse(_result("not the search json")),
        _sse({"jsonrpc": "2.0", "id": 1, "result": {"content": "x"}}),
        _sse(_result({"results": {"web": "x"}})),
    ],
)
async def test_malformed_reply_is_empty(monkeypatch, body):
    _patch(monkeypatch, _session(200, body))
    assert await YouComEngine().search("q", max_results=5) == []


async def test_network_error_is_empty(monkeypatch):
    from curl_cffi.requests.exceptions import RequestException

    _patch(monkeypatch, _session(exc=RequestException("connection reset")))
    assert await YouComEngine().search("q", max_results=5) == []


@pytest.mark.skipif(not NETWORK, reason="set SEARCH_MCP_TEST_NETWORK=1 to run")
async def test_live_search_returns_results():
    out = await YouComEngine().search("python asyncio", max_results=5)
    assert out, "the free profile answered with nothing (quota spent?)"
    assert all(r.url.startswith("http") and r.title for r in out)
