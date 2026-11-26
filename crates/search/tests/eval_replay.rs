//! `eval-replay` acceptance: the kernel's ranking on the committed fixture is
//! at least the Python 0.18.2 baseline minus 0.02, with 0 decoy misses and no
//! healthy bucket dropped.

use fsm_search::eval_replay::{self, PYTHON_BASELINE};
use std::path::PathBuf;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ranking/decoy_and_dupes.json")
}

#[test]
fn eval_replay_meets_the_python_baseline() {
    let f = eval_replay::load(&fixture()).unwrap();
    let report = eval_replay::replay(&f);
    let text = report.render(Some(&PYTHON_BASELINE));
    assert!(
        report.check(&PYTHON_BASELINE).is_ok(),
        "{:?}\n{text}",
        report.check(&PYTHON_BASELINE)
    );
    assert_eq!(report.decoy_misses(), 0, "{text}");
    assert_eq!(report.unmarked_drops(), 0, "{text}");
}

#[test]
fn eval_replay_matches_python_case_by_case() {
    // replay.py --guard --detail, 2026-11-25: plain -> shipped per case, and
    // the guard drops exactly the marked bing buckets (coherence 0.00/0.10/0.00).
    let report = eval_replay::replay(&eval_replay::load(&fixture()).unwrap());
    let got: Vec<(Option<usize>, Option<usize>)> =
        report.cases.iter().map(|c| (c.plain, c.shipped)).collect();
    assert_eq!(
        got,
        [
            (Some(4), Some(3)),
            (Some(4), Some(2)),
            (Some(1), Some(1)),
            (Some(1), Some(1))
        ],
        "{}",
        report.render(None)
    );
    let drops: Vec<(String, String)> = report
        .cases
        .iter()
        .flat_map(|c| {
            c.dropped
                .iter()
                .map(|(n, coh, _)| (n.clone(), format!("{coh:.2}")))
        })
        .collect();
    assert_eq!(
        drops,
        [
            ("bing".to_owned(), "0.00".to_owned()),
            ("bing".to_owned(), "0.10".to_owned()),
            ("bing".to_owned(), "0.00".to_owned())
        ]
    );
    assert_eq!(report.with_guard, PYTHON_BASELINE);
    assert_eq!(report.without_guard, PYTHON_BASELINE);
}

#[test]
fn the_gate_fails_on_a_worse_ranking_or_a_missed_decoy() {
    let mut f = eval_replay::load(&fixture()).unwrap();
    // A decoy the guard cannot see (only one bucket left: no witness) is a miss.
    f.cases[2].buckets.retain(|(n, _)| n == "bing");
    let report = eval_replay::replay(&f);
    assert_eq!(report.decoy_misses(), 1);
    let errs = report.check(&PYTHON_BASELINE).unwrap_err();
    assert!(errs.iter().any(|e| e.contains("marked decoy")), "{errs:?}");
    assert!(errs.iter().any(|e| e.starts_with("MRR")), "{errs:?}");
}
