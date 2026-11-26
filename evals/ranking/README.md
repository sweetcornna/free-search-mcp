# Ranking evaluation

`fixtures/decoy_and_dupes.json` is a dated slice of real engine output: three
cases where Bing answered HTTP 200 with ten well-formed results about only the
first word of the query, plus one paper that arrived as both `http://` and
`https://`. It is committed because it is evidence; the live web it was
captured from keeps moving.

Replay it offline through the merge with the built-in command:

```bash
free-search-mcp eval-replay                       # default fixture set
free-search-mcp eval-replay path/to/capture.json  # your own capture
```

`crates/search/src/eval_replay.rs` holds the scoring and the regression guard
(the numbers the Python 0.18.2 merge produced on this fixture); the
`eval_replay` tests in `crates/search` and `crates/server` run it in CI.
