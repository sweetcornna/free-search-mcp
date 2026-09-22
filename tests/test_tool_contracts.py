"""What every tool promises at its boundary.

These are not happy-path tests — `test_smoke.py` and the per-tool modules cover
that. This module pins the two things a caller cannot verify for themselves:
an empty or malformed argument produces an ACTIONABLE message rather than a
misleading one, and the output never claims something the payload contradicts.

Everything here goes through `mcp.call_tool`, so it exercises the same
argument coercion and output-schema validation a real client does.
"""

from __future__ import annotations

from typing import Any

import pytest

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.


async def call(name: str, args: dict[str, Any]) -> tuple[list[Any], Any]:
    """Normalize the SDK's per-version return shape (see test_tool_schemas)."""
    from search_mcp.server import mcp

    result = await mcp.call_tool(name, args)
    if isinstance(result, tuple):
        return result
    return list(result.content), result.structured_content


def text_of(blocks: list[Any]) -> str:
    return "\n".join(b.text for b in blocks if getattr(b, "text", None))


# ---------------------------------------------------------------------------
# Empty input names the argument that was wrong
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    ("tool", "args", "must_mention"),
    [
        # The whole phrase, not just the argument name. With the message
        # stripped, the SDK's fallback is "Error executing tool paper_graph" —
        # which contains "paper", so a name-only assertion passed on exactly
        # the failure it was written to catch.
        ("search", {"query": "   "}, "query must not be empty"),
        ("research", {"question": "  "}, "question must not be empty"),
        ("paper_graph", {"paper": " "}, "paper must not be empty"),
        # A blank source used to fall through to the local-file branch and come
        # back as "Local file reads are disabled; set SEARCH_MCP_DOCUMENT_ROOT"
        # — an answer to a question nobody asked, pointing the caller at a
        # sandbox they do not need.
        ("read_doc", {"source": "  "}, "source must not be empty"),
    ],
)
async def test_a_blank_argument_names_itself(tool, args, must_mention):
    with pytest.raises(Exception) as excinfo:
        await call(tool, args)
    assert must_mention in str(excinfo.value)


@pytest.mark.parametrize(
    ("tool", "args", "must_not_mention"),
    [
        # "no cached pages match" sends the caller to populate a cache that may
        # already be full; the mistake was the empty query.
        ("cache_search", {"query": ""}, "populate"),
    ],
)
async def test_a_blank_argument_does_not_blame_the_wrong_thing(
    tool, args, must_not_mention
):
    blocks, _ = await call(tool, args)
    assert must_not_mention not in text_of(blocks).lower()


async def test_empty_url_list_says_so_rather_than_returning_blank():
    """An empty string reads as "every fetch failed silently"."""
    blocks, _ = await call("fetch_batch", {"urls": []})
    body = text_of(blocks)
    assert body.strip()
    assert "no urls" in body.lower()


async def test_empty_query_still_returns_an_empty_list_in_json_mode():
    """The json shape must not change just because the input was rejected."""
    _blocks, structured = await call("cache_search", {"query": "", "format": "json"})
    assert structured == {"result": []}


# ---------------------------------------------------------------------------
# The header may not claim engines that produced nothing
# ---------------------------------------------------------------------------


@pytest.fixture
def stub_search(monkeypatch):
    """Replace the aggregator with a fixed payload — these assertions are about
    rendering, not about what any engine returns today."""
    from search_mcp import server as server_mod

    payload: dict[str, Any] = {}

    async def fake(query, **kw):
        return dict(payload)

    monkeypatch.setattr(server_mod, "aggregate_search", fake)
    return payload


async def test_header_names_the_engines_that_produced_the_results(stub_search):
    """`payload["engines"]` is the REQUEST — the list the cache key is built
    from. When a requested engine fails, the rescue pass substitutes another,
    and printing the request made the header claim `engines: serper` above ten
    results every per-result byline attributed to `bing`."""
    stub_search.update(
        query="x",
        engines=["serper"],
        results=[{"title": "T", "url": "https://e.example/1", "engines": ["bing"]}],
        errors={"serper": "serper not configured: ..."},
        rescued_via="bing",
    )
    body = text_of((await call("search", {"query": "x"}))[0])
    assert "_engines: bing_" in body
    assert "requested but contributed nothing: serper" in body


async def test_header_falls_back_to_the_request_when_nothing_came_back(stub_search):
    """With no results there is no attribution to read, and naming the request
    is the only honest thing left to say."""
    stub_search.update(query="x", engines=["mojeek"], results=[])
    body = text_of((await call("search", {"query": "x"}))[0])
    assert "_engines: mojeek_" in body
    assert "contributed nothing" not in body


async def test_no_disclaimer_when_every_requested_engine_contributed(stub_search):
    stub_search.update(
        query="x",
        engines=["mojeek"],
        results=[{"title": "T", "url": "https://e.example/1", "engines": ["mojeek"]}],
    )
    body = text_of((await call("search", {"query": "x"}))[0])
    assert "contributed nothing" not in body


# ---------------------------------------------------------------------------
# max_results is bounded
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    ("asked", "expected"),
    [
        # `n = max_results or default` turned an explicit 0 into the default:
        # a caller who asked for nothing got ten results and no sign anything
        # had been ignored.
        (0, 1),
        (-5, 1),
        (3, 3),
        # The number is also the PER-ENGINE budget, so it multiplies across the
        # fan-out; four figures buys duplicate noise with real latency.
        (9999, 50),
    ],
)
async def test_max_results_is_clamped(asked, expected, monkeypatch):
    from search_mcp import aggregator as agg

    seen: dict[str, int] = {}

    def fake_key(query, names, n, filters):
        seen["n"] = n
        return "k"

    monkeypatch.setattr(agg, "_key", fake_key)

    async def stop(*a, **kw):
        raise RuntimeError("stop after the clamp")

    monkeypatch.setattr(agg.cache, "get_search", stop)
    with pytest.raises(RuntimeError):
        await agg.aggregate_search("q", max_results=asked)
    assert seen["n"] == expected


async def test_max_results_none_uses_the_configured_default(monkeypatch):
    from search_mcp import aggregator as agg
    from search_mcp.config import settings

    seen: dict[str, int] = {}
    monkeypatch.setattr(agg, "_key", lambda q, names, n, f: seen.setdefault("n", n))

    async def stop(*a, **kw):
        raise RuntimeError("stop")

    monkeypatch.setattr(agg.cache, "get_search", stop)
    with pytest.raises(RuntimeError):
        await agg.aggregate_search("q", max_results=None)
    assert seen["n"] == settings.max_results_per_engine
