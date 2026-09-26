"""No API key, as an invariant rather than a slogan.

The project's identity is that it works with nothing configured: no account, no
key, no signup. Manual keys stay possible — an operator can enable a handful of
opt-in engines — but nothing the server does BY ITSELF may depend on one. These
tests are what makes that checkable: add a keyed engine to the default pool, a
reserve list or a category route, and the build fails.
"""
from __future__ import annotations

from typing import get_args

import pytest

from search_mcp import keystore, oauth
from search_mcp.aggregator import _nominal_pool, engines_for_category
from search_mcp.config import Settings
from search_mcp.engines import ENGINES, Category, get_engine
from search_mcp.engines.base import EngineKeyError
from search_mcp.keystore import PROVIDERS, opt_in_engines

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.

opt_in = set(opt_in_engines())


@pytest.fixture(autouse=True)
def _no_provider_keys(monkeypatch):
    """Every test here describes the server with nothing configured.

    conftest already removes provider keys, except in a live run
    (`SEARCH_MCP_TEST_NETWORK=1`), where it leaves them for the keyed engines'
    own live tests. With a key in the environment, real or left over, five of
    these tests fail only in live runs. None of them touches the network, so
    they clear the keys themselves.
    """
    for provider in PROVIDERS:
        for provider_field in provider.fields:
            monkeypatch.delenv(keystore._env_name(provider_field.key), raising=False)
    keystore._reset_cache()
    yield
    keystore._reset_cache()


def test_the_opt_in_set_is_exactly_the_engines_that_cannot_run_keyless():
    # `codex` and `antigravity` run on the operator's own sign-in rather than
    # a key; they are held to exactly the same rules.
    assert opt_in == {
        "brave_api", "serper", "tavily", "google_cse", "github_code", "codex", "antigravity",
    }  # fmt: skip
    assert opt_in <= set(ENGINES)


def test_nothing_is_configured_in_the_test_environment():
    # conftest scrubs provider keys; if this fails the rest of the file is
    # describing the developer's machine, not the product.
    assert opt_in_engines() == dict.fromkeys(opt_in, False)


@pytest.mark.parametrize(
    "field", ["default_engines", "reserve_engines", "rescue_engines", "fresh_engines"]
)
def test_no_automatic_pool_contains_an_opt_in_engine(field):
    shipped = Settings.model_fields[field].default
    assert shipped, field
    assert not set(shipped) & opt_in, f"{field} would need an API key: {set(shipped) & opt_in}"
    assert set(shipped) <= set(ENGINES), f"{field} names an engine that does not exist"


def test_no_locale_route_contains_an_opt_in_engine():
    for language, names in Settings.model_fields["locale_engines"].default.items():
        assert not set(names) & opt_in, language
        assert set(names) <= set(ENGINES), language


@pytest.mark.parametrize("category", [None, *get_args(Category)])
def test_no_category_routes_to_an_opt_in_engine(category):
    routed = set(engines_for_category(category)) if category else set()
    assert not routed & opt_in, f"category={category!r} routes to {routed & opt_in}"
    for freshness in (None, "day"):
        for query in ("python asyncio taskgroup", "西湖 龙井 采摘 时间"):
            pool = set(_nominal_pool(query, category, freshness))
            assert not pool & opt_in, (category, freshness, query, pool & opt_in)


# ---------------------------------------------------------------------------
# What a model is told when it names one anyway
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("name", sorted(opt_in))
async def test_naming_an_unconfigured_opt_in_engine_points_away_from_keys(name):
    with pytest.raises(EngineKeyError) as excinfo:
        await get_engine(name).search("anything at all", 5)
    message = str(excinfo.value)

    assert message.startswith(f"{name} not configured")
    # In this order: the search is fine, here is what to use, do not ask.
    fine = message.index("Nothing is wrong with the search")
    do_not_ask = message.index("Do not ask the user for an API key")
    operator = message.index("Operator note")
    assert fine < do_not_ask < operator
    # `uv run` is wrong for every real install (uvx, the plugin, the bundle).
    assert "uv run" not in message
    # The operator's half still names the exact knob: a key field, or the
    # sign-in command for an engine that runs on a sign-in.
    if name in oauth.PROVIDERS:
        assert f"search-mcp-login {name}" in message
        return
    provider = next(p for p in PROVIDERS if name == p.engine or name in p.unlocks)
    assert any(field.key in message for field in provider.fields)


async def test_the_aggregator_reports_it_as_an_error_not_as_no_results():
    from search_mcp.aggregator import aggregate_search

    out = await aggregate_search("anything at all", engines=["serper"], use_cache=False)
    assert out["results"] == []
    assert out["errors"]["serper"].startswith("serper not configured")


# ---------------------------------------------------------------------------
# What `engines` shows
# ---------------------------------------------------------------------------


def test_the_engines_tree_lists_only_what_runs_without_a_key():
    from search_mcp.server import engines

    md = engines()
    tree, _, closing = md.rpartition("Every engine above is keyless.")
    assert closing, "the closing line is what frames everything above it"
    for name in opt_in:
        assert f"- `{name}`" not in tree, f"{name} is on the menu"
        assert f"`{name}`" in closing
    assert "API key required" not in md
    assert "do not ask the user for a key" in closing
    assert "- `duckduckgo`" in tree and "- `so360`" in tree


def test_a_configured_opt_in_engine_is_marked_and_still_off_the_menu(monkeypatch):
    from search_mcp import keystore
    from search_mcp.server import engines

    monkeypatch.setenv("SEARCH_MCP_SERPER_API_KEY", "k")
    keystore._reset_cache()

    md = engines(group="web")
    assert "`serper` (configured)" in md
    assert "- `serper`" not in md

    payload = engines(group="web", format="json")
    assert payload["opt_in"]["serper"] is True and payload["opt_in"]["tavily"] is False
    assert payload["needs_api_key"] == sorted(payload["opt_in"])


def test_the_json_engine_list_is_still_the_whole_registry():
    from search_mcp.server import engines

    payload = engines(format="json")
    assert payload["engines"] == list(ENGINES)
    assert "github_code" in payload["needs_api_key"], "its provider is optional; it is not"
    assert isinstance(payload["not_auto_routed"], list)


# ---------------------------------------------------------------------------
# What the server says about itself
# ---------------------------------------------------------------------------


def test_the_registry_entry_and_bundle_ask_for_no_key():
    """Distribution metadata is where a key requirement would first become
    visible to a user: an install dialog with an "API key" field in it."""
    import json
    from pathlib import Path

    root = Path(__file__).resolve().parents[1]
    for path in (root / "server.json", root / "mcpb" / "manifest.json"):
        text = json.dumps(json.loads(path.read_text(encoding="utf-8"))).upper()
        for word in ("API_KEY", "APIKEY", "TOKEN"):
            assert word not in text, f"{path.name} mentions {word}"


# ---------------------------------------------------------------------------
# What the documentation says
# ---------------------------------------------------------------------------

_DOCS = (
    "README.md",
    "docs/USAGE.md",
    "docs/AGENT_USAGE.md",
    "docs/API_KEYS.md",
    "docs/PROXY_AND_GATES.md",
    "plugins/free-search/README.md",
)
# Each of these once sold a key as the better way to run the server, or wired
# up a package name that belongs to someone else.
_RETIRED_PHRASES = (
    "higher reliability",
    "higher quality",
    "Simplest setup",
    "Two of these",
    "Which should I pick",
    "Use configured API keys",
    '"args": ["search-mcp"]',
)


@pytest.mark.parametrize("doc", _DOCS)
def test_the_docs_do_not_sell_keys(doc):
    from pathlib import Path

    text = (Path(__file__).resolve().parents[1] / doc).read_text(encoding="utf-8")
    for phrase in _RETIRED_PHRASES:
        assert phrase not in text, f"{doc} says {phrase!r} again"
