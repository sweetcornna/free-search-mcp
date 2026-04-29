"""Prompts must be discoverable through the MCP layer and produce non-empty
text that mentions the relevant tool names — so the LLM actually uses them."""
import pytest

pytestmark = pytest.mark.asyncio


async def test_prompts_registered():
    from search_mcp.server import mcp
    prompts = await mcp.list_prompts()
    assert len(prompts) == 4
    names = {p.name for p in prompts}
    assert names == {
        "research_prompt",
        "factcheck_prompt",
        "compare_sources",
        "news_brief",
    }


async def test_research_prompt_renders_with_args():
    from search_mcp.server import mcp
    result = await mcp.get_prompt(
        "research_prompt", {"question": "what is MCP", "depth": 2},
    )
    # Expect at least one message
    assert result.messages, "prompt should yield at least one message"
    text = ""
    for msg in result.messages:
        content = msg.content
        # content can be a TextContent-like object with a .text attr
        text += getattr(content, "text", "") or ""
    assert "what is MCP" in text
    assert "research" in text.lower()
    # The prompt should tell the model how to cite
    assert "[1]" in text or "[n]" in text or "Sources" in text


async def test_factcheck_prompt_renders_with_args():
    from search_mcp.server import mcp
    claim = "The Eiffel Tower is in Berlin"
    result = await mcp.get_prompt("factcheck_prompt", {"claim": claim})
    assert result.messages
    text = ""
    for msg in result.messages:
        text += getattr(msg.content, "text", "") or ""
    assert claim in text
    # Should reference at least one of the search tools by name
    assert any(tool in text for tool in ("search", "fetch_batch", "fetch"))
    # Should ask for a verdict scale
    assert "TRUE" in text or "FALSE" in text


async def test_prompts_have_titles():
    from search_mcp.server import mcp
    prompts = await mcp.list_prompts()
    titles = {p.title for p in prompts}
    assert "Research thoroughly" in titles
    assert "Fact-check claim" in titles
    assert "Compare sources" in titles
    assert "News brief" in titles


async def test_compare_sources_prompt_renders():
    from search_mcp.server import mcp
    result = await mcp.get_prompt(
        "compare_sources",
        {"question": "which is faster", "urls": "https://a.example,https://b.example"},
    )
    assert result.messages
    text = ""
    for msg in result.messages:
        text += getattr(msg.content, "text", "") or ""
    assert "compare" in text
    assert "which is faster" in text
    assert "https://a.example" in text


async def test_news_brief_prompt_renders():
    from search_mcp.server import mcp
    result = await mcp.get_prompt(
        "news_brief", {"topic": "ai regulation", "since": "week"},
    )
    assert result.messages
    text = ""
    for msg in result.messages:
        text += getattr(msg.content, "text", "") or ""
    assert "ai regulation" in text
    assert "week" in text
    assert "search" in text and "fetch_batch" in text
