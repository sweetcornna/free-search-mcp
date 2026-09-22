# Ranking evaluation

This harness compares result-merging algorithms on real engine output.

Ranking changes are easy to argue for and hard to get right, so a change to
`aggregator._merge` has to prove itself here first. The harness captures what
the engines return and then replays those same results offline through each
candidate configuration. Keeping the merge away from the network matters,
because a live comparison is contaminated by whichever engine happened to
answer that minute.

## Running it

```bash
# 1. Capture. Hits the network; takes a few minutes. Do this once.
SEARCH_MCP_FETCH_STRATEGY=http uv run python evals/ranking/capture.py

# 2. Replay. Offline, instant, repeatable.
uv run python evals/ranking/replay.py            # add --detail, --sweep, --guard

# No capture yet? The committed slice replays too:
uv run python evals/ranking/replay.py --guard --data evals/ranking/fixtures/decoy_and_dupes.json
```

`capture.py` writes `buckets.json` next to itself and stamps it with
`captured_at`. The file is gitignored, and since 0.12 it is also untracked (it
had been both ignored and tracked). A capture is a snapshot of a web that keeps
changing, and a stale one would make every later measurement wrong without
anyone noticing.

`fixtures/decoy_and_dupes.json` is committed for the opposite reason. It holds
four cases cut from the 2026-08-29 capture, and they record something the live
web may not show again on demand. `tests/test_coherence_guard.py` and
`tests/test_merge_identity.py` replay it in CI.

## The capture is evidence about the engines, too

Read the buckets as well as the scores. The 2026-08-29 capture was used for
three weeks to tune the merge before anyone looked at what was in it. For three
of its fourteen queries, Bing had returned ten well-formed results about the
first word of the query only: the Steam page for "rust ownership borrowing",
the postgresql.org home page for "postgres explain analyze". Every number
measured on that capture was measured with a third of the web pool's input
replaced by noise, and rank fusion had been interleaving that noise into real
answers.

`replay.py --guard` would have caught it. It applies the aggregator's off-topic
rule (`coherence.py`) to every bucket and prints what it would drop and at what
coherence. It exits non-zero if it drops a bucket that the capture does not
list under `"decoy"`. That covers two cases: a decoy nobody has looked at yet,
and a healthy bucket that the guard would wrongly discard. The second case is
the more costly one.

| capture | pool | buckets the guard drops |
|---|---|---|
| 2026-08-29 | duckduckgo, mojeek, googlenews, bing | 3 of 3 Bing buckets on the evaluable no-category queries (coherence 0.00 / 0.00 / 0.10); nothing else |
| 2026-09-21 | duckduckgo, bing, anysearch, mojeek (+ so360 for Chinese, googlenews for `freshness`) | none. After the Bing request-shape fix every web bucket scores 0.7 to 1.0 |

The second row shows that the guard costs a healthy search nothing.

## What it measures

Each case names a query, an optional `category`, and a substring that
identifies the one result a knowledgeable person would call correct. From the
merged list the harness reports:

- hit@1 and hit@3: whether that result was first, or in the top three
- MRR: 1/rank of that result, averaged over cases (0 when it is absent)

A candidate has to improve the aggregate and regress no individual query.
`replay.py --detail` prints the per-query before and after table that shows
this.

## What this set has already settled

Each verdict names the capture it was measured on. Scores from different
captures cannot be compared with each other, because the case list and the web
both changed. Only the before and after within one capture is comparable.

| change | verdict |
|---|---|
| Native-category weight 1.0 → 2.0 | Adopted. 2026-08-29: hit@1 6→8, hit@3 9→13, MRR 0.605→0.747; 6 improved, 0 regressed. Checked again on 2026-09-21 with a decoy-free capture of 17 cases: 3 improved, 0 regressed |
| Off-topic bucket guard | Adopted. 2026-08-29: drops exactly the three decoy buckets, no case regresses. 2026-09-21: drops nothing |
| RRF key ignores scheme / `www.` / default port (`_url_key`) | Adopted. Neutral on both captures' scores; merges the `http://`/`https://` arXiv pair that was being printed twice. Emitting the key as the result URL regressed two cases, so the key is never emitted |
| RRF damping constant `k`, 60 → 5…30 | Rejected. MRR moves less than 0.01 at any value, because ranks are already correlated across engines |
| Lexical query/title overlap bonus | Rejected. Worse at every weight tried (0.747 → 0.645) |
| Stripping tracking parameters before the RRF key | Rejected on this evidence. 81 of 393 results carry one, but none of them collided with a clean copy of the same URL, so no merge changes |

## Caveats worth keeping in mind

The set is small: fourteen queries in the first capture and seventeen in the
second. That is enough to catch a change that breaks something obvious and to
reject the ideas above. It cannot justify a constant tuned to three decimal
places. This is why `_NATIVE_CATEGORY_WEIGHT` is 2.0, a value with a statable
meaning ("one native hit ties two general engines agreeing"), although 2.25
scored marginally higher here.

The expected-result substrings encode a judgement about what "correct" means.
Two of them were wrong on the first pass and were corrected once the output was
read. For example, a `paper.biomed` case demanded Europe PMC when PubMed is
equally right, so it was measuring an engine outage and not a ranking failure.
