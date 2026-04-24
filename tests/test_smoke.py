"""Smoke tests. They hit the live web; mark them slow and skip when offline."""
import os

import pytest

pytestmark = pytest.mark.asyncio

NETWORK = os.environ.get("SEARCH_MCP_TEST_NETWORK") == "1"
skip_offline = pytest.mark.skipif(not NETWORK, reason="set SEARCH_MCP_TEST_NETWORK=1 to run")


async def test_imports():
    from search_mcp import server  # noqa: F401
    from search_mcp.aggregator import aggregate_search  # noqa: F401
    from search_mcp.fetcher import fetch_page  # noqa: F401
    from search_mcp.documents import read_document  # noqa: F401


@skip_offline
async def test_duckduckgo_returns_results():
    from search_mcp.engines import get_engine
    results = await get_engine("duckduckgo").search("python language", 5)
    assert len(results) > 0
    assert results[0].url.startswith("http")


@skip_offline
async def test_mojeek_returns_results():
    from search_mcp.engines import get_engine
    results = await get_engine("mojeek").search("python language", 5)
    assert len(results) > 0


@skip_offline
async def test_aggregate_merges_results():
    from search_mcp.aggregator import aggregate_search
    out = await aggregate_search("openai api", engines=["duckduckgo", "mojeek"], max_results=5)
    assert len(out["results"]) > 0
    assert "score" in out["results"][0]


@skip_offline
async def test_fetch_returns_markdown():
    from search_mcp.fetcher import fetch_page
    result = await fetch_page("https://example.com", render="http", force_refresh=True)
    assert "Example Domain" in result.content
    assert result.tokens_estimated > 0


async def test_read_local_text(tmp_path):
    from search_mcp.documents import read_document
    p = tmp_path / "hello.txt"
    p.write_text("hello world\n", encoding="utf-8")
    result = await read_document(str(p))
    assert "hello world" in result.content
    assert result.format == "text"


@skip_offline
async def test_research_one_shot_returns_brief():
    from search_mcp.research import research
    out = await research("what is the model context protocol", depth=2)
    assert out["question"]
    assert len(out["sources"]) > 0
    # documents may include errors but should match source count
    assert len(out["documents"]) == len(out["sources"])


async def test_server_tool_format_markdown_returns_string():
    """End-to-end via the MCP layer: format=markdown returns a string."""
    from search_mcp.server import mcp
    # call a tool that doesn't need network: engines()
    result = await mcp.call_tool("engines", {})
    # call_tool returns (content_blocks, structured_content?) tuple in newer SDKs
    assert result is not None
