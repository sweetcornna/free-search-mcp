"""Startup warms Bing's jar always, and the browser only when the pool needs it."""

from __future__ import annotations

import asyncio

import pytest

from search_mcp import prewarm as prewarm_mod
from search_mcp.config import settings
from search_mcp.health import http_walls


@pytest.fixture
def warmed(monkeypatch):
    monkeypatch.setattr(settings, "prewarm", True)
    calls: list[str] = []

    async def bing_warm(self):
        calls.append("bing")

    async def launch():
        calls.append("browser")

    monkeypatch.setattr("search_mcp.engines.bing.BingEngine.warm", bing_warm)
    monkeypatch.setattr(prewarm_mod.pool, "launch", launch)
    monkeypatch.setattr("search_mcp.aggregator.default_pool_now", lambda: ["duckduckgo", "bing"])
    return calls


async def test_warms_bing_and_leaves_the_browser_alone_without_evidence(warmed):
    await prewarm_mod.prewarm()
    assert warmed == ["bing"]


async def test_launches_the_browser_when_a_pool_engine_was_walled(warmed):
    http_walls.mark("duckduckgo")
    await prewarm_mod.prewarm()
    assert sorted(warmed) == ["bing", "browser"]


async def test_launches_the_browser_for_an_engine_that_always_renders(warmed, monkeypatch):
    monkeypatch.setattr("search_mcp.aggregator.default_pool_now", lambda: ["duckduckgo", "brave"])
    await prewarm_mod.prewarm()
    assert warmed == ["browser"]


async def test_http_only_strategy_never_launches_the_browser(warmed, monkeypatch):
    monkeypatch.setattr(settings, "fetch_strategy", "http")
    http_walls.mark("duckduckgo")
    await prewarm_mod.prewarm()
    assert warmed == ["bing"]


async def test_off_does_nothing(warmed, monkeypatch):
    monkeypatch.setattr(settings, "prewarm", False)
    http_walls.mark("duckduckgo")
    await prewarm_mod.prewarm()
    assert warmed == []


async def test_a_failing_step_is_swallowed(warmed, monkeypatch):
    async def boom(self):
        raise RuntimeError("no network")

    monkeypatch.setattr("search_mcp.engines.bing.BingEngine.warm", boom)
    await prewarm_mod.prewarm()


def test_an_expired_wall_still_counts_for_a_day():
    from search_mcp.health import HTTP_WALL_SECONDS, HttpWalls

    now = [1000.0]
    walls = HttpWalls(wall=lambda: now[0])
    walls.mark("duckduckgo")
    now[0] += HTTP_WALL_SECONDS + 60
    assert not walls.is_walled("duckduckgo")
    assert HttpWalls(wall=lambda: now[0]).recently_walled("duckduckgo")
    now[0] += 24 * 3600
    assert not HttpWalls(wall=lambda: now[0]).recently_walled("duckduckgo")


async def test_the_server_lifespan_runs_prewarm(monkeypatch):
    from search_mcp import server

    ran = []

    async def fake():
        ran.append(True)

    monkeypatch.setattr(server, "prewarm", fake)
    async with server._lifespan(server.mcp) as state:
        assert state == {}
        await asyncio.sleep(0)  # the warm-up runs beside the server, not before it
    assert ran == [True]
