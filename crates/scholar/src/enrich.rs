//! Top-N single-id enrichment (SPEC.md §6, I16): OpenAlex `$0` and
//! Semantic Scholar single-paper lookups for the best-ranked works, at most
//! [`crate::Timing::enrich_concurrency`] in flight, all under one
//! [`crate::Timing::enrich_deadline`]; results that arrive in time are kept,
//! the rest are dropped. Responses are cached 30 days (see `lookup`), and a
//! 429 benches the provider until UTC midnight.

use crate::ScholarCore;
use crate::ids::Pid;
use crate::lookup::{Fetched, LookupCx, Provider, rfc3339};
use crate::merge::{Member, merge_members, prepare_parsed, primary_source};
use crate::parse::{openalex_work, s2_paper};
use crate::rank::Ranked;
use fsm_core::{CallCx, WorkIds};
use futures::StreamExt;
use std::collections::BTreeSet;

/// The id OpenAlex should be asked for (W-id > DOI > PMID > PMCID > arXiv).
pub(crate) fn openalex_pid(ids: &WorkIds) -> Option<Pid> {
    ids.openalex
        .clone()
        .map(Pid::OpenAlex)
        .or_else(|| ids.doi.clone().map(Pid::Doi))
        .or_else(|| ids.pmid.clone().map(Pid::Pmid))
        .or_else(|| ids.pmcid.clone().map(Pid::Pmcid))
        .or_else(|| ids.arxiv.clone().map(Pid::Arxiv))
}

/// The id Semantic Scholar should be asked for (paperId > DOI > arXiv > PMID > PMCID).
pub(crate) fn s2_pid(ids: &WorkIds) -> Option<Pid> {
    ids.s2
        .clone()
        .map(Pid::S2)
        .or_else(|| ids.doi.clone().map(Pid::Doi))
        .or_else(|| ids.arxiv.clone().map(Pid::Arxiv))
        .or_else(|| ids.pmid.clone().map(Pid::Pmid))
        .or_else(|| ids.pmcid.clone().map(Pid::Pmcid))
}

impl ScholarCore {
    /// Enrich the candidates at `picks` in place (members appended, work
    /// re-merged), never asking a provider in `skip`. Returns whether every
    /// launched lookup finished without failing (benched providers are
    /// skipped, not failures).
    pub(crate) async fn enrich(
        &self,
        cands: &mut [Ranked],
        picks: &[usize],
        skip: &BTreeSet<Provider>,
        cx: &CallCx,
        notes: &mut Vec<String>,
        used: &mut BTreeSet<String>,
    ) -> bool {
        let http = self.http();
        let now = (self.clock)();
        let mut benched_note = BTreeSet::new();
        let mut tasks = Vec::new();
        for &i in picks {
            let Some(c) = cands.get(i) else { continue };
            for p in [Provider::OpenAlex, Provider::SemanticScholar] {
                if skip.contains(&p) || c.members.iter().any(|m| primary_source(&m.work) == p.id())
                {
                    continue;
                }
                if let Some(until) = self.bench.until(p, &self.deps, now).await {
                    if benched_note.insert(p) {
                        notes.push(format!(
                            "{}: benched until {} after HTTP 429; enrichment skipped",
                            p.id(),
                            rfc3339(until)
                        ));
                    }
                    continue;
                }
                let pid = match p {
                    Provider::OpenAlex => openalex_pid(&c.work.ids),
                    _ => s2_pid(&c.work.ids),
                };
                let req = pid.and_then(|pid| match p {
                    Provider::OpenAlex => http.openalex_request(&pid),
                    _ => http.s2_request(&pid),
                });
                if let Some(req) = req {
                    tasks.push((i, p, req));
                }
            }
        }
        if tasks.is_empty() {
            return benched_note.is_empty();
        }
        let budget = self.timing.enrich_deadline.min(cx.remaining());
        let lcx = LookupCx {
            cancel: cx.cancel.child_token(),
            timeout: budget.max(std::time::Duration::from_millis(1)),
        };
        let total = tasks.len();
        let http = &http;
        let lcx_ref = &lcx;
        let mut stream = futures::stream::iter(tasks)
            .map(|(i, p, req)| async move { (i, p, http.fetch(p, req, lcx_ref).await) })
            .buffer_unordered(self.timing.enrich_concurrency.max(1));
        let deadline = tokio::time::sleep(budget);
        tokio::pin!(deadline);
        let mut done = 0usize;
        let mut touched = BTreeSet::new();
        let mut failures: Vec<String> = Vec::new();
        loop {
            tokio::select! {
                biased;
                next = stream.next() => {
                    let Some((i, p, fetched)) = next else { break };
                    done += 1;
                    match fetched {
                        Fetched::Found(body) => {
                            let parsed = serde_json::from_slice::<serde_json::Value>(&body)
                                .ok()
                                .and_then(|v| match p {
                                    Provider::OpenAlex => openalex_work(&v),
                                    _ => s2_paper(&v),
                                });
                            if let Some(w) = parsed {
                                cands[i].members.push(Member::enrichment(prepare_parsed(
                                    w,
                                    Some(p.id()),
                                )));
                                touched.insert(i);
                                used.insert(p.id().to_owned());
                            }
                        }
                        Fetched::Failed(why) => failures.push(format!("{}: {why}", p.id())),
                        Fetched::Benched(until) => {
                            if benched_note.insert(p) {
                                notes.push(format!(
                                    "{}: benched until {} after HTTP 429; enrichment skipped",
                                    p.id(),
                                    rfc3339(until)
                                ));
                            }
                        }
                        Fetched::NotFound => {}
                    }
                }
                () = &mut deadline => break,
            }
        }
        drop(stream);
        lcx.cancel.cancel();
        failures.sort();
        failures.dedup();
        let failed = !failures.is_empty();
        notes.extend(failures.into_iter().map(|f| format!("enrichment {f}")));
        if done < total {
            notes.push(format!(
                "enrichment: {done} of {total} lookups finished within {} ms",
                budget.as_millis()
            ));
        }
        for i in touched {
            cands[i].work = merge_members(&cands[i].members);
        }
        done == total && !failed && benched_note.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enrichment_id_preferences() {
        let mut ids = WorkIds::default();
        ids.arxiv = Some("1706.03762".into());
        assert_eq!(openalex_pid(&ids), Some(Pid::Arxiv("1706.03762".into())));
        assert_eq!(s2_pid(&ids), Some(Pid::Arxiv("1706.03762".into())));
        ids.doi = Some("10.1/x".into());
        ids.s2 = Some("abc".into());
        assert_eq!(openalex_pid(&ids), Some(Pid::Doi("10.1/x".into())));
        assert_eq!(s2_pid(&ids), Some(Pid::S2("abc".into())));
        assert_eq!(openalex_pid(&WorkIds::default()), None);
    }
}
