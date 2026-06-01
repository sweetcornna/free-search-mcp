"""Offline tests for formatting + truncation + token estimation."""
from search_mcp.formatting import (
    estimate_tokens,
    render_doc,
    render_fetch,
    render_research,
    render_search,
    smart_truncate,
)


def test_estimate_tokens_latin():
    assert estimate_tokens("hello world") > 0
    assert estimate_tokens("a" * 400) >= 90  # ~100 tokens, allow slack


def test_estimate_tokens_cjk():
    cjk_text = "中" * 100
    # CJK should be ~1 token per character.
    assert 80 <= estimate_tokens(cjk_text) <= 120


def test_estimate_tokens_empty():
    assert estimate_tokens("") == 0


def test_smart_truncate_under_limit():
    out, trunc = smart_truncate("short", 100)
    assert out == "short"
    assert trunc is False


def test_smart_truncate_paragraph_boundary():
    # Build a text where the paragraph break sits in the eligible zone
    # (>= 70% of the budget). 200 chars of para1, then \n\n, then 200 of para2;
    # budget = 250 puts the \n\n at index 200 / 250 = 80%.
    text = "paragraph one filler text content here. " * 5  # 200 chars
    text += "\n\n"
    text += "paragraph two starts here." * 8
    out, trunc = smart_truncate(text, 250)
    assert trunc is True
    assert out.endswith("[…truncated]")
    assert "paragraph two" not in out


def test_smart_truncate_falls_back_to_hard_cut():
    text = "no_boundaries_at_all_just_a_giant_word" * 5
    out, trunc = smart_truncate(text, 30)
    assert trunc is True
    assert len(out) <= 40  # tiny ellipsis allowed


def test_render_search_markdown_shape():
    payload = {
        "query": "rust async",
        "engines": ["duckduckgo"],
        "results": [
            {
                "title": "Tokio Tutorial",
                "url": "https://tokio.rs/tutorial",
                "snippet": "Learn async Rust.",
                "engines": ["duckduckgo"],
                "score": 0.0123,
            },
        ],
        "errors": None,
    }
    md = render_search(payload)
    assert "# Search: rust async" in md
    assert "Tokio Tutorial" in md
    assert "<https://tokio.rs/tutorial>" in md
    assert "score 0.0123" in md


def test_render_search_empty_results_gives_actionable_hint():
    md = render_search({"query": "asdfasdf", "engines": ["duckduckgo"], "results": [], "errors": None})
    assert "No results" in md
    assert "broader" in md.lower()


def test_render_fetch_includes_metadata_header():
    md = render_fetch({
        "url": "https://example.com",
        "title": "Example",
        "method": "http",
        "truncated": False,
        "tokens_estimated": 42,
        "content": "Body here.",
    })
    assert "# Example" in md
    assert "<https://example.com>" in md
    assert "~42 tokens" in md
    assert "Body here." in md


def test_render_doc_shows_pagination_slice():
    md = render_doc({
        "source": "/x.txt",
        "format": "text",
        "title": "",
        "content": "hello",
        "truncated": True,
        "pages": None,
        "tokens_estimated": 1,
        "total_chars": 100,
        "start": 0,
        "returned_chars": 50,
    })
    assert "slice [0:50]" in md
    assert "truncated" in md


def test_render_doc_full_read_has_no_slice_crumb():
    """A complete read (start=0, returned_chars==total_chars) must NOT print a
    'slice [...]' crumb — that crumb implies pagination that isn't happening (A7)."""
    md = render_doc({
        "source": "/x.txt",
        "format": "text",
        "title": "",
        "content": "hello world",
        "truncated": False,
        "pages": None,
        "tokens_estimated": 3,
        "total_chars": 11,
        "start": 0,
        "returned_chars": 11,
    })
    assert "slice [" not in md


def test_render_doc_subrange_still_shows_slice_crumb():
    """A genuine sub-range (returned_chars < total_chars) keeps the crumb."""
    md = render_doc({
        "source": "/x.txt",
        "format": "text",
        "title": "",
        "content": "hello",
        "truncated": True,
        "pages": None,
        "tokens_estimated": 1,
        "total_chars": 100,
        "start": 10,
        "returned_chars": 40,
    })
    assert "slice [10:50]" in md


def test_render_doc_start_at_zero_partial_shows_slice_crumb():
    """start=0 but returned_chars < total_chars is still a partial read -> crumb."""
    md = render_doc({
        "source": "/x.txt",
        "format": "text",
        "title": "",
        "content": "hello",
        "truncated": True,
        "pages": None,
        "tokens_estimated": 1,
        "total_chars": 100,
        "start": 0,
        "returned_chars": 50,
    })
    assert "slice [0:50]" in md


def test_render_research_lists_sources_and_documents():
    md = render_research({
        "question": "what is mcp",
        "engines": ["duckduckgo"],
        "sources": [{"rank": 1, "title": "MCP", "url": "https://mcp.io", "snippet": "spec"}],
        "documents": [{"url": "https://mcp.io", "title": "MCP", "content": "Content body.", "tokens_estimated": 5}],
        "tokens_estimated": 5,
    })
    assert "# Research brief: what is mcp" in md
    assert "## Sources" in md
    assert "## Documents" in md
    assert "Content body." in md
