"""Can a reader tell how old an answer is, and how far to trust its date?

The complaint this answers: results came back looking equally current, a week-
old cached answer looked identical to a live one, and nothing said that nine
results in ten had no date at all. None of these tests is about ranking — the
signals are reported, never scored.
"""
from __future__ import annotations

import re
import time

import pytest

from search_mcp import aggregator
from search_mcp.aggregator import USAGE_NOTE, _annotate, _merge, aggregate_search
from search_mcp.engines.base import SearchResult, classify_source
from search_mcp.fetcher import FetchResult
from search_mcp.formatting import render_fetch, render_research, render_search

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.

ISO_UTC = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$")


# ---------------------------------------------------------------------------
# classify_source
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    ("url", "kind"),
    [
        ("https://arxiv.org/abs/1706.03762", "paper"),
        ("https://github.com/python/cpython", "code"),
        ("https://stackoverflow.com/questions/1", "forum"),
        ("https://www.reuters.com/markets/", "news"),
        ("https://news.example.org/story", "news"),
        ("https://www.federalreserve.gov/newsevents.htm", "government"),
        ("https://www.gov.uk/guidance/x", "government"),
        ("https://www.moe.gov.cn/notice", "government"),
        ("https://www.army.mil/", "government"),
        ("https://www.service-public.gouv.fr/", "government"),
        ("https://ec.europa.eu/info", "government"),
        ("https://cs.stanford.edu/people", "academic"),
        ("https://www.tsinghua.edu.cn/", "academic"),
        ("https://www.ox.ac.uk/", "academic"),
        ("https://www.robomaster.com/zh-CN", ""),
        ("https://governor.example.com/", ""),
        ("https://education.example.com/", ""),
        ("https://mac.com/", ""),
        ("not a url", ""),
        ("", ""),
    ],
)
def test_classify_source(url, kind):
    assert classify_source(url) == kind


# ---------------------------------------------------------------------------
# Where a date came from
# ---------------------------------------------------------------------------


def _hit(url, engine="bing", rank=1, age="", confident=False):
    return SearchResult(
        title=f"Title for {url}",
        url=url,
        snippet="snippet",
        engine=engine,
        rank=rank,
        published_age=age,
        published_age_confident=confident,
    )


def test_each_result_says_where_its_date_came_from():
    merged = _merge(
        [
            [
                _hit("https://a.example/feed", "googlenews", 1, "2 days ago", confident=True),
                _hit("https://b.example/prose", "bing", 2, "Mar 3, 2021"),
                _hit("https://c.example/nothing", "bing", 3),
            ]
        ],
        10,
    )
    assert [r["date_source"] for r in merged] == ["structured", "snippet", "none"]
    assert "published_age" not in merged[2]
    # The internal marker the label is derived from never reaches the payload.
    assert not any("confident" in key for r in merged for key in r)


def test_a_structured_date_wins_over_a_scraped_one_for_the_same_page():
    merged = _merge(
        [
            [_hit("https://a.example/x", "bing", 1, "Jan 1, 2020")],
            [_hit("https://a.example/x", "googlenews", 1, "3 hours ago", confident=True)],
        ],
        10,
    )
    assert merged[0]["published_age"] == "3 hours ago"
    assert merged[0]["date_source"] == "structured"


def test_annotate_adds_the_kind_and_backfills_rows_cached_by_an_older_version():
    rows = _annotate(
        [
            {"url": "https://arxiv.org/abs/1", "published_age": "2024-01-01"},
            {"url": "https://www.robomaster.com/"},
            {"url": "https://x.gov/y", "date_source": "structured", "published_age": "1 day ago"},
        ]
    )
    assert rows[0] == {
        "url": "https://arxiv.org/abs/1",
        "published_age": "2024-01-01",
        "source_type": "paper",
        # Unknowable for an old row, so the weaker claim.
        "date_source": "snippet",
    }
    assert rows[1] == {"url": "https://www.robomaster.com/", "date_source": "none"}
    assert rows[2]["date_source"] == "structured" and rows[2]["source_type"] == "government"


# ---------------------------------------------------------------------------
# The search payload
# ---------------------------------------------------------------------------


class _Engine:
    name = "duckduckgo"

    def __init__(self, results):
        self._results = results

    async def search(self, query, n, filters=None, diagnostics=None):
        return list(self._results)


@pytest.fixture
def isolated_cache(tmp_path, monkeypatch):
    from search_mcp import cache as cache_mod

    fresh = cache_mod.Cache()
    fresh._path = str(tmp_path / "freshness.sqlite")
    monkeypatch.setattr(cache_mod, "cache", fresh)
    monkeypatch.setattr(aggregator, "cache", fresh)
    return fresh


def _wire(monkeypatch, results):
    engine = _Engine(results)
    monkeypatch.setattr(aggregator, "get_engine", lambda name: engine)


ONE_DATED_OF_FOUR = [
    _hit("https://www.cnbc.com/fed", "duckduckgo", 1, "2 days ago", confident=True),
    _hit("https://b.example/2", "duckduckgo", 2),
    _hit("https://c.example/3", "duckduckgo", 3),
    _hit("https://d.example/4", "duckduckgo", 4),
]


async def test_a_fresh_answer_says_when_it_was_retrieved(monkeypatch, isolated_cache):
    _wire(monkeypatch, ONE_DATED_OF_FOUR)
    before = time.time()

    out = await aggregate_search("fed rate decision", engines=["duckduckgo"])

    assert ISO_UTC.match(out["retrieved_at"])
    assert out["retrieved_at"] >= time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(before - 1))
    assert "cache_age_seconds" not in out
    assert out["dated_results"] == 1
    assert out["usage_note"] == USAGE_NOTE
    assert "freshness_note" not in out, "nobody asked for recency"


async def test_a_replayed_answer_says_how_old_it_is(monkeypatch, isolated_cache):
    _wire(monkeypatch, ONE_DATED_OF_FOUR)
    fresh = await aggregate_search("fed rate decision", engines=["duckduckgo"])
    conn = await isolated_cache._conn()
    await conn.execute("UPDATE search_cache SET created = created - 7200")
    await conn.commit()

    hit = await aggregate_search("fed rate decision", engines=["duckduckgo"])

    assert hit["cached"] is True
    assert 7200 <= hit["cache_age_seconds"] < 7260
    # `retrieved_at` is when the RESULTS were retrieved, not when they were replayed.
    assert hit["retrieved_at"] < fresh["retrieved_at"]
    assert hit["dated_results"] == 1
    assert [r.get("source_type") for r in hit["results"]] == ["news", None, None, None]


async def test_asking_for_recency_gets_told_when_recency_cannot_be_shown(monkeypatch, isolated_cache):
    _wire(monkeypatch, ONE_DATED_OF_FOUR)

    out = await aggregate_search("fed rate decision", engines=["duckduckgo"], freshness="week")

    note = out["freshness_note"]
    assert note.startswith("3 of 4 results carry no verifiable date")
    assert 'freshness="week"' in note and "Fetch the page" in note


async def test_no_note_when_most_results_are_dated(monkeypatch, isolated_cache):
    dated = [_hit(f"https://n.example/{i}", "duckduckgo", i, "1 day ago", True) for i in (1, 2, 3)]
    _wire(monkeypatch, [*dated, _hit("https://n.example/4", "duckduckgo", 4)])
    out = await aggregate_search("fed rate decision", engines=["duckduckgo"], freshness="week")
    assert "freshness_note" not in out


# ---------------------------------------------------------------------------
# Markdown
# ---------------------------------------------------------------------------


def _payload(**extra):
    return {
        "query": "fed rate decision",
        "engines": ["duckduckgo"],
        "cached": False,
        "retrieved_at": "2026-09-21T04:30:12Z",
        "dated_results": 2,
        "usage_note": USAGE_NOTE,
        "results": [
            {"title": "Fed raises rates", "url": "https://www.cnbc.com/fed", "snippet": "s",
             "engines": ["duckduckgo"], "score": 0.0164, "published_age": "2 days ago",
             "date_source": "structured", "source_type": "news"},
            {"title": "Old explainer", "url": "https://b.example/2", "snippet": "s",
             "engines": ["duckduckgo"], "score": 0.0161, "published_age": "Mar 3, 2021",
             "date_source": "snippet"},
            {"title": "Home page", "url": "https://www.federalreserve.gov", "snippet": "s",
             "engines": ["duckduckgo"], "score": 0.0159, "date_source": "none",
             "source_type": "government"},
        ],
        **extra,
    }  # fmt: skip


def test_search_markdown_shows_retrieval_time_dated_count_and_per_result_provenance():
    md = render_search(_payload())

    assert "_results: 3_  _dated: 2/3_" in md
    assert "_(retrieved 2026-09-21T04:30:12Z)_" in md
    assert "_duckduckgo_ · score 0.0164 · news · dated 2 days ago" in md
    assert "_duckduckgo_ · score 0.0161 · Mar 3, 2021 (from snippet text)" in md
    assert "_duckduckgo_ · score 0.0159 · government · undated" in md
    assert md.rstrip().endswith(f"_{USAGE_NOTE}_")


def test_search_markdown_marks_a_replayed_answer_with_its_age():
    md = render_search(_payload(cached=True, cache_age_seconds=3 * 86400 + 50))
    assert "_(cached 3 days ago · retrieved 2026-09-21T04:30:12Z)_" in md
    assert "_(from cache)_" not in md


def test_search_markdown_carries_the_undated_warning():
    md = render_search(_payload(freshness_note="2 of 3 results carry no verifiable date."))
    assert "⚠️ **Undated results:** 2 of 3 results carry no verifiable date." in md


def test_a_payload_from_before_these_fields_still_renders():
    old = {"query": "q", "engines": ["bing"], "cached": True,
           "results": [{"title": "T", "url": "https://e.org", "engines": ["bing"],
                        "published_age": "2020-01-01"}]}  # fmt: skip
    md = render_search(old)
    assert "_(from cache)_" in md
    assert "· 2020-01-01" in md and "undated" not in md and "dated:" not in md


# ---------------------------------------------------------------------------
# fetch
# ---------------------------------------------------------------------------


def _page(**kw) -> FetchResult:
    base = {"url": "https://e.org/a", "title": "A", "content": "Body.", "method": "http",
            "truncated": False, "tokens_estimated": 2}  # fmt: skip
    return FetchResult(**{**base, **kw})


def test_a_fetched_page_reports_when_this_copy_was_taken():
    live = _page(fetched_at=time.time()).to_dict()
    assert ISO_UTC.match(live["retrieved_at"]) and "cache_age_seconds" not in live

    cached = _page(method="cache", fetched_at=time.time() - 5 * 3600).to_dict()
    assert 5 * 3600 <= cached["cache_age_seconds"] < 5 * 3600 + 60

    assert "retrieved_at" not in _page().to_dict(), "unknown is omitted, not invented"


def test_fetch_markdown_states_the_publish_date_or_its_absence():
    dated = render_fetch(_page(published_date="2026-05-01", sitename="Example").to_dict())
    assert "_Example · published 2026-05-01_" in dated

    undated = render_fetch(_page().to_dict())
    assert "_no publication date found_" in undated

    image = render_fetch(_page(method="asset", media_type="image/png", content="a.png").to_dict())
    assert "no publication date" not in image


def test_fetch_markdown_says_when_the_copy_is_from_the_cache():
    md = render_fetch(_page(method="cache", fetched_at=time.time() - 2 * 86400 - 5).to_dict())
    assert "_fetched via cache_ · ~2 tokens · cached 2 days ago · retrieved " in md


# ---------------------------------------------------------------------------
# research
# ---------------------------------------------------------------------------


async def test_research_keeps_what_the_search_knew_about_each_source(monkeypatch):
    from search_mcp import research as research_mod

    async def fake_search(question, **kw):
        return {
            "query": question, "engines": ["duckduckgo"], "cached": True,
            "retrieved_at": "2026-09-20T00:00:00Z", "cache_age_seconds": 86400,
            "freshness_note": "2 of 3 results carry no verifiable date.",
            "results": [
                {"title": "Rules 2026", "url": "https://org.example/2026", "snippet": "s",
                 "engines": ["duckduckgo"], "score": 0.02, "published_age": "2026-01-09",
                 "date_source": "structured", "source_type": "government"},
                {"title": "Rules 2023", "url": "https://org.example/2023", "snippet": "s",
                 "engines": ["duckduckgo"], "score": 0.01, "date_source": "none"},
                {"title": "Forum thread", "url": "https://forum.example/t", "snippet": "s",
                 "engines": ["duckduckgo"], "score": 0.01, "date_source": "none"},
            ],
        }  # fmt: skip

    async def fake_fetch_many(urls, *a, **kw):
        dates = {"https://org.example/2026": "", "https://org.example/2023": "2023-02-01"}
        return [
            FetchResult(url=u, title="", content="Body.", method="cache", truncated=False,
                        tokens_estimated=2, published_date=dates.get(u, ""),
                        fetched_at=time.time() - 3 * 86400)
            for u in urls
        ]  # fmt: skip

    monkeypatch.setattr(research_mod, "aggregate_search", fake_search)
    monkeypatch.setattr(research_mod, "fetch_many", fake_fetch_many)

    out = await research_mod.research("robomaster rules", depth=3)

    assert out["sources"][0]["published_age"] == "2026-01-09"
    assert out["sources"][0]["date_source"] == "structured"
    assert out["sources"][0]["source_type"] == "government"
    assert "published_age" not in out["sources"][1]
    assert out["retrieved_at"] == "2026-09-20T00:00:00Z"
    assert out["search_cache_age_seconds"] == 86400
    assert out["freshness_note"].startswith("2 of 3")
    assert out["date_note"] == (
        "1 of 3 sources carry no publish date, so their age is unknown. Do not assume they "
        "are recent. Dated sources span 2023-02-01 to 2026-01-09. Where they disagree, "
        "prefer the newer one and say which you used."
    )

    md = render_research(out)
    assert "    _government · dated 2026-01-09 · page cached 3 days ago_" in md
    assert "    _published 2023-02-01 · page cached 3 days ago_" in md
    assert "    _undated · page cached 3 days ago_" in md
    assert "⚠️ **Dates:** 1 of 3 sources carry no publish date" in md
    assert "⚠️ **Undated results:** 2 of 3" in md
