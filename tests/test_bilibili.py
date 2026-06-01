"""Tests for the Bilibili keyless web-search JSON engine.

The engine talks to https://api.bilibili.com/x/web-interface/search/all/v2,
which returns a JSON envelope ``{"code":0,"data":{"result":[...groups...]}}``.
Offline tests mock the engine's ``_fetch`` so no network is touched; a single
live test is gated on ``SEARCH_MCP_TEST_NETWORK=1``.
"""

from __future__ import annotations

import json
import os
from unittest.mock import AsyncMock, patch

import pytest

from search_mcp.engines.base import SearchFilters
from search_mcp.engines.bilibili import BilibiliEngine

# pytest.ini sets `asyncio_mode = auto` so async tests need no decorator.

NETWORK = os.environ.get("SEARCH_MCP_TEST_NETWORK") == "1"
skip_offline = pytest.mark.skipif(
    not NETWORK, reason="set SEARCH_MCP_TEST_NETWORK=1 to run"
)


# A trimmed but faithful copy of the all/v2 envelope: a non-video group first
# (to prove we skip it), then the video group whose items we parse. The first
# video has an <em> highlight in its title and a protocol-relative arcurl; the
# second has a plain title and no pubdate.
SAMPLE_JSON = json.dumps(
    {
        "code": 0,
        "data": {
            "result": [
                {
                    "result_type": "bili_user",
                    "data": [{"uname": "Some Creator", "mid": 12345}],
                },
                {
                    "result_type": "video",
                    "data": [
                        {
                            "title": 'Learn <em class="keyword">Python</em> fast',
                            "arcurl": "//www.bilibili.com/video/BV1aa",
                            "author": "TeacherA",
                            "description": "A beginner Python tutorial.",
                            "pubdate": 1714316400,  # 2024-04-28 UTC
                            "play": 99999,
                        },
                        {
                            "title": "Plain title no tags",
                            "arcurl": "https://www.bilibili.com/video/BV1bb",
                            "author": "TeacherB",
                            "description": "Second video.",
                            "pubdate": 0,
                            "play": 10,
                        },
                    ],
                },
            ]
        },
    }
)


# ---------------------------------------------------------------------------
# build_url
# ---------------------------------------------------------------------------


def test_build_url_hits_all_v2_endpoint():
    e = BilibiliEngine()
    url = e.build_url("hello world", 10)
    assert url.startswith(
        "https://api.bilibili.com/x/web-interface/search/all/v2?"
    )
    assert "keyword=hello+world" in url
    assert "page=1" in url


def test_engine_flags():
    e = BilibiliEngine()
    assert e.name == "bilibili"
    assert e.needs_browser is False
    # JSON feed: no browser recovery render.
    assert e.supports_browser_fallback is False


# ---------------------------------------------------------------------------
# parse() — direct unit tests on canned JSON
# ---------------------------------------------------------------------------


def test_parse_extracts_video_group_only():
    e = BilibiliEngine()
    out = e.parse(SAMPLE_JSON)
    # Only the two items in the result_type=="video" group; bili_user skipped.
    assert len(out) == 2

    first = out[0]
    # <em class="keyword"> stripped from the title.
    assert first.title == "Learn Python fast"
    assert "<em" not in first.title and "</em>" not in first.title
    # Protocol-relative arcurl normalised to https.
    assert first.url == "https://www.bilibili.com/video/BV1aa"
    assert first.snippet == "A beginner Python tutorial."
    # unix pubdate -> ISO date.
    assert first.published_age == "2024-04-28"

    second = out[1]
    assert second.title == "Plain title no tags"
    assert second.url == "https://www.bilibili.com/video/BV1bb"
    # pubdate 0 -> empty published_age.
    assert second.published_age == ""


def test_parse_returns_empty_on_bad_or_empty_input():
    e = BilibiliEngine()
    assert e.parse("") == []
    assert e.parse("garbage") == []
    assert e.parse('{"code":-412}') == []
    # Valid JSON, code 0, but no usable data shapes.
    assert e.parse('{"code":0,"data":{}}') == []
    assert e.parse('{"code":0,"data":{"result":[]}}') == []
    # A JSON list (not a dict) must not raise.
    assert e.parse("[1, 2, 3]") == []


# ---------------------------------------------------------------------------
# search() — base impl with _fetch mocked (filters/rank/diagnostics for free)
# ---------------------------------------------------------------------------


async def test_search_parses_items_and_sets_rank_engine():
    e = BilibiliEngine()
    with patch.object(e, "_fetch", AsyncMock(return_value=SAMPLE_JSON)):
        out = await e.search("python", 10)
    assert len(out) == 2
    assert all(r.engine == "bilibili" for r in out)
    assert [r.rank for r in out] == [1, 2]
    assert out[0].title == "Learn Python fast"
    assert out[0].url == "https://www.bilibili.com/video/BV1aa"
    assert out[0].published_age == "2024-04-28"


async def test_search_empty_fetch_yields_empty():
    e = BilibiliEngine()
    with patch.object(e, "_fetch", AsyncMock(return_value="")):
        out = await e.search("python", 10)
    assert out == []


async def test_search_code_minus_412_yields_empty():
    e = BilibiliEngine()
    with patch.object(e, "_fetch", AsyncMock(return_value='{"code":-412}')):
        out = await e.search("python", 10)
    assert out == []


async def test_search_garbage_fetch_yields_empty():
    e = BilibiliEngine()
    with patch.object(e, "_fetch", AsyncMock(return_value="garbage")):
        out = await e.search("python", 10)
    assert out == []


async def test_search_truncates_to_max_results():
    e = BilibiliEngine()
    with patch.object(e, "_fetch", AsyncMock(return_value=SAMPLE_JSON)):
        out = await e.search("python", 1)
    assert len(out) == 1
    assert out[0].rank == 1


async def test_search_applies_post_filters():
    e = BilibiliEngine()
    with patch.object(e, "_fetch", AsyncMock(return_value=SAMPLE_JSON)):
        out = await e.search(
            "python", 10, filters=SearchFilters(exclude_text="beginner")
        )
    titles = {r.title for r in out}
    # First item's snippet contains "beginner" -> dropped.
    assert "Learn Python fast" not in titles
    assert "Plain title no tags" in titles


# ---------------------------------------------------------------------------
# Live network test
# ---------------------------------------------------------------------------


@skip_offline
async def test_live_bilibili_search():
    e = BilibiliEngine()
    out = await e.search("python", 5)
    if not out:
        pytest.skip("Bilibili returned no results (transient / -412)")
    assert out[0].url.startswith("http")
    assert all(r.engine == "bilibili" for r in out)
