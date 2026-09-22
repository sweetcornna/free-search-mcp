"""Live canary: are the default engines returning results ABOUT the query?

Gated on SEARCH_MCP_TEST_NETWORK=1. The older live tests assert "got some
results", and Bing passed that for weeks while serving ten well-formed results
about the first word of the query. This asserts relevance instead, with the
same coherence measure the aggregator's guard uses.

Two details matter:

  * The queries are built from today's date. Bing caches per query
    server-side, so a fixed query, once answered correctly, keeps being
    answered correctly whatever the request looks like — a fixed canary goes
    green and stays green through the very regression it is for.
  * A failure here is information about the live web, not necessarily a bug in
    this repo: it says an engine's request shape has stopped working and
    `engines/<name>.py` needs re-measuring.
"""
from __future__ import annotations

import datetime as dt
import os

import pytest

from search_mcp.coherence import bucket_coherence

NETWORK = os.environ.get("SEARCH_MCP_TEST_NETWORK") == "1"
pytestmark = pytest.mark.skipif(not NETWORK, reason="set SEARCH_MCP_TEST_NETWORK=1 to run")

# Multi-word technical topics: enough content words for coherence to be
# evaluable, specific enough that a first-word decoy is unmistakable.
_TOPICS = [
    "sqlite wal checkpoint starvation readers",
    "tokio select cancellation safety pitfalls",
    "terraform state locking dynamodb migration",
    "postgres vacuum freeze wraparound monitoring",
    "kubernetes pod disruption budget eviction",
    "rust async trait object safety workaround",
    "python asyncio taskgroup exception handling",
    "nginx upstream keepalive connection reuse",
    "git rebase autosquash fixup workflow",
    "linux cgroup memory pressure stall information",
]
_SUFFIXES = ["explained", "guide", "examples", "troubleshooting", "best practices", "tutorial",
             "internals"]  # fmt: skip


def _todays_query(offset: int = 0) -> str:
    day = dt.date.today().toordinal() + offset
    return f"{_TOPICS[day % len(_TOPICS)]} {_SUFFIXES[(day // len(_TOPICS)) % len(_SUFFIXES)]}"


@pytest.mark.parametrize("engine_name", ["duckduckgo", "bing", "anysearch"])
async def test_a_default_engine_answers_the_whole_query(engine_name):
    from search_mcp.engines import get_engine

    query = _todays_query()
    results = await get_engine(engine_name).search(query, 10)

    assert len(results) >= 5, f"{engine_name} returned {len(results)} results for {query!r}"
    score = bucket_coherence(query, results)
    assert score is not None and score >= 0.5, (
        f"{engine_name} looks off-topic for {query!r}: coherence={score}. "
        f"Titles: {[r.title for r in results[:5]]}"
    )


async def test_the_default_pool_end_to_end():
    from search_mcp.aggregator import aggregate_search

    query = _todays_query(offset=3)
    out = await aggregate_search(query, use_cache=False)

    assert len(out["results"]) >= 5
    score = bucket_coherence(query, out["results"])
    assert score is not None and score >= 0.7, (query, score)
    # Whatever happened to each engine, the payload accounts for it.
    asked = set(out["engines"])
    contributed = {e for r in out["results"] for e in r["engines"]}
    unexplained = (
        asked
        - contributed
        - set(out.get("gated_engines") or {})
        - set(out.get("errors") or {})
        - set(out.get("empty_engines") or [])
        - set(out.get("refused_engines") or {})
        - set(out.get("rate_limited_engines") or [])
    )
    assert len(contributed) >= 2, f"the answer rests on one index: {contributed}"
    assert not unexplained or len(out["results"]) > 3, unexplained
    assert out["retrieved_at"] and out["usage_note"]
