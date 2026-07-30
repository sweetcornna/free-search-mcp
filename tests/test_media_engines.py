"""Openverse (images) and Zenodo (datasets), plus the transport quirks they exposed.

Both engines needed a fix that no other source did, and both fixes are the
kind that silently regress: they look like cosmetic request tweaks but are the
difference between results and an empty list.
"""

from __future__ import annotations

import os

import pytest

from search_mcp.engines import ENGINES, get_engine
from search_mcp.engines.base import SearchFilters
from search_mcp.engines.jsonapi import USER_AGENT, JsonApiEngine
from search_mcp.engines.openverse import OpenverseEngine
from search_mcp.engines.zenodo import ZenodoEngine

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.

NETWORK = os.environ.get("SEARCH_MCP_TEST_NETWORK") == "1"
skip_offline = pytest.mark.skipif(
    not NETWORK, reason="set SEARCH_MCP_TEST_NETWORK=1 to run"
)


@pytest.mark.parametrize("name,category", [("openverse", "image"), ("zenodo", "dataset")])
def test_registered_with_its_category(name, category):
    assert name in ENGINES
    assert category in get_engine(name).categories


# ---------------------------------------------------------------------------
# Transport quirks
# ---------------------------------------------------------------------------


def test_openverse_asks_for_json_in_the_query_string():
    """Regression: Openverse is Django REST Framework, and header-based
    content negotiation does not survive curl_cffi's browser impersonation —
    it answered 200 with the browsable *HTML* API, which then failed to parse
    as JSON and looked exactly like "no results"."""
    assert "format=json" in OpenverseEngine().build_url("x", 5)


def test_zenodo_does_not_impersonate_a_browser():
    """Regression: Zenodo answers 403 to clients presenting a browser TLS and
    header fingerprint on its API. Every other engine wants the opposite."""
    assert ZenodoEngine().impersonate is None


def test_engines_impersonate_chrome_by_default():
    """The exception must stay an exception — scrapers need the fingerprint."""
    from search_mcp.httpfetch import IMPERSONATE

    assert JsonApiEngine.impersonate == IMPERSONATE
    assert OpenverseEngine().impersonate == IMPERSONATE


def test_honest_user_agent_is_contactable():
    """An API operator seeing this in their logs should be able to find us."""
    assert "free-search-mcp" in USER_AGENT
    assert "github.com" in USER_AGENT


async def test_non_impersonating_engine_sends_the_honest_agent(monkeypatch):
    seen: dict = {}

    class _Resp:
        status_code = 200
        text = "{}"

    class _Session:
        def __init__(self, **kwargs):
            seen.update(kwargs)

        async def __aenter__(self):
            return self

        async def __aexit__(self, *a):
            return False

        async def get(self, url):
            return _Resp()

    monkeypatch.setattr("search_mcp.engines.jsonapi.AsyncSession", _Session)
    await ZenodoEngine()._get_json("https://zenodo.org/api/records?q=x")

    assert "impersonate" not in seen
    assert seen["headers"]["User-Agent"] == USER_AGENT


async def test_impersonating_engine_keeps_its_fingerprint(monkeypatch):
    seen: dict = {}

    class _Resp:
        status_code = 200
        text = "{}"

    class _Session:
        def __init__(self, **kwargs):
            seen.update(kwargs)

        async def __aenter__(self):
            return self

        async def __aexit__(self, *a):
            return False

        async def get(self, url):
            return _Resp()

    monkeypatch.setattr("search_mcp.engines.jsonapi.AsyncSession", _Session)
    await OpenverseEngine()._get_json("https://api.openverse.org/v1/images/?q=x")

    from search_mcp.httpfetch import IMPERSONATE

    assert seen["impersonate"] == IMPERSONATE
    assert "User-Agent" not in seen["headers"]


# ---------------------------------------------------------------------------
# Openverse
# ---------------------------------------------------------------------------

_OPENVERSE = {
    "results": [
        {
            "title": "Cradle Mountain Sunrise",
            "url": "https://live.staticflickr.com/7023/6558731117.jpg",
            "foreign_landing_url": "https://www.flickr.com/photos/x/6558731117",
            "license": "by-nc-nd",
            "license_version": "2.0",
            "creator": "Mark Wassell",
            "width": 1024,
            "height": 764,
        },
        {"url": "https://x.example/untitled.png"},
        {"title": "no url"},
    ]
}


def test_openverse_url_is_the_image_file_itself():
    """So a result can go straight into `fetch(inline=True)`."""
    out = OpenverseEngine().map_results(_OPENVERSE)
    assert out[0].url.endswith(".jpg")


def test_openverse_snippet_leads_with_the_licence():
    """An image search that doesn't say what you may do with the picture is a
    trap; the licence is the first thing shown."""
    out = OpenverseEngine().map_results(_OPENVERSE)
    assert out[0].snippet.startswith("CC BY-NC-ND 2.0 · by Mark Wassell")
    assert "1024×764px" in out[0].snippet
    assert "flickr.com" in out[0].snippet


def test_openverse_untitled_images_fall_back_to_the_filename():
    out = OpenverseEngine().map_results(_OPENVERSE)
    assert out[1].title == "untitled.png"


def test_openverse_skips_entries_without_a_url():
    assert len(OpenverseEngine().map_results(_OPENVERSE)) == 2


@pytest.mark.parametrize("payload", [None, {}, {"results": "nope"}, "junk", []])
def test_openverse_tolerates_structural_surprises(payload):
    assert OpenverseEngine().map_results(payload) == []


# ---------------------------------------------------------------------------
# Zenodo
# ---------------------------------------------------------------------------

_ZENODO = {
    "hits": {
        "hits": [
            {
                "doi": "10.5281/zenodo.123",
                "links": {"self_html": "https://zenodo.org/records/123"},
                "files": [{"key": "a.csv"}, {"key": "b.csv"}],
                "metadata": {
                    "title": "All Countries' Dataset",
                    "publication_date": "2025-09-10",
                    "resource_type": {"type": "dataset"},
                    "creators": [{"name": "Ada L"}],
                    "description": "<p>Tidy country data.</p>",
                },
            },
            {
                "doi_url": "https://doi.org/10.5281/zenodo.456",
                "metadata": {"title": "DOI only", "publication_date": "2024-01-01"},
            },
            {"metadata": {"title": ""}},
        ]
    }
}


def test_zenodo_prefers_the_landing_page():
    out = ZenodoEngine().map_results(_ZENODO)
    assert out[0].url == "https://zenodo.org/records/123"


def test_zenodo_falls_back_to_the_doi():
    out = ZenodoEngine().map_results(_ZENODO)
    assert out[1].url == "https://doi.org/10.5281/zenodo.456"


def test_zenodo_snippet_describes_the_record():
    out = ZenodoEngine().map_results(_ZENODO)
    assert out[0].snippet.startswith("dataset — 2 files — Ada L —")
    assert "<p>" not in out[0].snippet
    assert "Tidy country data." in out[0].snippet


def test_zenodo_dates_are_confident():
    out = ZenodoEngine().map_results(_ZENODO)
    assert out[0].published_age == "2025-09-10"
    assert out[0].published_age_confident is True


def test_zenodo_skips_untitled_records():
    assert len(ZenodoEngine().map_results(_ZENODO)) == 2


def test_zenodo_freshness_switches_sort():
    e = ZenodoEngine()
    assert "sort=bestmatch" in e.build_url("x", 5)
    assert "sort=mostrecent" in e.build_url("x", 5, SearchFilters(freshness="week"))


# ---------------------------------------------------------------------------
# Exclusive routing
# ---------------------------------------------------------------------------


def test_image_and_dataset_replace_the_default_pool():
    """A general web engine cannot return an image file or a dataset record,
    so mixing it in only crowds out the source that can."""
    from search_mcp.aggregator import _EXCLUSIVE_CATEGORIES

    assert "image" in _EXCLUSIVE_CATEGORIES
    assert "dataset" in _EXCLUSIVE_CATEGORIES


async def test_image_search_uses_only_the_image_engine(monkeypatch):
    from search_mcp import aggregator as agg

    used: list[str] = []

    class _Stub:
        def __init__(self, name):
            self.name = name

        async def search(self, *a, **kw):
            used.append(self.name)
            return []

    monkeypatch.setattr(agg, "get_engine", lambda name: _Stub(name))

    async def _ok(name, max_wait=None):
        return True

    monkeypatch.setattr(agg.search_limiter, "acquire", _ok)
    monkeypatch.setattr(agg.settings, "rescue_enabled", False)

    await agg.aggregate_search("cats", category="image", max_results=3, use_cache=False)
    assert used == ["openverse"]


async def test_paper_search_still_augments_the_default_pool(monkeypatch):
    """Non-exclusive categories keep the web engines — a paper query benefits
    from both the specialists and ordinary web results."""
    from search_mcp import aggregator as agg

    used: list[str] = []

    class _Stub:
        def __init__(self, name):
            self.name = name

        async def search(self, *a, **kw):
            used.append(self.name)
            return []

    monkeypatch.setattr(agg, "get_engine", lambda name: _Stub(name))

    async def _ok(name, max_wait=None):
        return True

    monkeypatch.setattr(agg.search_limiter, "acquire", _ok)
    monkeypatch.setattr(agg.settings, "rescue_enabled", False)

    await agg.aggregate_search("x", category="paper", max_results=3, use_cache=False)
    assert "duckduckgo" in used
    assert "arxiv" in used


# ---------------------------------------------------------------------------
# Live network
# ---------------------------------------------------------------------------


@skip_offline
@pytest.mark.parametrize(
    "name,query", [("openverse", "mountain sunrise"), ("zenodo", "climate")]
)
async def test_live_returns_results(name, query):
    out = await get_engine(name).search(query, 3)
    if not out:
        pytest.skip(f"{name} returned nothing")
    assert out[0].url.startswith("http")
    assert all(r.engine == name for r in out)
