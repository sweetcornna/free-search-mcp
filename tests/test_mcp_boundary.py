"""Every tool, called the way a client calls it, in both protocol eras.

The rest of the suite mostly calls the tool FUNCTIONS. That is how 0.11.0
shipped with a green build and a broken product: the functions raised the right
`ValueError`s and returned the right values, while everything between the
function and the wire — the SDK's exception handling, result conversion and
schema validation — was exercised for five of the eleven tools and asserted on
for fewer. So:

  * the locked SDK was 2.0 while `uvx` resolved 2.2 for every real install, and
    from 2.1 a non-`ToolError` exception reaches the model as a bare
    "Error executing tool <name>". Every actionable message went dark.
  * markdown output reached the model as `{"result": "...\\n..."}` — a second,
    JSON-escaped copy inside `structuredContent` — because nothing asserted on
    what a client actually receives.

These tests drive `mcp.Client` against the real server object, so they see what
a client sees. They are hermetic: every network seam is stubbed, and a tripwire
fails the test if anything reaches for a socket anyway.
"""
from __future__ import annotations

import json
import socket
from typing import Any

import pytest
from mcp import Client

from search_mcp import fetcher
from search_mcp.config import settings

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.

# "legacy" is the SDK client's name for the handshake era (2025-11-25 and
# earlier, reached through `initialize`); the dated string is the stateless one.
ERAS = ["2026-07-28", "legacy"]

ALL_TOOLS = [
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

_PAGE_BODY = "# A page\n\nFirst paragraph of the body.\n\nSecond paragraph."


def _page(url: str, **kw: Any) -> fetcher.FetchResult:
    base = {
        "url": url,
        "title": "A page",
        "content": _PAGE_BODY,
        "method": "http",
        "truncated": False,
        "tokens_estimated": 12,
        "published_date": "2026-05-01",
        "sitename": "Example",
    }
    base.update(kw)
    return fetcher.FetchResult(**base)


_SEARCH_PAYLOAD = {
    "query": "boundary test",
    "engines": ["duckduckgo"],
    "cached": False,
    "results": [
        {
            "title": "First hit",
            "url": "https://example.org/first",
            "snippet": "A snippet about the boundary test, long enough to matter.",
            "engines": ["duckduckgo"],
            "score": 0.01639,
        },
        {
            "title": "Second hit",
            "url": "https://example.org/second",
            "snippet": "Another snippet.",
            "engines": ["duckduckgo"],
            "score": 0.01613,
        },
    ],
    "lead_snippet": None,
    "errors": None,
}

_JSON_LD_HTML = (
    b"<html><head><title>Event</title>"
    b'<script type="application/ld+json">'
    b'{"@context":"https://schema.org","@type":"Event","name":"Finals",'
    b'"startDate":"2026-10-15"}</script></head><body>hi</body></html>'
)


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture(autouse=True)
def _no_network(monkeypatch):
    """Tripwire. A stub that is missing, or a seam that moved, must fail loudly
    here rather than quietly succeed against the live web on a developer's
    machine and time out in CI. `AssertionError` is not an anticipated tool
    error, so it surfaces as an internal error and fails the happy-path checks.
    """

    async def _no_http(*a: Any, **kw: Any):
        raise AssertionError("hermetic test reached the HTTP fetcher")

    async def _no_browser(*a: Any, **kw: Any):
        raise AssertionError("hermetic test reached the browser pool")

    real_connect = socket.socket.connect

    def _no_connect(self, address):
        # AF_UNIX stays usable: the event loop and aiosqlite are entitled to
        # their own plumbing. Only a real network connection is the bug.
        if self.family in (socket.AF_INET, socket.AF_INET6):
            raise AssertionError(f"hermetic test opened a socket to {address!r}")
        return real_connect(self, address)

    from search_mcp.browser import pool

    monkeypatch.setattr(fetcher, "_http_fetch", _no_http)
    monkeypatch.setattr(pool, "fetch_html", _no_browser)
    monkeypatch.setattr(socket.socket, "connect", _no_connect)


@pytest.fixture
async def isolated_cache(tmp_path, monkeypatch):
    from search_mcp import cache as cache_mod
    from search_mcp import server as server_mod

    fresh = cache_mod.Cache()
    fresh._path = str(tmp_path / "boundary_cache.sqlite")
    monkeypatch.setattr(cache_mod, "cache", fresh)
    monkeypatch.setattr(server_mod, "cache", fresh)
    yield fresh
    await fresh.close()


@pytest.fixture
def stubbed(monkeypatch, tmp_path, isolated_cache):
    """Stub every network seam so all eleven tools can run offline.

    Returns the argument set each tool is called with."""
    from search_mcp import paper_graph as pg
    from tests.test_paper_graph import _Api

    async def fake_search(query, **kw):
        return dict(_SEARCH_PAYLOAD, query=query)

    async def fake_fetch_page(url, **kw):
        return _page(url)

    async def fake_fetch_many(urls, *a, **kw):
        return [_page(u) for u in urls]

    async def fake_stream(client, url, *, raise_for_status):
        return 200, "text/html; charset=utf-8", _JSON_LD_HTML

    async def fake_fetch_bytes(url):
        return fetcher.FetchResult(
            url=url,
            title="a.bin",
            content="",
            method="asset",
            truncated=False,
            media_type="application/octet-stream",
            bytes_size=4,
            sha256="abc",
            data=b"data",
        )

    monkeypatch.setattr("search_mcp.server.aggregate_search", fake_search)
    monkeypatch.setattr("search_mcp.research.aggregate_search", fake_search)
    monkeypatch.setattr("search_mcp.research.fetch_many", fake_fetch_many)
    monkeypatch.setattr("search_mcp.server.fetch_page", fake_fetch_page)
    monkeypatch.setattr("search_mcp.server.fetch_many", fake_fetch_many)
    monkeypatch.setattr("search_mcp.compare.fetch_many", fake_fetch_many)
    monkeypatch.setattr("search_mcp.structured.httpx_stream_capped", fake_stream)
    monkeypatch.setattr("search_mcp.server.fetch_bytes", fake_fetch_bytes)
    monkeypatch.setattr(pg._api, "_get_json", _Api())

    monkeypatch.setattr(settings, "document_root", tmp_path)
    doc = tmp_path / "doc.txt"
    doc.write_text("line one\nline two\nline three\n", encoding="utf-8")
    monkeypatch.setattr(settings, "download_enabled", True)
    monkeypatch.setattr(settings, "download_dir", tmp_path / "dl")

    return {
        "search": {"query": "boundary test"},
        "fetch": {"url": "https://example.org/first"},
        "fetch_batch": {"urls": ["https://example.org/a", "https://example.org/b"]},
        "read_doc": {"source": str(doc)},
        "research": {"question": "boundary test", "depth": 2},
        "paper_graph": {"paper": "10.1145/1571941.1572114", "limit": 2},
        "cache_search": {"query": "paragraph"},
        "engines": {"group": "news"},
        "compare": {
            "question": "which is newer?",
            "urls": ["https://example.org/a", "https://example.org/b"],
        },
        "extract_structured": {"url": "https://example.org/event"},
        "download": {"url": "https://example.org/a.bin"},
    }


def _texts(result: Any) -> list[str]:
    return [b.text for b in result.content if getattr(b, "type", "") == "text"]


# ---------------------------------------------------------------------------
# tools/list
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("era", ERAS)
async def test_both_eras_list_all_eleven_tools(era):
    from search_mcp.server import mcp

    async with Client(mcp, mode=era) as client:
        listed = await client.list_tools()
    assert [t.name for t in listed.tools] == ALL_TOOLS


# ---------------------------------------------------------------------------
# Happy path, markdown: one plain text block
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("era", ERAS)
@pytest.mark.parametrize("tool", ALL_TOOLS)
async def test_markdown_arrives_as_plain_text(tool, era, stubbed, isolated_cache):
    """The default output has to reach the model as prose.

    `structured_content is None` is the point: a client that prefers structured
    content shows the model THAT, and a markdown body in there is one JSON
    string with every newline escaped."""
    from search_mcp.server import mcp

    if tool == "cache_search":
        await isolated_cache.put_page("https://example.org/first", "A page", _PAGE_BODY)

    async with Client(mcp, mode=era) as client:
        result = await client.call_tool(tool, stubbed[tool])

    assert not result.is_error, _texts(result)
    assert result.structured_content is None
    texts = _texts(result)
    assert len(result.content) == 1 and len(texts) == 1, result.content
    body = texts[0]
    assert body.strip(), f"{tool} returned an empty body"
    assert "\n" in body, f"{tool}: markdown should carry real newlines"
    assert not body.lstrip().startswith(("{", "[")), f"{tool} sent JSON as its markdown"


# ---------------------------------------------------------------------------
# Happy path, json: structured, unwrapped, and mirrored in the text block
# ---------------------------------------------------------------------------

# Tools whose json payload is a list. `structuredContent` must be an object on
# the wire, so these are the only ones that keep a `{"result": [...]}` envelope.
_LIST_PAYLOAD_TOOLS = {"fetch_batch", "cache_search"}


@pytest.mark.parametrize("era", ERAS)
@pytest.mark.parametrize("tool", ALL_TOOLS)
async def test_json_arrives_structured(tool, era, stubbed, isolated_cache):
    from search_mcp.server import mcp

    if tool == "cache_search":
        await isolated_cache.put_page("https://example.org/first", "A page", _PAGE_BODY)

    async with Client(mcp, mode=era) as client:
        listed = {t.name: t for t in (await client.list_tools()).tools}
        result = await client.call_tool(tool, {**stubbed[tool], "format": "json"})

    assert not result.is_error, _texts(result)
    texts = _texts(result)
    assert len(texts) == 1
    parsed = json.loads(texts[0])
    if tool in _LIST_PAYLOAD_TOOLS:
        assert isinstance(parsed, list)
        assert result.structured_content == {"result": parsed}
    else:
        assert isinstance(parsed, dict)
        assert result.structured_content == parsed
        assert set(parsed) != {"result"}, f"{tool} still wraps its payload"

    # Whatever a tool advertises, it must honour — the SDK validates on the
    # way out, but only against a schema it derived. Today that is none of
    # them; if one ever returns, this keeps the pair honest.
    schema = listed[tool].output_schema
    if schema is not None:
        import jsonschema

        jsonschema.validate(result.structured_content, schema)


@pytest.mark.parametrize("era", ERAS)
async def test_inline_image_arrives_as_image_content(era, stubbed, monkeypatch):
    from search_mcp.server import mcp

    # 1x1 transparent PNG.
    png = bytes.fromhex(
        "89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c489"
        "0000000d49444154789c6360000002000001e221bc330000000049454e44ae426082"
    )

    async def fake_fetch_page(url, **kw):
        return _page(url, method="asset", media_type="image/png", data=png, content="a.png")

    monkeypatch.setattr("search_mcp.server.fetch_page", fake_fetch_page)

    async with Client(mcp, mode=era) as client:
        result = await client.call_tool(
            "fetch", {"url": "https://example.org/a.png", "inline": True}
        )

    assert not result.is_error, _texts(result)
    assert [b.type for b in result.content] == ["image"]
    assert result.content[0].mime_type == "image/png"


# ---------------------------------------------------------------------------
# Errors: the actionable text must reach the client
# ---------------------------------------------------------------------------

# (tool, args, phrase that must survive). The PHRASE, not the argument name:
# "Error executing tool paper_graph" contains "paper", which is how the older
# contract test kept passing while the message itself was gone.
_ERRORS = [
    ("search", {"query": "   "}, "query must not be empty"),
    ("research", {"question": " "}, "question must not be empty"),
    ("paper_graph", {"paper": " "}, "paper must not be empty"),
    ("read_doc", {"source": "  "}, "source must not be empty"),
    ("read_doc", {"source": "https://example.org/a.pdf", "start": -1}, "start must be >= 0"),
    ("fetch_batch", {"urls": ["https://example.org/x"] * 21}, "at most 20 URLs"),
    ("compare", {"question": "q", "urls": ["https://example.org/a"]}, "compare expects 2-5 URLs"),
    (
        "compare",
        {"question": "q", "urls": [f"https://example.org/{i}" for i in range(6)]},
        "compare expects 2-5 URLs",
    ),
    ("fetch", {"url": "http://169.254.169.254/latest"}, "Refusing to connect to blocked address"),
    ("fetch", {"url": "ftp://example.org/file"}, "only http and https are allowed"),
    ("extract_structured", {"url": "http://127.0.0.1/"}, "Refusing to connect"),
    ("download", {"url": "http://10.0.0.1/a.bin"}, "Refusing to connect"),
]


@pytest.mark.parametrize("era", ERAS)
@pytest.mark.parametrize(("tool", "args", "phrase"), _ERRORS)
async def test_anticipated_errors_keep_their_message(tool, args, phrase, era):
    from search_mcp.server import mcp

    async with Client(mcp, mode=era) as client:
        result = await client.call_tool(tool, args)

    assert result.is_error
    text = "\n".join(_texts(result))
    assert phrase in text, text


@pytest.mark.parametrize("era", ERAS)
async def test_local_read_refusals_keep_their_message(era, tmp_path, monkeypatch):
    from search_mcp.server import mcp

    outside = tmp_path / "outside.txt"
    outside.write_text("secret", encoding="utf-8")
    root = tmp_path / "root"
    root.mkdir()
    (root / "pic.png").write_bytes(b"\x89PNG\r\n\x1a\n")
    (root / "blob.xyz").write_bytes(b"\x00\x01")
    (root / "notes.txt").write_text("some notes\n", encoding="utf-8")

    async with Client(mcp, mode=era) as client:
        disabled = await client.call_tool("read_doc", {"source": str(outside)})
        monkeypatch.setattr(settings, "document_root", root)
        escaped = await client.call_tool("read_doc", {"source": str(outside)})
        file_url = await client.call_tool("read_doc", {"source": f"file://{outside}"})
        missing = await client.call_tool("read_doc", {"source": "nope.txt"})
        image = await client.call_tool("read_doc", {"source": "pic.png"})
        unknown = await client.call_tool("read_doc", {"source": "blob.xyz"})
        negative = await client.call_tool("read_doc", {"source": "notes.txt", "length": -1})

    for result, phrase in [
        (disabled, "Local file reads are disabled"),
        (escaped, "resolves outside the document_root sandbox"),
        (file_url, "file:// URLs are not allowed"),
        (missing, "No such file under SEARCH_MCP_DOCUMENT_ROOT"),
        (image, "is an image. Use `fetch`"),
        (unknown, "Unsupported document format"),
        (negative, "length must be >= 0"),
    ]:
        assert result.is_error
        assert phrase in "\n".join(_texts(result)), _texts(result)


@pytest.mark.parametrize("era", ERAS)
async def test_disabled_downloads_say_which_setting_disabled_them(era, monkeypatch):
    from search_mcp.server import mcp

    monkeypatch.setattr(settings, "download_enabled", False)
    async with Client(mcp, mode=era) as client:
        result = await client.call_tool("download", {"url": "https://example.org/a.bin"})
    assert result.is_error
    assert "SEARCH_MCP_DOWNLOAD_ENABLED=false" in "\n".join(_texts(result))


@pytest.mark.parametrize("era", ERAS)
async def test_a_failed_fetch_says_why(era, monkeypatch):
    """The fetch path's own failures are `FetchError`s, not bare RuntimeErrors,
    precisely so they can cross the boundary with their text."""
    from search_mcp.server import mcp

    async def failing(url, **kw):
        raise fetcher.FetchError(f"empty response for {url}: HTTP Error 404: ")

    monkeypatch.setattr("search_mcp.server.fetch_page", failing)
    async with Client(mcp, mode=era) as client:
        result = await client.call_tool("fetch", {"url": "https://example.org/gone"})
    assert result.is_error
    assert "empty response for https://example.org/gone" in "\n".join(_texts(result))


@pytest.mark.parametrize("era", ERAS)
async def test_an_unexpected_crash_does_not_leak_its_text(era, monkeypatch):
    """The other half of the contract. A bug's message is not for the model:
    it cannot act on it, and it may carry something it should not see."""
    from search_mcp.server import mcp

    async def crashing(query, **kw):
        raise KeyError("s3cr3t-internal-key")

    monkeypatch.setattr("search_mcp.server.aggregate_search", crashing)
    async with Client(mcp, mode=era) as client:
        result = await client.call_tool("search", {"query": "anything"})

    assert result.is_error
    text = "\n".join(_texts(result))
    assert "s3cr3t" not in text
    assert "internal error (KeyError)" in text
    assert "retrying the same call will not help" in text


async def test_direct_callers_still_get_native_exceptions_and_values(stubbed):
    """`_tool` registers a wrapper and returns the original. Anything importing
    the function — most of this suite — keeps builtin exceptions and plain
    return values, with no MCP types in sight."""
    from search_mcp.server import compare, search

    with pytest.raises(ValueError, match="query must not be empty"):
        await search("  ")
    with pytest.raises(ValueError, match="compare expects 2-5 URLs"):
        await compare("q", ["https://example.org/a"])
    assert isinstance(await search("boundary test"), str)
    assert isinstance(await search("boundary test", format="json"), dict)


# ---------------------------------------------------------------------------
# Resource links
# ---------------------------------------------------------------------------

# A URL that legitimately contains percent-escapes (any non-ASCII Wikipedia
# title does). `cached_page` used to decode the template variable a second
# time, so a URL like this could be cached and never read back.
_ESCAPED_URL = "https://zh.wikipedia.org/wiki/%E6%90%9C%E7%B4%A2%E5%BC%95%E6%93%8E"
_FULL_BODY = "# 搜索引擎\n\n" + "正文。" * 400


@pytest.mark.parametrize("era", ERAS)
async def test_a_truncated_page_links_to_its_full_text_and_the_link_reads_back(
    era, monkeypatch, isolated_cache
):
    from search_mcp.server import mcp

    await isolated_cache.put_page(_ESCAPED_URL, "搜索引擎", _FULL_BODY)

    async def truncated_page(url, **kw):
        return _page(url, content=_FULL_BODY[:200], truncated=True)

    monkeypatch.setattr("search_mcp.server.fetch_page", truncated_page)

    async with Client(mcp, mode=era) as client:
        result = await client.call_tool("fetch", {"url": _ESCAPED_URL})
        assert [b.type for b in result.content] == ["text", "resource_link"]
        link = result.content[-1]
        # Encoded so it fits an RFC 6570 `{url}`: no `/` or `:` left in it.
        assert str(link.uri).startswith("cache://page/https%3A%2F%2Fzh.wikipedia.org")
        full = await client.read_resource(str(link.uri))

    assert full.contents[0].text == _FULL_BODY


@pytest.mark.parametrize("era", ERAS)
async def test_a_complete_page_and_an_asset_carry_no_link(era, stubbed, monkeypatch):
    from search_mcp.server import mcp

    async with Client(mcp, mode=era) as client:
        whole = await client.call_tool("fetch", stubbed["fetch"])
    assert [b.type for b in whole.content] == ["text"]

    async def truncated_asset(url, **kw):
        return _page(url, method="asset", media_type="video/mp4", truncated=True, content="a.mp4")

    monkeypatch.setattr("search_mcp.server.fetch_page", truncated_asset)
    async with Client(mcp, mode=era) as client:
        asset = await client.call_tool("fetch", {"url": "https://example.org/a.mp4"})
    assert [b.type for b in asset.content] == ["text"]


def test_links_are_withheld_from_clients_that_predate_them():
    """`resource_link` arrived in 2025-06-18 and the SDK does not downgrade
    content blocks, so an older client would be handed a block it cannot parse."""
    from types import SimpleNamespace

    from search_mcp.server import _supports_links

    assert _supports_links(SimpleNamespace(protocol_version="2026-07-28"))
    assert _supports_links(SimpleNamespace(protocol_version="2025-06-18"))
    assert not _supports_links(SimpleNamespace(protocol_version="2025-03-26"))
    assert not _supports_links(SimpleNamespace(protocol_version=None))
    assert not _supports_links(None), "a direct Python caller gets the plain value"


@pytest.mark.parametrize("era", ERAS)
async def test_a_json_search_links_to_its_cached_result_set(era, stubbed, monkeypatch, isolated_cache):
    from search_mcp.server import mcp

    rows = _SEARCH_PAYLOAD["results"]
    await isolated_cache.put_search("abc123", "boundary test", ["duckduckgo"], rows)

    async def fake_search(query, **kw):
        return dict(_SEARCH_PAYLOAD, query=query, cache_key="abc123")

    monkeypatch.setattr("search_mcp.server.aggregate_search", fake_search)

    async with Client(mcp, mode=era) as client:
        as_json = await client.call_tool("search", {"query": "boundary test", "format": "json"})
        as_markdown = await client.call_tool("search", {"query": "boundary test"})
        link = as_json.content[-1]
        assert link.type == "resource_link" and str(link.uri) == "cache://search/abc123"
        replay = await client.read_resource(str(link.uri))

    assert json.loads(replay.contents[0].text) == rows
    # Markdown is for reading, not for storing: no link, no extra tokens.
    assert [b.type for b in as_markdown.content] == ["text"]


# ---------------------------------------------------------------------------
# Completion
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("era", ERAS)
async def test_prompt_arguments_complete_from_the_real_literals(era):
    from mcp.types import PromptReference

    from search_mcp.server import mcp

    async with Client(mcp, mode=era) as client:
        category = await client.complete(
            PromptReference(type="ref/prompt", name="research_prompt"),
            {"name": "category", "value": "paper."},
        )
        since = await client.complete(
            PromptReference(type="ref/prompt", name="news_brief"), {"name": "since", "value": ""}
        )
        unknown = await client.complete(
            PromptReference(type="ref/prompt", name="factcheck_prompt"),
            {"name": "claim", "value": "x"},
        )

    assert "paper.biomed" in category.completion.values
    assert all(v.startswith("paper.") for v in category.completion.values)
    assert since.completion.values == ["day", "week", "month", "year"]
    assert unknown.completion.values == []


@pytest.mark.parametrize("era", ERAS)
async def test_cached_urls_complete_in_the_only_form_the_template_accepts(era, isolated_cache):
    from mcp.types import ResourceTemplateReference

    from search_mcp.server import mcp

    await isolated_cache.put_page("https://docs.python.org/3/library/asyncio.html", "asyncio", "x")
    await isolated_cache.put_page("https://example.org/100%25_done", "pct", "y")
    await isolated_cache.put_search("deadbeef01", "q", ["duckduckgo"], [])

    async with Client(mcp, mode=era) as client:
        pages = await client.complete(
            ResourceTemplateReference(type="ref/resource", uri="cache://page/{url}"),
            {"name": "url", "value": "python.org"},
        )
        literal = await client.complete(
            ResourceTemplateReference(type="ref/resource", uri="cache://page/{url}"),
            {"name": "url", "value": "100%25_d"},
        )
        wildcard = await client.complete(
            ResourceTemplateReference(type="ref/resource", uri="cache://page/{url}"),
            {"name": "url", "value": "d_cs.pyth%n"},
        )
        encoded = await client.complete(
            ResourceTemplateReference(type="ref/resource", uri="cache://page/{url}"),
            {"name": "url", "value": "https%3A%2F%2Fdocs"},
        )
        searches = await client.complete(
            ResourceTemplateReference(type="ref/resource", uri="cache://search/{query_hash}"),
            {"name": "query_hash", "value": "dead"},
        )
        value = pages.completion.values[0]
        body = await client.read_resource(f"cache://page/{value}")

    assert pages.completion.values == ["https%3A%2F%2Fdocs.python.org%2F3%2Flibrary%2Fasyncio.html"]
    assert body.contents[0].text == "x", "a completed value must be usable as-is"
    # A cached URL that itself contains an escape is matched as typed...
    assert literal.completion.values == ["https%3A%2F%2Fexample.org%2F100%2525_done"]
    # ...`%` and `_` are characters, not LIKE wildcards...
    assert wildcard.completion.values == []
    # ...and a fragment typed in the encoded form still finds its page.
    assert encoded.completion.values == pages.completion.values
    assert searches.completion.values == ["deadbeef01"]


# ---------------------------------------------------------------------------
# `engines(group=...)` and the server's identity
# ---------------------------------------------------------------------------


def test_the_engine_group_enum_is_the_taxonomy():
    from typing import get_args

    from search_mcp.engines import source_taxonomy
    from search_mcp.server import EngineGroup

    assert set(get_args(EngineGroup)) == set(source_taxonomy())


@pytest.mark.parametrize("era", ERAS)
async def test_a_mistyped_group_is_an_error_that_lists_the_choices(era):
    from search_mcp.server import mcp

    async with Client(mcp, mode=era) as client:
        typo = await client.call_tool("engines", {"group": "papers"})
        filter_only = await client.call_tool("engines", {"group": "pdf"})
        dotted = await client.call_tool("engines", {"group": "paper.biomed"})

    assert typo.is_error and "'paper'" in "\n".join(_texts(typo))
    assert filter_only.is_error
    assert "is a result filter, not a source group" in "\n".join(_texts(filter_only))
    assert not dotted.is_error and "pubmed" in "\n".join(_texts(dotted))


async def test_the_server_describes_itself():
    from search_mcp.server import _CACHE_HINTS, mcp

    # The handshake era is where a client holds serverInfo as an object; the
    # stateless era carries the same fields in each result's `_meta`.
    async with Client(mcp, mode="legacy") as client:
        info = client.server_info

    assert info.title == "Free Search"
    assert "no API key" in info.description
    # serverInfo rides in every modern-era result's _meta: a URL, never bytes.
    assert [i.src.startswith("https://") for i in info.icons] == [True]
    assert "server/discover" in _CACHE_HINTS
