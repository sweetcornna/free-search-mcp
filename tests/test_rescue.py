"""Aggregation-level keyless rescue (offline).

Replaces the old per-engine SearXNG fallback tests (test_serp_fallback.py):
the fallback now lives in ONE place — ``aggregate_search`` runs a bounded
rescue pass (settings.rescue_engines) when the fresh run comes back empty or
nearly empty with demonstrably unhealthy engines. These tests stub the engine
registry so no I/O ever happens; the stub names below stand in for the
configured rescue list.
"""

from __future__ import annotations

import asyncio

import pytest

from search_mcp.aggregator import aggregate_search
from search_mcp.config import settings
from search_mcp.engines.base import SearchResult

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.


@pytest.fixture(autouse=True)
def _enable_rescue(monkeypatch):
    """conftest disables rescue suite-wide (offline safety); these tests run it
    against a fully stubbed registry, so re-enable it."""
    monkeypatch.setattr(settings, "rescue_enabled", True)
    # The stub names below; production defaults are configured in config.py.
    monkeypatch.setattr(settings, "rescue_engines", ["searx", "bing"])


def _mk_results(engine: str, n: int, prefix: str = "") -> list[SearchResult]:
    return [
        SearchResult(
            title=f"{prefix}{engine} result {i}",
            url=f"https://{prefix or 'example'}.com/{engine}/{i}",
            snippet="s" * 100,
            engine=engine,
            rank=i + 1,
        )
        for i in range(n)
    ]


class _StubEngine:
    def __init__(self, name, results, *, gate=None, raise_exc=None, delay=0.0):
        self.name = name
        self._results = results
        self._gate = gate
        self._raise = raise_exc
        self._delay = delay
        self.calls = 0

    async def search(self, query, max_results, filters=None, diagnostics=None):
        self.calls += 1
        if self._delay:
            await asyncio.sleep(self._delay)
        if self._raise is not None:
            raise self._raise
        if diagnostics is not None:
            diagnostics.setdefault("raw_per_engine", {})[self.name] = len(self._results)
            diagnostics.setdefault("after_filter_per_engine", {})[self.name] = len(
                self._results
            )
            if self._gate and not self._results:
                diagnostics.setdefault("gated", {})[self.name] = self._gate
        return list(self._results)


class _StubCache:
    def __init__(self):
        self.put_calls: list = []
        self.meta_calls: list = []

    async def get_search(self, key, max_age_seconds=None):
        return None

    async def put_search(self, key, query, engines, results, meta=None):
        self.put_calls.append((key, query, engines, results))
        self.meta_calls.append(meta)


def _wire(monkeypatch, engines: dict[str, _StubEngine]) -> _StubCache:
    def _get(name: str):
        try:
            return engines[name]
        except KeyError:
            raise ValueError(f"unknown engine: {name}") from None

    monkeypatch.setattr("search_mcp.aggregator.get_engine", _get)
    stub_cache = _StubCache()
    monkeypatch.setattr("search_mcp.aggregator.cache", stub_cache)
    return stub_cache


async def test_zero_results_triggers_rescue_and_caches(monkeypatch):
    searx = _StubEngine("searx", _mk_results("searx", 4, prefix="rescue"))
    stub_cache = _wire(
        monkeypatch,
        {
            "duckduckgo": _StubEngine("duckduckgo", [], gate="captcha"),
            "searx": searx,
        },
    )
    out = await aggregate_search("q", engines=["duckduckgo"])
    assert len(out["results"]) == 4
    assert all(r["engines"] == ["searx"] for r in out["results"])
    assert out["rescued_via"] == "searx"
    assert searx.calls == 1
    # Rescued results are cached under the original pool's key.
    assert len(stub_cache.put_calls) == 1
    assert len(stub_cache.put_calls[0][3]) == 4
    # Gate diagnostics survive alongside the rescue.
    assert out["gated_engines"]["duckduckgo"]["reason"] == "captcha"


async def test_sparse_unhealthy_run_merges_rescue_results(monkeypatch):
    searx = _StubEngine("searx", _mk_results("searx", 5, prefix="rescue"))
    _wire(
        monkeypatch,
        {
            "alpha": _StubEngine("alpha", _mk_results("alpha", 2)),
            "beta": _StubEngine("beta", [], gate="captcha"),  # unhealthy signal
            "searx": searx,
        },
    )
    out = await aggregate_search("q", engines=["alpha", "beta"], use_cache=False)
    assert out["rescued_via"] == "searx"
    urls = [r["url"] for r in out["results"]]
    # Both the partial default results and the rescue results survived the merge.
    assert any("/alpha/" in u for u in urls)
    assert any("/searx/" in u for u in urls)


async def test_sparse_but_healthy_run_does_not_rescue(monkeypatch):
    """The normal-path latency guarantee: 2 legit results, all engines healthy
    → the rescue engine must never be called."""
    searx = _StubEngine("searx", _mk_results("searx", 5))
    _wire(
        monkeypatch,
        {
            "alpha": _StubEngine("alpha", _mk_results("alpha", 2)),
            "searx": searx,
        },
    )
    out = await aggregate_search("nichequery", engines=["alpha"], use_cache=False)
    assert len(out["results"]) == 2
    assert "rescued_via" not in out
    assert searx.calls == 0


async def test_no_self_rescue_when_caller_asked_for_rescue_engine(monkeypatch):
    searx = _StubEngine("searx", [])
    bing = _StubEngine("bing", [])
    _wire(monkeypatch, {"searx": searx, "bing": bing})
    out = await aggregate_search("q", engines=["searx", "bing"], use_cache=False)
    assert out["results"] == []
    assert "rescued_via" not in out
    # Each ran once as a requested engine; the rescue pass never re-ran them.
    assert searx.calls == 1
    assert bing.calls == 1


async def test_rescue_engine_failure_is_swallowed(monkeypatch):
    searx = _StubEngine("searx", [], raise_exc=RuntimeError("instances dead"))
    bing = _StubEngine("bing", _mk_results("bing", 3, prefix="rescue"))
    _wire(
        monkeypatch,
        {
            "duckduckgo": _StubEngine("duckduckgo", []),
            "searx": searx,
            "bing": bing,
        },
    )
    out = await aggregate_search("q", engines=["duckduckgo"], use_cache=False)
    # searx blew up; the pass moved on to bing.
    assert out["rescued_via"] == "bing"
    assert len(out["results"]) == 3


async def test_all_rescue_engines_fail_returns_clean_empty(monkeypatch):
    _wire(
        monkeypatch,
        {
            "duckduckgo": _StubEngine("duckduckgo", []),
            "searx": _StubEngine("searx", [], raise_exc=RuntimeError("dead")),
            "bing": _StubEngine("bing", [], raise_exc=RuntimeError("dead")),
        },
    )
    out = await aggregate_search("q", engines=["duckduckgo"], use_cache=False)
    assert out["results"] == []
    assert "rescued_via" not in out


async def test_rescue_timeout_is_bounded(monkeypatch):
    monkeypatch.setattr(settings, "rescue_timeout", 0.05)
    searx = _StubEngine("searx", _mk_results("searx", 3), delay=0.5)
    _wire(
        monkeypatch,
        {
            "duckduckgo": _StubEngine("duckduckgo", []),
            "searx": searx,
        },
    )
    out = await aggregate_search("q", engines=["duckduckgo"], use_cache=False)
    assert out["results"] == []
    assert "rescued_via" not in out


async def test_rescue_disabled_by_setting(monkeypatch):
    monkeypatch.setattr(settings, "rescue_enabled", False)
    searx = _StubEngine("searx", _mk_results("searx", 3))
    _wire(
        monkeypatch,
        {
            "duckduckgo": _StubEngine("duckduckgo", []),
            "searx": searx,
        },
    )
    out = await aggregate_search("q", engines=["duckduckgo"], use_cache=False)
    assert out["results"] == []
    assert searx.calls == 0


# ---------------------------------------------------------------------------
# Engine-level behavior after the per-engine fallback removal
# ---------------------------------------------------------------------------

async def test_bing_http_raise_propagates_like_any_engine(monkeypatch):
    """bing has no swallow-everything wrapper anymore: a raise surfaces to the
    aggregator's errors map (visible, and enough to trigger rescue) instead of
    masquerading as a 'silent zero' the empty-hint would mislabel 'no error'."""
    from unittest.mock import AsyncMock

    import pytest as _pytest
    from curl_cffi.requests.exceptions import RequestException

    from search_mcp.engines.bing import BingEngine

    engine = BingEngine()
    monkeypatch.setattr(
        engine, "_fetch", AsyncMock(side_effect=RequestException("non-200 shell"))
    )
    with _pytest.raises(RequestException):
        await engine.search("anything", 5, diagnostics={})


async def test_three_results_with_gate_still_rescues(monkeypatch):
    """Rescue and the sparse-warning hints share the <=3 threshold: a run must
    never be 'sparse enough to warn about' yet 'too healthy to rescue'."""
    searx = _StubEngine("searx", _mk_results("searx", 5, prefix="rescue"))
    _wire(
        monkeypatch,
        {
            "alpha": _StubEngine("alpha", _mk_results("alpha", 3)),
            "beta": _StubEngine("beta", [], gate="captcha"),
            "searx": searx,
        },
    )
    out = await aggregate_search("q", engines=["alpha", "beta"], use_cache=False)
    assert out["rescued_via"] == "searx"
    # The recovery is attributed to the gated engine in the payload.
    assert out["gated_engines"]["beta"]["fallback"] == "searx"
    assert "served via searx" in out["gated_hint"]


async def test_failed_rescue_probe_does_not_pollute_caller_diagnostics(monkeypatch):
    """A rescue engine that comes back empty (or gated) is an internal probe:
    it must not appear in empty_engines/gated_engines, which describe the
    engines the CALLER asked for."""
    searx = _StubEngine("searx", [], gate="no_live_instance")
    bing = _StubEngine("bing", _mk_results("bing", 2, prefix="rescue"))
    _wire(
        monkeypatch,
        {
            "duckduckgo": _StubEngine("duckduckgo", []),
            "searx": searx,
            "bing": bing,
        },
    )
    out = await aggregate_search("q", engines=["duckduckgo"], use_cache=False)
    assert out["rescued_via"] == "bing"
    assert "searx" not in out.get("empty_engines", [])
    assert "searx" not in (out.get("gated_engines") or {})
    # The probe's outcome is still visible under the rescue summary for debugging.
    assert out.get("gated_engines") is None or "searx" not in out["gated_engines"]


async def test_rescue_provenance_survives_the_cache_round_trip(monkeypatch):
    """A cache hit must not silently re-label a rescued result set as normal.

    The rescue path writes its results into the cache under the ORIGINAL engine
    list, but the cache-hit branch only ever rebuilt `query/engines/cached/
    results/lead_snippet`. So the first query reported "duckduckgo was
    captcha-gated -> served via searx" and the second identical query, any time
    inside the 7-day TTL, reported a plain search over the same URLs. That is
    provenance describing the RESULTS, not a statistic about the run, so it has
    to be stored and replayed with them.
    """
    from search_mcp import aggregator as agg

    gated = _StubEngine("duckduckgo", results=[], gate="captcha")
    rescuer = _StubEngine("searx", results=_mk_results("searx", 1))
    stub_cache = _wire(monkeypatch, {"duckduckgo": gated, "searx": rescuer})
    monkeypatch.setattr(agg.settings, "rescue_enabled", True)
    monkeypatch.setattr(agg.settings, "rescue_engines", ["searx"])

    fresh = await agg.aggregate_search(
        "q", engines=["duckduckgo"], max_results=3, use_cache=True
    )
    assert fresh["rescued_via"] == "searx"
    assert fresh["gated_engines"]["duckduckgo"]["reason"] == "captcha"

    # The provenance was handed to the cache alongside the results.
    assert stub_cache.meta_calls, "put_search must receive the provenance meta"
    meta = stub_cache.meta_calls[-1]
    assert meta["rescued_via"] == "searx"
    assert meta["gated_engines"]["duckduckgo"]["fallback"] == "searx"

    # ...and replaying that row reproduces it.
    key, _query, _engines, results = stub_cache.put_calls[-1]

    async def _hit(k, max_age_seconds=None):
        return (results, meta) if k == key else None

    monkeypatch.setattr(stub_cache, "get_search", _hit)
    cached = await agg.aggregate_search(
        "q", engines=["duckduckgo"], max_results=3, use_cache=True
    )
    assert cached["cached"] is True
    assert cached["rescued_via"] == "searx"
    assert cached["gated_engines"]["duckduckgo"]["reason"] == "captcha"
    assert "served via searx" in cached["gated_hint"]


# --- exclusive categories must never rescue into the general web pool -------


@pytest.mark.parametrize("category", ["image", "dataset", "image.photo"])
def test_exclusive_categories_do_not_rescue(category):
    """`settings.rescue_engines` IS the general web pool, and the whole reason
    `image`/`dataset` replace that pool is that a web engine cannot return an
    image file or a dataset record. Rescuing into it hands back the wrong media
    type — HTML pages `fetch(inline=True)` cannot render — under a header
    saying the search succeeded."""
    from search_mcp.aggregator import _needs_rescue

    assert _needs_rescue([], {"openverse": "boom"}, {}, category) is False


@pytest.mark.parametrize("category", [None, "paper", "news", "finance.filings"])
def test_augmenting_categories_still_rescue(category):
    """Those keep the web pool in the request, so a web engine standing in for
    a gated web engine is a substitution of like for like."""
    from search_mcp.aggregator import _needs_rescue

    assert _needs_rescue([], {"duckduckgo": "boom"}, {}, category) is True


# ---------------------------------------------------------------------------
# The pass is a staggered race, not a queue
# ---------------------------------------------------------------------------


async def test_a_slow_first_candidate_does_not_hold_a_fast_second_one(monkeypatch):
    import time

    monkeypatch.setattr(settings, "rescue_timeout", 5.0)
    monkeypatch.setattr(settings, "rescue_stagger_seconds", 0.1)
    searx = _StubEngine("searx", _mk_results("searx", 3), delay=3.0)
    bing = _StubEngine("bing", _mk_results("bing", 3, prefix="rescue"), delay=0.05)
    _wire(monkeypatch, {"duckduckgo": _StubEngine("duckduckgo", []), "searx": searx,
                        "bing": bing})  # fmt: skip
    started = time.monotonic()
    out = await aggregate_search("q", engines=["duckduckgo"], use_cache=False)
    took = time.monotonic() - started
    assert out["rescued_via"] == "bing"
    # One stagger plus bing's own time, where a queue would have spent half
    # the 5 s budget on searx first.
    assert took < 1.0, f"{took:.2f} s"


async def test_the_winner_cancels_the_candidates_still_running(monkeypatch):
    monkeypatch.setattr(settings, "rescue_stagger_seconds", 0.0)
    cancelled: list[str] = []

    class _Slow(_StubEngine):
        async def search(self, query, max_results, filters=None, diagnostics=None):
            try:
                return await super().search(query, max_results, filters, diagnostics)
            except asyncio.CancelledError:
                cancelled.append(self.name)
                raise

    _wire(monkeypatch, {"duckduckgo": _StubEngine("duckduckgo", []),
                        "searx": _Slow("searx", _mk_results("searx", 3), delay=5.0),
                        "bing": _StubEngine("bing", _mk_results("bing", 3, prefix="r"))})  # fmt: skip
    out = await aggregate_search("q", engines=["duckduckgo"], use_cache=False)
    assert out["rescued_via"] == "bing"
    await asyncio.sleep(0)
    assert cancelled == ["searx"]


async def test_a_failed_candidate_starts_the_next_without_waiting_out_the_stagger(monkeypatch):
    import time

    monkeypatch.setattr(settings, "rescue_stagger_seconds", 5.0)
    searx = _StubEngine("searx", [], raise_exc=RuntimeError("dead"))
    bing = _StubEngine("bing", _mk_results("bing", 3, prefix="rescue"))
    _wire(monkeypatch, {"duckduckgo": _StubEngine("duckduckgo", []), "searx": searx,
                        "bing": bing})  # fmt: skip
    started = time.monotonic()
    out = await aggregate_search("q", engines=["duckduckgo"], use_cache=False)
    assert out["rescued_via"] == "bing"
    assert time.monotonic() - started < 1.0


async def test_a_quick_first_candidate_spares_the_others_a_request(monkeypatch):
    monkeypatch.setattr(settings, "rescue_stagger_seconds", 1.0)
    searx = _StubEngine("searx", _mk_results("searx", 3), delay=0.01)
    bing = _StubEngine("bing", _mk_results("bing", 3, prefix="rescue"))
    _wire(monkeypatch, {"duckduckgo": _StubEngine("duckduckgo", []), "searx": searx,
                        "bing": bing})  # fmt: skip
    out = await aggregate_search("q", engines=["duckduckgo"], use_cache=False)
    assert out["rescued_via"] == "searx"
    assert bing.calls == 0


async def test_answers_in_the_same_tick_go_to_the_earlier_candidate(monkeypatch):
    monkeypatch.setattr(settings, "rescue_stagger_seconds", 0.0)
    _wire(monkeypatch, {"duckduckgo": _StubEngine("duckduckgo", []),
                        "searx": _StubEngine("searx", _mk_results("searx", 3)),
                        "bing": _StubEngine("bing", _mk_results("bing", 3, prefix="r"))})  # fmt: skip
    out = await aggregate_search("q", engines=["duckduckgo"], use_cache=False)
    assert out["rescued_via"] == "searx"


async def test_candidates_still_running_at_the_deadline_are_benched(monkeypatch):
    from search_mcp.health import engine_health

    monkeypatch.setattr(settings, "rescue_timeout", 0.1)
    monkeypatch.setattr(settings, "rescue_stagger_seconds", 0.0)
    _wire(monkeypatch, {"duckduckgo": _StubEngine("duckduckgo", []),
                        "searx": _StubEngine("searx", _mk_results("searx", 3), delay=2.0),
                        "bing": _StubEngine("bing", _mk_results("bing", 3), delay=2.0)})  # fmt: skip
    out = await aggregate_search("q", engines=["duckduckgo"], use_cache=False)
    assert out["results"] == []
    # A timeout is a soft failure: the first one only counts a strike.
    assert not engine_health.is_open("searx")
    await aggregate_search("q2", engines=["duckduckgo"], use_cache=False)
    assert engine_health.is_open("searx") and engine_health.is_open("bing")


async def test_no_more_than_two_candidates_run_at_once(monkeypatch):
    monkeypatch.setattr(settings, "rescue_timeout", 0.3)
    monkeypatch.setattr(settings, "rescue_stagger_seconds", 0.0)
    monkeypatch.setattr(settings, "rescue_engines", ["a", "b", "c", "d"])
    stubs = {name: _StubEngine(name, _mk_results(name, 3), delay=5.0) for name in "abcd"}
    _wire(monkeypatch, {"duckduckgo": _StubEngine("duckduckgo", []), **stubs})
    out = await aggregate_search("q", engines=["duckduckgo"], use_cache=False)
    assert out["results"] == []
    assert [name for name, stub in stubs.items() if stub.calls] == ["a", "b"]
