"""Tests for snippet/title date extraction.

Search engines emit publication dates in mixed formats ("2 days ago",
"Apr 28, 2026", "2024-12-01"). We parse those into a normalised
``published_age`` field on each ``SearchResult`` so the LLM can judge
freshness without fetching every URL.
"""
from __future__ import annotations

from search_mcp.engines import SearchFilters, SearchResult, apply_post_filters
from search_mcp.engines.base import extract_date_hint


# ---------------------------------------------------------------------------
# extract_date_hint — pure helper, fully offline
# ---------------------------------------------------------------------------


def test_extract_date_hint_absolute_long_form():
    assert extract_date_hint("Apr 28, 2026 — OpenAI announced…") == "2026-04-28"


def test_extract_date_hint_iso():
    assert extract_date_hint("2024-12-01: release notes for…") == "2024-12-01"


def test_extract_date_hint_relative_days():
    assert extract_date_hint("2 days ago - Reuters reports…") == "2 days ago"


def test_extract_date_hint_relative_singular():
    assert extract_date_hint("1 day ago, Reuters reports…") == "1 day ago"


def test_extract_date_hint_relative_hours():
    assert extract_date_hint("3 hours ago: breaking news") == "3 hours ago"


def test_extract_date_hint_full_month_name():
    assert extract_date_hint("April 28, 2026: a press release") == "2026-04-28"


def test_extract_date_hint_no_year_returns_empty():
    # Without a year we'd have to guess — refuse to.
    assert extract_date_hint("Apr 28 — undated note") == ""


def test_extract_date_hint_empty_input():
    assert extract_date_hint("") == ""
    assert extract_date_hint(None) == ""  # type: ignore[arg-type]


def test_extract_date_hint_random_text():
    assert extract_date_hint("just some random text without a date") == ""


def test_extract_date_hint_today_yesterday_ignored():
    # Documented non-goal: too ambiguous without a timezone.
    assert extract_date_hint("Today's update from the team") == ""
    assert extract_date_hint("Yesterday's blog post") == ""


def test_extract_date_hint_relative_beats_absolute_when_both_present():
    # Engines often write "Apr 28, 2026 (2 days ago)" — relative is shorter
    # and self-describing, so it wins.
    out = extract_date_hint("Apr 28, 2026 (2 days ago) — story")
    assert out == "2 days ago"


# ---------------------------------------------------------------------------
# SearchResult plumbing — field carries through to_dict()
# ---------------------------------------------------------------------------


def test_search_result_default_published_age_is_empty():
    r = SearchResult(title="t", url="https://x", snippet="s", engine="x", rank=1)
    assert r.published_age == ""
    assert r.to_dict()["published_age"] == ""


def test_search_result_published_age_round_trips():
    r = SearchResult(
        title="t", url="https://x", snippet="s", engine="x", rank=1,
        published_age="2 days ago",
    )
    d = r.to_dict()
    assert d["published_age"] == "2 days ago"


# ---------------------------------------------------------------------------
# Regression: post-filter must not strip published_age
# ---------------------------------------------------------------------------


def test_apply_post_filters_preserves_published_age():
    results = [
        SearchResult(
            title="Anthropic update",
            url="https://github.com/anthropics/x",
            snippet="2 days ago — release",
            engine="x",
            rank=1,
            published_age="2 days ago",
        ),
        SearchResult(
            title="Other",
            url="https://example.com/y",
            snippet="",
            engine="x",
            rank=2,
            published_age="2024-12-01",
        ),
    ]
    out = apply_post_filters(
        results, SearchFilters(include_domains=["github.com"])
    )
    assert len(out) == 1
    assert out[0].published_age == "2 days ago"


def test_apply_post_filters_no_filter_preserves_field():
    results = [
        SearchResult(
            title="t", url="https://x", snippet="s",
            engine="x", rank=1, published_age="2026-04-28",
        ),
    ]
    out = apply_post_filters(results, None)
    assert out[0].published_age == "2026-04-28"
