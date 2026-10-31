"""YahooEngine tests (offline, plus one live test).

The fixture is trimmed from live search.yahoo.com pages saved on 2026-10-31
("python asyncio taskgroup exception handling", and "人工智能 最新进展" for
the ad block): the real row structure with the favicons, `data-yga` blobs and
tracking tokens cut down. The wikipedia row is the one invented target — the
C++ article, whose URL contains a literal `%2B` — wrapped exactly the way
Yahoo wraps every other target.

The cookie-jar tests stub the two network seams: `CurlAsyncSession` (the
handshake, which reads back the cookies it ends with) and `Engine._http_get`
(every search made with a jar). The live test runs with
``SEARCH_MCP_TEST_NETWORK=1``.
"""

from __future__ import annotations

import asyncio
import os
from types import SimpleNamespace
from urllib.parse import parse_qs, urlparse

import pytest
from curl_cffi.requests.exceptions import HTTPError, RequestException, Timeout

from search_mcp.engines import base as base_mod
from search_mcp.engines import yahoo as yahoo_mod
from search_mcp.engines.base import SearchFilters
from search_mcp.engines.yahoo import YahooEngine, resolve_yahoo_url

NETWORK = os.environ.get("SEARCH_MCP_TEST_NETWORK") == "1"
skip_offline = pytest.mark.skipif(not NETWORK, reason="set SEARCH_MCP_TEST_NETWORK=1 to run")


@pytest.fixture(autouse=True)
def _reset_yahoo_jar():
    """The jar is shared per egress for half an hour; one test's stubbed
    cookies must not answer for the next."""
    yahoo_mod._reset_jar()
    yield
    yahoo_mod._reset_jar()


def _wrap(target_encoded: str, token: str = "A2RRAQgvZuVqxwIAOXlXNyoA") -> str:
    return (
        f"https://r.search.yahoo.com/_ylt={token};_ylu=Y29sbwN1cy1lYXN0LTEEcG9zAzEEdnRpZAMEc2VjA3Ny"
        f"/RV=2/RE=1794628399/RO=10/RU={target_encoded}/RK=2/RS=TgXmVHRHuwoXIIPaa1P2PwBK3Qg-"
    )


def _row(href: str, site: str, crumb: str, title_html: str, snippet_html: str, *, cls: str = "dd algo algo-sr relsrch Sr") -> str:
    return f"""
<li><div class="{cls}" data-yga='{{"yModuleName":"Sr","yModType":"algo"}}'>
  <div class="compTitle options-toggle">
    <a class="d-ib va-top mt-38 mb-4 mxw-100p" data-matarget="algo" target="_blank" referrerpolicy="origin" href="{href}">
      <div class="d-ib p-abs t-0 l-0 fz-13 lh-16 fc-dustygray wr-bw ls-n fw-m">
        <div class="thmb algo-favicon"><img class="s-img" width="16" height="16" alt="" src="https://s.yimg.com/pv/static/favicon.png"/></div>
        <span style="letter-spacing: 0.195px;" class="d-ib va-mid"><span class="fc-141414 d-b">{site}</span>{crumb}</span>
      </div>
      <h3 style="display:block" class="title fc-2015C2-imp pt-6 ivmt-6 mxw-100p"><span class="d-b fz-20 lh-24 tc ls-024 fw-500">{title_html}</span></h3>
    </a>
  </div>
  <div class="compText aAbs"><p class="fc-dustygray fz-14 lh-22 ls-02 mah-44 ov-h d-box fbox-ov fbox-lc2">{snippet_html}</p></div>
</div></li>"""


GFG = _wrap("https%3a%2f%2fwww.geeksforgeeks.org%2fpython%2fpython-taskgroups-with-asyncio%2f")
DOCS = _wrap("https%3a%2f%2fdocs.python.org%2f3%2flibrary%2fasyncio-task.html", token="A2RRAQgvZuVqxwIAP3lXNyoA")
# Target https://en.wikipedia.org/wiki/C%2B%2B, percent-encoded once more by Yahoo.
CPP = _wrap("https%3a%2f%2fen.wikipedia.org%2fwiki%2fC%252B%252B", token="A2RRAQgvZuVqxwIAQnlXNyoA")
AD = (
    "https://r.search.yahoo.com/rdclks/dWU9NTQ4MWZudGxlYXBvMSZ1dD0xNzkzNDE5MDA5MTU0/RV=2/RE=1796011009/RO=14"
    "/RU=https%3a%2f%2fwww.bing.com%2faclick%3fld%3de8rIfY2Yh2IMP0HDGf%26u%3daHR0cHMlM2ElMmYlMmZ3d3cuaXNsYW5kLmlv"
    "/RK=2/RS=hsTtS668IcUH8qiR4HyHe8atgVU-;_ylt=A2RRDnQBZ.VqoQIAaUZXNyoA?IG=64510e74030b4008a16ae5670111f19c"
)
PRIVACY = (
    "https://r.search.yahoo.com/_ylt=A2RRAQgvZuVqxwIAUXlXNyoA/RV=2/RE=1794628399/RO=10"
    "/RU=https%3a%2f%2fguce.yahoo.com%2fstate-controls%3flocale%3den-US%26state%3dCA/RK=2/RS=yP5tQTB6xKuQpZ3ohAlA7RnUyzY-"
)

_PAGE = f"""<!doctype html><html id="atomic" lang="en-US"><head><title>python asyncio taskgroup exception handling - Yahoo Search Results</title></head><body>
<ol class="reg searchLeftTop"><li><div class="dd horizontal-timefilter fl-l timefilter">
  <a href="https://search.yahoo.com/search;_ylt=A2RRAQgvZuVqxwIAM3lXNyoA?ei=UTF-8&amp;p=python+asyncio&amp;fr2=time&amp;btf=d">Past day</a>
</div></li></ol>
<ol class="reg scta searchCenterTopAds">
  <li class="first"><div class="dd mb-16 AdHdrTop"><div class="compTitle"><h2 class="title mb-0"><span class="fz-13 lh-m">Sponsored results</span></h2></div></div></li>
  <li class="last"><div class="dd fst lst ads bcan1 relsrch p-rel AdTop"><div class="layoutMiddle">
    <div class="compTitle mb-2"><a class="d-b" data-matarget="ad" target="_blank" referrerpolicy="unsafe-url" href="{AD}">
      <span class="ad-domain d-ib va-mid">www.island.io</span><h3 class="title"><span>Island Zero Trust Browser</span></h3></a></div>
    <div class="compText"><p>Put AI in the browser. Schedule a demo.</p></div>
  </div></div></li>
</ol>
<ol class="reg  searchCenterMiddle">
{_row(GFG, "GeeksForGeeks", "https://www.geeksforgeeks.org › python › python-taskgroups",
      "Python Taskgroups with asyncIO - GeeksforGeeks",
      '<span class="fc-smoke">Jul 23, 2025 · </span>  Prerequisite <b>Exception</b> <b>Handling</b> <b>Asyncio</b> '
      '<b>Exception</b> Groups in <b>Python</b>', cls="dd fst algo algo-sr relsrch Sr")}
{_row(DOCS, "Python", "https://docs.python.org › 3 › library",
      "Coroutines and tasks — Python 3.15.2 documentation",
      '<span class="fc-smoke">1 day ago · </span>  Note <b>asyncio</b>.<b>TaskGroup</b>.create_task () is a new '
      "alternative leveraging structural concurrency")}
{_row(CPP, "Wikipedia", "https://en.wikipedia.org › wiki › C++",
      "C++ - Wikipedia", "C++ is a high-level, general-purpose programming language.", cls="dd algo algo-sr relsrch richAlgo")}
{_row(GFG.replace("OXlXNyoA", "QXlXNyoA"), "GeeksForGeeks", "https://www.geeksforgeeks.org",
      "Python Taskgroups with asyncIO (again)", "The same page under a different tracking token.", cls="dd lst algo algo-sr relsrch Sr")}
</ol>
<ol class="reg scba searchCenterBottomAds">
  <li class="last"><div class="dd fst lst ads bcas1 p-rel AdBttm"><div class="compTitle"><a href="{AD}"><h3 class="title">Island Zero Trust Browser</h3></a></div></div></li>
</ol>
<ol class="reg searchBottom"><li><div class="dd fst lst pagination Pgntn">
  <a href="https://search.yahoo.com/search;_ylt=A2RRAQgvZuVqxwIASHlXNyoA?p=python+asyncio&amp;b=8">2</a>
</div></li></ol>
<div id="ft"><ul><li class="sdk"><a referrerpolicy="unsafe-url" href="{PRIVACY}">Your Privacy Choices</a></li></ul></div>
</body></html>"""


# ---------------------------------------------------------------------------
# parse
# ---------------------------------------------------------------------------


def test_parse_unwraps_redirects_to_publisher_urls():
    results = YahooEngine().parse(_PAGE)
    assert [r.url for r in results] == [
        "https://www.geeksforgeeks.org/python/python-taskgroups-with-asyncio/",
        "https://docs.python.org/3/library/asyncio-task.html",
        "https://en.wikipedia.org/wiki/C%2B%2B",
    ]
    assert all(r.engine == "yahoo" and r.rank == 0 for r in results)


def test_parse_takes_title_from_h3_and_snippet_with_its_date():
    gfg, docs, _ = YahooEngine().parse(_PAGE)
    # Not the anchor text, which also carries the site name and breadcrumb.
    assert gfg.title == "Python Taskgroups with asyncIO - GeeksforGeeks"
    assert gfg.snippet.startswith("Jul 23, 2025 · Prerequisite Exception Handling Asyncio")
    assert gfg.published_age == "2025-07-23"
    assert gfg.published_age_confident is False
    # Highlights keep the page's own spacing.
    assert "asyncio.TaskGroup.create_task ()" in docs.snippet
    assert docs.published_age == "1 day ago"


def test_parse_skips_ads_and_yahoo_links():
    urls = [r.url for r in YahooEngine().parse(_PAGE)]
    assert not any("bing.com" in u or "rdclks" in u or "island.io" in u for u in urls)
    assert not any("yahoo.com" in u for u in urls)


def test_parse_dedups_the_same_target_under_different_tokens():
    urls = [r.url for r in YahooEngine().parse(_PAGE)]
    assert len(urls) == len(set(urls)) == 3


@pytest.mark.parametrize(
    "html",
    [
        "",  # what the `_bv` handshake's HTTP 500 carries
        "<html><body>Something went wrong</body></html>",
        "<<<not html at all>>>",
        '<div class="dd algo"><div class="compTitle"></div></div>',
    ],
)
def test_unparseable_page_yields_nothing(html):
    assert YahooEngine().parse(html) == []


# ---------------------------------------------------------------------------
# resolve_yahoo_url
# ---------------------------------------------------------------------------


def test_resolve_decodes_ru_exactly_once():
    # A second decode would turn the real "C++" URL into .../C++ and 404.
    assert resolve_yahoo_url(CPP) == "https://en.wikipedia.org/wiki/C%2B%2B"


def test_resolve_keeps_the_target_query_string():
    assert resolve_yahoo_url(PRIVACY) == "https://guce.yahoo.com/state-controls?locale=en-US&state=CA"


def test_resolve_passes_direct_urls_through():
    assert resolve_yahoo_url("https://example.com/x?a=1") == "https://example.com/x?a=1"


@pytest.mark.parametrize(
    "href",
    [
        "https://r.search.yahoo.com/_ylt=abc/RV=2/RE=1/RO=10/RK=2/RS=x-",  # no RU=
        "https://r.search.yahoo.com/_ylt=abc/RV=2/RU=javascript%3aalert(1)/RK=2",  # not http(s)
        "",
    ],
)
def test_resolve_rejects_unusable_redirects(href):
    assert resolve_yahoo_url(href) == ""


@pytest.mark.parametrize(
    ("url", "dropped"),
    [
        ("https://guce.yahoo.com/privacy-policy?locale=en-US", True),
        ("https://images.search.yahoo.com/search/images?p=x", True),
        ("https://scout.yahoo.com/chat?q=x", True),
        ("https://www.bing.com/aclick?ld=e8x&u=abc", True),
        # Organic Yahoo content is a real result.
        ("https://finance.yahoo.com/personal-finance/credit-cards/article/best", False),
        ("https://www.bing.com/search?q=x", False),
    ],
)
def test_internal_and_ad_hosts(url, dropped):
    assert yahoo_mod._is_dropped(url) is dropped


# ---------------------------------------------------------------------------
# build_url
# ---------------------------------------------------------------------------


def _params(url: str) -> dict[str, list[str]]:
    return parse_qs(urlparse(url).query)


def test_build_url_plain_query():
    url = YahooEngine().build_url("人工智能 最新进展", 10)
    assert url.startswith("https://search.yahoo.com/search?")
    params = _params(url)
    assert params["p"] == ["人工智能 最新进展"]
    assert params["ei"] == ["UTF-8"]
    # Ignored by Yahoo (measured), so never sent.
    assert not {"btf", "age", "n", "vm"} & params.keys()


def test_build_url_carries_site_and_filetype_operators():
    filters = SearchFilters(
        include_domains=["docs.python.org"], exclude_domains=["geeksforgeeks.org"], category="pdf"
    )
    q = _params(YahooEngine().build_url("asyncio taskgroup", 10, filters))["p"][0]
    assert q == "asyncio taskgroup site:docs.python.org -site:geeksforgeeks.org filetype:pdf"


@pytest.mark.parametrize(("freshness", "btf"), [("day", "d"), ("week", "w"), ("month", "m")])
def test_build_url_freshness_uses_btf(freshness, btf):
    url = YahooEngine().build_url("python release", 10, SearchFilters(freshness=freshness))
    assert _params(url)["btf"] == [btf]


def test_build_url_year_has_no_native_filter():
    # `btf=y` is silently ignored, so year is left to the post-filter.
    url = YahooEngine().build_url("python release", 10, SearchFilters(freshness="year"))
    assert "btf" not in _params(url)


def test_description_is_one_short_line():
    d = YahooEngine.description
    assert d and "\n" not in d and len(d) <= 90


# ---------------------------------------------------------------------------
# the `_bv` cookie jar
# ---------------------------------------------------------------------------


def _http_error(status: int) -> HTTPError:
    """What `raise_for_status` raises: an HTTPError carrying its response."""
    return HTTPError(f"HTTP Error {status}", response=SimpleNamespace(status_code=status))


class _Wire:
    """Scripts the handshake session and the jar-carrying search requests."""

    def __init__(self, monkeypatch, *, handshakes=(), searches=()):
        self.handshake_answers = list(handshakes)
        self.search_answers = list(searches)
        self.handshake_urls: list[str] = []
        self.search_cookies: list[dict | None] = []
        wire = self

        class FakeCurlSession:
            def __init__(self, **kwargs):
                self.cookies = SimpleNamespace(jar=[])

            async def __aenter__(self):
                return self

            async def __aexit__(self, *exc):
                return False

            async def get(self, url):
                wire.handshake_urls.append(url)
                # A real handshake takes a while; let concurrent searches in.
                await asyncio.sleep(0.01)
                status, body = wire.handshake_answers.pop(0)
                # Every hop sets YBV; a completed handshake also the A3 jar.
                self.cookies.jar = [SimpleNamespace(name="YBV", value=f"v0.{len(wire.handshake_urls)}")]
                if status == 200:
                    self.cookies.jar.append(SimpleNamespace(name="A3", value="d=AQAB"))
                return SimpleNamespace(status_code=status, text=body)

        async def http_get(engine, url, *, cookies=None, retry=True):
            # A replayed jar gets exactly one request: a rejection is answered
            # by the handshake, not by the generic retry.
            assert retry is False
            wire.search_cookies.append(cookies)
            answer = wire.search_answers.pop(0)
            if isinstance(answer, Exception):
                raise answer
            return answer

        monkeypatch.setattr(yahoo_mod, "CurlAsyncSession", FakeCurlSession)
        monkeypatch.setattr(base_mod.Engine, "_http_get", http_get)
        monkeypatch.setattr(yahoo_mod.settings, "fetch_strategy", "http")


async def test_handshake_mints_a_jar_that_later_searches_reuse(monkeypatch):
    wire = _Wire(monkeypatch, handshakes=[(200, _PAGE)], searches=[_PAGE])
    engine = YahooEngine()

    first = await engine.search("python asyncio taskgroup", 10)
    second = await engine.search("rust tokio runtime", 10)

    assert len(first) == 3 and len(second) == 3
    assert len(wire.handshake_urls) == 1
    assert wire.search_cookies == [{"YBV": "v0.1", "A3": "d=AQAB"}]


async def test_handshake_keeps_trying_in_the_same_session(monkeypatch):
    # Measured: after two refusals the third attempt usually gets through.
    wire = _Wire(monkeypatch, handshakes=[(500, ""), (500, ""), (200, _PAGE)])
    results = await YahooEngine().search("python asyncio taskgroup", 10)
    assert len(results) == 3
    assert len(wire.handshake_urls) == yahoo_mod._HANDSHAKE_ATTEMPTS == 3
    # The jar is what the session ended with: the last attempt's YBV.
    assert yahoo_mod._JARS[""].cookies == {"YBV": "v0.3", "A3": "d=AQAB"}


async def test_a_failed_handshake_raises_and_leaves_no_jar(monkeypatch):
    wire = _Wire(monkeypatch, handshakes=[(500, "")] * 3)
    # A RequestException (so the browser fallback still applies under
    # "auto"), worded as what it is rather than as a server fault.
    with pytest.raises(RequestException, match="bot check"):
        await YahooEngine().search("python asyncio taskgroup", 10)
    assert yahoo_mod._JARS == {}
    assert len(wire.handshake_urls) == 3


async def test_a_rejected_jar_is_replaced_within_the_same_search(monkeypatch):
    wire = _Wire(
        monkeypatch,
        handshakes=[(200, _PAGE), (200, _PAGE)],
        searches=[_http_error(500)],
    )
    engine = YahooEngine()
    await engine.search("first query", 10)  # mints
    stale = yahoo_mod._JARS[""]

    results = await engine.search("second query", 10)

    assert len(results) == 3
    assert len(wire.handshake_urls) == 2
    assert yahoo_mod._JARS[""] is not stale


async def test_a_rejected_jar_gets_fewer_handshake_attempts(monkeypatch):
    # The rejected request has already spent its time; the re-mint is capped.
    wire = _Wire(
        monkeypatch,
        handshakes=[(200, _PAGE)] + [(500, "")] * 3,
        searches=[_http_error(500)],
    )
    engine = YahooEngine()
    await engine.search("first query", 10)
    with pytest.raises(RequestException, match="bot check"):
        await engine.search("second query", 10)
    assert len(wire.handshake_urls) == 1 + yahoo_mod._REMINT_ATTEMPTS
    assert yahoo_mod._JARS == {}


async def test_a_timeout_with_a_jar_is_not_retried_as_a_handshake(monkeypatch):
    wire = _Wire(monkeypatch, handshakes=[(200, _PAGE)], searches=[Timeout("timed out")])
    engine = YahooEngine()
    await engine.search("first query", 10)
    jar = yahoo_mod._JARS[""]

    with pytest.raises(Timeout):
        await engine.search("second query", 10)
    assert len(wire.handshake_urls) == 1
    assert yahoo_mod._JARS[""] is jar


# ---------------------------------------------------------------------------
# live
# ---------------------------------------------------------------------------


@skip_offline
async def test_live_search_returns_unwrapped_urls(monkeypatch):
    monkeypatch.setattr(yahoo_mod.settings, "fetch_strategy", "http")
    results = await YahooEngine().search("python asyncio taskgroup", 10, None, diagnostics={})
    assert results, "Yahoo returned nothing (markup change, or the _bv handshake failed?)"
    for r in results:
        assert r.url.startswith(("http://", "https://"))
        assert "r.search.yahoo.com" not in r.url
        assert r.title


@pytest.mark.parametrize("status", [403, 429])
async def test_a_refusal_ends_the_handshake_at_once(monkeypatch, status):
    wire = _Wire(monkeypatch, handshakes=[(status, ""), (200, _PAGE), (200, _PAGE)])
    with pytest.raises(RequestException, match=f"HTTP {status}"):
        await YahooEngine().search("python asyncio taskgroup", 10)
    assert len(wire.handshake_urls) == 1


@pytest.mark.parametrize("failure", [_http_error(403), _http_error(429), RequestException("reset")])
async def test_a_refusal_or_reset_keeps_the_jar_and_sends_nothing_more(monkeypatch, failure):
    wire = _Wire(monkeypatch, handshakes=[(200, _PAGE)], searches=[failure])
    engine = YahooEngine()
    await engine.search("first query", 10)
    jar = yahoo_mod._JARS[""]
    with pytest.raises(RequestException):
        await engine.search("second query", 10)
    assert len(wire.handshake_urls) == 1
    assert yahoo_mod._JARS[""] is jar


async def test_concurrent_searches_share_one_handshake(monkeypatch):
    wire = _Wire(monkeypatch, handshakes=[(200, _PAGE)], searches=[_PAGE, _PAGE, _PAGE])
    engine = YahooEngine()
    results = await asyncio.gather(*(engine.search(f"query {i}", 10) for i in range(4)))
    assert all(len(r) == 3 for r in results)
    assert len(wire.handshake_urls) == 1


async def test_waiters_on_a_failed_handshake_send_nothing_of_their_own(monkeypatch):
    wire = _Wire(monkeypatch, handshakes=[(500, "")] * 3)
    engine = YahooEngine()
    outcomes = await asyncio.gather(
        *(engine.search(f"query {i}", 10) for i in range(3)), return_exceptions=True
    )
    assert all(isinstance(o, RequestException) for o in outcomes)
    assert len(wire.handshake_urls) == yahoo_mod._HANDSHAKE_ATTEMPTS


async def test_a_cancelled_handshake_hands_its_waiters_their_own(monkeypatch):
    wire = _Wire(monkeypatch, handshakes=[(200, _PAGE), (200, _PAGE)])
    engine = YahooEngine()
    owner = asyncio.ensure_future(engine.search("first", 10))
    await asyncio.sleep(0)  # the owner is inside its handshake
    waiter = asyncio.ensure_future(engine.search("second", 10))
    await asyncio.sleep(0)
    owner.cancel()
    results = await waiter
    assert len(results) == 3
    assert len(wire.handshake_urls) == 2


async def test_an_expired_jar_is_never_replayed(monkeypatch):
    wire = _Wire(monkeypatch, handshakes=[(200, _PAGE), (200, _PAGE)])
    engine = YahooEngine()
    await engine.search("first", 10)
    yahoo_mod._JARS[""].minted_at -= yahoo_mod._JAR_TTL + 1
    await engine.search("second", 10)
    assert len(wire.handshake_urls) == 2
    assert wire.search_cookies == []
