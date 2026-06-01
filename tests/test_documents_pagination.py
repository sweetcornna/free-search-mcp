"""Pagination semantics on read_document."""
import pytest

pytestmark = pytest.mark.asyncio


@pytest.fixture(autouse=True)
def _sandbox(tmp_path, monkeypatch):
    """Local reads are disabled by default; opt into a sandbox at tmp_path.

    All tests in this module read files they write under tmp_path, so we point
    settings.document_root at tmp_path to exercise the (now opt-in) local path.
    """
    from search_mcp import config, documents
    monkeypatch.setattr(config.settings, "document_root", tmp_path)
    monkeypatch.setattr(documents.settings, "document_root", tmp_path)
    yield


async def test_pagination_slices_offset(tmp_path):
    from search_mcp.documents import read_document
    p = tmp_path / "long.txt"
    p.write_text("A" * 1000 + "B" * 1000, encoding="utf-8")

    head = await read_document(str(p), start=0, length=500)
    assert head.returned_chars == 500
    assert head.start == 0
    assert head.total_chars == 2000
    assert head.truncated is True  # didn't read it all
    assert head.content[:5] == "AAAAA"

    middle = await read_document(str(p), start=900, length=200)
    assert middle.start == 900
    assert middle.returned_chars == 200
    assert middle.content.startswith("A")
    assert "B" in middle.content


async def test_pagination_zero_length_returns_to_end(tmp_path):
    from search_mcp.documents import read_document
    p = tmp_path / "small.txt"
    p.write_text("hi there", encoding="utf-8")
    out = await read_document(str(p))
    assert out.start == 0
    assert out.total_chars == 8
    assert out.returned_chars == 8
    assert out.truncated is False


async def test_tokens_estimated_present(tmp_path):
    from search_mcp.documents import read_document
    p = tmp_path / "x.txt"
    p.write_text("the quick brown fox jumps over the lazy dog", encoding="utf-8")
    out = await read_document(str(p))
    assert out.tokens_estimated > 0
