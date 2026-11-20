"""Research readers must see the age of the search that selected their sources."""
from __future__ import annotations

from unittest.mock import AsyncMock

import pytest

from search_mcp import research as research_mod
from search_mcp.formatting import render_research

STAMP = "2026-01-01T10:00:00Z"


@pytest.mark.parametrize("fetch", [False, True])
async def test_cached_source_selection_keeps_its_age_in_markdown(monkeypatch, fetch):
    url = "https://example.org/source"
    monkeypatch.setattr(research_mod, "fetch_many", AsyncMock(return_value=[{
        "url": url, "title": "Source", "content": "A newly fetched page.",
        "retrieved_at": "2026-01-02T10:00:00Z", "tokens_estimated": 5,
    }]))
    payload = await research_mod.research(
        "Current source", fetch=fetch,
        search_payload={
            "engines": ["github"], "retrieved_at": STAMP,
            "cache_age_seconds": 86400,
            "results": [{"url": url, "title": "Source", "snippet": "A lead"}],
        },
    )
    shown = render_research(payload)
    assert f"_(search cached 1 day ago · retrieved {STAMP})_" in shown
    assert shown.index("search cached") < shown.index("## Sources")
    assert payload["retrieved_at"] == STAMP
    assert payload["search_cache_age_seconds"] == 86400
    assert "cache_age_seconds" not in payload
    if fetch:
        assert "A newly fetched page." in shown
        assert "_(page retrieved 2026-01-02T10:00:00Z)_" in shown
        assert payload["documents"][0]["retrieved_at"] != STAMP
    else:
        assert "page retrieved" not in shown
        research_mod.fetch_many.assert_not_awaited()


def test_fresh_search_reports_retrieval_without_claiming_a_cache_hit():
    shown = render_research({"question": "q", "retrieved_at": STAMP})
    assert f"_(search retrieved {STAMP})_" in shown
    assert "cached" not in shown


def test_zero_age_is_still_a_cache_hit():
    shown = render_research({"retrieved_at": STAMP, "search_cache_age_seconds": 0})
    assert f"_(search cached under a minute ago · retrieved {STAMP})_" in shown


def test_known_cache_age_does_not_invent_a_retrieval_timestamp():
    shown = render_research({"search_cache_age_seconds": 7200})
    assert "_(search cached 2 hours ago)_" in shown
    assert "retrieved" not in shown


@pytest.mark.parametrize("age, phrase", [(0, "under a minute"), (7200, "2 hours")])
def test_page_cache_age_is_distinct_from_source_selection_age(age, phrase):
    shown = render_research({
        "question": "q", "retrieved_at": STAMP, "search_cache_age_seconds": 86400,
        "documents": [{
            "url": "https://example.org/source", "content": "Page body",
            "retrieved_at": "2026-01-02T10:00:00Z", "cache_age_seconds": age,
        }],
    })
    assert f"_(search cached 1 day ago · retrieved {STAMP})_" in shown
    assert f"_(page cached {phrase} ago · retrieved 2026-01-02T10:00:00Z)_" in shown


def test_older_payloads_without_provenance_still_render_without_inventing_it():
    shown = render_research({
        "question": "q", "engines": ["bing"], "sources": [],
        "documents": [{"url": "https://example.org/source", "content": "Page body"}],
    })
    assert "# Research brief: q" in shown
    assert "Page body" in shown
    assert "retrieved" not in shown
    assert "cached" not in shown
