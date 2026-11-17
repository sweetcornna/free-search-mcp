"""Offline regressions for orchestration deadlines, ownership and freshness."""
from __future__ import annotations

import asyncio
import gc
import time
from types import SimpleNamespace
from unittest.mock import AsyncMock

import pytest

from search_mcp import aggregator
from search_mcp import research as research_mod
from search_mcp.config import settings
from search_mcp.engines.base import SearchResult


def _hit(name: str) -> SearchResult:
    return SearchResult(
        title=f"Record from {name}", url=f"https://{name}.example/record",
        snippet=f"Current value from {name}", engine=name, rank=1,
    )


@pytest.mark.parametrize("lane_present", [False, True])
async def test_late_keyless_answer_ends_the_post_deadline_wait(monkeypatch, lane_present):
    """An empty/error completion must not cut the wait, but the first hit must."""
    monkeypatch.setattr(settings, "search_deadline_seconds", 1)
    loop = asyncio.get_running_loop()
    tasks = {name: loop.create_future() for name in ("empty", "error", "answer", "slow")}
    lane = [loop.create_future()] if lane_present else []
    real_wait = asyncio.wait
    calls = 0

    async def controlled_wait(
        watched, *, timeout=None, return_when=asyncio.ALL_COMPLETED,  # noqa: ASYNC109
    ):
        nonlocal calls
        calls += 1
        if calls == 1:
            assert timeout == 1
            return set(), set(watched)  # nobody answered at the deadline
        if calls <= 4:
            name = ("empty", "error", "answer")[calls - 2]
            task = tasks[name]
            outcome = {"empty": [], "error": RuntimeError("offline"), "answer": [_hit(name)]}
            task.set_result((name, outcome[name]))
            return {task}, set(watched) - {task}
        assert tasks["slow"].cancelled(), "kept waiting after a late keyless answer"
        return await real_wait(watched, timeout=timeout, return_when=return_when)

    monkeypatch.setattr(aggregator.asyncio, "wait", controlled_wait)
    diagnostics = {}
    try:
        results = await aggregator._fan_out(tasks.__getitem__, list(tasks), diagnostics, lane)
        assert [name for name, _ in results] == list(tasks)
        assert results[2][1] == [_hit("answer")]
        assert isinstance(results[3][1], TimeoutError)
        assert diagnostics == {"timed_out": ["slow"]}
        assert all(not task.cancelled() for task in lane), "the caller owns lane tasks"
    finally:
        for task in [*tasks.values(), *lane]:
            task.cancel()


@pytest.mark.parametrize("phase", ["initial", "after_deadline", "disabled"])
async def test_cancelling_fan_out_cancels_owned_children(monkeypatch, phase):
    monkeypatch.setattr(settings, "search_deadline_seconds", 0 if phase == "disabled" else 60)
    children = {}
    started = {name: asyncio.Event() for name in ("one", "two")}
    waiting = asyncio.Event()
    real_wait = asyncio.wait
    calls = 0

    async def run(name):
        children[name] = asyncio.current_task()
        started[name].set()
        await asyncio.Event().wait()
        return name, []

    async def controlled_wait(tasks, **kwargs):
        nonlocal calls
        calls += 1
        if phase == "after_deadline" and calls == 1:
            return set(), set(tasks)
        waiting.set()
        return await real_wait(tasks, **kwargs)

    monkeypatch.setattr(aggregator.asyncio, "wait", controlled_wait)
    parent = asyncio.create_task(aggregator._fan_out(run, list(started), {}))
    try:
        await asyncio.gather(*(event.wait() for event in started.values()))
        if phase != "disabled":
            await waiting.wait()
        parent.cancel()
        with pytest.raises(asyncio.CancelledError):
            await parent
        assert all(t.cancelled() or t.cancelling() for t in children.values()), (
            "the cancelled search left keyless engine requests running"
        )
    finally:
        parent.cancel()
        for child in children.values():
            child.cancel()
        await asyncio.gather(parent, *children.values(), return_exceptions=True)


async def test_cancelling_budgeted_research_cancels_page_reads(monkeypatch):
    urls = ["https://one.example/", "https://two.example/"]
    started = {url: asyncio.Event() for url in urls}
    children = {}

    async def fetch_page(url, **kwargs):
        children[url] = asyncio.current_task()
        started[url].set()
        await asyncio.Event().wait()

    monkeypatch.setattr(research_mod, "fetch_page", fetch_page)
    parent = asyncio.create_task(research_mod._read_within(urls, 60))
    try:
        await asyncio.gather(*(event.wait() for event in started.values()))
        parent.cancel()
        with pytest.raises(asyncio.CancelledError):
            await parent
        assert all(t.cancelled() or t.cancelling() for t in children.values()), (
            "the cancelled research left page requests running"
        )
    finally:
        parent.cancel()
        for child in children.values():
            child.cancel()
        await asyncio.gather(parent, *children.values(), return_exceptions=True)


async def _event_loop_turn():
    done = asyncio.get_running_loop().create_future()
    asyncio.get_running_loop().call_soon(done.set_result, None)
    await done


@pytest.mark.parametrize("owner", ["fan_out", "read_within", "deadline"])
async def test_cancelled_children_are_owned_until_cleanup_exceptions_are_consumed(monkeypatch, owner):
    """Cancellation must stay prompt even if cleanup waits, then fails later."""
    loop = asyncio.get_running_loop()
    started = asyncio.Event()
    cleaning = asyncio.Event()
    release = asyncio.Event()
    cleaned = asyncio.Event()
    unhandled = []
    old_handler = loop.get_exception_handler()
    loop.set_exception_handler(lambda loop, context: unhandled.append(context))
    child_ref = []

    async def child(name, **kwargs):
        child_ref.append(asyncio.current_task())
        started.set()
        try:
            await asyncio.Event().wait()
        finally:
            cleaning.set()
            await release.wait()
            cleaned.set()
            raise RuntimeError("cleanup failed")

    async def run(name):
        if name == "fast":
            return name, [_hit(name)]
        return await child(name)

    if owner == "read_within":
        monkeypatch.setattr(research_mod, "fetch_page", child)
        call = research_mod._read_within(["https://slow.example/"], 60)
    else:
        monkeypatch.setattr(settings, "search_deadline_seconds", 60)
        if owner == "deadline":
            real_wait = asyncio.wait
            calls = 0

            async def controlled_wait(tasks, **kwargs):
                nonlocal calls
                calls += 1
                if calls == 1:
                    await started.wait()
                    done = {task for task in tasks if task.done()}
                    return done, set(tasks) - done
                if calls == 2:
                    await cleaning.wait()
                    return set(), set(tasks)  # the 1 s cleanup grace has elapsed
                return await real_wait(tasks, **kwargs)

            monkeypatch.setattr(aggregator.asyncio, "wait", controlled_wait)
        call = aggregator._fan_out(run, ["fast", "slow"], {})
    parent = asyncio.create_task(call)
    try:
        await started.wait()
        if owner == "deadline":
            results = await parent
            assert isinstance(results[1][1], TimeoutError)
        else:
            parent.cancel()
            with pytest.raises(asyncio.CancelledError):
                await parent
        assert not cleaned.is_set(), "parent must not wait indefinitely for cleanup"
        await cleaning.wait()
        assert child_ref[0] in aggregator._cancelled_search_tasks
        release.set()
        await cleaned.wait()
        await _event_loop_turn()  # run callbacks that consume the child's exception
        assert child_ref[0] not in aggregator._cancelled_search_tasks
        child_ref.clear()
        del parent
        gc.collect()
        await _event_loop_turn()
        assert unhandled == [], "a cancelled child's cleanup exception was never retrieved"
    finally:
        release.set()
        for task in child_ref:
            task.cancel()
        await asyncio.gather(*child_ref, return_exceptions=True)
        loop.set_exception_handler(old_handler)


@pytest.mark.parametrize("cleanup", ["blocked", "returns", "raises"])
async def test_read_budget_does_not_wait_for_page_cleanup(monkeypatch, cleanup):
    urls = ["https://fast.example/", "https://slow.example/"]
    started = asyncio.Event()
    cleaning = asyncio.Event()
    release = asyncio.Event()
    children = []

    async def fetch_page(url, **kwargs):
        if url == urls[0]:
            return {"url": url, "content": "Ready"}
        children.append(asyncio.current_task())
        started.set()
        try:
            await asyncio.Event().wait()
        except asyncio.CancelledError:
            cleaning.set()
            if cleanup == "blocked":
                await release.wait()
                raise
            if cleanup == "raises":
                raise RuntimeError("cleanup failed") from None
            return {"url": url, "content": "Too late"}

    async def deadline_wait(tasks, **kwargs):
        await started.wait()
        assert kwargs == {"timeout": 0.1}
        done = {task for task in tasks if task.done()}
        return done, set(tasks) - done

    monkeypatch.setattr(research_mod, "fetch_page", fetch_page)
    monkeypatch.setattr(research_mod.asyncio, "wait", deadline_wait)
    parent = asyncio.create_task(research_mod._read_within(urls, 0.1))
    try:
        await cleaning.wait()
        await _event_loop_turn()
        assert parent.done(), "page cleanup held the research past its read budget"
        assert parent.result() == [
            {"url": urls[0], "content": "Ready"},
            {"url": urls[1], "error": "not read within 0.1 s"},
        ], "every page pending at the deadline must keep its timeout outcome"
        if cleanup == "blocked":
            assert children[0] in aggregator._cancelled_search_tasks
            assert not children[0].done()
    finally:
        release.set()
        await asyncio.gather(parent, *children, return_exceptions=True)
        await _event_loop_turn()
        assert all(task not in aggregator._cancelled_search_tasks for task in children)


class _SearchCache:
    def __init__(self, age):
        self.age = age
        self.results = [{"title": "Old answer", "url": "https://old.example/", "snippet": "Old"}]
        self.meta = {}
        self.reads = []
        self.writes = []

    async def get_search(self, key, *, max_age_seconds=None):
        self.reads.append(max_age_seconds)
        ttl = settings.cache_ttl_seconds if max_age_seconds is None else max_age_seconds
        if self.age > ttl:
            return None
        return self.results, {**self.meta, "cached_at": time.time() - self.age}

    async def put_search(self, key, query, engines, results, meta=None):
        self.writes.append(key)
        self.age = 0
        self.results = results
        self.meta = meta or {}


def _offline_pool(monkeypatch, names):
    """Keep real category/claim metadata, never call an engine or credential store."""
    web = SimpleNamespace(
        categories=frozenset(), claims=lambda q: False, answers_directly=lambda q: False,
        is_available=lambda: True, search=AsyncMock(return_value=[_hit("web")]),
    )
    engines = {"web": web}
    for name in names:
        engine = aggregator.ENGINES[name]
        monkeypatch.setattr(engine, "is_available", lambda: True)
        monkeypatch.setattr(engine, "search", AsyncMock(return_value=[_hit(name)]))
        engines[name] = engine
    monkeypatch.setattr(aggregator, "ENGINES", engines)
    monkeypatch.setattr(aggregator, "get_engine", engines.__getitem__)
    monkeypatch.setattr(settings, "default_engines", ["web"])
    monkeypatch.setattr(settings, "locale_engines", {})
    monkeypatch.setattr(settings, "fresh_engines", [])
    monkeypatch.setattr(settings, "agent_engines", "off")
    monkeypatch.setattr(settings, "health_enabled", False)
    monkeypatch.setattr(settings, "auto_route_enabled", True)
    monkeypatch.setattr(settings, "claim_engine_limit", -1)
    return engines


@pytest.mark.parametrize("mode", ["auto", "named", "category"])
@pytest.mark.parametrize(
    ("name", "query", "category", "ttl"),
    [
        ("openmeteo", "上海明天天气", "weather", 3600),
        ("frankfurter", "100 usd to cny", "finance.fx", 3600),
        ("pypi", "latest fastapi version", "software.python", 6 * 3600),
        ("osv", "requests vulnerabilities", "security.package", 6 * 3600),
        ("worldclock", "现在北京时间几点", "calendar.clock", 0),
    ],
)
async def test_record_cache_caps_follow_the_engines_asked(monkeypatch, mode, name, query, category, ttl):
    engines = _offline_pool(monkeypatch, [name])
    cache = _SearchCache(age=ttl + 1)
    monkeypatch.setattr(aggregator, "cache", cache)
    kwargs = {}
    if mode == "named":
        kwargs["engines"] = [name]
        monkeypatch.setattr(settings, "auto_route_enabled", False)
    elif mode == "category":
        kwargs["category"] = category

    first = await aggregator.aggregate_search(query, **kwargs)
    assert first["cached"] is False, "a stale direct answer was replayed"
    assert name in first["results"][0]["engines"]
    assert first["cache_key"] == cache.writes[0], "TTL caps must not disable cache writes"
    assert engines[name].search.await_count == 1
    assert cache.reads == ([] if ttl == 0 else [ttl])

    second = await aggregator.aggregate_search(query, **kwargs)
    assert second["cached"] is (ttl != 0)
    assert engines[name].search.await_count == (2 if ttl == 0 else 1)


@pytest.mark.parametrize("mode", ["not_routed", "explicit_web", "holiday"])
async def test_unselected_record_engines_do_not_shorten_the_cache(monkeypatch, mode):
    _offline_pool(monkeypatch, ["openmeteo", "worldclock", "holidays"])
    cache = _SearchCache(age=24 * 3600)
    monkeypatch.setattr(aggregator, "cache", cache)
    kwargs = {}
    query = "上海明天天气"
    if mode == "not_routed":
        monkeypatch.setattr(settings, "auto_route_enabled", False)
    elif mode == "explicit_web":
        kwargs["engines"] = ["web"]
    else:
        query = "2026年中国放假安排"
        kwargs["category"] = "calendar.holidays"
    payload = await aggregator.aggregate_search(query, **kwargs)
    assert payload["cached"] is True
    assert cache.reads == [None]


@pytest.mark.parametrize(("freshness", "max_age", "expected"), [(None, 60, 60), ("day", None, 3600)])
async def test_engine_caps_still_respect_tighter_caller_limits(monkeypatch, freshness, max_age, expected):
    _offline_pool(monkeypatch, ["pypi"])
    cache = _SearchCache(age=0)
    monkeypatch.setattr(aggregator, "cache", cache)
    await aggregator.aggregate_search("latest fastapi version", freshness=freshness, max_age_seconds=max_age)
    assert cache.reads == [expected]


def test_clock_category_never_replays_a_cached_time():
    assert aggregator._read_ttl(None, "calendar.clock", None) == 0
    assert aggregator._read_ttl(None, "calendar.holidays", None) is None


@pytest.mark.parametrize("budget", [None, 60])
@pytest.mark.parametrize(("max_age", "cached"), [(0, None), (60, None), (60, {"content": "Recent"})])
async def test_research_budget_keeps_page_freshness(monkeypatch, budget, max_age, cached):
    url = "https://record.example/current"
    get_page = AsyncMock(return_value=cached)
    fetch_page = AsyncMock(return_value={"url": url, "content": "Body"})
    monkeypatch.setattr(research_mod.cache, "get_page", get_page)
    monkeypatch.setattr(research_mod, "fetch_page", fetch_page)
    brief = await research_mod.research(
        "current record", search_payload={"results": [{"url": url, "title": "Record"}]},
        read_budget_seconds=budget, page_max_age_seconds=max_age,
    )
    fetch_page.assert_awaited_once_with(url, force_refresh=(max_age == 0 or cached is None))
    if max_age == 0:
        get_page.assert_not_awaited()
    else:
        get_page.assert_awaited_once_with(url, max_age_seconds=max_age)
    assert brief["documents"] == [{"url": url, "title": "Record", "content": "Body"}]


async def test_budget_with_no_freshness_limit_keeps_default_page_cache(monkeypatch):
    url = "https://record.example/current"
    fetch_page = AsyncMock(return_value={"url": url, "content": "Cached body"})
    get_page = AsyncMock(side_effect=AssertionError("no freshness precheck needed"))
    monkeypatch.setattr(research_mod, "fetch_page", fetch_page)
    monkeypatch.setattr(research_mod.cache, "get_page", get_page)
    await research_mod.research(
        "record", search_payload={"results": [{"url": url}]}, read_budget_seconds=60,
    )
    fetch_page.assert_awaited_once_with(url)
    get_page.assert_not_awaited()
