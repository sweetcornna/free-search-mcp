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
    "paper_graph",
    "cache_search",
    "engines",
    "compare",
    "extract_structured",
    "download",
]

# Every tool reads except this one — it writes an auto-expiring local file.
WRITING_TOOLS = {"download"}

# Tools that answer in either format: `format="markdown"` (a str, the default)
# or `format="json"` (a dict or a list). That is every tool except `fetch`,
# which can additionally hand back an image.
DUAL_FORMAT_TOOLS = [
    "search",
    "fetch_batch",
    "read_doc",
    "research",
    "paper_graph",
    "cache_search",
    "engines",
    "compare",
    "extract_structured",
    "download",
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
    """Every tool carries a real `Tool.title`.

    `annotations.title` is a different protocol field — annotations are
    explicitly untrusted hints — so a title that only lived there would not
    count. All eleven set the real one.
    """
    for name, tool in (await _tools_by_name()).items():
        assert tool.title, f"{name} has no Tool.title"


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


@pytest.mark.parametrize("name", DUAL_FORMAT_TOOLS)
async def test_dual_format_tools_advertise_no_output_schema(name):
    """A tool whose default output is prose must not promise structured output.

    The spec is explicit: a tool that declares an `outputSchema` MUST return
    `structuredContent` conforming to it. Deriving one from `str | dict` gave
    `{"result": <either>}`, and honouring it meant shipping the whole markdown
    body a second time inside `structuredContent` — which is the copy clients
    like Claude Code show the model, as one JSON string with escaped newlines.
    Pinned so a future `structured_output=None` cannot quietly bring it back.
    """
    tool = (await _tools_by_name())[name]
    assert tool.output_schema is None, f"{name} advertises a derived output schema again"


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


async def test_engines_tool_takes_a_group_and_a_format():
    """`engines` follows the same `format=` convention as every other tool, and
    can be narrowed to one group so the model does not have to read the whole
    registry to pick a paper source."""
    tool = (await _tools_by_name())["engines"]
    assert set(tool.input_schema["properties"]) == {"group", "format"}


async def test_every_tool_input_schema_is_an_object():
    for name, tool in (await _tools_by_name()).items():
        assert tool.input_schema["type"] == "object", f"{name} input schema is not an object"


async def test_download_input_schema_has_no_policy_controls():
    tool = (await _tools_by_name())["download"]
    assert set(tool.input_schema["properties"]) == {"url", "format"}


# ---------------------------------------------------------------------------
# call_tool — structuredContent actually matches the advertised schema
# ---------------------------------------------------------------------------


async def test_engines_markdown_names_every_registered_engine():
    """The rendered tree is derived from the registry, so it cannot omit an
    engine the way the hand-maintained buckets it replaced did — those never
    mentioned `openverse` or `zenodo`, and advertised `pubmed` for a category
    the engine limit stopped it from ever running in."""
    from search_mcp.engines import ENGINES

    blocks, _structured = await call_tool("engines", {})
    text = "\n".join(getattr(b, "text", "") or "" for b in blocks)
    missing = [name for name in ENGINES if f"`{name}`" not in text]
    assert not missing, missing


async def test_engines_json_still_returns_the_flat_name_list():
    """Programmatic callers keep the flat list they had before the taxonomy."""
    from search_mcp.engines import ENGINES

    _blocks, structured = await call_tool("engines", {"format": "json"})
    payload = structured["result"] if set(structured) == {"result"} else structured
    assert payload["engines"] == list(ENGINES)
    assert set(payload["taxonomy"]) <= {"web", *{c.split(".")[0] for e in ENGINES.values() for c in e.categories}}
    assert set(payload["descriptions"]) == set(ENGINES)


async def test_engines_group_filter_narrows_to_one_group():
    _blocks, structured = await call_tool("engines", {"group": "paper", "format": "json"})
    payload = structured["result"] if set(structured) == {"result"} else structured
    assert set(payload["taxonomy"]) == {"paper"}
    assert "arxiv" in payload["engines"]
    assert "duckduckgo" not in payload["engines"]


async def test_markdown_format_is_one_plain_text_block(tmp_path, monkeypatch):
    """`format="markdown"` (the default) is a text block and nothing else.

    No `structuredContent`: a second copy of the body there is what the model
    ends up reading, JSON-escaped, in clients that prefer structured content.
    """
    from search_mcp import config, documents

    monkeypatch.setattr(config.settings, "document_root", tmp_path)
    monkeypatch.setattr(documents.settings, "document_root", tmp_path)
    p = tmp_path / "doc.txt"
    p.write_text("hello structured world", encoding="utf-8")

    blocks, structured = await call_tool("read_doc", {"source": str(p)})
    assert structured is None
    assert len(blocks) == 1
    assert "hello structured world" in blocks[0].text
    assert "\n" in blocks[0].text, "markdown should keep real newlines"
    assert not blocks[0].text.lstrip().startswith("{")


async def test_json_format_is_structured_and_unwrapped(tmp_path, monkeypatch):
    """`format="json"` on the *same* tool: the payload IS the structured
    content — no `{"result": ...}` envelope — and the text block is that same
    JSON, for clients that only read text."""
    import json

    from search_mcp import config, documents

    monkeypatch.setattr(config.settings, "document_root", tmp_path)
    monkeypatch.setattr(documents.settings, "document_root", tmp_path)
    p = tmp_path / "doc.txt"
    p.write_text("hello structured world", encoding="utf-8")

    blocks, structured = await call_tool(
        "read_doc", {"source": str(p), "format": "json"}
    )
    assert isinstance(structured, dict)
    assert "result" not in structured
    assert structured["format"] == "text"
    assert "hello structured world" in structured["content"]
    assert json.loads(blocks[0].text) == structured


async def test_a_json_list_keeps_an_object_envelope():
    """The wire format requires `structuredContent` to be an object, so the
    tools whose json payload is a list are the one place the envelope stays."""
    import json

    blocks, structured = await call_tool("fetch_batch", {"urls": [], "format": "json"})
    assert structured == {"result": []}
    assert json.loads(blocks[0].text) == []


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
        "quick_search",
    ]
    for p in prompts:
        assert p.title, f"{p.name} has no title"


async def test_resource_templates_are_declared_as_templates():
    from search_mcp.server import mcp

    templates = await mcp.list_resource_templates()
    uris = {t.uri_template for t in templates}
    assert uris == {"cache://page/{url}", "cache://search/{query_hash}"}
