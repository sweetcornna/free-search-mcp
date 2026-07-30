"""GitHub / Stack Exchange / Hacker News engines (offline).

Grouped like `test_academic_engines.py` — same `JsonApiEngine` base, related
payload quirks. What these fixtures encode is the stuff that is easy to get
wrong and invisible until a user sees it: Stack Exchange's HTML-escaped
titles, its epoch timestamps, and HN text posts that carry no submitted URL.
"""

from __future__ import annotations

import os

import pytest

from search_mcp.engines import ENGINES, get_engine
from search_mcp.engines.base import EngineKeyError, SearchFilters
from search_mcp.engines.github import GitHubCodeEngine, GitHubEngine
from search_mcp.engines.hackernews import HackerNewsEngine
from search_mcp.engines.stackexchange import StackExchangeEngine

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.

NETWORK = os.environ.get("SEARCH_MCP_TEST_NETWORK") == "1"
skip_offline = pytest.mark.skipif(
    not NETWORK, reason="set SEARCH_MCP_TEST_NETWORK=1 to run"
)


# ---------------------------------------------------------------------------
# Registry contract
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "name,category",
    [
        ("github", "github"),
        ("github_code", "github"),
        ("stackexchange", "forum"),
        ("hackernews", "forum"),
    ],
)
def test_registered_with_its_category(name, category):
    assert name in ENGINES
    assert category in get_engine(name).categories


@pytest.mark.parametrize("name", ["github", "github_code", "stackexchange", "hackernews"])
def test_stays_out_of_the_default_pool(name):
    from search_mcp.config import Settings

    assert name not in Settings().default_engines


# ---------------------------------------------------------------------------
# GitHub
# ---------------------------------------------------------------------------

_REPOS = {
    "items": [
        {
            "full_name": "modelcontextprotocol/servers",
            "html_url": "https://github.com/modelcontextprotocol/servers",
            "description": "Model Context Protocol Servers",
            "stargazers_count": 89050,
            "language": "TypeScript",
            "pushed_at": "2026-07-29T23:09:46Z",
        },
        {"full_name": "no/url", "description": "missing html_url"},
    ]
}

_ISSUES = {
    "items": [
        {
            "title": "Event loop is closed on shutdown",
            "html_url": "https://github.com/o/r/issues/1",
            "state": "open",
            "comments": 4,
            "created_at": "2026-07-21T16:38:23Z",
            "body": "Repro steps here.",
        },
        {
            "title": "fix: guard task.cancel()",
            "html_url": "https://github.com/o/r/pull/2",
            "state": "open",
            "pull_request": {"url": "https://api.github.com/..."},
            "created_at": "2026-07-22T00:00:00Z",
        },
    ]
}


def test_github_maps_repos_with_stars_and_language():
    out = GitHubEngine()._map_repos(_REPOS)
    assert len(out) == 1  # the url-less entry is dropped
    assert out[0].title == "modelcontextprotocol/servers"
    assert out[0].snippet == "★89050 · TypeScript · Model Context Protocol Servers"


def test_github_repo_date_is_last_push_not_creation():
    """"Is this project alive?" is answered by the last push, not its birthday."""
    out = GitHubEngine()._map_repos(_REPOS)
    assert out[0].published_age == "2026-07-29"
    assert out[0].published_age_confident is True


def test_github_distinguishes_prs_from_issues():
    """The search/issues endpoint returns both; the label has to say which."""
    out = GitHubEngine()._map_issues(_ISSUES)
    assert out[0].snippet.startswith("issue open · 4 comments")
    assert out[1].snippet.startswith("PR open")


def test_github_token_header_only_sent_when_configured(monkeypatch):
    from search_mcp.engines import github as mod

    monkeypatch.setattr(mod, "_token", lambda: "")
    assert "Authorization" not in mod._auth_headers()
    monkeypatch.setattr(mod, "_token", lambda: "ghp_x")
    assert mod._auth_headers()["Authorization"] == "Bearer ghp_x"


async def test_github_skips_the_issues_call_when_repos_fill_the_budget(monkeypatch):
    """GitHub's anonymous search limit is 10/min per IP and counts both calls,
    so the second request must not fire when it isn't needed."""
    e = GitHubEngine()
    calls: list[str] = []

    async def fake_get_json(url, **kw):
        calls.append(url)
        return _REPOS if "repositories" in url else _ISSUES

    monkeypatch.setattr(e, "_get_json", fake_get_json)
    await e.fetch_results("q", 1, None)
    assert len(calls) == 1, "issues call should be skipped once the budget is full"


async def test_github_falls_back_to_issues_when_repos_are_thin(monkeypatch):
    e = GitHubEngine()

    async def fake_get_json(url, **kw):
        return _REPOS if "repositories" in url else _ISSUES

    monkeypatch.setattr(e, "_get_json", fake_get_json)
    out = await e.fetch_results("q", 10, None)
    assert len(out) == 3  # 1 repo + 2 issues


@pytest.mark.parametrize("payload", [None, {}, {"items": "nope"}, "garbage", []])
def test_github_tolerates_structural_surprises(payload):
    assert GitHubEngine()._map_repos(payload) == []
    assert GitHubEngine()._map_issues(payload) == []


# ---------------------------------------------------------------------------
# github_code — keyed
# ---------------------------------------------------------------------------


async def test_github_code_without_a_token_explains_itself(monkeypatch):
    """Regression: this error used to be swallowed by JsonApiEngine's
    never-raise boundary, so an unconfigured engine reported "no results" —
    indistinguishable from "nothing matched"."""
    from search_mcp.engines import github as mod

    monkeypatch.setattr(mod, "_token", lambda: "")
    with pytest.raises(EngineKeyError, match="rejects anonymous requests"):
        await GitHubCodeEngine().search("asyncio.run", 3)


async def test_github_code_error_names_the_keyless_alternative(monkeypatch):
    from search_mcp.engines import github as mod

    monkeypatch.setattr(mod, "_token", lambda: "")
    with pytest.raises(EngineKeyError, match="`github` engine"):
        await GitHubCodeEngine().search("asyncio.run", 3)


def test_github_code_maps_repo_qualified_paths():
    payload = {
        "items": [
            {
                "path": "src/main.py",
                "html_url": "https://github.com/o/r/blob/main/src/main.py",
                "repository": {"full_name": "o/r", "description": "a repo"},
            }
        ]
    }
    out = GitHubCodeEngine().map_results(payload)
    assert out[0].title == "o/r/src/main.py"


# ---------------------------------------------------------------------------
# Stack Exchange
# ---------------------------------------------------------------------------

_SE = {
    "items": [
        {
            "title": "&quot;Asyncio Event Loop is Closed&quot; when getting loop",
            "link": "https://stackoverflow.com/questions/45600579/x",
            "creation_date": 1502312506,
            "score": 137,
            "answer_count": 3,
            "is_answered": True,
            "tags": ["python", "python-asyncio", "python-3.5"],
        },
        {"title": "No link here", "creation_date": 0},
    ]
}


def test_stackexchange_unescapes_html_entities_in_titles():
    """Titles arrive HTML-escaped; leaving them so puts literal `&quot;` in
    the rendered result list."""
    out = StackExchangeEngine().map_results(_SE)
    assert out[0].title == '"Asyncio Event Loop is Closed" when getting loop'


def test_stackexchange_converts_epoch_timestamps():
    out = StackExchangeEngine().map_results(_SE)
    assert out[0].published_age == "2017-08-09"
    assert out[0].published_age_confident is True


def test_stackexchange_snippet_leads_with_answered():
    """Whether a question has an accepted answer is the strongest signal of
    whether it is worth opening."""
    out = StackExchangeEngine().map_results(_SE)
    assert out[0].snippet.startswith("answered · score 137 · answers 3")
    assert "[python]" in out[0].snippet


def test_stackexchange_drops_entries_without_a_link():
    assert len(StackExchangeEngine().map_results(_SE)) == 1


@pytest.mark.parametrize("bad", [None, "x", -1, 0, 10**20])
def test_stackexchange_bad_epoch_yields_no_date(bad):
    assert StackExchangeEngine()._epoch_date(bad) == ""


def test_stackexchange_freshness_sorts_by_activity():
    e = StackExchangeEngine()
    assert "sort=relevance" in e.build_url("x", 5)
    assert "sort=activity" in e.build_url("x", 5, SearchFilters(freshness="week"))


# ---------------------------------------------------------------------------
# Hacker News
# ---------------------------------------------------------------------------

_HN = {
    "hits": [
        {
            "title": "Model Context Protocol",
            "url": "https://www.anthropic.com/news/model-context-protocol",
            "created_at": "2024-11-25T16:14:22Z",
            "points": 872,
            "num_comments": 258,
            "author": "benocodes",
            "objectID": "42237424",
        },
        {
            "title": "Ask HN: how do you test MCP servers?",
            "created_at": "2026-01-02T00:00:00Z",
            "points": 12,
            "objectID": "99999",
        },
        {"created_at": "2026-01-02T00:00:00Z", "objectID": "1"},
    ]
}


def test_hackernews_uses_the_submitted_url_for_link_posts():
    out = HackerNewsEngine().map_results(_HN)
    assert out[0].url == "https://www.anthropic.com/news/model-context-protocol"


def test_hackernews_text_posts_fall_back_to_the_thread():
    """Ask HN / Show HN posts carry no `url`; the discussion IS the content."""
    out = HackerNewsEngine().map_results(_HN)
    assert out[1].url == "https://news.ycombinator.com/item?id=99999"


def test_hackernews_link_posts_also_surface_the_discussion():
    out = HackerNewsEngine().map_results(_HN)
    assert "discussion: https://news.ycombinator.com/item?id=42237424" in out[0].snippet
    # A text post's URL already IS the thread — don't repeat it.
    assert "discussion:" not in out[1].snippet


def test_hackernews_skips_untitled_hits():
    assert len(HackerNewsEngine().map_results(_HN)) == 2


def test_hackernews_freshness_switches_endpoint():
    """Algolia exposes recency as a different endpoint, not a sort parameter."""
    e = HackerNewsEngine()
    assert "/search?" in e.build_url("x", 5)
    assert "/search_by_date?" in e.build_url("x", 5, SearchFilters(freshness="day"))


# ---------------------------------------------------------------------------
# Live network
# ---------------------------------------------------------------------------


@skip_offline
@pytest.mark.parametrize(
    "name,query",
    [
        ("github", "model context protocol"),
        ("stackexchange", "asyncio event loop is closed"),
        ("hackernews", "model context protocol"),
    ],
)
async def test_live_returns_results(name, query):
    out = await get_engine(name).search(query, 3)
    if not out:
        pytest.skip(f"{name} returned nothing (rate limit or outage)")
    assert out[0].url.startswith("http")
    assert all(r.engine == name for r in out)


def test_github_code_is_not_auto_routed_without_a_token(monkeypatch):
    """Category routing must not add a guaranteed failure to the pool.

    Regression: `category="github"` used to pull in github_code even with no
    token, so every such search surfaced a key error alongside its results.
    """
    from search_mcp.aggregator import engines_for_category
    from search_mcp.engines import github as mod

    monkeypatch.setattr(mod, "_token", lambda: "")
    assert "github_code" not in engines_for_category("github")

    monkeypatch.setattr(mod, "_token", lambda: "ghp_x")
    assert "github_code" in engines_for_category("github")


async def test_naming_github_code_explicitly_still_raises(monkeypatch):
    """Availability gates auto-selection only — an explicit request deserves
    the actionable error, not silence."""
    from search_mcp.engines import github as mod

    monkeypatch.setattr(mod, "_token", lambda: "")
    with pytest.raises(EngineKeyError):
        await GitHubCodeEngine().search("x", 3)
