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
    _lead_query_terms,
    _lead_snippet,
    _merge,
)
from search_mcp.engines import SearchResult
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


def test_dedup_keeps_version_bumps_on_same_host():
    # Near-identical titles differing only by a version/year/quantity number must
    # NOT be collapsed — the fuzzy ratio scores them >=92 but they are distinct
    # results. Guards the numeric-token exception in _dedup_by_title.
    items = [
        {"title": "Python 3.13 released", "url": "https://www.python.org/downloads/release/3130"},
        {"title": "Python 3.12 released", "url": "https://www.python.org/downloads/release/3120"},
    ]
    out = _dedup_by_title(items)
    assert len(out) == 2

    items = [
        {"title": "The 6 best mechanical keyboards of 2026", "url": "https://www.nytimes.com/a"},
        {"title": "The 6 best mechanical keyboards of 2025", "url": "https://www.nytimes.com/b"},
    ]
    assert len(_dedup_by_title(items)) == 2


def test_dedup_still_collapses_when_numbers_match():
    # Same digit-tokens + near-identical wording on the same host is still a dup
    # (AMP/mobile/cc-TLD syndication of the SAME story).
    items = [
        {"title": "Fed holds rates at 5% in 2026", "url": "https://www.bbc.com/news/x"},
        {"title": "Fed holds rates at 5% in 2026.", "url": "https://amp.bbc.com/news/x"},
    ]
    assert len(_dedup_by_title(items)) == 1


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
# _merge — dedup must run BEFORE the slice so backfill keeps max_results (#7)
# ---------------------------------------------------------------------------


def _sr(title: str, url: str, rank: int, engine: str = "e1", snippet: str = "snip") -> SearchResult:
    return SearchResult(title=title, url=url, snippet=snippet, engine=engine, rank=rank)


def test_merge_backfills_after_title_dedup_to_keep_max_results():
    """The top `max_results` slots contain a same-host title-duplicate pair, and
    there are extra unique URLs ranked just below the cutoff. Dedup must run over
    the FULL ranked list and THEN slice, so the duplicate is replaced by the next
    unique result rather than leaving us short.

    With max_results=3 and a duplicate inside the top-3, the OLD (slice-then-dedup)
    code returned only 2 results. The fix backfills to a full 3.
    """
    # Lower rank => higher RRF score => higher in the ranked list.
    bucket = [
        _sr("How async works in Python", "https://www.example.com/post", rank=0),
        # Same canonical host + near-identical title -> a title duplicate that
        # sits in the top-3 by score.
        _sr("How async works in Python", "https://amp.example.com/post", rank=1),
        _sr("Totally different unique article", "https://other.com/a", rank=2),
        # Just below the cutoff — must be pulled up to backfill the dropped dup.
        _sr("Another distinct article here", "https://third.com/b", rank=3),
    ]
    out = _merge([bucket], max_results=3)
    assert len(out) == 3, "dedup-before-slice must backfill to a full max_results"
    urls = {r["url"] for r in out}
    # Exactly one of the example.com variants survives.
    assert len({u for u in urls if "example.com" in u}) == 1
    # The previously-below-cutoff unique result was pulled in.
    assert "https://third.com/b" in urls


def test_merge_never_exceeds_max_results():
    bucket = [
        _sr(f"Unique title {i}", f"https://site{i}.com/x", rank=i) for i in range(10)
    ]
    out = _merge([bucket], max_results=4)
    assert len(out) == 4


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


def test_lead_snippet_attributes_googlenews_to_real_outlet():
    # A GoogleNews lead carries an opaque news.google.com URL, but the outlet is
    # in the title as "(Reuters)" — the lead must attribute to that, not to
    # "news.google.com".
    results = [
        {
            "title": "OpenAI ships a new python async runtime (Reuters)",
            "url": "https://news.google.com/rss/articles/CBMiABC123",
            "snippet": (
                "The new python async runtime overhauls coroutine scheduling "
                "and the event loop for high-throughput inference workloads."
            ),
        },
    ]
    out = _lead_snippet("python async runtime", results)
    assert out is not None
    assert out.startswith("According to Reuters:")
    assert "news.google.com" not in out


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


# ---------------------------------------------------------------------------
# CJK-aware lead_snippet (B fix)
# ---------------------------------------------------------------------------
def test_lead_query_terms_cjk_emits_bigrams():
    terms = _lead_query_terms("DeepSeek MoE 模型架构")
    assert "deepseek" in terms          # ASCII >3 kept
    assert "moe" not in terms            # ASCII len 3, dropped
    # CJK bigrams from "模型架构"
    assert "模型" in terms
    assert "型架" in terms
    assert "架构" in terms


def test_lead_query_terms_pure_ascii_unchanged():
    # Regression guard: English queries still tokenize the old way.
    terms = _lead_query_terms("python async tutorial guide")
    assert terms == {"python", "async", "tutorial", "guide"}


def test_lead_query_terms_skips_solo_cjk_char():
    terms = _lead_query_terms("python 是 best")
    # "是" is a single CJK char alone — too generic, dropped.
    assert "是" not in terms
    assert terms == {"python", "best"}


def test_lead_snippet_picks_chinese_query_when_bigrams_match():
    results = [
        {
            "title": "DeepSeek 解析",
            "url": "https://zhuanlan.zhihu.com/p/123",
            "snippet": (
                "DeepSeek 是一个基于稀疏 MoE 模型 的开源大模型，其 架构 设计在"
                "多个维度上都有显著创新。本文从专家路由、负载均衡、训练稳定性、"
                "以及推理时延等多个角度系统拆解 DeepSeek-V3 的关键设计权衡。"
            ),
            "engines": ["mojeek"],
            "score": 0.05,
        },
    ]
    out = _lead_snippet("DeepSeek MoE 模型架构", results)
    assert out is not None
    assert "zhihu.com" in out
    assert "DeepSeek" in out
