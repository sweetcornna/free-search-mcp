//! Two-stage Union-Find dedup (SPEC.md §6).
//!
//! Stage 1 joins records sharing any exact identifier (DOI, arXiv, PMID,
//! PMCID, OpenAlex, S2, DBLP, OpenAIRE; arXiv DataCite DOIs are already
//! folded into `arxiv` by [`crate::ids::normalize_work_ids`]).
//!
//! Stage 2 joins what is left by bibliographic signature:
//! `token_set_ratio(fold(title)) >= 92` with the numeric guard (titles that
//! both carry digits must carry the same ones), `|year a − year b| <= 1`
//! when both years are known, and the first author's surname equal when
//! both records name authors. Where either record has no authors,
//! `token_set_ratio` alone is too lenient (it scores every superset title
//! 100), so the whole titles must also agree: `token_sort_ratio >= 92`.
//! Two clusters that carry DIFFERENT arXiv ids or DIFFERENT PMIDs are never
//! joined: those registries do not mint two ids for one work.

use crate::fuzzy::{
    digit_tokens, fold_title, numbers_agree, surname_candidates, token_set_ratio, token_sort_ratio,
};
use crate::ids::identity_keys;
use fsm_core::ScholarlyWork;
use std::collections::{BTreeSet, HashMap};

/// Stage-2 title threshold (SPEC.md §6).
pub const TITLE_DEDUP_MIN: f64 = 92.0;

struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
        }
    }

    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }

    /// Join the two sets; the smaller index becomes the root (keeps input order).
    fn union(&mut self, a: usize, b: usize) -> usize {
        let (ra, rb) = (self.find(a), self.find(b));
        let (root, child) = if ra <= rb { (ra, rb) } else { (rb, ra) };
        self.parent[child] = root;
        root
    }
}

/// The bibliographic signature of one record.
struct Sig {
    title: String,
    digits: Vec<String>,
    year: Option<u16>,
    surnames: Vec<String>,
}

impl Sig {
    fn of(w: &ScholarlyWork) -> Self {
        Self {
            title: fold_title(&w.title),
            digits: digit_tokens(&w.title),
            year: w.year,
            surnames: w
                .authors
                .first()
                .map(|a| surname_candidates(&a.name))
                .unwrap_or_default(),
        }
    }
}

/// Whether two records describe the same work by signature alone.
fn same_work(a: &Sig, b: &Sig) -> bool {
    if a.title.is_empty() || b.title.is_empty() || !numbers_agree(&a.digits, &b.digits) {
        return false;
    }
    if let (Some(x), Some(y)) = (a.year, b.year)
        && x.abs_diff(y) > 1
    {
        return false;
    }
    if token_set_ratio(&a.title, &b.title) < TITLE_DEDUP_MIN {
        return false;
    }
    if !a.surnames.is_empty() && !b.surnames.is_empty() {
        a.surnames.iter().any(|x| b.surnames.iter().any(|y| x == y))
    } else {
        token_sort_ratio(&a.title, &b.title) >= TITLE_DEDUP_MIN
    }
}

#[derive(Default, Clone)]
struct Exclusive {
    arxiv: BTreeSet<String>,
    pmid: BTreeSet<String>,
}

impl Exclusive {
    fn conflicts(&self, other: &Self) -> bool {
        let clash = |a: &BTreeSet<String>, b: &BTreeSet<String>| {
            !a.is_empty() && !b.is_empty() && a.is_disjoint(b)
        };
        clash(&self.arxiv, &other.arxiv) || clash(&self.pmid, &other.pmid)
    }

    fn absorb(&mut self, other: Self) {
        self.arxiv.extend(other.arxiv);
        self.pmid.extend(other.pmid);
    }
}

/// Group `works` (ids already normalised) into identity clusters. Each
/// cluster lists member indices ascending; clusters are ordered by their
/// first member.
pub(crate) fn clusters(works: &[ScholarlyWork]) -> Vec<Vec<usize>> {
    let n = works.len();
    let mut uf = UnionFind::new(n);
    // Stage 1: exact identifiers.
    let mut seen: HashMap<String, usize> = HashMap::new();
    for (i, w) in works.iter().enumerate() {
        for key in identity_keys(&w.ids) {
            match seen.get(&key) {
                Some(&j) => {
                    uf.union(i, j);
                }
                None => {
                    seen.insert(key, i);
                }
            }
        }
    }
    let mut excl: HashMap<usize, Exclusive> = HashMap::new();
    for (i, w) in works.iter().enumerate() {
        let root = uf.find(i);
        let e = excl.entry(root).or_default();
        e.arxiv.extend(w.ids.arxiv.clone());
        e.pmid.extend(w.ids.pmid.clone());
    }
    // Stage 2: bibliographic signature, never across an id conflict.
    let sigs: Vec<Sig> = works.iter().map(Sig::of).collect();
    for i in 0..n {
        for j in (i + 1)..n {
            let (ri, rj) = (uf.find(i), uf.find(j));
            if ri == rj || !same_work(&sigs[i], &sigs[j]) {
                continue;
            }
            let (ei, ej) = (
                excl.get(&ri).cloned().unwrap_or_default(),
                excl.get(&rj).cloned().unwrap_or_default(),
            );
            if ei.conflicts(&ej) {
                continue;
            }
            excl.remove(&ri);
            excl.remove(&rj);
            let root = uf.union(ri, rj);
            let mut merged = ei;
            merged.absorb(ej);
            excl.insert(root, merged);
        }
    }
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut slot: HashMap<usize, usize> = HashMap::new();
    for i in 0..n {
        let root = uf.find(i);
        match slot.get(&root) {
            Some(&g) => groups[g].push(i),
            None => {
                slot.insert(root, groups.len());
                groups.push(vec![i]);
            }
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsm_core::AuthorRef;

    fn w(title: &str, year: Option<u16>, author: Option<&str>) -> ScholarlyWork {
        let mut w = ScholarlyWork::new(title, "");
        w.year = year;
        w.authors = author.into_iter().map(AuthorRef::new).collect();
        w
    }

    #[test]
    fn stage_one_joins_shared_ids_transitively() {
        let mut a = w(
            "Attention Is All You Need",
            Some(2017),
            Some("Ashish Vaswani"),
        );
        a.ids.arxiv = Some("1706.03762".into());
        let mut b = w("Totally different metadata", Some(2017), None);
        b.ids.arxiv = Some("1706.03762".into());
        b.ids.doi = Some("10.5555/3295222.3295349".into());
        let mut c = w("Attention is all you need (NeurIPS)", Some(2017), None);
        c.ids.doi = Some("10.5555/3295222.3295349".into());
        let d = w("Unrelated", None, None);
        assert_eq!(clusters(&[a, b, d, c]), vec![vec![0, 1, 3], vec![2]]);
    }

    #[test]
    fn stage_two_joins_preprint_and_published_by_signature() {
        let mut pre = w(
            "Deep Residual Learning for Image Recognition",
            Some(2015),
            Some("Kaiming He"),
        );
        pre.ids.arxiv = Some("1512.03385".into());
        let mut pubd = w(
            "Deep Residual Learning for Image Recognition",
            Some(2016),
            Some("He, Kaiming"),
        );
        pubd.ids.doi = Some("10.1109/cvpr.2016.90".into());
        assert_eq!(clusters(&[pre, pubd]), vec![vec![0, 1]]);
    }

    #[test]
    fn stage_two_guards() {
        let base = || {
            w(
                "Attention Is All You Need",
                Some(2017),
                Some("Ashish Vaswani"),
            )
        };
        // Different first author: a superset title from someone else is another paper.
        let speech = w(
            "Attention Is All You Need In Speech Separation",
            Some(2017),
            Some("Cem Subakan"),
        );
        assert_eq!(clusters(&[base(), speech]).len(), 2);
        // Years more than one apart.
        let later = w(
            "Attention Is All You Need",
            Some(2020),
            Some("Ashish Vaswani"),
        );
        assert_eq!(clusters(&[base(), later]).len(), 2);
        // Digits that differ.
        let gpt3 = w(
            "Language Models are Few-Shot Learners GPT-3",
            Some(2020),
            Some("Tom Brown"),
        );
        let gpt4 = w(
            "Language Models are Few-Shot Learners GPT-4",
            Some(2020),
            Some("Tom Brown"),
        );
        assert_eq!(clusters(&[gpt3, gpt4]).len(), 2);
        // No authors on one side: the superset title is not enough...
        let anon_superset = w(
            "Attention Is All You Need In Speech Separation",
            Some(2017),
            None,
        );
        assert_eq!(clusters(&[base(), anon_superset]).len(), 2);
        // ...but the same title is.
        let anon_same = w("Attention is all you need.", None, None);
        assert_eq!(clusters(&[base(), anon_same]).len(), 1);
    }

    #[test]
    fn stage_two_never_joins_different_arxiv_ids_or_pmids() {
        let mut a = w(
            "A Survey of Large Language Models",
            Some(2023),
            Some("Wayne Xin Zhao"),
        );
        a.ids.arxiv = Some("2303.18223".into());
        let mut b = w(
            "A Survey of Large Language Models",
            Some(2023),
            Some("Zhao W"),
        );
        b.ids.arxiv = Some("2307.06435".into());
        let c = w(
            "A Survey of Large Language Models",
            Some(2023),
            Some("W. Zhao"),
        );
        // c may join either, but never both: a and b stay apart.
        let groups = clusters(&[a, b, c]);
        assert_eq!(groups.len(), 2);
        assert!(groups.iter().all(|g| !(g.contains(&0) && g.contains(&1))));

        let mut p = w("Semaglutide and obesity", Some(2021), Some("Wilding JPH"));
        p.ids.pmid = Some("33567185".into());
        let mut q = w("Semaglutide and obesity", Some(2021), Some("John Wilding"));
        q.ids.pmid = Some("99999999".into());
        assert_eq!(clusters(&[p, q]).len(), 2);
    }

    #[test]
    fn empty_titles_never_join_by_signature() {
        assert_eq!(clusters(&[w("", None, None), w("", None, None)]).len(), 2);
        assert!(clusters(&[]).is_empty());
    }

    #[test]
    fn stage_two_matches_given_family_caps_and_generational_suffixes() {
        let a = w(
            "Deep Residual Learning for Image Recognition",
            Some(2015),
            Some("Kaiming HE"),
        );
        let b = w(
            "Deep Residual Learning for Image Recognition",
            Some(2016),
            Some("Kaiming He"),
        );
        assert_eq!(clusters(&[a, b]), vec![vec![0, 1]]);

        let c = w(
            "Distributed Consensus Protocols",
            Some(2021),
            Some("John Smith Jr."),
        );
        let d = w(
            "Distributed Consensus Protocols",
            Some(2021),
            Some("Smith, John"),
        );
        assert_eq!(clusters(&[c, d]), vec![vec![0, 1]]);
    }
}
