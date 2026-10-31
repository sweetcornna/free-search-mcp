"""QuarkEngine tests.

The fixture is trimmed from live quark.sm.cn result pages (2026-10-31:
"python 异步编程", "rust tokio select macro" and a week-filtered page). It keeps
Quark's real card wrappers (`div.sc` with a `data-sc` JSON naming the card
type) and its `qk-*` components: four organic cards of the four organic types,
then what must NOT become a result — the related-searches card (which has a
`.qk-title-text` of its own), a Baidu Baike knowledge card built from the same
template as an organic card, a `nature_result` wrapping Quark's own video page,
a client-rendered note card, and a `cpc` ad. All offline; one live test is
gated on ``SEARCH_MCP_TEST_NETWORK=1``.
"""

from __future__ import annotations

import html
import json
import os

import pytest

from search_mcp.engines.base import SearchFilters, detect_gate
from search_mcp.engines.quark import QuarkEngine

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.

NETWORK = os.environ.get("SEARCH_MCP_TEST_NETWORK") == "1"
skip_offline = pytest.mark.skipif(
    not NETWORK, reason="set SEARCH_MCP_TEST_NETWORK=1 to run"
)


def _sc(sc: str, **extra) -> str:
    """A card's `data-sc` attribute, escaped the way Quark serves it."""
    meta = {"sc": sc, "q": "python+%E5%BC%82%E6%AD%A5%E7%BC%96%E7%A8%8B", "pos": "1_1", **extra}
    return html.escape(json.dumps(meta, ensure_ascii=False), quote=True)


_SS_TEXT = f"""
<div class="sc sc_structure_template_normal" data-sc="{_sc("ss_text", ads=0)}" data-tpl="structure_template_normal" id="sc_ss_text_1_2_48094_59709"><div class="qk-card qk-card-media-mobile qk-card-base qk-card-pos-1_2 qk-cursor-pointer kv klOfggoeIB" data-cmp="ca">
 <div class="qk-title qk-title-size-xl qk-title-clamp-2 qk-title-full qk-title-mobile title" data-cmp="ti"><div class="qk-title-content"><a href="https://blog.csdn.net/agoutongxie/article/details/167988798" class="qk-link-wrapper"><div class="qk-title-text-wrapper"><div class="qk-title-text qk-font-bold"><em>Python异步编程</em>实战指南-CSDN博客</div></div></a></div></div>
 <div class="summary-section"><a href="https://blog.csdn.net/agoutongxie/article/details/167988798" class="qk-link-wrapper"><div class="qk-paragraph qk-color-darkest" data-cmp="pa"><div class="qk-paragraph-content qk-clamp-3"><div class="qk-paragraph-value"><div class="qk-paragraph-text">IO 密集场景异步才是王道——<em>asyncio 是 Python 异步编程的核心</em>。本文覆盖协程、事件循环、async/await。</div></div></div></div></a></div>
 <div class="source-section"><div class="qk-source qk-source-media-mobile qk-source-full" data-cmp="so"><div class="qk-source-content"><a href="https://blog.csdn.net/agoutongxie/article/details/167988798" class="qk-link-wrapper"><div class="qk-source-items"><span class="qk-source-item qk-clamp-1">CSDN博客 · 2026-10-15</span></div></a></div></div></div>
</div></div>
"""

_SS_KV = f"""
<div class="sc sc_structure_template_normal" data-sc="{_sc("ss_kv", ads=0)}" data-tpl="structure_template_normal" id="sc_ss_kv_1_5_48091_99334"><div class="qk-card qk-card-media-mobile qk-card-base qk-card-pos-1_5 kv uLYmpwZpjV" data-cmp="ca">
 <div class="qk-title qk-title-size-xl qk-title-clamp-2 qk-title-full qk-title-mobile qk-margin-bottom-m" data-cmp="ti"><div class="qk-title-content"><a href="https://cloud.tencent.com/developer/article/2532143" class="qk-link-wrapper" data-log="0"><div class="qk-title-text-wrapper"><div class="qk-title-text qk-font-bold">rust select! 与取消安全的 Future-腾讯云</div></div></a></div></div>
 <div class="qk-grid"><div class="qk-row qk-row-cols-2"><div class="qk-col" data-cmp="co"><div class="qk-paragraph" data-cmp="pa"><div class="qk-paragraph-content qk-clamp-3"><div class="qk-paragraph-value"><a href="https://cloud.tencent.com/developer/article/2532143" class="qk-link-wrapper" data-log="1"><div class="qk-paragraph-text"><em>tokio</em>::<em>select</em>! 宏用于并发 poll 多个 Future，并在首个完成时返回。</div></a></div></div></div>
 <div class="qk-source qk-source-media-mobile qk-source-full qk-margin-top-m" data-cmp="so"><div class="qk-source-content"><a href="https://cloud.tencent.com/developer/article/2532143" class="qk-link-wrapper" data-log="2"><div class="qk-source-items"><span class="qk-source-item qk-clamp-1">cloud.tencent.com</span></div></a></div></div></div></div></div>
</div></div>
"""

_NATURE = f"""
<div class="sc sc_natural_result result js-nature-result nature-result" data-sc="{_sc("nature_result")}" data-tpl="natural_result~index" id="sc_nature_result_1_5_48429_80584"><div class="qk-card qk-card-media-mobile qk-card-base qk-card-pos-1_5" data-cmp="ca">
 <a href="http://m.php.cn/article/561601.html" class="qk-link-wrapper" data-log="1"><div class="qk-title qk-title-size-xl qk-title-clamp-2 qk-title-full qk-title-mobile qk-margin-bottom-m" data-cmp="ti"><div class="qk-title-content"><div class="qk-title-text-wrapper"><div class="qk-title-text qk-font-bold"><em>Python</em> web<em>异步编程</em>的基础-php中文网</div></div></div></div>
 <div class="qk-paragraph" data-cmp="pa"><div class="qk-paragraph-content qk-clamp-3"><div class="qk-paragraph-value"><div class="qk-paragraph-text">随着web应用程序的不断增长和复杂性的增加，web<em>异步编程</em>变得越来越流行。</div></div></div></div></a>
 <div class="qk-source qk-source-media-mobile qk-source-full qk-margin-top-m" data-cmp="so"><div class="qk-source-content"><a href="http://m.php.cn/article/561601.html" class="qk-link-wrapper" data-log="4"><div class="qk-source-items"><span class="qk-source-item qk-clamp-1">php中文网</span></div></a></div></div>
</div></div>
"""

_SS_PIC_RECENT = f"""
<div class="sc sc_structure_template_normal" data-sc="{_sc("ss_pic", ads=0)}" data-tpl="structure_template_normal" id="sc_ss_pic_1_7_44370_68269"><div class="qk-card qk-card-media-mobile qk-card-base qk-card-pos-1_7" data-cmp="ca">
 <div class="qk-title qk-title-size-xl" data-cmp="ti"><div class="qk-title-content"><a href="https://liaoxuefeng.com/books/python/async-io/asyncio/index.html" class="qk-link-wrapper"><div class="qk-title-text-wrapper"><div class="qk-title-text qk-font-bold">使用<em>asyncio</em>-<em>Python</em>教程-廖雪峰的官方网站</div></div></a></div></div>
 <div class="summary-section"><a href="https://liaoxuefeng.com/books/python/async-io/asyncio/index.html" class="qk-link-wrapper"><div class="qk-paragraph" data-cmp="pa"><div class="qk-paragraph-text"><em>asyncio</em> 是Python 3.4版本引入的标准库，直接内置了对异步IO的支持。</div></div></a></div>
 <div class="qk-picture qk-picture-3-2" data-cmp="pi"><a href="https://liaoxuefeng.com/books/python/async-io/asyncio/index.html" class="qk-link-wrapper"><div class="qk-image qk-image-lazy"><img class="qk-image-background-img"></div></a></div>
 <div class="source-section"><div class="qk-source" data-cmp="so"><div class="qk-source-content"><a href="https://liaoxuefeng.com/books/python/async-io/asyncio/index.html" class="qk-link-wrapper"><div class="qk-source-items"><span class="qk-source-item qk-clamp-1">廖雪峰的官方网站 · 6天前</span></div></a></div></div></div>
</div></div>
"""

_RELATED = f"""
<div class="sc sc_text_recommend sc_text_recommend" data-sc="{_sc("text_recommend", ads=0)}" data-tpl="text_recommend~index" id="sc_text_recommend_1_3_47914_79864"><div><div class="qk-card qk-card-media-mobile qk-card-base qk-card-pos-1_3" data-cmp="ca">
 <div class="qk-view qk-display-flex" data-cmp="vi"><div class="qk-title qk-title-size-l" data-cmp="ti"><div class="qk-title-content"><div class="qk-title-text-wrapper"><div class="qk-title-text qk-font-bold">其他人还搜了</div></div></div></div></div>
 <a href="https://quark.sm.cn/s?q=rust+tokio%E5%AE%9E%E6%88%98%E6%8C%87%E5%8D%97&amp;by=tuijian&amp;by2=text" class="c-block"><div class="btn-text">rust tokio实战指南</div></a>
</div></div></div>
"""

_BAIKE = f"""
<div class="sc sc_structure_template_normal" data-sc="{_sc("baike", ads=0)}" data-tpl="structure_template_normal" id="sc_baike_1_1_48426_98649"><div class="qk-card qk-card-media-mobile qk-card-base qk-card-pos-1_1" data-cmp="ca">
 <div class="qk-title qk-title-size-xl" data-cmp="ti"><div class="qk-title-content"><a href="https://baike.baidu.com/item/Python%E5%BC%82%E6%AD%A5%E7%BC%96%E7%A8%8B/56879040" class="qk-link-wrapper"><div class="qk-title-text-wrapper"><div class="qk-title-text qk-font-bold"><em>Python异步编程</em>实战——基于AIO的全栈开发技术-百度百科</div></div></a></div></div>
 <div class="qk-paragraph" data-cmp="pa"><div class="qk-paragraph-text">2021年清华大学出版社出版的图书</div></div>
</div></div>
"""

_OWN_VIDEO = f"""
<div class="sc sc_natural_result result js-nature-result nature-result" data-sc="{_sc("nature_result")}" data-tpl="natural_result~index" id="sc_nature_result_1_2_47914_67195"><div class="qk-card qk-card-media-mobile qk-card-base qk-card-pos-1_2" data-cmp="ca">
 <a href="https://page.sm.cn/blm/video-page-710/video?h=www.bilibili.com&amp;id=26_89e397d9449bcbe8875b714dff1c3c37&amp;q=rust+tokio+select+macro" class="qk-link-wrapper"><div class="qk-title" data-cmp="ti"><div class="qk-title-content"><div class="qk-title-text-wrapper"><div class="qk-title-text qk-font-bold">Async Rust - select! 宏</div></div></div></div></a>
 <div class="qk-source" data-cmp="so"><div class="qk-source-items"><span class="qk-source-item qk-clamp-1">哔哩哔哩</span><span class="qk-source-item qk-clamp-1">2026-01-29</span></div></div>
</div></div>
"""

# Notes render client-side: the server sends an empty shell.
_NOTE = f"""<div class="sc sc_structure_template_normal" data-sc="{_sc("ss_note", ads=0)}" id="sc_ss_note_1_9_47913_1"><div class="qk-card"></div></div>"""

_AD = """
<div class="cpc-card cpc_img c-container ad-wolong-c"><a class="cpc-a" href="http://quark.sm.cn/adclick?disguise_click_url=AMIAAIgCAAAhXD" index="" adtype="" data-url="" click_area="click_url"><img src="http://content-understand-strategy.sm.cn/1688/20231218_style1/fef474874e764e348f8a9f59a05a68ef_style1.jpg" class="cpc-img-m c-radius-l pic-img" click_area="pic_url"><div class="cpc-img-ad">广告</div></a></div>
"""

_HTML = (
    '<html><head><title>python 异步编程</title></head><body class="scene-quark newtheme">'
    '<article id="wraper"><section id="main"><div class="c-theme-olympic c-theme-quark" id="results">'
    + _OWN_VIDEO
    + _SS_TEXT
    + _RELATED
    + _AD
    + _SS_KV
    + _BAIKE
    + _NATURE
    + _NOTE
    + _SS_PIC_RECENT
    + _SS_TEXT  # the same card twice must not count twice
    + "</div></section></article></body></html>"
)


# ---------------------------------------------------------------------------
# parse
# ---------------------------------------------------------------------------


def test_parse_yields_the_organic_cards_in_order():
    out = QuarkEngine().parse(_HTML)
    assert [r.url for r in out] == [
        "https://blog.csdn.net/agoutongxie/article/details/167988798",
        "https://cloud.tencent.com/developer/article/2532143",
        "http://m.php.cn/article/561601.html",
        "https://liaoxuefeng.com/books/python/async-io/asyncio/index.html",
    ]
    assert all(r.engine == "quark" and r.rank == 0 for r in out)


def test_titles_and_snippets_are_clean():
    out = QuarkEngine().parse(_HTML)
    assert out[0].title == "Python异步编程实战指南-CSDN博客"
    assert out[0].snippet.startswith("IO 密集场景异步才是王道——asyncio 是 Python 异步编程的核心")
    # <em> around "::" and "!" must not gain spaces.
    assert out[1].snippet.startswith("tokio::select! 宏用于并发 poll")
    assert out[2].title == "Python web异步编程的基础-php中文网"


def test_widgets_ads_and_own_pages_are_skipped():
    urls = [r.url for r in QuarkEngine().parse(_HTML)]
    assert not any("sm.cn" in u or "quark.cn" in u for u in urls)
    assert not any("baike.baidu.com" in u for u in urls)
    assert "其他人还搜了" not in [r.title for r in QuarkEngine().parse(_HTML)]


def test_ads_flag_on_a_card_is_honoured():
    flagged = _SS_TEXT.replace(html.escape('"ads": 0'), html.escape('"ads": 1'))
    assert flagged != _SS_TEXT
    assert QuarkEngine().parse(flagged + _SS_KV)[0].url == (
        "https://cloud.tencent.com/developer/article/2532143"
    )


def test_source_line_date_becomes_published_age():
    out = {r.url: r for r in QuarkEngine().parse(_HTML)}
    assert out["https://blog.csdn.net/agoutongxie/article/details/167988798"].published_age == "2026-10-15"
    assert out["https://liaoxuefeng.com/books/python/async-io/asyncio/index.html"].published_age == "6 days ago"
    assert out["http://m.php.cn/article/561601.html"].published_age == ""
    assert not any(r.published_age_confident for r in out.values())


@pytest.mark.parametrize(
    "page",
    [
        "",
        "<html><body>nothing here</body></html>",
        _RELATED + _BAIKE + _OWN_VIDEO + _NOTE + _AD,
        # Quark's answer to any `site:` query: the no-results page.
        "<html><body><div id='results'>抱歉，没有找到与“asyncio site:zhihu.com”相关的结果。</div></body></html>",
        # A card whose data-sc is not JSON.
        '<div class="sc" data-sc="{oops"><div class="qk-title-text">x</div></div>',
    ],
)
def test_pages_without_organic_cards_yield_empty(page):
    assert QuarkEngine().parse(page) == []


def test_x5sec_punish_page_yields_empty():
    """Quark's burst wall, trimmed from a live response. It parses to [], and
    `detect_gate` names it a captcha so the breaker benches Quark at once."""
    wall = (
        '<script>sessionStorage.x5referer = window.location.href;var url = window.location.protocol'
        ' + "//quark.sm.cn//s/_____tmd_____/punish?x5secdata=xfD2F0HgaLjOD8vVoxMij&x5step=1";'
        'window.location.replace(url);window._config_ = {"action":"captcha","url":'
        '"https://quark.sm.cn//s/_____tmd_____/punish?x5secdata=xfD2F0HgaLjOD8vVoxMij&x5step=1"};'
        "</script><!--rgv587_flag:sm-->"
    )
    assert QuarkEngine().parse(wall) == []
    assert detect_gate(wall, QuarkEngine.gate_markers) == "captcha"


# ---------------------------------------------------------------------------
# build_url
# ---------------------------------------------------------------------------


def test_build_url_plain_query():
    url = QuarkEngine().build_url("python 异步编程", 10)
    assert url == "https://quark.sm.cn/s?q=python+%E5%BC%82%E6%AD%A5%E7%BC%96%E7%A8%8B"


def test_build_url_never_adds_operators_quark_rejects():
    """Any `site:`/`-site:` query returns Quark's empty page, and `filetype:` is
    read as words, so domain and PDF filters must stay out of the query."""
    e = QuarkEngine()
    url = e.build_url(
        "asyncio",
        10,
        SearchFilters(include_domains=["zhihu.com"], exclude_domains=["csdn.net"], category="pdf"),
    )
    assert url == e.build_url("asyncio", 10)
    assert "site" not in url and "filetype" not in url


@pytest.mark.parametrize(
    "freshness, token", [("day", "4"), ("week", "3"), ("month", "2"), ("year", "1")]
)
def test_build_url_maps_freshness_to_tl_request(freshness, token):
    url = QuarkEngine().build_url("q", 10, SearchFilters(freshness=freshness))
    assert url.endswith(f"&tl_request={token}")


def test_description_is_one_short_line():
    d = QuarkEngine.description
    assert d and "\n" not in d and len(d) <= 90


# ---------------------------------------------------------------------------
# Live network test
# ---------------------------------------------------------------------------


@skip_offline
async def test_live_quark_search():
    out = await QuarkEngine().search("python 异步编程", 10)
    if not out:
        pytest.skip("Quark returned no results (transient, or its x5sec burst wall)")
    assert all(r.url.startswith("http") and r.engine == "quark" for r in out)
    assert not any("sm.cn" in r.url for r in out)
