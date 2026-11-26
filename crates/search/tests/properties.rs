//! Property tests on the ranking core: fusion and dedup invariants, and
//! `token_set_ratio`'s basic laws.

use fsm_core::Hit;
use fsm_core::url_canon::url_key;
use fsm_search::rank::{Weighting, dedup_by_title, fuse, token_set_ratio};
use proptest::prelude::*;
use std::collections::HashSet;

fn arb_hit() -> impl Strategy<Value = Hit> {
    (
        prop::sample::select(vec![
            "a.example",
            "www.a.example",
            "b.example",
            "amp.b.example",
            "c.co.uk",
        ]),
        0u8..6,
        prop::sample::select(vec![
            "Rust ownership explained",
            "Rust ownership explained.",
            "Python 3.12 released",
            "Python 3.13 released",
            "",
            "Totally unrelated page",
        ]),
        prop::bool::ANY,
    )
        .prop_map(|(host, path, title, https)| {
            let scheme = if https { "https" } else { "http" };
            Hit::new(title, format!("{scheme}://{host}/p{path}"))
        })
}

fn arb_buckets() -> impl Strategy<Value = Vec<(String, Vec<Hit>)>> {
    prop::collection::vec(prop::collection::vec(arb_hit(), 0..8), 1..5).prop_map(|bs| {
        bs.into_iter()
            .enumerate()
            .map(|(i, b)| (format!("e{i}"), b))
            .collect()
    })
}

proptest! {
    #[test]
    fn fuse_never_repeats_a_page_and_respects_the_limit(buckets in arb_buckets(), limit in 1usize..20) {
        let out = fuse(&buckets, &Weighting::default(), limit);
        prop_assert!(out.len() <= limit);
        let keys: HashSet<String> = out.iter().map(|h| url_key(&h.url)).collect();
        prop_assert_eq!(keys.len(), out.len());
        for w in out.windows(2) {
            prop_assert!(w[0].score >= w[1].score, "scores are non-increasing");
        }
        for h in &out {
            prop_assert!(!h.sources.is_empty());
            prop_assert!(h.sources.windows(2).all(|p| p[0] < p[1]), "sorted, unique");
        }
    }

    #[test]
    fn fuse_is_deterministic(buckets in arb_buckets()) {
        let w = Weighting::default();
        prop_assert_eq!(fuse(&buckets, &w, 50), fuse(&buckets, &w, 50));
    }

    #[test]
    fn a_larger_limit_only_extends_the_list(buckets in arb_buckets(), limit in 1usize..10) {
        let w = Weighting::default();
        let short = fuse(&buckets, &w, limit);
        let long = fuse(&buckets, &w, limit + 5);
        prop_assert_eq!(&long[..short.len()], &short[..]);
    }

    #[test]
    fn dedup_keeps_a_subset_in_order(hits in prop::collection::vec(arb_hit(), 0..12)) {
        let out = dedup_by_title(hits.clone());
        prop_assert!(out.len() <= hits.len());
        let mut it = hits.iter();
        for kept in &out {
            prop_assert!(it.any(|h| h == kept), "order preserved");
        }
        prop_assert_eq!(dedup_by_title(out.clone()), out, "idempotent");
    }

    #[test]
    fn token_set_ratio_is_symmetric_and_bounded(a in "[a-c ]{0,12}", b in "[a-c ]{0,12}") {
        let x = token_set_ratio(&a, &b);
        prop_assert!((0.0..=100.0).contains(&x));
        prop_assert!((x - token_set_ratio(&b, &a)).abs() < 1e-9);
        if !a.trim().is_empty() {
            prop_assert!((token_set_ratio(&a, &a) - 100.0).abs() < 1e-9);
        }
    }
}
