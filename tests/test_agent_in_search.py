"""The answer agent running beside `search`.

When an operator configures a backend, `search` starts the agent on the same
ranked list the engines are building and puts its short sourced answer above
the links. These tests pin the parts the caller can observe: the answer block,
the json `agent` field, the fact that the search is not run twice, and that no
agent failure can take the links away or hold them past the deadline.
"""
from __future__ import annotations

import asyncio
import time
from typing import Any

import pytest

from search_mcp import server
from search_mcp.config import settings

PAYLOAD: dict[str, Any] = {
    "query": "latest examplelib release",
    "engines": ["duckduckgo"],
    "results": [
        {
            "title": "examplelib 2.4.0",
            "url": "https://example.org/releases",
            "snippet": "examplelib 2.4.0 is out.",
            "engines": ["duckduckgo"],
            "score": 1.0,
        }
    ],
    "retrieved_at": "2026-10-29T00:00:00Z",
}

ANSWER: dict[str, Any] = {
    "question": "latest examplelib release",
    "answer": "examplelib 2.4.0 is the latest release.",
    "backend": "api",
    "model": "small-model",
    "sources": [
        {
            "rank": 1,
            "title": "examplelib 2.4.0",
            "url": "https://example.org/releases",
            "date": "2026-08-30",
            "read": True,
        }
    ],
    "elapsed_seconds": {"search": 0.0, "model": 1.0, "total": 1.0},
}


@pytest.fixture
def configured(monkeypatch):
    """A backend that passes `config_problem()` with no CLI or network."""
    monkeypatch.setattr(settings, "agent_backend", "api")
    monkeypatch.setattr(settings, "agent_api_base_url", "https://llm.example/v1")
    monkeypatch.setattr(settings, "agent_model", "small-model")
    monkeypatch.setattr(settings, "agent_in_search", True)


@pytest.fixture
def stub_search(monkeypatch):
    async def fake(query, **kwargs):
        return dict(PAYLOAD)

    monkeypatch.setattr(server, "aggregate_search", fake)
    return fake


def _stub_agent(monkeypatch, answer: dict[str, Any]):
    calls: list[dict[str, Any]] = []

    async def fake(question, **kwargs):
        calls.append({"question": question, **kwargs})
        return dict(answer)

    monkeypatch.setattr(server, "run_ask", fake)
    return calls


async def test_search_puts_the_agent_answer_in_markdown(configured, stub_search, monkeypatch):
    calls = _stub_agent(monkeypatch, ANSWER)

    body = await server.search("latest examplelib release")

    assert "## Answer" in body
    assert "examplelib 2.4.0 is the latest release." in body
    assert "backend: api" in body
    # The links are still there below the answer.
    assert "https://example.org/releases" in body
    # The agent was handed the search TASK, not a second search.
    assert len(calls) == 1
    assert isinstance(calls[0]["seed"], asyncio.Task)


async def test_search_json_carries_a_compact_agent_record(configured, stub_search, monkeypatch):
    _stub_agent(monkeypatch, ANSWER)

    payload = await server.search("latest examplelib release", format="json")

    agent = payload["agent"]
    assert agent["answer"] == "examplelib 2.4.0 is the latest release."
    assert agent["backend"] == "api"
    assert agent["model"] == "small-model"
    assert [s["url"] for s in agent["sources"]] == ["https://example.org/releases"]
    # The full research payload belongs to `ask`, not to every search.
    assert "research" not in agent


async def test_a_failed_agent_leaves_the_links_and_says_why(configured, stub_search, monkeypatch):
    _stub_agent(
        monkeypatch,
        {
            "answer": None,
            "backend": "api",
            "model": "",
            "sources": [],
            "elapsed_seconds": {"search": 0.0, "model": 0.0, "total": 0.0},
            "agent_error": "the model returned no answer text",
        },
    )

    payload = await server.search("latest examplelib release", format="json")

    assert [r["url"] for r in payload["results"]] == ["https://example.org/releases"]
    assert payload["agent"]["error"] == "the model returned no answer text"

    body = await server.search("latest examplelib release")
    assert "Answer agent: the model returned no answer text" in body
    assert "https://example.org/releases" in body


async def test_a_slow_agent_cannot_hold_the_search(configured, stub_search, monkeypatch):
    monkeypatch.setattr(settings, "agent_search_timeout_seconds", 0.05)

    async def slow(question, **kwargs):
        await asyncio.sleep(5)
        return ANSWER

    monkeypatch.setattr(server, "run_ask", slow)

    started = time.monotonic()
    payload = await server.search("latest examplelib release", format="json")
    elapsed = time.monotonic() - started

    assert elapsed < 1.0
    assert payload["results"]
    assert "did not finish within" in payload["agent"]["error"]


async def test_agent_in_search_can_be_switched_off(configured, stub_search, monkeypatch):
    monkeypatch.setattr(settings, "agent_in_search", False)
    calls = _stub_agent(monkeypatch, ANSWER)

    payload = await server.search("latest examplelib release", format="json")

    assert "agent" not in payload
    assert calls == []


async def test_an_off_backend_never_runs_the_agent(stub_search, monkeypatch):
    monkeypatch.setattr(settings, "agent_backend", "off")
    calls = _stub_agent(monkeypatch, ANSWER)

    payload = await server.search("latest examplelib release", format="json")

    assert "agent" not in payload
    assert calls == []


async def test_a_broken_agent_config_does_not_fail_search(stub_search, monkeypatch):
    """A backend named in the environment but missing its model is a setup
    mistake; the links are still the tool's job."""
    monkeypatch.setattr(settings, "agent_backend", "api")
    monkeypatch.setattr(settings, "agent_api_base_url", "https://llm.example/v1")
    monkeypatch.setattr(settings, "agent_model", "")
    calls = _stub_agent(monkeypatch, ANSWER)

    payload = await server.search("latest examplelib release", format="json")

    assert [r["url"] for r in payload["results"]] == ["https://example.org/releases"]
    assert "agent" not in payload
    assert calls == []


async def test_an_empty_search_does_not_wait_for_an_answer(configured, monkeypatch):
    """With no links there is no evidence. The agent leg is cancelled instead
    of spending its deadline on a question it cannot answer."""
    async def empty(query, **kwargs):
        return {**PAYLOAD, "results": []}

    monkeypatch.setattr(server, "aggregate_search", empty)

    async def slow(question, **kwargs):
        await asyncio.sleep(30)
        return ANSWER

    monkeypatch.setattr(server, "run_ask", slow)
    monkeypatch.setattr(settings, "agent_search_timeout_seconds", 20)

    started = time.monotonic()
    payload = await server.search("latest examplelib release", format="json")
    elapsed = time.monotonic() - started

    assert elapsed < 1.0
    assert payload["results"] == []
    assert "agent" not in payload


@pytest.mark.parametrize("category", ["image", "dataset", "dataset.repository"])
async def test_visual_categories_do_not_ask_for_an_answer(configured, stub_search, monkeypatch, category):
    calls = _stub_agent(monkeypatch, ANSWER)

    payload = await server.search(
        "latest examplelib release", category=category, format="json"
    )

    assert "agent" not in payload
    assert calls == []
