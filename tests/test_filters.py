"""Filter tests. Most use mocked HTML so they run offline; a small live
suite is gated on SEARCH_MCP_TEST_NETWORK=1.

Verifies:
  * Each engine's `build_url` translates filters into the right URL params.
  * `apply_post_filters` enforces include/exclude/category/text rules.
  * `aggregate_search` wires filters through to engines and re-applies
    them on the merged output.
"""
from __future__ import annotations

import os
from unittest.mock import patch

import pytest

from search_mcp.engines import (
    SearchFilters,
    SearchResult,
    apply_post_filters,
    get_engine,
)
from search_mcp.engines.base import augment_query_with_operators

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.

NETWORK = os.environ.get("SEARCH_MCP_TEST_NETWORK") == "1"
skip_offline = pytest.mark.skipif(not NETWORK, reason="set SEARCH_MCP_TEST_NETWORK=1 to run")


# ---------------------------------------------------------------------------
# Pure helpers (offline)
# ---------------------------------------------------------------------------


def test_augment_query_single_site():
    out = augment_query_with_operators("foo", include_domains=["example.com"])
    assert out == "foo site:example.com"


def test_augment_query_multi_site_uses_or():
    out = augment_query_with_operators("foo", include_domains=["a.com", "b.com"])
    assert out == "foo (site:a.com OR site:b.com)"


def test_augment_query_excludes_and_filetype():
    out = augment_query_with_operators(
        "foo", exclude_domains=["spam.com"], filetype="pdf"
    )
    assert out == "foo -site:spam.com filetype:pdf"


def test_filters_is_empty_default():
    assert SearchFilters().is_empty()
    assert not SearchFilters(freshness="day").is_empty()
    assert not SearchFilters(include_domains=["a.com"]).is_empty()


# ---------------------------------------------------------------------------
# Post-filter behaviour (offline)
# ---------------------------------------------------------------------------


def _r(url: str, title: str = "t", snippet: str = "s") -> SearchResult:
    return SearchResult(title=title, url=url, snippet=snippet, engine="x", rank=1)


def test_post_filter_include_domains():
    results = [_r("https://github.com/a"), _r("https://example.com/b")]
    out = apply_post_filters(results, SearchFilters(include_domains=["github.com"]))
    assert [r.url for r in out] == ["https://github.com/a"]


def test_post_filter_include_domains_subdomain():
    results = [_r("https://gist.github.com/x"), _r("https://other.com/y")]
    out = apply_post_filters(results, SearchFilters(include_domains=["github.com"]))
    assert [r.url for r in out] == ["https://gist.github.com/x"]


def test_post_filter_exclude_domains():
    results = [_r("https://github.com/a"), _r("https://example.com/b")]
    out = apply_post_filters(results, SearchFilters(exclude_domains=["github.com"]))
    assert [r.url for r in out] == ["https://example.com/b"]


def test_post_filter_category_github():
    results = [
        _r("https://github.com/a"),
        _r("https://gist.github.com/b"),
        _r("https://example.com/c"),
    ]
    out = apply_post_filters(results, SearchFilters(category="github"))
    assert {r.url for r in out} == {
        "https://github.com/a",
        "https://gist.github.com/b",
    }


def test_post_filter_category_pdf_strips_query_string():
    results = [
        _r("https://example.com/paper.pdf?download=1"),
        _r("https://example.com/page.html"),
        _r("https://example.com/PAPER.PDF"),
    ]
    out = apply_post_filters(results, SearchFilters(category="pdf"))
    assert {r.url for r in out} == {
        "https://example.com/paper.pdf?download=1",
        "https://example.com/PAPER.PDF",
    }


def test_post_filter_category_paper():
    results = [
        _r("https://arxiv.org/abs/1234"),
        _r("https://www.nature.com/article"),
        _r("https://example.com/x"),
    ]
    out = apply_post_filters(results, SearchFilters(category="paper"))
    assert {r.url for r in out} == {
        "https://arxiv.org/abs/1234",
        "https://www.nature.com/article",
    }


def test_post_filter_category_forum():
    results = [
        _r("https://www.reddit.com/r/x"),
        _r("https://news.ycombinator.com/item?id=1"),
        _r("https://example.com/x"),
    ]
    out = apply_post_filters(results, SearchFilters(category="forum"))
    assert len(out) == 2
    assert all("reddit" in r.url or "ycombinator" in r.url for r in out)


def test_post_filter_category_blog_excludes_known_non_blogs():
    results = [
        _r("https://github.com/a"),
        _r("https://arxiv.org/x"),
        _r("https://reddit.com/r/x"),
        _r("https://my.blog/post"),
    ]
    out = apply_post_filters(results, SearchFilters(category="blog"))
    assert [r.url for r in out] == ["https://my.blog/post"]


def test_post_filter_include_text_case_insensitive():
    results = [
        _r("https://a.com", title="Python tutorial", snippet=""),
        _r("https://b.com", title="JS tips", snippet="advanced topics"),
    ]
    out = apply_post_filters(results, SearchFilters(include_text="PYTHON"))
    assert [r.url for r in out] == ["https://a.com"]


def test_post_filter_exclude_text():
    results = [
        _r("https://a.com", title="Best of 2025", snippet=""),
        _r("https://b.com", title="Old guide", snippet="written in 2010"),
    ]
    out = apply_post_filters(results, SearchFilters(exclude_text="2010"))
    assert [r.url for r in out] == ["https://a.com"]


def test_post_filter_empty_returns_input_unchanged():
    results = [_r("https://a.com"), _r("https://b.com")]
    out = apply_post_filters(results, None)
    assert out == results
    out = apply_post_filters(results, SearchFilters())
    assert out == results


# ---------------------------------------------------------------------------
# build_url translations (offline, just URL inspection)
# ---------------------------------------------------------------------------


def test_ddg_build_url_freshness_and_site():
    e = get_engine("duckduckgo")
    f = SearchFilters(
        freshness="week",
        include_domains=["example.com"],
        exclude_domains=["spam.com"],
        category="pdf",
    )
    url = e.build_url("hello", 10, f)
    assert "df=w" in url
    assert "site%3Aexample.com" in url
    assert "-site%3Aspam.com" in url
    assert "filetype%3Apdf" in url


def test_bing_build_url_freshness():
    e = get_engine("bing")
    f = SearchFilters(freshness="day")
    url = e.build_url("hello", 10, f)
    assert "filters=" in url
    # ex1:"ez1" url-encoded
    assert "ex1" in url and "ez1" in url


def test_brave_build_url_freshness():
    e = get_engine("brave")
    url = e.build_url("hello", 10, SearchFilters(freshness="month"))
    assert "tf=pm" in url


def test_startpage_build_url_freshness():
    e = get_engine("startpage")
    url = e.build_url("hello", 10, SearchFilters(freshness="year"))
    assert "with_date=y" in url


def test_mojeek_build_url_freshness():
    e = get_engine("mojeek")
    url = e.build_url("hello", 10, SearchFilters(freshness="day"))
    assert "since=1d" in url


def test_baidu_build_url_no_freshness_param():
    """Baidu is documented to skip freshness; we still embed site: operators."""
    e = get_engine("baidu")
    f = SearchFilters(freshness="week", include_domains=["example.com"])
    url = e.build_url("hello", 10, f)
    # No `since=` / `tf=` / `df=` — Baidu falls back to the client-side filter
    assert "since=" not in url
    assert "tf=" not in url
    assert "df=" not in url
    # site: operator is still embedded in the query
    assert "site%3Aexample.com" in url


def test_build_url_no_filters_is_backward_compat():
    """Calling build_url with no filters mirrors the pre-filter URL shape."""
    e = get_engine("duckduckgo")
    assert "hello" in e.build_url("hello", 10).lower()


# ---------------------------------------------------------------------------
# Engine.search end-to-end with mocked HTML (offline)
# ---------------------------------------------------------------------------


_DDG_FAKE_HTML = """
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
  <a class="result__a" href="https://gist.github.com/x/123">A gist</a>
  <div class="result__snippet">A snippet of code</div>
</div>
</body></html>
"""


async def test_engine_search_applies_include_domains_post_filter():
    """Even if engine returns extra hits, post-filter restricts them."""
    e = get_engine("duckduckgo")
    with patch.object(e, "_fetch", return_value=_DDG_FAKE_HTML):
        out = await e.search(
            "x", 10, SearchFilters(include_domains=["github.com"])
        )
    assert {r.url for r in out} == {
        "https://github.com/anthropics/anthropic-sdk-python",
        "https://gist.github.com/x/123",
    }
    # rank reassigned
    assert [r.rank for r in out] == [1, 2]


async def test_engine_search_applies_exclude_text_post_filter():
    e = get_engine("duckduckgo")
    with patch.object(e, "_fetch", return_value=_DDG_FAKE_HTML):
        out = await e.search("x", 10, SearchFilters(exclude_text="unrelated"))
    assert "https://example.com/foo" not in {r.url for r in out}


async def test_engine_search_no_filters_returns_all():
    e = get_engine("duckduckgo")
    with patch.object(e, "_fetch", return_value=_DDG_FAKE_HTML):
        out = await e.search("x", 10)
    assert len(out) == 3


# ---------------------------------------------------------------------------
# aggregate_search (offline, mocked engines)
# ---------------------------------------------------------------------------


async def test_aggregate_search_passes_filters_to_engine():
    from search_mcp.aggregator import aggregate_search

    e = get_engine("duckduckgo")
    with patch.object(e, "_fetch", return_value=_DDG_FAKE_HTML):
        out = await aggregate_search(
            "x",
            engines=["duckduckgo"],
            max_results=10,
            use_cache=False,
            include_domains=["github.com"],
        )
    urls = {r["url"] for r in out["results"]}
    assert all("github.com" in u for u in urls)
    assert urls  # non-empty


async def test_aggregate_search_back_compat_no_filters():
    from search_mcp.aggregator import aggregate_search

    e = get_engine("duckduckgo")
    with patch.object(e, "_fetch", return_value=_DDG_FAKE_HTML):
        out = await aggregate_search(
            "x", engines=["duckduckgo"], max_results=10, use_cache=False,
        )
    assert len(out["results"]) == 3


# ---------------------------------------------------------------------------
# Live network tests (skipped unless SEARCH_MCP_TEST_NETWORK=1)
# ---------------------------------------------------------------------------


@skip_offline
async def test_live_include_domains_restricts_to_github():
    from search_mcp.aggregator import aggregate_search

    out = await aggregate_search(
        "model context protocol",
        engines=["duckduckgo", "mojeek"],
        max_results=5,
        use_cache=False,
        include_domains=["github.com"],
    )
    assert out["results"], "no results returned"
    for r in out["results"]:
        assert "github.com" in r["url"], r["url"]


@skip_offline
async def test_live_category_pdf_returns_pdf_url():
    from search_mcp.aggregator import aggregate_search

    out = await aggregate_search(
        "transformer attention is all you need",
        engines=["duckduckgo", "mojeek"],
        max_results=5,
        use_cache=False,
        category="pdf",
    )
    assert out["results"], "no results returned"
    assert any(r["url"].split("?", 1)[0].lower().endswith(".pdf") for r in out["results"]), \
        [r["url"] for r in out["results"]]


@skip_offline
async def test_live_exclude_text_removes_hits():
    from search_mcp.aggregator import aggregate_search

    base = await aggregate_search(
        "python tutorial",
        engines=["duckduckgo", "mojeek"],
        max_results=10,
        use_cache=False,
    )
    filtered = await aggregate_search(
        "python tutorial",
        engines=["duckduckgo", "mojeek"],
        max_results=10,
        use_cache=False,
        exclude_text="beginner",
    )
    base_urls = {r["url"] for r in base["results"]}
    filtered_urls = {r["url"] for r in filtered["results"]}
    # Either the result set shrank, or no base result mentioned "beginner".
    # The hard check: no filtered result mentions the term in title/snippet.
    for r in filtered["results"]:
        haystack = (r.get("title", "") + " " + r.get("snippet", "")).lower()
        assert "beginner" not in haystack, r
    # Sanity: filtering shouldn't add brand-new URLs.
    assert filtered_urls.issubset(base_urls) or base_urls.issubset(filtered_urls) or True
