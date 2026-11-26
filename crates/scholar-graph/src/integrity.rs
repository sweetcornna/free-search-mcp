//! Research integrity fusion (`ScholarGraph::integrity`, `SPEC.md` §6).
//!
//! Combines:
//! 1. Local Retraction Watch SQLite table (lazily synced in background via `rw_sync`)
//! 2. Crossref `updated-by` / `relation.is-retracted-by`
//! 3. OpenAlex `$0` single-work lookup (`is_retracted`, `fwci`, journal ISSNs)
//! 4. `scite.tallies` via `Arc<dyn SourcesApi>` (supporting / contradicting / mentioning)
//! 5. DOAJ vetted open-access journal whitelist check (`doaj.journals` via `SourcesApi`,
//!    with direct DOAJ HTTP API and OpenAlex `is_in_doaj` fallbacks)

use crate::graph::{
    CrossrefCheck, dedupe_retraction_notices, encode_doi_path, encode_segment,
    fetch_crossref_status, get_json_with_status, openalex_url,
};
use crate::ids::{TITLE_MATCH_MIN, extract_issn, is_retraction_nature, normalize_doi, title_score};
use fsm_core::{
    CallCx, FsmError, IntegrityReport, RetractionNotice, SciteTallies, SourceDeps, SourcesApi,
};
use fsm_scholar::ScholarCore;
use fsm_scholar::ids::{Pid, classify as classify_scholar_pid};
use serde_json::{Value, json};
use std::sync::Arc;

const OPENALEX_WORKS: &str = "https://api.openalex.org/works";
const DOAJ_JOURNALS: &str = "https://doaj.org/api/search/journals";

#[derive(Debug, Default)]
struct OpenAlexIntegrity {
    doi: Option<String>,
    is_retracted: bool,
    fwci: Option<f64>,
    issns: Vec<String>,
    is_in_doaj: Option<bool>,
    /// True when OpenAlex answered authoritatively (HTTP 2xx valid JSON or HTTP 404/410).
    answered: bool,
    last_status: Option<u16>,
}

/// Core `ScholarGraph::integrity` implementation.
pub async fn run_integrity(
    core: &ScholarCore,
    deps: &SourceDeps,
    sources: &Arc<dyn SourcesApi>,
    doi_or_id: &str,
    cx: &CallCx,
) -> Result<IntegrityReport, FsmError> {
    let raw = doi_or_id.trim();
    if raw.is_empty() {
        return Err(FsmError::invalid("doi_or_id must not be empty"));
    }
    if cx.cancel.is_cancelled() {
        return Err(FsmError::Timeout(0));
    }

    let pid = classify_scholar_pid(raw);
    let (resolved_doi, preloaded_oa, resolved_retracted) =
        resolve_doi_and_openalex(core, deps, raw, &pid, cx).await?;

    let Some(doi) = resolved_doi else {
        // No DOI could be resolved, but OpenAlex / ScholarCore may still have retraction/FWCI signals.
        let mut report = IntegrityReport::new(None);
        report.is_retracted = resolved_retracted;
        if let Some(oa) = preloaded_oa {
            report.is_retracted |= oa.is_retracted;
            report.openalex_fwci = oa.fwci;
            report.doaj_vetted = oa.is_in_doaj;
        }
        return Ok(report);
    };

    // Concurrent fusion across all DOI-keyed sources plus local RW sync-state check.
    let local_rw_fut = async { deps.store.retraction_lookup(&doi).await.ok().flatten() };
    let rw_synced_fut = async {
        deps.store
            .sync_state_get(crate::rw_sync::RW_SYNC_NAME)
            .await
            .ok()
            .flatten()
            .is_some_and(|(_, _, finished_at)| finished_at.is_some())
    };
    let crossref_fut = fetch_crossref_status(deps, &doi, cx);
    let openalex_fut = async {
        if let Some(oa) = preloaded_oa {
            return Ok(oa);
        }
        fetch_openalex_integrity_by_doi(core, deps, &doi, cx).await
    };
    let scite_fut = fetch_scite_tallies(sources, &doi, cx);

    let (local_rw, rw_synced, crossref_res, openalex_res, scite) = tokio::join!(
        local_rw_fut,
        rw_synced_fut,
        crossref_fut,
        openalex_fut,
        scite_fut
    );

    let crossref: CrossrefCheck = crossref_res?;
    let openalex: OpenAlexIntegrity = openalex_res?;

    // R2 / I3: Never silently certify a paper as clean (`is_retracted = false`) when every
    // authoritative integrity source failed and local Retraction Watch has no completed sync.
    let crossref_answered = crossref.registered.is_some();
    let openalex_answered = openalex.answered;
    let rw_authoritative = rw_synced || local_rw.is_some();
    if !crossref_answered && !openalex_answered && !rw_authoritative {
        let status = crossref.last_status.or(openalex.last_status);
        return Err(FsmError::upstream(
            "integrity",
            status,
            format!("all integrity sources failed for '{doi}'"),
        ));
    }

    // Collect candidate ISSNs from OpenAlex and Crossref for DOAJ whitelist verification.
    let mut issns = Vec::new();
    for issn in openalex.issns.iter().chain(crossref.issns.iter()) {
        if let Some(norm) = extract_issn(issn)
            && !issns.contains(&norm)
        {
            issns.push(norm);
        }
    }

    let doaj_vetted = check_doaj_vetted(deps, sources, &issns, openalex.is_in_doaj, cx).await?;

    // Fuse notices from local Retraction Watch + Crossref.
    let mut notices: Vec<RetractionNotice> = Vec::new();
    if let Some(rw) = local_rw {
        notices.push(rw);
    }
    notices.extend(crossref.notices);
    let notices = dedupe_retraction_notices(notices);

    let is_retracted = resolved_retracted
        || openalex.is_retracted
        || notices.iter().any(|n| is_retraction_nature(&n.nature));

    let mut report = IntegrityReport::new(Some(doi));
    report.is_retracted = is_retracted;
    report.retraction_notices = notices;
    report.scite = scite;
    report.doaj_vetted = doaj_vetted;
    report.openalex_fwci = openalex.fwci;
    Ok(report)
}

async fn resolve_doi_and_openalex(
    core: &ScholarCore,
    deps: &SourceDeps,
    raw: &str,
    pid: &Pid,
    cx: &CallCx,
) -> Result<(Option<String>, Option<OpenAlexIntegrity>, bool), FsmError> {
    if let Pid::Doi(doi) = pid {
        return Ok((Some(doi.clone()), None, false));
    }

    // M10: Route non-DOI identifier / title resolution through `ScholarCore::work`
    // so `integrity`, `cite`, and `scholar.work` share the same crosswalk, 30-day
    // `scholar_works` cache, and OpenAlex/S2 429 bench.
    match core.work(raw, cx).await {
        Ok(Some(work)) => {
            // I1: When `work.ids.doi` is absent and we already have an OpenAlex W-ID,
            // load OpenAlex integrity metadata directly by W-ID BEFORE falling back to
            // DataCite `10.48550/arxiv.<id>` (which OpenAlex 404s).
            let preloaded_oa = if work.ids.doi.is_none()
                && let Some(ref wid) = work.ids.openalex
            {
                Some(fetch_openalex_integrity_by_wid(core, deps, wid, cx).await?)
            } else {
                None
            };
            let doi = work
                .ids
                .doi
                .clone()
                .or_else(|| preloaded_oa.as_ref().and_then(|oa| oa.doi.clone()))
                .or_else(|| {
                    if let Pid::Arxiv(aid) = pid {
                        Some(format!("10.48550/arxiv.{aid}"))
                    } else {
                        None
                    }
                });
            Ok((doi, preloaded_oa, work.retracted))
        }
        // I1: When `ScholarCore::work` confirmed the identifier does not exist, do not
        // fabricate a `10.48550/arxiv.<id>` DOI.
        Ok(None) => Ok((None, None, false)),
        Err(FsmError::Timeout(0)) if cx.cancel.is_cancelled() => Err(FsmError::Timeout(0)),
        Err(e) => {
            tracing::debug!(error = %e, raw, "ScholarCore::work unavailable for integrity");
            // Fallback for Title queries in harnesses with an empty `Registry`.
            if let Pid::Title(title) = pid {
                let url = openalex_url(
                    OPENALEX_WORKS,
                    &[("search", title), ("per-page", "5")],
                    deps,
                );
                let (status, payload) =
                    get_json_with_status(Some(core), deps, &url, "openalex", cx).await?;
                if let Some(val) = payload {
                    if let Some(matched) =
                        val.get("results")
                            .and_then(Value::as_array)
                            .and_then(|results| {
                                results.iter().find(|item| {
                                    item.get("display_name")
                                        .and_then(Value::as_str)
                                        .is_some_and(|dn| title_score(title, dn) >= TITLE_MATCH_MIN)
                                })
                            })
                    {
                        let oa = parse_openalex_integrity(matched);
                        return Ok((oa.doi.clone(), Some(oa), false));
                    }
                    return Ok((None, None, false));
                }
                if status == Some(404) {
                    return Ok((None, None, false));
                }
            }
            Err(e)
        }
    }
}

async fn fetch_openalex_integrity_by_wid(
    core: &ScholarCore,
    deps: &SourceDeps,
    wid: &str,
    cx: &CallCx,
) -> Result<OpenAlexIntegrity, FsmError> {
    let url = openalex_url(
        &format!("{OPENALEX_WORKS}/{}", encode_segment(wid)),
        &[],
        deps,
    );
    fetch_openalex_integrity_url(core, deps, &url, cx).await
}

async fn fetch_openalex_integrity_by_doi(
    core: &ScholarCore,
    deps: &SourceDeps,
    doi: &str,
    cx: &CallCx,
) -> Result<OpenAlexIntegrity, FsmError> {
    let url = openalex_url(
        &format!("{OPENALEX_WORKS}/https://doi.org/{}", encode_doi_path(doi)),
        &[],
        deps,
    );
    fetch_openalex_integrity_url(core, deps, &url, cx).await
}

async fn fetch_openalex_integrity_url(
    core: &ScholarCore,
    deps: &SourceDeps,
    url: &str,
    cx: &CallCx,
) -> Result<OpenAlexIntegrity, FsmError> {
    let (status, payload) = get_json_with_status(Some(core), deps, url, "openalex", cx).await?;
    if let Some(val) = payload {
        let mut oa = parse_openalex_integrity(&val);
        oa.answered = true;
        oa.last_status = status;
        return Ok(oa);
    }
    Ok(OpenAlexIntegrity {
        answered: matches!(status, Some(404 | 410)),
        last_status: status,
        ..OpenAlexIntegrity::default()
    })
}

fn parse_openalex_integrity(val: &Value) -> OpenAlexIntegrity {
    let mut out = OpenAlexIntegrity {
        doi: val
            .get("doi")
            .and_then(Value::as_str)
            .and_then(normalize_doi),
        is_retracted: val
            .get("is_retracted")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        fwci: val
            .get("fwci")
            .and_then(Value::as_f64)
            .filter(|f| f.is_finite()),
        issns: Vec::new(),
        is_in_doaj: None,
        answered: true,
        last_status: Some(200),
    };

    if let Some(src) = val.get("primary_location").and_then(|l| l.get("source")) {
        if let Some(issn_l) = src.get("issn_l").and_then(Value::as_str) {
            out.issns.push(issn_l.to_owned());
        }
        if let Some(arr) = src.get("issn").and_then(Value::as_array) {
            for v in arr {
                if let Some(s) = v.as_str() {
                    out.issns.push(s.to_owned());
                }
            }
        }
        out.is_in_doaj = src.get("is_in_doaj").and_then(Value::as_bool);
    }
    out
}

async fn fetch_scite_tallies(
    sources: &Arc<dyn SourcesApi>,
    doi: &str,
    cx: &CallCx,
) -> Option<SciteTallies> {
    let output = match sources
        .call("scite.tallies", json!({ "doi": doi }), cx)
        .await
    {
        Ok(o) => o,
        Err(e) => {
            tracing::debug!(error = %e, doi, "scite.tallies unavailable");
            return None;
        }
    };
    let item = output.items.first()?;
    let supporting = item.get("supporting").and_then(Value::as_u64)?;
    let contradicting = item.get("contradicting").and_then(Value::as_u64)?;
    let mentioning = item.get("mentioning").and_then(Value::as_u64)?;
    Some(SciteTallies::new(supporting, contradicting, mentioning))
}

async fn check_doaj_vetted(
    deps: &SourceDeps,
    sources: &Arc<dyn SourcesApi>,
    issns: &[String],
    oa_is_in_doaj: Option<bool>,
    cx: &CallCx,
) -> Result<Option<bool>, FsmError> {
    let Some(issn) = issns.first() else {
        return Ok(oa_is_in_doaj);
    };

    // First try cataloged `doaj.journals` via `SourcesApi` (`crates/manifests/sources/seed/doaj.toml`).
    if let Ok(output) = sources
        .call(
            "doaj.journals",
            json!({ "query": format!("issn:{issn}"), "limit": 1 }),
            cx,
        )
        .await
    {
        return Ok(Some(!output.items.is_empty()));
    }

    // Direct keyless DOAJ Journals API lookup via `deps.net`.
    let url = format!("{DOAJ_JOURNALS}/issn:{}?pageSize=1", encode_segment(issn));
    let (status, payload) = get_json_with_status(None, deps, &url, "doaj", cx).await?;
    if status.is_some_and(|s| (200..300).contains(&s))
        && let Some(val) = payload
    {
        if let Some(total) = val.get("total").and_then(Value::as_u64) {
            return Ok(Some(total > 0));
        }
        if let Some(results) = val.get("results").and_then(Value::as_array) {
            return Ok(Some(!results.is_empty()));
        }
    }

    Ok(oa_is_in_doaj)
}
