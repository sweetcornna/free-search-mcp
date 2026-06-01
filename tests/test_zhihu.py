"""ZhihuEngine tests.

Zhihu is browser-rendered and aggressively gated, so these tests are OFFLINE:
they feed hand-built search-results HTML straight to ``parse()`` and assert URL
normalisation, plus that a login wall / empty page yields ``[]``. A login wall
returning no cards is the honest outcome, so [] is a correct result here, not an
error. An optional live-network test is gated on ``SEARCH_MCP_TEST_NETWORK=1``.
"""
from __future__ import annotations

import os

import pytest

from search_mcp.engines.zhihu import ZhihuEngine, _normalize_url

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.

NETWORK = os.environ.get("SEARCH_MCP_TEST_NETWORK") == "1"
skip_offline = pytest.mark.skipif(
    not NETWORK, reason="set SEARCH_MCP_TEST_NETWORK=1 to run"
)


# ---------------------------------------------------------------------------
# Wiring / build_url
# ---------------------------------------------------------------------------


def test_engine_needs_browser_and_wait_selector():
    e = ZhihuEngine()
    assert e.name == "zhihu"
    assert e.needs_browser is True
    assert e.wait_selector == ".SearchResult-Card"


def test_build_url_has_type_content_and_encoded_query():
    e = ZhihuEngine()
    url = e.build_url("python 教程", 10)
    assert url.startswith("https://www.zhihu.com/search?")
    assert "type=content" in url
    # quote_plus encodes spaces as '+' and non-ASCII as %xx.
    assert "q=python+%E6%95%99%E7%A8%8B" in url


# ---------------------------------------------------------------------------
# _normalize_url
# ---------------------------------------------------------------------------


def test_normalize_url_variants():
    assert _normalize_url("https://zhuanlan.zhihu.com/p/1") == "https://zhuanlan.zhihu.com/p/1"
    assert _normalize_url("//www.zhihu.com/question/123") == "https://www.zhihu.com/question/123"
    assert _normalize_url("/answer/456") == "https://www.zhihu.com/answer/456"
    assert _normalize_url("http://www.zhihu.com/x") == "https://www.zhihu.com/x"
    # Non-navigational / empty -> dropped.
    assert _normalize_url("") == ""
    assert _normalize_url("   ") == ""
    assert _normalize_url("javascript:void(0)") == ""
    assert _normalize_url("#login") == ""


# ---------------------------------------------------------------------------
# parse() — hand-built search-results HTML
# ---------------------------------------------------------------------------


# Three cards spanning all three href shapes: absolute https, protocol-relative
# '//www.zhihu.com/question/123', and root-relative '/answer/456'.
_FAKE_ZHIHU_HTML = """
<html><body>
<div class="Card SearchResult-Card">
  <h2 class="ContentItem-title"><a href="https://zhuanlan.zhihu.com/p/789">Absolute column post</a></h2>
  <div class="RichText">A great column article, published 2 days ago.</div>
</div>
<div class="Card SearchResult-Card">
  <h2><a href="//www.zhihu.com/question/123">Protocol-relative question</a></h2>
  <span class="RichText">Why is the sky blue? Posted 2026-04-28.</span>
</div>
<div class="Card SearchResult-Card">
  <div class="ContentItem-title"><a href="/answer/456">Root-relative answer</a></div>
  <div class="SearchItem-excerpt">Here is the accepted answer body.</div>
</div>
</body></html>
"""


def test_parse_extracts_and_normalizes_all_urls_to_https():
    e = ZhihuEngine()
    out = e.parse(_FAKE_ZHIHU_HTML)
    urls = [r.url for r in out]
    assert urls == [
        "https://zhuanlan.zhihu.com/p/789",
        "https://www.zhihu.com/question/123",
        "https://www.zhihu.com/answer/456",
    ]
    # Every URL is absolute https.
    assert all(u.startswith("https://") for u in urls)
    # Titles + snippets came through.
    assert out[0].title == "Absolute column post"
    assert out[0].snippet.startswith("A great column article")
    assert out[1].title == "Protocol-relative question"
    assert out[2].title == "Root-relative answer"
    assert out[2].snippet == "Here is the accepted answer body."
    # Date hints extracted from the two cards that carry them.
    assert out[0].published_age == "2 days ago"
    assert out[1].published_age == "2026-04-28"


def test_parse_dedupes_by_url():
    html = """
    <html><body>
    <div class="SearchResult-Card"><h2><a href="/answer/1">First</a></h2>
      <div class="RichText">one</div></div>
    <div class="SearchResult-Card"><h2><a href="/answer/1">Dup</a></h2>
      <div class="RichText">two</div></div>
    </body></html>
    """
    out = ZhihuEngine().parse(html)
    assert [r.url for r in out] == ["https://www.zhihu.com/answer/1"]


def test_parse_snippet_fallback_excludes_title():
    """When a card has no known excerpt node, the snippet falls back to the
    longest text block — but it must NOT pick the card's outer wrapper (whose
    text concatenates the title + every block), which would duplicate the title
    into the snippet. Regression for the fallback-pollution bug."""
    html = """
    <html><body>
    <div class="SearchResult-Card">
      <h2 class="ContentItem-title"><a href="/answer/9">Why is the sky blue</a></h2>
      <p>short</p>
      <p>This is the much longer real excerpt paragraph that should win.</p>
    </div>
    </body></html>
    """
    out = ZhihuEngine().parse(html)
    assert len(out) == 1
    snippet = out[0].snippet
    assert snippet == "This is the much longer real excerpt paragraph that should win."
    # The title must not have leaked into the snippet.
    assert "Why is the sky blue" not in snippet


def test_parse_empty_string_returns_empty():
    assert ZhihuEngine().parse("") == []


def test_parse_login_wall_returns_empty():
    # A gated page (login wall) has no result cards -> honest empty result.
    assert ZhihuEngine().parse("<html>login wall</html>") == []


def test_parse_skips_cards_without_usable_link():
    html = """
    <html><body>
    <div class="SearchResult-Card"><h2><a href="javascript:void(0)">JS</a></h2></div>
    <div class="SearchResult-Card"><h2><a href="#">anchor</a></h2></div>
    <div class="SearchResult-Card"><div>no link at all</div></div>
    </body></html>
    """
    assert ZhihuEngine().parse(html) == []


# ---------------------------------------------------------------------------
# Live network test
# ---------------------------------------------------------------------------


@skip_offline
async def test_live_zhihu_best_effort():
    from search_mcp.engines.zhihu import ZhihuEngine

    e = ZhihuEngine()
    out = await e.search("python", 5)
    # Zhihu may gate the request behind a login wall -> empty is acceptable.
    if not out:
        pytest.skip("Zhihu gated the headless request (login wall) — empty is honest")
    assert out[0].url.startswith("https://")
    assert all(r.engine == "zhihu" for r in out)
