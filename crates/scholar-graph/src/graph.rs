//! Citation-graph traversal (`ScholarGraph::graph`).
//!
//! Preserves Python `paper_graph.py` semantics (identifier resolution, arXiv
//! exact-title matching, title similarity gating with near-miss reporting,
//! bounded 4-request OpenAlex + Crossref happy path, notice deduplication &
//! severity ranking, concurrent fan-out with clean cancellation) while adding
//! 1.0 anti-429 multi-source fallbacks (Crossref `reference[]`, OpenCitations v2,
//! Semantic Scholar, Europe PMC) and local Retraction Watch lookups.

use crate::ids::{
    PaperInput, TITLE_MATCH_MIN, classify_paper_input, crossref_date, default_process,
    is_retraction_nature, normalize_doi, notice_rank, short_openalex_id, title_score,
};
use fsm_core::{
    AuthorRef, CallCx, CitationGraph, FsmError, GraphDirection, GraphNotice, GraphOpts,
    HttpRequest, NetCx, OaLocation, RetractionNotice, ScholarlyWork, SourceDeps, TransportProfile,
};
use fsm_scholar::{Provider, ScholarCore};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

const OPENALEX_WORKS: &str = "https://api.openalex.org/works";
const CROSSREF_WORKS: &str = "https://api.crossref.org/works";
const ARXIV_API: &str = "https://export.arxiv.org/api/query";
const OPENCITATIONS_V2: &str = "https://api.opencitations.net/index/v2";
const S2_PAPER_API: &str = "https://api.semanticscholar.org/graph/v1/paper";
const EUROPEPMC_REST: &str = "https://www.ebi.ac.uk/europepmc/webservices/rest";

const OPENALEX_SELECT: &str =
    "id,doi,display_name,publication_year,cited_by_count,primary_location,is_retracted";
const OPENALEX_SELECT_TARGET: &str = "id,ids,type,doi,display_name,publication_year,cited_by_count,primary_location,is_retracted,referenced_works,authorships";

const MAX_BATCH: usize = 50;
const MAX_LIMIT: usize = 50;
const TITLE_CANDIDATES: usize = 5;
const MAX_NOTICES: usize = 6;
const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(8);

/// Characters safe in a DOI / S2 typed-ID path segment (`/` and `:` preserved).
const PATH_DOI_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'/')
    .remove(b':')
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

/// Full query/path segment encode set (RFC 3986 unreserved).
const SEGMENT_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

pub(crate) fn encode_doi_path(doi: &str) -> String {
    utf8_percent_encode(doi, PATH_DOI_SET).to_string()
}

pub(crate) fn encode_segment(s: &str) -> String {
    utf8_percent_encode(s, SEGMENT_SET).to_string()
}

/// Crossref work status + extracted post-publication notices + fallback references.
#[derive(Debug, Clone, Default)]
pub(crate) struct CrossrefCheck {
    /// `Some(true)` = 200 OK, `Some(false)` = HTTP 404 (likely DataCite DOI),
    /// `None` = network error / timeout / no DOI.
    pub registered: Option<bool>,
    pub last_status: Option<u16>,
    pub notices: Vec<RetractionNotice>,
    pub references: Vec<ScholarlyWork>,
    pub issns: Vec<String>,
    pub venue: Option<String>,
    pub work: Option<ScholarlyWork>,
}

/// How an `arXiv:` identifier was resolved to its target graph node.
enum ArxivResolution {
    PublishedDoi(String),
    Title(String),
    SemanticScholar,
    ArxivFallback,
}

/// Resolved target metadata from OpenAlex / arXiv / Crossref / S2.
struct ResolvedTarget {
    work: ScholarlyWork,
    referenced_openalex_ids: Vec<String>,
    arxiv_resolution: Option<ArxivResolution>,
    s2_references: Vec<ScholarlyWork>,
    s2_citations: Vec<ScholarlyWork>,
}

/// Core `ScholarGraph::graph` implementation.
pub async fn run_graph(
    core: &ScholarCore,
    deps: &SourceDeps,
    id_or_title: &str,
    opts: GraphOpts,
    cx: &CallCx,
) -> Result<CitationGraph, FsmError> {
    if cx.cancel.is_cancelled() {
        return Err(FsmError::Timeout(0));
    }

    let limit = (opts.limit as usize).clamp(1, MAX_LIMIT);
    let direction = opts.direction;
    let input = classify_paper_input(id_or_title);
    let resolved_as = input.kind_str().to_owned();

    let (resolved, near_misses, arxiv_title) = resolve_target(core, deps, &input, cx).await?;

    let Some(target_bundle) = resolved else {
        let mut notes = Vec::new();
        let reason = match &input {
            PaperInput::Arxiv(aid) => match arxiv_title {
                Some(ref t) if !t.is_empty() => format!(
                    " arXiv calls it '{t}', but OpenAlex has no confident match for that title. A very recent preprint may not be indexed yet."
                ),
                _ => format!(" arXiv returned no record for '{aid}'; check the identifier."),
            },
            PaperInput::Doi(_) => " The DOI may be unregistered or mistyped.".to_owned(),
            _ => " Try the exact title, or a DOI.".to_owned(),
        };
        notes.push(format!(
            "No work matched '{id_or_title}' in OpenAlex.{reason}"
        ));
        if !near_misses.is_empty() {
            let listed = near_misses
                .iter()
                .map(|t| format!("'{t}'"))
                .collect::<Vec<_>>()
                .join("; ");
            notes.push(format!(
                "Closest titles found, none close enough to act on: {listed}"
            ));
        }
        let mut graph = CitationGraph::new(id_or_title, resolved_as, ScholarlyWork::default());
        graph.notes = notes;
        return Ok(graph);
    };

    let mut paper = target_bundle.work;
    let mut notes = Vec::new();

    if let (PaperInput::Arxiv(aid), Some(res)) = (&input, &target_bundle.arxiv_resolution) {
        match res {
            ArxivResolution::PublishedDoi(pub_doi) => notes.push(format!(
                "Resolved arXiv:{aid} through its published DOI {pub_doi}."
            )),
            ArxivResolution::Title(title) => notes.push(format!(
                "Resolved arXiv:{aid} by its title ('{title}'): OpenAlex does not resolve arXiv's own DOIs. Check the paper above is the one you meant."
            )),
            ArxivResolution::SemanticScholar => notes.push(format!(
                "Resolved arXiv:{aid} via Semantic Scholar (OpenAlex rate-limited)."
            )),
            ArxivResolution::ArxivFallback => notes.push(format!(
                "Resolved arXiv:{aid} via arXiv metadata + fallback citation indexes (OpenAlex rate-limited)."
            )),
        }
    }

    let want_refs = matches!(direction, GraphDirection::Both | GraphDirection::References);
    let want_cites = matches!(direction, GraphDirection::Both | GraphDirection::Citations);
    let doi_opt = paper.ids.doi.clone();
    let oa_id_opt = paper.ids.openalex.clone();
    let arxiv_opt = paper.ids.arxiv.clone();
    let pmid_opt = paper.ids.pmid.clone();
    let pmcid_opt = paper.ids.pmcid.clone();
    let ref_oa_ids = target_bundle.referenced_openalex_ids;
    let s2_refs_seed = target_bundle.s2_references;
    let s2_cites_seed = target_bundle.s2_citations;

    // Concurrent fan-out: references, citations, Crossref verification, local RW lookup.
    // All futures share `cx.cancel`, so dropping/cancelling settles every in-flight call.
    let refs_fut = async {
        if !want_refs {
            return Ok((Vec::new(), 0usize, false));
        }
        if !ref_oa_ids.is_empty() {
            let (nodes, total, oa_ok) =
                fetch_openalex_references(core, deps, &ref_oa_ids, limit, cx).await?;
            if oa_ok {
                return Ok((nodes, total, true));
            }
        }
        // Fallback when OpenAlex refs are unavailable or 429'd.
        let (nodes, total) = fetch_fallback_references(
            core,
            deps,
            doi_opt.as_deref(),
            arxiv_opt.as_deref(),
            pmid_opt.as_deref(),
            pmcid_opt.as_deref(),
            &s2_refs_seed,
            limit,
            cx,
        )
        .await?;
        Ok((nodes, total.max(ref_oa_ids.len()), false))
    };

    let cites_fut = async {
        if !want_cites {
            return Ok((Vec::new(), false));
        }
        if let Some(ref wid) = oa_id_opt {
            let (nodes, oa_ok) = fetch_openalex_citations(core, deps, wid, limit, cx).await?;
            if oa_ok {
                return Ok((nodes, true));
            }
        }
        // Fallback when OpenAlex cites are unavailable or 429'd.
        let nodes = fetch_fallback_citations(
            core,
            deps,
            doi_opt.as_deref(),
            arxiv_opt.as_deref(),
            pmid_opt.as_deref(),
            pmcid_opt.as_deref(),
            &s2_cites_seed,
            limit,
            cx,
        )
        .await?;
        Ok((nodes, false))
    };

    let crossref_fut = async {
        match doi_opt.as_deref() {
            Some(doi) => fetch_crossref_status(deps, doi, cx).await,
            None => Ok(CrossrefCheck::default()),
        }
    };

    let local_rw_fut = async {
        match doi_opt.as_deref() {
            Some(doi) => deps.store.retraction_lookup(doi).await.ok().flatten(),
            None => None,
        }
    };

    let (refs_res, cites_res, crossref_res, local_rw) =
        tokio::join!(refs_fut, cites_fut, crossref_fut, local_rw_fut);

    let (mut references, mut total_references, from_oa_refs) = refs_res?;
    let (citations, from_oa_cites) = cites_res?;
    let crossref = crossref_res?;

    // I6 (SPEC §6): on the fallback path, always union Crossref `reference[]`
    // (already fetched with real titles & bibliography order) with OpenCitations /
    // S2 / Europe PMC so bare-DOI OpenCitations rows never eclipse Crossref titles.
    if want_refs && !from_oa_refs && !crossref.references.is_empty() {
        let mut combined = crossref.references;
        combined.extend(references);
        let (fused, fused_total) = dedupe_and_rank_neighbors_with_total(combined, limit);
        references = fused;
        total_references = total_references.max(fused_total);
    }

    if want_refs {
        if total_references > 0 && references.is_empty() {
            notes.push(format!(
                "{total_references} references are recorded but none could be restored; the target's own record may be incomplete."
            ));
        } else if total_references > references.len() {
            let ranked = from_oa_refs || references.iter().any(|w| w.cited_by.is_some());
            if ranked {
                notes.push(format!(
                    "Showing {} of {total_references} references, most-cited first.",
                    references.len()
                ));
            } else {
                notes.push(format!(
                    "Showing {} of {total_references} references.",
                    references.len()
                ));
            }
        }
    }

    if want_cites
        && let Some(cited_by) = paper.cited_by
        && (cited_by as usize) > citations.len()
    {
        let ranked = from_oa_cites || citations.iter().any(|w| w.cited_by.is_some());
        if ranked {
            notes.push(format!(
                "Showing {} of {cited_by} citing works, most-cited first.",
                citations.len()
            ));
        } else {
            notes.push(format!(
                "Showing {} of {cited_by} citing works.",
                citations.len()
            ));
        }
    }

    // Fuse Crossref notices + local Retraction Watch notice, deduplicated by notice DOI.
    let mut combined_notices = crossref.notices;
    if let Some(rw) = local_rw {
        if paper.retraction_reason.is_none() {
            paper.retraction_reason = rw.reason.clone();
        }
        combined_notices.push(rw);
    }
    let deduped = dedupe_retraction_notices(combined_notices);
    let has_retraction = deduped.iter().any(|n| is_retraction_nature(&n.nature));

    if paper.retracted || has_retraction {
        paper.retracted = true;
        if paper.retraction_reason.is_none() {
            paper.retraction_reason = deduped.iter().find_map(|n| n.reason.clone());
        }
        notes.insert(
            0,
            "RETRACTED. Do not cite this paper as a standing result.".to_owned(),
        );
    } else if crossref.registered == Some(false) {
        notes.insert(
            0,
            "Crossref has no record of this DOI. It is likely a DataCite DOI (dataset or repository), so retraction notices could not be checked."
                .to_owned(),
        );
    }

    let notices: Vec<GraphNotice> = deduped
        .into_iter()
        .take(MAX_NOTICES)
        .map(retraction_to_graph_notice)
        .collect();

    let mut graph = CitationGraph::new(id_or_title, resolved_as, paper);
    graph.references = references;
    graph.citations = citations;
    graph.notices = notices;
    graph.notes = notes;
    Ok(graph)
}

fn retraction_to_graph_notice(n: RetractionNotice) -> GraphNotice {
    let mut g = GraphNotice::new(n.nature);
    g.date = n.date;
    g.url = n
        .notice_doi
        .as_ref()
        .map(|d| format!("https://doi.org/{d}"));
    g.doi = n.notice_doi;
    g
}

// ---------------------------------------------------------------------------
// Target resolution (OpenAlex $0 + arXiv Atom + S2 + Crossref fallbacks)
// ---------------------------------------------------------------------------

async fn resolve_target(
    core: &ScholarCore,
    deps: &SourceDeps,
    input: &PaperInput,
    cx: &CallCx,
) -> Result<(Option<ResolvedTarget>, Vec<String>, Option<String>), FsmError> {
    match input {
        PaperInput::Doi(doi) => {
            let url = openalex_url(
                &format!("{OPENALEX_WORKS}/https://doi.org/{}", encode_doi_path(doi)),
                &[("select", OPENALEX_SELECT_TARGET)],
                deps,
            );
            let (status, payload) =
                get_json_with_status(Some(core), deps, &url, "openalex", cx).await?;
            if let Some(item) = payload.filter(|v| v.get("id").and_then(Value::as_str).is_some()) {
                return Ok((Some(parse_openalex_target(&item)), Vec::new(), None));
            }
            // Anti-429 fallback: if OpenAlex rate-limited (429) or errored (5xx), try S2 & Crossref.
            if status.is_some_and(|s| s == 429 || s >= 500) {
                if let Some(s2) = fetch_s2_paper(core, deps, &format!("DOI:{doi}"), cx).await? {
                    return Ok((Some(s2), Vec::new(), None));
                }
                let cr = fetch_crossref_status(deps, doi, cx).await?;
                if let Some(work) = cr.work {
                    return Ok((
                        Some(ResolvedTarget {
                            work,
                            referenced_openalex_ids: Vec::new(),
                            arxiv_resolution: None,
                            s2_references: Vec::new(),
                            s2_citations: Vec::new(),
                        }),
                        Vec::new(),
                        None,
                    ));
                }
            }
            Ok((None, Vec::new(), None))
        }
        PaperInput::OpenAlex(wid) => {
            let url = openalex_url(
                &format!("{OPENALEX_WORKS}/{}", encode_segment(wid)),
                &[("select", OPENALEX_SELECT_TARGET)],
                deps,
            );
            let (_, payload) = get_json_with_status(Some(core), deps, &url, "openalex", cx).await?;
            if let Some(item) = payload.filter(|v| v.get("id").and_then(Value::as_str).is_some()) {
                return Ok((Some(parse_openalex_target(&item)), Vec::new(), None));
            }
            Ok((None, Vec::new(), None))
        }
        PaperInput::Arxiv(arxiv_id) => resolve_arxiv(core, deps, arxiv_id, cx).await,
        PaperInput::Pmid(pmid) => {
            let url = openalex_url(
                &format!("{OPENALEX_WORKS}/pmid:{}", encode_segment(pmid)),
                &[("select", OPENALEX_SELECT_TARGET)],
                deps,
            );
            let (_, payload) = get_json_with_status(Some(core), deps, &url, "openalex", cx).await?;
            if let Some(item) = payload.filter(|v| v.get("id").and_then(Value::as_str).is_some()) {
                let mut target = parse_openalex_target(&item);
                if target.work.ids.pmid.is_none() {
                    target.work.ids.pmid = Some(pmid.clone());
                }
                return Ok((Some(target), Vec::new(), None));
            }
            if let Some(s2) = fetch_s2_paper(core, deps, &format!("PMID:{pmid}"), cx).await? {
                return Ok((Some(s2), Vec::new(), None));
            }
            Ok((None, Vec::new(), None))
        }
        PaperInput::Pmcid(pmcid) => {
            let url = openalex_url(
                &format!("{OPENALEX_WORKS}/pmcid:{}", encode_segment(pmcid)),
                &[("select", OPENALEX_SELECT_TARGET)],
                deps,
            );
            let (_, payload) = get_json_with_status(Some(core), deps, &url, "openalex", cx).await?;
            if let Some(item) = payload.filter(|v| v.get("id").and_then(Value::as_str).is_some()) {
                let mut target = parse_openalex_target(&item);
                if target.work.ids.pmcid.is_none() {
                    target.work.ids.pmcid = Some(pmcid.clone());
                }
                return Ok((Some(target), Vec::new(), None));
            }
            if let Some(s2) = fetch_s2_paper(core, deps, &format!("PMCID:{pmcid}"), cx).await? {
                return Ok((Some(s2), Vec::new(), None));
            }
            Ok((None, Vec::new(), None))
        }
        PaperInput::Title(title) => {
            if title.is_empty() {
                return Ok((None, Vec::new(), None));
            }
            let (candidates, status) =
                fetch_openalex_title_candidates(core, deps, title, cx).await?;
            if let Some(matched) = best_title_match(title, &candidates) {
                return Ok((Some(parse_openalex_target(matched)), Vec::new(), None));
            }
            if !candidates.is_empty() {
                let near = near_miss_titles(&candidates);
                return Ok((None, near, None));
            }
            // Anti-429 fallback when OpenAlex title search was rate-limited.
            if status == Some(429)
                && let Some(cr_work) = search_crossref_by_title(deps, title, cx).await?
            {
                return Ok((
                    Some(ResolvedTarget {
                        work: cr_work,
                        referenced_openalex_ids: Vec::new(),
                        arxiv_resolution: None,
                        s2_references: Vec::new(),
                        s2_citations: Vec::new(),
                    }),
                    Vec::new(),
                    None,
                ));
            }
            Ok((None, Vec::new(), None))
        }
    }
}

/// Maximum bytes accepted for a single-entry arXiv Atom feed response (R3 / I2).
const ARXIV_MAX_XML_BYTES: u64 = 512 * 1024;
/// Maximum Unicode scalar values extracted from an arXiv Atom `<title>` (R3 / I2).
const ARXIV_MAX_TITLE_CHARS: usize = 1024;

/// Parsed arXiv Atom `<entry>` metadata needed for OpenAlex graph resolution.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArxivEntry {
    pub title: String,
    pub doi: Option<String>,
}

/// Query arXiv Atom API (`id_list={arxiv_id}&max_results=1`) and parse with `roxmltree`.
pub(crate) async fn fetch_arxiv_entry(
    deps: &SourceDeps,
    arxiv_id: &str,
    cx: &CallCx,
) -> Result<Option<ArxivEntry>, FsmError> {
    let url = format!(
        "{ARXIV_API}?id_list={}&max_results=1",
        encode_segment(arxiv_id)
    );
    let req = HttpRequest::get(url)
        .header(
            "Accept",
            "application/atom+xml, application/xml;q=0.9, */*;q=0.5",
        )
        .for_source("arxiv")
        .with_max_bytes(ARXIV_MAX_XML_BYTES);
    let ncx = net_cx_for_source(cx, deps, Some("arxiv"));
    let resp = match deps.net.send(req, TransportProfile::Plain, &ncx).await {
        Ok(r) => r,
        Err(FsmError::Timeout(0)) if cx.cancel.is_cancelled() => return Err(FsmError::Timeout(0)),
        Err(e) => {
            tracing::debug!(error = %e, arxiv_id, "arXiv lookup failed");
            return Ok(None);
        }
    };
    if !resp.is_success() || resp.truncated {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&resp.body);
    Ok(parse_arxiv_atom_xml(&text))
}

/// Parse an arXiv Atom feed XML document with `roxmltree`, enforcing R3 / I2:
/// - Rejects XML containing `<!ENTITY` / `<!DOCTYPE` declarations.
/// - Requires `<entry><id>` to point to `arxiv.org/abs/` (rejecting arXiv
///   `/api/errors#...` entries whose `<title>` is `"Error"`).
/// - Caps extracted `<title>` length to [`ARXIV_MAX_TITLE_CHARS`].
pub fn parse_arxiv_atom_xml(xml: &str) -> Option<ArxivEntry> {
    // R3 defense-in-depth: reject any entity/DTD declarations before parsing.
    let xml_lower = xml.to_ascii_lowercase();
    if xml_lower.contains("<!entity") || xml_lower.contains("<!doctype") {
        return None;
    }

    let doc = roxmltree::Document::parse(xml).ok()?;
    let entry = doc
        .descendants()
        .find(|n| n.is_element() && n.tag_name().name().eq_ignore_ascii_case("entry"))?;

    let id_text = entry
        .children()
        .find(|n| n.is_element() && n.tag_name().name().eq_ignore_ascii_case("id"))
        .and_then(|n| n.text())
        .map(str::trim)?;
    let id_lower = id_text.to_ascii_lowercase();
    if !id_lower.contains("arxiv.org/abs/") || id_lower.contains("/api/errors") {
        return None;
    }

    let collapse = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");

    let raw_title = entry
        .children()
        .find(|n| n.is_element() && n.tag_name().name().eq_ignore_ascii_case("title"))
        .and_then(|n| n.text())
        .map(collapse)
        .unwrap_or_default();
    let title = fsm_core::text::clip_chars(&raw_title, ARXIV_MAX_TITLE_CHARS).to_owned();
    if title.is_empty() {
        return None;
    }

    let doi = entry
        .children()
        .find(|n| n.is_element() && n.tag_name().name().eq_ignore_ascii_case("doi"))
        .and_then(|n| n.text())
        .and_then(normalize_doi);

    Some(ArxivEntry { title, doi })
}

async fn resolve_arxiv(
    core: &ScholarCore,
    deps: &SourceDeps,
    arxiv_id: &str,
    cx: &CallCx,
) -> Result<(Option<ResolvedTarget>, Vec<String>, Option<String>), FsmError> {
    let Some(entry) = fetch_arxiv_entry(deps, arxiv_id, cx).await? else {
        return Ok((None, Vec::new(), None));
    };
    let title = entry.title.clone();

    // If arXiv supplied a published journal DOI, try OpenAlex $0 DOI lookup first.
    if let Some(ref pub_doi) = entry.doi {
        let url = openalex_url(
            &format!(
                "{OPENALEX_WORKS}/https://doi.org/{}",
                encode_doi_path(pub_doi)
            ),
            &[("select", OPENALEX_SELECT_TARGET)],
            deps,
        );
        let (_, payload) = get_json_with_status(Some(core), deps, &url, "openalex", cx).await?;
        if let Some(item) = payload.filter(|v| v.get("id").and_then(Value::as_str).is_some()) {
            let mut target = parse_openalex_target(&item);
            if target.work.ids.arxiv.is_none() {
                target.work.ids.arxiv = Some(arxiv_id.to_owned());
            }
            target.arxiv_resolution = Some(ArxivResolution::PublishedDoi(pub_doi.clone()));
            return Ok((Some(target), Vec::new(), Some(title)));
        }
    }

    // Query OpenAlex by arXiv's authoritative title, requiring an exact normalized match
    // (Python `paper_graph._resolve_arxiv` rule: pick highest cited_by_count among exact matches).
    let (candidates, status) = fetch_openalex_title_candidates(core, deps, &title, cx).await?;
    let norm_title = default_process(&title);
    let exact: Vec<&Value> = candidates
        .iter()
        .filter(|c| {
            c.get("display_name")
                .and_then(Value::as_str)
                .is_some_and(|dn| default_process(dn) == norm_title)
        })
        .collect();

    if let Some(best) = exact
        .into_iter()
        .max_by_key(|c| c.get("cited_by_count").and_then(Value::as_u64).unwrap_or(0))
    {
        let mut target = parse_openalex_target(best);
        if target.work.ids.arxiv.is_none() {
            target.work.ids.arxiv = Some(arxiv_id.to_owned());
        }
        target.arxiv_resolution = Some(ArxivResolution::Title(title.clone()));
        return Ok((Some(target), Vec::new(), Some(title)));
    }

    // Anti-429 fallback: if OpenAlex search was 429'd (not a clean miss with candidates),
    // try Semantic Scholar single-paper lookup `ARXIV:<id>` ($0 keyless!), then cached/live
    // ScholarCore metadata or the arXiv Atom entry itself so OpenCitations fallback can run.
    if candidates.is_empty() && status == Some(429) {
        if let Some(mut s2) = fetch_s2_paper(core, deps, &format!("ARXIV:{arxiv_id}"), cx).await? {
            if s2.work.ids.arxiv.is_none() {
                s2.work.ids.arxiv = Some(arxiv_id.to_owned());
            }
            s2.arxiv_resolution = Some(ArxivResolution::SemanticScholar);
            return Ok((Some(s2), Vec::new(), Some(title)));
        }
        let mut work = match core.work(&format!("arXiv:{arxiv_id}"), cx).await {
            Ok(Some(w)) => w,
            _ => {
                let mut w = ScholarlyWork::new(&title, format!("https://arxiv.org/abs/{arxiv_id}"));
                w.work_type = "preprint".to_owned();
                w.sources = vec!["arxiv".to_owned()];
                w
            }
        };
        if work.ids.arxiv.is_none() {
            work.ids.arxiv = Some(arxiv_id.to_owned());
        }
        if work.ids.doi.is_none() {
            work.ids.doi = entry.doi.clone();
        }
        return Ok((
            Some(ResolvedTarget {
                work,
                referenced_openalex_ids: Vec::new(),
                arxiv_resolution: Some(ArxivResolution::ArxivFallback),
                s2_references: Vec::new(),
                s2_citations: Vec::new(),
            }),
            Vec::new(),
            Some(title),
        ));
    }

    let near = near_miss_titles(&candidates);
    Ok((None, near, Some(title)))
}

async fn fetch_openalex_title_candidates(
    core: &ScholarCore,
    deps: &SourceDeps,
    title: &str,
    cx: &CallCx,
) -> Result<(Vec<Value>, Option<u16>), FsmError> {
    let per_page = TITLE_CANDIDATES.to_string();
    let url = openalex_url(
        OPENALEX_WORKS,
        &[
            ("search", title),
            ("per-page", &per_page),
            ("select", OPENALEX_SELECT_TARGET),
        ],
        deps,
    );
    let (status, payload) = get_json_with_status(Some(core), deps, &url, "openalex", cx).await?;
    let items = payload
        .and_then(|v| v.get("results")?.as_array().cloned())
        .unwrap_or_default();
    Ok((items, status))
}

/// First candidate in OpenAlex relevance order that clears [`TITLE_MATCH_MIN`].
pub fn best_title_match<'a>(query: &str, candidates: &'a [Value]) -> Option<&'a Value> {
    candidates.iter().find(|item| {
        item.get("display_name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .is_some_and(|t| title_score(query, t) >= TITLE_MATCH_MIN)
    })
}

fn near_miss_titles(candidates: &[Value]) -> Vec<String> {
    candidates
        .iter()
        .filter_map(|c| {
            c.get("display_name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        })
        .take(3)
        .collect()
}

// ---------------------------------------------------------------------------
// OpenAlex references & citations
// ---------------------------------------------------------------------------

async fn fetch_openalex_references(
    core: &ScholarCore,
    deps: &SourceDeps,
    ref_ids: &[String],
    limit: usize,
    cx: &CallCx,
) -> Result<(Vec<ScholarlyWork>, usize, bool), FsmError> {
    if ref_ids.is_empty() {
        return Ok((Vec::new(), 0, true));
    }
    let total = ref_ids.len();
    let wanted: Vec<&str> = ref_ids.iter().take(MAX_BATCH).map(String::as_str).collect();
    let filter_val = format!("openalex_id:{}", wanted.join("|"));
    let per_page = wanted.len().to_string();
    let url = openalex_url(
        OPENALEX_WORKS,
        &[
            ("filter", &filter_val),
            ("per-page", &per_page),
            ("select", OPENALEX_SELECT),
        ],
        deps,
    );
    let (status, payload) = get_json_with_status(Some(core), deps, &url, "openalex", cx).await?;
    if status == Some(429) {
        return Ok((Vec::new(), total, false));
    }
    let Some(items) = payload.and_then(|v| v.get("results")?.as_array().cloned()) else {
        return Ok((
            Vec::new(),
            total,
            status.is_some_and(|s| (200..300).contains(&s)),
        ));
    };
    let mut nodes: Vec<ScholarlyWork> = items.iter().map(parse_openalex_node).collect();
    nodes.sort_by_key(|a| std::cmp::Reverse(a.cited_by.unwrap_or(0)));
    nodes.truncate(limit);
    Ok((nodes, total, true))
}

async fn fetch_openalex_citations(
    core: &ScholarCore,
    deps: &SourceDeps,
    work_id: &str,
    limit: usize,
    cx: &CallCx,
) -> Result<(Vec<ScholarlyWork>, bool), FsmError> {
    if work_id.is_empty() {
        return Ok((Vec::new(), true));
    }
    let filter_val = format!("cites:{work_id}");
    let per_page = limit.to_string();
    let url = openalex_url(
        OPENALEX_WORKS,
        &[
            ("filter", &filter_val),
            ("sort", "cited_by_count:desc"),
            ("per-page", &per_page),
            ("select", OPENALEX_SELECT),
        ],
        deps,
    );
    let (status, payload) = get_json_with_status(Some(core), deps, &url, "openalex", cx).await?;
    if status == Some(429) {
        return Ok((Vec::new(), false));
    }
    let Some(items) = payload.and_then(|v| v.get("results")?.as_array().cloned()) else {
        return Ok((Vec::new(), status.is_some_and(|s| (200..300).contains(&s))));
    };
    let nodes: Vec<ScholarlyWork> = items.iter().map(parse_openalex_node).collect();
    Ok((nodes, true))
}

// ---------------------------------------------------------------------------
// Multi-source anti-429 fallbacks (OpenCitations v2, S2, Europe PMC)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn fetch_fallback_references(
    core: &ScholarCore,
    deps: &SourceDeps,
    doi: Option<&str>,
    arxiv: Option<&str>,
    pmid: Option<&str>,
    pmcid: Option<&str>,
    s2_seed: &[ScholarlyWork],
    limit: usize,
    cx: &CallCx,
) -> Result<(Vec<ScholarlyWork>, usize), FsmError> {
    let mut all: Vec<ScholarlyWork> = s2_seed.to_vec();

    let oc_doi = doi
        .map(str::to_owned)
        .or_else(|| arxiv.map(|a| format!("10.48550/arxiv.{}", a.to_ascii_lowercase())));
    let oc_fut = async {
        match oc_doi.as_deref() {
            Some(d) => fetch_opencitations_edges(deps, "references", d, limit, cx).await,
            None => Ok(Vec::new()),
        }
    };
    let s2_fut = async {
        if !s2_seed.is_empty() {
            return Ok(Vec::new());
        }
        let s2_id = doi
            .map(|d| format!("DOI:{d}"))
            .or_else(|| arxiv.map(|a| format!("ARXIV:{a}")));
        match s2_id {
            Some(id) => fetch_s2_edges(core, deps, &id, "references", limit, cx).await,
            None => Ok(Vec::new()),
        }
    };
    let epmc_fut = async {
        match (pmcid, pmid) {
            (Some(id), _) => fetch_europepmc_edges(deps, "PMC", id, "references", limit, cx).await,
            (None, Some(id)) => {
                fetch_europepmc_edges(deps, "MED", id, "references", limit, cx).await
            }
            (None, None) => Ok(Vec::new()),
        }
    };

    let (oc, s2, epmc) = tokio::join!(oc_fut, s2_fut, epmc_fut);
    all.extend(s2?);
    all.extend(epmc?);
    all.extend(oc?);

    Ok(dedupe_and_rank_neighbors_with_total(all, limit))
}

#[allow(clippy::too_many_arguments)]
async fn fetch_fallback_citations(
    core: &ScholarCore,
    deps: &SourceDeps,
    doi: Option<&str>,
    arxiv: Option<&str>,
    pmid: Option<&str>,
    pmcid: Option<&str>,
    s2_seed: &[ScholarlyWork],
    limit: usize,
    cx: &CallCx,
) -> Result<Vec<ScholarlyWork>, FsmError> {
    let mut all: Vec<ScholarlyWork> = s2_seed.to_vec();

    let oc_doi = doi
        .map(str::to_owned)
        .or_else(|| arxiv.map(|a| format!("10.48550/arxiv.{}", a.to_ascii_lowercase())));
    let oc_fut = async {
        match oc_doi.as_deref() {
            Some(d) => fetch_opencitations_edges(deps, "citations", d, limit, cx).await,
            None => Ok(Vec::new()),
        }
    };
    let s2_fut = async {
        if !s2_seed.is_empty() {
            return Ok(Vec::new());
        }
        let s2_id = doi
            .map(|d| format!("DOI:{d}"))
            .or_else(|| arxiv.map(|a| format!("ARXIV:{a}")));
        match s2_id {
            Some(id) => fetch_s2_edges(core, deps, &id, "citations", limit, cx).await,
            None => Ok(Vec::new()),
        }
    };
    let epmc_fut = async {
        match (pmcid, pmid) {
            (Some(id), _) => fetch_europepmc_edges(deps, "PMC", id, "citations", limit, cx).await,
            (None, Some(id)) => {
                fetch_europepmc_edges(deps, "MED", id, "citations", limit, cx).await
            }
            (None, None) => Ok(Vec::new()),
        }
    };

    let (oc, s2, epmc) = tokio::join!(oc_fut, s2_fut, epmc_fut);
    all.extend(s2?);
    all.extend(epmc?);
    all.extend(oc?);

    Ok(dedupe_and_rank_neighbors_with_total(all, limit).0)
}

async fn fetch_opencitations_edges(
    deps: &SourceDeps,
    edge: &str,
    doi: &str,
    limit: usize,
    cx: &CallCx,
) -> Result<Vec<ScholarlyWork>, FsmError> {
    let url = format!("{OPENCITATIONS_V2}/{edge}/doi:{}", encode_doi_path(doi));
    let (_, payload) = get_json_with_status(None, deps, &url, "opencitations", cx).await?;
    let Some(rows) = payload.and_then(|v| v.as_array().cloned()) else {
        return Ok(Vec::new());
    };
    let target_field = if edge == "references" {
        "cited"
    } else {
        "citing"
    };
    let mut out = Vec::new();
    for row in rows.iter().take(limit * 2) {
        let Some(raw_ids) = row.get(target_field).and_then(Value::as_str) else {
            continue;
        };
        let peer_doi = raw_ids
            .split_whitespace()
            .find_map(|tok| tok.strip_prefix("doi:").and_then(normalize_doi));
        let Some(peer_doi) = peer_doi else {
            continue;
        };
        let mut w = ScholarlyWork::new(peer_doi.clone(), format!("https://doi.org/{peer_doi}"));
        w.work_type = "article".to_owned();
        w.ids.doi = Some(peer_doi);
        if let Some(created) = row.get("creation").and_then(Value::as_str) {
            w.published_date = fsm_core::dates::normalize_date(created);
            w.year = w
                .published_date
                .as_deref()
                .and_then(|d| d.get(..4))
                .and_then(|y| y.parse().ok());
        }
        w.sources = vec!["opencitations".to_owned()];
        out.push(w);
    }
    Ok(out)
}

async fn fetch_s2_paper(
    core: &ScholarCore,
    deps: &SourceDeps,
    paper_id: &str,
    cx: &CallCx,
) -> Result<Option<ResolvedTarget>, FsmError> {
    let fields = "paperId,externalIds,title,authors,year,venue,citationCount,influentialCitationCount,url,openAccessPdf,references.paperId,references.externalIds,references.title,references.year,references.citationCount,references.url,citations.paperId,citations.externalIds,citations.title,citations.year,citations.citationCount,citations.url";
    let url = format!(
        "{S2_PAPER_API}/{}?fields={fields}",
        encode_doi_path(paper_id)
    );
    let mut req = HttpRequest::get(url)
        .header("Accept", "application/json")
        .for_source("semanticscholar");
    if let Some(key) = deps.cfg.api_key("SEARCH_MCP_SEMANTICSCHOLAR_API_KEY") {
        req = req.header("x-api-key", key).redact_header("x-api-key");
    }
    let (_, payload) = send_json_with_status(Some(core), deps, req, cx).await?;
    let Some(obj) = payload.filter(|v| v.get("title").and_then(Value::as_str).is_some()) else {
        return Ok(None);
    };
    let work = parse_s2_node(&obj);
    let s2_references = obj
        .get("references")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter(|v| v.get("title").and_then(Value::as_str).is_some())
                .map(parse_s2_node)
                .collect()
        })
        .unwrap_or_default();
    let s2_citations = obj
        .get("citations")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter(|v| v.get("title").and_then(Value::as_str).is_some())
                .map(parse_s2_node)
                .collect()
        })
        .unwrap_or_default();
    Ok(Some(ResolvedTarget {
        work,
        referenced_openalex_ids: Vec::new(),
        arxiv_resolution: None,
        s2_references,
        s2_citations,
    }))
}

async fn fetch_s2_edges(
    core: &ScholarCore,
    deps: &SourceDeps,
    paper_id: &str,
    edge: &str,
    limit: usize,
    cx: &CallCx,
) -> Result<Vec<ScholarlyWork>, FsmError> {
    let fields = "paperId,externalIds,title,authors,year,venue,citationCount,url";
    let url = format!(
        "{S2_PAPER_API}/{}/{edge}?fields={fields}&limit={limit}",
        encode_doi_path(paper_id)
    );
    let mut req = HttpRequest::get(url)
        .header("Accept", "application/json")
        .for_source("semanticscholar");
    if let Some(key) = deps.cfg.api_key("SEARCH_MCP_SEMANTICSCHOLAR_API_KEY") {
        req = req.header("x-api-key", key).redact_header("x-api-key");
    }
    let (_, payload) = send_json_with_status(Some(core), deps, req, cx).await?;
    let wrapper = if edge == "references" {
        "citedPaper"
    } else {
        "citingPaper"
    };
    let Some(rows) = payload.and_then(|v| v.get("data")?.as_array().cloned()) else {
        return Ok(Vec::new());
    };
    Ok(rows
        .iter()
        .filter_map(|row| row.get(wrapper))
        .filter(|v| v.get("title").and_then(Value::as_str).is_some())
        .map(parse_s2_node)
        .collect())
}

async fn fetch_europepmc_edges(
    deps: &SourceDeps,
    src: &str,
    ext_id: &str,
    edge: &str,
    limit: usize,
    cx: &CallCx,
) -> Result<Vec<ScholarlyWork>, FsmError> {
    let url = format!(
        "{EUROPEPMC_REST}/{src}/{}/{edge}?format=json&pageSize={limit}",
        encode_segment(ext_id)
    );
    let (_, payload) = get_json_with_status(None, deps, &url, "europepmc", cx).await?;
    let list_key = if edge == "references" {
        ("referenceList", "reference")
    } else {
        ("citationList", "citation")
    };
    let Some(rows) = payload.and_then(|v| v.get(list_key.0)?.get(list_key.1)?.as_array().cloned())
    else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for row in &rows {
        let title = row
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        if title.is_empty() {
            continue;
        }
        let doi = row
            .get("doi")
            .and_then(Value::as_str)
            .and_then(normalize_doi);
        let pmid = row.get("id").and_then(Value::as_str).map(str::to_owned);
        let url = doi
            .as_ref()
            .map(|d| format!("https://doi.org/{d}"))
            .or_else(|| {
                pmid.as_ref()
                    .map(|p| format!("https://europepmc.org/article/MED/{p}"))
            })
            .unwrap_or_default();
        let mut w = ScholarlyWork::new(title, url);
        w.work_type = "article".to_owned();
        w.ids.doi = doi;
        w.ids.pmid = pmid;
        w.year = row
            .get("pubYear")
            .and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .and_then(|y| u16::try_from(y).ok());
        w.venue = row
            .get("journalAbbreviation")
            .or_else(|| row.get("journalTitle"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        w.cited_by = row.get("citedByCount").and_then(Value::as_u64);
        if w.cited_by.is_some() {
            w.cited_by_source = Some("europepmc".to_owned());
        }
        w.sources = vec!["europepmc".to_owned()];
        out.push(w);
    }
    Ok(out)
}

fn neighbor_dedup_keys(item: &ScholarlyWork) -> Vec<String> {
    let mut keys = Vec::new();
    if let Some(ref d) = item.ids.doi {
        keys.push(format!("doi:{d}"));
    }
    if let Some(ref o) = item.ids.openalex {
        keys.push(format!("oa:{o}"));
    }
    if let Some(ref s) = item.ids.s2 {
        keys.push(format!("s2:{s}"));
    }
    if let Some(ref p) = item.ids.pmid {
        keys.push(format!("pmid:{p}"));
    }
    if let Some(ref p) = item.ids.pmcid {
        keys.push(format!("pmcid:{p}"));
    }
    if let Some(ref a) = item.ids.arxiv {
        keys.push(format!("arxiv:{a}"));
    }
    if keys.is_empty() {
        keys.push(format!("title:{}", default_process(&item.title)));
    }
    keys
}

/// Deduplicate neighbor works across any shared identifier (`doi`, `openalex`,
/// `s2`, `pmid`, `pmcid`, `arxiv`, falling back to normalized title) while
/// preserving insertion order; sort by `cited_by` descending ONLY when at least
/// one candidate carries a citation count (I6), otherwise preserve source order.
fn dedupe_and_rank_neighbors_with_total(
    items: Vec<ScholarlyWork>,
    limit: usize,
) -> (Vec<ScholarlyWork>, usize) {
    let mut index_by_key: HashMap<String, usize> = HashMap::new();
    let mut out: Vec<ScholarlyWork> = Vec::new();
    for item in items {
        let keys = neighbor_dedup_keys(&item);
        match keys.iter().find_map(|k| index_by_key.get(k).copied()) {
            None => {
                let idx = out.len();
                for k in keys {
                    index_by_key.insert(k, idx);
                }
                out.push(item);
            }
            Some(idx) => {
                for k in keys {
                    index_by_key.entry(k).or_insert(idx);
                }
                let existing = &mut out[idx];
                // Prefer richer record (real title over bare DOI, higher cited_by).
                let existing_is_bare_doi =
                    existing.ids.doi.as_deref() == Some(existing.title.as_str());
                let new_is_bare_doi = item.ids.doi.as_deref() == Some(item.title.as_str());
                if existing_is_bare_doi && !new_is_bare_doi {
                    existing.title = item.title;
                }
                if item.cited_by.unwrap_or(0) > existing.cited_by.unwrap_or(0)
                    || (existing.cited_by.is_none() && item.cited_by.is_some())
                {
                    existing.cited_by = item.cited_by;
                    existing.cited_by_source = item.cited_by_source;
                }
                if existing.ids.doi.is_none() {
                    existing.ids.doi = item.ids.doi;
                }
                if existing.ids.openalex.is_none() {
                    existing.ids.openalex = item.ids.openalex;
                }
                if existing.ids.s2.is_none() {
                    existing.ids.s2 = item.ids.s2;
                }
                if existing.ids.pmid.is_none() {
                    existing.ids.pmid = item.ids.pmid;
                }
                if existing.ids.pmcid.is_none() {
                    existing.ids.pmcid = item.ids.pmcid;
                }
                if existing.ids.arxiv.is_none() {
                    existing.ids.arxiv = item.ids.arxiv;
                }
                if existing.authors.is_empty() && !item.authors.is_empty() {
                    existing.authors = item.authors;
                }
                if existing.year.is_none() {
                    existing.year = item.year;
                }
                if existing.venue.is_none() {
                    existing.venue = item.venue;
                }
                existing.retracted |= item.retracted;
                for s in item.sources {
                    if !existing.sources.contains(&s) {
                        existing.sources.push(s);
                    }
                }
            }
        }
    }
    let total_unique = out.len();
    if out.iter().any(|w| w.cited_by.is_some()) {
        out.sort_by(|a, b| {
            b.cited_by
                .unwrap_or(0)
                .cmp(&a.cited_by.unwrap_or(0))
                .then_with(|| a.title.cmp(&b.title))
        });
    }
    out.truncate(limit);
    (out, total_unique)
}

// ---------------------------------------------------------------------------
// Crossref verification & post-publication notices
// ---------------------------------------------------------------------------

pub(crate) async fn fetch_crossref_status(
    deps: &SourceDeps,
    doi: &str,
    cx: &CallCx,
) -> Result<CrossrefCheck, FsmError> {
    let mut out = CrossrefCheck::default();
    if doi.is_empty() {
        return Ok(out);
    }
    let mut url = format!("{CROSSREF_WORKS}/{}", encode_doi_path(doi));
    if let Some(ref email) = deps.cfg.contact_email
        && !email.is_empty()
    {
        url.push_str("?mailto=");
        url.push_str(&encode_segment(email));
    }
    let (status, payload) = get_json_with_status(None, deps, &url, "crossref", cx).await?;
    out.last_status = status;
    let Some(msg) = payload.and_then(|v| v.get("message").cloned()) else {
        if status == Some(404) {
            out.registered = Some(false);
        }
        return Ok(out);
    };
    if !msg.is_object() {
        return Ok(out);
    }
    out.registered = Some(true);
    out.notices = extract_crossref_notices(&msg);
    out.references = extract_crossref_references(&msg);
    out.issns = msg
        .get("ISSN")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    out.venue = msg
        .get("container-title")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .map(str::to_owned);
    out.work = parse_crossref_work(&msg);
    Ok(out)
}

/// Extract post-publication notices about this work from Crossref
/// `message.updated-by` and `message.relation.is-retracted-by`.
///
/// B1: `message.update-to` is intentionally ignored — in Crossref it points
/// *from* a retraction/correction notice *to* the original paper, so reading it
/// would falsely flag notices themselves as retracted.
/// M2/M3: Crossref entries always normalize `source` to `"crossref"` and leave
/// `reason = None` (Crossref `label` is only the type display name; only
/// Retraction Watch supplies retraction reasons).
pub(crate) fn extract_crossref_notices(msg: &Value) -> Vec<RetractionNotice> {
    let mut raw = Vec::new();
    let parse_entry = |entry: &Value| -> Option<RetractionNotice> {
        let kind = entry.get("type").and_then(Value::as_str)?.trim();
        if kind.is_empty() {
            return None;
        }
        let notice_doi = entry
            .get("DOI")
            .and_then(Value::as_str)
            .and_then(normalize_doi);
        let mut n = RetractionNotice::new(kind, "crossref");
        n.notice_doi = notice_doi;
        n.date = crossref_date(entry.get("updated"));
        n.reason = None;
        Some(n)
    };

    if let Some(updated_by) = msg.get("updated-by").and_then(Value::as_array) {
        for entry in updated_by {
            if let Some(n) = parse_entry(entry) {
                raw.push(n);
            }
        }
    }

    if let Some(rels) = msg
        .get("relation")
        .and_then(|r| r.get("is-retracted-by"))
        .and_then(Value::as_array)
    {
        for rel in rels {
            let id_type = rel.get("id-type").and_then(Value::as_str).unwrap_or("");
            if id_type.eq_ignore_ascii_case("doi")
                && let Some(doi) = rel
                    .get("id")
                    .and_then(Value::as_str)
                    .and_then(normalize_doi)
            {
                let mut n = RetractionNotice::new("retraction", "crossref");
                n.notice_doi = Some(doi);
                raw.push(n);
            }
        }
    }

    dedupe_retraction_notices(raw)
}

/// Deduplicate notices by `notice_doi` keeping the most severe reading (preserving
/// richer `reason`/`date` across sources), then sort by `(notice_rank, date)`.
pub(crate) fn dedupe_retraction_notices(notices: Vec<RetractionNotice>) -> Vec<RetractionNotice> {
    let mut by_doi: HashMap<String, RetractionNotice> = HashMap::new();
    let mut no_doi: Vec<RetractionNotice> = Vec::new();
    let mut seen_no_doi: HashSet<(String, String)> = HashSet::new();

    for mut notice in notices {
        if let Some(ref doi) = notice.notice_doi {
            let key = doi.to_ascii_lowercase();
            match by_doi.get_mut(&key) {
                None => {
                    by_doi.insert(key, notice);
                }
                Some(cur) => {
                    let new_rank = notice_rank(&notice.nature);
                    let cur_rank = notice_rank(&cur.nature);
                    if new_rank < cur_rank {
                        if notice.date.is_none() {
                            notice.date = cur.date.clone();
                        }
                        if notice.reason.is_none() {
                            notice.reason = cur.reason.clone();
                        }
                        *cur = notice;
                    } else {
                        if cur.date.is_none() {
                            cur.date = notice.date;
                        }
                        // Prefer detailed Retraction Watch reason over generic Crossref label.
                        if (notice.source == "retraction_watch" && notice.reason.is_some())
                            || cur.reason.is_none()
                        {
                            cur.reason = notice.reason;
                        }
                    }
                }
            }
        } else {
            let k = (
                notice.nature.to_ascii_lowercase(),
                notice.source.to_ascii_lowercase(),
            );
            if seen_no_doi.insert(k) {
                no_doi.push(notice);
            }
        }
    }

    let mut all: Vec<RetractionNotice> = by_doi.into_values().chain(no_doi).collect();
    all.sort_by(|a, b| {
        notice_rank(&a.nature)
            .cmp(&notice_rank(&b.nature))
            .then_with(|| {
                a.date
                    .as_deref()
                    .unwrap_or("")
                    .cmp(b.date.as_deref().unwrap_or(""))
            })
            .then_with(|| a.notice_doi.cmp(&b.notice_doi))
    });
    all
}

fn extract_crossref_references(msg: &Value) -> Vec<ScholarlyWork> {
    let Some(refs) = msg.get("reference").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for r in refs.iter().take(MAX_BATCH) {
        let doi = r.get("DOI").and_then(Value::as_str).and_then(normalize_doi);
        let title = r
            .get("article-title")
            .or_else(|| r.get("volume-title"))
            .or_else(|| r.get("unstructured"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| fsm_core::text::clip_chars(s, 300).to_owned())
            .or_else(|| doi.clone());
        let Some(title) = title else {
            continue;
        };
        let url = doi
            .as_ref()
            .map(|d| format!("https://doi.org/{d}"))
            .unwrap_or_default();
        let mut w = ScholarlyWork::new(title, url);
        w.work_type = "article".to_owned();
        w.ids.doi = doi;
        if let Some(author) = r.get("author").and_then(Value::as_str)
            && !author.trim().is_empty()
        {
            w.authors.push(AuthorRef::new(author.trim()));
        }
        w.year = r
            .get("year")
            .and_then(Value::as_str)
            .and_then(|s| s.get(..4))
            .and_then(|y| y.parse().ok());
        w.venue = r
            .get("journal-title")
            .and_then(Value::as_str)
            .map(str::to_owned);
        w.sources = vec!["crossref".to_owned()];
        out.push(w);
    }
    out
}

async fn search_crossref_by_title(
    deps: &SourceDeps,
    title: &str,
    cx: &CallCx,
) -> Result<Option<ScholarlyWork>, FsmError> {
    let mut url = format!(
        "{CROSSREF_WORKS}?query.title={}&rows={TITLE_CANDIDATES}",
        encode_segment(title)
    );
    if let Some(ref email) = deps.cfg.contact_email
        && !email.is_empty()
    {
        url.push_str("&mailto=");
        url.push_str(&encode_segment(email));
    }
    let (_, payload) = get_json_with_status(None, deps, &url, "crossref", cx).await?;
    let Some(items) = payload.and_then(|v| v.get("message")?.get("items")?.as_array().cloned())
    else {
        return Ok(None);
    };
    for item in &items {
        if let Some(w) = parse_crossref_work(item)
            && title_score(title, &w.title) >= TITLE_MATCH_MIN
        {
            return Ok(Some(w));
        }
    }
    Ok(None)
}

pub(crate) fn parse_crossref_work(msg: &Value) -> Option<ScholarlyWork> {
    let title = msg
        .get("title")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_owned();
    let doi = msg
        .get("DOI")
        .and_then(Value::as_str)
        .and_then(normalize_doi);
    let url = msg
        .get("URL")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| doi.as_ref().map(|d| format!("https://doi.org/{d}")))
        .unwrap_or_default();
    let mut w = ScholarlyWork::new(title, url);
    w.ids.doi = doi;
    w.work_type = msg
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("article")
        .to_owned();
    w.venue = msg
        .get("container-title")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .map(str::to_owned);
    w.published_date = crossref_date(
        msg.get("published-print")
            .or_else(|| msg.get("published-online"))
            .or_else(|| msg.get("issued"))
            .or_else(|| msg.get("created")),
    );
    w.year = w
        .published_date
        .as_deref()
        .and_then(|d| d.get(..4))
        .and_then(|y| y.parse().ok());
    w.cited_by = msg.get("is-referenced-by-count").and_then(Value::as_u64);
    if w.cited_by.is_some() {
        w.cited_by_source = Some("crossref".to_owned());
    }
    if let Some(authors) = msg.get("author").and_then(Value::as_array) {
        for a in authors {
            let given = a.get("given").and_then(Value::as_str).unwrap_or("").trim();
            let family = a.get("family").and_then(Value::as_str).unwrap_or("").trim();
            let name = match (given.is_empty(), family.is_empty()) {
                (false, false) => format!("{given} {family}"),
                (true, false) => family.to_owned(),
                (false, true) => given.to_owned(),
                (true, true) => a
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_owned(),
            };
            if !name.is_empty() {
                let mut aref = AuthorRef::new(name);
                aref.orcid = a
                    .get("ORCID")
                    .and_then(Value::as_str)
                    .and_then(crate::ids::extract_orcid);
                w.authors.push(aref);
            }
        }
    }
    w.sources = vec!["crossref".to_owned()];
    Some(w)
}

// ---------------------------------------------------------------------------
// OpenAlex & S2 JSON mappers
// ---------------------------------------------------------------------------

fn openalex_work_url(item: &Value) -> String {
    if let Some(u) = item
        .get("primary_location")
        .and_then(|l| l.get("landing_page_url"))
        .and_then(Value::as_str)
        .filter(|s| s.starts_with("http"))
    {
        return u.to_owned();
    }
    if let Some(d) = item
        .get("doi")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        return if d.starts_with("http") {
            d.to_owned()
        } else {
            format!("https://doi.org/{d}")
        };
    }
    item.get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

pub(crate) fn parse_openalex_node(item: &Value) -> ScholarlyWork {
    let raw_title = item
        .get("display_name")
        .or_else(|| item.get("title"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let title = fsm_core::text::clip_chars(raw_title, 300).to_owned();
    let url = openalex_work_url(item);
    let mut w = ScholarlyWork::new(title, url);
    w.work_type = item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("article")
        .to_owned();
    w.ids.doi = item
        .get("doi")
        .and_then(Value::as_str)
        .and_then(normalize_doi);
    w.ids.openalex = item
        .get("id")
        .and_then(Value::as_str)
        .and_then(short_openalex_id);
    if let Some(ids) = item.get("ids") {
        if w.ids.pmid.is_none() {
            w.ids.pmid = ids.get("pmid").and_then(Value::as_str).map(|s| {
                s.trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or(s)
                    .to_owned()
            });
        }
        if w.ids.pmcid.is_none() {
            w.ids.pmcid = ids.get("pmcid").and_then(Value::as_str).map(|s| {
                s.trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or(s)
                    .to_ascii_uppercase()
            });
        }
    }
    w.year = item
        .get("publication_year")
        .and_then(Value::as_u64)
        .and_then(|y| u16::try_from(y).ok());
    w.published_date = item
        .get("publication_date")
        .and_then(Value::as_str)
        .and_then(fsm_core::dates::normalize_date);
    w.venue = item
        .get("primary_location")
        .and_then(|l| l.get("source"))
        .and_then(|s| s.get("display_name"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    w.cited_by = Some(
        item.get("cited_by_count")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    );
    w.cited_by_source = Some("openalex".to_owned());
    w.retracted = item
        .get("is_retracted")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if let Some(authorships) = item.get("authorships").and_then(Value::as_array) {
        for a in authorships {
            let Some(name) = a
                .get("author")
                .and_then(|au| au.get("display_name"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let mut aref = AuthorRef::new(name);
            aref.orcid = a
                .get("author")
                .and_then(|au| au.get("orcid"))
                .and_then(Value::as_str)
                .and_then(crate::ids::extract_orcid);
            aref.affiliation = a
                .get("institutions")
                .and_then(Value::as_array)
                .and_then(|arr| arr.first())
                .and_then(|inst| inst.get("display_name"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            w.authors.push(aref);
        }
    }
    w.sources = vec!["openalex".to_owned()];
    w
}

fn parse_openalex_target(item: &Value) -> ResolvedTarget {
    let work = parse_openalex_node(item);
    let referenced_openalex_ids: Vec<String> = item
        .get("referenced_works")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .filter_map(short_openalex_id)
                .collect()
        })
        .unwrap_or_default();
    ResolvedTarget {
        work,
        referenced_openalex_ids,
        arxiv_resolution: None,
        s2_references: Vec::new(),
        s2_citations: Vec::new(),
    }
}

fn parse_s2_node(item: &Value) -> ScholarlyWork {
    let title = item
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    let doi = item
        .get("externalIds")
        .and_then(|e| e.get("DOI"))
        .and_then(Value::as_str)
        .and_then(normalize_doi);
    let arxiv = item
        .get("externalIds")
        .and_then(|e| e.get("ArXiv"))
        .and_then(Value::as_str)
        .map(|s| s.trim().to_ascii_lowercase());
    let s2_id = item
        .get("paperId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let url = item
        .get("url")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| doi.as_ref().map(|d| format!("https://doi.org/{d}")))
        .or_else(|| arxiv.as_ref().map(|a| format!("https://arxiv.org/abs/{a}")))
        .unwrap_or_default();
    let mut w = ScholarlyWork::new(title, url);
    w.work_type = "article".to_owned();
    w.ids.doi = doi;
    w.ids.arxiv = arxiv;
    w.ids.s2 = s2_id;
    w.ids.pmid = item
        .get("externalIds")
        .and_then(|e| e.get("PubMed"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    w.ids.pmcid = item
        .get("externalIds")
        .and_then(|e| e.get("PubMedCentral"))
        .and_then(Value::as_str)
        .map(|s| {
            if s.starts_with("PMC") {
                s.to_owned()
            } else {
                format!("PMC{s}")
            }
        });
    w.year = item
        .get("year")
        .and_then(Value::as_u64)
        .and_then(|y| u16::try_from(y).ok());
    w.venue = item
        .get("venue")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    w.cited_by = item.get("citationCount").and_then(Value::as_u64);
    if w.cited_by.is_some() {
        w.cited_by_source = Some("semanticscholar".to_owned());
    }
    w.influential_citations = item.get("influentialCitationCount").and_then(Value::as_u64);
    if let Some(pdf_url) = item
        .get("openAccessPdf")
        .and_then(|o| o.get("url"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        let mut oa = OaLocation::new("green");
        oa.pdf_url = Some(pdf_url.to_owned());
        w.oa = Some(oa);
    }
    if let Some(authors) = item.get("authors").and_then(Value::as_array) {
        for a in authors {
            if let Some(name) = a
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                w.authors.push(AuthorRef::new(name));
            }
        }
    }
    w.sources = vec!["semanticscholar".to_owned()];
    w
}

// ---------------------------------------------------------------------------
// Shared HTTP JSON helpers
// ---------------------------------------------------------------------------

pub(crate) fn openalex_url(base: &str, params: &[(&str, &str)], deps: &SourceDeps) -> String {
    let mut url = url::Url::parse(base).expect("valid openalex base url");
    {
        let mut qp = url.query_pairs_mut();
        for (k, v) in params {
            qp.append_pair(k, v);
        }
        if let Some(key) = deps.cfg.api_key("SEARCH_MCP_OPENALEX_API_KEY") {
            qp.append_pair("api_key", key);
        } else if let Some(ref email) = deps.cfg.contact_email
            && !email.is_empty()
        {
            qp.append_pair("mailto", email);
        }
    }
    url.to_string()
}

/// Shared GCRA host buckets and default RPS matching `fsm-scholar`'s `Provider::default_limits`.
fn shared_host_limits(source_id: &str) -> Option<(&'static str, f64)> {
    match source_id {
        "openalex" => Some(("api.openalex.org", 5.0)),
        "semanticscholar" | "s2" => Some(("api.semanticscholar.org", 1.0)),
        "crossref" => Some(("api.crossref.org", 5.0)),
        "arxiv" => Some(("export.arxiv.org", 0.33)),
        "pmc_idconv" => Some(("pmc.ncbi.nlm.nih.gov", 3.0)),
        "opencitations" => Some(("api.opencitations.net", 2.0)),
        "europepmc" => Some(("www.ebi.ac.uk", 3.0)),
        "doaj" => Some(("doaj.org", 2.0)),
        _ => None,
    }
}

pub(crate) fn net_cx_from_call(cx: &CallCx, deps: &SourceDeps) -> NetCx {
    net_cx_for_source(cx, deps, None)
}

pub(crate) fn net_cx_for_source(cx: &CallCx, deps: &SourceDeps, source_id: Option<&str>) -> NetCx {
    // M3: Honor user-configured `request_timeout_s` even when below `DEFAULT_HTTP_TIMEOUT`.
    let cfg_timeout = if deps.cfg.request_timeout_s > 0.0 && deps.cfg.request_timeout_s.is_finite()
    {
        Duration::from_secs_f64(deps.cfg.request_timeout_s)
    } else {
        DEFAULT_HTTP_TIMEOUT
    };
    let timeout = cx
        .remaining()
        .min(cfg_timeout)
        .max(Duration::from_millis(100));
    let mut ncx = NetCx::new(cx.cancel.child_token(), timeout);
    if let Some(src) = source_id
        && let Some((bucket, rps)) = shared_host_limits(src)
    {
        ncx.host_bucket = Some(bucket.to_owned());
        ncx.rps = Some(rps);
    }
    ncx
}

fn bench_provider(source_id: Option<&str>) -> Option<Provider> {
    match source_id? {
        "openalex" => Some(Provider::OpenAlex),
        "semanticscholar" => Some(Provider::SemanticScholar),
        _ => None,
    }
}

pub(crate) async fn get_json_with_status(
    core: Option<&ScholarCore>,
    deps: &SourceDeps,
    url: &str,
    source_id: &str,
    cx: &CallCx,
) -> Result<(Option<u16>, Option<Value>), FsmError> {
    let mut req = HttpRequest::get(url)
        .header("Accept", "application/json")
        .for_source(source_id);
    if source_id == "openalex" && deps.cfg.api_key("SEARCH_MCP_OPENALEX_API_KEY").is_some() {
        req = req.redact_query_key("api_key");
    }
    send_json_with_status(core, deps, req, cx).await
}

async fn send_json_with_status(
    core: Option<&ScholarCore>,
    deps: &SourceDeps,
    req: HttpRequest,
    cx: &CallCx,
) -> Result<(Option<u16>, Option<Value>), FsmError> {
    if cx.cancel.is_cancelled() {
        return Err(FsmError::Timeout(0));
    }
    let provider = bench_provider(req.source_id.as_deref());
    if let (Some(c), Some(p)) = (core, provider)
        && c.benched_until(p).await.is_some()
    {
        return Ok((Some(429), None));
    }
    let ncx = net_cx_for_source(cx, deps, req.source_id.as_deref());
    let resp = match deps.net.send(req, TransportProfile::Plain, &ncx).await {
        Ok(r) => r,
        Err(FsmError::Timeout(0)) if cx.cancel.is_cancelled() => return Err(FsmError::Timeout(0)),
        Err(FsmError::Upstream { status, .. }) => {
            if status == Some(429)
                && let (Some(c), Some(p)) = (core, provider)
            {
                let _ = c.note_rate_limited(p).await;
            }
            return Ok((status, None));
        }
        // B1: `FsmError::RateLimited` from `fsm-net` is a LOCAL GCRA bucket / Retry-After
        // wait that exceeded `ncx.timeout`, NOT an upstream HTTP 429 — never poison the UTC-midnight
        // daily bench across scholar crates on a local wait, but still report status 429 so
        // callers activate anti-429 fallback indexes (S2, Crossref, OpenCitations).
        Err(FsmError::RateLimited { .. }) => {
            tracing::debug!(
                "scholar-graph local rate-limit wait exceeded budget; triggering 429 fallback without daily bench"
            );
            return Ok((Some(429), None));
        }
        Err(e) => {
            tracing::debug!(error = %e, "scholar-graph HTTP request failed");
            return Ok((None, None));
        }
    };
    let status = resp.status;
    if status == 429
        && let (Some(c), Some(p)) = (core, provider)
    {
        let _ = c.note_rate_limited(p).await;
    }
    if !resp.is_success() {
        return Ok((Some(status), None));
    }
    match serde_json::from_slice::<Value>(&resp.body) {
        Ok(val) => Ok((Some(status), Some(val))),
        Err(e) => {
            tracing::debug!(error = %e, status, "scholar-graph JSON parse failed");
            Ok((Some(status), None))
        }
    }
}
