//! `eval-replay`: score the kernel's ranking on captured ranking fixtures
//! (`evals/ranking/fixtures/*.json`), offline and repeatable (port of
//! `evals/ranking/replay.py --guard --detail`).
//!
//! The kernel's own pieces do the work — [`fuse`] (weighted RRF, URL identity,
//! title dedup) and the coherence guard — so a ranking regression in the
//! kernel shows up here. The fixtures name Python 0.18 engines, so whether a
//! bucket is "native" to a case's category and whether it is guarded comes
//! from [`ENGINE_TABLE`], the Python engine table as shipped in 0.18.2.
//!
//! The library never prints: the CLI (`free-search-mcp eval-replay`, lane
//! W6a) renders [`Report::render`] and exits non-zero on [`Report::check`].

use crate::coherence::{bucket_coherence, is_witness, looks_like_decoy};
use crate::rank::{NATIVE_WEIGHT, RRF_K, Weighting, fuse};
use fsm_core::{FsmError, Hit};
use serde_json::Value;
use std::path::Path;

/// Results scored per case (`replay.py`'s `cap`).
pub const CAP: usize = 50;
/// Tolerance of the acceptance gate (`SPEC.md` §5: `>= Python - 0.02`).
pub const TOLERANCE: f64 = 0.02;

/// One Python engine: name, categories, guarded (a general web index).
pub type EngineRow = (&'static str, &'static [&'static str], bool);

/// The Python 0.18.2 engine table (`ENGINES`: `categories`, `_is_guarded`),
/// generated 2026-11-25 from the v0.18.2 tree.
pub const ENGINE_TABLE: &[EngineRow] = &[
    ("duckduckgo", &[], true),
    ("googlenews", &["news"], false),
    ("brave", &[], true),
    ("bing", &[], true),
    ("yahoojp", &[], true),
    ("yahoo", &[], true),
    ("seznam", &[], true),
    ("baidu", &[], true),
    ("anysearch", &[], true),
    ("youcom", &[], true),
    ("bilibili", &[], false),
    ("zhihu", &[], false),
    ("arxiv", &["paper", "paper.preprint"], false),
    ("openalex", &["paper", "paper.index"], false),
    (
        "europepmc",
        &[
            "paper",
            "paper.biomed",
            "paper.openaccess",
            "paper.preprint",
        ],
        false,
    ),
    ("crossref", &["paper", "paper.index"], false),
    ("pubmed", &["paper", "paper.biomed"], false),
    ("semanticscholar", &["paper", "paper.index"], false),
    ("dblp", &["paper", "paper.cs"], false),
    ("doaj", &["paper", "paper.openaccess"], false),
    ("clinicaltrials", &["paper", "paper.trial"], false),
    ("zbmath", &["paper", "paper.math"], false),
    ("github", &["github"], false),
    ("stackexchange", &["forum"], false),
    ("hackernews", &["forum"], false),
    ("wikipedia", &[], false),
    ("openlibrary", &[], false),
    ("gdelt", &["news", "news.world"], false),
    ("openverse", &["image"], false),
    ("wikimedia", &["image"], false),
    ("dryad", &["dataset", "dataset.repository"], false),
    ("huggingface", &["dataset", "dataset.ml"], false),
    ("dataeuropa", &["dataset", "dataset.gov"], false),
    ("dataverse", &["dataset", "dataset.repository"], false),
    ("zenodo", &["dataset", "dataset.repository"], false),
    ("figshare", &["dataset", "dataset.repository"], false),
    ("so360", &[], true),
    ("quark", &[], true),
    ("naver", &[], true),
    ("sec_edgar", &["finance", "finance.filings"], false),
    ("yahoofinance", &["finance", "finance.market"], false),
    ("worldbank", &["finance", "finance.macro"], false),
    ("cninfo", &["finance", "finance.filings"], false),
    ("imf", &["finance", "finance.macro"], false),
    ("endoflife", &["software", "software.lifecycle"], false),
    ("github_releases", &["software", "software.github"], false),
    ("pypi", &["software", "software.python"], false),
    ("npm", &["software", "software.node"], false),
    ("crates", &["software", "software.rust"], false),
    ("registries", &["software", "software.registry"], false),
    ("appstore", &["software", "software.app"], false),
    ("nvd", &["security", "security.cve"], false),
    ("osv", &["security", "security.package"], false),
    ("cisakev", &["security", "security.exploited"], false),
    ("wikidata", &["reference"], false),
    ("rdap", &["reference", "reference.domain"], false),
    ("openmeteo", &["weather"], false),
    ("mdn", &["docs", "docs.web"], false),
    ("ietf", &["docs", "docs.rfc"], false),
    ("wdi", &["stats", "stats.indicator"], false),
    ("holidays", &["calendar", "calendar.holidays"], false),
    ("worldclock", &["calendar", "calendar.clock"], false),
    ("frankfurter", &["finance", "finance.fx"], false),
    ("cfets", &["finance", "finance.fx"], false),
    ("gleif", &["finance", "finance.entity"], false),
    ("coingecko", &["finance", "finance.crypto"], false),
    ("federalregister", &["gov", "gov.us"], false),
    ("govuk", &["gov", "gov.uk"], false),
    ("brave_api", &[], true),
    ("serper", &[], true),
    ("serpbase", &[], true),
    ("tavily", &[], true),
    ("google_cse", &[], true),
    ("github_code", &["github"], false),
    ("codex", &[], true),
    ("antigravity", &[], true),
];

fn engine(name: &str) -> Option<&'static EngineRow> {
    ENGINE_TABLE.iter().find(|(n, ..)| *n == name)
}

/// `_is_guarded` for a fixture bucket name (unknown names are not guarded).
pub fn is_guarded(name: &str) -> bool {
    engine(name).is_some_and(|(_, _, g)| *g)
}

/// `_native_engines(category)`: buckets whose engine declares the category.
fn native(name: &str, category: Option<&str>) -> bool {
    category.is_some_and(|c| engine(name).is_some_and(|(_, cats, _)| cats.contains(&c)))
}

/// One captured case.
#[derive(Debug, Clone)]
pub struct Case {
    pub query: String,
    pub category: Option<String>,
    /// Case-insensitive substring of the URL that answers the query.
    pub expect: String,
    /// Buckets a human marked as decoys.
    pub decoy: Vec<String>,
    /// Per-engine results in capture order (bucket order matters for ties).
    pub buckets: Buckets,
}

/// A capture: its cases and when it was taken.
#[derive(Debug, Clone)]
pub struct Fixture {
    pub captured_at: String,
    pub cases: Vec<Case>,
}

fn s(v: &Value, k: &str) -> String {
    v.get(k)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Parse a capture: `{"captured_at", "cases": [...]}` or a bare case list.
pub fn parse(json: &str) -> Result<Fixture, FsmError> {
    let raw: Value = serde_json::from_str(json)
        .map_err(|e| FsmError::invalid(format!("ranking fixture is not JSON: {e}")))?;
    let (cases, captured_at) = match &raw {
        Value::Array(a) => (a.as_slice(), "unknown (pre-2026-09 capture)".to_owned()),
        Value::Object(o) => (
            o.get("cases")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .ok_or_else(|| FsmError::invalid("ranking fixture has no \"cases\" list"))?,
            o.get("captured_at")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned(),
        ),
        _ => {
            return Err(FsmError::invalid(
                "ranking fixture must be an object or a list",
            ));
        }
    };
    let cases = cases
        .iter()
        .map(|c| {
            let buckets = c
                .get("buckets")
                .and_then(Value::as_object)
                .ok_or_else(|| {
                    FsmError::invalid(format!("case {:?} has no buckets", s(c, "query")))
                })?
                .iter()
                .map(|(name, rows)| {
                    let hits = rows
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|r| {
                            let mut h = Hit::new(s(r, "title"), s(r, "url"));
                            h.snippet = s(r, "snippet");
                            let age = s(r, "published_age");
                            h.date = (!age.is_empty()).then_some(age);
                            h.source = name.clone();
                            h
                        })
                        .collect();
                    (name.clone(), hits)
                })
                .collect();
            Ok(Case {
                query: s(c, "query"),
                category: c.get("category").and_then(Value::as_str).map(str::to_owned),
                expect: s(c, "expect"),
                decoy: c
                    .get("decoy")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
                buckets,
            })
        })
        .collect::<Result<Vec<_>, FsmError>>()?;
    Ok(Fixture { captured_at, cases })
}

/// Read and parse a capture file.
pub fn load(path: &Path) -> Result<Fixture, FsmError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| FsmError::invalid(format!("cannot read {}: {e}", path.display())))?;
    parse(&text)
}

/// Aggregate ranking quality.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    pub hit1: usize,
    pub hit3: usize,
    /// Mean reciprocal rank, rounded to 4 places like `replay.py`.
    pub mrr: f64,
    pub n: usize,
}

impl Metrics {
    pub fn hit1_rate(&self) -> f64 {
        self.hit1 as f64 / self.n.max(1) as f64
    }

    pub fn hit3_rate(&self) -> f64 {
        self.hit3 as f64 / self.n.max(1) as f64
    }
}

/// Python 0.18.2 on `evals/ranking/fixtures/decoy_and_dupes.json`, measured
/// 2026-11-25 with `python evals/ranking/replay.py --guard --detail --data
/// evals/ranking/fixtures/decoy_and_dupes.json`: identical with and without
/// the guard.
pub const PYTHON_BASELINE: Metrics = Metrics {
    hit1: 2,
    hit3: 4,
    mrr: 0.7083,
    n: 4,
};

/// One case's outcome.
#[derive(Debug, Clone, PartialEq)]
pub struct CaseReport {
    pub query: String,
    pub category: Option<String>,
    /// Rank of the expected URL with native weight 1.0 (guard applied).
    pub plain: Option<usize>,
    /// Rank with the shipped weights (guard applied).
    pub shipped: Option<usize>,
    /// Buckets the guard dropped: (name, coherence, marked as decoy).
    pub dropped: Vec<(String, f64, bool)>,
    /// Marked decoys the guard kept.
    pub missed: Vec<String>,
}

/// The replay's findings.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub captured_at: String,
    pub without_guard: Metrics,
    pub with_guard: Metrics,
    pub cases: Vec<CaseReport>,
}

/// Per-engine result lists, in capture order.
pub type Buckets = Vec<(String, Vec<Hit>)>;

/// The guard's verdict on one case: (kept buckets, dropped with coherence).
pub fn guard(case: &Case) -> (Buckets, Vec<(String, f64)>) {
    let suspects: Vec<&str> = case
        .buckets
        .iter()
        .filter(|(n, rows)| is_guarded(n) && looks_like_decoy(&case.query, rows))
        .map(|(n, _)| n.as_str())
        .collect();
    let witnessed = case
        .buckets
        .iter()
        .any(|(n, rows)| !suspects.contains(&n.as_str()) && is_witness(&case.query, rows));
    if suspects.is_empty() || !witnessed {
        return (case.buckets.clone(), Vec::new());
    }
    let dropped = case
        .buckets
        .iter()
        .filter(|(n, _)| suspects.contains(&n.as_str()))
        .map(|(n, rows)| {
            (
                n.clone(),
                bucket_coherence(&case.query, rows).unwrap_or(0.0),
            )
        })
        .collect();
    let kept = case
        .buckets
        .iter()
        .filter(|(n, _)| !suspects.contains(&n.as_str()))
        .cloned()
        .collect();
    (kept, dropped)
}

/// The kernel's merge of one case's buckets.
pub fn merge(case: &Case, buckets: &[(String, Vec<Hit>)], native_weight: bool) -> Vec<Hit> {
    let mut w = Weighting::default();
    if native_weight {
        w.native = buckets
            .iter()
            .filter(|(n, _)| native(n, case.category.as_deref()))
            .map(|(n, _)| n.clone())
            .collect();
    }
    fuse(buckets, &w, CAP)
}

/// 1-based rank of the first result whose URL contains `expect`.
pub fn rank_of(results: &[Hit], expect: &str) -> Option<usize> {
    let e = expect.to_lowercase();
    results
        .iter()
        .position(|r| r.url.to_lowercase().contains(&e))
        .map(|i| i + 1)
}

fn metrics(positions: &[Option<usize>]) -> Metrics {
    let hit1 = positions.iter().filter(|p| **p == Some(1)).count();
    let hit3 = positions
        .iter()
        .filter(|p| p.is_some_and(|p| p <= 3))
        .count();
    let sum: f64 = positions.iter().flatten().map(|p| 1.0 / *p as f64).sum();
    let n = positions.len();
    Metrics {
        hit1,
        hit3,
        mrr: ((sum / n.max(1) as f64) * 1e4).round() / 1e4,
        n,
    }
}

/// Replay every case with and without the guard.
pub fn replay(f: &Fixture) -> Report {
    let mut raw_pos = Vec::new();
    let mut cases = Vec::new();
    for case in &f.cases {
        raw_pos.push(rank_of(&merge(case, &case.buckets, true), &case.expect));
        let (kept, dropped) = guard(case);
        let shipped = rank_of(&merge(case, &kept, true), &case.expect);
        let plain = rank_of(&merge(case, &kept, false), &case.expect);
        let missed = case
            .decoy
            .iter()
            .filter(|d| !dropped.iter().any(|(n, _)| n == *d))
            .cloned()
            .collect();
        cases.push(CaseReport {
            query: case.query.clone(),
            category: case.category.clone(),
            plain,
            shipped,
            dropped: dropped
                .into_iter()
                .map(|(n, c)| {
                    let marked = case.decoy.contains(&n);
                    (n, c, marked)
                })
                .collect(),
            missed,
        });
    }
    let shipped: Vec<Option<usize>> = cases.iter().map(|c| c.shipped).collect();
    Report {
        captured_at: f.captured_at.clone(),
        without_guard: metrics(&raw_pos),
        with_guard: metrics(&shipped),
        cases,
    }
}

impl Report {
    /// Buckets dropped that no human marked as decoys (must be 0).
    pub fn unmarked_drops(&self) -> usize {
        self.cases
            .iter()
            .flat_map(|c| &c.dropped)
            .filter(|(_, _, marked)| !marked)
            .count()
    }

    /// Marked decoys the guard kept (must be 0).
    pub fn decoy_misses(&self) -> usize {
        self.cases.iter().map(|c| c.missed.len()).sum()
    }

    /// The acceptance gate: hit@1, hit@3 (as rates) and MRR at least the
    /// baseline's minus [`TOLERANCE`], no decoy missed, no healthy bucket
    /// dropped. `Err` lists every failure.
    pub fn check(&self, baseline: &Metrics) -> Result<(), Vec<String>> {
        let m = &self.with_guard;
        let mut errs = Vec::new();
        let gates = [
            ("hit@1", m.hit1_rate(), baseline.hit1_rate()),
            ("hit@3", m.hit3_rate(), baseline.hit3_rate()),
            ("MRR", m.mrr, baseline.mrr),
        ];
        for (name, got, base) in gates {
            if got + 1e-9 < base - TOLERANCE {
                errs.push(format!(
                    "{name} {got:.4} < baseline {base:.4} - {TOLERANCE}"
                ));
            }
        }
        if self.decoy_misses() > 0 {
            errs.push(format!(
                "{} marked decoy bucket(s) were not dropped",
                self.decoy_misses()
            ));
        }
        if self.unmarked_drops() > 0 {
            errs.push(format!(
                "{} dropped bucket(s) are not marked as decoys",
                self.unmarked_drops()
            ));
        }
        if errs.is_empty() { Ok(()) } else { Err(errs) }
    }

    /// The text the CLI prints (mirrors `replay.py --guard --detail`).
    pub fn render(&self, baseline: Option<&Metrics>) -> String {
        let fmt = |m: &Metrics| {
            format!(
                "hit@1={} hit@3={} MRR={:.4} n={}",
                m.hit1, m.hit3, m.mrr, m.n
            )
        };
        let mut out = format!(
            "capture taken {}, {} cases\n\noff-topic guard (coherence < {}, given a witness):\n",
            self.captured_at,
            self.cases.len(),
            crate::coherence::COHERENCE_MIN
        );
        for c in &self.cases {
            for (name, coh, marked) in &c.dropped {
                let q: String = c.query.chars().take(40).collect();
                out.push_str(&format!(
                    "  drops {name:12} coherence={coh:.2}  {q:?}  [{}]\n",
                    if *marked {
                        "marked decoy"
                    } else {
                        "NOT MARKED — read this bucket"
                    }
                ));
            }
            for name in &c.missed {
                out.push_str(&format!("  MISSED decoy {name} in {:?}\n", c.query));
            }
        }
        out.push_str(&format!("  without guard: {}\n", fmt(&self.without_guard)));
        out.push_str(&format!(
            "  with guard:    {}\nshipped (k={RRF_K}, native_weight={NATIVE_WEIGHT}): {}\n",
            fmt(&self.with_guard),
            fmt(&self.with_guard)
        ));
        if let Some(b) = baseline {
            out.push_str(&format!("python baseline: {}\n", fmt(b)));
        }
        out.push_str(&format!(
            "\n{:>7}{:>8}   {:18}  query\n",
            "plain", "shipped", "category"
        ));
        let show = |p: Option<usize>| p.map_or_else(|| "None".to_owned(), |p| p.to_string());
        for c in &self.cases {
            let q: String = c.query.chars().take(36).collect();
            out.push_str(&format!(
                "{:>7}{:>8}   {:18}  {q}\n",
                show(c.plain),
                show(c.shipped),
                c.category.as_deref().unwrap_or("None")
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/ranking/decoy_and_dupes.json");

    #[test]
    fn the_fixture_parses_in_capture_order() {
        let f = parse(FIXTURE).unwrap();
        assert_eq!(f.captured_at, "2026-08-29");
        assert_eq!(f.cases.len(), 4);
        let names: Vec<&str> = f.cases[1].buckets.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            ["duckduckgo", "googlenews", "bing", "worldbank", "imf"]
        );
        assert_eq!(f.cases[0].buckets[2].1.len(), 10);
    }

    #[test]
    fn a_bare_list_is_a_pre_2026_09_capture() {
        let f = parse(r#"[{"query":"q","category":null,"expect":"x","buckets":{}}]"#).unwrap();
        assert!(f.captured_at.starts_with("unknown"));
        assert!(parse("{}").is_err());
        assert!(parse("not json").is_err());
    }

    #[test]
    fn rank_of_is_a_case_insensitive_substring() {
        let rs = [
            Hit::new("a", "https://A.example/X"),
            Hit::new("b", "https://doc.rust-lang.org/book"),
        ];
        assert_eq!(rank_of(&rs, "doc.RUST-lang.org"), Some(2));
        assert_eq!(rank_of(&rs, "a.example/x"), Some(1));
        assert_eq!(rank_of(&rs, "missing"), None);
    }

    #[test]
    fn the_engine_table_matches_python_guarding() {
        assert!(is_guarded("bing") && is_guarded("duckduckgo") && is_guarded("naver"));
        assert!(!is_guarded("googlenews") && !is_guarded("wikipedia") && !is_guarded("imf"));
        assert!(!is_guarded("not-an-engine"));
        assert!(native("imf", Some("finance.macro")) && native("arxiv", Some("paper")));
        assert!(!native("bing", Some("paper")) && !native("imf", None));
    }
}
