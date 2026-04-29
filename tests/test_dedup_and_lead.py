"""Offline tests for the title-fuzzy + host-canonical dedup pass and the
extractive lead_snippet helper.

Both helpers live in `aggregator.py` and are pure / sync — no event loop,
no network. Tests stay deliberately small so a regression here points at
exactly the helper that broke.
"""
from __future__ import annotations

from search_mcp.aggregator import (
    _canonical_host,
    _dedup_by_title,
    _lead_snippet,
)
from search_mcp.formatting import render_search


# ---------------------------------------------------------------------------
# _canonical_host
# ---------------------------------------------------------------------------


def test_canonical_host_strips_www_and_normalizes_co_uk():
    assert _canonical_host("https://www.bbc.co.uk/news") == "bbc.com"


def test_canonical_host_strips_amp_prefix():
    assert _canonical_host("https://amp.example.com/x") == "example.com"


def test_canonical_host_strips_m_mobile_prefix():
    assert _canonical_host("https://m.example.com/x") == "example.com"
    assert _canonical_host("https://mobile.example.com/x") == "example.com"


def test_canonical_host_collapses_country_tlds():
    assert _canonical_host("https://news.co.jp/foo") == "news.com"
    assert _canonical_host("https://shop.com.au/foo") == "shop.com"
    assert _canonical_host("https://times.co.in/foo") == "times.com"


def test_canonical_host_handles_missing_scheme_gracefully():
    # urlparse without scheme returns None for hostname — must not crash.
    assert _canonical_host("not-a-url") == ""


def test_canonical_host_leaves_plain_com_untouched():
    assert _canonical_host("https://example.com/x") == "example.com"


# ---------------------------------------------------------------------------
# _dedup_by_title
# ---------------------------------------------------------------------------


def test_dedup_removes_near_duplicate_title_on_same_canonical_host():
    items = [
        {"title": "Major Storm Hits East Coast Today", "url": "https://www.bbc.com/news/storm"},
        # Different host string, same canonical host, same tokens reordered
        # (token_set_ratio handles word reordering / extra punctuation > 92).
        {"title": "Major Storm Hits East Coast Today.", "url": "https://www.bbc.co.uk/news/storm-uk"},
    ]
    out = _dedup_by_title(items)
    assert len(out) == 1
    assert out[0]["url"] == "https://www.bbc.com/news/storm"


def test_dedup_keeps_differing_titles_on_same_host():
    items = [
        {"title": "Storm hits coast", "url": "https://www.bbc.com/news/storm"},
        {"title": "Election results 2026", "url": "https://www.bbc.com/news/election"},
    ]
    out = _dedup_by_title(items)
    assert len(out) == 2


def test_dedup_keeps_same_title_across_different_canonical_hosts():
    """Wire-story syndication: Reuters and AP both run the same headline.
    These are legitimately distinct sources — we must not collapse them.
    """
    items = [
        {"title": "Breaking: Major Storm Hits Coast", "url": "https://www.reuters.com/x"},
        {"title": "Breaking: Major Storm Hits Coast", "url": "https://www.apnews.com/x"},
    ]
    out = _dedup_by_title(items)
    assert len(out) == 2


def test_dedup_collapses_amp_variant_with_same_title():
    items = [
        {"title": "How async works in Python", "url": "https://www.example.com/post"},
        {"title": "How async works in Python", "url": "https://amp.example.com/post"},
    ]
    out = _dedup_by_title(items)
    assert len(out) == 1


def test_dedup_keeps_items_without_title():
    """Empty-title items are passed through verbatim — we have no signal to
    judge them on, and dropping them silently would surprise callers."""
    items = [
        {"title": "", "url": "https://a.com/1"},
        {"title": "", "url": "https://b.com/2"},
    ]
    out = _dedup_by_title(items)
    assert len(out) == 2


def test_dedup_empty_input():
    assert _dedup_by_title([]) == []


# ---------------------------------------------------------------------------
# _lead_snippet
# ---------------------------------------------------------------------------


def test_lead_snippet_picks_top3_with_two_query_terms_and_long_enough():
    results = [
        # Top result: snippet too short
        {"title": "x", "url": "https://a.com", "snippet": "tiny"},
        # 2nd: long enough, hits 2 query terms ('python' and 'async')
        {
            "title": "y",
            "url": "https://realpython.com/async-tutorial",
            "snippet": (
                "This guide walks through python async programming, "
                "covering coroutines and the event loop in detail."
            ),
        },
        # 3rd: also qualifies but should not win — first qualifier wins
        {
            "title": "z",
            "url": "https://docs.python.org",
            "snippet": (
                "The python docs explain async/await syntax with examples "
                "and runtime behavior for the curious reader."
            ),
        },
    ]
    out = _lead_snippet("python async tutorial guide", results)
    assert out is not None
    assert out.startswith("According to realpython.com:")
    assert "python async programming" in out


def test_lead_snippet_returns_none_when_no_snippet_qualifies():
    results = [
        {"title": "x", "url": "https://a.com", "snippet": "too short"},
        {"title": "y", "url": "https://b.com", "snippet": "another short one"},
        {"title": "z", "url": "https://c.com", "snippet": ""},
    ]
    assert _lead_snippet("python async tutorial guide", results) is None


def test_lead_snippet_requires_two_terms_not_one():
    results = [
        {
            "title": "x",
            "url": "https://a.com",
            "snippet": (
                "This is a long enough snippet that mentions python only once "
                "but never the other terms used in the query at all here."
            ),
        }
    ]
    # Only one query term ("python") appears — should be rejected.
    assert _lead_snippet("python async tutorial guide", results) is None


def test_lead_snippet_ignores_short_query_terms():
    # 'a', 'is', 'in' are <=3 chars and ignored. With nothing left, we bail.
    results = [
        {
            "title": "x",
            "url": "https://a.com",
            "snippet": "a long snippet that is in fact eighty plus chars long for the test to be valid input here.",
        }
    ]
    assert _lead_snippet("a is in", results) is None


def test_lead_snippet_only_inspects_top_3():
    results = [{"title": "x", "url": "https://a.com", "snippet": "short"}] * 3 + [
        {
            "title": "y",
            "url": "https://b.com",
            "snippet": (
                "This long snippet about python async programming would qualify, "
                "but it's the 4th result so we should never even look at it."
            ),
        }
    ]
    assert _lead_snippet("python async", results) is None


def test_lead_snippet_strips_www_prefix_from_host():
    results = [
        {
            "title": "x",
            "url": "https://www.example.com/post",
            "snippet": (
                "A long-form article about python async programming with "
                "concrete examples and benchmarks for various scenarios."
            ),
        }
    ]
    out = _lead_snippet("python async tutorial", results)
    assert out is not None
    assert out.startswith("According to example.com:")


# ---------------------------------------------------------------------------
# render_search integration
# ---------------------------------------------------------------------------


def test_render_search_includes_lead_block_when_present():
    payload = {
        "query": "python async",
        "engines": ["duckduckgo"],
        "results": [
            {
                "title": "Async Guide",
                "url": "https://example.com/x",
                "snippet": "A long snippet.",
                "engines": ["duckduckgo"],
                "score": 0.1,
            }
        ],
        "lead_snippet": "According to example.com: A long extractive snippet about python async programming.",
        "errors": None,
    }
    md = render_search(payload)
    assert "Lead:" in md
    assert "example.com: A long extractive snippet" in md
    # Lead must appear before the first result heading.
    assert md.index("Lead:") < md.index("## 1.")


def test_render_search_omits_lead_block_when_absent():
    payload = {
        "query": "x",
        "engines": ["duckduckgo"],
        "results": [
            {
                "title": "T",
                "url": "https://example.com/x",
                "snippet": "s",
                "engines": ["duckduckgo"],
                "score": 0.1,
            }
        ],
        "lead_snippet": None,
        "errors": None,
    }
    md = render_search(payload)
    assert "Lead:" not in md
