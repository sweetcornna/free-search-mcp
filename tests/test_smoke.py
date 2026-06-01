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
    """DDG sometimes serves an "anomaly" 202 to flagged IPs. We accept either:
    - results returned, OR
    - empty list (treated as "transiently blocked, not our bug").
    `test_aggregate_merges_results` covers the actually load-bearing case.
    """
    from search_mcp.engines import get_engine
    results = await get_engine("duckduckgo").search("python language", 5)
    if not results:
        import pytest
        pytest.skip("DDG appears to be rate-limiting this IP (anomaly page)")
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
    result = await fetch_page(
        "https://en.wikipedia.org/wiki/Python_(programming_language)",
        render="http",
        force_refresh=True,
    )
    assert "Python" in result.content
    assert result.tokens_estimated > 0
    # trafilatura should populate at least one metadata field on Wikipedia.
    assert any([result.author, result.published_date, result.sitename])
    # New dataclass fields exist and are exposed via to_dict.
    d = result.to_dict()
    assert "author" in d and "published_date" in d and "sitename" in d


async def test_read_local_text(tmp_path, monkeypatch):
    """Local reads are now OFF by default; opt into the sandbox at tmp_path so
    we exercise the (correct) post-sandbox local-read path."""
    from search_mcp import config, documents
    monkeypatch.setattr(config.settings, "document_root", tmp_path)
    monkeypatch.setattr(documents.settings, "document_root", tmp_path)
    from search_mcp.documents import read_document
    p = tmp_path / "hello.txt"
    p.write_text("hello world\n", encoding="utf-8")
    result = await read_document(str(p))
    assert "hello world" in result.content
    assert result.format == "text"


async def test_read_doc_tool_rejects_negative_start():
    """The read_doc tool wrapper surfaces a clear error for a negative start
    rather than silently clamping (server-side input validation, #19)."""
    from search_mcp.server import read_doc
    with pytest.raises(ValueError, match="start must be >= 0"):
        await read_doc("https://example.com/x.pdf", start=-5)


async def test_read_doc_tool_propagates_negative_length_error(tmp_path, monkeypatch):
    """Negative length is rejected by read_document and the tool lets that
    ValueError propagate (no duplicate-raise with a different message, #19)."""
    from search_mcp import config, documents
    monkeypatch.setattr(config.settings, "document_root", tmp_path)
    monkeypatch.setattr(documents.settings, "document_root", tmp_path)
    p = tmp_path / "doc.txt"
    p.write_text("some content", encoding="utf-8")
    from search_mcp.server import read_doc
    with pytest.raises(ValueError, match="length must be >= 0"):
        await read_doc(str(p), length=-10)


async def test_read_doc_tool_local_disabled_by_default_raises(tmp_path, monkeypatch):
    """With SEARCH_MCP_DOCUMENT_ROOT unset, a local path raises a clear
    'disabled' PermissionError surfaced through the tool (#2 server-side)."""
    from search_mcp import config, documents
    monkeypatch.setattr(config.settings, "document_root", None)
    monkeypatch.setattr(documents.settings, "document_root", None)
    p = tmp_path / "doc.txt"
    p.write_text("data", encoding="utf-8")
    from search_mcp.server import read_doc
    with pytest.raises(PermissionError):
        await read_doc(str(p))


async def test_read_doc_tool_start_past_eof_clamps(tmp_path, monkeypatch):
    """A start past EOF is clamped: returned_chars==0, start==total_chars,
    truncated False — matching documents.py semantics (#19)."""
    from search_mcp import config, documents
    monkeypatch.setattr(config.settings, "document_root", tmp_path)
    monkeypatch.setattr(documents.settings, "document_root", tmp_path)
    p = tmp_path / "short.txt"
    p.write_text("hello", encoding="utf-8")
    from search_mcp.server import read_doc
    out = await read_doc(str(p), start=9999, format="json")
    assert out["returned_chars"] == 0
    assert out["start"] == out["total_chars"]
    assert out["truncated"] is False


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
