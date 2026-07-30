"""End-to-end protocol checks against a real in-memory client.

`test_tool_schemas.py` asserts what the server *declares*; this file asserts
what a client actually receives, over both protocol eras that MCP SDK v2
serves from one `MCPServer` instance:

  * modern (`2026-07-28`) — no handshake, per-request metadata, `resultType`
    on every result, `ttlMs`/`cacheScope` on cacheable ones
  * legacy (`2025-11-25` and earlier) — the `initialize` handshake

Dual-era support is the whole reason the 0.5.0 upgrade is not a breaking
change for users, so it gets a test rather than a paragraph in the changelog.
"""

from __future__ import annotations

import pytest
from mcp.client import Client

from search_mcp.server import mcp

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.

MODERN = "2026-07-28"


# ---------------------------------------------------------------------------
# Both eras serve the same tool surface
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("mode", [MODERN, "legacy"])
async def test_tools_are_identical_across_protocol_eras(mode):
    """A client on either era sees the same nine tools with the same titles.

    This is the regression that matters for existing users: upgrading the SDK
    must not strand anyone still speaking the handshake protocol.
    """
    async with Client(mcp, mode=mode) as client:
        result = await client.list_tools()
        assert [t.name for t in result.tools] == [
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
        # Titles reach the client via the real `Tool.title` field, not only as
        # an untrusted annotations hint.
        assert all(t.title for t in result.tools)


@pytest.mark.parametrize("mode", [MODERN, "legacy"])
async def test_engines_tool_callable_on_both_eras(mode):
    from search_mcp.engines import ENGINES

    async with Client(mcp, mode=mode) as client:
        result = await client.call_tool("engines", {})
        assert result.structured_content == {"result": list(ENGINES)}


# ---------------------------------------------------------------------------
# 2026-07-28 specifics
# ---------------------------------------------------------------------------


async def test_modern_results_are_tagged_complete():
    """Every result carries `resultType`; ours are always `"complete"` because
    no tool asks the user anything mid-call (no multi-round-trip requests)."""
    async with Client(mcp, mode=MODERN) as client:
        assert (await client.list_tools()).result_type == "complete"
        assert (await client.call_tool("engines", {})).result_type == "complete"


async def test_modern_list_results_carry_our_cache_hints():
    """`tools/list` and friends are static for the life of the process, so the
    server advertises a long public TTL rather than letting clients poll."""
    async with Client(mcp, mode=MODERN) as client:
        for result in (
            await client.list_tools(),
            await client.list_prompts(),
            await client.list_resource_templates(),
        ):
            assert result.ttl_ms == 3_600_000
            assert result.cache_scope == "public"


async def test_modern_client_negotiates_the_latest_revision():
    async with Client(mcp, mode=MODERN) as client:
        assert client.protocol_version == MODERN


async def test_missing_resource_reports_invalid_params_not_internal_error():
    """A cache miss is "not found", not "the server broke".

    2026-07-28 moved resource-not-found from `-32002` to `-32602` (invalid
    params). Raising a bare `ValueError` from the handler would surface as
    `-32603` internal error instead, which tells the client to retry something
    that will never succeed.
    """
    from mcp.shared.exceptions import MCPError

    async with Client(mcp, mode=MODERN) as client:
        with pytest.raises(MCPError) as excinfo:
            await client.read_resource("cache://search/no-such-hash")

    error = excinfo.value.error
    assert error.code == -32602
    assert error.data == {"uri": "cache://search/no-such-hash"}


async def test_server_identifies_itself():
    """`server/discover` / serverInfo should carry real identity, not the
    SDK's empty-string default."""
    from search_mcp import __version__

    async with Client(mcp, mode="legacy") as client:
        info = client.server_info
        assert info.name == "search-mcp"
        assert info.version == __version__
        assert info.website_url
        assert client.instructions, "server instructions guide tool selection"
