"""Accepted engine spellings must share dispatch, cache and limiter identity."""
from __future__ import annotations

import pytest

from search_mcp import aggregator
from search_mcp import engines as registry
from search_mcp.config import settings
from search_mcp.engines.base import Engine, SearchFilters, SearchResult
from search_mcp.health import engine_health
from search_mcp.ratelimit import RateLimiter


class CountingEngine(Engine):
    def __init__(self, name: str, *, empty: bool = False):
        self.name = name
        self.empty = empty
        self.calls = 0

    def build_url(self, query, max_results, filters=None):
        raise AssertionError("offline fixture must not build a network URL")

    def parse(self, html):
        raise AssertionError("offline fixture must not parse network output")

    async def search(self, query, max_results, filters=None, diagnostics=None):
        self.calls += 1
        if self.empty:
            return []
        return [SearchResult(
            title="Python async tutorial", url=f"https://{self.name}.example/tutorial",
            snippet="Python async tutorial with coroutines, tasks and event loops.",
            engine=self.name, rank=1,
        )]


@pytest.fixture
def identity_world(monkeypatch):
    engines = {name: CountingEngine(name) for name in ("bing", "duckduckgo", "yahoojp")}
    # Preserve get_engine itself: a fake normalizer would hide the defect.
    monkeypatch.setattr(registry, "ENGINES", engines)
    monkeypatch.setattr(aggregator, "ENGINES", engines)
    monkeypatch.setattr(aggregator, "search_limiter", RateLimiter(30))
    monkeypatch.setattr(settings, "default_engines", ["bing"])
    monkeypatch.setattr(settings, "reserve_engines", [])
    monkeypatch.setattr(settings, "rescue_engines", [])
    monkeypatch.setattr(settings, "fresh_engines", [])
    monkeypatch.setattr(settings, "locale_engines", {})
    monkeypatch.setattr(settings, "health_enabled", False)
    monkeypatch.setattr(settings, "auto_route_enabled", False)
    monkeypatch.setattr(settings, "agent_engines", "off")
    monkeypatch.setattr(settings, "coherence_guard_enabled", False)
    return engines


@pytest.mark.parametrize("explicit", [False, True])
async def test_aliases_share_one_dispatch_limiter_and_cache(identity_world, monkeypatch, explicit):
    names = ["bing", " BING ", "Bing"]
    if not explicit:
        monkeypatch.setattr(settings, "default_engines", names)
    assert registry.get_engine("bing") is registry.get_engine(" BING ")
    first = await aggregator.aggregate_search(
        "python async tutorial", engines=names if explicit else None,
        max_results=1, use_cache=True,
    )
    second = await aggregator.aggregate_search(
        "python async tutorial", engines=["bing"], max_results=1, use_cache=True,
    )
    assert identity_world["bing"].calls == 1
    assert list(aggregator.search_limiter._buckets) == ["bing"]
    assert first["engines"] == second["engines"] == ["bing"]
    assert not first["cached"] and second["cached"]
    assert first["cache_key"] == second["cache_key"]
    if not explicit:
        assert settings.default_engines == names


def test_aliases_share_cache_key_without_collapsing_agent_lane_identity():
    filters = SearchFilters()
    assert aggregator._key("q", ["bing", " BING "], 1, filters) == aggregator._key(
        "q", ["bing"], 1, filters,
    )
    assert aggregator._key("q", ["agent:codex"], 1, filters) != aggregator._key(
        "q", ["codex"], 1, filters,
    )


def test_nominal_pool_normalizes_configured_extras(identity_world, monkeypatch):
    monkeypatch.setattr(settings, "default_engines", [" BING ", "bing"])
    monkeypatch.setattr(settings, "fresh_engines", [" DUCKDUCKGO ", "duckduckgo"])
    monkeypatch.setattr(settings, "locale_engines", {"zh": [" YAHOOJP ", "yahoojp"]})
    assert aggregator._nominal_pool("中文搜索", None, "day") == [
        "bing", "duckduckgo", "yahoojp",
    ]


async def test_configured_aliases_respect_canonical_health(identity_world, monkeypatch):
    monkeypatch.setattr(settings, "health_enabled", True)
    monkeypatch.setattr(settings, "min_healthy_engines", 2)
    monkeypatch.setattr(settings, "default_engines", [" BING ", "duckduckgo"])
    monkeypatch.setattr(settings, "reserve_engines", [" YAHOOJP ", "yahoojp"])
    engine_health.record_failure("bing", "captcha")
    result = await aggregator.aggregate_search("python async tutorial", use_cache=False)
    assert identity_world["bing"].calls == 0
    assert identity_world["duckduckgo"].calls == identity_world["yahoojp"].calls == 1
    assert result["engines"] == ["duckduckgo", "yahoojp"]
    assert result["benched_engines"]["bing"]["substitute"] == "yahoojp"


async def test_duplicate_reserve_aliases_do_not_fill_two_seats(identity_world, monkeypatch):
    identity_world.update({name: CountingEngine(name) for name in ("seznam", "quark")})
    monkeypatch.setattr(settings, "health_enabled", True)
    monkeypatch.setattr(settings, "min_healthy_engines", 3)
    monkeypatch.setattr(settings, "default_engines", [" BING ", " DUCKDUCKGO ", "yahoojp"])
    monkeypatch.setattr(settings, "reserve_engines", [" SEZNAM ", "seznam", " QUARK "])
    for name in ("bing", "duckduckgo"):
        engine_health.record_failure(name, "captcha")

    result = await aggregator.aggregate_search("python async tutorial", use_cache=False)

    assert result["engines"] == ["yahoojp", "seznam", "quark"]
    assert identity_world["bing"].calls == identity_world["duckduckgo"].calls == 0
    assert all(identity_world[name].calls == 1 for name in result["engines"])
    assert result["benched_engines"]["bing"]["substitute"] == "seznam"
    assert result["benched_engines"]["duckduckgo"]["substitute"] == "quark"


async def test_rescue_does_not_retry_an_already_requested_alias(identity_world, monkeypatch):
    identity_world["bing"].empty = True
    monkeypatch.setattr(settings, "rescue_enabled", True)
    monkeypatch.setattr(settings, "reserve_engines", [" BING ", " DUCKDUCKGO "])
    monkeypatch.setattr(settings, "rescue_engines", ["duckduckgo"])
    result = await aggregator.aggregate_search(
        "python async tutorial", engines=["bing"], use_cache=False,
    )
    assert identity_world["bing"].calls == 1
    assert identity_world["duckduckgo"].calls == 1
    assert result["rescued_via"] == "duckduckgo"
    assert set(aggregator.search_limiter._buckets) == {"bing", "duckduckgo"}


def test_default_pool_inspection_uses_canonical_names(identity_world, monkeypatch):
    monkeypatch.setattr(settings, "default_engines", [" BING ", "bing"])
    assert aggregator.default_pool_now() == ["bing"]


async def test_default_alias_excludes_the_same_engine_from_the_agent_lane(identity_world, monkeypatch):
    agent = CountingEngine("codex")
    agent.agent_search = True
    agent.agent_lane_auto = True
    identity_world["codex"] = agent
    monkeypatch.setattr(settings, "agent_engines", "auto")
    monkeypatch.setattr(settings, "default_engines", [" CODEX ", "codex"])

    assert aggregator.agent_lane_now() == []
    direct = await aggregator.aggregate_search(
        "python async tutorial", max_results=1, use_cache=True,
    )
    assert agent.calls == 1
    assert list(aggregator.search_limiter._buckets) == ["codex"]
    assert direct["engines"] == ["codex"]
    assert "agent_lane" not in direct

    monkeypatch.setattr(settings, "default_engines", ["bing"])
    with_lane = await aggregator.aggregate_search(
        "python async tutorial", max_results=1, use_cache=True,
    )
    assert not with_lane["cached"]
    assert agent.calls == 2
    assert identity_world["bing"].calls == 1
    assert with_lane["agent_lane"]["engines"] == ["codex"]
    assert set(aggregator.search_limiter._buckets) == {"bing", "codex"}
    assert direct["cache_key"] != with_lane["cache_key"]


async def test_unknown_engine_remains_a_reported_error(identity_world):
    result = await aggregator.aggregate_search(
        "python async tutorial", engines=["missing_engine"], use_cache=False,
    )
    assert "Unknown engine" in result["errors"]["missing_engine"]
    assert not any(engine.calls for engine in identity_world.values())
