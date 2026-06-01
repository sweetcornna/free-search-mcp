"""GoogleEngine + SerpSearchEngine tests (offline).

The engine scrapes the public Google web SERP over plain HTTP and relies on
the base-class Playwright browser fallback when Google serves a JS/consent
shell. These unit tests never touch the network: ``parse()`` is exercised
against a hand-built results-page fixture, and a small live suite is gated on
``SEARCH_MCP_TEST_NETWORK=1``.
"""

from __future__ import annotations

import os

import pytest

from search_mcp.engines.base import SearchFilters
from search_mcp.engines.google import GoogleEngine
from search_mcp.engines.serpsearch import SerpSearchEngine

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.

NETWORK = os.environ.get("SEARCH_MCP_TEST_NETWORK") == "1"
skip_offline = pytest.mark.skipif(
    not NETWORK, reason="set SEARCH_MCP_TEST_NETWORK=1 to run"
)


# ---------------------------------------------------------------------------
# build_url
# ---------------------------------------------------------------------------


def test_build_url_has_core_params():
    e = GoogleEngine()
    url = e.build_url("hello world", 10)
    assert "q=hello+world" in url
    assert "num=" in url
    assert "hl=en" in url
    assert "gl=us" in url


def test_build_url_clamps_num_to_10_20_band():
    e = GoogleEngine()
    assert "num=10" in e.build_url("x", 1)
    assert "num=20" in e.build_url("x", 50)
    assert "num=15" in e.build_url("x", 15)


def test_build_url_freshness_week_uses_tbs_qdr_w():
    e = GoogleEngine()
    url = e.build_url("hello", 10, SearchFilters(freshness="week"))
    assert "tbs=qdr:w" in url


def test_build_url_encodes_site_operators():
    e = GoogleEngine()
    f = SearchFilters(
        include_domains=["example.com"],
        exclude_domains=["spam.com"],
        category="pdf",
    )
    url = e.build_url("hello", 10, f)
    # site:/-site:/filetype: are URL-encoded into the q param
    assert "site%3Aexample.com" in url
    assert "-site%3Aspam.com" in url
    assert "filetype%3Apdf" in url


def test_build_url_safesearch_moderate_adds_safe_active(monkeypatch):
    e = GoogleEngine()
    monkeypatch.setattr("search_mcp.engines.google.settings.safesearch", "moderate")
    assert "safe=active" in e.build_url("hello", 10)


def test_build_url_safesearch_off_omits_safe(monkeypatch):
    e = GoogleEngine()
    monkeypatch.setattr("search_mcp.engines.google.settings.safesearch", "off")
    assert "safe=" not in e.build_url("hello", 10)


# ---------------------------------------------------------------------------
# parse() — a small hand-built Google results page
# ---------------------------------------------------------------------------

# Contains:
#   * one ordinary organic result (direct https href)
#   * one organic result whose link is a /url?q= redirect wrapper
#   * one ad row (div[data-text-ad]) that must be skipped
#   * one internal Google nav link (/search?...) that must be skipped
#   * a duplicate of the first result that must be deduped away
_FAKE_GOOGLE_HTML = """
<html><body>
<div id="tads">
  <div class="g" data-hveid="ad1">
    <div data-text-ad="1">
      <a href="https://ads.example.com/sponsored"><h3>Sponsored Ad Title</h3></a>
      <div class="VwiC3b">Buy now, sponsored content.</div>
    </div>
  </div>
</div>

<div class="g" data-hveid="r1">
  <a href="https://example.com/a"><h3>Example A title</h3></a>
  <div class="VwiC3b">First snippet body — published 2 days ago.</div>
</div>

<div class="g" data-hveid="r2">
  <a href="/url?q=https%3A%2F%2Fexample.com%2Fb&amp;sa=U&amp;ved=xyz"><h3>Example B title</h3></a>
  <div class="VwiC3b">Second snippet body about things.</div>
</div>

<div class="g" data-hveid="rnav">
  <a href="/search?q=related+stuff"><h3>Related searches</h3></a>
  <div class="VwiC3b">Internal Google navigation.</div>
</div>

<div class="g" data-hveid="r1dup">
  <a href="https://example.com/a"><h3>Example A title (dup)</h3></a>
  <div class="VwiC3b">Duplicate of the first result.</div>
</div>
</body></html>
"""


def test_parse_extracts_organic_and_skips_ads_and_internal():
    e = GoogleEngine()
    out = e.parse(_FAKE_GOOGLE_HTML)
    urls = [r.url for r in out]
    # Ad + internal link skipped; /url?q= unwrapped; duplicate deduped.
    assert urls == ["https://example.com/a", "https://example.com/b"]
    assert out[0].title == "Example A title"
    assert out[0].snippet.startswith("First snippet body")
    assert out[1].title == "Example B title"
    assert out[1].snippet.startswith("Second snippet body")
    # No sponsored result leaked through.
    assert "https://ads.example.com/sponsored" not in urls


def test_parse_unwraps_url_redirect():
    e = GoogleEngine()
    out = e.parse(_FAKE_GOOGLE_HTML)
    b = [r for r in out if r.title == "Example B title"]
    assert b and b[0].url == "https://example.com/b"


def test_parse_extracts_date_hint():
    e = GoogleEngine()
    out = e.parse(_FAKE_GOOGLE_HTML)
    assert out[0].published_age == "2 days ago"


def test_parse_dedups_repeated_urls():
    e = GoogleEngine()
    out = e.parse(_FAKE_GOOGLE_HTML)
    assert [r.url for r in out].count("https://example.com/a") == 1


def test_parse_empty_string_returns_empty_list():
    assert GoogleEngine().parse("") == []


def test_parse_garbage_returns_empty_list():
    assert GoogleEngine().parse("<html><body>not a serp</body></html>") == []
    assert GoogleEngine().parse("<<<broken>>>") == []


# ---------------------------------------------------------------------------
# SerpSearchEngine alias
# ---------------------------------------------------------------------------


def test_serpsearch_is_named_serpsearch():
    assert SerpSearchEngine().name == "serpsearch"


def test_serpsearch_is_a_google_engine():
    assert isinstance(SerpSearchEngine(), GoogleEngine)


def test_serpsearch_parses_like_google():
    # The alias inherits parse() verbatim — same titles/urls/snippets. Only the
    # engine label differs (parse() stamps self.name on each result).
    serp = SerpSearchEngine().parse(_FAKE_GOOGLE_HTML)
    goog = GoogleEngine().parse(_FAKE_GOOGLE_HTML)
    assert [(r.title, r.url, r.snippet) for r in serp] == [
        (r.title, r.url, r.snippet) for r in goog
    ]
    assert all(r.engine == "serpsearch" for r in serp)


# ---------------------------------------------------------------------------
# Live network test
# ---------------------------------------------------------------------------


@skip_offline
async def test_live_google_returns_results():
    e = GoogleEngine()
    out = await e.search("python programming language", 5)
    if not out:
        pytest.skip("Google served an interstitial / no parseable results")
    assert out[0].url.startswith("http")
    assert all(r.engine == "google" for r in out)
