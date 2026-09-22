"""`research(read_budget_seconds=...)`: the pages share a time budget, and a
page that misses it is reported like a failed fetch and left out.

Measured 2026-09-21 on a Chinese query: a Baidu Baike page took 25.7 s to fail
while the two pages holding the answer arrived in 0.5 s and 2.1 s. Without a
budget the brief waits for the slowest page.
"""
from __future__ import annotations

import asyncio
import time
from typing import Any

from search_mcp import fetcher
from search_mcp import research as research_mod


def _page(url: str) -> fetcher.FetchResult:
    return fetcher.FetchResult(
        url=url, title="A page", content="Body of " + url, method="http", truncated=False
    )


async def test_slow_pages_are_dropped_and_cancelled(monkeypatch):
    cancelled: list[str] = []

    async def fake_fetch_page(url: str, **_: Any) -> fetcher.FetchResult:
        if "slow" in url:
            try:
                await asyncio.sleep(30)
            except asyncio.CancelledError:
                cancelled.append(url)
                raise
        if "broken" in url:
            raise fetcher.FetchError(f"fetch failed for {url}: HTTP 503")
        return _page(url)

    monkeypatch.setattr(research_mod, "fetch_page", fake_fetch_page)
    urls = ["https://fast.example/a", "https://slow.example/b", "https://broken.example/c"]
    started = time.monotonic()
    results = await research_mod._read_within(urls, 0.2)

    assert time.monotonic() - started < 5
    assert results[0].url == urls[0], "order follows the input, not completion"
    assert results[1] == {"url": urls[1], "error": "not read within 0.2 s"}
    assert results[2] == {"url": urls[2], "error": f"fetch failed for {urls[2]}: HTTP 503"}
    assert cancelled == [urls[1]], "the slow fetch is stopped, not left running"


async def test_research_uses_the_budget_only_when_asked(monkeypatch):
    async def fake_search(question: str, **_: Any) -> dict[str, Any]:
        return {
            "results": [
                {"title": "One", "url": "https://fast.example/a", "snippet": "first"},
                {"title": "Two", "url": "https://slow.example/b", "snippet": "second"},
            ],
            "engines": ["duckduckgo"],
        }

    async def fake_fetch_page(url: str, **_: Any) -> fetcher.FetchResult:
        if "slow" in url:
            await asyncio.sleep(30)
        return _page(url)

    async def unbudgeted(urls: list[str], page_max_age_seconds: int | None) -> list[Any]:
        return [_page(u) for u in urls]

    monkeypatch.setattr(research_mod, "aggregate_search", fake_search)
    monkeypatch.setattr(research_mod, "fetch_page", fake_fetch_page)
    monkeypatch.setattr(research_mod, "_fetch_with_freshness", unbudgeted)

    brief = await research_mod.research("a question", depth=2, read_budget_seconds=0.2)
    assert brief["documents"][0]["content"] == "Body of https://fast.example/a"
    assert brief["documents"][1] == {
        "url": "https://slow.example/b",
        "error": "not read within 0.2 s",
    }
    # The `research` tool passes no budget and keeps waiting for every page.
    brief = await research_mod.research("a question", depth=2)
    assert [d["content"] for d in brief["documents"]] == [
        "Body of https://fast.example/a",
        "Body of https://slow.example/b",
    ]
