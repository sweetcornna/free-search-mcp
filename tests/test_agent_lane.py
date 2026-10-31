"""The agent lane: signed-in agent engines search beside the keyless pool.

`codex` and `antigravity` used to run only when a call named them, and naming
one switched the keyless pool off for that call. The lane asks the signed-in
ones at the same moment as the keyless engines and fuses what they find into
the same ranking, without letting a slow one hold the keyless results back.
"""
from __future__ import annotations

import asyncio
import re
import time

import pytest

from search_mcp import aggregator
from search_mcp import prewarm as prewarm_mod
from search_mcp.aggregator import _configured_lane, aggregate_search
from search_mcp.config import settings
from search_mcp.engines.base import EngineKeyError, SearchResult
from search_mcp.formatting import render_search
from search_mcp.health import engine_health

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.

QUERY = "rust ownership borrowing"


def bucket(engine: str, n: int = 4) -> list[SearchResult]:
    return [
        SearchResult(
            title=f"Ownership and borrowing in Rust, part {i} ({engine})",
            url=f"https://{engine}.example/{i}",
            snippet="Rust ownership and borrowing explained.",
            engine=engine,
            rank=i + 1,
        )
        for i in range(n)
    ]


class Stub:
    categories: frozenset[str] = frozenset()
    single_site = False
    needs_browser = False

    def __init__(self, name, *, delay=0.0, exc=None, results=None, available=True):
        self.name = name
        self.delay = delay
        self.exc = exc
        self.results = bucket(name) if results is None else results
        self.available = available
        self.calls = 0
        self.cancelled = False

    def is_available(self) -> bool:
        return self.available

    async def search(self, query, n, filters=None, diagnostics=None):
        self.calls += 1
        try:
            await asyncio.sleep(self.delay)
        except asyncio.CancelledError:
            self.cancelled = True
            raise
        if self.exc is not None:
            raise self.exc
        if diagnostics is not None:
            diagnostics.setdefault("raw_per_engine", {})[self.name] = len(self.results)
        return list(self.results)


class Cache:
    def __init__(self):
        self.rows: dict[str, tuple[list, dict]] = {}
        self.writes = 0

    async def get_search(self, key, max_age_seconds=None):
        return self.rows.get(key)

    async def put_search(self, key, query, engines, results, meta=None, *, created=None):
        self.writes += 1
        stamp = time.time() if created is None else created
        self.rows[key] = (results, {**(meta or {}), "cached_at": stamp})


@pytest.fixture
def world(monkeypatch):
    stubs: dict[str, Stub] = {}

    def add(*engines: Stub) -> dict[str, Stub]:
        stubs.update({e.name: e for e in engines})
        return stubs

    def _get(name: str):
        try:
            return stubs[name]
        except KeyError:
            raise ValueError(f"unknown engine: {name}") from None

    tokens: dict[str, bool] = {}

    async def _token(name, max_wait=None):
        return tokens.get(name, True)

    monkeypatch.setattr(aggregator, "get_engine", _get)
    monkeypatch.setattr(aggregator, "engines_for_category", lambda category, exclude=(): [])
    monkeypatch.setattr(aggregator.search_limiter, "acquire", _token)
    cache = Cache()
    monkeypatch.setattr(aggregator, "cache", cache)
    monkeypatch.setattr(settings, "default_engines", ["duckduckgo", "bing", "anysearch"])
    monkeypatch.setattr(settings, "reserve_engines", [])
    monkeypatch.setattr(settings, "rescue_engines", [])
    monkeypatch.setattr(settings, "locale_engines", {})
    monkeypatch.setattr(settings, "auto_route_enabled", False)
    add(*(Stub(n) for n in ["duckduckgo", "bing", "anysearch"]))
    add(Stub("codex"), Stub("antigravity"))
    add.cache = cache
    add.stubs = stubs
    add.tokens = tokens
    return add


# ---------------------------------------------------------------------------
# Which agent engines the lane seats
# ---------------------------------------------------------------------------


def test_auto_seats_codex_but_never_antigravity(monkeypatch):
    monkeypatch.setattr(settings, "agent_engines", "auto")
    assert _configured_lane() == ["codex"]


@pytest.mark.parametrize(
    ("value", "expected"),
    [
        ("off", []),
        ("", []),
        ("codex,antigravity", ["codex", "antigravity"]),
        ("antigravity codex", ["antigravity", "codex"]),
        ("auto, antigravity", ["codex", "antigravity"]),
        ("auto codex", ["codex"]),
        ("off codex", []),
        ("CODEX", ["codex"]),
    ],
)
def test_the_setting_names_the_lane(monkeypatch, value, expected):
    monkeypatch.setattr(settings, "agent_engines", value)
    assert _configured_lane() == expected


def test_an_engine_that_is_not_an_agent_engine_is_ignored(monkeypatch, caplog):
    monkeypatch.setattr(settings, "agent_engines", "bing,nonsense,codex")
    assert _configured_lane() == ["codex"]
    assert "not an agent engine" in caplog.text


def test_only_codex_and_antigravity_are_agent_engines():
    from search_mcp.engines import ENGINES

    assert sorted(n for n, e in ENGINES.items() if e.agent_search) == ["antigravity", "codex"]
    assert [n for n, e in ENGINES.items() if e.agent_lane_auto] == ["codex"]


# ---------------------------------------------------------------------------
# A search with the lane
# ---------------------------------------------------------------------------


async def test_without_a_sign_in_nothing_changes(world):
    world.stubs["codex"].available = False
    payload = await aggregate_search(QUERY, use_cache=False)
    assert payload["engines"] == ["duckduckgo", "bing", "anysearch"]
    assert "agent_lane" not in payload
    assert world.stubs["codex"].calls == 0


async def test_a_signed_in_codex_runs_beside_the_keyless_engines(world):
    payload = await aggregate_search(QUERY, use_cache=False)
    assert payload["engines"] == ["duckduckgo", "bing", "anysearch", "codex"]
    contributed = {e for r in payload["results"] for e in r["engines"]}
    assert "codex" in contributed
    assert payload["agent_lane"] == {"engines": ["codex"], "answered": {"codex": 4}}
    assert "agent_lane_hint" not in payload
    assert "errors" not in payload or not payload["errors"]


async def test_the_lane_starts_with_the_keyless_engines_not_after_them(world):
    for name in ("duckduckgo", "bing", "anysearch", "codex"):
        world.stubs[name].delay = 0.3
    started = time.monotonic()
    payload = await aggregate_search(QUERY, use_cache=False)
    took = time.monotonic() - started
    assert payload["agent_lane"]["answered"] == {"codex": 4}
    assert took < 0.55, f"lane ran after the pool: {took:.2f} s"


async def test_a_slow_lane_engine_cannot_hold_the_keyless_results(world, monkeypatch):
    monkeypatch.setattr(settings, "agent_lane_wait_seconds", 0.1)
    world.stubs["codex"].delay = 0.6
    started = time.monotonic()
    payload = await aggregate_search(QUERY)
    assert time.monotonic() - started < 0.4
    contributed = {e for r in payload["results"] for e in r["engines"]}
    assert "codex" not in contributed
    assert payload["engines"] == ["duckduckgo", "bing", "anysearch"]
    assert payload["agent_lane"]["pending"] == ["codex"]
    assert payload["agent_lane"]["merges_into_cache"] is True
    assert re.search(r"codex had not answered within 0\.[12] s", payload["agent_lane_hint"])
    assert "repeat the same call" in payload["agent_lane_hint"]
    assert "Agent lane:" in render_search(payload)

    # It keeps searching, and its results land in the cached row.
    await asyncio.sleep(0.8)
    assert not aggregator._lane_background
    rows = list(world.cache.rows.values())
    assert len(rows) == 1
    results, meta = rows[0]
    assert "codex" in {e for r in results for e in r["engines"]}
    assert meta["engines"] == ["duckduckgo", "bing", "anysearch", "codex"]

    # The next identical search is a cache hit carrying the fused set.
    again = await aggregate_search(QUERY)
    assert again["cached"] is True
    assert "codex" in {e for r in again["results"] for e in r["engines"]}
    assert world.stubs["codex"].calls == 1


async def test_without_the_cache_a_late_lane_engine_is_cancelled(world, monkeypatch):
    monkeypatch.setattr(settings, "agent_lane_wait_seconds", 0.05)
    world.stubs["codex"].delay = 5
    payload = await aggregate_search(QUERY, use_cache=False)
    assert payload["agent_lane"]["merges_into_cache"] is False
    assert "repeat the same call" not in payload["agent_lane_hint"]
    await asyncio.sleep(0)
    assert world.stubs["codex"].cancelled
    assert not aggregator._lane_background


async def test_the_wait_stretches_when_the_keyless_engines_found_nothing(world, monkeypatch):
    monkeypatch.setattr(settings, "agent_lane_wait_seconds", 0.05)
    monkeypatch.setattr(settings, "search_deadline_seconds", 2.0)
    for name in ("duckduckgo", "bing", "anysearch"):
        world.stubs[name].results = []
    world.stubs["codex"].delay = 0.3
    payload = await aggregate_search(QUERY, use_cache=False)
    assert payload["results"]
    assert {e for r in payload["results"] for e in r["engines"]} == {"codex"}
    assert "pending" not in payload["agent_lane"]


async def test_a_transient_lane_failure_takes_two_strikes(world):
    world.stubs["codex"].exc = RuntimeError("connection reset")
    await aggregate_search(QUERY, use_cache=False)
    assert not engine_health.is_open("codex")
    await aggregate_search(QUERY, use_cache=False)
    assert engine_health.is_open("codex")


async def test_a_failing_lane_engine_is_benched_and_reported_apart(world):
    world.stubs["codex"].exc = EngineKeyError("codex: the plan's usage limit was reached")
    payload = await aggregate_search(QUERY, use_cache=False)
    assert payload["results"]
    assert not payload.get("errors")
    assert payload["agent_lane"]["errors"] == {
        "codex": "codex: the plan's usage limit was reached"
    }
    assert "sits out the next searches" in payload["agent_lane_hint"]
    assert engine_health.is_open("codex")

    again = await aggregate_search(QUERY, use_cache=False)
    assert again["agent_lane"] == {"engines": [], "answered": {}, "benched": ["codex"]}
    assert world.stubs["codex"].calls == 1


async def test_a_lane_engine_out_of_tokens_sits_the_search_out(world):
    world.tokens["codex"] = False
    payload = await aggregate_search(QUERY, use_cache=False)
    assert payload["agent_lane"]["rate_limited"] == ["codex"]
    assert world.stubs["codex"].calls == 0
    assert not engine_health.is_open("codex")
    assert "per-minute limit" in payload["agent_lane_hint"]
    assert "rate_limited_engines" not in payload


async def test_naming_engines_keeps_the_lane_out(world):
    payload = await aggregate_search(QUERY, engines=["duckduckgo"], use_cache=False)
    assert payload["engines"] == ["duckduckgo"]
    assert "agent_lane" not in payload
    assert world.stubs["codex"].calls == 0


async def test_an_image_search_keeps_the_lane_out(world, monkeypatch):
    monkeypatch.setattr(aggregator, "engines_for_category", lambda c, exclude=(): ["duckduckgo"])
    payload = await aggregate_search(QUERY, category="image", use_cache=False)
    assert "agent_lane" not in payload
    assert world.stubs["codex"].calls == 0


async def test_switched_off_the_lane_is_never_asked(world, monkeypatch):
    monkeypatch.setattr(settings, "agent_engines", "off")
    payload = await aggregate_search(QUERY, use_cache=False)
    assert "agent_lane" not in payload
    assert world.stubs["codex"].calls == 0


async def test_antigravity_joins_only_when_listed(world, monkeypatch):
    payload = await aggregate_search(QUERY, use_cache=False)
    assert world.stubs["antigravity"].calls == 0
    monkeypatch.setattr(settings, "agent_engines", "codex,antigravity")
    payload = await aggregate_search(QUERY, use_cache=False)
    assert payload["agent_lane"]["engines"] == ["codex", "antigravity"]
    assert world.stubs["antigravity"].calls == 1


async def test_the_cache_key_includes_the_lane(world):
    world.stubs["codex"].available = False
    await aggregate_search(QUERY)
    world.stubs["codex"].available = True
    payload = await aggregate_search(QUERY)
    # Signing in makes the same question a different search, not a replay of
    # the keyless-only answer.
    assert payload["cached"] is False
    assert len(world.cache.rows) == 2


async def test_a_cached_replay_names_the_lane(world):
    await aggregate_search(QUERY)
    again = await aggregate_search(QUERY)
    assert again["cached"] is True
    assert again["engines"] == ["duckduckgo", "bing", "anysearch", "codex"]


async def test_a_cancelled_search_cancels_its_lane(world):
    for name in ("duckduckgo", "bing", "anysearch", "codex"):
        world.stubs[name].delay = 5
    task = asyncio.ensure_future(aggregate_search(QUERY, use_cache=False))
    await asyncio.sleep(0.05)
    task.cancel()
    with pytest.raises(asyncio.CancelledError):
        await task
    await asyncio.sleep(0)
    assert world.stubs["codex"].cancelled


async def test_an_off_topic_lane_bucket_is_dropped(world):
    world.stubs["codex"].results = [
        SearchResult(
            title=f"Rust (video game) store page {i}",
            url=f"https://store.example/{i}",
            snippet="Survive the island. Rust is a multiplayer survival game.",
            engine="codex",
            rank=i + 1,
        )
        for i in range(5)
    ]
    payload = await aggregate_search(QUERY, use_cache=False)
    assert "codex" not in {e for r in payload["results"] for e in r["engines"]}
    assert payload["gated_engines"]["codex"]["reason"] == "off_topic"
    assert payload["agent_lane"]["answered"] == {}
    assert engine_health.is_open("codex")


# ---------------------------------------------------------------------------
# Warm-up
# ---------------------------------------------------------------------------


async def test_prewarm_warms_the_signed_in_lane(monkeypatch):
    monkeypatch.setattr(settings, "prewarm", True)
    warmed: list[str] = []

    async def codex_warm(self):
        warmed.append("codex")

    monkeypatch.setattr("search_mcp.engines.codex.CodexEngine.warm", codex_warm)
    monkeypatch.setattr("search_mcp.aggregator.default_pool_now", lambda: ["duckduckgo"])
    monkeypatch.setattr("search_mcp.aggregator.agent_lane_now", lambda: ["codex"])
    await prewarm_mod.prewarm()
    assert warmed == ["codex"]


async def test_codex_warm_does_nothing_without_a_sign_in():
    from search_mcp.engines.codex import CodexEngine

    # The suite's config directory holds no sign-in, so this must return
    # without touching the network or opening a sign-in page.
    await CodexEngine().warm()


def test_agent_lane_now_is_empty_without_a_sign_in():
    assert aggregator.agent_lane_now() == []


# ---------------------------------------------------------------------------
# An empty keyless run rescues beside the lane wait, not after it
# ---------------------------------------------------------------------------


async def test_an_empty_keyless_run_rescues_while_the_lane_is_searching(world, monkeypatch):
    monkeypatch.setattr(settings, "rescue_enabled", True)
    monkeypatch.setattr(settings, "reserve_engines", ["so360"])
    monkeypatch.setattr(settings, "agent_lane_wait_seconds", 0.4)
    monkeypatch.setattr(settings, "search_deadline_seconds", 0.4)
    for name in ("duckduckgo", "bing", "anysearch"):
        world.stubs[name].results = []
    world(Stub("so360", delay=0.3))
    world.stubs["codex"].delay = 5
    started = time.monotonic()
    payload = await aggregate_search(QUERY, use_cache=False)
    took = time.monotonic() - started
    assert payload["rescued_via"] == "so360"
    # 0.4 s of lane wait with the 0.3 s rescue inside it, not 0.7 s.
    assert took < 0.65, f"{took:.2f} s"


async def test_an_early_rescue_the_lane_made_unnecessary_is_cancelled(world, monkeypatch):
    monkeypatch.setattr(settings, "rescue_enabled", True)
    monkeypatch.setattr(settings, "reserve_engines", ["so360"])
    monkeypatch.setattr(settings, "search_deadline_seconds", 2.0)
    for name in ("duckduckgo", "bing", "anysearch"):
        world.stubs[name].results = []
    world.stubs["codex"].results = bucket("codex", 6)
    world.stubs["codex"].delay = 0.1
    world(Stub("so360", delay=3))
    payload = await aggregate_search(QUERY, use_cache=False)
    assert "rescued_via" not in payload
    assert {e for r in payload["results"] for e in r["engines"]} == {"codex"}
    # Two hops: the rescue task, then the probe it was waiting on.
    await asyncio.sleep(0.05)
    assert world.stubs["so360"].cancelled


# ---------------------------------------------------------------------------
# Review round 1
# ---------------------------------------------------------------------------


async def test_a_rescued_answer_ends_the_stretched_lane_wait(world, monkeypatch):
    monkeypatch.setattr(settings, "rescue_enabled", True)
    monkeypatch.setattr(settings, "reserve_engines", ["so360"])
    monkeypatch.setattr(settings, "agent_lane_wait_seconds", 0.2)
    monkeypatch.setattr(settings, "search_deadline_seconds", 3.0)
    for name in ("duckduckgo", "bing", "anysearch"):
        world.stubs[name].results = []
    world(Stub("so360", delay=0.05))
    world.stubs["codex"].delay = 5
    started = time.monotonic()
    payload = await aggregate_search(QUERY, use_cache=False)
    took = time.monotonic() - started
    assert payload["rescued_via"] == "so360"
    # The lane's ordinary 0.2 s, not the 3 s deadline the empty run stretched to.
    assert took < 0.6, f"{took:.2f} s"
    assert payload["agent_lane"]["pending"] == ["codex"]


async def test_a_run_missing_its_lane_engine_is_replayed_for_an_hour_at_most(world):
    world.stubs["codex"].exc = RuntimeError("upstream reset")
    await aggregate_search(QUERY)
    (results, meta), = world.cache.rows.values()
    assert meta["lane_missing"] == ["codex"]
    assert not aggregator._too_degraded_to_replay(meta)
    assert aggregator._too_degraded_to_replay({**meta, "cached_at": time.time() - 3700})


async def test_a_complete_lane_leaves_the_row_unmarked(world):
    await aggregate_search(QUERY)
    (_, meta), = world.cache.rows.values()
    assert "lane_missing" not in meta


async def test_the_late_merge_clears_the_mark_once_the_lane_is_in(world, monkeypatch):
    monkeypatch.setattr(settings, "agent_lane_wait_seconds", 0.05)
    world.stubs["codex"].delay = 0.2
    await aggregate_search(QUERY)
    (_, meta), = world.cache.rows.values()
    assert meta["lane_missing"] == ["codex"]
    await asyncio.sleep(0.4)
    (results, meta), = world.cache.rows.values()
    assert "lane_missing" not in meta
    assert "codex" in {e for r in results for e in r["engines"]}


async def test_keyless_engines_take_no_silent_strike_from_a_lane_find(world):
    for name in ("duckduckgo", "bing", "anysearch"):
        world.stubs[name].results = []
    for _ in range(4):
        await aggregate_search(QUERY, use_cache=False)
    assert not any(engine_health.is_open(n) for n in ("duckduckgo", "bing", "anysearch"))


async def test_a_late_merge_never_overwrites_a_newer_row(world, monkeypatch):
    monkeypatch.setattr(settings, "agent_lane_wait_seconds", 0.05)
    world.stubs["codex"].delay = 0.3
    await aggregate_search(QUERY)  # leaves codex merging in the background
    # A forced refresh of the same search, whose codex is out of tokens.
    world.tokens["codex"] = False
    world.stubs["duckduckgo"].results = bucket("fresh", 4)
    await aggregate_search(QUERY, max_age_seconds=0)
    await asyncio.sleep(0.5)
    (results, _), = world.cache.rows.values()
    engines_found = {e for r in results for e in r["engines"]}
    assert "codex" not in engines_found, "the first search's late merge overwrote the refresh"
    assert any(r["url"].startswith("https://fresh.example/") for r in results)
    assert not aggregator._lane_rows


# ---------------------------------------------------------------------------
# Review round 2
# ---------------------------------------------------------------------------


async def test_a_lane_answer_ends_the_wait_for_empty_stragglers(world, monkeypatch):
    monkeypatch.setattr(settings, "search_deadline_seconds", 0.1)
    for name in ("duckduckgo", "bing", "anysearch"):
        world.stubs[name].results = []
    world.stubs["anysearch"].delay = 3
    world.stubs["codex"].delay = 0.2
    started = time.monotonic()
    payload = await aggregate_search(QUERY, use_cache=False)
    took = time.monotonic() - started
    assert {e for r in payload["results"] for e in r["engines"]} == {"codex"}
    assert payload["timed_out_engines"] == ["anysearch"]
    assert took < 1.5, f"{took:.2f} s"


async def test_a_late_empty_answer_completes_the_lane(world, monkeypatch):
    monkeypatch.setattr(settings, "agent_lane_wait_seconds", 0.05)
    world.stubs["codex"].delay = 0.2
    world.stubs["codex"].results = []
    await aggregate_search(QUERY)
    (_, meta), = world.cache.rows.values()
    assert meta["lane_missing"] == ["codex"]
    await asyncio.sleep(0.4)
    (_, meta), = world.cache.rows.values()
    assert "lane_missing" not in meta
    assert meta["engines"][-1] == "codex"


async def test_an_early_rescue_that_came_back_is_used(world, monkeypatch):
    monkeypatch.setattr(settings, "rescue_enabled", True)
    monkeypatch.setattr(settings, "reserve_engines", ["so360"])
    monkeypatch.setattr(settings, "search_deadline_seconds", 2.0)
    for name in ("duckduckgo", "bing", "anysearch"):
        world.stubs[name].results = []
    world(Stub("so360", delay=0.02))
    world.stubs["codex"].results = bucket("codex", 6)
    world.stubs["codex"].delay = 0.2
    payload = await aggregate_search(QUERY, use_cache=False)
    assert payload["rescued_via"] == "so360"
    assert {e for r in payload["results"] for e in r["engines"]} == {"codex", "so360"}


async def test_a_cached_set_without_its_lane_engine_says_so(world):
    world.stubs["codex"].exc = RuntimeError("upstream reset")
    await aggregate_search(QUERY)
    world.stubs["codex"].exc = None
    again = await aggregate_search(QUERY)
    assert again["cached"] is True
    assert again["agent_lane"] == {"missing": ["codex"]}
    assert "cached without codex" in again["agent_lane_hint"]
    assert "Agent lane:" in render_search(again)


# ---------------------------------------------------------------------------
# Review round 3
# ---------------------------------------------------------------------------


async def test_a_lane_answer_that_beat_a_slow_pool_is_kept(world, monkeypatch):
    monkeypatch.setattr(settings, "agent_lane_wait_seconds", 0.1)
    world.stubs["duckduckgo"].delay = 0.3
    world.stubs["codex"].delay = 0.05
    payload = await aggregate_search(QUERY, use_cache=False)
    assert "codex" in {e for r in payload["results"] for e in r["engines"]}
    assert payload["agent_lane"] == {"engines": ["codex"], "answered": {"codex": 4}}
    assert "agent_lane_hint" not in payload


# ---------------------------------------------------------------------------
# Review round 4
# ---------------------------------------------------------------------------


async def test_a_lasting_lane_failure_does_not_shorten_the_cache(world):
    world.stubs["codex"].exc = EngineKeyError("codex: the plan's usage limit was reached")
    await aggregate_search(QUERY)
    (_, meta), = world.cache.rows.values()
    assert "lane_missing" not in meta
    # Benched now: the next search's row is not marked either.
    await aggregate_search(QUERY, max_age_seconds=0)
    (_, meta), = world.cache.rows.values()
    assert "lane_missing" not in meta


async def test_one_lane_answer_ends_the_stretched_wait(world, monkeypatch):
    monkeypatch.setattr(settings, "agent_engines", "codex,antigravity")
    monkeypatch.setattr(settings, "agent_lane_wait_seconds", 0.2)
    monkeypatch.setattr(settings, "search_deadline_seconds", 3.0)
    for name in ("duckduckgo", "bing", "anysearch"):
        world.stubs[name].results = []
    world.stubs["codex"].delay = 0.05
    world.stubs["antigravity"].delay = 5
    started = time.monotonic()
    payload = await aggregate_search(QUERY, use_cache=False)
    took = time.monotonic() - started
    assert {e for r in payload["results"] for e in r["engines"]} == {"codex"}
    assert payload["agent_lane"]["pending"] == ["antigravity"]
    assert took < 0.6, f"{took:.2f} s"


def _decoy(engine: str) -> list[SearchResult]:
    return [
        SearchResult(
            title=f"Rust (video game) store page {i}",
            url=f"https://store-{engine}.example/{i}",
            snippet="Survive the island. Rust is a multiplayer survival game.",
            engine=engine,
            rank=i + 1,
        )
        for i in range(6)
    ]


async def test_a_late_lane_answer_is_the_witness_that_drops_a_decoy(world, monkeypatch):
    monkeypatch.setattr(settings, "agent_lane_wait_seconds", 0.05)
    world.stubs["bing"].results = _decoy("bing")
    world.stubs["duckduckgo"].results = []
    world.stubs["anysearch"].results = []
    world.stubs["codex"].delay = 0.2
    first = await aggregate_search(QUERY)
    # No witness yet: the suspect is kept for now.
    assert "bing" in {e for r in first["results"] for e in r["engines"]}
    await asyncio.sleep(0.4)
    (results, meta), = world.cache.rows.values()
    assert {e for r in results for e in r["engines"]} == {"codex"}
    assert meta["gated_engines"]["bing"]["reason"] == "off_topic"
    assert engine_health.is_open("bing")


# ---------------------------------------------------------------------------
# Review round 5
# ---------------------------------------------------------------------------


async def test_a_lane_answer_during_the_rescue_wait_is_kept(world, monkeypatch):
    monkeypatch.setattr(settings, "rescue_enabled", True)
    monkeypatch.setattr(settings, "reserve_engines", ["so360"])
    monkeypatch.setattr(settings, "agent_lane_wait_seconds", 0.1)
    monkeypatch.setattr(settings, "search_deadline_seconds", 0.1)
    for name in ("duckduckgo", "bing", "anysearch"):
        world.stubs[name].results = []
    world(Stub("so360", delay=0.6))
    world.stubs["codex"].delay = 0.3
    payload = await aggregate_search(QUERY, use_cache=False)
    assert "codex" in {e for r in payload["results"] for e in r["engines"]}
    assert "pending" not in payload["agent_lane"]


async def test_an_off_topic_lane_bucket_does_not_cut_the_keyless_wait(world, monkeypatch):
    monkeypatch.setattr(settings, "search_deadline_seconds", 0.1)
    world.stubs["duckduckgo"].results = []
    world.stubs["bing"].results = []
    world.stubs["anysearch"].delay = 0.4
    world.stubs["codex"].results = _decoy("codex")
    world.stubs["codex"].delay = 0.02
    payload = await aggregate_search(QUERY, use_cache=False)
    assert "anysearch" in {e for r in payload["results"] for e in r["engines"]}
    assert "timed_out_engines" not in payload


async def test_no_early_rescue_once_a_lane_engine_answered(world, monkeypatch):
    monkeypatch.setattr(settings, "agent_engines", "codex,antigravity")
    monkeypatch.setattr(settings, "rescue_enabled", True)
    monkeypatch.setattr(settings, "reserve_engines", ["so360"])
    monkeypatch.setattr(settings, "agent_lane_wait_seconds", 0.2)
    for name in ("duckduckgo", "bing", "anysearch"):
        world.stubs[name].results = []
        world.stubs[name].delay = 0.1
    world(Stub("so360"))
    world.stubs["codex"].results = bucket("codex", 6)
    world.stubs["antigravity"].delay = 5
    payload = await aggregate_search(QUERY, use_cache=False)
    assert world.stubs["so360"].calls == 0
    assert "rescued_via" not in payload


async def test_the_hint_says_when_a_failed_lane_engine_is_asked_again(world):
    world.stubs["codex"].exc = RuntimeError("connection reset")
    payload = await aggregate_search(QUERY, use_cache=False)
    assert "is asked again on the next search" in payload["agent_lane_hint"]


async def test_a_late_lasting_failure_does_not_shorten_the_row(world, monkeypatch):
    monkeypatch.setattr(settings, "agent_lane_wait_seconds", 0.05)
    world.stubs["codex"].delay = 0.2
    world.stubs["codex"].exc = EngineKeyError("codex: the plan's usage limit was reached")
    await aggregate_search(QUERY)
    (_, meta), = world.cache.rows.values()
    assert meta["lane_missing"] == ["codex"]
    await asyncio.sleep(0.4)
    (_, meta), = world.cache.rows.values()
    assert "lane_missing" not in meta


# ---------------------------------------------------------------------------
# Review round 6
# ---------------------------------------------------------------------------


def _transient(message: str) -> EngineKeyError:
    return EngineKeyError(message, transient=True)


async def test_a_transient_sign_in_failure_is_a_passing_one(world):
    world.stubs["codex"].exc = _transient("codex: could not reach auth.openai.com")
    await aggregate_search(QUERY)
    assert not engine_health.is_open("codex")
    (_, meta), = world.cache.rows.values()
    assert meta["lane_missing"] == ["codex"]


async def test_a_lane_engine_benched_after_passing_failures_still_marks_the_row(world):
    world.stubs["codex"].exc = RuntimeError("connection reset")
    for _ in range(2):
        await aggregate_search(QUERY, use_cache=False)
    assert engine_health.is_open("codex")
    await aggregate_search(QUERY)
    (_, meta), = world.cache.rows.values()
    assert meta["lane_missing"] == ["codex"]


async def test_the_rescue_does_not_hold_a_search_a_lane_answer_settled(world, monkeypatch):
    monkeypatch.setattr(settings, "agent_engines", "codex,antigravity")
    monkeypatch.setattr(settings, "rescue_enabled", True)
    monkeypatch.setattr(settings, "reserve_engines", ["so360"])
    monkeypatch.setattr(settings, "agent_lane_wait_seconds", 0.3)
    monkeypatch.setattr(settings, "search_deadline_seconds", 2.0)
    for name in ("duckduckgo", "bing", "anysearch"):
        world.stubs[name].results = []
    world(Stub("so360", delay=3))
    world.stubs["codex"].results = bucket("codex", 6)
    world.stubs["codex"].delay = 0.1
    world.stubs["antigravity"].delay = 5
    started = time.monotonic()
    payload = await aggregate_search(QUERY, use_cache=False)
    took = time.monotonic() - started
    assert {e for r in payload["results"] for e in r["engines"]} == {"codex"}
    assert took < 1.0, f"{took:.2f} s"


async def test_a_late_off_topic_answer_still_completes_the_lane(world, monkeypatch):
    monkeypatch.setattr(settings, "agent_lane_wait_seconds", 0.05)
    world.stubs["codex"].delay = 0.2
    world.stubs["codex"].results = _decoy("codex")
    await aggregate_search(QUERY)
    await asyncio.sleep(0.4)
    (results, meta), = world.cache.rows.values()
    assert "lane_missing" not in meta
    assert "codex" not in {e for r in results for e in r["engines"]}
    assert engine_health.is_open("codex")


# ---------------------------------------------------------------------------
# Review round 7
# ---------------------------------------------------------------------------


async def test_a_quick_pool_gives_the_lane_only_its_grace(world, monkeypatch):
    monkeypatch.setattr(settings, "agent_lane_wait_seconds", 5.0)
    monkeypatch.setattr(settings, "agent_lane_grace_seconds", 0.1)
    world.stubs["codex"].delay = 2
    started = time.monotonic()
    payload = await aggregate_search(QUERY, use_cache=False)
    assert time.monotonic() - started < 0.6
    assert payload["agent_lane"]["pending"] == ["codex"]


async def test_a_late_witness_that_thins_the_pool_marks_the_row_degraded(world, monkeypatch):
    monkeypatch.setattr(settings, "agent_lane_wait_seconds", 0.05)
    monkeypatch.setattr(
        settings, "default_engines", ["duckduckgo", "bing", "anysearch", "yahoojp"]
    )
    world(Stub("yahoojp", results=bucket("yahoojp", 2)))
    # Too few results to judge, so neither a decoy nor a witness.
    world.stubs["duckduckgo"].results = bucket("duckduckgo", 2)
    world.stubs["bing"].results = _decoy("bing")
    world.stubs["anysearch"].results = _decoy("anysearch")
    world.stubs["codex"].delay = 0.2
    await aggregate_search(QUERY)
    (_, meta), = world.cache.rows.values()
    assert "degraded" not in meta
    await asyncio.sleep(0.4)
    (results, meta), = world.cache.rows.values()
    assert "bing" not in {e for r in results for e in r["engines"]}
    assert meta["degraded"] is True


# ---------------------------------------------------------------------------
# Review round 8
# ---------------------------------------------------------------------------


async def test_a_lane_answer_never_lengthens_the_grace(world, monkeypatch):
    monkeypatch.setattr(settings, "agent_engines", "codex,antigravity")
    monkeypatch.setattr(settings, "agent_lane_wait_seconds", 3.0)
    monkeypatch.setattr(settings, "agent_lane_grace_seconds", 0.2)
    world.stubs["codex"].delay = 0.1
    world.stubs["antigravity"].delay = 5
    started = time.monotonic()
    payload = await aggregate_search(QUERY, use_cache=False)
    assert time.monotonic() - started < 0.8
    assert payload["agent_lane"]["pending"] == ["antigravity"]


async def test_a_late_merge_keeps_the_row_s_original_time(world, monkeypatch):
    monkeypatch.setattr(settings, "agent_lane_wait_seconds", 0.05)
    world.stubs["codex"].delay = 1.2
    await aggregate_search(QUERY)
    (_, first), = world.cache.rows.values()
    await asyncio.sleep(1.5)
    (results, meta), = world.cache.rows.values()
    assert "codex" in {e for r in results for e in r["engines"]}
    assert int(meta["cached_at"]) == int(first["cached_at"])


async def test_a_late_failure_is_listed_like_one_in_time(world, monkeypatch):
    monkeypatch.setattr(settings, "agent_lane_wait_seconds", 0.05)
    world.stubs["codex"].delay = 0.2
    world.stubs["codex"].exc = RuntimeError("connection reset")
    await aggregate_search(QUERY)
    await asyncio.sleep(0.4)
    (_, meta), = world.cache.rows.values()
    assert meta["engines"][-1] == "codex"
    assert meta["lane_missing"] == ["codex"]


async def test_a_late_off_topic_answer_is_recorded_as_a_gate(world, monkeypatch):
    monkeypatch.setattr(settings, "agent_lane_wait_seconds", 0.05)
    world.stubs["codex"].delay = 0.2
    world.stubs["codex"].results = _decoy("codex")
    await aggregate_search(QUERY)
    await asyncio.sleep(0.4)
    (_, meta), = world.cache.rows.values()
    assert meta["gated_engines"]["codex"]["reason"] == "off_topic"


# ---------------------------------------------------------------------------
# Review round 9
# ---------------------------------------------------------------------------


async def test_a_named_search_with_the_same_key_takes_the_row_from_a_late_merge(world, monkeypatch):
    monkeypatch.setattr(settings, "agent_lane_wait_seconds", 0.05)
    world.stubs["codex"].delay = 0.3
    await aggregate_search(QUERY)  # leaves codex merging in the background
    # Naming the pool plus codex is the same cache key; it writes afresh.
    world.stubs["duckduckgo"].results = bucket("fresh", 4)
    world.stubs["codex"].delay = 0
    await aggregate_search(
        QUERY, engines=["duckduckgo", "bing", "anysearch", "codex"], max_age_seconds=0
    )
    await asyncio.sleep(0.5)
    (results, _), = world.cache.rows.values()
    assert any(r["url"].startswith("https://fresh.example/") for r in results)
    assert world.cache.writes == 2


async def test_an_off_topic_lane_bucket_alone_does_not_start_a_rescue(world, monkeypatch):
    monkeypatch.setattr(settings, "rescue_enabled", True)
    monkeypatch.setattr(settings, "reserve_engines", ["so360"])
    world(Stub("so360"))
    for name in ("duckduckgo", "bing", "anysearch"):
        world.stubs[name].results = bucket(name, 2)  # healthy, too few to judge
    world.stubs["codex"].results = _decoy("codex")
    payload = await aggregate_search(QUERY, use_cache=False)
    assert world.stubs["so360"].calls == 0
    assert "rescued_via" not in payload


async def test_stragglers_cut_by_a_lane_answer_do_not_start_a_rescue(world, monkeypatch):
    monkeypatch.setattr(settings, "rescue_enabled", True)
    monkeypatch.setattr(settings, "reserve_engines", ["so360"])
    monkeypatch.setattr(settings, "search_deadline_seconds", 0.1)
    world(Stub("so360"))
    for name in ("duckduckgo", "bing", "anysearch"):
        world.stubs[name].delay = 2
    world.stubs["codex"].results = bucket("codex", 6)
    payload = await aggregate_search(QUERY, use_cache=False)
    assert world.stubs["so360"].calls == 0
    assert {e for r in payload["results"] for e in r["engines"]} == {"codex"}


def test_the_late_ceiling_follows_the_engine_timeouts(monkeypatch):
    assert aggregator._late_ceiling() == 150.0
    monkeypatch.setattr(settings, "antigravity_timeout", 200.0)
    assert aggregator._late_ceiling() == 430.0
