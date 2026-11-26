//! Identity resolution behind `work` / `resolve` (SPEC.md §6 "Identity
//! Resolution & `arXiv:1706.03762` Fix").
//!
//! * DOI: Crossref + OpenAlex `$0` + S2 in parallel; PMC idconv only when
//!   OpenAlex could not say (lazy).
//! * arXiv: arXiv Atom `id_list` + S2 `ARXIV:` + OpenAlex via the DataCite
//!   DOI in parallel, then the published version: by the DOI any of them
//!   reports, else by an EXACT (normalised) title + first-author match in a
//!   Crossref bibliographic query. The fuzzy title gate is only for typed
//!   titles; a registry title must match exactly (a sequel or a prefix is
//!   another paper).
//! * PMID / PMCID: idconv → the DOI flow; without a DOI, OpenAlex + S2.
//! * OpenAlex W-id / S2 id: that record, then its DOI.
//! * Title: a scholarly search, then the first result (relevance order)
//!   whose title clears the [`crate::fuzzy::TITLE_MIN`] gate; the rejected
//!   titles come back as near misses.

use crate::ScholarCore;
use crate::fuzzy::{
    TITLE_MIN, best_title_match, exact_key, surname_candidates, surname_key, surnames_match,
    title_score,
};
use crate::ids::{Pid, is_preprint_doi};
use crate::lookup::{Fetched, Http, LookupCx, Provider};
use crate::merge::{Member, merge_members, prepare_parsed};
use crate::parse::{
    arxiv_feed, crossref_items, crossref_work, idconv_ids, openalex_work, s2_paper,
};
use fsm_core::{CallCx, ScholarQuery, ScholarlyWork, WorkIds};
use serde_json::Value;

/// Outcome of resolving an identifier or title.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct Resolution {
    /// How the input was read.
    pub pid: Pid,
    /// The merged work, when one was found.
    pub work: Option<ScholarlyWork>,
    /// Diagnostics (failed lookups, how a version was linked, ...).
    pub notes: Vec<String>,
    /// Titles a title lookup considered and rejected (at most three).
    pub near_misses: Vec<String>,
    /// Served from the local scholar cache.
    pub cached: bool,
    /// No upstream answered at all (every lookup failed or was benched):
    /// "not found" is then unknown rather than established.
    pub upstream_failed: bool,
    /// Some lookups failed or were benched: the record may lack fields
    /// (it is then not written to the scholar cache).
    pub partial: bool,
}

impl Resolution {
    pub fn new(pid: Pid) -> Self {
        Self {
            pid,
            ..Self::default()
        }
    }
}

/// Lookup outcomes of one resolution.
#[derive(Default)]
struct Gather {
    members: Vec<Member>,
    notes: Vec<String>,
    answered: usize,
    failed: usize,
}

impl Gather {
    /// Record one outcome; returns the parsed work when found.
    fn take(
        &mut self,
        p: Provider,
        f: Option<Fetched>,
        parse: impl Fn(&[u8]) -> Option<ScholarlyWork>,
    ) -> Option<ScholarlyWork> {
        self.take_at(p, f, parse, false)
    }

    /// Like [`Self::take`], but inserts a found record before any earlier
    /// record from the same provider (used when a published-DOI record should
    /// take priority over a preprint record from the same registry).
    fn take_front(
        &mut self,
        p: Provider,
        f: Option<Fetched>,
        parse: impl Fn(&[u8]) -> Option<ScholarlyWork>,
    ) -> Option<ScholarlyWork> {
        self.take_at(p, f, parse, true)
    }

    fn take_at(
        &mut self,
        p: Provider,
        f: Option<Fetched>,
        parse: impl Fn(&[u8]) -> Option<ScholarlyWork>,
        front: bool,
    ) -> Option<ScholarlyWork> {
        match f? {
            Fetched::Found(body) => {
                self.answered += 1;
                match parse(&body) {
                    Some(w) => {
                        let w = prepare_parsed(w, Some(p.id()));
                        let member = Member::primary(w.clone());
                        if front
                            && let Some(pos) = self
                                .members
                                .iter()
                                .position(|m| m.work.sources.first().is_some_and(|s| s == p.id()))
                        {
                            self.members.insert(pos, member);
                        } else {
                            self.members.push(member);
                        }
                        Some(w)
                    }
                    None => {
                        self.notes
                            .push(format!("{}: answered without a usable record", p.id()));
                        None
                    }
                }
            }
            Fetched::NotFound => {
                self.answered += 1;
                None
            }
            Fetched::Benched(until) => {
                self.failed += 1;
                self.notes.push(format!(
                    "{}: benched until {} after HTTP 429",
                    p.id(),
                    crate::lookup::rfc3339(until)
                ));
                None
            }
            Fetched::Failed(why) => {
                self.failed += 1;
                self.notes.push(format!("{}: {why}", p.id()));
                None
            }
        }
    }

    fn finish(self, pid: Pid, missing: impl FnOnce() -> String) -> Resolution {
        let mut r = Resolution::new(pid);
        r.notes = self.notes;
        r.partial = self.failed > 0;
        if self.members.is_empty() {
            r.upstream_failed = self.answered == 0;
            if !r.upstream_failed {
                r.notes.insert(0, missing());
            }
        } else {
            r.work = Some(merge_members(&self.members));
        }
        r
    }
}

fn json(body: &[u8]) -> Option<Value> {
    serde_json::from_slice(body).ok()
}

fn p_openalex(b: &[u8]) -> Option<ScholarlyWork> {
    json(b).as_ref().and_then(openalex_work)
}

fn p_s2(b: &[u8]) -> Option<ScholarlyWork> {
    json(b).as_ref().and_then(s2_paper)
}

fn p_crossref(b: &[u8]) -> Option<ScholarlyWork> {
    json(b).and_then(|v| v.get("message").and_then(crossref_work))
}

fn p_arxiv(b: &[u8]) -> Option<ScholarlyWork> {
    arxiv_feed(&String::from_utf8_lossy(b)).into_iter().next()
}

/// A non-preprint DOI any of these records reports.
fn published_doi<'a>(works: impl IntoIterator<Item = &'a ScholarlyWork>) -> Option<String> {
    works
        .into_iter()
        .filter_map(|w| w.ids.doi.clone())
        .find(|d| !is_preprint_doi(d))
}

impl ScholarCore {
    async fn fetch_opt(
        &self,
        http: &Http<'_>,
        p: Provider,
        req: Option<fsm_core::HttpRequest>,
        lcx: &LookupCx,
    ) -> Option<Fetched> {
        match req {
            Some(r) => Some(http.fetch(p, r, lcx).await),
            None => None,
        }
    }

    /// Resolve a non-title id (no cache).
    pub(crate) async fn resolve_pid(&self, pid: &Pid, cx: &CallCx) -> Resolution {
        match pid {
            Pid::Doi(d) => self.by_doi(d, cx).await,
            Pid::Arxiv(a) => self.by_arxiv(a, cx).await,
            Pid::Pmid(_) | Pid::Pmcid(_) => self.by_pubmed(pid, cx).await,
            Pid::OpenAlex(w) => self.by_openalex(w, cx).await,
            Pid::S2(_) => self.by_s2(pid, cx).await,
            Pid::Title(t) => self.by_title(t, cx).await,
        }
    }

    async fn by_doi(&self, doi: &str, cx: &CallCx) -> Resolution {
        let http = self.http();
        let lcx = self.lookup_cx(cx);
        let pid = Pid::Doi(doi.to_owned());
        let (cr, oa, s2) = tokio::join!(
            http.fetch(Provider::Crossref, http.crossref_request(doi), &lcx),
            self.fetch_opt(&http, Provider::OpenAlex, http.openalex_request(&pid), &lcx),
            self.fetch_opt(
                &http,
                Provider::SemanticScholar,
                http.s2_request(&pid),
                &lcx
            ),
        );
        let mut g = Gather::default();
        let crossref_missing = matches!(cr, Fetched::NotFound);
        g.take(Provider::Crossref, Some(cr), p_crossref);
        let openalex_answered = matches!(oa, Some(Fetched::Found(_) | Fetched::NotFound));
        g.take(Provider::OpenAlex, oa, p_openalex);
        g.take(Provider::SemanticScholar, s2, p_s2);
        if crossref_missing && !g.members.is_empty() {
            g.notes.insert(
                0,
                format!(
                    "Crossref has no record of {doi}: it is probably registered with DataCite or another agency, so Crossref retraction notices do not cover it."
                ),
            );
        }
        let mut r = g.finish(pid, || {
            format!(
                "No record of DOI {doi} in Crossref, OpenAlex or Semantic Scholar; it may be unregistered or mistyped."
            )
        });
        if let Some(w) = r.work.as_mut()
            && w.ids.pmid.is_none()
            && w.ids.pmcid.is_none()
            && !openalex_answered
            && let Fetched::Found(body) = http
                .fetch(Provider::PmcIdconv, http.idconv_request(doi), &lcx)
                .await
            && let Some(ids) = json(&body).as_ref().and_then(idconv_ids)
        {
            fill_ids(&mut w.ids, &ids);
        }
        r
    }

    async fn by_arxiv(&self, id: &str, cx: &CallCx) -> Resolution {
        let http = self.http();
        let lcx = self.lookup_cx(cx);
        let pid = Pid::Arxiv(id.to_owned());
        let (ax, s2, oa) = tokio::join!(
            http.fetch(Provider::Arxiv, http.arxiv_request(id), &lcx),
            self.fetch_opt(
                &http,
                Provider::SemanticScholar,
                http.s2_request(&pid),
                &lcx
            ),
            self.fetch_opt(&http, Provider::OpenAlex, http.openalex_request(&pid), &lcx),
        );
        let mut g = Gather::default();
        let arxiv_found = matches!(ax, Fetched::Found(_));
        let ax_work = g.take(Provider::Arxiv, Some(ax), p_arxiv);
        let arxiv_empty = arxiv_found && ax_work.is_none();
        if arxiv_empty {
            // An Atom feed with no entry: arXiv has no such id.
            g.notes.retain(|n| !n.starts_with("arxiv:"));
        }
        let s2_not_found = matches!(s2, Some(Fetched::NotFound));
        let s2_work = g.take(Provider::SemanticScholar, s2, p_s2);
        let oa_missing = matches!(oa, Some(Fetched::NotFound));
        let oa_benched_or_failed = matches!(oa, Some(Fetched::Benched(_) | Fetched::Failed(_)));
        let oa_work = g.take(Provider::OpenAlex, oa, p_openalex);
        if oa_missing && ax_work.is_some() {
            g.notes.push(format!(
                "OpenAlex has no record of arXiv:{id}; recent preprints may not be indexed yet, so citation counts may be missing."
            ));
        }
        let found: Vec<&ScholarlyWork> = [&ax_work, &s2_work, &oa_work]
            .into_iter()
            .flatten()
            .collect();
        if let Some(doi) = published_doi(found.iter().copied()) {
            let need_openalex = !oa_benched_or_failed
                && !oa_work
                    .as_ref()
                    .is_some_and(|w| w.ids.doi.as_deref() == Some(doi.as_str()));
            let doi_pid = Pid::Doi(doi.clone());
            let (cr, oa2, s2_2) = tokio::join!(
                http.fetch(Provider::Crossref, http.crossref_request(&doi), &lcx),
                self.fetch_opt(
                    &http,
                    Provider::OpenAlex,
                    need_openalex
                        .then(|| http.openalex_request(&doi_pid))
                        .flatten(),
                    &lcx
                ),
                self.fetch_opt(
                    &http,
                    Provider::SemanticScholar,
                    s2_not_found.then(|| http.s2_request(&doi_pid)).flatten(),
                    &lcx
                ),
            );
            g.take(Provider::Crossref, Some(cr), p_crossref);
            if g.take_front(Provider::OpenAlex, oa2, p_openalex).is_some() {
                g.notes.retain(|n| !n.contains("may not be indexed yet"));
            }
            g.take(Provider::SemanticScholar, s2_2, p_s2);
        } else if let Some(base) = ax_work.as_ref().or(s2_work.as_ref()) {
            let surname = base.authors.first().and_then(|a| surname_key(&a.name));
            let req = http.crossref_title_request(&base.title, surname.as_deref());
            match http.fetch(Provider::Crossref, req, &lcx).await {
                Fetched::Found(body) => {
                    let items = json(&body).map(|v| crossref_items(&v)).unwrap_or_default();
                    match pick_published_version(base, &items) {
                        Some(hit) => {
                            g.notes.push(format!(
                                "Linked the published version {} by exact title and first author (Crossref).",
                                hit.ids.doi.as_deref().unwrap_or("?")
                            ));
                            let hit_doi = hit.ids.doi.clone();
                            g.members.push(Member::primary(prepare_parsed(
                                hit.clone(),
                                Some("crossref"),
                            )));
                            if let Some(doi) = hit_doi {
                                let need_openalex = !oa_benched_or_failed
                                    && !oa_work.as_ref().is_some_and(|w| {
                                        w.ids.doi.as_deref() == Some(doi.as_str())
                                    });
                                let doi_pid = Pid::Doi(doi);
                                let (oa2, s2_2) = tokio::join!(
                                    self.fetch_opt(
                                        &http,
                                        Provider::OpenAlex,
                                        need_openalex
                                            .then(|| http.openalex_request(&doi_pid))
                                            .flatten(),
                                        &lcx
                                    ),
                                    self.fetch_opt(
                                        &http,
                                        Provider::SemanticScholar,
                                        s2_not_found.then(|| http.s2_request(&doi_pid)).flatten(),
                                        &lcx
                                    ),
                                );
                                if g.take_front(Provider::OpenAlex, oa2, p_openalex).is_some() {
                                    g.notes.retain(|n| !n.contains("may not be indexed yet"));
                                }
                                g.take(Provider::SemanticScholar, s2_2, p_s2);
                            }
                        }
                        None => {
                            if let Some(closest) = items.iter().find(|c| !c.title.is_empty()) {
                                g.notes.push(format!(
                                    "No published version titled exactly '{}' in Crossref (closest: '{}'); kept the arXiv record.",
                                    base.title, closest.title
                                ));
                            }
                        }
                    }
                }
                Fetched::Failed(why) => {
                    g.failed += 1;
                    g.notes.push(format!("crossref (published version): {why}"));
                }
                Fetched::NotFound | Fetched::Benched(_) => {}
            }
        }
        g.finish(pid, || {
            if arxiv_empty {
                format!("arXiv returned no record for '{id}'; check the identifier.")
            } else {
                format!("No record of arXiv:{id} in arXiv, Semantic Scholar or OpenAlex.")
            }
        })
    }

    async fn by_pubmed(&self, pid: &Pid, cx: &CallCx) -> Resolution {
        let http = self.http();
        let lcx = self.lookup_cx(cx);
        let conv = http
            .fetch(Provider::PmcIdconv, http.idconv_request(pid.value()), &lcx)
            .await;
        let ids = match &conv {
            Fetched::Found(b) => json(b).as_ref().and_then(idconv_ids),
            _ => None,
        };
        if let Some(doi) = ids.as_ref().and_then(|i| i.doi.clone()) {
            let mut r = Box::pin(self.by_doi(&doi, cx)).await;
            r.pid = pid.clone();
            if let (Some(w), Some(ids)) = (r.work.as_mut(), ids.as_ref()) {
                fill_ids(&mut w.ids, ids);
            }
            return r;
        }
        let (oa, s2) = tokio::join!(
            self.fetch_opt(&http, Provider::OpenAlex, http.openalex_request(pid), &lcx),
            self.fetch_opt(&http, Provider::SemanticScholar, http.s2_request(pid), &lcx),
        );
        let mut g = Gather::default();
        if let Fetched::Failed(why) = &conv {
            g.failed += 1;
            g.notes.push(format!("pmc_idconv: {why}"));
        }
        let oa_work = g.take(Provider::OpenAlex, oa, p_openalex);
        let s2_work = g.take(Provider::SemanticScholar, s2, p_s2);
        let found: Vec<&ScholarlyWork> = [&oa_work, &s2_work].into_iter().flatten().collect();
        if let Some(doi) = published_doi(found.iter().copied()) {
            let cr = http
                .fetch(Provider::Crossref, http.crossref_request(&doi), &lcx)
                .await;
            g.take(Provider::Crossref, Some(cr), p_crossref);
        }
        let label = format!("{} {}", pid.kind().to_ascii_uppercase(), pid.value());
        let mut r = g.finish(pid.clone(), || {
            format!("No record of {label} in PMC, OpenAlex or Semantic Scholar.")
        });
        if let (Some(w), Some(ids)) = (r.work.as_mut(), ids.as_ref()) {
            fill_ids(&mut w.ids, ids);
        }
        r
    }

    async fn by_openalex(&self, id: &str, cx: &CallCx) -> Resolution {
        let http = self.http();
        let lcx = self.lookup_cx(cx);
        let pid = Pid::OpenAlex(id.to_owned());
        let oa = self
            .fetch_opt(&http, Provider::OpenAlex, http.openalex_request(&pid), &lcx)
            .await;
        let mut g = Gather::default();
        let first = g.take(Provider::OpenAlex, oa, p_openalex);
        if let Some(w) = &first {
            if let Some(doi) = w.ids.doi.clone() {
                let (cr, s2) = tokio::join!(
                    http.fetch(Provider::Crossref, http.crossref_request(&doi), &lcx),
                    self.fetch_opt(
                        &http,
                        Provider::SemanticScholar,
                        http.s2_request(&Pid::Doi(doi.clone())),
                        &lcx
                    ),
                );
                g.take(Provider::Crossref, Some(cr), p_crossref);
                g.take(Provider::SemanticScholar, s2, p_s2);
            } else if let Some(a) = w.ids.arxiv.clone() {
                let arxiv_pid = Pid::Arxiv(a.clone());
                let (ax, s2) = tokio::join!(
                    http.fetch(Provider::Arxiv, http.arxiv_request(&a), &lcx),
                    self.fetch_opt(
                        &http,
                        Provider::SemanticScholar,
                        http.s2_request(&arxiv_pid),
                        &lcx
                    ),
                );
                g.take(Provider::Arxiv, Some(ax), p_arxiv);
                g.take(Provider::SemanticScholar, s2, p_s2);
            }
        }
        g.finish(pid, || format!("No OpenAlex work {id}."))
    }

    async fn by_s2(&self, pid: &Pid, cx: &CallCx) -> Resolution {
        let http = self.http();
        let lcx = self.lookup_cx(cx);
        let s2 = self
            .fetch_opt(&http, Provider::SemanticScholar, http.s2_request(pid), &lcx)
            .await;
        let mut g = Gather::default();
        let first = g.take(Provider::SemanticScholar, s2, p_s2);
        if let Some(w) = &first {
            if let Some(doi) = w.ids.doi.clone() {
                let (cr, oa) = tokio::join!(
                    http.fetch(Provider::Crossref, http.crossref_request(&doi), &lcx),
                    self.fetch_opt(
                        &http,
                        Provider::OpenAlex,
                        http.openalex_request(&Pid::Doi(doi.clone())),
                        &lcx
                    ),
                );
                g.take(Provider::Crossref, Some(cr), p_crossref);
                g.take(Provider::OpenAlex, oa, p_openalex);
            } else if let Some(a) = w.ids.arxiv.clone() {
                let ax = http
                    .fetch(Provider::Arxiv, http.arxiv_request(&a), &lcx)
                    .await;
                g.take(Provider::Arxiv, Some(ax), p_arxiv);
            }
        }
        g.finish(pid.clone(), || {
            format!("No Semantic Scholar paper {}.", pid.value())
        })
    }

    async fn by_title(&self, title: &str, cx: &CallCx) -> Resolution {
        let mut r = Resolution::new(Pid::Title(title.to_owned()));
        let mut q = ScholarQuery::new(title);
        q.limit = 5;
        let result = match self
            .keyword_search(&q, title, crate::rank::SortMode::Relevance, 5, cx)
            .await
        {
            Ok(res) => res,
            Err(e) => {
                r.notes.push(e.to_string());
                r.upstream_failed = true;
                return r;
            }
        };
        if result.works.is_empty() && result.meta.sources.is_empty() {
            r.upstream_failed = true;
        }
        r.notes = result.meta.notes;
        let titles = result.works.iter().map(|w| w.title.as_str());
        match best_title_match(title, titles) {
            Some(i) => r.work = result.works.into_iter().nth(i),
            None => {
                r.near_misses = result
                    .works
                    .iter()
                    .map(|w| w.title.clone())
                    .filter(|t| !t.trim().is_empty())
                    .take(3)
                    .collect();
                let mut note = format!(
                    "No work matched '{title}' confidently (title similarity < {TITLE_MIN}); try the exact title or a DOI."
                );
                if !r.near_misses.is_empty() {
                    let listed: Vec<String> =
                        r.near_misses.iter().map(|t| format!("'{t}'")).collect();
                    note.push_str(&format!(" Closest titles found: {}.", listed.join("; ")));
                }
                r.notes.insert(0, note);
            }
        }
        r
    }
}

/// The published version among Crossref candidates: exact-title matches
/// only, the most cited of them (first on ties).
fn pick_published_version<'a>(
    base: &ScholarlyWork,
    items: &'a [ScholarlyWork],
) -> Option<&'a ScholarlyWork> {
    items
        .iter()
        .enumerate()
        .filter(|(_, c)| same_registry_title(base, c))
        .max_by_key(|(i, c)| (c.cited_by.unwrap_or(0), std::cmp::Reverse(*i)))
        .map(|(_, c)| c)
}

/// A registry title must match EXACTLY (normalised), with the same first
/// author when both name one, and no earlier than the preprint's year − 1.
fn same_registry_title(base: &ScholarlyWork, cand: &ScholarlyWork) -> bool {
    if exact_key(&base.title) != exact_key(&cand.title) || exact_key(&cand.title).is_empty() {
        return false;
    }
    if let (Some(a), Some(b)) = (base.authors.first(), cand.authors.first())
        && !surname_candidates(&a.name).is_empty()
        && !surname_candidates(&b.name).is_empty()
        && !surnames_match(&a.name, &b.name)
    {
        return false;
    }
    if let (Some(y0), Some(y1)) = (base.year, cand.year)
        && y1 + 1 < y0
    {
        return false;
    }
    debug_assert!(title_score(&base.title, &cand.title) >= TITLE_MIN);
    true
}

fn fill_ids(dst: &mut WorkIds, src: &WorkIds) {
    for (d, s) in [
        (&mut dst.doi, &src.doi),
        (&mut dst.pmid, &src.pmid),
        (&mut dst.pmcid, &src.pmcid),
    ] {
        if d.is_none() {
            d.clone_from(s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsm_core::AuthorRef;

    fn w(title: &str, author: &str, year: u16) -> ScholarlyWork {
        let mut w = ScholarlyWork::new(title, "");
        w.authors = vec![AuthorRef::new(author)];
        w.year = Some(year);
        w
    }

    fn cited(title: &str, author: &str, year: u16, n: u64) -> ScholarlyWork {
        let mut c = w(title, author, year);
        c.cited_by = Some(n);
        c
    }

    const AIAYN: &str = "Attention Is All You Need";

    /// Port: the registry crosswalk never links a title that only starts
    /// with the preprint's title.
    #[test]
    fn test_arxiv_registry_title_does_not_resolve_to_a_prefix_only_match() {
        let base = w(AIAYN, "Ashish Vaswani", 2017);
        let wrong = w(
            "Attention Is All You Need In Speech Separation",
            "Ashish Vaswani",
            2017,
        );
        assert!(!same_registry_title(&base, &wrong));
        assert!(pick_published_version(&base, &[wrong]).is_none());
    }

    /// Port: a sequel ("... II") with more citations is another paper.
    #[test]
    fn test_arxiv_registry_title_does_not_pick_a_short_sequel_suffix() {
        let t = "BERT: Pre-training of Deep Bidirectional Transformers for Language Understanding";
        let base = w(t, "Jacob Devlin", 2018);
        let items = [
            cited(&format!("{t} II"), "Jacob Devlin", 2019, 1000),
            cited(t, "Jacob Devlin", 2019, 100),
        ];
        assert_eq!(
            pick_published_version(&base, &items).and_then(|c| c.cited_by),
            Some(100)
        );
    }

    /// Port: a far more cited title that merely extends the preprint's loses.
    #[test]
    fn test_arxiv_exact_match_does_not_promote_a_more_cited_title_prefix() {
        let base = w(AIAYN, "Ashish Vaswani", 2017);
        let items = [
            cited(
                &format!("{AIAYN} in speech separation"),
                "Ashish Vaswani",
                2017,
                1_000_000,
            ),
            cited(AIAYN, "Ashish Vaswani", 2017, 100),
        ];
        assert_eq!(
            pick_published_version(&base, &items).and_then(|c| c.cited_by),
            Some(100)
        );
    }

    /// Port: among records with the exact title, the most cited wins.
    #[test]
    fn test_among_records_with_the_exact_arxiv_title_the_most_cited_wins() {
        let base = w(AIAYN, "Ashish Vaswani", 2017);
        let items = [
            cited(AIAYN, "Ashish Vaswani", 2017, 12),
            cited(
                "Attention Is All You Need in speech separation",
                "Ashish Vaswani",
                2017,
                999_999,
            ),
            cited(AIAYN, "Vaswani, A.", 2017, 90_000),
        ];
        assert_eq!(
            pick_published_version(&base, &items).and_then(|c| c.cited_by),
            Some(90_000)
        );
    }

    /// Port (registry side of the fuzzy-module test of the same name):
    /// spacing and punctuation do not break an exact match.
    #[test]
    fn test_arxiv_exact_match_normalizes_spacing_and_punctuation() {
        let base = w(AIAYN, "Ashish Vaswani", 2017);
        for variant in [
            "Attention  Is  All  You  Need",
            "Attention — Is — All — You — Need",
        ] {
            let items = [
                cited(AIAYN, "Ashish Vaswani", 2017, 1),
                cited(variant, "A Vaswani", 2018, 1000),
            ];
            let hit = pick_published_version(&base, &items).unwrap();
            assert_eq!(hit.title, variant);
        }
    }

    #[test]
    fn registry_match_also_checks_author_and_year() {
        let base = w(AIAYN, "Ashish Vaswani", 2017);
        assert!(!same_registry_title(
            &base,
            &w("Attention is all you need", "Jane Doe", 2023)
        ));
        assert!(!same_registry_title(
            &base,
            &w("Attention is all you need", "Vaswani A", 2012)
        ));
        assert!(same_registry_title(
            &base,
            &w("Attention is all you need.", "Vaswani, A.", 2018)
        ));
    }
}
