"""Wikipedia / Open Library / GDELT / Sogou / 360 engines (offline).

Plus the rate-limit contract they motivated: GDELT publishes a much stricter
limit than the default pool, and search is a parallel fan-out, so a slow
bucket must *skip* rather than make every other engine wait for it.
"""

from __future__ import annotations

import os

import pytest

from search_mcp.engines import ENGINES, get_engine
from search_mcp.engines.base import SearchFilters
from search_mcp.engines.gdelt import GdeltEngine
from search_mcp.engines.openlibrary import OpenLibraryEngine
from search_mcp.engines.so360 import So360Engine
from search_mcp.engines.sogou import SogouEngine
from search_mcp.engines.wikipedia import WikipediaEngine

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.

NETWORK = os.environ.get("SEARCH_MCP_TEST_NETWORK") == "1"
skip_offline = pytest.mark.skipif(
    not NETWORK, reason="set SEARCH_MCP_TEST_NETWORK=1 to run"
)

NEW = ["wikipedia", "openlibrary", "gdelt", "sogou", "so360"]


@pytest.mark.parametrize("name", NEW)
def test_registered_and_out_of_the_default_pool(name):
    from search_mcp.config import Settings

    assert name in ENGINES
    assert get_engine(name).name == name
    assert name not in Settings().default_engines


# ---------------------------------------------------------------------------
# Wikipedia
# ---------------------------------------------------------------------------

_WIKI = {
    "query": {
        "search": [
            {
                "title": "Model Context Protocol",
                "pageid": 79706999,
                "snippet": 'The <span class="searchmatch">Model</span> Context '
                "Protocol (MCP) is an open standard &amp; framework",
            },
            {"title": "", "snippet": "no title"},
        ]
    }
}


def test_wikipedia_strips_search_highlight_markup():
    """Snippets ship with `<span class="searchmatch">` around matched terms;
    leaving it in puts raw HTML into the rendered result."""
    out = WikipediaEngine().map_results(_WIKI)
    assert "<span" not in out[0].snippet
    assert out[0].snippet.startswith("The Model Context Protocol (MCP)")


def test_wikipedia_unescapes_entities():
    out = WikipediaEngine().map_results(_WIKI)
    assert "&amp;" not in out[0].snippet
    assert "open standard & framework" in out[0].snippet


def test_wikipedia_builds_article_urls_from_titles():
    out = WikipediaEngine().map_results(_WIKI)
    assert out[0].url == "https://en.wikipedia.org/wiki/Model_Context_Protocol"


def test_wikipedia_skips_untitled_entries():
    assert len(WikipediaEngine().map_results(_WIKI)) == 1


def test_wikipedia_language_follows_region(monkeypatch):
    e = WikipediaEngine()
    monkeypatch.setattr("search_mcp.engines.wikipedia.settings.region", "cn-zh")
    assert e.build_url("x", 5).startswith("https://zh.wikipedia.org/")
    monkeypatch.setattr("search_mcp.engines.wikipedia.settings.region", "us-en")
    assert e.build_url("x", 5).startswith("https://en.wikipedia.org/")


@pytest.mark.parametrize("region", ["", "garbage", "us-", "us-e/../n", "us-toolongtag"])
def test_wikipedia_bad_region_falls_back_to_english(monkeypatch, region):
    """The language becomes part of the hostname, so anything that isn't a
    plain subtag must not be interpolated."""
    monkeypatch.setattr("search_mcp.engines.wikipedia.settings.region", region)
    assert WikipediaEngine().build_url("x", 5).startswith("https://en.wikipedia.org/")


# ---------------------------------------------------------------------------
# Open Library
# ---------------------------------------------------------------------------

_OL = {
    "docs": [
        {
            "title": "Children of Dune",
            "key": "/works/OL893516W",
            "author_name": ["Frank Herbert"],
            "first_publish_year": 1976,
            "edition_count": 77,
        },
        {"title": "No Key"},
        {"title": "Bad Key", "key": "not-a-path"},
    ]
}


def test_openlibrary_maps_works():
    out = OpenLibraryEngine().map_results(_OL)
    assert len(out) == 1
    assert out[0].url == "https://openlibrary.org/works/OL893516W"
    assert out[0].snippet == "Frank Herbert · first published 1976 · 77 editions"


def test_openlibrary_year_is_not_a_confident_date():
    """Only a publication *year* is known, so the freshness filter must not
    treat it as a precise date and drop on it."""
    out = OpenLibraryEngine().map_results(_OL)
    assert out[0].published_age == "1976"
    assert out[0].published_age_confident is False


def test_openlibrary_requests_only_the_fields_it_renders():
    """The default response ships every edition of every work and runs to
    megabytes."""
    assert "fields=" in OpenLibraryEngine().build_url("x", 5)


# ---------------------------------------------------------------------------
# GDELT
# ---------------------------------------------------------------------------

_GDELT = {
    "articles": [
        {
            "url": "https://example.com/story",
            "title": "Climate policy shifts",
            "seendate": "20260730T130338Z",
            "domain": "example.com",
            "language": "English",
            "sourcecountry": "United States",
        },
        {"title": "no url"},
    ]
}


def test_gdelt_maps_articles_and_parses_seendate():
    out = GdeltEngine().map_results(_GDELT)
    assert len(out) == 1
    assert out[0].published_age == "2026-07-30"
    assert out[0].published_age_confident is True
    assert "example.com" in out[0].snippet


@pytest.mark.parametrize("bad", [None, "", "short", "notadateZ", 12345])
def test_gdelt_bad_seendate_yields_no_date(bad):
    assert GdeltEngine()._seendate(bad) == ""


def test_gdelt_declares_a_stricter_limit_than_the_default():
    """GDELT answers 429 with "limit requests to one every 5 seconds"; the
    global 30/min default would walk straight into it."""
    from search_mcp.config import Settings

    e = GdeltEngine()
    assert e.rate_limit_per_minute is not None
    assert e.rate_limit_per_minute < Settings().rate_limit_per_minute


def test_gdelt_refuses_to_make_a_search_wait():
    """A bounded wait is what keeps one slow source off the critical path of a
    parallel fan-out."""
    assert GdeltEngine().rate_limit_max_wait is not None
    assert GdeltEngine().rate_limit_max_wait <= 2.0


def test_gdelt_freshness_uses_timespan():
    e = GdeltEngine()
    assert "timespan" not in e.build_url("x", 5)
    assert "timespan=1w" in e.build_url("x", 5, SearchFilters(freshness="week"))


# ---------------------------------------------------------------------------
# Rate-limit skip contract
# ---------------------------------------------------------------------------


async def test_engine_bucket_configured_from_its_declaration():
    from search_mcp.aggregator import search_limiter

    bucket = search_limiter._buckets["gdelt"]
    assert bucket.rate == pytest.approx(GdeltEngine().rate_limit_per_minute / 60.0)


async def test_token_bucket_gives_up_instead_of_queueing():
    from search_mcp.ratelimit import TokenBucket

    bucket = TokenBucket(6, burst=1)
    assert await bucket.acquire(1) is True          # takes the only token
    # Next token is ~10s away; a 0.1s budget must decline rather than sleep.
    assert await bucket.acquire(1, max_wait=0.1) is False


async def test_declined_token_is_not_consumed():
    """A refused acquire must leave the bucket untouched, or a burst of
    refusals would starve the next real caller."""
    from search_mcp.ratelimit import TokenBucket

    bucket = TokenBucket(60, burst=1)
    await bucket.acquire(1)
    before = bucket.tokens
    assert await bucket.acquire(1, max_wait=0.0) is False
    assert bucket.tokens >= before


async def test_rate_limited_engine_is_skipped_and_reported(monkeypatch):
    """The whole point: a starved bucket must not add its wait to the search."""
    from search_mcp import aggregator as agg

    class _Stub:
        name = "gdelt"
        rate_limit_max_wait = 0.1

        async def search(self, *a, **kw):
            raise AssertionError("must not run when the token was refused")

    monkeypatch.setattr(agg, "get_engine", lambda name: _Stub())

    async def _refuse(name, max_wait=None):
        return False

    monkeypatch.setattr(agg.search_limiter, "acquire", _refuse)

    out = await agg.aggregate_search(
        "anything", engines=["gdelt"], max_results=3, use_cache=False
    )
    assert out["results"] == []


# ---------------------------------------------------------------------------
# Chinese scrapers
# ---------------------------------------------------------------------------

_SO360_HTML = """
<html><body>
<li class="res-list">
  <h3><a href="https://www.runoob.com/python3/python-asyncio.html">Python asyncio 模块 | 菜鸟教程</a></h3>
  <p class="res-desc">asyncio 提供了一种高效的方式来处理并发任务。</p>
</li>
<li class="res-list">
  <h3><a href="https://blog.csdn.net/x/article/details/1">python asyncio 协程-CSDN博客</a></h3>
</li>
<li class="res-list">
  <h3><a href="javascript:void(0);">not a result</a></h3>
</li>
<li class="res-list">
  <h3><a href="https://www.runoob.com/python3/python-asyncio.html">duplicate url</a></h3>
</li>
</body></html>
"""


def test_so360_extracts_direct_urls():
    out = So360Engine().parse(_SO360_HTML)
    assert [r.url for r in out] == [
        "https://www.runoob.com/python3/python-asyncio.html",
        "https://blog.csdn.net/x/article/details/1",
    ]
    assert out[0].snippet.startswith("asyncio 提供了")


def test_so360_skips_non_http_and_duplicates():
    out = So360Engine().parse(_SO360_HTML)
    assert all(r.url.startswith("http") for r in out)
    assert len({r.url for r in out}) == len(out)


def test_so360_empty_and_garbage_return_empty():
    assert So360Engine().parse("") == []
    assert So360Engine().parse("<html><body>nope</body></html>") == []


_SOGOU_HTML = """
<html><body>
<div class="vrwrap">
  <h3 class="vr-title"><a href="/link?url=ENCRYPTEDBLOB1">python asyncio 教程</a></h3>
  <div class="text-layout">asyncio 是 Python 的异步 IO 库。</div>
</div>
<div class="vrwrap">
  <h3 class="vr-title"><a href="javascript:void(0);">widget</a></h3>
</div>
<div class="vrwrap">
  <h3 class="vr-title"><a href="?query=rewrite">related search</a></h3>
</div>
</body></html>
"""


def test_sogou_absolutizes_redirect_links():
    out = SogouEngine().parse(_SOGOU_HTML)
    assert len(out) == 1
    assert out[0].url == "https://www.sogou.com/link?url=ENCRYPTEDBLOB1"


def test_sogou_skips_widgets_and_requery_links():
    """The page is padded with `javascript:void(0)` widgets and relative
    re-query links that are not results."""
    out = SogouEngine().parse(_SOGOU_HTML)
    assert all("/link?url=" in r.url for r in out)


def test_sogou_finds_snippet_from_an_ancestor():
    out = SogouEngine().parse(_SOGOU_HTML)
    assert out[0].snippet.startswith("asyncio 是 Python")


def test_sogou_empty_and_garbage_return_empty():
    assert SogouEngine().parse("") == []
    assert SogouEngine().parse("<html><body>nope</body></html>") == []


# ---------------------------------------------------------------------------
# Live network
# ---------------------------------------------------------------------------


@skip_offline
@pytest.mark.parametrize(
    "name,query",
    [
        ("wikipedia", "model context protocol"),
        ("openlibrary", "dune frank herbert"),
        ("so360", "python asyncio"),
        ("sogou", "python asyncio"),
    ],
)
async def test_live_returns_results(name, query):
    out = await get_engine(name).search(query, 3)
    if not out:
        pytest.skip(f"{name} returned nothing (blocked or markup changed)")
    assert out[0].url.startswith("http")
    assert all(r.engine == name for r in out)


@skip_offline
async def test_live_gdelt_is_either_useful_or_politely_empty():
    """GDELT rate-limits hard; an empty result is an acceptable outcome, an
    exception is not."""
    out = await get_engine("gdelt").search("climate policy", 3)
    for r in out:
        assert r.url.startswith("http")
