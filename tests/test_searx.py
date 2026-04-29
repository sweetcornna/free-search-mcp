"""SearxEngine tests.

The engine fans out across a shortlist of public SearXNG instances, so
unit tests mock the per-instance HTTP path rather than hit the network.
A small live suite is gated on ``SEARCH_MCP_TEST_NETWORK=1``.
"""
from __future__ import annotations

import os
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from search_mcp.engines import SearchFilters, get_engine
from search_mcp.engines.searx import _INSTANCES, SearxEngine

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.

NETWORK = os.environ.get("SEARCH_MCP_TEST_NETWORK") == "1"
skip_offline = pytest.mark.skipif(
    not NETWORK, reason="set SEARCH_MCP_TEST_NETWORK=1 to run"
)


# ---------------------------------------------------------------------------
# Wiring
# ---------------------------------------------------------------------------


def test_engine_registered_under_searx_name():
    e = get_engine("searx")
    assert isinstance(e, SearxEngine)
    assert e.name == "searx"
    assert e.needs_browser is False


def test_instance_shortlist_is_https():
    assert _INSTANCES, "no SearXNG instances configured"
    for inst in _INSTANCES:
        assert inst.startswith("https://"), inst
        # No trailing slash — _instance_url does the join itself.
        assert not inst.endswith("/"), inst


# ---------------------------------------------------------------------------
# build_url / _instance_url
# ---------------------------------------------------------------------------


def test_build_url_returns_first_instance_search_endpoint():
    e = SearxEngine()
    url = e.build_url("hello world", 10)
    assert url.startswith(_INSTANCES[0])
    assert "/search?" in url
    assert "q=hello+world" in url


def test_instance_url_embeds_filters_as_operators():
    e = SearxEngine()
    f = SearchFilters(
        include_domains=["example.com"],
        exclude_domains=["spam.com"],
        category="pdf",
    )
    url = e._instance_url("https://example-instance.test", "hello", f)
    # site:/-site:/filetype: are URL-encoded into the q param
    assert "site%3Aexample.com" in url
    assert "-site%3Aspam.com" in url
    assert "filetype%3Apdf" in url


def test_instance_url_freshness_uses_time_range():
    e = SearxEngine()
    url = e._instance_url(
        "https://example-instance.test", "hello", SearchFilters(freshness="week")
    )
    assert "time_range=week" in url


# ---------------------------------------------------------------------------
# parse() — the exact HTML SearXNG renders for a result block
# ---------------------------------------------------------------------------


_FAKE_SEARX_HTML = """
<html><body>
<article class="result result-default category-general">
  <a href="https://example.com/a" class="url_header">x</a>
  <h3><a href="https://example.com/a">Example A title</a></h3>
  <p class="content">First snippet body — written 2 days ago.</p>
</article>
<article class="result result-default category-general">
  <a href="https://example.com/b" class="url_header">x</a>
  <h3><a href="https://example.com/b">Example B title</a></h3>
  <p class="content">Second snippet body.</p>
</article>
<article class="result result-ad">
  <h3><a href="https://ads.example.com/c">Ad row</a></h3>
  <p class="content">Sponsored.</p>
</article>
<article class="result result-default category-general">
  <a href="https://example.com/d" class="url_header">x</a>
  <h3><a href="https://example.com/d">Example D title</a></h3>
  <p class="content">Third snippet body.</p>
</article>
</body></html>
"""


def test_parse_extracts_title_url_snippet_and_skips_ads():
    e = SearxEngine()
    out = e.parse(_FAKE_SEARX_HTML)
    urls = [r.url for r in out]
    assert urls == [
        "https://example.com/a",
        "https://example.com/b",
        "https://example.com/d",
    ]
    assert out[0].title == "Example A title"
    assert out[0].snippet.startswith("First snippet body")
    # Date hint extracted from the snippet
    assert out[0].published_age == "2 days ago"


def test_parse_returns_empty_list_on_empty_html():
    assert SearxEngine().parse("<html></html>") == []


# ---------------------------------------------------------------------------
# search() — fallback across instances
# ---------------------------------------------------------------------------


def _mock_session_returning(status: int, body: str):
    """Build an AsyncSession-shaped context manager that returns one response."""
    response = MagicMock()
    response.status_code = status
    response.text = body

    session = MagicMock()
    session.get = AsyncMock(return_value=response)
    cm = MagicMock()
    cm.__aenter__ = AsyncMock(return_value=session)
    cm.__aexit__ = AsyncMock(return_value=None)
    return cm


async def test_search_returns_first_nonempty_instance(monkeypatch):
    e = SearxEngine()

    # First instance: empty body. Second: real results. Third: never called.
    sessions = iter([
        _mock_session_returning(200, "<html></html>"),
        _mock_session_returning(200, _FAKE_SEARX_HTML),
        _mock_session_returning(200, _FAKE_SEARX_HTML),
    ])

    def factory(*args, **kwargs):
        return next(sessions)

    # Make the instance order deterministic so we can assert "tried in order".
    monkeypatch.setattr(
        "search_mcp.engines.searx.random.shuffle", lambda lst: None
    )
    monkeypatch.setattr("search_mcp.engines.searx.AsyncSession", factory)

    out = await e.search("hello", max_results=10)
    assert len(out) == 3
    assert all(r.engine == "searx" for r in out)
    assert [r.rank for r in out] == [1, 2, 3]


async def test_search_skips_non_200_instances(monkeypatch):
    e = SearxEngine()

    sessions = iter([
        _mock_session_returning(429, "Too Many Requests"),
        _mock_session_returning(403, "Forbidden"),
        _mock_session_returning(200, _FAKE_SEARX_HTML),
    ])

    def factory(*args, **kwargs):
        return next(sessions)

    monkeypatch.setattr(
        "search_mcp.engines.searx.random.shuffle", lambda lst: None
    )
    monkeypatch.setattr("search_mcp.engines.searx.AsyncSession", factory)

    out = await e.search("hello", max_results=10)
    assert len(out) == 3


async def test_search_returns_empty_when_all_instances_fail(monkeypatch):
    e = SearxEngine()
    # Limit the patched instances list so we don't hit the live network.
    monkeypatch.setattr(
        "search_mcp.engines.searx._INSTANCES",
        ["https://a.test", "https://b.test"],
    )
    monkeypatch.setattr(
        "search_mcp.engines.searx.random.shuffle", lambda lst: None
    )

    def factory(*args, **kwargs):
        raise RuntimeError("boom")

    monkeypatch.setattr("search_mcp.engines.searx.AsyncSession", factory)

    out = await e.search("hello", max_results=10)
    assert out == []


async def test_search_applies_post_filters(monkeypatch):
    e = SearxEngine()

    monkeypatch.setattr(
        "search_mcp.engines.searx.random.shuffle", lambda lst: None
    )
    monkeypatch.setattr(
        "search_mcp.engines.searx.AsyncSession",
        lambda *a, **kw: _mock_session_returning(200, _FAKE_SEARX_HTML),
    )

    out = await e.search(
        "hello",
        max_results=10,
        filters=SearchFilters(exclude_text="Second"),
    )
    urls = {r.url for r in out}
    assert "https://example.com/b" not in urls
    assert "https://example.com/a" in urls


async def test_search_truncates_to_max_results(monkeypatch):
    e = SearxEngine()
    monkeypatch.setattr(
        "search_mcp.engines.searx.random.shuffle", lambda lst: None
    )
    monkeypatch.setattr(
        "search_mcp.engines.searx.AsyncSession",
        lambda *a, **kw: _mock_session_returning(200, _FAKE_SEARX_HTML),
    )

    out = await e.search("hello", max_results=2)
    assert len(out) == 2
    assert [r.rank for r in out] == [1, 2]


# ---------------------------------------------------------------------------
# Live network test
# ---------------------------------------------------------------------------


@skip_offline
async def test_live_searx_returns_results():
    e = get_engine("searx")
    out = await e.search("python language", 5)
    if not out:
        pytest.skip("all configured Searx instances were unreachable")
    assert out[0].url.startswith("http")
    assert all(r.engine == "searx" for r in out)
