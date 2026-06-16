"""SearXNG auto-fallback for gated SERP engines (offline).

GoogleEngine / BingEngine override search() to recover keylessly via the
working SearXNG meta-search when the provider gated us (CAPTCHA/consent shell
=> parse() yields [] and base.search() records the gate). These tests never
touch the network: the HTTP fetch is patched to return a gate page (so the
real engine parses to []), and SearxEngine is swapped for a stub returning
canned results, so we assert the fallback wiring without any I/O.
"""

from __future__ import annotations

from unittest.mock import AsyncMock

from search_mcp.engines.base import SearchResult
from search_mcp.engines.bing import BingEngine
from search_mcp.engines.google import GoogleEngine

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.


# A Google "/sorry/" interstitial: detect_gate() classifies it as "captcha"
# and GoogleEngine.parse() finds no organic blocks, so super().search() == [].
_GATE_HTML = "<html>/sorry/index unusual traffic</html>"

# A minimal but real Google SERP that parses to one organic result, so the
# happy path returns hits and NEVER touches the fallback.
_REAL_HTML = """
<html><body>
<div class="g" data-hveid="r1">
  <a href="https://example.com/a"><h3>Example A title</h3></a>
  <div class="VwiC3b">A real snippet body.</div>
</div>
</body></html>
"""


def _fake_searx_results() -> list[SearchResult]:
    return [
        SearchResult(
            title="Fallback one",
            url="https://fallback.example/1",
            snippet="from searx",
            engine="searx",
            rank=0,
        ),
        SearchResult(
            title="Fallback two",
            url="https://fallback.example/2",
            snippet="from searx",
            engine="searx",
            rank=0,
        ),
    ]


def _stub_searx_class(monkeypatch):
    """Replace ``search_mcp.engines.searx.SearxEngine`` with a stub class whose
    instances' ``search`` returns two canned results. Both GoogleEngine and
    BingEngine import it lazily via ``from .searx import SearxEngine``, so the
    name resolves from the source module at call time — patching it there
    covers both engines. Returns the AsyncMock so callers can assert call
    counts."""
    search_mock = AsyncMock(return_value=_fake_searx_results())

    class _StubSearx:
        def __init__(self, *a, **kw):
            pass

        async def search(self, *a, **kw):
            return await search_mock(*a, **kw)

    monkeypatch.setattr("search_mcp.engines.searx.SearxEngine", _StubSearx)
    return search_mock


# ---------------------------------------------------------------------------
# Google
# ---------------------------------------------------------------------------


async def test_google_gated_falls_back_to_searx(monkeypatch):
    engine = GoogleEngine()
    # super().search() fetches a gate page -> parse() == [] -> base records gate.
    monkeypatch.setattr(engine, "_fetch", AsyncMock(return_value=_GATE_HTML))
    # Skip the base Playwright browser-fallback render (offline, fast).
    monkeypatch.setattr(
        "search_mcp.engines.base.settings.fetch_strategy", "http"
    )
    search_mock = _stub_searx_class(monkeypatch)

    diag: dict = {}
    out = await engine.search("anything", 5, diagnostics=diag)

    assert [r.url for r in out] == [
        "https://fallback.example/1",
        "https://fallback.example/2",
    ]
    assert all(r.engine == "searx" for r in out)
    # The fallback was actually invoked.
    search_mock.assert_awaited_once()
    # Diagnostics record both the gate and the fallback engine.
    assert diag["fallback"]["google"] == "searx"
    assert diag["gated"]["google"]  # base recorded "captcha"; fallback keeps it


async def test_google_with_results_does_not_fall_back(monkeypatch):
    engine = GoogleEngine()
    monkeypatch.setattr(engine, "_fetch", AsyncMock(return_value=_REAL_HTML))
    monkeypatch.setattr(
        "search_mcp.engines.base.settings.fetch_strategy", "http"
    )
    search_mock = _stub_searx_class(monkeypatch)

    diag: dict = {}
    out = await engine.search("anything", 5, diagnostics=diag)

    assert [r.url for r in out] == ["https://example.com/a"]
    assert all(r.engine == "google" for r in out)
    # No gate, real results: the fallback must NOT be called.
    search_mock.assert_not_awaited()
    assert "fallback" not in diag


# ---------------------------------------------------------------------------
# Bing (HTTP-first, browser fallback on empty parse)
# ---------------------------------------------------------------------------


async def test_bing_gated_falls_back_to_searx(monkeypatch):
    engine = BingEngine()
    # Bing is now needs_browser=False: super().search() does the HTTP _fetch
    # first, and on an empty parse the base retries via the browser pool. Stub
    # both to return the gate page so the test stays offline and parse() == [],
    # which makes base record the gate and bing fall back to searx.
    monkeypatch.setattr(engine, "_fetch", AsyncMock(return_value=_GATE_HTML))
    monkeypatch.setattr(
        "search_mcp.engines.base.pool.fetch_html",
        AsyncMock(return_value=("", _GATE_HTML)),
    )
    search_mock = _stub_searx_class(monkeypatch)

    diag: dict = {}
    out = await engine.search("anything", 5, diagnostics=diag)

    assert [r.url for r in out] == [
        "https://fallback.example/1",
        "https://fallback.example/2",
    ]
    assert all(r.engine == "searx" for r in out)
    search_mock.assert_awaited_once()
    assert diag["fallback"]["bing"] == "searx"
    assert diag["gated"]["bing"]


async def test_bing_http_fetch_raises_still_falls_back_to_searx(monkeypatch):
    # Bing is now HTTP-first (needs_browser=False). Under fetch_strategy="http" a
    # www4 non-200 makes base._fetch RAISE instead of returning an empty body;
    # bing must still recover via SearXNG (never propagate / never silently die).
    from curl_cffi.requests.exceptions import RequestException

    engine = BingEngine()
    monkeypatch.setattr(
        engine, "_fetch", AsyncMock(side_effect=RequestException("non-200 shell"))
    )
    search_mock = _stub_searx_class(monkeypatch)

    diag: dict = {}
    out = await engine.search("anything", 5, diagnostics=diag)

    assert [r.url for r in out] == [
        "https://fallback.example/1",
        "https://fallback.example/2",
    ]
    assert all(r.engine == "searx" for r in out)
    search_mock.assert_awaited_once()
    assert diag["fallback"]["bing"] == "searx"


async def test_bing_with_results_does_not_fall_back(monkeypatch):
    engine = BingEngine()
    real_bing_html = """
    <html><body>
    <li class="b_algo">
      <h2><a href="https://example.com/bing">Bing Result</a></h2>
      <div class="b_caption"><p>A real bing snippet.</p></div>
    </li>
    </body></html>
    """
    monkeypatch.setattr(engine, "_fetch", AsyncMock(return_value=real_bing_html))
    search_mock = _stub_searx_class(monkeypatch)

    diag: dict = {}
    out = await engine.search("anything", 5, diagnostics=diag)

    assert [r.url for r in out] == ["https://example.com/bing"]
    assert all(r.engine == "bing" for r in out)
    search_mock.assert_not_awaited()
    assert "fallback" not in diag
