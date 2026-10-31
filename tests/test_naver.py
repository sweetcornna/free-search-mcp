"""NaverEngine tests.

The fixture is trimmed from live result pages (2026-10-31: "python asyncio
taskgroup", plus the power-link ad block and a blog card from "파이썬 비동기
프로그래밍"). It keeps Naver's real nesting and design-system class names:
three web documents, then everything that must NOT become a result — the tab
navigation, the power-link ad section, a Naver blog (UGC) card, and each
document's image and breadcrumb anchors. All offline; one live test is gated on
``SEARCH_MCP_TEST_NETWORK=1``.
"""

from __future__ import annotations

import os

import pytest

from search_mcp.engines.base import SearchFilters
from search_mcp.engines.naver import NaverEngine

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.

NETWORK = os.environ.get("SEARCH_MCP_TEST_NETWORK") == "1"
skip_offline = pytest.mark.skipif(
    not NETWORK, reason="set SEARCH_MCP_TEST_NETWORK=1 to run"
)

_NAV = """
<div class="api_flicking_wrap">
  <a href="?ssc=tab.blog.all&amp;sm=tab_jum&amp;query=python+asyncio+taskgroup" class="tab" aria-selected="false">블로그</a>
  <a href="?ssc=tab.news.all&amp;where=news&amp;sm=tab_jum&amp;query=python+asyncio+taskgroup" class="tab" aria-selected="false">뉴스</a>
  <a href="?ssc=tab.nx.all&amp;where=nexearch&amp;sm=tab_opt&amp;query=python+asyncio+taskgroup&amp;nso=p%3A1w&amp;nso_open=1">1주</a>
</div>
"""

_AD = """
<section><div id="pcPowerLink_ea68-9w2m" class="pcPowerLink_ea68"><div class="sc_new ad_section section _pl_section bg_type_v2"><div class="api_subject_bx">
  <div class="mod_title_area"><div class="title_wrap"><h2 class="title_area"><span class="title">파이썬비동기프로그래밍</span> <span class="ad_text">관련 광고</span></h2></div></div>
  <div class="nad_area" id="power_link_body"><ul class="lst_type"><li class="lst js-hover-item type_sublink"><div class="inner"><div class="title_url_area"><div class="url_area">
    <span class="ad_mark" role="img" aria-label="광고"> </span>
    <a href="https://ader.naver.com/v1/6D7mN9Mp5fvBx0yOlTcNrn1VROL5VWWJIkWQRTusKA3Y?c=naver.search.pc.plsearch&amp;t=0" class="lnk_head" target="_blank"><span class="lnk_tit">파이썬 비동기 프로그래밍 강의 - 지금 수강신청</span></a>
  </div></div></div></li></ul></div>
</div></div></div></section>
"""


def _doc(url: str, date: str, crumbs: str, title: str, snippet: str) -> str:
    """One `fds-web-doc-root`, nesting and classes as served (Keep menu and
    favicon image trimmed)."""
    date_link = (
        f'<a class="sds-comps-horizontal-layout sds-comps-inline-layout fds-anchor-layout mCJiOlyftI4kjifC J4ypZYOl_FAfWcQn" href="{url}" data-nlog-area="urB_coR.w.link" data-heatmap-target=".link">'
        f'<span class="sds-comps-text sds-comps-text-type-body3 sds-comps-text-weight-sm VwRv7x9LnDcMzNlm">{date}</span>'
        '<span class="PTx_qoYxT2svByfz">새 창 열림</span></a>'
        '<span class="sds-comps-base-layout sds-comps-full-layout sds-comps-divider sds-comps-divider-dot"></span>'
        if date
        else ""
    )
    return f"""
<div class="sds-comps-vertical-layout sds-comps-full-layout fds-web-doc-root YmmugzkrL889KGpD aDl8NmJvmR0uNF_U fds-web-normal-doc-root fds-root-overflow-reset"><div class="sds-comps-vertical-layout sds-comps-full-layout hdS7ohCqeYVmaMvj">
 <div class="sds-comps-horizontal-layout sds-comps-full-layout e3EG6tQagtLzhjyU"><div class="sds-comps-horizontal-layout sds-comps-full-layout sds-comps-profile type-web size-lg variant-2line" data-sds-comp="Profile"><div class="sds-comps-horizontal-layout sds-comps-inline-layout sds-comps-profile-source"><div class="sds-comps-vertical-layout sds-comps-inline-layout sds-comps-profile-text">
  <div class="sds-comps-horizontal-layout sds-comps-inline-layout sds-comps-profile-info-title"><span class="sds-comps-text sds-comps-text-ellipsis sds-comps-text-ellipsis-1 sds-comps-text-type-body3 sds-comps-text-weight-sm sds-comps-profile-info-title-text"><div class="sds-comps-horizontal-layout sds-comps-inline-layout fds-anchor-layout mCJiOlyftI4kjifC eCAu6lU7YDISQycJ" data-nlog-area="urB_coR.w.brand" data-heatmap-target=".brand">{crumbs.split(" ")[0]}</div></span></div>
  <div class="sds-comps-horizontal-layout sds-comps-inline-layout sds-comps-profile-info-subtexts"><span class="sds-comps-text sds-comps-text-type-body3 sds-comps-text-weight-sm sds-comps-profile-info-subtext"><div class="sds-comps-horizontal-layout sds-comps-full-layout cqai6mMDJYIuSlpc">{date_link}<a class="sds-comps-horizontal-layout sds-comps-inline-layout fds-anchor-layout mCJiOlyftI4kjifC fgzdFiwrV8Mz68YT" href="{url}" data-nlog-area="urB_coR.w.bread" data-heatmap-target=".bread"><div class="sds-comps-horizontal-layout sds-comps-inline-layout g0AyfhvnRhsAUSCK _kCaqa_qlLYIeqmh"><span class="sds-comps-text sds-comps-text-type-body3 sds-comps-text-weight-sm">{crumbs}</span></div><span class="PTx_qoYxT2svByfz">새 창 열림</span></a></div></span></div>
 </div></div></div></div>
 <div class="sds-comps-vertical-layout sds-comps-full-layout XTH64Qzdz2Ag7ToC Dv6RWoSvwGgllVuQ">
  <a class="sds-comps-horizontal-layout sds-comps-inline-layout fds-anchor-layout mCJiOlyftI4kjifC ThQqDhlXvRsAxx6K" href="{url}" data-nlog-area="urB_coR.w.simg" data-heatmap-target=".simg"><div class="sds-comps-base-layout sds-comps-inline-layout sds-comps-image fit-contain"><img width="104" alt="" loading="lazy" src="https://search.pstatic.net/sunny?src=x&amp;type=fff208_208_ar"></div><span class="PTx_qoYxT2svByfz">새 창 열림</span></a>
  <div class="sds-comps-horizontal-layout sds-comps-full-layout amF1zj0z3H4QT5ND"><a class="sds-comps-horizontal-layout sds-comps-inline-layout fds-anchor-layout mCJiOlyftI4kjifC zNIY6IGsvHctYax3" href="{url}" data-nlog-area="urB_coR.w.link" data-heatmap-target=".link"><span class="sds-comps-text sds-comps-text-ellipsis sds-comps-text-ellipsis-1 sds-comps-text-type-headline1 sds-comps-text-weight-sm">{title}</span><span class="PTx_qoYxT2svByfz">새 창 열림</span></a></div>
  <div class="sds-comps-base-layout sds-comps-full-layout Dv6RWoSvwGgllVuQ"><a class="sds-comps-horizontal-layout sds-comps-inline-layout fds-anchor-layout mCJiOlyftI4kjifC Ni0KMykE2aMcmXoT" href="{url}" data-nlog-area="urB_coR.w.link" data-heatmap-target=".link"><span class="sds-comps-text sds-comps-text-ellipsis sds-comps-text-ellipsis-3 sds-comps-text-type-body1 sds-comps-text-weight-sm _rjF5rcfm1UFZh9D">{snippet}</span><span class="PTx_qoYxT2svByfz">새 창 열림</span></a></div>
 </div>
</div></div>
"""


_REALPYTHON = _doc(
    "https://realpython.com/python-async-features/",
    "2026.08.31.",
    "realpython.com › python-async-features",
    "Async Programming in <mark>Python</mark>: From Generators to <mark>asyncio</mark> – Real <mark>Python</mark>",
    "Learn how <mark>Python</mark> async programming works. Write async functions with async and await, and run slow I/O operations concurrently with <mark>asyncio</mark>.",
)
_YUN = _doc(
    "https://076923.github.io/posts/Python-51/",
    "3주 전",
    "076923.github.io › posts",
    "<mark>Python</mark> 강좌 : 제 51강 - 비동기 처리 - YUN DAE HEE",
    "비동기 처리(Asynchronous Processing)란 어떤 작업이 끝나기를 기다리는 동안 다른 작업을 먼저 진행하는 처리 방식입니다. <mark>Python</mark>에서.....",
)
_GIHYO = _doc(
    "https://gihyo.jp/article/2022/10/monthly-python-2210",
    "",
    "gihyo.jp › monthly-python-2210",
    "<mark>Python</mark> 3.11の新機能：<mark>asyncio</mark>.<mark>TaskGroup</mark>を使った予測可能でより安全な非同期処理",
    "こんにちは、福田（@JunyaFff）です。",
)

# A Naver blog card from the same ranked collection: Naver's own UGC vertical.
_BLOG_CARD = """
<div class="sds-comps-vertical-layout sds-comps-full-layout fds-ugc-ur-desk"><div class="sds-comps-vertical-layout sds-comps-full-layout lMwO9NEdXjc0vwjg">
 <div class="sds-comps-horizontal-layout sds-comps-full-layout sds-comps-profile type-basic size-lg variant-2line"><div class="sds-comps-vertical-layout sds-comps-inline-layout sds-comps-profile-text">
  <a nocr="1" class="a0TqvBP4mYg4KtNt E3sEDEJtS4ZKRrsu" href="https://blog.naver.com/vronpy" data-heatmap-target="articleSourceJSX_title" data-nlog-area="urB_b1R.b.profile"><span class="sds-comps-text sds-comps-text-ellipsis sds-comps-text-ellipsis-1">&lt; 코딩자율학습 23기 &gt;</span></a>
  <span class="sds-comps-text sds-comps-text-type-body3 sds-comps-text-weight-sm sds-comps-profile-info-subtext"><a nocr="1" class="a0TqvBP4mYg4KtNt GmLWOnFW8XQcjb1b" href="https://blog.naver.com/vronpy/224459930245" data-heatmap-target="articleSourceJSX_createdDate" data-nlog-area="urB_b1R.b.link"><span class="sds-comps-text sds-comps-text-ellipsis sds-comps-text-ellipsis-1">2시간 전</span></a></span>
 </div></div>
 <a class="sds-comps-base-layout sds-comps-full-layout fds-anchor-layout mCJiOlyftI4kjifC HgnTHsNDdAl3H7IO" href="https://blog.naver.com/vronpy/224459930245" data-nlog-area="urB_b1R.b.link" data-heatmap-target=".link"><span class="sds-comps-text sds-comps-text-ellipsis sds-comps-text-ellipsis-1 ai26KQMBqIovDWBE">[1주차 학습일지] - 나도코딩의 <mark>파이썬</mark> 입문</span></a>
 <a class="sds-comps-base-layout sds-comps-full-layout fds-anchor-layout mCJiOlyftI4kjifC eXf5fZZ50geaPQ7a" href="https://blog.naver.com/vronpy/224459930245" data-nlog-area="urB_b1R.b.link" data-heatmap-target=".link"><span class="sds-comps-text sds-comps-text-ellipsis sds-comps-text-ellipsis-2 sds-comps-text-type-body1">이번 주에는 파이썬 기초 문법을 정리했다.</span></a>
</div></div>
"""


def _collection(*items: str) -> str:
    return (
        '<div class="spw_fsolid type_head _fsolid_head _slog_visible" data-collection="urB_coR" data-nlog-area="urB_coR">'
        '<div class="sc_new desktop_mode" data-collection="fender_renderer" data-meta-area="urB_coR" data-meta-ssc="tab.nx.all">'
        '<div class="tYcn5aob2vpZiJ7ReP2D desktop_mode api_subject_bx">'
        + "".join(
            '<div class="sds-comps-vertical-layout sds-comps-full-layout fds-web-root">'
            f'<div class="sds-comps-vertical-layout sds-comps-full-layout fds-web-list-root">{item}</div></div>'
            for item in items
        )
        + "</div></div></div>"
    )


_HTML = (
    "<html><head><title>python asyncio taskgroup : 네이버 검색</title></head><body>"
    + _NAV
    + _AD
    + _collection(_REALPYTHON, _YUN, _BLOG_CARD, _GIHYO, _REALPYTHON)
    + "</body></html>"
)


# ---------------------------------------------------------------------------
# parse
# ---------------------------------------------------------------------------


def test_parse_yields_web_documents_in_order():
    out = NaverEngine().parse(_HTML)
    assert [r.url for r in out] == [
        "https://realpython.com/python-async-features/",
        "https://076923.github.io/posts/Python-51/",
        "https://gihyo.jp/article/2022/10/monthly-python-2210",
    ]
    assert all(r.engine == "naver" and r.rank == 0 for r in out)


def test_titles_and_snippets_are_clean():
    out = NaverEngine().parse(_HTML)
    assert out[0].title == "Async Programming in Python: From Generators to asyncio – Real Python"
    assert out[0].snippet.startswith("Learn how Python async programming works.")
    # <mark> mid-word or around a dot must not gain spaces.
    assert out[1].snippet.endswith("Python에서.....")
    assert out[2].title == "Python 3.11の新機能：asyncio.TaskGroupを使った予測可能でより安全な非同期処理"
    assert not any("새 창 열림" in r.title + r.snippet for r in out)


def test_new_window_label_is_stripped_even_inside_the_title_span():
    page = _HTML.replace(
        "Real <mark>Python</mark></span>",
        'Real <mark>Python</mark><span class="blind">새 창 열림</span></span>',
    )
    assert NaverEngine().parse(page)[0].title.endswith("– Real Python")


def test_profile_date_becomes_published_age():
    out = NaverEngine().parse(_HTML)
    assert out[0].published_age == "2026-08-31"
    assert out[1].published_age == "3 weeks ago"
    assert out[2].published_age == ""
    assert not any(r.published_age_confident for r in out)


def test_ads_ugc_cards_and_navigation_are_skipped():
    urls = [r.url for r in NaverEngine().parse(_HTML)]
    assert not any("naver.com" in u for u in urls)
    assert len(urls) == len(set(urls))


def test_ad_redirect_is_skipped_even_as_a_web_document():
    ad_doc = _doc(
        "https://ader.naver.com/v1/6D7mN9Mp5fvBx0yOlTcN?c=naver.search.pc.plsearch",
        "",
        "ader.naver.com",
        "파이썬 강의",
        "광고",
    )
    out = NaverEngine().parse(_collection(ad_doc, _YUN))
    assert [r.url for r in out] == ["https://076923.github.io/posts/Python-51/"]


@pytest.mark.parametrize(
    "page",
    [
        "",
        "<html><body>nothing here</body></html>",
        _NAV + _AD + _collection(_BLOG_CARD),
        # A document whose title span lost its anchor.
        '<div class="fds-web-doc-root"><span class="sds-comps-text-type-headline1">orphan</span></div>',
    ],
)
def test_pages_without_web_documents_yield_empty(page):
    assert NaverEngine().parse(page) == []


# ---------------------------------------------------------------------------
# build_url
# ---------------------------------------------------------------------------


def test_build_url_plain_query():
    url = NaverEngine().build_url("파이썬 비동기", 10)
    assert url == (
        "https://search.naver.com/search.naver?where=nexearch"
        "&query=%ED%8C%8C%EC%9D%B4%EC%8D%AC+%EB%B9%84%EB%8F%99%EA%B8%B0"
    )


def test_build_url_injects_operators():
    url = NaverEngine().build_url(
        "asyncio",
        10,
        SearchFilters(include_domains=["github.com"], exclude_domains=["spam.example"], category="pdf"),
    )
    assert "site%3Agithub.com" in url
    assert "-site%3Aspam.example" in url
    assert "filetype%3Apdf" in url


@pytest.mark.parametrize(
    "freshness, token",
    [("day", "p%3A1d"), ("week", "p%3A1w"), ("month", "p%3A1m"), ("year", "p%3A1y")],
)
def test_build_url_maps_freshness_to_nso(freshness, token):
    url = NaverEngine().build_url("q", 10, SearchFilters(freshness=freshness))
    assert url.endswith(f"&sm=tab_opt&nso={token}")


def test_build_url_without_freshness_has_no_nso():
    assert "nso=" not in NaverEngine().build_url("q", 10, SearchFilters(include_domains=["a.kr"]))


def test_description_is_one_short_line():
    d = NaverEngine.description
    assert d and "\n" not in d and len(d) <= 90


# ---------------------------------------------------------------------------
# Live network test
# ---------------------------------------------------------------------------


@skip_offline
async def test_live_naver_search():
    out = await NaverEngine().search("파이썬 asyncio 비동기", 10)
    if not out:
        pytest.skip("Naver returned no web documents (transient or walled)")
    assert all(r.url.startswith("http") and r.engine == "naver" for r in out)
    assert not any("새 창 열림" in r.title for r in out)
