"""Unit + smoke tests for the `compare` tool / `compare_urls` helper.

The unit tests mock `fetch_many` so they're hermetic and fast. The live
test (gated on SEARCH_MCP_TEST_NETWORK=1) actually compares two Wikipedia
pages end-to-end, which is the only way to catch real fetcher regressions.
"""
from __future__ import annotations

import os
from dataclasses import dataclass

import pytest

pytestmark = pytest.mark.asyncio


@dataclass
class _FakeResult:
    url: str
    title: str
    content: str
    method: str = "http"
    truncated: bool = False
    tokens_estimated: int = 0
    author: str = ""
    published_date: str = ""
    sitename: str = ""

    def to_dict(self) -> dict:
        return {
            "url": self.url,
            "title": self.title,
            "content": self.content,
            "method": self.method,
            "truncated": self.truncated,
            "tokens_estimated": self.tokens_estimated,
            "author": self.author,
            "published_date": self.published_date,
            "sitename": self.sitename,
        }


def _patch_fetch_many(monkeypatch, results):
    async def fake_fetch_many(urls, render="auto"):
        return results

    from search_mcp import compare as compare_mod
    monkeypatch.setattr(compare_mod, "fetch_many", fake_fetch_many)


async def test_compare_urls_shape(monkeypatch):
    from search_mcp.compare import compare_urls

    fakes = [
        _FakeResult(
            url="https://a.example",
            title="A",
            content="alpha body " * 50,
            sitename="a.example",
            published_date="2024-01-01",
        ),
        _FakeResult(
            url="https://b.example",
            title="B",
            content="beta body " * 50,
            sitename="b.example",
        ),
    ]
    _patch_fetch_many(monkeypatch, fakes)

    payload = await compare_urls(
        "what is the difference?", ["https://a.example", "https://b.example"],
    )
    assert payload["question"] == "what is the difference?"
    assert payload["urls"] == ["https://a.example", "https://b.example"]
    assert len(payload["excerpts"]) == 2

    a, b = payload["excerpts"]
    assert a["url"] == "https://a.example"
    assert a["title"] == "A"
    assert a["sitename"] == "a.example"
    assert a["published_date"] == "2024-01-01"
    assert "alpha body" in a["excerpt"]
    assert isinstance(a["tokens_estimated"], int) and a["tokens_estimated"] > 0
    assert b["url"] == "https://b.example"
    assert "beta body" in b["excerpt"]

    # tokens_estimated rolls up across all excerpts
    assert payload["tokens_estimated"] == sum(
        e["tokens_estimated"] for e in payload["excerpts"]
    )


async def test_compare_urls_propagates_per_url_errors(monkeypatch):
    from search_mcp.compare import compare_urls

    fakes = [
        _FakeResult(url="https://a.example", title="A", content="ok"),
        {"url": "https://b.example", "error": "boom"},
    ]
    _patch_fetch_many(monkeypatch, fakes)

    payload = await compare_urls("q", ["https://a.example", "https://b.example"])
    assert payload["excerpts"][0]["url"] == "https://a.example"
    assert "error" not in payload["excerpts"][0]
    assert payload["excerpts"][1] == {"url": "https://b.example", "error": "boom"}


async def test_compare_urls_truncates_long_bodies(monkeypatch):
    from search_mcp.compare import compare_urls

    big = "lorem ipsum " * 2000  # ~24k chars >> 6k budget
    fakes = [
        _FakeResult(url="https://a.example", title="A", content=big),
        _FakeResult(url="https://b.example", title="B", content=big),
    ]
    _patch_fetch_many(monkeypatch, fakes)

    payload = await compare_urls("q", ["https://a.example", "https://b.example"])
    for e in payload["excerpts"]:
        assert e["truncated"] is True
        # 6000 char budget + a small "[…truncated]" tail is acceptable.
        assert len(e["excerpt"]) <= 6100


async def test_compare_urls_rejects_too_few_urls():
    from search_mcp.compare import compare_urls
    with pytest.raises(ValueError):
        await compare_urls("q", ["https://only.example"])


async def test_compare_urls_rejects_too_many_urls():
    from search_mcp.compare import compare_urls
    with pytest.raises(ValueError):
        await compare_urls("q", [f"https://x{i}.example" for i in range(6)])


async def test_render_compare_markdown_has_per_url_sections():
    from search_mcp.formatting import render_compare

    payload = {
        "question": "diff?",
        "urls": ["https://a.example", "https://b.example"],
        "excerpts": [
            {
                "url": "https://a.example",
                "title": "Alpha",
                "sitename": "a.example",
                "published_date": "2024-01-01",
                "excerpt": "alpha body",
                "truncated": False,
                "tokens_estimated": 5,
            },
            {
                "url": "https://b.example",
                "title": "Beta",
                "sitename": "",
                "published_date": "",
                "excerpt": "beta body",
                "truncated": True,
                "tokens_estimated": 5,
            },
        ],
        "tokens_estimated": 10,
    }
    md = render_compare(payload)
    assert "# Compare: diff?" in md
    assert "## 1. Alpha" in md
    assert "## 2. Beta" in md
    assert "alpha body" in md and "beta body" in md
    assert "truncated" in md  # second excerpt's metadata
    assert "<https://a.example>" in md


async def test_render_compare_markdown_handles_errors():
    from search_mcp.formatting import render_compare
    md = render_compare({
        "question": "q",
        "excerpts": [{"url": "https://x.example", "error": "boom"}],
        "tokens_estimated": 0,
    })
    assert "⚠ https://x.example" in md
    assert "boom" in md


@pytest.mark.skipif(
    os.environ.get("SEARCH_MCP_TEST_NETWORK") != "1",
    reason="set SEARCH_MCP_TEST_NETWORK=1 to enable live compare smoke",
)
async def test_compare_urls_live_wikipedia():
    from search_mcp.compare import compare_urls

    payload = await compare_urls(
        "what is the difference between python and ruby?",
        [
            "https://en.wikipedia.org/wiki/Python_(programming_language)",
            "https://en.wikipedia.org/wiki/Ruby_(programming_language)",
        ],
    )
    assert len(payload["excerpts"]) == 2
    for e in payload["excerpts"]:
        assert "error" not in e
        assert e["excerpt"], "live wikipedia fetch should return content"
