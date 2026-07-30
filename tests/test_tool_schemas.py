"""Wire-shape pins for the MCP tool surface.

These tests exist to make an SDK upgrade a *visible* diff instead of a silent
one. Nothing in `server.py` declares an output schema by hand — the SDK derives
one from each tool's return annotation and, for unions/lists/scalars, wraps the
payload in ``{"result": ...}``. That is emergent behavior nobody asked for, and
from protocol revision 2026-07-28 onward the SDK *validates* returns against
those derived schemas, so a change in derivation rules turns into a tool error
at call time rather than a test failure at build time.

So: assert the shapes here, and let the migration prove it kept them.

The SDK-plumbing differences between protocol eras are confined to
``call_tool()`` below — every assertion in this file is written against the
protocol-level shape, not against the Python return type of the day.
"""

from __future__ import annotations

from typing import Any

import pytest

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.

# Declaration order in server.py. `tools/list` ordering is not cosmetic: the
# 2026-07-28 spec asks servers to return a deterministic order so clients can
# cache the list and LLM prompt caches keep hitting.
EXPECTED_TOOL_ORDER = [
    "search",
    "fetch",
    "fetch_batch",
    "read_doc",
    "research",
    "cache_search",
    "engines",
    "compare",
    "extract_structured",
    "download",
]

# Every tool reads except this one — it is the only thing in the server that
# writes outside the cache, which is why it asks permission first.
WRITING_TOOLS = {"download"}

# Tools whose return annotation is a union (`str | dict` / `str | list[dict]`),
# which the SDK cannot express as a bare object schema and therefore wraps.
UNION_RETURNING_TOOLS = [
    "search",
    "fetch_batch",
    "read_doc",
    "research",
    "cache_search",
    "compare",
    "extract_structured",
]


async def call_tool(name: str, args: dict[str, Any]) -> tuple[list[Any], Any]:
    """Call a tool and normalize the result to ``(content_blocks, structured)``.

    The SDK's Python-level return type changed across major versions (v1 hands
    back a bare tuple; v2 returns a `CallToolResult`). The protocol-level shape
    did not. Absorb that difference in one place so the assertions below stay
    about the protocol.
    """
    from search_mcp.server import mcp

    result = await mcp.call_tool(name, args)
    if isinstance(result, tuple):
        return result
    return list(result.content), result.structured_content


async def _tools_by_name() -> dict[str, Any]:
    from search_mcp.server import mcp

    return {t.name: t for t in await mcp.list_tools()}


# ---------------------------------------------------------------------------
# tools/list
# ---------------------------------------------------------------------------


async def test_tool_order_is_deterministic():
    from search_mcp.server import mcp

    names = [t.name for t in await mcp.list_tools()]
    assert names == EXPECTED_TOOL_ORDER


async def test_tool_list_is_stable_across_calls():
    from search_mcp.server import mcp

    first = [t.name for t in await mcp.list_tools()]
    second = [t.name for t in await mcp.list_tools()]
    assert first == second


async def test_every_tool_has_a_human_readable_title():
    """A title has to reach the client *somewhere*.

    Today it only lives in `annotations.title`; `Tool.title` is None on all
    nine. Those are distinct protocol fields — annotations are explicitly
    untrusted hints — so this accepts either while the migration moves titles
    to the real field.
    """
    for name, tool in (await _tools_by_name()).items():
        title = tool.title or (tool.annotations.title if tool.annotations else None)
        assert title, f"{name} exposes no title in either location"


async def test_read_only_annotations_match_what_each_tool_actually_does():
    """A wrong `readOnlyHint` is worse than none: clients use it to decide
    whether a call needs confirmation."""
    for name, tool in (await _tools_by_name()).items():
        assert tool.annotations is not None, f"{name} has no annotations"
        expected = name not in WRITING_TOOLS
        assert tool.annotations.read_only_hint is expected, (
            f"{name} is marked read_only_hint={tool.annotations.read_only_hint}"
        )
        assert tool.description, f"{name} has no description"


# ---------------------------------------------------------------------------
# Derived output schemas
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("name", UNION_RETURNING_TOOLS)
async def test_union_returning_tools_wrap_output_in_result(name):
    """`str | dict` cannot be one object schema, so the SDK wraps it.

    Pinned because it is the shape clients validate against — and because
    `format="markdown"` vs `format="json"` returning different types is what
    forces the wrapper in the first place.
    """
    tool = (await _tools_by_name())[name]
    schema = tool.output_schema
    assert schema is not None, f"{name} lost its derived output schema"
    assert schema["type"] == "object"
    assert schema["required"] == ["result"]
    assert set(schema["properties"]) == {"result"}
    assert "anyOf" in schema["properties"]["result"], (
        f"{name} should still admit both the markdown string and the json payload"
    )


async def test_fetch_opts_out_of_structured_output():
    """`fetch` is the one tool with no derived output schema, deliberately.

    It can return page text, a JSON payload, OR an actual image, and no single
    JSON Schema covers an ImageContent block. Since 2026-07-28 the SDK
    validates returns against the derived schema, so deriving one would turn
    every `inline=True` image fetch into a tool error.
    """
    tool = (await _tools_by_name())["fetch"]
    assert tool.output_schema is None
    assert "inline" in tool.input_schema["properties"]


async def test_engines_tool_output_schema_is_a_wrapped_string_array():
    tool = (await _tools_by_name())["engines"]
    assert tool.output_schema == {
        "properties": {
            "result": {"items": {"type": "string"}, "title": "Result", "type": "array"}
        },
        "required": ["result"],
        "title": "enginesOutput",
        "type": "object",
    }


async def test_every_tool_input_schema_is_an_object():
    for name, tool in (await _tools_by_name()).items():
        assert tool.input_schema["type"] == "object", f"{name} input schema is not an object"


# ---------------------------------------------------------------------------
# call_tool — structuredContent actually matches the advertised schema
# ---------------------------------------------------------------------------


async def test_engines_call_returns_wrapped_structured_content():
    blocks, structured = await call_tool("engines", {})
    assert structured is not None, "engines advertises an output schema but returned none"
    assert set(structured) == {"result"}
    assert isinstance(structured["result"], list)
    from search_mcp.engines import ENGINES

    assert structured["result"] == list(ENGINES)
    # One text block per engine name, alongside the structured payload.
    assert len(blocks) == len(ENGINES)


async def test_markdown_format_returns_a_string_inside_the_wrapper(tmp_path, monkeypatch):
    """`format="markdown"` (the default) must land in the `result` slot as a
    plain string — not as an object, and not unwrapped."""
    from search_mcp import config, documents

    monkeypatch.setattr(config.settings, "document_root", tmp_path)
    monkeypatch.setattr(documents.settings, "document_root", tmp_path)
    p = tmp_path / "doc.txt"
    p.write_text("hello structured world", encoding="utf-8")

    _blocks, structured = await call_tool("read_doc", {"source": str(p)})
    assert set(structured) == {"result"}
    assert isinstance(structured["result"], str)
    assert "hello structured world" in structured["result"]


async def test_json_format_returns_an_object_inside_the_same_wrapper(tmp_path, monkeypatch):
    """`format="json"` on the *same* tool returns a dict in the same slot.

    This is the pair that makes the union — and therefore the wrapper —
    unavoidable. If a future SDK stops admitting both, this is where it shows.
    """
    from search_mcp import config, documents

    monkeypatch.setattr(config.settings, "document_root", tmp_path)
    monkeypatch.setattr(documents.settings, "document_root", tmp_path)
    p = tmp_path / "doc.txt"
    p.write_text("hello structured world", encoding="utf-8")

    _blocks, structured = await call_tool(
        "read_doc", {"source": str(p), "format": "json"}
    )
    assert set(structured) == {"result"}
    assert isinstance(structured["result"], dict)
    assert structured["result"]["format"] == "text"
    assert "hello structured world" in structured["result"]["content"]


# ---------------------------------------------------------------------------
# prompts / resource templates
# ---------------------------------------------------------------------------


async def test_prompt_list_is_deterministic_and_titled():
    from search_mcp.server import mcp

    prompts = await mcp.list_prompts()
    assert [p.name for p in prompts] == [
        "research_prompt",
        "factcheck_prompt",
        "compare_sources",
        "news_brief",
    ]
    for p in prompts:
        assert p.title, f"{p.name} has no title"


async def test_resource_templates_are_declared_as_templates():
    from search_mcp.server import mcp

    templates = await mcp.list_resource_templates()
    uris = {t.uri_template for t in templates}
    assert uris == {"cache://page/{url}", "cache://search/{query_hash}"}
