//! The human-readable diagnostics a search returns in `notes` (ports of
//! `_gate_hint`, `_benched_hint`, `_filter_hint`, the empty-engine hint and
//! `_freshness_signals`). Each function returns one line.

use crate::cache::freshness_str;
use crate::filters::Drops;
use crate::plan::BenchNote;
use fsm_core::{Freshness, Hit};

/// Results at or below this count are "sparse": filter and empty-engine
/// diagnostics are added (the same threshold everywhere).
pub const SPARSE: usize = 3;

const PROXY_ADVICE: &str = " Configure a proxy (SEARCH_MCP_PROXY) to route through a \
    non-blocked IP, or rely on the keyless default engines.";

/// One walled or off-topic source and the reserve that covered it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gate {
    pub id: String,
    pub reason: String,
    pub via: Option<String>,
}

/// Which sources hit a wall or answered off-topic, and how each was covered.
pub fn gate_hint(gates: &[Gate]) -> String {
    let mut sorted: Vec<&Gate> = gates.iter().collect();
    sorted.sort_by(|a, b| a.id.cmp(&b.id));
    let mut parts = Vec::new();
    let (mut real, mut off_topic) = (false, false);
    for g in sorted {
        let mut desc = if g.reason == "off_topic" {
            off_topic = true;
            format!(
                "{} answered with results that match only the first word of the query; \
                 they were discarded",
                g.id
            )
        } else {
            real = true;
            format!("{} was {}-gated", g.id, g.reason)
        };
        match &g.via {
            Some(via) => desc.push_str(&format!(" → served via {via}")),
            None if g.reason != "off_topic" => desc.push_str(" (no results)"),
            None => {}
        }
        parts.push(desc);
    }
    let mut hint = parts.join("; ") + ".";
    if real {
        hint.push_str(PROXY_ADVICE);
    }
    if off_topic {
        hint.push_str(
            " Discarded results are not a block and a proxy will not change them; \
             the remaining engines answered normally.",
        );
    }
    hint
}

/// Which sources sat this search out on the breaker's bench.
pub fn benched_hint(benched: &[BenchNote]) -> String {
    let mut sorted: Vec<&BenchNote> = benched.iter().collect();
    sorted.sort_by(|a, b| a.id.cmp(&b.id));
    let parts: Vec<String> = sorted
        .iter()
        .map(|b| {
            let minutes = ((b.retry_in_s as f64) / 60.0).round().max(1.0) as u64;
            let mut d = format!(
                "{} is benched ({}; retried in ~{minutes} min)",
                b.id, b.reason
            );
            if let Some(sub) = &b.substitute {
                d.push_str(&format!(", {sub} is standing in"));
            }
            d
        })
        .collect();
    parts.join("; ")
        + ". A benched source failed recently and was not asked this time. \
           Name it in `sources=` to force an attempt."
}

/// Why a filtered result set is sparse, naming the filter to relax.
pub fn filter_hint(drops: &Drops, raw_total: usize, kept: usize) -> String {
    match drops.top() {
        None => format!(
            "Engines returned only {raw_total} raw results (none dropped by filters). \
             Try a broader query or different sources."
        ),
        Some((reason, _)) => format!(
            "Filters dropped {} of {raw_total} raw results (kept {kept}). Most were excluded \
             by {reason}. Try widening or removing one filter.",
            drops.total()
        ),
    }
}

/// Sources that refused (status) or silently returned nothing, on a sparse run.
pub fn empty_hint(refused: &[(String, u16)], empty: &[String]) -> Option<String> {
    let mut hints = Vec::new();
    if !refused.is_empty() {
        let detail: Vec<String> = refused
            .iter()
            .map(|(n, s)| format!("{n} (HTTP {s})"))
            .collect();
        hints.push(format!(
            "{}: the source refused the request, which is different from finding no \
             matches. 429 means back off and retry later; 5xx means the source is down.",
            detail.join(", ")
        ));
    }
    if !empty.is_empty() {
        hints.push(format!(
            "{} returned 0 results with no error and no CAPTCHA/consent wall detected. \
             This may be a silent IP block or a markup change. If this persists, configure \
             a proxy (SEARCH_MCP_PROXY) or pick different sources via `sources=`.",
            empty.join(", ")
        ));
    }
    (!hints.is_empty()).then(|| hints.join(" "))
}

/// `freshness_note`: most results carry no verifiable date although the
/// caller asked for recent ones.
pub fn freshness_note(results: &[Hit], freshness: Option<Freshness>) -> Option<String> {
    let f = freshness?;
    if results.is_empty() {
        return None;
    }
    let dated = results.iter().filter(|r| r.date_source.is_some()).count();
    let undated = results.len() - dated;
    (undated * 2 >= results.len()).then(|| {
        format!(
            "{undated} of {} results carry no verifiable date, and freshness=\"{}\" keeps \
             undated results, so being listed here does not show that a result is recent. \
             Fetch the page and check its publish date before treating it as current.",
            results.len(),
            freshness_str(f)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate(id: &str, reason: &str, via: Option<&str>) -> Gate {
        Gate {
            id: id.into(),
            reason: reason.into(),
            via: via.map(Into::into),
        }
    }

    // --- tests/test_coherence_guard.py / test_rescue.py hint wording ---

    #[test]
    fn an_off_topic_engine_gets_no_proxy_advice() {
        let h = gate_hint(&[gate("bing", "off_topic", None)]);
        assert!(h.starts_with("bing answered with results that match only the first word"));
        assert!(h.contains("a proxy will not change them"));
        assert!(!h.contains("Configure a proxy"));
        assert!(!h.contains("(no results)"));
    }

    #[test]
    fn a_real_wall_gets_proxy_advice_and_says_how_it_was_covered() {
        let h = gate_hint(&[
            gate("duckduckgo", "captcha", Some("yahoo")),
            gate("bing", "consent", None),
        ]);
        assert!(h.starts_with(
            "bing was consent-gated (no results); duckduckgo was captcha-gated → served via yahoo."
        ));
        assert!(h.contains("Configure a proxy"));
    }

    #[test]
    fn a_real_gate_next_to_an_off_topic_engine_keeps_its_own_remedy() {
        // tests/test_coherence_guard.py (same name)
        let h = gate_hint(&[
            gate("bing", "off_topic", None),
            gate("mojeek", "captcha", None),
        ]);
        assert!(h.contains("mojeek was captcha-gated (no results)"));
        assert!(h.contains("Configure a proxy"));
        assert!(h.contains("a proxy will not change them"));
    }

    #[test]
    fn the_bench_hint_names_the_stand_in() {
        let b = BenchNote {
            id: "bing".into(),
            reason: "captcha".into(),
            retry_in_s: 540,
            substitute: Some("yahoo".into()),
        };
        let h = benched_hint(&[b]);
        assert!(
            h.starts_with("bing is benched (captcha; retried in ~9 min), yahoo is standing in.")
        );
        assert!(h.contains("`sources=`"));
    }

    #[test]
    fn filter_hint_names_the_top_reason() {
        let mut d = Drops::default();
        d.bump("include_domains");
        d.bump("freshness");
        d.bump("freshness");
        assert_eq!(
            filter_hint(&d, 10, 2),
            "Filters dropped 3 of 10 raw results (kept 2). Most were excluded by freshness. \
             Try widening or removing one filter."
        );
        assert!(
            filter_hint(&Drops::default(), 2, 2).starts_with("Engines returned only 2 raw results")
        );
    }

    #[test]
    fn empty_hint_separates_refusals_from_silence() {
        let h = empty_hint(&[("youcom".into(), 429)], &["seznam".into()]).unwrap();
        assert!(h.starts_with("youcom (HTTP 429): the source refused the request"));
        assert!(h.contains("seznam returned 0 results with no error"));
        assert_eq!(empty_hint(&[], &[]), None);
    }

    // --- tests/test_freshness_signals.py ---

    fn dated(src: Option<&str>) -> Hit {
        let mut h = Hit::new("t", "https://a.example");
        h.date = src.map(|_| "2026-11-20".to_owned());
        h.date_source = src.map(str::to_owned);
        h
    }

    #[test]
    fn a_freshness_request_answered_mostly_undated_says_so() {
        let rs = [dated(None), dated(None), dated(Some("snippet"))];
        let note = freshness_note(&rs, Some(Freshness::Week)).unwrap();
        assert!(
            note.starts_with("2 of 3 results carry no verifiable date, and freshness=\"week\"")
        );
    }

    #[test]
    fn no_note_without_freshness_or_when_mostly_dated() {
        let rs = [dated(None), dated(None)];
        assert_eq!(freshness_note(&rs, None), None);
        let rs = [
            dated(Some("structured")),
            dated(Some("snippet")),
            dated(None),
        ];
        assert_eq!(freshness_note(&rs, Some(Freshness::Day)), None);
        assert_eq!(freshness_note(&[], Some(Freshness::Day)), None);
    }

    #[test]
    fn half_undated_is_enough_for_the_note() {
        let rs = [dated(None), dated(Some("structured"))];
        assert!(freshness_note(&rs, Some(Freshness::Day)).is_some());
    }
}
