"""The opt-in `codex` engine: OpenAI's web search on a ChatGPT sign-in.

Offline. The Codex backend is an `httpx.MockTransport` that records what the
engine sent, and the sign-in is a credential written straight to the store.
"""
from __future__ import annotations

import json
import time

import httpx
import pytest

from search_mcp import oauth
from search_mcp.config import settings
from search_mcp.engines import ENGINES, get_engine
from search_mcp.engines.base import EngineKeyError, SearchFilters
from search_mcp.engines.codex import (
    CodexEngine,
    clean_url,
    hits_from_items,
    split_line,
)

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.

BASE = "https://chatgpt.com/backend-api/codex"


def _sign_in(**fields) -> None:
    oauth.save(oauth.Credential(**{
        "provider": "codex", "access_token": "ACCESS", "refresh_token": "r",
        "expires_at": time.time() + 3600, "account_id": "acct-1", **fields,
    }))


@pytest.fixture
def backend(monkeypatch):
    """`routes[path]` answers requests to that path; every request is kept."""
    calls: list[httpx.Request] = []
    routes: dict = {}

    def factory(engine=None, *, timeout=30.0):
        def handle(request: httpx.Request) -> httpx.Response:
            calls.append(request)
            route = routes[request.url.path.rsplit("/codex", 1)[-1]]
            return route(request) if callable(route) else route

        return httpx.AsyncClient(transport=httpx.MockTransport(handle))

    monkeypatch.setattr(oauth, "http_client", factory)
    return routes, calls


def _sse(*events: dict) -> httpx.Response:
    body = "".join(f"event: {e.get('type')}\ndata: {json.dumps(e)}\n\n" for e in events)
    return httpx.Response(200, content=body.encode(), headers={"content-type": "text/event-stream"})


_SEARCH_REPLY = {
    "output": "plaintext for the model",
    "encrypted_output": None,
    "results": [
        {"type": "text_result", "ref_id": "turn0search0",
         "url": "https://docs.python.org/3/library/asyncio-task.html?utm_source=openai",
         "title": "Coroutines and Tasks — Python 3.14 documentation",
         "snippet": "Published 2026-08-01. TaskGroup  holds a group of tasks."},
        {"type": "text_result", "ref_id": "turn0search1",
         "url": "https://example.com/blog/taskgroup", "title": "Using TaskGroup",
         "snippet": "A walk-through."},
        {"type": "image_result", "url": "https://example.com/i.png", "title": "img"},
        {"type": "text_result", "url": "https://docs.python.org/3/library/asyncio-task.html",
         "title": "duplicate after the tracking parameter is gone", "snippet": ""},
    ],
}


# --- wiring --------------------------------------------------------------------------


def test_it_is_registered_as_an_opt_in_web_engine():
    engine = get_engine("codex")
    assert isinstance(engine, CodexEngine)
    assert engine.categories == frozenset()
    assert engine.supports_browser_fallback is False
    assert "codex" not in settings.default_engines
    assert ENGINES["codex"].is_available() is False
    _sign_in()
    assert ENGINES["codex"].is_available() is True


async def test_without_a_sign_in_it_points_away_from_keys_and_sign_ins(backend):
    _, calls = backend
    with pytest.raises(EngineKeyError) as excinfo:
        await CodexEngine().search("anything", 5)
    message = str(excinfo.value)
    assert message.startswith("codex not configured")
    assert message.index("Nothing is wrong with the search") < message.index(
        "Do not ask the user for an API key or a sign-in"
    ) < message.index("search-mcp-login codex")
    assert calls == []


# --- the search endpoint ---------------------------------------------------------------


async def test_the_search_endpoint_is_asked_the_way_codex_asks_it(backend):
    routes, calls = backend
    routes["/alpha/search"] = httpx.Response(200, json=_SEARCH_REPLY)
    _sign_in()

    filters = SearchFilters(freshness="week", include_domains=["python.org"])
    await CodexEngine().search("asyncio taskgroup", 5, filters)

    (sent,) = calls
    assert str(sent.url) == f"{BASE}/alpha/search"
    assert sent.headers["authorization"] == "Bearer ACCESS"
    assert sent.headers["chatgpt-account-id"] == "acct-1"
    assert sent.headers["originator"] == "codex_cli_rs"
    assert sent.headers["user-agent"].startswith("free-search-mcp/")
    body = json.loads(sent.content)
    assert body["commands"] == {
        "search_query": [{"q": "asyncio taskgroup", "recency": 7, "domains": ["python.org"]}]
    }
    assert body["model"] == settings.codex_model
    assert body["settings"]["external_web_access"] is True
    assert body["id"] == sent.headers["session-id"]


async def test_structured_results_become_search_results(backend):
    routes, _ = backend
    routes["/alpha/search"] = httpx.Response(200, json=_SEARCH_REPLY)
    _sign_in()

    results = await CodexEngine().search("asyncio taskgroup", 10)

    assert [r.url for r in results] == [
        "https://docs.python.org/3/library/asyncio-task.html",
        "https://example.com/blog/taskgroup",
    ]
    first = results[0]
    assert first.title == "Coroutines and Tasks — Python 3.14 documentation"
    assert first.snippet == "Published 2026-08-01. TaskGroup holds a group of tasks."
    assert first.published_age == "2026-08-01"
    assert first.published_age_confident is False
    assert [r.rank for r in results] == [1, 2]
    assert {r.engine for r in results} == {"codex"}


async def test_the_post_filters_still_apply(backend):
    routes, _ = backend
    routes["/alpha/search"] = httpx.Response(200, json=_SEARCH_REPLY)
    _sign_in()
    results = await CodexEngine().search(
        "asyncio taskgroup", 10, SearchFilters(exclude_domains=["example.com"])
    )
    assert [r.url for r in results] == ["https://docs.python.org/3/library/asyncio-task.html"]


async def test_an_empty_result_list_is_an_empty_search_not_a_fallback(backend):
    routes, calls = backend
    routes["/alpha/search"] = httpx.Response(200, json={"output": "", "results": []})
    _sign_in()
    assert await CodexEngine().search("zzzz", 5) == []
    assert len(calls) == 1


# --- the Responses fallback ------------------------------------------------------------

_TEXT = (
    "Coroutines and Tasks | 2026-08-01 | TaskGroup runs tasks and waits for all of them. "
    "([docs.python.org](https://docs.python.org/3/library/asyncio-task.html?utm_source=openai))\n"
    "A page the model made up | undated | Never cited, so it must not appear. "
    "[fake](https://invented.example/page)\n"
    "- **PEP 654** | 2021-02-22 | Exception groups and except*. "
    "([peps.python.org](https://peps.python.org/pep-0654/?utm_source=openai))\n"
)


def _responses_reply() -> httpx.Response:
    first = _TEXT.index("([docs")
    second = _TEXT.index("([peps")
    message = {
        "type": "message", "role": "assistant",
        "content": [{
            "type": "output_text", "text": _TEXT,
            "annotations": [
                {"type": "url_citation", "start_index": first, "end_index": first + 10,
                 "url": "https://docs.python.org/3/library/asyncio-task.html?utm_source=openai",
                 "title": "Coroutines and Tasks — Python 3.14 documentation"},
                {"type": "url_citation", "start_index": second, "end_index": second + 10,
                 "url": "https://peps.python.org/pep-0654/?utm_source=openai", "title": ""},
            ],
        }],
    }
    call = {
        "type": "web_search_call", "status": "completed",
        "action": {"type": "search", "query": "asyncio taskgroup", "sources": [
            {"type": "url", "url": "https://docs.python.org/3/library/asyncio-task.html"},
            {"type": "url", "url": "https://realpython.com/python-taskgroup/"},
        ]},
    }
    return _sse(
        {"type": "response.created", "response": {"id": "resp_1"}},
        {"type": "response.output_item.done", "output_index": 0, "item": call},
        {"type": "response.output_item.done", "output_index": 1, "item": message},
        {"type": "response.completed", "response": {"id": "resp_1", "output": []}},
    )


@pytest.mark.parametrize(
    "unsupported",
    [
        httpx.Response(404, json={"detail": "Not Found"}),
        httpx.Response(501, text="not implemented"),
        httpx.Response(200, json={"output": "", "encrypted_output": "gAAAA..."}),
    ],
    ids=["404", "501", "encrypted-only"],
)
async def test_without_the_search_endpoint_it_falls_back_to_responses(backend, unsupported):
    routes, calls = backend
    routes["/alpha/search"] = unsupported
    routes["/responses"] = _responses_reply()
    _sign_in()

    results = await CodexEngine().search("asyncio taskgroup", 10)

    assert [str(c.url) for c in calls] == [f"{BASE}/alpha/search", f"{BASE}/responses"]
    body = json.loads(calls[1].content)
    assert body["stream"] is True and body["store"] is False
    assert body["tools"][0]["type"] == "web_search"
    assert "temperature" not in body and "max_output_tokens" not in body
    assert calls[1].headers["accept"] == "text/event-stream"

    urls = [r.url for r in results]
    assert urls == [
        "https://docs.python.org/3/library/asyncio-task.html",
        "https://peps.python.org/pep-0654/",
        # Consulted by the search but cited on no line: kept, after the rest.
        "https://realpython.com/python-taskgroup/",
    ]
    assert "https://invented.example/page" not in urls
    docs, pep, consulted = results
    # The page's own title wins over the model's paraphrase...
    assert docs.title == "Coroutines and Tasks — Python 3.14 documentation"
    assert docs.published_age == "2026-08-01"
    assert docs.snippet == "TaskGroup runs tasks and waits for all of them."
    # ...and the model's is the fallback when the citation carries none.
    assert pep.title == "PEP 654" and pep.snippet == "Exception groups and except*."
    assert consulted.title == "realpython.com" and consulted.snippet == ""


def test_a_failed_stream_is_an_error():
    from search_mcp.engines.codex import output_items

    with pytest.raises(RuntimeError, match="context_length_exceeded"):
        output_items([{"type": "response.failed",
                       "response": {"error": {"code": "x", "message": "context_length_exceeded"}}}])


def test_the_final_output_fills_in_when_no_item_events_came():
    from search_mcp.engines.codex import output_items

    item = {"type": "message", "content": []}
    assert output_items([{"type": "response.completed", "response": {"output": [item]}}]) == [item]


def test_lines_that_drift_from_the_format_still_parse():
    assert split_line("1. Title | 2026-01-02 | Summary | with a pipe") == (
        split_line("Title | 2026-01-02 | Summary | with a pipe")
    )
    line = split_line("Just a sentence about the page.")
    assert (line.title, line.date, line.summary) == ("", "", "Just a sentence about the page.")
    assert split_line("Title | Summary").title == "Title"
    assert split_line("   ") is None


def test_an_uncited_message_yields_only_the_consulted_sources():
    items = [
        {"type": "message", "content": [{"type": "output_text",
                                         "text": "A | undated | [x](https://made.up/)",
                                         "annotations": []}]},
    ]
    assert hits_from_items(items) == []


def test_clean_url_keeps_other_parameters_and_refuses_non_http():
    assert clean_url("https://a.example/p?q=1&utm_source=openai") == "https://a.example/p?q=1"
    assert clean_url("https://a.example/p?utm_source=newsletter") == (
        "https://a.example/p?utm_source=newsletter"
    )
    assert clean_url("javascript:alert(1)") == ""


# --- failures ------------------------------------------------------------------------------


async def test_an_expired_token_is_refreshed_once_and_the_search_retried(backend, monkeypatch):
    routes, calls = backend
    replies = iter([httpx.Response(401, json={"detail": "Unauthorized"}),
                    httpx.Response(200, json=_SEARCH_REPLY)])
    routes["/alpha/search"] = lambda request: next(replies)
    _sign_in()
    forced: list[bool] = []

    async def credential(provider_id, *, force_refresh=False):
        forced.append(force_refresh)
        return oauth.Credential(provider="codex", access_token="NEW" if force_refresh else "OLD",
                                account_id="acct-1", expires_at=time.time() + 3600)

    monkeypatch.setattr(oauth, "credential", credential)
    results = await CodexEngine().search("asyncio taskgroup", 5)
    assert forced == [False, True]
    assert [c.headers["authorization"] for c in calls] == ["Bearer OLD", "Bearer NEW"]
    assert results


async def test_a_second_refusal_says_to_sign_in_again(backend, monkeypatch):
    routes, _ = backend
    routes["/alpha/search"] = httpx.Response(401, json={"detail": "Unauthorized"})

    async def credential(provider_id, *, force_refresh=False):
        return oauth.Credential(provider="codex", access_token="X", account_id="acct-1",
                                expires_at=time.time() + 3600)

    monkeypatch.setattr(oauth, "credential", credential)
    with pytest.raises(EngineKeyError, match="Sign in again"):
        await CodexEngine().search("q", 5)


async def test_the_usage_limit_is_reported_with_its_reset_time(backend):
    routes, _ = backend
    routes["/alpha/search"] = httpx.Response(429, json={"error": {
        "type": "usage_limit_reached", "message": "The usage limit has been reached",
        "plan_type": "plus", "resets_in_seconds": 7709,
    }})
    _sign_in()
    with pytest.raises(EngineKeyError, match=r"usage limit.*resets in about 128 min"):
        await CodexEngine().search("q", 5)


async def test_a_usage_limit_disguised_as_404_is_not_a_fallback(backend):
    routes, calls = backend
    routes["/alpha/search"] = httpx.Response(404, json={"error": {
        "type": "usage_limit_reached", "message": "limit"}})
    _sign_in()
    with pytest.raises(EngineKeyError, match="usage limit"):
        await CodexEngine().search("q", 5)
    assert len(calls) == 1


async def test_a_model_the_plan_lacks_names_the_setting(backend):
    routes, _ = backend
    routes["/alpha/search"] = httpx.Response(400, json={"detail": (
        "The 'gpt-6-luna' model is not supported when using Codex with a ChatGPT account.")})
    _sign_in()
    with pytest.raises(EngineKeyError, match="SEARCH_MCP_CODEX_MODEL"):
        await CodexEngine().search("q", 5)


async def test_a_server_fault_is_a_plain_error_the_breaker_counts(backend):
    routes, _ = backend
    routes["/alpha/search"] = httpx.Response(503, json={"error": {"code": "server_is_overloaded",
                                                                  "message": "overloaded"}})
    _sign_in()
    with pytest.raises(RuntimeError, match="HTTP 503") as excinfo:
        await CodexEngine().search("q", 5)
    assert not isinstance(excinfo.value, ValueError)


async def test_an_expired_codex_cli_link_is_a_configuration_error(backend):
    _, calls = backend
    path = oauth.codex_cli_auth_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps({"auth_mode": "chatgpt", "tokens": {
        "access_token": "a.eyJleHAiOiAxfQ.s", "id_token": "", "account_id": "acct-1"}}))
    oauth.save(oauth.Credential(provider="codex", source="codex-cli", linked_path=str(path)))
    with pytest.raises(EngineKeyError, match="Run `codex` once"):
        await CodexEngine().search("q", 5)
    assert calls == []


async def test_the_aggregator_reports_an_unsigned_codex_as_an_error():
    from search_mcp.aggregator import aggregate_search

    out = await aggregate_search("anything at all", engines=["codex"], use_cache=False)
    assert out["results"] == []
    assert out["errors"]["codex"].startswith("codex not configured")
