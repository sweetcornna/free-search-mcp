"""The aggregator's off-topic guard: drop a decoy bucket, and only a decoy.

Two failure directions, and the second is the one that would be unforgivable:
letting a decoy through costs three bad results in ten; dropping a healthy
bucket silently halves the search. So most of these tests are about when the
guard must NOT act.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from search_mcp import aggregator
from search_mcp.aggregator import _drop_decoy_buckets, _gate_hint, _is_guarded, aggregate_search
from search_mcp.config import settings
from search_mcp.engines import ENGINES
from search_mcp.engines.base import SearchResult

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.

FIXTURE = Path(__file__).resolve().parents[1] / "evals/ranking/fixtures/decoy_and_dupes.json"
QUERY = "rust ownership borrowing"


def _bucket(engine: str, rows: list[tuple[str, str]], host: str | None = None) -> list[SearchResult]:
    return [
        SearchResult(
            title=title,
            url=f"https://{host or engine}.example/{i}",
            snippet=snippet,
            engine=engine,
            rank=i + 1,
        )
        for i, (title, snippet) in enumerate(rows)
    ]


def decoy(engine: str, n: int = 8) -> list[SearchResult]:
    return _bucket(engine, [("Rust on Steam", "The only aim in Rust is to survive.")] * n)


def healthy(engine: str, n: int = 8) -> list[SearchResult]:
    return _bucket(engine, [("Understanding Ownership", "Ownership and borrowing in Rust.")] * n)


class _Stub:
    def __init__(self, name, results, *, categories=frozenset(), single_site=False):
        self.name = name
        self.categories = categories
        self.single_site = single_site
        self._results = results
        self.calls = 0

    async def search(self, query, max_results, filters=None, diagnostics=None):
        self.calls += 1
        if diagnostics is not None:
            diagnostics.setdefault("raw_per_engine", {})[self.name] = len(self._results)
            diagnostics.setdefault("after_filter_per_engine", {})[self.name] = len(self._results)
        return list(self._results)


class _Cache:
    def __init__(self):
        self.meta: list = []

    async def get_search(self, key, max_age_seconds=None):
        return None

    async def put_search(self, key, query, engines, results, meta=None):
        self.meta.append(meta)


def _wire(monkeypatch, *stubs: _Stub) -> _Cache:
    registry = {s.name: s for s in stubs}

    def _get(name: str):
        try:
            return registry[name]
        except KeyError:
            raise ValueError(f"unknown engine: {name}") from None

    monkeypatch.setattr(aggregator, "get_engine", _get)
    cache = _Cache()
    monkeypatch.setattr(aggregator, "cache", cache)
    return cache


def _hosts(payload) -> set[str]:
    return {r["url"].split("/")[2] for r in payload["results"]}


# ---------------------------------------------------------------------------
# The case it exists for
# ---------------------------------------------------------------------------


async def test_a_decoy_bucket_is_dropped_and_the_healthy_one_kept(monkeypatch):
    cache = _wire(monkeypatch, _Stub("bing", decoy("bing")), _Stub("duckduckgo", healthy("ddg")))

    out = await aggregate_search(QUERY, engines=["bing", "duckduckgo"])

    assert _hosts(out) == {"ddg.example"}
    assert out["gated_engines"] == {"bing": {"reason": "off_topic", "fallback": None}}
    # The provenance is cached with the results, so a cache hit says it too.
    assert cache.meta[0]["gated_engines"]["bing"]["reason"] == "off_topic"


async def test_the_hint_explains_and_does_not_send_anyone_to_fix_their_network(monkeypatch):
    _wire(monkeypatch, _Stub("bing", decoy("bing")), _Stub("duckduckgo", healthy("ddg")))

    hint = (await aggregate_search(QUERY, engines=["bing", "duckduckgo"]))["gated_hint"]

    assert "match only the first word" in hint
    assert "Configure a proxy" not in hint
    assert "-gated" not in hint and "(no results)" not in hint


def test_a_real_gate_next_to_an_off_topic_engine_keeps_its_own_remedy():
    hint = _gate_hint({"bing": "off_topic", "mojeek": "captcha"}, {})
    assert "mojeek was captcha-gated (no results)" in hint
    assert "Configure a proxy" in hint
    assert "a proxy will not change them" in hint


# ---------------------------------------------------------------------------
# When it must not act
# ---------------------------------------------------------------------------


async def test_a_specialist_that_does_not_echo_the_query_is_left_alone(monkeypatch):
    """SEC EDGAR answers "NVDA risk factors" with filing titles containing
    neither word. Coherence 0.0, every result correct."""
    filings = _bucket("sec_edgar", [("10-K 2026-02-21", "Annual report")] * 8)
    _wire(
        monkeypatch,
        _Stub("sec_edgar", filings, categories=frozenset({"finance"})),
        _Stub("duckduckgo", healthy("ddg")),
    )

    out = await aggregate_search(QUERY, engines=["sec_edgar", "duckduckgo"])

    assert _hosts(out) == {"sec_edgar.example", "ddg.example"}
    assert "gated_engines" not in out


async def test_a_single_site_catalogue_is_left_alone(monkeypatch):
    books = _bucket("openlibrary", [("The Rust Book", "by Someone")] * 8)
    _wire(
        monkeypatch,
        _Stub("openlibrary", books, single_site=True),
        _Stub("duckduckgo", healthy("ddg")),
    )
    out = await aggregate_search(QUERY, engines=["openlibrary", "duckduckgo"])
    assert "openlibrary.example" in _hosts(out)


async def test_two_engines_that_agree_are_the_querys_doing_not_a_decoy(monkeypatch):
    """No witness, no verdict. An English query answered in Japanese echoes
    none of its words in ANY engine; dropping every bucket would turn a
    working search into an empty one."""
    _wire(monkeypatch, _Stub("bing", decoy("bing")), _Stub("duckduckgo", decoy("ddg")))

    out = await aggregate_search(QUERY, engines=["bing", "duckduckgo"])

    assert _hosts(out) == {"bing.example", "ddg.example"}
    assert "gated_engines" not in out


async def test_a_grey_bucket_is_neither_dropped_nor_a_witness(monkeypatch):
    # 3 of 8 on topic: 0.375 — above the decoy cut, below the witness bar.
    grey = decoy("ddg", 5) + _bucket("ddg", [("Ownership in Rust", "borrowing")] * 3, host="ddg2")
    _wire(monkeypatch, _Stub("bing", decoy("bing")), _Stub("duckduckgo", grey))

    out = await aggregate_search(QUERY, engines=["bing", "duckduckgo"])

    assert "bing.example" in _hosts(out), "a grey bucket cannot convict another"


@pytest.mark.parametrize("query", ["rust", "rust ownership"])
async def test_a_query_too_short_to_judge_is_never_guarded(monkeypatch, query):
    _wire(monkeypatch, _Stub("bing", decoy("bing")), _Stub("duckduckgo", healthy("ddg")))
    out = await aggregate_search(query, engines=["bing", "duckduckgo"])
    assert "bing.example" in _hosts(out)


async def test_a_bucket_too_small_to_judge_is_never_guarded(monkeypatch):
    _wire(monkeypatch, _Stub("bing", decoy("bing", 3)), _Stub("duckduckgo", healthy("ddg")))
    out = await aggregate_search(QUERY, engines=["bing", "duckduckgo"])
    assert "bing.example" in _hosts(out)


async def test_the_guard_can_be_switched_off(monkeypatch):
    monkeypatch.setattr(settings, "coherence_guard_enabled", False)
    _wire(monkeypatch, _Stub("bing", decoy("bing")), _Stub("duckduckgo", healthy("ddg")))
    out = await aggregate_search(QUERY, engines=["bing", "duckduckgo"])
    assert _hosts(out) == {"bing.example", "ddg.example"}


# ---------------------------------------------------------------------------
# One engine, no peer: a second opinion through rescue
# ---------------------------------------------------------------------------


async def test_a_lone_decoy_gets_a_second_opinion_and_loses(monkeypatch):
    monkeypatch.setattr(settings, "rescue_enabled", True)
    monkeypatch.setattr(settings, "rescue_engines", ["searx"])
    searx = _Stub("searx", healthy("searx"))
    _wire(monkeypatch, _Stub("bing", decoy("bing")), searx)

    out = await aggregate_search(QUERY, engines=["bing"])

    assert searx.calls == 1, "ten decoys are not ten results: rescue runs despite the count"
    assert _hosts(out) == {"searx.example"}
    assert out["rescued_via"] == "searx"
    assert out["gated_engines"] == {"bing": {"reason": "off_topic", "fallback": "searx"}}
    assert "served via searx" in out["gated_hint"]


async def test_a_lone_suspect_is_kept_when_the_second_opinion_agrees_with_it(monkeypatch):
    monkeypatch.setattr(settings, "rescue_enabled", True)
    monkeypatch.setattr(settings, "rescue_engines", ["searx", "so360"])
    searx, so360 = _Stub("searx", decoy("searx")), _Stub("so360", decoy("so360"))
    _wire(monkeypatch, _Stub("bing", decoy("bing")), searx, so360)

    out = await aggregate_search(QUERY, engines=["bing"])

    # Every rescue candidate was tried, none was usable, nothing was convicted.
    assert (searx.calls, so360.calls) == (1, 1)
    assert _hosts(out) == {"bing.example"}
    assert "gated_engines" not in out and "rescued_via" not in out


async def test_a_lone_suspect_is_kept_when_rescue_is_disabled(monkeypatch):
    _wire(monkeypatch, _Stub("bing", decoy("bing")))  # conftest: rescue_enabled=False
    out = await aggregate_search(QUERY, engines=["bing"])
    assert _hosts(out) == {"bing.example"}


async def test_rescue_never_recovers_into_a_decoy(monkeypatch):
    """Public searx instances measured 0.0-0.2 coherence on a bad day."""
    monkeypatch.setattr(settings, "rescue_enabled", True)
    monkeypatch.setattr(settings, "rescue_engines", ["searx", "so360"])
    searx, so360 = _Stub("searx", decoy("searx")), _Stub("so360", healthy("so360"))
    _wire(monkeypatch, _Stub("duckduckgo", []), searx, so360)

    out = await aggregate_search(QUERY, engines=["duckduckgo"], use_cache=False)

    assert out["rescued_via"] == "so360"
    assert _hosts(out) == {"so360.example"}


# ---------------------------------------------------------------------------
# Scope, pinned
# ---------------------------------------------------------------------------


def test_the_guarded_engines_are_exactly_the_web_indexes():
    """Adding an engine with no `categories` makes it guarded by default. That
    is right for a web index and wrong for a catalogue, so the decision has to
    be made on purpose: set `single_site = True`, or add the name here."""
    assert {name for name in ENGINES if _is_guarded(name)} == {
        "duckduckgo", "mojeek", "searx", "startpage", "brave", "bing", "baidu", "google",
        "serpsearch", "anysearch", "sogou", "so360", "brave_api", "serper", "tavily",
        "google_cse", "codex", "antigravity",
    }  # fmt: skip


# ---------------------------------------------------------------------------
# Real data: the committed capture
# ---------------------------------------------------------------------------

_CASES = json.loads(FIXTURE.read_text(encoding="utf-8"))["cases"]


@pytest.mark.parametrize("case", _CASES, ids=[c["query"] for c in _CASES])
def test_on_captured_traffic_the_guard_drops_the_decoys_and_nothing_else(case):
    named = [
        (name, [SearchResult(title=r["title"], url=r["url"], snippet=r.get("snippet", ""),
                             engine=name, rank=i + 1) for i, r in enumerate(rows)])
        for name, rows in case["buckets"].items()
    ]  # fmt: skip
    diagnostics: dict = {}

    kept, unconfirmed = _drop_decoy_buckets(case["query"], named, diagnostics)

    dropped = sorted(set(case["buckets"]) - {name for name, _ in kept})
    assert dropped == sorted(case["decoy"])
    assert unconfirmed == []
    assert diagnostics.get("gated", {}) == dict.fromkeys(case["decoy"], "off_topic")
