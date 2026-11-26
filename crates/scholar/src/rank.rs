//! Ranking (SPEC.md §6 "Ranking"): weighted RRF + title/abstract BM25 +
//! `0.15·ln(1+citedBy)` + recency + `0.05` OA, retracted works demoted.
//!
//! Scale (an interpretation, see the handoff): the spec's `+0.05` OA bonus
//! only reads as "small" when the main terms are O(1), so both relevance
//! terms are normalised to 0..1 within the candidate set:
//!
//! * `rrf` = Σ w_s / (60 + rank_s), divided by the score of a work every
//!   answering source ranked first;
//! * `bm25` = BM25 of the query against title + abstract (k1 1.2, b 0.75,
//!   English stemming), divided by the best score in the set;
//! * a near-exact whole-title match (`token_sort_ratio >= 95`) adds
//!   [`TITLE_MATCH_BONUS`], so a pasted title is not out-ranked by a looser
//!   match with orders of magnitude more citations;
//! * `0.15·ln(1+citedBy)` as specified; a work with no known count gets the
//!   median of the known ones, so missing metadata is neutral rather than
//!   a penalty;
//! * recency: `0.1·max(0, 1 − age/10 years)`;
//! * retracted: [`RETRACTION_PENALTY`] subtracted (kept, flagged, demoted).
//!
//! `sort = "citations"` / `"recent"` order by count / date (unknown last) with
//! the relevance score as tie-break, and list retracted works after the rest.

use crate::fuzzy::{default_process, token_sort_ratio};
use crate::merge::Member;
use bm25::{Embedder, EmbedderBuilder, Language, Scorer};
use fsm_core::ScholarlyWork;
use std::cmp::Ordering;

/// RRF constant.
pub const RRF_K: f64 = 60.0;
/// Citation prior coefficient.
pub const CITATION_WEIGHT: f64 = 0.15;
/// OA bonus.
pub const OA_BONUS: f64 = 0.05;
/// Near-exact whole-title bonus.
pub const TITLE_MATCH_BONUS: f64 = 1.0;
/// Demotion of a retracted work in relevance order.
pub const RETRACTION_PENALTY: f64 = 1.0;
/// Recency weight (linear decay over ten years).
pub const RECENCY_WEIGHT: f64 = 0.1;

/// Requested order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortMode {
    #[default]
    Relevance,
    Citations,
    Recent,
}

impl SortMode {
    /// `"relevance" | "citations" | "recent"` (also accepts `"date"`, `"cited"`).
    pub fn parse(s: Option<&str>) -> Option<Self> {
        match s.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            None | Some("" | "relevance") => Some(Self::Relevance),
            Some("citations" | "cited" | "cited_by" | "citedby") => Some(Self::Citations),
            Some("recent" | "date" | "newest") => Some(Self::Recent),
            _ => None,
        }
    }
}

/// A merged work with the per-source ranks it was found at.
#[derive(Debug, Clone)]
pub(crate) struct Ranked {
    /// The records the work was merged from (re-merged after enrichment).
    pub members: Vec<Member>,
    pub work: ScholarlyWork,
    /// `(source weight, 1-based rank)` per source list it appeared in.
    pub ranks: Vec<(f64, usize)>,
    pub score: f64,
}

fn bm25_scores(query: &str, docs: &[String]) -> Vec<f64> {
    if docs.is_empty() || query.trim().is_empty() {
        return vec![0.0; docs.len()];
    }
    let corpus: Vec<&str> = docs.iter().map(String::as_str).collect();
    let embedder: Embedder =
        EmbedderBuilder::with_fit_to_corpus(Language::English, &corpus).build();
    let mut scorer: Scorer<usize> = Scorer::new();
    for (i, d) in corpus.iter().enumerate() {
        scorer.upsert(&i, embedder.embed(d));
    }
    let q = embedder.embed(query);
    (0..docs.len())
        .map(|i| f64::from(scorer.score(&i, &q).unwrap_or(0.0)))
        .collect()
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    let mid = v.len() / 2;
    if v.len().is_multiple_of(2) {
        (v[mid - 1] + v[mid]) / 2.0
    } else {
        v[mid]
    }
}

/// Score every candidate in place. `rrf_max` is Σ w_s/(K+1) over the sources
/// that answered; `this_year` drives recency.
pub(crate) fn score_all(query: &str, items: &mut [Ranked], rrf_max: f64, this_year: i32) {
    let docs: Vec<String> = items
        .iter()
        .map(|r| {
            format!(
                "{} {}",
                r.work.title,
                r.work.abstract_text.as_deref().unwrap_or("")
            )
        })
        .collect();
    let bm = bm25_scores(query, &docs);
    let bm_max = bm.iter().copied().fold(0.0_f64, f64::max);
    let known: Vec<f64> = items
        .iter()
        .filter_map(|r| r.work.cited_by)
        .map(|c| CITATION_WEIGHT * (1.0 + c as f64).ln())
        .collect();
    let neutral = median(known);
    let q = default_process(query);
    for (r, b) in items.iter_mut().zip(bm) {
        let rrf: f64 = r
            .ranks
            .iter()
            .map(|(w, rank)| w / (RRF_K + *rank as f64))
            .sum();
        let rrf = if rrf_max > 0.0 {
            (rrf / rrf_max).min(1.0)
        } else {
            0.0
        };
        let bm25 = if bm_max > 0.0 { b / bm_max } else { 0.0 };
        let title_bonus =
            if !q.is_empty() && token_sort_ratio(&q, &default_process(&r.work.title)) >= 95.0 {
                TITLE_MATCH_BONUS
            } else {
                0.0
            };
        let prior = r
            .work
            .cited_by
            .map_or(neutral, |c| CITATION_WEIGHT * (1.0 + c as f64).ln());
        let recency = r.work.year.map_or(0.0, |y| {
            let age = f64::from((this_year - i32::from(y)).max(0));
            RECENCY_WEIGHT * (1.0 - age / 10.0).max(0.0)
        });
        let oa = match &r.work.oa {
            Some(o) if !o.status.trim().is_empty() && !o.status.eq_ignore_ascii_case("closed") => {
                OA_BONUS
            }
            _ => 0.0,
        };
        let penalty = if r.work.retracted {
            RETRACTION_PENALTY
        } else {
            0.0
        };
        r.score = rrf + bm25 + title_bonus + prior + recency + oa - penalty;
    }
}

fn date_key(w: &ScholarlyWork) -> Option<String> {
    w.published_date
        .clone()
        .or_else(|| w.year.map(|y| format!("{y:04}")))
}

/// Order scored candidates for `mode` (stable: ties keep input order).
/// Retracted works stay visible and flagged, always listed below non-retracted matches.
pub(crate) fn sort(items: &mut [Ranked], mode: SortMode) {
    let by_score =
        |a: &Ranked, b: &Ranked| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal);
    match mode {
        SortMode::Relevance => items.sort_by(|a, b| {
            a.work
                .retracted
                .cmp(&b.work.retracted)
                .then_with(|| by_score(a, b))
        }),
        SortMode::Citations => items.sort_by(|a, b| {
            a.work
                .retracted
                .cmp(&b.work.retracted)
                .then_with(|| b.work.cited_by.cmp(&a.work.cited_by))
                .then_with(|| by_score(a, b))
        }),
        SortMode::Recent => items.sort_by(|a, b| {
            a.work
                .retracted
                .cmp(&b.work.retracted)
                .then_with(|| date_key(&b.work).cmp(&date_key(&a.work)))
                .then_with(|| by_score(a, b))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsm_core::OaLocation;

    fn cand(title: &str, abs: &str, cited: Option<u64>, ranks: &[(f64, usize)]) -> Ranked {
        let mut w = ScholarlyWork::new(title, "");
        w.abstract_text = Some(abs.into()).filter(|a: &String| !a.is_empty());
        w.cited_by = cited;
        Ranked {
            members: Vec::new(),
            work: w,
            ranks: ranks.to_vec(),
            score: 0.0,
        }
    }

    fn titles(items: &[Ranked]) -> Vec<&str> {
        items.iter().map(|r| r.work.title.as_str()).collect()
    }

    const RRF4: f64 = 4.0 / 61.0;

    #[test]
    fn an_exact_title_beats_a_looser_match_with_1000x_the_citations() {
        let mut items = vec![
            cand(
                "Transformers for image recognition at scale",
                "We apply transformers to image patches.",
                Some(20_000),
                &[(1.0, 1), (1.0, 1), (1.0, 2)],
            ),
            cand(
                "Reciprocal rank fusion outperforms condorcet and individual rank learning methods",
                "Reciprocal rank fusion combines rankings.",
                Some(20),
                &[(1.0, 2), (1.0, 2), (1.0, 1)],
            ),
        ];
        let q = "Reciprocal rank fusion outperforms condorcet and individual rank learning methods";
        score_all(q, &mut items, RRF4, 2026);
        sort(&mut items, SortMode::Relevance);
        assert_eq!(items[0].work.title, q);
    }

    #[test]
    fn a_work_without_a_count_is_not_buried_for_lacking_it() {
        // Same relevance; one has no count. It must rank alongside the median,
        // not below every counted work.
        let mut items = vec![
            cand("graph neural networks survey", "", Some(10), &[(1.0, 3)]),
            cand("graph neural networks review", "", Some(1000), &[(1.0, 3)]),
            cand("graph neural networks tutorial", "", None, &[(1.0, 3)]),
        ];
        score_all("graph neural networks", &mut items, RRF4, 2026);
        sort(&mut items, SortMode::Relevance);
        assert_eq!(
            titles(&items),
            [
                "graph neural networks review",
                "graph neural networks tutorial",
                "graph neural networks survey"
            ]
        );
    }

    #[test]
    fn a_retracted_work_is_present_but_below_its_peer() {
        let mut a = cand(
            "Hydroxychloroquine outcomes in covid-19",
            "registry analysis",
            Some(900),
            &[(1.0, 1), (1.0, 1)],
        );
        a.work.retracted = true;
        let b = cand(
            "Hydroxychloroquine outcomes in covid-19 patients",
            "trial",
            Some(300),
            &[(1.0, 2), (1.0, 2)],
        );
        let mut items = vec![a, b];
        score_all("hydroxychloroquine covid-19", &mut items, 2.0 / 61.0, 2026);
        sort(&mut items, SortMode::Relevance);
        assert_eq!(items.len(), 2);
        assert!(!items[0].work.retracted && items[1].work.retracted);
        sort(&mut items, SortMode::Citations);
        assert!(
            items[1].work.retracted,
            "citations order lists retracted works last"
        );
    }

    #[test]
    fn oa_is_a_tie_break_and_sort_modes_order_by_their_key() {
        let mut open = cand("same title here", "", Some(5), &[(1.0, 2)]);
        open.work.oa = Some(OaLocation::new("gold"));
        let mut closed = cand("same title here", "", Some(5), &[(1.0, 2)]);
        closed.work.oa = Some(OaLocation::new("closed"));
        let mut items = vec![closed, open];
        score_all("same title", &mut items, 1.0 / 61.0, 2026);
        sort(&mut items, SortMode::Relevance);
        assert_eq!(items[0].work.oa.as_ref().unwrap().status, "gold");
        assert!((items[0].score - items[1].score - OA_BONUS).abs() < 1e-9);

        let mut old = cand("a", "", Some(1), &[(1.0, 1)]);
        old.work.published_date = Some("2001-01-01".into());
        let mut new = cand("b", "", None, &[(1.0, 2)]);
        new.work.year = Some(2025);
        let undated = cand("c", "", Some(50), &[(1.0, 3)]);
        let mut items = vec![old, undated, new];
        score_all("x", &mut items, 1.0 / 61.0, 2026);
        sort(&mut items, SortMode::Recent);
        assert_eq!(titles(&items), ["b", "a", "c"]);
        sort(&mut items, SortMode::Citations);
        assert_eq!(titles(&items), ["c", "a", "b"]);
    }

    #[test]
    fn sort_mode_parsing() {
        assert_eq!(SortMode::parse(None), Some(SortMode::Relevance));
        assert_eq!(
            SortMode::parse(Some("Citations")),
            Some(SortMode::Citations)
        );
        assert_eq!(SortMode::parse(Some("recent")), Some(SortMode::Recent));
        assert_eq!(SortMode::parse(Some("sideways")), None);
    }

    #[test]
    fn highly_cited_retracted_paper_still_ranks_below_non_retracted_peer() {
        let mut retracted = cand(
            "Hydroxychloroquine or chloroquine with or without a macrolide",
            "Multinational registry analysis",
            Some(1_322),
            &[(1.0, 1), (1.0, 1)],
        );
        retracted.work.retracted = true;
        let peer = cand(
            "Hydroxychloroquine clinical trial outcomes in covid-19",
            "Randomized clinical trial",
            Some(5),
            &[(1.0, 2), (1.0, 2)],
        );
        let mut items = vec![retracted, peer];
        score_all(
            "hydroxychloroquine chloroquine registry",
            &mut items,
            2.0 / 61.0,
            2026,
        );
        sort(&mut items, SortMode::Relevance);
        assert_eq!(items.len(), 2);
        assert!(!items[0].work.retracted);
        assert!(items[1].work.retracted);
    }

    #[test]
    fn empty_oa_status_gets_no_oa_bonus() {
        let mut blank = cand("same title here", "", Some(5), &[(1.0, 2)]);
        blank.work.oa = Some(OaLocation::new(""));
        let plain = cand("same title here", "", Some(5), &[(1.0, 2)]);
        let mut items = vec![blank, plain];
        score_all("same title", &mut items, 1.0 / 61.0, 2026);
        assert!((items[0].score - items[1].score).abs() < 1e-9);
    }
}
