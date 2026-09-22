"""Capture the raw per-engine results for the eval queries, once.

Replaying a saved capture is what isolates the MERGE from network variance.
Hits the network; run it directly, not under pytest.
"""

from __future__ import annotations

import asyncio
import datetime as dt
import json
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[2] / "src"))

from search_mcp.aggregator import _nominal_pool  # noqa: E402
from search_mcp.engines import SearchFilters, category_group, get_engine  # noqa: E402

OUT = pathlib.Path(__file__).parent / "buckets.json"

# (query, category, substring identifying the one correct result[, freshness])
CASES = [
    ("reciprocal rank fusion", "paper", "10.1145/1571941.1572114"),
    ("attention is all you need", "paper", "1706.03762"),
    ("deep residual learning for image recognition", "paper", "10.1109/cvpr.2016.90"),
    # PubMed and Europe PMC are equally right here; demanding one of them
    # measured an engine outage rather than the ranking.
    ("crispr base editing", "paper.biomed", "ncbi.nlm.nih.gov"),
    ("reciprocal rank fusion", "paper.cs", "10.1145/1571941.1572114"),
    ("NVDA risk factors", "finance.filings", "sec.gov/Archives"),
    ("NVIDIA 10-K risk factors", "finance.filings", "sec.gov/Archives"),
    ("人工智能", "finance.filings", "static.cninfo.com.cn"),
    ("vietnam gdp growth", "finance.macro", "imf.org/external/datamapper"),
    ("semaglutide obesity", "paper.trial", "clinicaltrials.gov/study"),
    ("python asyncio", None, "docs.python.org"),
    ("rust ownership borrowing", None, "doc.rust-lang.org"),
    ("postgres explain analyze", None, "postgresql.org/docs"),
    ("kubernetes operator pattern", None, "kubernetes.io/docs"),
    # Chinese, segmented and not: the locale route (so360) and the CJK half of
    # the coherence measure only ever see queries like these.
    ("西湖 龙井 采摘 时间", None, "zhihu.com"),
    ("机甲大师高校联盟赛规则手册", None, "robomaster.com"),
    # A recency query: the only kind that seats the news feed in the pool.
    ("federal reserve rate decision", None, "federalreserve.gov", "week"),
]

N = 10


async def main() -> None:
    cases = []
    for query, category, expect, *rest in CASES:
        freshness = rest[0] if rest else None
        # The pool a real search is entitled to — imported, not re-derived. The
        # hand-copied routing this replaces had already drifted from the
        # aggregator once. Nominal on purpose: a capture should record what a
        # benched engine returns too, since that is the evidence for benching.
        names = _nominal_pool(query, category, freshness)
        filters = SearchFilters(
            freshness=freshness, category=category_group(category), category_token=category
        )
        diagnostics: dict = {}

        # Bind the loop variables explicitly: a closure over them would have
        # every engine search for whatever query the loop had reached by the
        # time it ran.
        async def run(name: str, query=query, filters=filters, diagnostics=diagnostics):
            try:
                return name, await get_engine(name).search(query, N, filters, diagnostics)
            except Exception as exc:  # a dead engine is data, not a crash
                print(f"    {name}: {type(exc).__name__}", file=sys.stderr)
                return name, []

        got = await asyncio.gather(*(run(n) for n in names))
        buckets = {
            name: [r.to_dict() | {"rank": r.rank, "engine": r.engine} for r in rows]
            for name, rows in got
        }
        total = sum(len(v) for v in buckets.values())
        print(f"{query!r:46} cat={str(category):18} engines={len(names)} raw={total}")
        cases.append(
            {
                "query": query,
                "category": category,
                "freshness": freshness,
                "expect": expect,
                "engines": names,
                # Why an engine's bucket is empty: a wall is not "no results".
                "gated": diagnostics.get("gated") or {},
                # Filled in BY HAND after reading the output: the engines whose
                # bucket is a decoy. `replay.py --guard` fails on any bucket it
                # would drop that is not listed here.
                "decoy": [],
                "buckets": buckets,
            }
        )
    out = {
        # A capture is a snapshot of a moving web. Every conclusion drawn from
        # one should be able to say which day's web it was.
        "captured_at": dt.datetime.now(dt.UTC).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "cases": cases,
    }
    OUT.write_text(json.dumps(out, ensure_ascii=False))
    print(f"\nwrote {OUT} ({OUT.stat().st_size // 1024} KB)")


if __name__ == "__main__":
    asyncio.run(main())
