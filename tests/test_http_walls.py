"""An engine whose HTTP path is walled goes straight to the browser for a while."""

from __future__ import annotations

from search_mcp.browser import BrowserUnavailableError
from search_mcp.engines.base import Engine, SearchResult
from search_mcp.health import HTTP_WALL_SECONDS, HttpWalls, http_walls

_CAPTCHA = "<html><body>Please complete the following challenge</body></html>"
_RESULTS = "<html><body><a href='https://a.example/'>hit</a></body></html>"


class _Engine(Engine):
    name = "walled"

    def build_url(self, query, max_results, filters=None):
        return "https://walled.example/search"

    def parse(self, html):
        if "hit" not in html:
            return []
        return [SearchResult(title="hit", url="https://a.example/", snippet="", engine=self.name, rank=1)]


def _wire(monkeypatch, *, http=_CAPTCHA, browser=_RESULTS):
    calls = {"http": 0, "browser": 0}
    engine = _Engine()

    async def _fetch(url):
        calls["http"] += 1
        return http

    async def _fetch_html(url, wait_selector=None):
        calls["browser"] += 1
        if isinstance(browser, Exception):
            raise browser
        return "", browser

    monkeypatch.setattr(engine, "_fetch", _fetch)
    monkeypatch.setattr("search_mcp.engines.base.pool.fetch_html", _fetch_html)
    return engine, calls


async def test_a_wall_the_browser_got_past_skips_http_next_time(monkeypatch):
    engine, calls = _wire(monkeypatch)
    first = await engine.search("q", 5)
    assert [r.url for r in first] == ["https://a.example/"]
    assert calls == {"http": 1, "browser": 1}
    assert http_walls.is_walled("walled")

    second = await engine.search("q", 5)
    assert [r.url for r in second] == ["https://a.example/"]
    assert calls == {"http": 1, "browser": 2}


async def test_an_empty_http_and_browser_answer_is_not_a_wall(monkeypatch):
    engine, calls = _wire(monkeypatch, http="<html></html>", browser="<html></html>")
    assert await engine.search("q", 5) == []
    assert not http_walls.is_walled("walled")
    await engine.search("q", 5)
    assert calls == {"http": 2, "browser": 2}


async def test_browser_first_coming_back_empty_lifts_the_wall(monkeypatch):
    engine, calls = _wire(monkeypatch, browser="<html></html>")
    http_walls.mark("walled")
    assert await engine.search("q", 5) == []
    assert calls == {"http": 0, "browser": 1}
    assert not http_walls.is_walled("walled")


async def test_a_missing_browser_falls_back_to_http(monkeypatch):
    engine, calls = _wire(
        monkeypatch, http=_RESULTS, browser=BrowserUnavailableError("no chromium")
    )
    http_walls.mark("walled")
    out = await engine.search("q", 5)
    assert [r.url for r in out] == ["https://a.example/"]
    assert calls == {"http": 1, "browser": 1}
    assert not http_walls.is_walled("walled")


async def test_a_failed_render_lifts_the_wall_and_asks_http(monkeypatch):
    engine, calls = _wire(
        monkeypatch, http=_RESULTS, browser=RuntimeError("net::ERR_TIMED_OUT")
    )
    http_walls.mark("walled")
    out = await engine.search("q", 5)
    assert [r.url for r in out] == ["https://a.example/"]
    assert calls == {"http": 1, "browser": 1}
    assert not http_walls.is_walled("walled")


async def test_engines_without_a_browser_fallback_ignore_the_wall(monkeypatch):
    engine, calls = _wire(monkeypatch, http=_RESULTS)
    monkeypatch.setattr(engine, "supports_browser_fallback", False)
    http_walls.mark("walled")
    await engine.search("q", 5)
    assert calls == {"http": 1, "browser": 0}


async def test_http_only_strategy_ignores_the_wall(monkeypatch):
    from search_mcp.config import settings

    engine, calls = _wire(monkeypatch, http=_RESULTS)
    monkeypatch.setattr(settings, "fetch_strategy", "http")
    http_walls.mark("walled")
    await engine.search("q", 5)
    assert calls == {"http": 1, "browser": 0}


def test_the_wall_expires_and_survives_a_restart():
    now = [1000.0]
    walls = HttpWalls(wall=lambda: now[0])
    walls.mark("duckduckgo")
    assert walls.is_walled("duckduckgo")

    restarted = HttpWalls(wall=lambda: now[0])
    assert restarted.is_walled("duckduckgo")

    now[0] += HTTP_WALL_SECONDS + 1
    assert not restarted.is_walled("duckduckgo")
    assert not HttpWalls(wall=lambda: now[0]).is_walled("duckduckgo")


def test_an_unreadable_file_is_no_wall(tmp_path, monkeypatch):
    from search_mcp.config import settings

    monkeypatch.setattr(settings, "cache_dir", tmp_path)
    (tmp_path / "http_walls.json").write_text("not json", encoding="utf-8")
    assert not HttpWalls().is_walled("duckduckgo")
