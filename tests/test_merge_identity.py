"""One page, one result: what counts as the same URL, and what must not.

Plus the cache half of the same concern — an answer is only reusable for the
question it actually answered, for as long as it can still be true.
"""
from __future__ import annotations

import json
import time
from pathlib import Path

import pytest

from search_mcp import aggregator
from search_mcp.aggregator import (
    _dedup_by_title,
    _key,
    _merge,
    _read_ttl,
    _url_key,
    aggregate_search,
)
from search_mcp.config import settings
from search_mcp.engines.base import SearchFilters, SearchResult

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.

FIXTURE = Path(__file__).resolve().parents[1] / "evals/ranking/fixtures/decoy_and_dupes.json"


def hit(url: str, engine: str, rank: int = 1, title: str = "A page", snippet: str = "") -> SearchResult:
    return SearchResult(title=title, url=url, snippet=snippet, engine=engine, rank=rank)


# ---------------------------------------------------------------------------
# _url_key
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "variant",
    [
        "http://arxiv.org/abs/1512.03385",
        "https://www.arxiv.org/abs/1512.03385",
        "https://ARXIV.org/abs/1512.03385/",
        "https://arxiv.org:443/abs/1512.03385",
        "http://arxiv.org:80/abs/1512.03385#abstract",
        "//arxiv.org/abs/1512.03385",
    ],
)
def test_variants_of_one_page_share_a_key(variant):
    assert _url_key(variant) == _url_key("https://arxiv.org/abs/1512.03385")


@pytest.mark.parametrize(
    ("a", "b"),
    [
        # Different pages. `_canonical_host` folds these; identity must not.
        ("https://www.amazon.co.uk/dp/B0ABC", "https://www.amazon.com/dp/B0ABC"),
        ("https://m.example.com/story", "https://example.com/story"),
        # The query string is part of the page.
        ("https://example.com/watch?v=1", "https://example.com/watch?v=2"),
        # Tracking parameters stay: stripping them was measured and rejected.
        ("https://example.com/a?utm_source=x", "https://example.com/a"),
        ("https://example.com:8443/a", "https://example.com/a"),
        ("https://example.com/a", "https://example.com/A"),
    ],
)
def test_different_pages_keep_different_keys(a, b):
    assert _url_key(a) != _url_key(b)


@pytest.mark.parametrize("junk", ["", "not a url", "https://[::1", "mailto:someone@example.com"])
def test_unparseable_input_falls_back_without_raising(junk):
    assert isinstance(_url_key(junk), str)


# ---------------------------------------------------------------------------
# _merge
# ---------------------------------------------------------------------------


def test_agreement_across_schemes_is_rewarded_once_not_printed_twice():
    merged = _merge(
        [
            [hit("http://arxiv.org/abs/1512.03385", "openalex", 1, snippet="short")],
            [hit("https://arxiv.org/abs/1512.03385", "bing", 2, snippet="a much longer snippet")],
            [hit("https://www.arxiv.org/abs/1512.03385/", "duckduckgo", 1)],
        ],
        10,
    )
    assert len(merged) == 1
    rec = merged[0]
    assert rec["engines"] == ["bing", "duckduckgo", "openalex"]
    assert rec["score"] == round(1 / 61 + 1 / 62 + 1 / 61, 5)
    assert rec["snippet"] == "a much longer snippet"
    # The https sighting is what gets handed back, though http was seen first.
    assert rec["url"] == "https://arxiv.org/abs/1512.03385"


def test_the_emitted_url_is_always_one_an_engine_returned():
    """The key is scheme-less. It must never reach the payload: replaying the
    capture with the key as the URL regressed two cases outright."""
    merged = _merge([[hit("https://www.example.com/a/", "bing"), hit("http://Example.org/b", "bing", 2)]], 10)
    assert [r["url"] for r in merged] == ["https://www.example.com/a", "http://Example.org/b"]


def test_an_http_only_page_stays_http():
    merged = _merge([[hit("http://legacy.example/a", "bing")], [hit("http://legacy.example/a", "ddg")]], 10)
    assert merged[0]["url"] == "http://legacy.example/a"


def test_the_captured_arxiv_duplicate_now_merges():
    case = next(
        c for c in json.loads(FIXTURE.read_text(encoding="utf-8"))["cases"]
        if c["query"].startswith("deep residual")
    )  # fmt: skip
    buckets = [
        [SearchResult(title=r["title"], url=r["url"], snippet=r.get("snippet", ""), engine=name, rank=i + 1)
         for i, r in enumerate(rows)]
        for name, rows in case["buckets"].items()
    ]  # fmt: skip
    merged = _merge(buckets, 50, case["category"])
    paper = [r for r in merged if r["url"].rstrip("/").endswith("arxiv.org/abs/1512.03385")]
    assert len(paper) == 1
    assert {"openalex", "bing", "duckduckgo"} <= set(paper[0]["engines"])
    assert paper[0]["url"].startswith("https://")


# ---------------------------------------------------------------------------
# _dedup_by_title: the digit guard
# ---------------------------------------------------------------------------


def _item(title: str, url: str) -> dict:
    return {"title": title, "url": url}


def test_an_arxiv_id_prefix_does_not_make_the_same_paper_distinct():
    items = [
        _item("Deep Residual Learning for Image Recognition", "https://arxiv.org/abs/1512.03385"),
        _item("[1512.03385] Deep Residual Learning for Image Recognition", "https://arxiv.org/pdf/1512.03385"),
    ]
    assert len(_dedup_by_title(items)) == 1


@pytest.mark.parametrize(
    ("a", "b"),
    [
        ("Python 3.13 released", "Python 3.12 released"),
        ("iPhone 15 review", "iPhone 14 review"),
        ("Fed cuts rates by 25 basis points", "Fed cuts rates by 50 basis points"),
    ],
)
def test_titles_whose_numbers_differ_are_still_distinct(a, b):
    items = [_item(a, "https://example.com/1"), _item(b, "https://example.com/2")]
    assert len(_dedup_by_title(items)) == 2


# ---------------------------------------------------------------------------
# The cache key
# ---------------------------------------------------------------------------


def test_the_key_changes_with_anything_that_changes_the_answer(monkeypatch):
    base = _key("q", ["duckduckgo"], 10, SearchFilters())
    assert base == _key("q", ["duckduckgo"], 10, SearchFilters())

    for field, value in [("region", "jp-ja"), ("safesearch", "off"), ("accept_language", "ja")]:
        with monkeypatch.context() as patch:
            patch.setattr(settings, field, value)
            assert _key("q", ["duckduckgo"], 10, SearchFilters()) != base, field


def test_the_key_is_versioned_so_pre_guard_rows_are_orphaned():
    """Rows written before the off-topic guard can hold merged decoy results
    for a week. The version field is what makes them unreachable."""
    import hashlib
    from dataclasses import asdict

    unversioned = hashlib.sha256(
        json.dumps(
            {"q": "q", "e": ["duckduckgo"], "n": 10, "f": asdict(SearchFilters())},
            ensure_ascii=False,
            sort_keys=True,
        ).encode()
    ).hexdigest()
    assert _key("q", ["duckduckgo"], 10, SearchFilters()) != unversioned


# ---------------------------------------------------------------------------
# How long a cached answer stays usable
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    ("freshness", "category", "max_age", "expected"),
    [
        (None, None, None, None),  # the configured default
        (None, None, 120, 120),
        ("day", None, None, 3600),
        ("week", None, None, 6 * 3600),
        ("month", None, None, 24 * 3600),
        ("year", None, None, None),
        ("day", None, 60, 60),  # the caller's tighter bound wins
        ("day", None, 999_999, 3600),  # ...and a looser one does not
        (None, "news", None, 6 * 3600),
        (None, "news.tech", None, 6 * 3600),
        ("day", "news", None, 3600),
        (None, "paper", None, None),
        ("week", None, 0, 0),
    ],
)
def test_read_ttl(freshness, category, max_age, expected):
    assert _read_ttl(freshness, category, max_age) == expected


@pytest.fixture
def isolated_cache(tmp_path, monkeypatch):
    from search_mcp import cache as cache_mod

    fresh = cache_mod.Cache()
    fresh._path = str(tmp_path / "merge_identity.sqlite")
    monkeypatch.setattr(cache_mod, "cache", fresh)
    monkeypatch.setattr(aggregator, "cache", fresh)
    return fresh


class _Engine:
    name = "duckduckgo"

    def __init__(self):
        self.calls = 0

    async def search(self, query, n, filters=None, diagnostics=None):
        self.calls += 1
        return [hit(f"https://example.org/{self.calls}", self.name, title=f"run {self.calls}")]


async def _age_rows(cache, seconds: int) -> None:
    conn = await cache._conn()
    await conn.execute("UPDATE search_cache SET created = created - ?", (seconds,))
    await conn.commit()


async def test_a_two_hour_old_answer_is_not_served_for_a_freshness_day_query(
    isolated_cache, monkeypatch
):
    engine = _Engine()
    monkeypatch.setattr(aggregator, "get_engine", lambda name: engine)

    first = await aggregate_search("anything new", engines=["duckduckgo"], freshness="day")
    await _age_rows(isolated_cache, 2 * 3600)
    second = await aggregate_search("anything new", engines=["duckduckgo"], freshness="day")

    assert (first["cached"], second["cached"]) == (False, False)
    assert engine.calls == 2


async def test_the_same_two_hour_old_answer_is_fine_without_a_freshness_window(
    isolated_cache, monkeypatch
):
    engine = _Engine()
    monkeypatch.setattr(aggregator, "get_engine", lambda name: engine)

    await aggregate_search("evergreen topic", engines=["duckduckgo"])
    await _age_rows(isolated_cache, 2 * 3600)
    second = await aggregate_search("evergreen topic", engines=["duckduckgo"])

    assert second["cached"] is True
    assert engine.calls == 1


async def test_a_cache_hit_knows_when_it_was_written_without_storing_it(isolated_cache):
    before = time.time()
    await isolated_cache.put_search("k", "q", ["duckduckgo"], [{"url": "https://e.org"}], {"x": 1})

    _, meta = await isolated_cache.get_search("k")
    assert before - 1 <= meta["cached_at"] <= time.time() + 1
    assert meta["x"] == 1

    conn = await isolated_cache._conn()
    row = await (await conn.execute("SELECT meta FROM search_cache WHERE cache_key='k'")).fetchone()
    assert "cached_at" not in json.loads(row[0])
