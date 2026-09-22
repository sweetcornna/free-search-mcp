"""Extra engine tests covering:

  * #4  — empty parse() must NOT trigger a Playwright render for RSS engines
          (GoogleNews) but MUST still trigger it for HTML engines.
  * A1  — DuckDuckGo _unwrap single-decodes the uddg target (no double unquote).
  * A2  — safesearch/region settings are wired into each engine's build_url.
  * A3  — client-side freshness enforcement in
          apply_post_filters_with_diagnostics.
  * #18 — SearxEngine races a small batch of instances concurrently so dead
          instances don't cost K x per-instance timeout.

All offline; the network / browser boundaries are mocked.
"""
from __future__ import annotations

import asyncio
import time
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from search_mcp.config import settings
from search_mcp.engines import SearchFilters, SearchResult, get_engine
from search_mcp.engines.base import (
    apply_post_filters_with_diagnostics,
)

# ---------------------------------------------------------------------------
# #4 — browser-fallback gating on empty parse()
# ---------------------------------------------------------------------------


# A scrap of XML that GoogleNews can fetch but parse() turns into [] (no items).
_GN_EMPTY_RSS = """<?xml version="1.0"?><rss version="2.0"><channel></channel></rss>"""

# Garbage that the HTML engine parser turns into [] as well.
_HTML_EMPTY = "<html><body>nothing here</body></html>"


async def test_googlenews_empty_parse_does_not_render_browser(monkeypatch):
    """An RSS engine whose fetch yields empty/garbage XML must NOT fall back to
    a Playwright render — that fallback is pointless for an RSS feed."""
    e = get_engine("googlenews")
    assert e.supports_browser_fallback is False

    monkeypatch.setattr(settings, "fetch_strategy", "auto")
    with patch.object(e, "_fetch", AsyncMock(return_value=_GN_EMPTY_RSS)), patch(
        "search_mcp.engines.base.pool.fetch_html",
        new=AsyncMock(return_value=("u", "<html></html>")),
    ) as mock_fetch_html:
        out = await e.search("anything", 10)
    assert out == []
    mock_fetch_html.assert_not_called()


async def test_html_engine_empty_parse_still_renders_browser(monkeypatch):
    """An HTML engine returning [] from the HTTP path MUST still try the
    Playwright fallback (interstitial/captcha recovery)."""
    e = get_engine("duckduckgo")
    assert e.supports_browser_fallback is True

    monkeypatch.setattr(settings, "fetch_strategy", "auto")
    with patch.object(e, "_fetch", AsyncMock(return_value=_HTML_EMPTY)), patch(
        "search_mcp.engines.base.pool.fetch_html",
        new=AsyncMock(return_value=("u", _HTML_EMPTY)),
    ) as mock_fetch_html:
        await e.search("anything", 10)
    mock_fetch_html.assert_called_once()


# ---------------------------------------------------------------------------
# A1 — DuckDuckGo _unwrap single-decode
# ---------------------------------------------------------------------------


def test_unwrap_single_decodes_literal_percent_xx():
    from search_mcp.engines.duckduckgo import _unwrap

    # The uddg value is the percent-encoding of
    # 'https://en.wikipedia.org/wiki/C%2B%2B' (a URL that literally contains
    # %2B%2B). parse_qs decodes once -> the literal-%2B URL. A second unquote
    # would corrupt it into C++.
    wrapped = (
        "https://duckduckgo.com/l/?uddg="
        "https%3A%2F%2Fen.wikipedia.org%2Fwiki%2FC%252B%252B"
    )
    assert _unwrap(wrapped) == "https://en.wikipedia.org/wiki/C%2B%2B"


def test_unwrap_preserves_encoded_space():
    from search_mcp.engines.duckduckgo import _unwrap

    # %2520 -> after single decode -> %20 (literal). A double decode would turn
    # it into a space, corrupting the URL.
    wrapped = "https://duckduckgo.com/l/?uddg=https%3A%2F%2Fa.com%2Fb%2520c"
    assert _unwrap(wrapped) == "https://a.com/b%20c"


def test_unwrap_idempotent_on_plain_url():
    from search_mcp.engines.duckduckgo import _unwrap

    plain = "https://example.com/page"
    assert _unwrap(plain) == plain
    assert _unwrap(_unwrap(plain)) == plain


# ---------------------------------------------------------------------------
# A2 — safesearch / region wired into build_url
# ---------------------------------------------------------------------------


def test_ddg_build_url_emits_safesearch_and_region(monkeypatch):
    monkeypatch.setattr(settings, "safesearch", "strict")
    monkeypatch.setattr(settings, "region", "uk-en")
    e = get_engine("duckduckgo")
    url = e.build_url("hello", 10)
    assert "kp=1" in url       # strict
    assert "kl=uk-en" in url   # region


def test_ddg_build_url_safesearch_off(monkeypatch):
    monkeypatch.setattr(settings, "safesearch", "off")
    e = get_engine("duckduckgo")
    url = e.build_url("hello", 10)
    assert "kp=-2" in url


def test_bing_build_url_emits_safesearch_and_market(monkeypatch):
    monkeypatch.setattr(settings, "safesearch", "strict")
    monkeypatch.setattr(settings, "region", "uk-en")
    e = get_engine("bing")
    url = e.build_url("hello", 10)
    assert "adlt=strict" in url
    assert "mkt=en-GB" in url


@pytest.mark.parametrize("max_results", [1, 10, 25, 50])
def test_bing_build_url_never_sends_count(max_results):
    """`count=` looks like the obvious way to ask for N results. It is also,
    measured, what flips Bing from real results to a decoy page that matches
    only the first word of the query — with or without a warmed session.
    `form=QBRE` is the other half of the working shape."""
    from urllib.parse import parse_qs, urlparse

    url = get_engine("bing").build_url("rust ownership borrowing", max_results)
    params = parse_qs(urlparse(url).query)
    assert "count" not in params
    assert params["form"] == ["QBRE"]


def test_brave_build_url_emits_safesearch(monkeypatch):
    monkeypatch.setattr(settings, "safesearch", "moderate")
    e = get_engine("brave")
    url = e.build_url("hello", 10)
    assert "safesearch=moderate" in url


def test_mojeek_build_url_emits_safe(monkeypatch):
    monkeypatch.setattr(settings, "safesearch", "off")
    e = get_engine("mojeek")
    url = e.build_url("hello", 10)
    assert "safe=0" in url


def test_safesearch_region_urls_still_parse_and_keep_freshness(monkeypatch):
    """Adding the new params must not break existing freshness tokens nor URL
    well-formedness."""
    from urllib.parse import urlparse

    monkeypatch.setattr(settings, "safesearch", "strict")
    monkeypatch.setattr(settings, "region", "uk-en")
    for name, fresh_tok in (
        ("duckduckgo", "df=w"),
        ("bing", "ex1"),
        ("brave", "tf=pm"),
        ("mojeek", "since="),
    ):
        e = get_engine(name)
        url = e.build_url("hello", 10, SearchFilters(freshness="week" if name != "brave" else "month"))
        parsed = urlparse(url)
        assert parsed.scheme == "https" and parsed.netloc
        assert fresh_tok in url


# ---------------------------------------------------------------------------
# A3 — client-side freshness enforcement
# ---------------------------------------------------------------------------


def _r(url: str, published_age: str = "", confident: bool = False) -> SearchResult:
    return SearchResult(
        title="t", url=url, snippet="s", engine="x", rank=1,
        published_age=published_age, published_age_confident=confident,
    )


def test_freshness_drops_old_results_and_counts_them():
    results = [
        _r("https://a.com", published_age="2 years ago"),
        _r("https://b.com", published_age="3 hours ago"),
    ]
    kept, drops = apply_post_filters_with_diagnostics(
        results, SearchFilters(freshness="day")
    )
    assert [r.url for r in kept] == ["https://b.com"]
    assert drops == {"freshness": 1}


def test_freshness_keeps_unparseable_published_age():
    results = [
        _r("https://a.com", published_age=""),         # unknown -> keep
        _r("https://b.com", published_age="who knows"),  # unparseable -> keep
        _r("https://c.com", published_age="5 years ago"),  # old -> drop
    ]
    kept, drops = apply_post_filters_with_diagnostics(
        results, SearchFilters(freshness="day")
    )
    assert {r.url for r in kept} == {"https://a.com", "https://b.com"}
    assert drops == {"freshness": 1}


def test_freshness_confident_iso_date_outside_window_dropped():
    # A STRUCTURED (confident) absolute date — e.g. an RSS pubDate — outside the
    # window is dropped.
    results = [_r("https://a.com", published_age="2019-01-01", confident=True)]
    kept, drops = apply_post_filters_with_diagnostics(
        results, SearchFilters(freshness="month")
    )
    assert kept == []
    assert drops == {"freshness": 1}


def test_freshness_keeps_unconfident_absolute_date():
    # An absolute date scraped from snippet text (confident=False) is display-only
    # — a fresh page that merely mentions an old year must NOT be dropped.
    results = [_r("https://a.com", published_age="2019-01-01", confident=False)]
    kept, drops = apply_post_filters_with_diagnostics(
        results, SearchFilters(freshness="month")
    )
    assert [r.url for r in kept] == ["https://a.com"]
    assert drops == {}


def test_freshness_relative_phrase_dropped_even_without_confident_flag():
    # A relative "N ago" phrase is an explicit recency claim — trusted to drop
    # regardless of the confident flag.
    results = [_r("https://a.com", published_age="5 years ago", confident=False)]
    kept, drops = apply_post_filters_with_diagnostics(
        results, SearchFilters(freshness="month")
    )
    assert kept == []
    assert drops == {"freshness": 1}


def test_freshness_conservation_invariant_holds():
    """sum(drops.values()) == raw - kept, even mixing freshness with other
    filters."""
    results = [
        _r("https://github.com/a", published_age="10 years ago"),  # dropped: freshness or domain
        _r("https://example.com/b", published_age="1 hour ago"),   # dropped: include_domains
        _r("https://github.com/c", published_age="2 hours ago"),   # kept
        _r("https://github.com/d", published_age=""),              # kept (unparseable age)
    ]
    kept, drops = apply_post_filters_with_diagnostics(
        results,
        SearchFilters(freshness="day", include_domains=["github.com"]),
    )
    assert len(kept) + sum(drops.values()) == len(results)


def test_freshness_within_window_kept():
    results = [_r("https://a.com", published_age="2 days ago")]
    kept, drops = apply_post_filters_with_diagnostics(
        results, SearchFilters(freshness="week")
    )
    assert [r.url for r in kept] == ["https://a.com"]
    assert drops == {}


# ---------------------------------------------------------------------------
# #18 — Searx instance racing is concurrent, not strictly serial
# ---------------------------------------------------------------------------


async def test_searx_races_instances_concurrently(monkeypatch):
    """With several dead/slow instances, total latency must be bounded well
    below K x per-instance-timeout because the batch is raced concurrently."""
    from search_mcp.engines import searx as searx_mod

    e = get_engine("searx")

    # Five instances; each "request" sleeps for ~the per-instance timeout then
    # raises (dead). Serial execution would take ~5 x DELAY; concurrent racing
    # collapses it to ~DELAY per batch.
    DELAY = 0.30
    monkeypatch.setattr(
        searx_mod,
        "_INSTANCES",
        [f"https://dead{i}.test" for i in range(5)],
    )
    monkeypatch.setattr(searx_mod.random, "shuffle", lambda lst: None)

    def factory(*args, **kwargs):
        async def slow_get(url):
            await asyncio.sleep(DELAY)
            raise searx_mod.RequestException("dead")

        session = MagicMock()
        session.get = slow_get
        cm = MagicMock()
        cm.__aenter__ = AsyncMock(return_value=session)
        cm.__aexit__ = AsyncMock(return_value=None)
        return cm

    monkeypatch.setattr(searx_mod, "AsyncSession", factory)

    t0 = time.monotonic()
    out = await e.search("hello", max_results=10)
    elapsed = time.monotonic() - t0

    assert out == []  # all dead -> never raise, empty
    # Serial would be ~5*0.30 = 1.5s. Concurrent racing should be far less.
    assert elapsed < 5 * DELAY * 0.6, f"too slow ({elapsed:.2f}s) — not concurrent"


async def test_searx_returns_first_nonempty_when_racing(monkeypatch):
    """A fast instance returning results should win even when others are slow."""
    from search_mcp.engines import searx as searx_mod
    from tests.test_searx import _FAKE_SEARX_HTML  # reuse fixture HTML

    e = get_engine("searx")
    monkeypatch.setattr(
        searx_mod,
        "_INSTANCES",
        ["https://slow.test", "https://fast.test"],
    )
    monkeypatch.setattr(searx_mod.random, "shuffle", lambda lst: None)

    def factory(*args, **kwargs):
        async def get(u):
            if "slow" in u:
                await asyncio.sleep(0.5)
            resp = MagicMock()
            resp.status_code = 200
            resp.text = _FAKE_SEARX_HTML
            return resp

        session = MagicMock()
        # url is passed to .get(), not the session ctor, so we ignore factory args
        session.get = get
        cm = MagicMock()
        cm.__aenter__ = AsyncMock(return_value=session)
        cm.__aexit__ = AsyncMock(return_value=None)
        return cm

    monkeypatch.setattr(searx_mod, "AsyncSession", factory)

    out = await e.search("hello", max_results=10)
    assert len(out) == 3
    assert all(r.engine == "searx" for r in out)


# --- Bing ck/a redirect unwrapping ------------------------------------------
# Raw wrapper hrefs are unique per SERP impression, so leaving them in defeats
# the RRF merge's URL key and the title dedup: the same page found by Bing and
# by DuckDuckGo is emitted twice and the caller's max_results is spent on
# duplicates.


def _bing_wrapper(target: str) -> str:
    import base64

    payload = base64.urlsafe_b64encode(target.encode("utf-8")).decode("ascii").rstrip("=")
    return f"https://www.bing.com/ck/a?!&&p=deadbeef&u=a1{payload}&ntb=1"


def test_bing_resolve_url_unwraps_redirect():
    from search_mcp.engines.bing import resolve_bing_url

    target = "https://zod.dev/api?id=zstrictobject"
    assert resolve_bing_url(_bing_wrapper(target)) == target


def test_bing_resolve_url_passes_through_plain_and_undecodable():
    from search_mcp.engines.bing import resolve_bing_url

    plain = "https://example.com/page"
    assert resolve_bing_url(plain) == plain
    # `u` present but not a decodable http payload — keep the working link.
    junk = "https://www.bing.com/ck/a?u=a1!!!!notbase64"
    assert resolve_bing_url(junk) == junk


def test_bing_parse_emits_publisher_urls():
    from search_mcp.engines import get_engine

    target = "https://zod.dev/api?id=zstrictobject"
    html = (
        "<ol id='b_results'>"
        "<li class='b_algo'>"
        f"<h2><a href=\"{_bing_wrapper(target)}\">Defining schemas | Zod</a></h2>"
        "<div class='b_caption'><p>Strict object schemas.</p></div>"
        "</li></ol>"
    )

    results = get_engine("bing").parse(html)

    assert [r.url for r in results] == [target]
# ---------------------------------------------------------------------------
# Brave — a comma selector must not emit every result twice
# ---------------------------------------------------------------------------

_BRAVE_HTML = """
<div id="results">
  <div class="snippet" data-type="web">
    <a href="https://a.example/1"><span class="title">Alpha</span></a>
    <div class="snippet-description">Alpha body text.</div>
  </div>
  <div class="snippet" data-type="web">
    <a href="https://b.example/2"><span class="title">Beta</span></a>
    <div class="snippet-description">Beta body text.</div>
  </div>
</div>
"""


def test_brave_parse_emits_each_result_once():
    """selectolax concatenates a comma selector's match sets WITHOUT
    deduplicating, so every `.snippet[data-type="web"]` inside `#results`
    matched both halves of `'div.snippet[data-type="web"], #results .snippet'`
    and was emitted twice.

    Two consequences, both silent: half the `max_results` budget went to
    duplicates, and the survivors were scored twice by the aggregator's RRF
    merge, handing Brave roughly double the weight of every other engine. The
    DuckDuckGo parser documents the same defect for its own former comma
    selector.
    """
    out = get_engine("brave").parse(_BRAVE_HTML)
    assert [r.url for r in out] == ["https://a.example/1", "https://b.example/2"]


def test_brave_parse_dedups_a_repeated_url():
    """Belt-and-braces: the same URL twice in one SERP is one result."""
    doubled = _BRAVE_HTML.replace("https://b.example/2", "https://a.example/1")
    out = get_engine("brave").parse(doubled)
    assert [r.url for r in out] == ["https://a.example/1"]


# ---------------------------------------------------------------------------
# Chinese indexes — site:/-site:/filetype: are their ONLY filter channel
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("name", ["sogou", "so360"])
def test_cn_indexes_inject_query_operators(name):
    """`sogou` and `so360` were the only web engines that never called
    `augment_query_with_operators`.

    Neither has a URL parameter for domain or filetype constraints, so the
    engine returned unconstrained results and `apply_post_filters` then
    discarded them wholesale — `include_domains` came back empty while DDG
    returned ten, and the sparse-result hint blamed the filter rather than the
    engine.
    """
    url = get_engine(name).build_url(
        "asyncio",
        10,
        SearchFilters(
            include_domains=["python.org"],
            exclude_domains=["spam.example"],
            category="pdf",
        ),
    )
    assert "site%3Apython.org" in url
    assert "-site%3Aspam.example" in url
    assert "filetype%3Apdf" in url


def test_playwright_call_logs_are_cut_from_error_text():
    """A failed navigation reaches the model through `errors` and through a
    failed `fetch`. The first line says what happened; the call log repeats it."""
    from search_mcp.browser import brief_error

    raw = (
        "Page.goto: net::ERR_CONNECTION_CLOSED at https://www.zhihu.com/search?q=python\n"
        "Call log:\n"
        '  - navigating to "https://www.zhihu.com/search?q=python", waiting until "domcontentloaded"\n'
    )
    assert brief_error(RuntimeError(raw)) == (
        "Page.goto: net::ERR_CONNECTION_CLOSED at https://www.zhihu.com/search?q=python"
    )
    # Anything without a call log is returned as it is.
    assert brief_error(ValueError("query must not be empty")) == "query must not be empty"

