"""SeznamEngine tests.

Seznam's markup uses per-build hashed class names, so the engine reads the
JSON state the page embeds in ``<script id="renderer-state-data"
data-state="...">`` instead. The fixture below is that state trimmed from live
pages (2026-10-31): the organic hits of "python asyncio taskgroup exception
handling" plus the Sklik ad and related-queries block of "pojisteni auta",
next to the rendered markup of one hit with its sitelink sub-anchors. All
offline; one live test is gated on ``SEARCH_MCP_TEST_NETWORK=1``.
"""

from __future__ import annotations

import html
import json
import os

import pytest

from search_mcp.engines.base import SearchFilters
from search_mcp.engines.seznam import SeznamEngine

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.

NETWORK = os.environ.get("SEARCH_MCP_TEST_NETWORK") == "1"
skip_offline = pytest.mark.skipif(
    not NETWORK, reason="set SEARCH_MCP_TEST_NETWORK=1 to run"
)


def _organic(doc_id: str, snippet: dict, attributes: dict, **extra) -> dict:
    return {
        "id": {"service": "ftxt-organic", "type": "result", "key": doc_id, "callId": "a0a80386"},
        "data": {"docId": doc_id, "snippet": snippet, "attributes": attributes, **extra},
    }


_AD = {
    "id": {"service": "sklik", "type": "skliktop", "key": "15512221386094345614"},
    "data": {
        "adImpressionId": "15512221386094345614",
        "cleanClickthruUrl": "https://www.epojisteni.cz/povinne-ruceni/",
        "clickUrl": "https://s.seznam.cz/click?",
        "data": {
            "clickthruText": "e<b>pojisteni</b>.cz/<b>pojištění</b>/<b>auta</b>",
            "clickthruUrl": "https://www.epojisteni.cz/povinne-ruceni/?pid=77&utm_source=seznam",
            "headline1": "<b>Pojištění</b> <b>auta</b> 2026",
            "headline2": "Online sjednání do 5 minut",
            "description": "Srovnání nabídek povinného ručení. Zjistěte kolik ušetříte právě Vy!",
        },
        "sitelinks": [
            {"name": "Rychlý výpočet pojištění", "clickthruUrl": "https://www.epojisteni.cz/"}
        ],
    },
}

_RELATED = {
    "id": {"service": "related", "type": "relates", "callId": "9d4b7a03"},
    "data": {
        "query": "python asyncio taskgroup exception handling",
        "results": [{"source": "search_sellma_ftxt", "text": "python 3", "title": "python 3"}],
    },
}

_STATE = {
    "odra": {"page": "search", "imagesFilters": {"pornFilter": 1}},
    "data": {
        "query": "python asyncio taskgroup exception handling",
        "pornFilterLevel": "noporn",
        "count": 10,
        "entities": [
            {
                "id": {"service": "layout", "type": "desktop", "key": "layout-desktop"},
                "subEntities": [
                    {"id": {"service": "layout", "type": "top-row", "key": "top-row"}},
                    {
                        "id": {"service": "layout", "type": "legacy-left", "key": "legacy-left"},
                        "subEntities": [
                            _AD,
                            # A video hit: Seznam prints its date ("Před 5 měsíci").
                            _organic(
                                "4b1fbafedb0efdf4",
                                {
                                    "url": "https://www.geeksforgeeks.org/python/python-exception-handling/",
                                    "description": "<b class=sec>Exception</b> <b class=sec>Handling</b> allows a program to handle unexpected errors during execution in a controlled way&hellip;",
                                    "title": "<b class=sec>Python</b> <b class=sec>Exception</b> <b class=sec>Handling</b> - GeeksforGeeks",
                                },
                                {"language": "EN", "fileType": "HTML", "publicationDate": 1780055857, "categories": ["VIDEO", "ARTICLE", "NEWS"]},
                                video={"title": "Python Exception Handling", "publicationDate": 1703688982, "duration": 1386},
                            ),
                            # Sitelinks live in relatedDocuments, never as hits.
                            _organic(
                                "3a5bed411c587e09",
                                {
                                    "url": "https://realpython.com/python311-exception-groups/",
                                    "description": "<b class=sec>Python</b> 3.11 will be released in October 2022. In this tutorial, you'll install the latest alpha release of Python 3.11 in order to preview task and <b class=sec>exception</b> groups&hellip;",
                                    "siteName": "Real Python",
                                    "title": "<b class=sec>Python</b> 3.11 Preview: Task and <b class=sec>Exception</b> Groups – Real Python",
                                },
                                {"language": "EN", "fileType": "HTML", "publicationDate": 1650463200, "categories": ["ARTICLE", "NEWS"]},
                                relatedDocuments=[
                                    {"type": "documentSection", "url": "https://realpython.com/python311-exception-groups/#python-311-alpha", "title": "<b class=sec>Python</b> 3.11 Alpha"},
                                    {"type": "documentSection", "url": "https://realpython.com/python311-exception-groups/#other-new-features", "title": "Other New Features"},
                                ],
                            ),
                            _RELATED,
                            # A forum hit: Seznam prints "28. 2. 2024".
                            _organic(
                                "ab12de09dcd0b7b0",
                                {
                                    "url": "https://github.com/python/cpython/issues/116048",
                                    "description": "Bug report Bug description: As I understand it, <b class=sec>asyncio</b>.<b class=sec>TaskGroup</b> should never silently lose a task.",
                                    "title": "<b class=sec>asyncio</b>.<b class=sec>TaskGroup</b> may silently discard request to run a task&hellip;",
                                },
                                {"language": "EN", "fileType": "HTML", "publicationDate": 1709078400, "categories": ["FORUM"]},
                                forumPosting={"name": "asyncio.TaskGroup may silently discard request to run a task"},
                            ),
                            # publicationDate here is a first-crawl time and is not shown.
                            _organic(
                                "7e0498d1ecf313d3",
                                {
                                    "url": "https://docs.python.org/3/library/asyncio-task.html",
                                    "description": "This section outlines high-level <b class=sec>asyncio</b> APIs to work with coroutines and Tasks.",
                                    "title": "Coroutines and tasks — <b class=sec>Python</b> 3.15.1 documentation",
                                },
                                {"language": "EN", "fileType": "HTML", "publicationDate": 1398384000},
                            ),
                            # The same page again must not count twice.
                            _organic(
                                "3a5bed411c587e10",
                                {
                                    "url": "https://realpython.com/python311-exception-groups/",
                                    "description": "duplicate",
                                    "title": "Python 3.11 Preview",
                                },
                                {},
                            ),
                            {"id": {"service": "sklik", "type": "sklikbottom", "key": "2"}, "data": {"cleanClickthruUrl": "https://www.klik.cz/povinne-ruceni/"}},
                        ],
                    },
                    {"id": {"service": "ftxt-organic", "type": "paging"}, "data": {"totalResults": 1000}},
                ],
            },
            {"id": {"service": "ftxt-organic", "type": "is-porn", "callId": "7b57f7ec"}, "data": {"isPorn": False}},
        ],
    },
}

# The rendered markup around the state, trimmed: one hit with its sitelink
# sub-anchors and the related-queries block. None of it is parsed.
_MARKUP = """
<div data-e-b-z="main" class="Layout--left">
  <div class="f2c528"><div data-n-id="i18" data-e-b-n="iOjlYk~u"><div class="cfb4f6">
    <h3 class="Uac63d27e9f Ud05c5c9321"><a tabindex="0" class="Uc5ecc67d33" title="Přejít na: https://realpython.com/python311-exception-groups/" data-e-a="heading" href="https://realpython.com/python311-exception-groups/" data-l-id="3a5bed411c587e09"><span><b class="sec">Python</b> 3.11 Preview: Task and <b class="sec">Exception</b> Groups – Real Python</span></a></h3>
    <div data-n-id="i20" data-e-b-n="KaMRbi"><ul class="ac026f" data-e-a="tSvCXKaMRbij">
      <li class="f00d6a"><a tabindex="0" class="d6e05c" href="https://realpython.com/python311-exception-groups/#python-311-alpha" data-l-id="AXfkAr7y"><span><b class="sec">Python</b> 3.11 Alpha</span></a></li>
      <li class="f00d6a"><a tabindex="0" class="d6e05c" href="https://realpython.com/python311-exception-groups/#other-new-features" data-l-id="uDcqA8JX"><span>Other New Features</span></a></li>
    </ul></div>
  </div></div></div>
  <div class="f2c528"><div data-n-id="i43"><h4 class="c872c2">Související dotazy</h4>
    <a class="e66a1b" href="?q=python%203&amp;sId=tSvCXqcf29g&amp;sourceid=web&amp;thru=related" data-e-a="link"><span>python 3</span></a>
  </div></div>
</div>
"""


def _page(state: dict) -> str:
    blob = html.escape(json.dumps(state), quote=True)
    return (
        '<!DOCTYPE html><html lang="cs"><head><meta charSet="utf-8"/></head><body>'
        f"{_MARKUP}"
        f'<script id="renderer-state-data" data-state="{blob}"></script>'
        "</body></html>"
    )


_HTML = _page(_STATE)


# ---------------------------------------------------------------------------
# parse
# ---------------------------------------------------------------------------


def test_parse_yields_one_result_per_organic_block():
    out = SeznamEngine().parse(_HTML)
    assert [r.url for r in out] == [
        "https://www.geeksforgeeks.org/python/python-exception-handling/",
        "https://realpython.com/python311-exception-groups/",
        "https://github.com/python/cpython/issues/116048",
        "https://docs.python.org/3/library/asyncio-task.html",
    ]
    assert all(r.engine == "seznam" and r.rank == 0 for r in out)


def test_parse_strips_markup_without_splitting_dotted_names():
    out = {r.url: r for r in SeznamEngine().parse(_HTML)}
    gh = out["https://github.com/python/cpython/issues/116048"]
    assert gh.title == "asyncio.TaskGroup may silently discard request to run a task…"
    assert gh.snippet.startswith("Bug report Bug description: As I understand it, asyncio.TaskGroup")
    rp = out["https://realpython.com/python311-exception-groups/"]
    assert rp.title == "Python 3.11 Preview: Task and Exception Groups – Real Python"
    assert "<b" not in rp.snippet and "&hellip;" not in rp.snippet


def test_sitelinks_ads_and_widgets_are_not_results():
    urls = [r.url for r in SeznamEngine().parse(_HTML)]
    assert not any("#" in u for u in urls)
    assert "https://www.epojisteni.cz/povinne-ruceni/" not in urls
    assert "https://www.klik.cz/povinne-ruceni/" not in urls
    assert not any("seznam.cz" in u for u in urls)


def test_date_only_where_seznam_prints_one():
    out = {r.url: r for r in SeznamEngine().parse(_HTML)}
    assert out["https://www.geeksforgeeks.org/python/python-exception-handling/"].published_age == "2026-05-29"
    assert out["https://github.com/python/cpython/issues/116048"].published_age == "2024-02-28"
    # First-crawl time on an evergreen page must not read as a 2014 publish date.
    assert out["https://docs.python.org/3/library/asyncio-task.html"].published_age == ""
    assert out["https://realpython.com/python311-exception-groups/"].published_age == ""
    assert not any(r.published_age_confident for r in out.values())


def test_hits_without_http_url_or_title_are_skipped():
    state = json.loads(json.dumps(_STATE))
    left = state["data"]["entities"][0]["subEntities"][1]["subEntities"]
    left.append(_organic("x1", {"url": "javascript:void(0)", "title": "Bad"}, {}))
    left.append(_organic("x2", {"url": "https://example.com/no-title", "title": ""}, {}))
    left.append(_organic("x3", {"title": "No URL"}, {}))
    urls = [r.url for r in SeznamEngine().parse(_page(state))]
    assert len(urls) == 4
    assert "https://example.com/no-title" not in urls


@pytest.mark.parametrize(
    "page",
    [
        "",
        "<html><body>nothing here</body></html>",
        _MARKUP,  # the markup alone, without the state blob
        '<script id="renderer-state-data" data-state="{not json"></script>',
        '<script id="renderer-state-data" data-state="[1, 2]"></script>',
        '<script id="renderer-state-data" data-state="{&quot;data&quot;: null}"></script>',
    ],
)
def test_unparseable_pages_yield_empty(page):
    assert SeznamEngine().parse(page) == []


# ---------------------------------------------------------------------------
# build_url
# ---------------------------------------------------------------------------


def test_build_url_plain_query():
    url = SeznamEngine().build_url("python asyncio", 10)
    assert url == "https://search.seznam.cz/?q=python+asyncio"


def test_build_url_injects_operators_seznam_honours():
    url = SeznamEngine().build_url(
        "asyncio",
        10,
        SearchFilters(include_domains=["github.com"], exclude_domains=["spam.example"], category="pdf"),
    )
    assert "site%3Agithub.com" in url
    assert "-site%3Aspam.example" in url
    assert "filetype%3Apdf" in url


def test_build_url_asks_for_twenty_only_when_needed():
    assert "count=" not in SeznamEngine().build_url("q", 10)
    assert SeznamEngine().build_url("q", 11).endswith("&count=20")
    assert SeznamEngine().build_url("q", 50).endswith("&count=20")


def test_build_url_has_no_freshness_parameter():
    """Seznam has no time filter; freshness is left to the post-filter."""
    e = SeznamEngine()
    assert e.build_url("q", 10, SearchFilters(freshness="week")) == e.build_url("q", 10)


def test_description_is_one_short_line():
    d = SeznamEngine.description
    assert d and "\n" not in d and len(d) <= 90


# ---------------------------------------------------------------------------
# Live network test
# ---------------------------------------------------------------------------


@skip_offline
async def test_live_seznam_search():
    out = await SeznamEngine().search("python asyncio taskgroup", 10)
    if not out:
        pytest.skip("Seznam returned no results (transient or walled)")
    assert all(r.url.startswith("http") and r.engine == "seznam" for r in out)
    assert not any("#" in r.url and r.url.split("#")[0] in {x.url for x in out} for r in out)
