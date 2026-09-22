"""Which engines a search asks, and what it admits to afterwards.

The pool used to be a fixed list asked in full every time. These tests pin the
replacement: a nominal pool (what the search is entitled to, and what the cache
is keyed on), an active pool (minus benched engines, plus a reserve only when
the pool is actually thin), and a payload that says which is which.
"""
from __future__ import annotations

import pytest

from search_mcp import aggregator
from search_mcp.aggregator import _active_pool, _nominal_pool, aggregate_search
from search_mcp.config import settings
from search_mcp.engines.base import SearchResult
from search_mcp.health import engine_health

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.

QUERY = "rust ownership borrowing"


def bucket(engine: str, n: int = 6) -> list[SearchResult]:
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
    def __init__(self, name, results=None, *, gate=None, exc=None, categories=frozenset(),
                 needs_browser=False, available=True):  # fmt: skip
        self.name = name
        self.categories = categories
        self.needs_browser = needs_browser
        self._available = available
        self._results = bucket(name) if results is None else results
        self._gate = gate
        self._exc = exc
        self.calls = 0

    def is_available(self) -> bool:
        return self._available

    async def search(self, query, n, filters=None, diagnostics=None):
        self.calls += 1
        if self._exc is not None:
            raise self._exc
        results = [] if self._gate else list(self._results)
        if diagnostics is not None:
            diagnostics.setdefault("raw_per_engine", {})[self.name] = len(results)
            diagnostics.setdefault("after_filter_per_engine", {})[self.name] = len(results)
            if self._gate:
                diagnostics.setdefault("gated", {})[self.name] = self._gate
        return results


class Cache:
    def __init__(self):
        self.rows: dict[str, tuple[list, dict]] = {}

    async def get_search(self, key, max_age_seconds=None):
        return self.rows.get(key)

    async def put_search(self, key, query, engines, results, meta=None):
        import time

        self.rows[key] = (results, {**(meta or {}), "cached_at": time.time()})


@pytest.fixture
def world(monkeypatch):
    """A registry of stub engines under the real default configuration."""
    stubs: dict[str, Stub] = {}

    def add(*engines: Stub) -> dict[str, Stub]:
        stubs.update({e.name: e for e in engines})
        return stubs

    def _get(name: str):
        try:
            return stubs[name]
        except KeyError:
            raise ValueError(f"unknown engine: {name}") from None

    async def _token(name, max_wait=None):
        # The limiter is process-wide and refills at one token per two seconds;
        # these tests run the same engine names many times over and would
        # otherwise spend most of a minute queueing for tokens.
        return True

    monkeypatch.setattr(aggregator, "get_engine", _get)
    monkeypatch.setattr(aggregator, "engines_for_category", lambda category, exclude=(): [])
    monkeypatch.setattr(aggregator.search_limiter, "acquire", _token)
    cache = Cache()
    monkeypatch.setattr(aggregator, "cache", cache)
    monkeypatch.setattr(settings, "default_engines", ["duckduckgo", "bing", "anysearch", "mojeek"])
    monkeypatch.setattr(settings, "reserve_engines", ["so360", "brave", "searx"])
    add(*(Stub(n) for n in ["duckduckgo", "bing", "anysearch", "mojeek", "so360", "brave", "searx",
                            "googlenews"]))  # fmt: skip
    add.cache = cache
    add.stubs = stubs
    return add


# ---------------------------------------------------------------------------
# The nominal pool
# ---------------------------------------------------------------------------


def test_the_default_pool_is_four_general_web_engines_and_no_news_feed():
    assert _nominal_pool("python asyncio taskgroup", None, None) == [
        "duckduckgo", "bing", "anysearch", "mojeek",
    ]  # fmt: skip


@pytest.mark.parametrize(("freshness", "joins"), [("day", True), ("week", True), ("month", False),
                                                   ("year", False), (None, False)])  # fmt: skip
def test_the_news_feed_joins_only_when_recency_was_asked_for(freshness, joins):
    assert ("googlenews" in _nominal_pool("fed rate decision", None, freshness)) is joins


@pytest.mark.parametrize(
    ("query", "joins"),
    [
        ("西湖 龙井 采摘 时间", True),
        ("RoboMaster 2026 机甲大师 规则手册", True),
        ("python asyncio taskgroup", False),
        ("人工知能ニュース", False),  # Japanese: kana decides, Han alone does not
    ],
)
def test_a_chinese_query_also_asks_a_chinese_index(query, joins):
    assert ("so360" in _nominal_pool(query, None, None)) is joins


def test_the_configured_region_does_not_add_a_locale_engine(monkeypatch):
    """An operator in cn-zh typing an English query wants the English web."""
    monkeypatch.setattr(settings, "region", "cn-zh")
    assert "so360" not in _nominal_pool("python asyncio taskgroup", None, None)


# ---------------------------------------------------------------------------
# The active pool
# ---------------------------------------------------------------------------


def test_one_benched_engine_is_dropped_and_not_replaced(world):
    """Three healthy indexes are enough. A reserve here would add latency — for
    `brave`, a browser render — to every search, to restore a number."""
    engine_health.record_failure("mojeek", "captcha")
    nominal = _nominal_pool(QUERY, None, None)

    active, benched = _active_pool(nominal)

    assert active == ["duckduckgo", "bing", "anysearch"]
    assert benched["mojeek"]["reason"] == "captcha"
    assert benched["mojeek"]["substitute"] is None


def test_a_thin_pool_gets_reserves_in_order_up_to_the_minimum(world):
    engine_health.record_failure("mojeek", "captcha")
    engine_health.record_failure("bing", "off_topic")

    active, benched = _active_pool(_nominal_pool(QUERY, None, None))

    assert active == ["duckduckgo", "anysearch", "so360"]
    assert [benched[n]["substitute"] for n in ("bing", "mojeek")].count("so360") == 1


def test_a_reserve_that_is_benched_unavailable_or_cannot_render_is_skipped(world, monkeypatch):
    for name in ("mojeek", "bing", "so360"):
        engine_health.record_failure(name, "captcha")
    world.stubs["brave"].needs_browser = True
    monkeypatch.setattr(type(aggregator.browser_pool), "known_unavailable", property(lambda s: True))

    active, _ = _active_pool(_nominal_pool(QUERY, None, None))

    assert active == ["duckduckgo", "anysearch", "searx"]


def test_with_no_general_engine_left_the_breaker_is_ignored(world):
    for name in ("duckduckgo", "bing", "anysearch", "mojeek", "so360", "brave", "searx"):
        engine_health.record_failure(name, "captcha")
    nominal = _nominal_pool(QUERY, None, None)
    assert _active_pool(nominal) == (nominal, {})


def test_the_breaker_can_be_switched_off(world, monkeypatch):
    monkeypatch.setattr(settings, "health_enabled", False)
    engine_health.record_failure("mojeek", "captcha")
    nominal = _nominal_pool(QUERY, None, None)
    assert _active_pool(nominal) == (nominal, {})


# ---------------------------------------------------------------------------
# End to end
# ---------------------------------------------------------------------------


async def test_a_walled_engine_is_asked_once_then_benched_and_reported(world):
    world(Stub("mojeek", gate="captcha"))

    first = await aggregate_search(QUERY, use_cache=False)
    second = await aggregate_search(QUERY, use_cache=False)

    assert world.stubs["mojeek"].calls == 1
    assert first["engines"] == ["duckduckgo", "bing", "anysearch", "mojeek"]
    assert "benched_engines" not in first
    # The second answer lists what actually ran, and says who was left out.
    assert second["engines"] == ["duckduckgo", "bing", "anysearch"]
    assert second["benched_engines"]["mojeek"]["reason"] == "captcha"
    assert "mojeek is benched (captcha" in second["benched_hint"]
    assert "engines=" in second["benched_hint"]


async def test_naming_a_benched_engine_still_runs_it(world):
    engine_health.record_failure("mojeek", "captcha")
    out = await aggregate_search(QUERY, engines=["mojeek"], use_cache=False)
    assert world.stubs["mojeek"].calls == 1
    assert out["engines"] == ["mojeek"] and "benched_engines" not in out


async def test_a_named_engine_that_fails_is_still_recorded(world):
    world(Stub("mojeek", gate="captcha"))
    await aggregate_search(QUERY, engines=["mojeek", "duckduckgo"], use_cache=False)
    assert engine_health.is_open("mojeek")


async def test_an_engine_that_recovers_is_un_benched(world):
    engine_health.record_failure("bing", "error")
    await aggregate_search(QUERY, use_cache=False)
    engine_health.record_failure("bing", "error")
    assert not engine_health.is_open("bing"), "the success in between cleared the first strike"


@pytest.mark.parametrize(
    "exc",
    [ValueError("serper not configured: set SEARCH_MCP_SERPER_API_KEY"), ValueError("unknown engine")],
)
async def test_a_configuration_error_is_not_a_health_problem(world, exc):
    world(Stub("bing", exc=exc))
    for _ in range(3):
        await aggregate_search(QUERY, use_cache=False)
    assert not engine_health.is_open("bing")


async def test_a_missing_browser_is_never_held_against_an_engine(world):
    world(Stub("bing", gate="browser_unavailable"))
    for _ in range(3):
        out = await aggregate_search(QUERY, use_cache=False)
    assert not engine_health.is_open("bing")
    assert out["gated_engines"]["bing"]["reason"] == "browser_unavailable"


async def test_a_rate_limit_skip_is_ours_not_the_engines(world, monkeypatch):
    async def acquire(name, max_wait=None):
        return name != "bing"

    monkeypatch.setattr(aggregator.search_limiter, "acquire", acquire)
    for _ in range(4):
        await aggregate_search(QUERY, use_cache=False)
    assert not engine_health.is_open("bing")


async def test_silence_counts_only_when_a_peer_found_something(world):
    world(Stub("anysearch", results=[]))
    for _ in range(3):
        await aggregate_search(QUERY, use_cache=False)
    assert engine_health.is_open("anysearch")

    engine_health.reset()
    for name in ("duckduckgo", "bing", "mojeek"):
        world(Stub(name, results=[]))
    for _ in range(3):
        await aggregate_search("a query nobody has an answer for", use_cache=False)
    assert not engine_health.is_open("anysearch"), "everyone was silent: that is the query"


async def test_losing_engines_mid_run_triggers_a_stand_in_even_with_plenty_of_results(
    world, monkeypatch
):
    """One survivor returns ten results. The count looks fine; the answer
    rests on a single index."""
    monkeypatch.setattr(settings, "rescue_enabled", True)
    world(Stub("bing", gate="captcha"), Stub("mojeek", gate="captcha"),
          Stub("anysearch", exc=RuntimeError("boom")))  # fmt: skip

    out = await aggregate_search(QUERY, use_cache=False)

    assert len([r for r in out["results"] if "duckduckgo" in r["engines"]]) > 3
    assert out["rescued_via"] == "so360"


async def test_the_same_loss_among_named_engines_does_not(world, monkeypatch):
    monkeypatch.setattr(settings, "rescue_enabled", True)
    world(Stub("bing", gate="captcha"))
    out = await aggregate_search(QUERY, engines=["duckduckgo", "bing"], use_cache=False)
    assert "rescued_via" not in out


async def test_rescue_skips_a_benched_candidate(world, monkeypatch):
    monkeypatch.setattr(settings, "rescue_enabled", True)
    engine_health.record_failure("so360", "captcha")
    world(Stub("duckduckgo", results=[]))

    out = await aggregate_search(QUERY, engines=["duckduckgo"], use_cache=False)

    assert world.stubs["so360"].calls == 0
    assert out["rescued_via"] == "brave"


# ---------------------------------------------------------------------------
# The cache
# ---------------------------------------------------------------------------


async def test_the_cache_key_does_not_wobble_with_the_breaker(world):
    await aggregate_search(QUERY)
    engine_health.record_failure("mojeek", "captcha")

    again = await aggregate_search(QUERY)

    assert again["cached"] is True
    assert len(world.cache.rows) == 1


async def test_a_cache_hit_reports_the_engines_that_produced_it(world):
    engine_health.record_failure("mojeek", "captcha")
    fresh = await aggregate_search(QUERY)
    engine_health.reset()

    hit = await aggregate_search(QUERY)

    assert hit["cached"] is True
    assert hit["engines"] == fresh["engines"] == ["duckduckgo", "bing", "anysearch"]
    assert hit["benched_engines"]["mojeek"]["reason"] == "captcha"


async def test_a_degraded_answer_is_replayed_for_an_hour_not_a_week(world):
    world(Stub("bing", gate="captcha"), Stub("mojeek", gate="captcha"),
          Stub("anysearch", gate="captcha"))  # fmt: skip
    await aggregate_search(QUERY)
    (key, (results, meta)), = world.cache.rows.items()
    assert meta["degraded"] is True

    assert (await aggregate_search(QUERY))["cached"] is True
    world.cache.rows[key] = (results, {**meta, "cached_at": meta["cached_at"] - 3601})
    assert (await aggregate_search(QUERY))["cached"] is False


async def test_a_healthy_answer_is_not_marked_degraded(world):
    world(Stub("mojeek", gate="captcha"))  # three of four standing
    await aggregate_search(QUERY)
    (_, (_, meta)), = world.cache.rows.items()
    assert "degraded" not in meta
