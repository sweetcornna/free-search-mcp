"""Regression tests for two bug classes found together:

1. **Category filtering discarded native sources.** ``category="news"`` /
   ``"paper"`` / ``"forum"`` re-checked every result against a hand-maintained
   hostname allowlist, including results from engines that natively index that
   category. Measured before the fix: Crossref returned 8 papers on
   ``category="paper"`` and all 8 were dropped because Crossref emits doi.org
   links; GDELT's non-Anglophone news was dropped the same way.

2. **Region-derived locale was hardcoded** in the two Google engines while
   every other region-aware engine read ``settings.region``. Google News is
   edition-scoped, so this returned an EMPTY feed for any query outside the
   US/English edition — which then read as "possible IP block" downstream.

All offline: pure helpers plus ``build_url`` string checks.
"""
from __future__ import annotations

from unittest.mock import patch

import pytest

from search_mcp.engines import SearchFilters, SearchResult, apply_post_filters
from search_mcp.engines.base import (
    _is_news_host,
    region_to_google_news_ceid,
    region_to_google_params,
)


def _r(url: str) -> SearchResult:
    return SearchResult(title="t", url=url, snippet="s", engine="x", rank=1)


# ---------------------------------------------------------------------------
# 1. native_category bypasses the hostname allowlist
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    ("category", "url"),
    [
        # Crossref/OpenAlex emit DOI resolver links; PubMed emits NCBI links.
        ("paper", "https://doi.org/10.1000/xyz123"),
        ("paper", "https://pubmed.ncbi.nlm.nih.gov/12345678/"),
        # GDELT indexes news in 100+ languages.
        ("news", "https://news.ltn.com.tw/news/world/breakingnews/1"),
        ("news", "https://www.thepaper.cn/newsDetail_forward_1"),
        ("forum", "https://www.zhihu.com/question/123"),
    ],
)
def test_native_category_keeps_results_a_hostname_allowlist_would_drop(category, url):
    """A source that natively indexes the category is trusted for it."""
    results = [_r(url)]
    f = SearchFilters(category=category)
    assert apply_post_filters(results, f, native_category=True) == results


def test_non_native_category_still_filters_by_host():
    """The allowlist must keep working for general web engines — that is the
    only reason it exists."""
    results = [_r("https://github.com/a/b")]
    kept = apply_post_filters(results, SearchFilters(category="news"))
    assert kept == []


def test_native_category_still_honors_domain_and_freshness_filters():
    """Trusting the source for CATEGORY must not disable the other filters."""
    results = [_r("https://doi.org/10.1/a"), _r("https://arxiv.org/abs/1")]
    kept = apply_post_filters(
        results,
        SearchFilters(category="paper", include_domains=["arxiv.org"]),
        native_category=True,
    )
    assert [r.url for r in kept] == ["https://arxiv.org/abs/1"]

    stale = _r("https://doi.org/10.1/old")
    stale.published_age = "3 years ago"
    kept = apply_post_filters(
        [stale], SearchFilters(category="paper", freshness="day"), native_category=True
    )
    assert kept == []


def test_native_category_does_not_bypass_pdf_check():
    """category='pdf' is checked against the URL itself, not a host guess, so
    it stays authoritative even for a native source."""
    kept = apply_post_filters(
        [_r("https://arxiv.org/abs/2401.00001")],
        SearchFilters(category="pdf"),
        native_category=True,
    )
    assert kept == []


def test_engine_declaring_category_is_trusted_end_to_end():
    """finalize_results derives native_category from the engine's own
    `categories` declaration."""
    from search_mcp.engines import get_engine

    gdelt = get_engine("gdelt")
    assert "news" in gdelt.categories
    taiwanese = [_r("https://news.ltn.com.tw/news/world/1")]
    kept = gdelt.finalize_results(taiwanese, SearchFilters(category="news"), 10)
    assert len(kept) == 1

    # A general web engine gets no such pass.
    ddg = get_engine("duckduckgo")
    assert "news" not in ddg.categories
    assert ddg.finalize_results(
        [_r("https://github.com/a")], SearchFilters(category="news"), 10
    ) == []


# ---------------------------------------------------------------------------
# 1b. the news allowlist itself is no longer Anglosphere-only
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "host",
    [
        "www.thepaper.cn", "news.sina.com.cn", "36kr.com", "www.scmp.com",
        "www.nikkei.com", "www.lemonde.fr", "www.spiegel.de",
        "www.reuters.com",  # the originals must keep working
    ],
)
def test_news_allowlist_covers_non_anglophone_outlets(host):
    assert _is_news_host(host)


def test_news_host_convention_matches_unlisted_outlets():
    """No tuple can list every outlet on earth; the `news.<domain>` naming
    convention covers a slice of the long tail."""
    assert _is_news_host("news.daheiai.com")
    assert _is_news_host("newsroom.example.co.jp")


@pytest.mark.parametrize(
    "host", ["postgresql.org", "newsletter.example.com", "github.com", "arxiv.org"]
)
def test_news_convention_does_not_overmatch(host):
    """The convention test is deliberately narrow — a bare 'news'/'post'
    substring rule would match these."""
    assert not _is_news_host(host)


# ---------------------------------------------------------------------------
# 2. locale derives from settings.region
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    ("region", "expected"),
    [
        ("us-en", ("en-US", "US")),
        ("cn-zh", ("zh-CN", "CN")),
        ("de-de", ("de-DE", "DE")),
        ("uk-en", ("en-GB", "GB")),  # Google says GB, not UK
        ("", ("en-US", "US")),       # malformed -> safe default
        ("garbage", ("en-US", "US")),
    ],
)
def test_region_to_google_params(region, expected):
    assert region_to_google_params(region) == expected


@pytest.mark.parametrize(
    ("region", "ceid"),
    [
        ("us-en", "US:en"),
        ("cn-zh", "CN:zh-Hans"),   # Simplified and Traditional are separate feeds
        ("tw-zh", "TW:zh-Hant"),
        ("jp-ja", "JP:ja"),
    ],
)
def test_region_to_google_news_ceid(region, ceid):
    assert region_to_google_news_ceid(region) == ceid


def test_googlenews_url_follows_region_setting():
    from search_mcp.engines.googlenews import GoogleNewsEngine

    engine = GoogleNewsEngine()
    with patch("search_mcp.engines.googlenews.settings.region", "cn-zh"):
        url = engine.build_url("AI 新闻", 5, None)
    assert "hl=zh-CN" in url
    assert "gl=CN" in url
    assert "ceid=CN:zh-Hans" in url


@pytest.mark.parametrize(
    ("query", "configured", "expected"),
    [
        # Latin-script queries are never rewritten.
        ("artificial intelligence news", "us-en", "us-en"),
        ("AI", "us-en", "us-en"),
        # CJK queries get the edition that can actually serve them.
        ("AI 新闻 最新进展", "us-en", "cn-zh"),
        ("人工知能 ニュース", "us-en", "jp-ja"),
        ("인공지능 뉴스", "us-en", "kr-ko"),
        # Every script with a distinctive writing system, not just CJK.
        ("искусственный интеллект новости", "us-en", "ru-ru"),
        ("أخبار الذكاء الاصطناعي", "us-en", "eg-ar"),
        ("ειδήσεις τεχνητής νοημοσύνης", "us-en", "gr-el"),
        ("חדשות בינה מלאכותית", "us-en", "il-he"),
        ("ข่าวปัญญาประดิษฐ์", "us-en", "th-th"),
        ("कृत्रिम बुद्धिमत्ता समाचार", "us-en", "in-hi"),
        ("কৃত্রিম বুদ্ধিমত্তা সংবাদ", "us-en", "in-bn"),
        ("செயற்கை நுண்ணறிவு செய்திகள்", "us-en", "in-ta"),
        ("ხელოვნური ინტელექტის ახალი ამბები", "us-en", "ge-ka"),
        # Arabic script is shared: Persian and Urdu have exclusive letters.
        ("اخبار هوش مصنوعی پژوهش", "us-en", "ir-fa"),
        ("مصنوعی ذہانت کی خبریں ٹیکنالوجی", "us-en", "pk-ur"),
        # An operator who configured a matching region keeps their choice —
        # tw-zh must not be rewritten to cn-zh.
        ("AI 新闻", "tw-zh", "tw-zh"),
        # An empty / punctuation-only query has no script to go on.
        ("", "us-en", "us-en"),
        ("!!!", "us-en", "us-en"),
    ],
)
def test_detect_query_region(query, configured, expected):
    from search_mcp.engines.base import detect_query_region

    assert detect_query_region(query, configured) == expected


def test_kana_beats_han_for_japanese():
    """Japanese prose is mostly kanji, so a plain most-frequent-script vote
    sends 人工知能ニュース to the CHINESE edition on a 4-vs-4 tie — measured at
    36 items against 100 from the Japanese one. Kana is exclusive to Japanese,
    so its presence has to decide regardless of how much Han surrounds it."""
    from search_mcp.engines.base import detect_query_region

    assert detect_query_region("人工知能 ニュース", "us-en") == "jp-ja"
    # Han-heavy, one kana token — still Japanese.
    assert detect_query_region("最新人工知能技術研究開発の動向", "us-en") == "jp-ja"
    # No kana at all — Chinese.
    assert detect_query_region("最新人工智能技术研究", "us-en") == "cn-zh"
    # Hangul is likewise exclusive.
    assert detect_query_region("인공지능 漢字 뉴스", "us-en") == "kr-ko"


def test_latin_script_queries_are_never_rerouted():
    """Measured: the US/English edition serves German, French, Spanish,
    Vietnamese and Turkish queries at full volume, and script alone cannot tell
    them apart from English. Rerouting them would be a guess with nothing to
    gain."""
    from search_mcp.engines.base import detect_query_region

    for q in [
        "künstliche Intelligenz Nachrichten",
        "actualités intelligence artificielle",
        "noticias inteligencia artificial",
        "tin tức trí tuệ nhân tạo",
        "yapay zeka haberleri",
    ]:
        assert detect_query_region(q, "us-en") == "us-en"


def test_googlenews_picks_edition_from_query_script():
    """Google News is edition-scoped: the US/English feed answers a Chinese
    query with 0 items, which then reads as an outage. Measured on
    'AI 新闻 最新进展': 0 items on US:en, 35 on CN:zh-Hans."""
    from search_mcp.engines.googlenews import GoogleNewsEngine

    engine = GoogleNewsEngine()
    url = engine.build_url("AI 新闻 最新进展", 5, None)
    assert "ceid=CN:zh-Hans" in url
    # An English query on the same default config is untouched.
    assert "ceid=US:en" in engine.build_url("AI news", 5, None)


def test_google_url_follows_region_setting():
    from search_mcp.engines.google import GoogleEngine

    engine = GoogleEngine()
    with patch("search_mcp.engines.google.settings.region", "cn-zh"):
        url = engine.build_url("AI", 10, None)
    assert "hl=zh-CN" in url
    assert "gl=cn" in url
