"""An engine repeating a page is not independent evidence for that page."""
from __future__ import annotations

import hashlib
import json
from dataclasses import asdict
from types import SimpleNamespace
from unittest.mock import AsyncMock

import pytest

from search_mcp import aggregator
from search_mcp.config import settings
from search_mcp.engines.base import SearchFilters, SearchResult


def _hit(url: str, engine: str, rank: int, **kwargs) -> SearchResult:
    return SearchResult(
        title="Search ranking semantics", url=url,
        snippet="Search ranking semantics explained", engine=engine, rank=rank,
        **kwargs,
    )


def test_repeated_page_cannot_outrank_independent_engine_agreement():
    repeated = [
        _hit("https://repeat.example/page", "bing", 1),
        _hit("http://www.repeat.example/page/", "bing", 2),
        _hit("https://repeat.example/page#section", "bing", 3),
    ]
    independent = "https://consensus.example/guide"
    results = aggregator._merge(
        [repeated, [_hit(independent, "duckduckgo", 1)],
         [_hit(independent, "yahoojp", 1)]],
        10,
    )
    assert results[0]["url"] == independent
    assert results[0]["score"] == round(2 / 61, 5)
    assert results[1]["score"] == round(1 / 61, 5)
    assert results[1]["engines"] == ["bing"]


@pytest.mark.parametrize("later_rank", [2, 14])
def test_repeated_engine_buckets_keep_the_best_single_vote_and_enrich_metadata(later_rank):
    first = _hit("http://example.org/guide", "bing", 9)
    later = _hit(
        "https://www.example.org/guide/", "bing", later_rank,
        published_age="2026-01-01", published_age_confident=True,
    )
    later.snippet = "A longer explanation of search ranking semantics with more detail."
    result, = aggregator._merge([[first], [later]], 10)
    assert result["score"] == round(1 / (60 + min(9, later_rank)), 5)
    assert result["url"] == "https://www.example.org/guide"
    assert result["snippet"] == later.snippet
    assert result["published_age"] == "2026-01-01"
    assert result["date_source"] == "structured"
    assert result["engines"] == ["bing"]


def test_repeated_direct_record_keeps_one_weighted_vote():
    url = "https://pypi.org/project/example/"
    results = aggregator._merge(
        [[_hit(url, "pypi", 1), _hit(url + "#history", "pypi", 2)]],
        10, category="software.python",
    )
    assert results[0]["score"] == round(5 / 61, 5)


def test_repeated_names_share_the_single_engine_cache_key():
    filters = SearchFilters()
    assert aggregator._key("query", ["bing", "bing"], 10, filters) == aggregator._key(
        "query", ["bing"], 10, filters,
    )


def test_duplicate_inflated_scores_from_the_previous_cache_version_are_not_replayed():
    filters = SearchFilters()
    previous = hashlib.sha256(json.dumps(
        {
            "v": 3, "q": "query", "e": ["bing"], "n": 10, "f": asdict(filters),
            "r": settings.region, "s": settings.safesearch, "l": settings.accept_language,
        },
        ensure_ascii=False, sort_keys=True,
    ).encode("utf-8")).hexdigest()
    assert aggregator._key("query", ["bing"], 10, filters) != previous


@pytest.mark.parametrize("explicit", [False, True])
async def test_repeated_engine_names_are_queried_once(monkeypatch, explicit):
    search = AsyncMock(return_value=[_hit("https://example.org/guide", "duckduckgo", 1)])
    engine = SimpleNamespace(
        categories=frozenset(), single_site=False, search=search,
        is_available=lambda: True, needs_browser=False,
    )
    monkeypatch.setattr(aggregator, "get_engine", lambda name: engine)
    monkeypatch.setattr(aggregator.search_limiter, "acquire", AsyncMock(return_value=True))
    monkeypatch.setattr(settings, "default_engines", ["duckduckgo", "duckduckgo"])
    monkeypatch.setattr(settings, "locale_engines", {})
    monkeypatch.setattr(settings, "health_enabled", False)
    monkeypatch.setattr(settings, "auto_route_enabled", False)
    monkeypatch.setattr(settings, "agent_engines", "off")
    result = await aggregator.aggregate_search(
        "search ranking semantics",
        engines=["duckduckgo", "duckduckgo"] if explicit else None,
        use_cache=False,
    )
    assert search.await_count == 1
    assert result["engines"] == ["duckduckgo"]
    assert result["results"][0]["score"] == round(1 / 61, 5)
