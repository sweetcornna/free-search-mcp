"""Replay a capture through merge variants and score them.

Offline and repeatable: the same `buckets.json` always produces the same
numbers, so a difference between two runs is a difference between two
algorithms and nothing else.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[2] / "src"))

from search_mcp.aggregator import (  # noqa: E402
    _NATIVE_CATEGORY_WEIGHT,
    _RRF_K,
    _dedup_by_title,
    _is_guarded,
    _native_engines,
    _normalize_url,
    _url_key,
)
from search_mcp.coherence import (  # noqa: E402
    COHERENCE_MIN,
    bucket_coherence,
    is_witness,
    looks_like_decoy,
)

HERE = pathlib.Path(__file__).parent
DATA_PATH = HERE / "buckets.json"
FIXTURE_PATH = HERE / "fixtures" / "decoy_and_dupes.json"


def load(path: pathlib.Path = DATA_PATH):
    """The cases of a capture, plus when it was taken.

    Accepts both shapes: the dated `{"captured_at", "cases"}` one, and the bare
    list that captures before 2026-09 were written as.
    """
    if not path.exists():
        raise SystemExit(
            f"{path} not found — run `python evals/ranking/capture.py` first, or pass "
            f"`--data {FIXTURE_PATH.relative_to(HERE.parents[1])}` for the committed slice."
        )
    raw = json.loads(path.read_text(encoding="utf-8"))
    if isinstance(raw, list):
        return raw, "unknown (pre-2026-09 capture)"
    return raw["cases"], raw.get("captured_at", "unknown")


def merge(case, *, k=_RRF_K, native_weight=_NATIVE_CATEGORY_WEIGHT, cap=50):
    """A standalone copy of `_merge`'s scoring, parameterised.

    Deliberately not a call into `_merge`: the point is to compare the shipped
    configuration against alternatives it does not support.
    """
    native = _native_engines(case["category"])
    scores: dict[str, float] = {}
    rep: dict[str, dict] = {}
    engines_for: dict[str, list[str]] = {}
    for name, rows in case["buckets"].items():
        weight = native_weight if name in native else 1.0
        for rank, r in enumerate(rows, 1):
            url = _normalize_url(r["url"])
            if not url:
                continue
            # Keyed on the page's identity, emitted as the URL an engine gave —
            # the same split `_merge` makes, and for the same reason.
            key = _url_key(url)
            scores[key] = scores.get(key, 0.0) + weight / (k + r.get("rank", rank))
            engines_for.setdefault(key, []).append(name)
            if key not in rep:
                rep[key] = dict(r) | {"url": url}
    out = []
    for key, score in sorted(scores.items(), key=lambda kv: -kv[1]):
        rec = rep[key]
        rec["engines"] = sorted(set(engines_for[key]))
        rec["score"] = score
        rec.pop("rank", None)
        rec.pop("engine", None)
        out.append(rec)
    return _dedup_by_title(out)[:cap]


def guard(case) -> tuple[dict, list[tuple[str, float]]]:
    """Apply the aggregator's off-topic rule to one captured case.

    Returns the case without the buckets the guard would drop, and what was
    dropped with its coherence. Same rule as `_drop_decoy_buckets`: a suspect
    goes only when another bucket witnesses that the query's words do get
    echoed.
    """
    buckets = case["buckets"]
    suspects = [
        name for name, rows in buckets.items()
        if _is_guarded(name) and looks_like_decoy(case["query"], rows)
    ]
    witnessed = any(
        name not in suspects and is_witness(case["query"], rows)
        for name, rows in buckets.items()
    )
    if not suspects or not witnessed:
        return case, []
    dropped = [(name, bucket_coherence(case["query"], buckets[name])) for name in suspects]
    kept = {name: rows for name, rows in buckets.items() if name not in suspects}
    return case | {"buckets": kept}, dropped


def rank_of(results, expect):
    for i, r in enumerate(results, 1):
        if expect.lower() in r["url"].lower():
            return i
    return None


def evaluate(data, **kw):
    hit1 = hit3 = 0
    mrr = 0.0
    detail = []
    for case in data:
        pos = rank_of(merge(case, **kw), case["expect"])
        hit1 += pos == 1
        hit3 += bool(pos and pos <= 3)
        mrr += (1.0 / pos) if pos else 0.0
        detail.append((case["query"], case["category"], pos))
    n = len(data) or 1
    return {"hit@1": hit1, "hit@3": hit3, "mrr": round(mrr / n, 4), "n": len(data)}, detail


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--detail", action="store_true",
                    help="per-query before/after table")
    ap.add_argument("--sweep", action="store_true",
                    help="grid over k and the native weight")
    ap.add_argument("--guard", action="store_true",
                    help="report what the off-topic guard drops, and score with it applied; "
                         "exits non-zero if it drops a bucket the capture does not mark as a decoy")
    ap.add_argument("--data", type=pathlib.Path, default=DATA_PATH,
                    help=f"capture to replay (default: {DATA_PATH.name}; the committed slice "
                         f"is {FIXTURE_PATH.relative_to(HERE)})")
    args = ap.parse_args()
    data, captured_at = load(args.data)
    print(f"capture: {args.data.name}, taken {captured_at}, {len(data)} cases")

    if args.guard:
        # The guard has two ways to be wrong and only one is tolerable. Missing
        # a decoy costs a few bad results; dropping a HEALTHY bucket silently
        # halves a search. So the second is an error here, not a statistic.
        unmarked = 0
        guarded = []
        print(f"\noff-topic guard (coherence < {COHERENCE_MIN}, given a witness):")
        for case in data:
            kept, dropped = guard(case)
            guarded.append(kept)
            for name, score in dropped:
                marked = name in (case.get("decoy") or [])
                unmarked += not marked
                flag = "marked decoy" if marked else "NOT MARKED — read this bucket"
                print(f"  drops {name:12} coherence={score:.2f}  {case['query'][:40]!r}  [{flag}]")
        before, _ = evaluate(data)
        after, _ = evaluate(guarded)
        print(f"  without guard: {before}\n  with guard:    {after}")
        if unmarked:
            raise SystemExit(
                f"\n{unmarked} dropped bucket(s) are not marked as decoys. Either the guard is "
                'wrong, or the capture is missing a `"decoy": [...]` annotation — read the '
                "bucket and decide which."
            )
        data = guarded

    shipped, shipped_detail = evaluate(data)
    print(f"shipped (k={_RRF_K}, native_weight={_NATIVE_CATEGORY_WEIGHT}): {shipped}")

    if args.sweep:
        print(f"\n{'k':>6} {'weight':>7} | {'hit@1':>5} {'hit@3':>5} {'MRR':>8}")
        print("-" * 40)
        for k in (60.0, 30.0, 10.0, 5.0):
            for w in (1.0, 1.5, 2.0, 2.5, 3.0, 4.0):
                m, _ = evaluate(data, k=k, native_weight=w)
                print(f"{k:>6} {w:>7} | {m['hit@1']:>5} {m['hit@3']:>5} {m['mrr']:>8.4f}")

    if args.detail:
        # The comparison that gates a change: an aggregate win is not enough,
        # nothing individual may get worse.
        _, before = evaluate(data, native_weight=1.0)
        print(f"\n{'plain':>7}{'shipped':>8}   category            query")
        improved = regressed = 0
        for (q, c, a), (_, _, b) in zip(before, shipped_detail, strict=True):
            av, bv = (a or 999), (b or 999)
            tag = ""
            if bv < av:
                tag, improved = "  improved", improved + 1
            elif bv > av:
                tag, regressed = "  REGRESSED", regressed + 1
            print(f"{str(a):>7}{str(b):>8}   {str(c):18}  {q[:36]}{tag}")
        print(f"\nimproved={improved}  regressed={regressed}")


if __name__ == "__main__":
    main()
