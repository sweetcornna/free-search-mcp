//! Field-level merge of one identity cluster (SPEC.md §6 "Field Merge").
//!
//! * `title` / `venue` / `year` (and date, authors, type): Crossref > OpenAlex >
//!   Europe PMC > arXiv > OpenAIRE > any other source, in input order.
//! * `abstract`: the longest that does not look truncated.
//! * `citedBy`: the maximum, with the source that reported it.
//! * `oa`: Unpaywall / PMC JATS > OpenAlex > S2 > DOAJ > CORE > others;
//!   "closed" only when no source knows an open copy.
//! * `retracted`: any source saying so wins.
//! * `versions`: preprint and published versions found across the cluster.
//!
//! Enrichment records (top-N single-id lookups, see `enrich`) fill
//! `title`/`venue`/`year`/`date`/`authors`/`type` only when no search source
//! had them: they add counts, OA, abstracts, ids and retraction flags, but
//! never rewrite what the search sources agreed on (OpenAlex has been seen
//! reporting 2025 for the 2017 "Attention Is All You Need").

use crate::fuzzy::{author_key_pairs, fold_title};
use crate::ids::{is_preprint_doi, normalize_work_ids};
use crate::parse::{canonical_url, clean_abstract, clean_text};
use fsm_core::{AuthorRef, OaLocation, ScholarlyWork, WorkIds, WorkVersion};
use std::collections::{HashMap, HashSet};

/// Title/venue/year source priority (lower wins); unlisted sources rank 5.
const FIELD_PRIORITY: [&str; 5] = ["crossref", "openalex", "europepmc", "arxiv", "openaire"];

/// One input of a merge.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Member {
    pub work: ScholarlyWork,
    /// A top-N enrichment record rather than a search/lookup result.
    pub enrichment: bool,
}

impl Member {
    pub(crate) fn primary(work: ScholarlyWork) -> Self {
        Self {
            work,
            enrichment: false,
        }
    }

    pub(crate) fn enrichment(work: ScholarlyWork) -> Self {
        Self {
            work,
            enrichment: true,
        }
    }
}

/// The first entry of `sources` (the source that produced the record).
pub(crate) fn primary_source(w: &ScholarlyWork) -> &str {
    w.sources.first().map_or("", String::as_str)
}

fn field_rank(src: &str) -> usize {
    FIELD_PRIORITY
        .iter()
        .position(|s| *s == src)
        .unwrap_or(FIELD_PRIORITY.len())
}

pub(crate) fn is_preprint_type(t: &str) -> bool {
    matches!(
        t.to_ascii_lowercase().as_str(),
        "preprint" | "posted-content" | "posted_content"
    )
}

/// Clean one external record as it enters the pipeline: markup out of the
/// title and abstract, ids normalised, the producing source first in `sources`.
pub(crate) fn prepare(w: ScholarlyWork, source: Option<&str>) -> ScholarlyWork {
    prepare_inner(w, source, true)
}

/// Normalise one record already produced by an internal `crate::parse` parser:
/// skips markup/entity cleaning so already-decoded text (`"a <b> c"`, math
/// inequalities) is never stripped on a second pass.
pub(crate) fn prepare_parsed(w: ScholarlyWork, source: Option<&str>) -> ScholarlyWork {
    prepare_inner(w, source, false)
}

fn prepare_inner(mut w: ScholarlyWork, source: Option<&str>, clean_markup: bool) -> ScholarlyWork {
    if clean_markup {
        w.title = clean_text(&w.title);
        w.abstract_text = w.abstract_text.as_deref().and_then(clean_abstract);
        w.venue = w
            .venue
            .take()
            .map(|v| clean_text(&v))
            .filter(|v| !v.is_empty());
        w.authors.retain_mut(|a| {
            a.name = clean_text(&a.name);
            !a.name.is_empty()
        });
    } else {
        w.title = w.title.trim().to_owned();
        w.abstract_text = w
            .abstract_text
            .take()
            .map(|a| a.trim().to_owned())
            .filter(|a| !a.is_empty());
        w.venue = w
            .venue
            .take()
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty());
        w.authors.retain_mut(|a| {
            a.name = a.name.trim().to_owned();
            !a.name.is_empty()
        });
    }
    normalize_work_ids(&mut w.ids);
    if w.year.is_none() {
        w.year = w
            .published_date
            .as_deref()
            .and_then(|d| d.get(..4))
            .and_then(|y| y.parse().ok());
    }
    w.url = canonical_url(&w.url);
    if w.url.is_empty() {
        if let Some(d) = &w.ids.doi {
            w.url = format!("https://doi.org/{d}");
        } else if let Some(a) = &w.ids.arxiv {
            w.url = format!("https://arxiv.org/abs/{a}");
        }
    }
    if w.work_type.trim().is_empty() {
        w.work_type = "article".into();
    }
    if let Some(src) = source.filter(|s| !s.is_empty()) {
        w.sources.retain(|s| s != src);
        w.sources.insert(0, src.to_owned());
    }
    w.sources.retain(|s| !s.trim().is_empty());
    w
}

fn looks_truncated(s: &str) -> bool {
    let t = s.trim_end();
    t.ends_with('…') || t.ends_with("...") || t.ends_with("[...]") || t.ends_with("(truncated)")
}

fn oa_tier(m: &Member) -> Option<u8> {
    let oa = m.work.oa.as_ref()?;
    let status = oa.status.trim();
    if status.is_empty() {
        return None;
    }
    if status.eq_ignore_ascii_case("closed") {
        return Some(9);
    }
    let src = primary_source(&m.work);
    if src == "unpaywall" || oa.structured_format.as_deref() == Some("jats") {
        return Some(0);
    }
    Some(match src {
        "openalex" => 1,
        "semanticscholar" | "s2" => 2,
        "doaj" => 3,
        "core_ac" | "core" => 4,
        _ => 5,
    })
}

fn version_key(v: &WorkVersion) -> (String, Option<String>, Option<String>) {
    (v.kind.clone(), v.doi.clone(), v.arxiv.clone())
}

/// Merge one cluster (members in input order). Never empty-titled when any
/// member has a title.
pub(crate) fn merge_members(members: &[Member]) -> ScholarlyWork {
    // Field order: search/lookup records by source priority, then enrichment.
    let mut order: Vec<&Member> = members.iter().collect();
    order.sort_by_key(|m| (m.enrichment, field_rank(primary_source(&m.work))));
    let first = |f: &dyn Fn(&ScholarlyWork) -> bool| order.iter().find(|m| f(&m.work));

    let mut out = ScholarlyWork::default();
    out.title = first(&|w| !w.title.is_empty())
        .map(|m| m.work.title.clone())
        .unwrap_or_default();
    out.venue = first(&|w| w.venue.is_some()).and_then(|m| m.work.venue.clone());
    let year_member = first(&|w| w.year.is_some());
    out.year = year_member.and_then(|m| m.work.year);
    out.published_date = {
        let matching: Vec<&str> = order
            .iter()
            .filter_map(|m| m.work.published_date.as_deref())
            .filter(|d| {
                out.year
                    .is_none_or(|y| d.get(..4).and_then(|p| p.parse::<u16>().ok()) == Some(y))
            })
            .collect();
        // The most precise date that agrees with the chosen year.
        matching
            .iter()
            .enumerate()
            .max_by_key(|(i, d)| (d.len(), std::cmp::Reverse(*i)))
            .map(|(_, d)| (*d).to_owned())
    };

    // Authors from the best-ranked list; ORCID / affiliation filled from others
    // without colliding co-authors who share a surname.
    let mut authors: Vec<AuthorRef> = first(&|w| !w.authors.is_empty())
        .map(|m| m.work.authors.clone())
        .unwrap_or_default();
    let count_author_keys = |list: &[AuthorRef]| {
        let mut exact: HashMap<String, usize> = HashMap::new();
        let mut full: HashMap<(String, char), usize> = HashMap::new();
        let mut surname: HashMap<String, usize> = HashMap::new();
        for a in list {
            let ex = fold_title(&a.name);
            if !ex.is_empty() {
                *exact.entry(ex).or_insert(0) += 1;
            }
            let mut seen_surnames: HashSet<String> = HashSet::new();
            let mut seen_full: HashSet<(String, char)> = HashSet::new();
            for (s, init) in author_key_pairs(&a.name) {
                if seen_surnames.insert(s.clone()) {
                    *surname.entry(s.clone()).or_insert(0) += 1;
                }
                if let Some(ch) = init
                    && seen_full.insert((s.clone(), ch))
                {
                    *full.entry((s, ch)).or_insert(0) += 1;
                }
            }
        }
        (exact, full, surname)
    };
    let (win_exact, win_full, win_surname) = count_author_keys(&authors);
    let mut by_exact: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();
    let mut by_full: HashMap<(String, char), (Option<String>, Option<String>)> = HashMap::new();
    let mut by_surname: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();
    let record_extra = |slot: &mut (Option<String>, Option<String>), a: &AuthorRef| {
        if slot.0.is_none() {
            slot.0.clone_from(&a.orcid);
        }
        if slot.1.is_none() {
            slot.1.clone_from(&a.affiliation);
        }
    };
    for m in &order {
        let (d_exact, d_full, d_surname) = count_author_keys(&m.work.authors);
        for a in &m.work.authors {
            if a.orcid.is_none() && a.affiliation.is_none() {
                continue;
            }
            let ex = fold_title(&a.name);
            if !ex.is_empty() && d_exact.get(&ex).copied() == Some(1) {
                record_extra(by_exact.entry(ex).or_default(), a);
            }
            for (s, init) in author_key_pairs(&a.name) {
                if let Some(ch) = init
                    && d_full.get(&(s.clone(), ch)).copied() == Some(1)
                {
                    record_extra(by_full.entry((s.clone(), ch)).or_default(), a);
                }
                if d_surname.get(&s).copied() == Some(1) {
                    record_extra(by_surname.entry(s).or_default(), a);
                }
            }
        }
    }
    let apply_extra = |a: &mut AuthorRef, (orcid, aff): &(Option<String>, Option<String>)| {
        if a.orcid.is_none() {
            a.orcid.clone_from(orcid);
        }
        if a.affiliation.is_none() {
            a.affiliation.clone_from(aff);
        }
    };
    for a in &mut authors {
        let ex = fold_title(&a.name);
        if !ex.is_empty()
            && win_exact.get(&ex).copied() == Some(1)
            && let Some(vals) = by_exact.get(&ex)
        {
            apply_extra(a, vals);
        }
        let pairs = author_key_pairs(&a.name);
        for (s, init) in &pairs {
            if let Some(ch) = *init
                && win_full.get(&(s.clone(), ch)).copied() == Some(1)
                && let Some(vals) = by_full.get(&(s.clone(), ch))
            {
                apply_extra(a, vals);
            }
        }
        for (s, _) in &pairs {
            if win_surname.get(s).copied() == Some(1)
                && let Some(vals) = by_surname.get(s)
            {
                apply_extra(a, vals);
            }
        }
    }
    out.authors = authors;

    // Ids: field by field in priority order; a published DOI beats a preprint one.
    let mut ids = WorkIds::default();
    for m in &order {
        let i = &m.work.ids;
        for (dst, src) in [
            (&mut ids.arxiv, &i.arxiv),
            (&mut ids.pmid, &i.pmid),
            (&mut ids.pmcid, &i.pmcid),
            (&mut ids.openalex, &i.openalex),
            (&mut ids.s2, &i.s2),
            (&mut ids.dblp, &i.dblp),
            (&mut ids.openaire, &i.openaire),
        ] {
            if dst.is_none() {
                dst.clone_from(src);
            }
        }
    }
    ids.doi = order
        .iter()
        .filter_map(|m| m.work.ids.doi.as_ref())
        .find(|d| !is_preprint_doi(d))
        .or_else(|| order.iter().find_map(|m| m.work.ids.doi.as_ref()))
        .cloned();

    // Versions seen across the cluster.
    let mut versions: Vec<WorkVersion> = Vec::new();
    let mut push = |v: WorkVersion| {
        if !versions.iter().any(|x| version_key(x) == version_key(&v)) {
            versions.push(v);
        }
    };
    for m in &order {
        for v in &m.work.versions {
            push(v.clone());
        }
        if let Some(a) = &m.work.ids.arxiv {
            let mut v = WorkVersion::new("preprint", format!("https://arxiv.org/abs/{a}"));
            v.arxiv = Some(a.clone());
            push(v);
        }
        if let Some(d) = &m.work.ids.doi {
            // On arXiv records `<arxiv:doi>` is the published version's DOI,
            // whereas any other source with `work_type == "preprint"` (Crossref
            // posted-content, Europe PMC PPR, bioRxiv) carries a preprint DOI.
            let preprint = is_preprint_doi(d)
                || (is_preprint_type(&m.work.work_type) && primary_source(&m.work) != "arxiv");
            let mut v = WorkVersion::new(
                if preprint { "preprint" } else { "published" },
                format!("https://doi.org/{d}"),
            );
            v.doi = Some(d.clone());
            push(v);
        }
    }
    let has_published = versions.iter().any(|v| v.kind == "published");
    out.versions = if versions.len() >= 2 {
        versions
    } else {
        Vec::new()
    };

    out.work_type = first(&|w| !w.work_type.is_empty())
        .map(|m| m.work.work_type.clone())
        .unwrap_or_else(|| "article".into());
    if is_preprint_type(&out.work_type) && has_published {
        out.work_type = order
            .iter()
            .map(|m| m.work.work_type.as_str())
            .find(|t| !t.is_empty() && !is_preprint_type(t))
            .unwrap_or("article")
            .to_owned();
    }

    // Abstract: longest that does not look cut off.
    let abstracts: Vec<&String> = order
        .iter()
        .filter_map(|m| m.work.abstract_text.as_ref())
        .filter(|a| !a.trim().is_empty())
        .collect();
    let longest = |it: &mut dyn Iterator<Item = &&String>| {
        it.enumerate()
            .max_by_key(|(i, a)| (a.chars().count(), std::cmp::Reverse(*i)))
            .map(|(_, a)| (*a).clone())
    };
    out.abstract_text = longest(&mut abstracts.iter().filter(|a| !looks_truncated(a)))
        .or_else(|| longest(&mut abstracts.iter()));

    // Counts: the maximum, attributed (earliest / highest-priority source on ties).
    if let Some((_, m)) = order
        .iter()
        .enumerate()
        .filter(|(_, m)| m.work.cited_by.is_some())
        .max_by_key(|(i, m)| (m.work.cited_by, std::cmp::Reverse(*i)))
    {
        out.cited_by = m.work.cited_by;
        out.cited_by_source = m
            .work
            .cited_by_source
            .clone()
            .or_else(|| Some(primary_source(&m.work).to_owned()).filter(|s| !s.is_empty()));
    }
    out.influential_citations = order
        .iter()
        .filter_map(|m| m.work.influential_citations)
        .max();

    // Open access: best tier wins; missing URL / PDF / licence / structured
    // format are backfilled from lower open tiers.
    let mut tiers: Vec<(u8, usize, &OaLocation)> = members
        .iter()
        .enumerate()
        .filter_map(|(i, m)| Some((oa_tier(m)?, i, m.work.oa.as_ref()?)))
        .collect();
    tiers.sort_by_key(|(t, i, _)| (*t, *i));
    if let Some((tier, _, best)) = tiers.first() {
        let mut oa = (*best).clone();
        if *tier < 9 {
            for (t, _, other) in tiers.iter().skip(1) {
                if *t >= 9 {
                    break;
                }
                if oa.url.as_deref().is_none_or(|s| s.trim().is_empty()) {
                    oa.url.clone_from(&other.url);
                }
                if oa.pdf_url.as_deref().is_none_or(|s| s.trim().is_empty()) {
                    oa.pdf_url.clone_from(&other.pdf_url);
                }
                if oa.license.as_deref().is_none_or(|s| s.trim().is_empty()) {
                    oa.license.clone_from(&other.license);
                }
                if oa
                    .structured_format
                    .as_deref()
                    .is_none_or(|s| s.trim().is_empty())
                {
                    oa.structured_format.clone_from(&other.structured_format);
                }
            }
        }
        out.oa = Some(oa);
    }

    out.retracted = members.iter().any(|m| m.work.retracted);
    out.retraction_reason = order.iter().find_map(|m| m.work.retraction_reason.clone());

    out.url = order
        .iter()
        .map(|m| m.work.url.as_str())
        .find(|u| u.starts_with("https://"))
        .or_else(|| {
            order
                .iter()
                .map(|m| m.work.url.as_str())
                .find(|u| !u.is_empty())
        })
        .unwrap_or_default()
        .to_owned();
    if out.url.is_empty() {
        if let Some(d) = &ids.doi {
            out.url = format!("https://doi.org/{d}");
        } else if let Some(a) = &ids.arxiv {
            out.url = format!("https://arxiv.org/abs/{a}");
        }
    }
    out.ids = ids;

    for m in &order {
        for s in &m.work.sources {
            if !out.sources.contains(s) {
                out.sources.push(s.clone());
            }
        }
    }
    out
}

/// Merge plain records (all treated as search/lookup results).
#[cfg(test)]
pub(crate) fn merge_cluster(works: Vec<ScholarlyWork>) -> ScholarlyWork {
    let members: Vec<Member> = works.into_iter().map(Member::primary).collect();
    merge_members(&members)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work(src: &str, title: &str) -> ScholarlyWork {
        let mut w = ScholarlyWork::new(title, "");
        w.sources = vec![src.into()];
        w.work_type = "article".into();
        w
    }

    #[test]
    fn field_priorities_follow_the_spec() {
        let mut arxiv = work("arxiv", "Attention Is All You Need");
        arxiv.year = Some(2017);
        arxiv.published_date = Some("2017-06-12".into());
        arxiv.work_type = "preprint".into();
        arxiv.ids.arxiv = Some("1706.03762".into());
        arxiv.abstract_text = Some(
            "The dominant sequence transduction models are based on complex recurrent networks."
                .into(),
        );
        arxiv.venue = Some("arXiv".into());
        let mut crossref = work("crossref", "Attention is All you Need");
        crossref.year = Some(2017);
        crossref.venue = Some("NeurIPS".into());
        crossref.ids.doi = Some("10.5555/3295222.3295349".into());
        crossref.cited_by = Some(50);
        crossref.cited_by_source = Some("crossref".into());
        crossref.abstract_text = Some("The dominant sequence...".into());
        let mut openaire = work("openaire", "attention is all you need");
        openaire.cited_by = Some(900);
        openaire.authors = vec![AuthorRef::new("Ashish Vaswani")];
        let merged = merge_cluster(vec![arxiv, openaire, crossref]);
        assert_eq!(merged.title, "Attention is All you Need");
        assert_eq!(merged.venue.as_deref(), Some("NeurIPS"));
        assert_eq!(merged.year, Some(2017));
        assert_eq!(merged.published_date.as_deref(), Some("2017-06-12"));
        assert_eq!(merged.ids.doi.as_deref(), Some("10.5555/3295222.3295349"));
        assert_eq!(merged.ids.arxiv.as_deref(), Some("1706.03762"));
        assert_eq!(
            (merged.cited_by, merged.cited_by_source.as_deref()),
            (Some(900), Some("openaire"))
        );
        assert!(
            merged
                .abstract_text
                .unwrap()
                .starts_with("The dominant sequence transduction")
        );
        assert_eq!(
            merged.work_type, "article",
            "a published version outranks the preprint type"
        );
        let kinds: Vec<&str> = merged.versions.iter().map(|v| v.kind.as_str()).collect();
        assert_eq!(kinds, ["published", "preprint"]);
        assert_eq!(merged.sources, ["crossref", "arxiv", "openaire"]);
        assert_eq!(merged.authors[0].name, "Ashish Vaswani");
    }

    #[test]
    fn enrichment_fills_gaps_but_does_not_rewrite_the_year() {
        let mut arxiv = work("arxiv", "Attention Is All You Need");
        arxiv.year = Some(2017);
        arxiv.ids.arxiv = Some("1706.03762".into());
        let mut oa = work("openalex", "Attention Is All You Need");
        oa.year = Some(2025);
        oa.venue = Some("arXiv (Cornell University)".into());
        oa.cited_by = Some(6000);
        oa.cited_by_source = Some("openalex".into());
        oa.retracted = false;
        let merged = merge_members(&[Member::primary(arxiv), Member::enrichment(oa)]);
        assert_eq!(merged.year, Some(2017));
        assert_eq!(merged.venue.as_deref(), Some("arXiv (Cornell University)"));
        assert_eq!(merged.cited_by, Some(6000));
    }

    #[test]
    fn oa_tiers_and_closed() {
        let mut core = work("core_ac", "T");
        core.oa = Some(OaLocation::new("green"));
        let mut oalex = work("openalex", "T");
        let mut loc = OaLocation::new("gold");
        loc.license = Some("cc-by".into());
        oalex.oa = Some(loc);
        let mut s2 = work("semanticscholar", "T");
        let mut l2 = OaLocation::new("green");
        l2.pdf_url = Some("https://x/pdf".into());
        s2.oa = Some(l2);
        let mut crossref = work("crossref", "T");
        crossref.oa = Some(OaLocation::new("closed"));
        let merged = merge_cluster(vec![core.clone(), crossref.clone(), s2, oalex]);
        let oa = merged.oa.unwrap();
        assert_eq!(oa.status, "gold");
        assert_eq!(oa.license.as_deref(), Some("cc-by"));
        assert_eq!(oa.pdf_url.as_deref(), Some("https://x/pdf"));
        let mut jats = work("europepmc", "T");
        let mut lj = OaLocation::new("green");
        lj.structured_format = Some("jats".into());
        jats.oa = Some(lj);
        let merged = merge_cluster(vec![core.clone(), jats]);
        assert_eq!(
            merged.oa.unwrap().structured_format.as_deref(),
            Some("jats")
        );
        // Closed only when nobody knows an open copy.
        let merged = merge_cluster(vec![crossref.clone(), core]);
        assert_eq!(merged.oa.unwrap().status, "green");
        assert_eq!(merge_cluster(vec![crossref]).oa.unwrap().status, "closed");
    }

    #[test]
    fn retraction_any_and_truncated_abstracts_lose() {
        let mut a = work("arxiv", "T");
        a.abstract_text = Some(
            "A much longer abstract that was cut off by the source because of a cap...".into(),
        );
        let mut b = work("crossref", "T");
        b.abstract_text = Some("A complete but shorter abstract.".into());
        b.retracted = true;
        b.retraction_reason = Some("Crossref retraction notice".into());
        let merged = merge_cluster(vec![a, b]);
        assert!(merged.retracted);
        assert_eq!(
            merged.abstract_text.as_deref(),
            Some("A complete but shorter abstract.")
        );
        assert!(merged.versions.is_empty());
    }

    #[test]
    fn prepare_cleans_and_tags() {
        let mut w = ScholarlyWork::new("  <i>Deep</i>  Residual ", "http://dx.doi.org/10.1234/X");
        w.ids.doi = Some("https://doi.org/10.1234/X".into());
        w.published_date = Some("2016-06-01".into());
        let w = prepare(w, Some("crossref"));
        assert_eq!(w.title, "Deep Residual");
        assert_eq!(w.url, "https://doi.org/10.1234/X");
        assert_eq!(w.ids.doi.as_deref(), Some("10.1234/x"));
        assert_eq!(w.year, Some(2016));
        assert_eq!(w.sources, ["crossref"]);
        assert_eq!(w.work_type, "article");
    }

    #[test]
    fn oa_backfills_url_and_structured_format_from_lower_tiers() {
        let mut s2 = work("semanticscholar", "Attention Is All You Need");
        let mut s2_oa = OaLocation::new("green");
        s2_oa.pdf_url = Some("https://arxiv.org/pdf/1706.03762".into());
        s2.oa = Some(s2_oa);

        let mut arxiv = work("arxiv", "Attention Is All You Need");
        let mut ax_oa = OaLocation::new("green");
        ax_oa.url = Some("https://arxiv.org/abs/1706.03762".into());
        ax_oa.structured_format = Some("html5".into());
        arxiv.oa = Some(ax_oa);

        let merged = merge_cluster(vec![s2, arxiv]);
        let oa = merged.oa.unwrap();
        assert_eq!(oa.url.as_deref(), Some("https://arxiv.org/abs/1706.03762"));
        assert_eq!(
            oa.pdf_url.as_deref(),
            Some("https://arxiv.org/pdf/1706.03762")
        );
        assert_eq!(oa.structured_format.as_deref(), Some("html5"));
    }

    #[test]
    fn coauthor_same_surname_never_copies_orcid_and_cited_by_ties_prefer_priority() {
        let mut crossref = work("crossref", "Joint Study");
        crossref.cited_by = Some(100);
        crossref.cited_by_source = Some("crossref".into());
        let mut wei = AuthorRef::new("Wei Wang");
        wei.orcid = Some("0000-0001-1111-1111".into());
        wei.affiliation = Some("MIT".into());
        let ming = AuthorRef::new("Ming Wang");
        crossref.authors = vec![wei, ming];

        let mut openaire = work("openaire", "Joint Study");
        openaire.cited_by = Some(100);
        openaire.cited_by_source = Some("openaire".into());
        openaire.authors = vec![AuthorRef::new("Wei Wang"), AuthorRef::new("Ming Wang")];

        let merged = merge_cluster(vec![openaire, crossref]);
        assert_eq!(
            (merged.cited_by, merged.cited_by_source.as_deref()),
            (Some(100), Some("crossref"))
        );
        assert_eq!(
            merged.authors[0].orcid.as_deref(),
            Some("0000-0001-1111-1111")
        );
        assert_eq!(merged.authors[0].affiliation.as_deref(), Some("MIT"));
        assert_eq!(merged.authors[1].orcid, None);
        assert_eq!(merged.authors[1].affiliation, None);

        // Same surname AND same first initial ("Wei Wang" vs "Wu Wang") do not collide either.
        let mut a = work("crossref", "T");
        a.authors = vec![AuthorRef::new("Wei Wang"), AuthorRef::new("Wu Wang")];
        let mut b = work("openalex", "T");
        let mut w_only = AuthorRef::new("W. Wang");
        w_only.orcid = Some("0000-0002-2222-2222".into());
        b.authors = vec![w_only];
        let merged = merge_cluster(vec![a, b]);
        assert_eq!(merged.authors[0].orcid, None);
        assert_eq!(merged.authors[1].orcid, None);
    }

    #[test]
    fn empty_oa_status_is_not_open_and_non_crossref_preprints_stay_preprint() {
        let mut empty = work("openalex", "T");
        empty.oa = Some(OaLocation::new("  "));
        let mut closed = work("crossref", "T");
        closed.oa = Some(OaLocation::new("closed"));
        assert_eq!(
            merge_cluster(vec![empty.clone(), closed])
                .oa
                .unwrap()
                .status,
            "closed"
        );
        assert!(merge_cluster(vec![empty]).oa.is_none());

        // Europe PMC PPR preprint with an unlisted preprint DOI stays a preprint.
        let mut epmc = work("europepmc", "Novel Preprint");
        epmc.work_type = "preprint".into();
        epmc.ids.doi = Some("10.9999/custom-preprint.1".into());
        let merged = merge_cluster(vec![epmc]);
        assert_eq!(merged.work_type, "preprint");

        // arXiv record carrying a published <arxiv:doi> still upgrades to published.
        let mut ax = work("arxiv", "Published From Arxiv");
        ax.work_type = "preprint".into();
        ax.ids.arxiv = Some("1512.03385".into());
        ax.ids.doi = Some("10.1109/cvpr.2016.90".into());
        let merged = merge_cluster(vec![ax]);
        assert_eq!(merged.work_type, "article");
        let kinds: Vec<&str> = merged.versions.iter().map(|v| v.kind.as_str()).collect();
        assert_eq!(kinds, ["preprint", "published"]);
    }
}
