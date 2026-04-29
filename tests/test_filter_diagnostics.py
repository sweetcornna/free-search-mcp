"""Tests for the filter-diagnostics path: when a filtered search returns
sparse results, the aggregator emits a ``filter_diagnostics`` block that
explains *why* and the formatter renders it as a clearly-marked Markdown
section.

All offline; engines are mocked at the ``_fetch`` boundary.
"""
from __future__ import annotations

from unittest.mock import patch

from search_mcp.aggregator import aggregate_search
from search_mcp.engines import (
    SearchFilters,
    SearchResult,
    apply_post_filters_with_diagnostics,
    get_engine,
)
from search_mcp.formatting import render_search


# ---------------------------------------------------------------------------
# apply_post_filters_with_diagnostics — pure helper
# ---------------------------------------------------------------------------


def _r(url: str, title: str = "t", snippet: str = "s") -> SearchResult:
    return SearchResult(title=title, url=url, snippet=snippet, engine="x", rank=1)


def test_diagnostics_drops_counted_per_reason():
    """Every dropped result is attributed to exactly one reason."""
    results = [
        _r("https://github.com/a"),       # kept (passes include_domains)
        _r("https://example.com/b"),      # dropped: include_domains
        _r("https://spam.com/c"),         # dropped: include_domains (loses to inc check first)
    ]
    kept, drops = apply_post_filters_with_diagnostics(
        results, SearchFilters(include_domains=["github.com"])
    )
    assert [r.url for r in kept] == ["https://github.com/a"]
    assert drops == {"include_domains": 2}
    # Conservation: kept + dropped == input
    assert len(kept) + sum(drops.values()) == len(results)


def test_diagnostics_drops_attribute_to_first_failing_filter():
    """When multiple filters would reject a result, only the FIRST is counted.
    This makes ``sum(drops.values()) == dropped_count`` exact."""
    results = [
        # github.com passes include but fails category=paper — counted as
        # category_paper, not include_domains.
        _r("https://github.com/a"),
        # Fails include_domains first — counted there only.
        _r("https://example.com/b"),
    ]
    kept, drops = apply_post_filters_with_diagnostics(
        results,
        SearchFilters(include_domains=["github.com"], category="paper"),
    )
    assert kept == []
    assert drops == {"category_paper": 1, "include_domains": 1}


def test_diagnostics_no_filters_returns_empty_drops():
    results = [_r("https://a.com"), _r("https://b.com")]
    kept, drops = apply_post_filters_with_diagnostics(results, None)
    assert kept == results
    assert drops == {}
    kept2, drops2 = apply_post_filters_with_diagnostics(results, SearchFilters())
    assert kept2 == results
    assert drops2 == {}


def test_diagnostics_exclude_text_counted():
    results = [
        _r("https://a.com", title="Beginner Python"),
        _r("https://b.com", title="Advanced Python"),
    ]
    kept, drops = apply_post_filters_with_diagnostics(
        results, SearchFilters(exclude_text="beginner")
    )
    assert [r.url for r in kept] == ["https://b.com"]
    assert drops == {"exclude_text": 1}


# ---------------------------------------------------------------------------
# aggregate_search — diagnostics presence/absence
# ---------------------------------------------------------------------------


# Three results: only one is on github.com — so include_domains=github.com
# kills two, leaving one. That triggers the "sparse" threshold (<=3).
_DDG_FAKE_HTML_THIN = """
<html><body>
<div class="result">
  <a class="result__a" href="https://github.com/anthropics/anthropic-sdk-python">Anthropic SDK</a>
  <div class="result__snippet">Python SDK for the Anthropic API</div>
</div>
<div class="result">
  <a class="result__a" href="https://example.com/foo">Example page</a>
  <div class="result__snippet">Some unrelated thing</div>
</div>
<div class="result">
  <a class="result__a" href="https://other.com/bar">Other</a>
  <div class="result__snippet">Yet another non-github page</div>
</div>
</body></html>
"""


# Plenty of github results — the post-filter passes them all so the result
# count exceeds the sparse threshold and diagnostics MUST be omitted.
_DDG_FAKE_HTML_PLENTIFUL = """
<html><body>
<div class="result">
  <a class="result__a" href="https://github.com/a/1">a/1</a>
  <div class="result__snippet">snippet 1</div>
</div>
<div class="result">
  <a class="result__a" href="https://github.com/b/2">b/2</a>
  <div class="result__snippet">snippet 2</div>
</div>
<div class="result">
  <a class="result__a" href="https://github.com/c/3">c/3</a>
  <div class="result__snippet">snippet 3</div>
</div>
<div class="result">
  <a class="result__a" href="https://github.com/d/4">d/4</a>
  <div class="result__snippet">snippet 4</div>
</div>
<div class="result">
  <a class="result__a" href="https://github.com/e/5">e/5</a>
  <div class="result__snippet">snippet 5</div>
</div>
</body></html>
"""


async def test_aggregate_emits_diagnostics_when_results_sparse():
    e = get_engine("duckduckgo")
    with patch.object(e, "_fetch", return_value=_DDG_FAKE_HTML_THIN):
        out = await aggregate_search(
            "x",
            engines=["duckduckgo"],
            max_results=10,
            use_cache=False,
            include_domains=["github.com"],
        )
    assert len(out["results"]) <= 3
    diag = out.get("filter_diagnostics")
    assert diag is not None
    assert diag["raw_per_engine"] == {"duckduckgo": 3}
    assert diag["after_filter_per_engine"] == {"duckduckgo": 1}
    assert diag["drops_by_reason"] == {"include_domains": 2}
    assert isinstance(diag["hint"], str) and diag["hint"]


async def test_aggregate_omits_diagnostics_when_results_plentiful():
    """Diagnostics MUST be absent on the happy path even when filters were set."""
    e = get_engine("duckduckgo")
    with patch.object(e, "_fetch", return_value=_DDG_FAKE_HTML_PLENTIFUL):
        out = await aggregate_search(
            "x",
            engines=["duckduckgo"],
            max_results=10,
            use_cache=False,
            include_domains=["github.com"],
        )
    assert len(out["results"]) > 3
    assert "filter_diagnostics" not in out


async def test_aggregate_omits_diagnostics_when_no_filters_set():
    """Even with sparse results, if no filter was set there's nothing to
    explain — the field must be absent."""
    e = get_engine("duckduckgo")
    with patch.object(e, "_fetch", return_value=_DDG_FAKE_HTML_THIN):
        out = await aggregate_search(
            "x", engines=["duckduckgo"], max_results=10, use_cache=False,
        )
    # Three raw results, no filter → result count <=3 but diagnostics omitted.
    assert "filter_diagnostics" not in out


async def test_aggregate_diagnostic_hint_names_top_dropping_filter():
    """The hint sentence must mention the highest-drop filter name."""
    e = get_engine("duckduckgo")
    with patch.object(e, "_fetch", return_value=_DDG_FAKE_HTML_THIN):
        out = await aggregate_search(
            "x",
            engines=["duckduckgo"],
            max_results=10,
            use_cache=False,
            include_domains=["github.com"],
        )
    diag = out["filter_diagnostics"]
    # Top reason was include_domains (2 drops). Hint should call it out.
    assert "include_domains" in diag["hint"]


# ---------------------------------------------------------------------------
# render_search integration
# ---------------------------------------------------------------------------


def test_render_search_includes_filter_diagnostics_block_when_present():
    payload = {
        "query": "rust async",
        "engines": ["duckduckgo", "mojeek"],
        "results": [
            {
                "title": "Some forum post",
                "url": "https://news.ycombinator.com/item?id=1",
                "snippet": "discussion",
                "engines": ["duckduckgo"],
                "score": 0.01,
            }
        ],
        "lead_snippet": None,
        "errors": None,
        "filter_diagnostics": {
            "raw_per_engine": {"duckduckgo": 10, "mojeek": 8},
            "after_filter_per_engine": {"duckduckgo": 1, "mojeek": 0},
            "drops_by_reason": {"category_forum": 12, "exclude_text": 5},
            "hint": (
                "Filters dropped 17 of 18 raw results (kept 1). "
                "Most were excluded by category=forum. "
                "Try widening or removing one filter."
            ),
        },
    }
    md = render_search(payload)
    assert "Filter diagnostics" in md
    assert "category=forum" in md  # comes from the hint text
    assert "Top drops:" in md
    # Diagnostics block sits AFTER the result heading.
    assert md.index("## 1.") < md.index("Filter diagnostics")


def test_render_search_omits_filter_diagnostics_when_absent():
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
    assert "Filter diagnostics" not in md
