"""Bing's shared session: minted once, re-minted on a decoy, never fatal.

The engine's own HTTP is stubbed at two seams — `_warm_cookies` (the home-page
GET that mints the jar) and `Engine._http_get` (the search request) — so these
tests are about the bookkeeping between them, which is where the bugs would be.
"""
from __future__ import annotations

import asyncio

import pytest

from search_mcp.engines import base as base_mod
from search_mcp.engines import bing as bing_mod
from search_mcp.engines.bing import BingEngine

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.

QUERY = "rust ownership borrowing"


def _serp(rows: list[tuple[str, str]], host: str = "r.example") -> str:
    return (
        '<html><body><ol id="b_results">'
        + "".join(
            f'<li class="b_algo"><h2><a href="https://{host}/{i}">{t}</a></h2>'
            f'<div class="b_caption"><p>{s}</p></div></li>'
            for i, (t, s) in enumerate(rows)
        )
        + "</ol></body></html>"
    )


DECOY = _serp([("Rust on Steam", "The only aim in Rust is to survive.")] * 8, host="decoy.example")
REAL = _serp([("Understanding Ownership - Rust", "Ownership and borrowing in Rust.")] * 8)
PAGE_TWO = _serp(
    [("References and Borrowing", "Borrowing rules and ownership.")] * 6, host="p2.example"
)


class _Wire:
    """Records warm-ups and search requests; answers from a script."""

    def __init__(self, monkeypatch, pages, *, cookies=None, warm_delay=0.0):
        self.warmups = 0
        self.requests: list[tuple[str, dict | None]] = []
        self._pages = list(pages)
        self._cookies = [{"MUID": "one"}] if cookies is None else list(cookies)
        self._warm_delay = warm_delay

        async def warm(impersonate, proxy_kwargs):
            self.warmups += 1
            if self._warm_delay:
                await asyncio.sleep(self._warm_delay)
            jar = self._cookies[min(self.warmups, len(self._cookies)) - 1]
            if isinstance(jar, Exception):
                raise jar
            return dict(jar)

        async def http_get(engine, url, *, cookies=None):
            self.requests.append((url, cookies))
            page = self._pages[min(len(self.requests), len(self._pages)) - 1]
            return page(url, cookies) if callable(page) else page

        monkeypatch.setattr(bing_mod, "_warm_cookies", warm)
        monkeypatch.setattr(base_mod.Engine, "_http_get", http_get)
        monkeypatch.setattr(bing_mod.settings, "fetch_strategy", "http")


async def test_the_session_is_minted_once_and_sent_with_every_search(monkeypatch):
    wire = _Wire(monkeypatch, [REAL])
    engine = BingEngine()

    first = await engine.search(QUERY, 10)
    second = await engine.search("rust borrowing ownership rules", 10)

    assert len(first) == 8 and len(second) == 8
    assert wire.warmups == 1
    assert [cookies for _, cookies in wire.requests] == [{"MUID": "one"}, {"MUID": "one"}]


async def test_concurrent_cold_searches_share_one_warm_up(monkeypatch):
    wire = _Wire(monkeypatch, [REAL], warm_delay=0.02)
    engine = BingEngine()

    await asyncio.gather(*(engine.search(QUERY, 10) for _ in range(5)))

    assert wire.warmups == 1
    assert len(wire.requests) == 5
    assert all(cookies == {"MUID": "one"} for _, cookies in wire.requests)


async def test_a_decoy_re_mints_the_session_and_retries_once(monkeypatch):
    wire = _Wire(monkeypatch, [DECOY, REAL], cookies=[{"MUID": "stale"}, {"MUID": "fresh"}])

    results = await BingEngine().search(QUERY, 10)

    assert wire.warmups == 2
    assert [cookies for _, cookies in wire.requests] == [{"MUID": "stale"}, {"MUID": "fresh"}]
    assert all("r.example" in r.url for r in results)


async def test_a_persistent_decoy_is_returned_for_the_aggregator_to_judge(monkeypatch):
    """One retry, not a loop. The engine hands back what it got; the aggregator
    runs the same check, drops the bucket, and is the one place that says so."""
    wire = _Wire(monkeypatch, [DECOY], cookies=[{"MUID": "a"}, {"MUID": "b"}])
    diagnostics: dict = {}

    results = await BingEngine().search(QUERY, 10, diagnostics=diagnostics)

    assert len(wire.requests) == 2
    assert wire.warmups == 2
    assert len(results) == 8 and all("decoy.example" in r.url for r in results)
    # finalize_results ran exactly once: the count is the bucket, not double it.
    assert diagnostics["raw_per_engine"] == {"bing": 8}


async def test_concurrent_decoys_re_mint_once_not_once_each(monkeypatch):
    def page(url, cookies):
        return DECOY if cookies == {"MUID": "stale"} else REAL

    wire = _Wire(
        monkeypatch, [page], cookies=[{"MUID": "stale"}, {"MUID": "fresh"}], warm_delay=0.01
    )
    engine = BingEngine()

    buckets = await asyncio.gather(*(engine.search(QUERY, 10) for _ in range(4)))

    assert wire.warmups == 2, "four decoys from one stale jar are one re-mint"
    assert all(all("r.example" in r.url for r in bucket) for bucket in buckets)


async def test_a_failed_warm_up_does_not_fail_the_search(monkeypatch):
    wire = _Wire(monkeypatch, [REAL], cookies=[{}])

    results = await BingEngine().search(QUERY, 10)

    assert len(results) == 8
    # No jar to send — and `cookies=None` means the base session is built
    # exactly as it is for every other engine.
    assert wire.requests[0][1] is None


async def test_a_failed_warm_up_is_remembered_briefly_not_retried_per_search(monkeypatch):
    wire = _Wire(monkeypatch, [REAL], cookies=[{}])
    engine = BingEngine()
    clock = [1000.0]
    monkeypatch.setattr(bing_mod.time, "monotonic", lambda: clock[0])

    await engine.search(QUERY, 10)
    await engine.search(QUERY, 10)
    assert wire.warmups == 1

    clock[0] += bing_mod._WARM_FAIL_TTL + 1
    await engine.search(QUERY, 10)
    assert wire.warmups == 2


async def test_the_session_expires(monkeypatch):
    wire = _Wire(monkeypatch, [REAL])
    engine = BingEngine()
    clock = [1000.0]
    monkeypatch.setattr(bing_mod.time, "monotonic", lambda: clock[0])

    await engine.search(QUERY, 10)
    clock[0] += bing_mod._JAR_TTL - 1
    await engine.search(QUERY, 10)
    assert wire.warmups == 1

    clock[0] += 2
    await engine.search(QUERY, 10)
    assert wire.warmups == 2


async def test_a_different_proxy_egress_gets_its_own_session(monkeypatch):
    """Cookies issued to one exit IP and replayed from another are the very
    inconsistency the jar exists to avoid."""
    wire = _Wire(monkeypatch, [REAL], cookies=[{"MUID": "direct"}, {"MUID": "proxied"}])
    engine = BingEngine()
    egress: dict = {}
    monkeypatch.setattr(bing_mod, "curl_proxy_kwargs", lambda name=None: dict(egress))

    await engine.search(QUERY, 10)
    egress["proxy"] = "http://127.0.0.1:7890"
    await engine.search(QUERY, 10)
    egress.clear()
    await engine.search(QUERY, 10)

    assert wire.warmups == 2
    assert [c["MUID"] for _, c in wire.requests] == ["direct", "proxied", "direct"]


async def test_warm_cookies_never_raises(monkeypatch):
    class Boom:
        def __init__(self, *a, **kw):
            raise OSError("no route to host")

    monkeypatch.setattr(bing_mod, "AsyncSession", Boom)
    assert await bing_mod._warm_cookies("edge", {}) == {}


async def test_more_than_ten_results_come_from_a_second_page(monkeypatch):
    def page(url, cookies):
        return PAGE_TWO if "first=11" in url else REAL

    wire = _Wire(monkeypatch, [page])

    results = await BingEngine().search(QUERY, 20)

    urls = [url for url, _ in wire.requests]
    assert len(urls) == 2 and "first=11" in urls[1] and "count=" not in urls[1]
    assert len(results) == 14
    assert [r.rank for r in results] == list(range(1, 15))


async def test_ten_results_or_fewer_is_one_request(monkeypatch):
    wire = _Wire(monkeypatch, [REAL])
    await BingEngine().search(QUERY, 10)
    assert len(wire.requests) == 1


async def test_a_failing_second_page_costs_only_the_extra_results(monkeypatch):
    def page(url, cookies):
        if "first=11" in url:
            raise RuntimeError("page two exploded")
        return REAL

    _Wire(monkeypatch, [page])
    results = await BingEngine().search(QUERY, 20)
    assert len(results) == 8


async def test_a_decoy_is_not_paginated(monkeypatch):
    wire = _Wire(monkeypatch, [DECOY])
    await BingEngine().search(QUERY, 20)
    assert not any("first=11" in url for url, _ in wire.requests)


@pytest.mark.parametrize("query", ["rust", "rust ownership"])
async def test_a_query_too_short_to_judge_is_never_retried(monkeypatch, query):
    wire = _Wire(monkeypatch, [DECOY])
    await BingEngine().search(query, 10)
    assert len(wire.requests) == 1 and wire.warmups == 1
