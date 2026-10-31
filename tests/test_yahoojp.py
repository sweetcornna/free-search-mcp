"""YahooJapanEngine tests (offline, plus one live test).

The fixture is trimmed from live search.yahoo.co.jp pages saved on 2026-10-31
("python asyncio taskgroup exception handling" and "クレジットカード おすすめ"):
the real card structure with the favicons, tracking `ping` values and
`data-cl-params` cut down. No ad was served to this machine in any sample, so
the cards that must be skipped are the ones Yahoo! JAPAN does serve around the
organic hits: a Chiebukuro Q&A box and a video carousel. The live test runs
with ``SEARCH_MCP_TEST_NETWORK=1``.
"""

from __future__ import annotations

import os
from urllib.parse import parse_qs, urlparse

import pytest

from search_mcp.engines.base import SearchFilters, extract_date_hint
from search_mcp.engines.yahoojp import YahooJapanEngine

NETWORK = os.environ.get("SEARCH_MCP_TEST_NETWORK") == "1"
skip_offline = pytest.mark.skipif(not NETWORK, reason="set SEARCH_MCP_TEST_NETWORK=1 to run")


def _algo(href: str, title_html: str, summary_html: str, *, modifier: str = "", extra: str = "") -> str:
    cls = f"sw-Card Algo {modifier}".strip()
    return f"""
<div class="sw-CardBase"><div class="{cls}"><section>
  <div class="sw-Card__section sw-Card__section--header"><div class="sw-Card__headerSpace"><div class="sw-Card__title">
    <a href="{href}" class="sw-Card__titleInner" data-cl-params="_cl_vmodule:sr;_cl_link:title" ping="/xts/FOR=abc/*-https%3A//search.yahoo.co.jp/clear.gif" data-a11y-main-content="1"><br/>
      <h3 class="sw-Card__titleMain sw-Card__titleMain--clamp sw-Card__titleMain--siteName util-Clamps--2"><span>{title_html}</span></h3>
      <div class="sw-Card__titleCiteWrapper"><div class="sw-Cite Algo__cite"><span class="sw-Cite__favicon"><img src="" width="16" height="16" alt=""/></span>
        <div class="sw-Cite__text"><span class="sw-Cite__textSiteName util-Clamps--1">Site name</span>
          <div class="sw-Cite__textUrl"><cite class="sw-Breadcrumbs"><ol class="sw-Breadcrumbs__orderedList">
            <li class="sw-Breadcrumbs__item">https://example</li>
            <li class="sw-Breadcrumbs__item"><span class="sw-Breadcrumbs__delimiter" aria-hidden="true">&gt;</span><span>crumb</span></li>
          </ol></cite></div></div></div></div>
    </a>
  </div></div></div>
  <div class="sw-Card__section"><div class="sw-Card__space">
    <div class="sw-Card__floatContainer"><p class="sw-Card__summary">{summary_html}</p></div>{extra}
  </div></div>
</section></div></div>"""


_SITELINKS = """
<ul class="Algo__oslLinksList">
  <li class="Algo__oslLinksListItem"><a href="https://diamond.jp/zai/category/creditcard#%E5%B9%B4%E4%BC%9A%E8%B2%BB" data-cl-params="_cl_vmodule:sr;_cl_link:osl">◇年会費無料の高還元クレジット...</a></li>
  <li class="Algo__oslLinksListItem"><a href="https://diamond.jp/zai/category/creditcard#%E4%BB%98%E5%B8%AF" data-cl-params="_cl_vmodule:sr;_cl_link:osl">◇付帯特典充実おすすめプラチ...</a></li>
</ul>"""

_CHIEBUKURO = """
<div class="sw-CardBase"><div class="sw-Card AnswerChiebukuro"><section>
  <div class="sw-Card__section sw-Card__section--header"><div class="sw-Card__headerSpace sw-Card__headerSpace--5"><div class="sw-Card__title">
    <a href="https://chiebukuro.yahoo.co.jp/search/?rkf=1&amp;ei=UTF-8&amp;p=%E3%82%AF%E3%83%AC&amp;fr=sc_dr" class="sw-Card__titleInner" data-cl-params="_cl_vmodule:sc-top3;_cl_link:next"><br/>
      <h3 class="sw-Card__titleMain sw-Card__titleMain--cite"><strong>クレジットカード</strong> <strong>おすすめ</strong>に関するQ&amp;A（18,000件）</h3>
      <div class="sw-Card__titleCiteWrapper"><div class="sw-Cite"><cite>Yahoo!知恵袋</cite></div></div>
    </a>
  </div></div></div>
  <div class="sw-Card__section"><div class="sw-Card__space"><div class="sw-Iterable"><div class="sw-Iterable__item"><div class="AnswerChiebukuro__content">
    <div class="sw-Card__title"><h4 class="sw-Card__titleList util-Clamps--1"><span class="AnswerChiebukuro__mark AnswerChiebukuro__mark--question">Q.</span>
      <a href="https://detail.chiebukuro.yahoo.co.jp/qa/question_detail/q14332077679?fr=sc_dr" data-cl-params="_cl_vmodule:sc-top3;_cl_link:ttl">おすすめのクレジットカードは？</a></h4></div>
  </div></div></div></div></div>
</section></div></div>"""

_VIDEO = """
<div class="sw-CardBase"><div class="sw-Card AnswerVideoUniversal"><section>
  <div class="sw-Card__section sw-Card__section--header"><div class="sw-Card__headerSpace sw-Card__headerSpace--5"><div class="sw-Card__title">
    <a href="https://search.yahoo.co.jp/video/search?rkf=2&amp;ei=UTF-8&amp;fr=wsr_gvu&amp;p=python%20asyncio" class="sw-Card__titleInner"><br/>
      <h3 class="sw-Card__titleMain sw-Card__titleMain--cite"><span><strong>python</strong> <strong>asyncio</strong>の動画</span></h3>
      <div class="sw-Card__titleCiteWrapper"><div class="sw-Cite"><cite>Yahoo!検索（動画）</cite></div></div>
    </a>
  </div></div></div>
  <div class="sw-Expand"><div class="sw-Expand__contents"><div class="AnswerVideoUniversal__listItem sw-Card__section">
    <a href="https://www.youtube.com/watch?v=FvWXyAXyb4Q" class="AnswerVideoUniversal__listItemLink sw-Card__space"><div class="sw-Card__column">Python asyncio TaskGroup</div></a>
  </div></div></div>
</section></div></div>"""

_PAGE = (
    """<!doctype html><html lang="ja"><head><title>「python asyncio taskgroup」の検索結果 - Yahoo!検索</title></head>
<body><div id="contents" role="main" class="target_modules"><div id="contents__wrap"><div class="Contents__inner Contents__inner--main">
<div class="Contents__innerGroupHeader"><div class="Hits"><div class="Hits__contents"><div class="Hits__item">約124件<span class="util-Margin--Left8">1ページ目</span></div></div></div></div>
<div class="Contents__innerGroupBody">"""
    + _algo(
        "https://docs.python.org/3/library/asyncio-task.html",
        "Coroutines and tasks — <b>Python</b> 3.15.2 documentation",
        "In the case where a <b>task group</b> is cancelled externally and also must raise an ExceptionGroup , "
        "it will call the parent task&#39;s cancel() method. This ensures that&nbsp;...",
    )
    + _CHIEBUKURO
    + _algo(
        "https://diamond.jp/zai/category/creditcard",
        "【<b>クレジットカードおすすめ</b>比較】専門家が人気<b>クレカ</b>の還元 ...",
        '\n    <span class="util-Text--sub">2026/10/1</span>\n    <span class="util-Delimiter">-</span>'
        "【2026年9月3日時点】 <b>おすすめ</b>の「年会費無料の高還元<b>クレジットカード</b>」はコレだ！&nbsp;...",
        modifier="Algo-anotherSuggest",
        extra=_SITELINKS,
    )
    + _algo(
        "https://stackoverflow.com/questions/79986011/why-does-asyncio-taskgroup-intermittently",
        "Why does <b>asyncio</b>.<b>TaskGroup</b> intermittently swallow exceptions?",
        '<span class="util-Text--sub">5日前</span><span class="util-Delimiter">-</span>'
        "I&#39;m building a high-concurrency scraper with <b>asyncio</b>.<b>TaskGroup</b>.",
    )
    + _VIDEO
    # The same page again further down: must not be emitted twice.
    + _algo(
        "https://docs.python.org/3/library/asyncio-task.html",
        "Coroutines and tasks — duplicate",
        "Duplicate row.",
    )
    # Guards: a card that points back into Yahoo's own search, one with a
    # relative link, and one with no title.
    + _algo("https://search.yahoo.co.jp/search?p=asyncio&ei=UTF-8", "asyncio の検索結果", "Yahoo's own page.")
    + _algo("/search?p=Python%20asyncio&rkf=1&ei=UTF-8", "Relative link", "Not a result.")
    + _algo("https://example.com/no-title", "", "No title.")
    + """</div></div></div></div></body></html>"""
)


# ---------------------------------------------------------------------------
# parse
# ---------------------------------------------------------------------------


def test_parse_yields_direct_urls_titles_and_snippets():
    results = YahooJapanEngine().parse(_PAGE)
    assert [r.url for r in results] == [
        "https://docs.python.org/3/library/asyncio-task.html",
        "https://diamond.jp/zai/category/creditcard",
        "https://stackoverflow.com/questions/79986011/why-does-asyncio-taskgroup-intermittently",
    ]
    first = results[0]
    # The h3 alone: the anchor also holds the site name and the breadcrumb.
    assert first.title == "Coroutines and tasks — Python 3.15.2 documentation"
    assert first.snippet.startswith("In the case where a task group is cancelled externally")
    assert "Site name" not in first.title and "crumb" not in first.title
    assert all(r.engine == "yahoojp" and r.rank == 0 for r in results)


def test_highlights_keep_the_pages_own_spacing():
    # `<b>` highlights must not be split off with spaces: Japanese has none,
    # and "asyncio.TaskGroup" is one token.
    results = YahooJapanEngine().parse(_PAGE)
    assert results[1].title == "【クレジットカードおすすめ比較】専門家が人気クレカの還元 ..."
    assert results[2].title == "Why does asyncio.TaskGroup intermittently swallow exceptions?"
    assert results[1].snippet.startswith("2026/10/1 -【2026年9月3日時点】 おすすめの「年会費無料の高還元クレジットカード」")


def test_published_age_comes_from_the_snippet_and_is_not_confident():
    for r in YahooJapanEngine().parse(_PAGE):
        assert r.published_age == extract_date_hint(r.snippet)
        assert r.published_age_confident is False


def test_answer_cards_sitelinks_and_yahoo_links_are_skipped():
    results = YahooJapanEngine().parse(_PAGE)
    urls = [r.url for r in results]
    hosts = {urlparse(u).hostname for u in urls}
    # Chiebukuro box, video carousel (even its YouTube link), Yahoo's own search.
    assert not any("yahoo.co.jp" in (h or "") for h in hosts)
    assert "www.youtube.com" not in hosts
    # The site links nested inside the diamond.jp card are part of that hit.
    assert not any("#" in u for u in urls)


def test_parse_dedups_repeated_urls():
    urls = [r.url for r in YahooJapanEngine().parse(_PAGE)]
    assert len(urls) == len(set(urls))


@pytest.mark.parametrize(
    "html",
    [
        "",
        "<html><body>JavaScriptが無効です</body></html>",
        "<<<not html at all>>>",
        '<div class="sw-Card Algo"><section></section></div>',
    ],
)
def test_unparseable_page_yields_nothing(html):
    assert YahooJapanEngine().parse(html) == []


# ---------------------------------------------------------------------------
# build_url
# ---------------------------------------------------------------------------


def _params(url: str) -> dict[str, list[str]]:
    return parse_qs(urlparse(url).query)


def test_build_url_plain_query():
    url = YahooJapanEngine().build_url("人工智能 最新进展", 10)
    assert url.startswith("https://search.yahoo.co.jp/search?")
    params = _params(url)
    assert params["p"] == ["人工智能 最新进展"]
    assert params["ei"] == ["UTF-8"]
    assert "vd" not in params


def test_build_url_carries_site_operators():
    filters = SearchFilters(include_domains=["docs.python.org"], exclude_domains=["reddit.com"])
    q = _params(YahooJapanEngine().build_url("asyncio taskgroup", 10, filters))["p"][0]
    assert q == "asyncio taskgroup site:docs.python.org -site:reddit.com"


def test_build_url_multiple_include_domains_are_or_ed():
    filters = SearchFilters(include_domains=["docs.python.org", "peps.python.org"])
    q = _params(YahooJapanEngine().build_url("taskgroup", 10, filters))["p"][0]
    assert q == "taskgroup (site:docs.python.org OR site:peps.python.org)"


def test_build_url_pdf_category_adds_filetype():
    q = _params(YahooJapanEngine().build_url("asyncio", 10, SearchFilters(category="pdf")))["p"][0]
    assert q == "asyncio filetype:pdf"


@pytest.mark.parametrize(
    ("freshness", "vd"),
    # month is m1: a bare `vd=m` is not a value Yahoo! JAPAN accepts.
    [("day", "d"), ("week", "w"), ("month", "m1"), ("year", "y")],
)
def test_build_url_freshness_uses_vd(freshness, vd):
    url = YahooJapanEngine().build_url("python release", 10, SearchFilters(freshness=freshness))
    assert _params(url)["vd"] == [vd]


def test_description_is_one_short_line():
    d = YahooJapanEngine.description
    assert d and "\n" not in d and len(d) <= 90


# ---------------------------------------------------------------------------
# live
# ---------------------------------------------------------------------------


@skip_offline
async def test_live_search_returns_direct_urls(monkeypatch):
    from search_mcp.config import settings

    monkeypatch.setattr(settings, "fetch_strategy", "http")
    results = await YahooJapanEngine().search("python asyncio taskgroup", 10, None, diagnostics={})
    assert results, "Yahoo! JAPAN returned nothing (markup change, or a geo block?)"
    for r in results:
        assert r.url.startswith(("http://", "https://"))
        assert "search.yahoo.co.jp" not in r.url
        assert r.title
