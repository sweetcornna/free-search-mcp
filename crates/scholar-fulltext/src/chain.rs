//! Ordered 11-step lawful full-text resolution chain (`SPEC.md` §6, `academic.md` §11).

use crate::bioc::parse_bioc_json;
use crate::html5::{parse_arxiv_html5, parse_generic_html5};
use crate::ids::{
    ParsedPaperId, extract_zenodo_id_from_doi, is_biorxiv_doi, is_lawful_candidate_url,
    is_plos_doi, is_springer_nature_doi, normalize_arxiv_id, normalize_doi, normalize_pmcid,
    normalize_pmid, parse_paper_id, plos_journal_slug,
};
use crate::jats::parse_jats_xml_with_limit;
use crate::sections::{
    ParsedDocument, ParsedSection, decode_response_text, extract_text_sections, filter_and_budget,
    has_fulltext_article_structure, looks_like_paywall_or_stub, normalize_license,
};
use fsm_core::ext::w3c::{FulltextReport, UNPAYWALL_SKIP_NOTE};
use fsm_core::{
    CallCx, DocApi, FetchApi, FetchOpts, FetchOutcome, FsmError, FulltextOpts, FulltextResult,
    HealthRecord, HttpBytes, HttpRequest, NetCx, SourceDeps, StructuredMeta, TransportProfile,
};
use fsm_scholar::{Provider, ScholarCore};
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};
use tracing::debug;

/// 30-day cache TTL for resolved full-text documents (`SPEC.md` §5).
const FULLTEXT_CACHE_TTL_S: u64 = 30 * 86_400;

/// 24-hour cache TTL for confirmed-unavailable / closed papers (M3).
const NEGATIVE_CACHE_TTL_S: u64 = 86_400;

/// Cooldown after a CORE v3 HTTP 429 rate-limit response (M3).
const CORE_BENCH_TTL_S: u64 = 300;
const CORE_BENCH_CACHE_KEY: &str = "scholar:bench:core_ac";

/// Per-step network timeout ceiling (clamped by `CallCx::remaining()` minus reserve — I6).
const STEP_TIMEOUT_S: u64 = 6;

/// Maximum response bytes for XML / JSON / HTML full-text fetches (15 MiB).
const MAX_FULLTEXT_BYTES: u64 = 15 * 1024 * 1024;

/// Maximum response bytes for Crossref work records (10 MiB for large author lists — M10).
const CROSSREF_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// Lean OpenAlex field selection so works with thousands of authors stay small (M10).
const OPENALEX_SELECT: &str = "id,ids,doi,open_access,best_oa_location,locations,primary_location";

/// Percent-encoding set for DOI paths (preserves `/` and unreserved chars, encodes `?`, `#`, `%`, spaces).
const DOI_PATH_ENCODE: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}');

/// Percent-encoding set for a single path segment (encodes `/` -> `%2F`).
const SEGMENT_ENCODE: &AsciiSet = &DOI_PATH_ENCODE.add(b'/').add(b':');

/// Subscriber-only TDM API hosts that require institutional tokens (`academic.md` L455).
const SUBSCRIBER_TDM_HOSTS: &[&str] = &["api.wiley.com", "api.elsevier.com", "tdm.wiley.com"];

/// Cache payload stored in `api_cache` under `fulltext:v1:{s|p}:{canonical_id}` (B1, M3).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum CachedEntry {
    Negative(CachedNegative),
    Positive(CachedFulltext),
}

/// Negative cache record for confirmed-closed or not-found papers (M3).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedNegative {
    negative: bool,
    why: Option<String>,
    oa_status: Option<String>,
    landing_url: Option<String>,
    url: Option<String>,
    #[serde(default)]
    unpaywall_skipped: bool,
    #[serde(default)]
    notes: Vec<String>,
}

/// Cached unfiltered full-text artifact stored in `StoreApi::api_cache_*` (B1).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedFulltext {
    format: String,
    source: String,
    license: Option<String>,
    url: String,
    oa_status: Option<String>,
    landing_url: Option<String>,
    title: Option<String>,
    #[serde(default)]
    truncated: bool,
    sections: Vec<CachedSection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedSection {
    heading: String,
    text: String,
    subsections: Vec<(String, String)>,
}

impl CachedFulltext {
    #[allow(clippy::too_many_arguments)]
    fn from_parsed(
        format: &str,
        source: &str,
        license: Option<String>,
        url: String,
        oa_status: Option<String>,
        landing_url: Option<String>,
        truncated: bool,
        doc: &ParsedDocument,
    ) -> Self {
        Self {
            format: format.to_owned(),
            source: source.to_owned(),
            license: license.or_else(|| doc.license.clone()),
            url,
            oa_status,
            landing_url,
            title: doc.title.clone(),
            truncated,
            sections: doc
                .sections
                .iter()
                .map(|s| CachedSection {
                    heading: s.heading.clone(),
                    text: s.text.clone(),
                    subsections: s
                        .subsections
                        .iter()
                        .map(|sub| (sub.heading.clone(), sub.text.clone()))
                        .collect(),
                })
                .collect(),
        }
    }

    fn into_parsed(
        self,
    ) -> (
        String,
        String,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
        ParsedDocument,
    ) {
        let doc = ParsedDocument {
            title: self.title,
            license: self.license.clone(),
            sections: self
                .sections
                .into_iter()
                .map(|s| ParsedSection {
                    heading: s.heading,
                    text: s.text,
                    subsections: s
                        .subsections
                        .into_iter()
                        .map(|(h, t)| fsm_core::FulltextSection::new(h, t))
                        .collect(),
                })
                .collect(),
        };
        (
            self.format,
            self.source,
            self.license,
            self.url,
            self.oa_status,
            self.landing_url,
            doc,
        )
    }
}

/// Classification of one upstream HTTP attempt (tracks M4 ok/error counts & 429 benching).
enum HttpOutcome {
    Ok(HttpBytes),
    NotFound,
    RateLimited,
    Error,
}

/// Mutable state carried across the 11 resolution steps.
struct ChainState {
    ids: ParsedPaperId,
    opts: FulltextOpts,
    started: Instant,
    budget_reserve: Duration,
    budget_exhausted_after: Option<String>,
    notes: Vec<String>,
    steps_tried: Vec<String>,
    /// Positive open-access evidence (`gold`, `green`, `hybrid`, `bronze`, `diamond` — I4).
    oa_positive: Option<String>,
    /// True if any metadata source reported `closed` or `is_oa: false` (I4).
    saw_closed: bool,
    license: Option<String>,
    /// Publisher or repository landing URL (never an aggregator page like S2/CORE — I5).
    landing_url: Option<String>,
    /// Last-resort aggregator landing URL (e.g. semanticscholar.org, core.ac.uk), never used
    /// by Signposting or Wayback (I5).
    fallback_landing_url: Option<String>,
    unpaywall_skipped_no_email: bool,
    /// Upstream response accounting (M4): answered (2xx/404) vs transient failures (429/5xx/net).
    upstream_ok_count: usize,
    upstream_err_count: usize,
    /// Deferred OA PDF URLs (e.g., from arXiv or publisher `citation_pdf_url` when structured steps run first).
    deferred_pdfs: Vec<(String, String, Option<String>)>,
    /// OA PDF / full-text URLs that failed to download/parse — inputs for Step 11 Wayback CDX.
    failed_oa_urls: Vec<String>,
    /// URLs already attempted (dedup across steps).
    tried_urls: HashSet<String>,
    /// Track whether structured routes were already run (prevents duplicate loops on newly discovered IDs).
    tried_epmc_pmcid: Option<String>,
    tried_bioc_pmcid: Option<String>,
    tried_arxiv_id: Option<String>,
}

impl ChainState {
    fn new(
        ids: ParsedPaperId,
        opts: FulltextOpts,
        started: Instant,
        initial_rem: Duration,
    ) -> Self {
        let landing_url = ids.landing_url.clone();
        let budget_reserve = compute_budget_reserve(initial_rem);
        Self {
            ids,
            opts,
            started,
            budget_reserve,
            budget_exhausted_after: None,
            notes: Vec::new(),
            steps_tried: Vec::new(),
            oa_positive: None,
            saw_closed: false,
            license: None,
            landing_url,
            fallback_landing_url: None,
            unpaywall_skipped_no_email: false,
            upstream_ok_count: 0,
            upstream_err_count: 0,
            deferred_pdfs: Vec::new(),
            failed_oa_urls: Vec::new(),
            tried_urls: HashSet::new(),
            tried_epmc_pmcid: None,
            tried_bioc_pmcid: None,
            tried_arxiv_id: None,
        }
    }

    fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn mark_budget_exhausted(&mut self) {
        if self.budget_exhausted_after.is_none() {
            let last = self
                .steps_tried
                .last()
                .cloned()
                .unwrap_or_else(|| "init".to_owned());
            self.budget_exhausted_after = Some(last);
        }
    }

    /// Check cancellation and remaining step budget before starting a new step (I6).
    /// Returns `Err(Timeout(elapsed_ms))` on explicit cancellation, `Ok(false)` when
    /// budget dropped below the reserve, or `Ok(true)` when the step may proceed.
    fn check_step_budget(&mut self, cx: &CallCx) -> Result<bool, FsmError> {
        if cx.cancel.is_cancelled() {
            return Err(FsmError::Timeout(self.elapsed_ms()));
        }
        if self.budget_exhausted_after.is_some() || cx.remaining() <= self.budget_reserve {
            self.mark_budget_exhausted();
            return Ok(false);
        }
        Ok(true)
    }

    fn set_landing_if_empty(&mut self, url: Option<&str>) {
        if self.landing_url.is_none()
            && let Some(u) = url.map(str::trim).filter(|s| s.starts_with("http"))
            && is_lawful_candidate_url(u)
            && !is_bare_doi_resolver_url(u)
        {
            self.landing_url = Some(u.to_owned());
        }
    }

    fn set_fallback_landing_if_empty(&mut self, url: Option<&str>) {
        if self.fallback_landing_url.is_none()
            && let Some(u) = url.map(str::trim).filter(|s| s.starts_with("http"))
            && is_lawful_candidate_url(u)
        {
            self.fallback_landing_url = Some(u.to_owned());
        }
    }

    fn set_license_if_empty(&mut self, lic: Option<&str>) {
        if self.license.is_none()
            && let Some(norm) = lic.and_then(normalize_license)
        {
            self.license = Some(norm);
        }
    }

    fn record_oa_status(&mut self, status: &str) {
        let norm = status.trim().to_ascii_lowercase();
        if norm.is_empty() || norm == "unknown" {
            return;
        }
        if norm == "closed" {
            self.saw_closed = true;
        } else {
            self.oa_positive = Some(norm);
        }
    }

    fn record_is_oa(&mut self, is_oa: bool) {
        if is_oa {
            if self.oa_positive.is_none() {
                self.oa_positive = Some("green".to_owned());
            }
        } else {
            self.saw_closed = true;
        }
    }

    fn is_confirmed_closed(&self) -> bool {
        self.saw_closed && self.oa_positive.is_none()
    }

    fn effective_oa_status(&self) -> Option<String> {
        self.oa_positive
            .clone()
            .or_else(|| self.saw_closed.then(|| "closed".to_owned()))
    }

    fn record_failed_oa_url(&mut self, url: &str) {
        if is_lawful_candidate_url(url) && !self.failed_oa_urls.iter().any(|u| u == url) {
            self.failed_oa_urls.push(url.to_owned());
        }
    }

    fn record_outcome_stats(&mut self, outcome: &HttpOutcome) {
        match outcome {
            HttpOutcome::Ok(_) | HttpOutcome::NotFound => self.upstream_ok_count += 1,
            HttpOutcome::RateLimited | HttpOutcome::Error => self.upstream_err_count += 1,
        }
    }

    fn default_landing_url(&self) -> Option<String> {
        if let Some(ref u) = self.landing_url {
            return Some(u.clone());
        }
        if let Some(ref d) = self.ids.doi {
            return Some(format!("https://doi.org/{d}"));
        }
        if let Some(ref a) = self.ids.arxiv {
            return Some(format!("https://arxiv.org/abs/{a}"));
        }
        if let Some(ref pmc) = self.ids.pmcid {
            return Some(format!("https://pmc.ncbi.nlm.nih.gov/articles/{pmc}/"));
        }
        if let Some(ref pmid) = self.ids.pmid {
            return Some(format!("https://pubmed.ncbi.nlm.nih.gov/{pmid}/"));
        }
        if let Some(ref hal) = self.ids.hal {
            return Some(format!("https://hal.science/{hal}"));
        }
        if let Some(ref z) = self.ids.zenodo {
            return Some(format!("https://zenodo.org/records/{z}"));
        }
        self.fallback_landing_url.clone()
    }
}

/// The lawful 11-step structured-first full-text chain.
pub struct ScholarFulltext {
    core: Arc<ScholarCore>,
    docs: Arc<dyn DocApi>,
    fetch: Arc<dyn FetchApi>,
    deps: SourceDeps,
    core_benched_until: AtomicI64,
}

impl ScholarFulltext {
    pub fn new(
        core: Arc<ScholarCore>,
        docs: Arc<dyn DocApi>,
        fetch: Arc<dyn FetchApi>,
        deps: SourceDeps,
    ) -> Self {
        Self {
            core,
            docs,
            fetch,
            deps,
            core_benched_until: AtomicI64::new(0),
        }
    }

    /// Frozen facade entry point (`SPEC-contracts.md` §5.2).
    pub async fn fulltext(
        &self,
        id_or_doi: &str,
        opts: FulltextOpts,
        cx: &CallCx,
    ) -> Result<FulltextResult, FsmError> {
        Ok(self.fulltext_report(id_or_doi, opts, cx).await?.result)
    }

    /// Full-text resolution returning both the wire [`FulltextResult`] and
    /// diagnostic `notes` / `steps_tried` (including the I16 Unpaywall skip note).
    pub async fn fulltext_report(
        &self,
        id_or_doi: &str,
        opts: FulltextOpts,
        cx: &CallCx,
    ) -> Result<FulltextReport, FsmError> {
        if opts.max_chars == Some(0) {
            return Err(FsmError::invalid("maxChars must be greater than 0"));
        }
        if cx.cancel.is_cancelled() || cx.remaining().is_zero() {
            return Err(FsmError::Timeout(0));
        }

        let started = Instant::now();
        let initial_rem = cx.remaining();
        let mut ids = parse_paper_id(id_or_doi)?;

        // Cheaply enrich known IDs from `StoreApi::scholar_get` (zero network).
        for key in ids.store_lookup_keys() {
            if let Ok(Some(work)) = self.deps.store.scholar_get(&key).await {
                ids.merge_work(&work);
                break;
            }
        }

        // I8: If caller passed a publisher/repository URL with no embedded PID, try
        // `FetchApi::structured` first (citation_doi / dc.identifier / prism.doi / JSON-LD /
        // citation_pdf_url), then fall back to `ScholarCore::work`.
        let mut url_deferred_pdfs: Vec<(String, String, Option<String>)> = Vec::new();
        let mut url_meta_ok = 0usize;
        let mut url_meta_err = 0usize;
        if !ids.has_scholarly_pid()
            && let Some(url) = ids.landing_url.clone()
        {
            let mut extracted_title: Option<String> = None;
            if is_lawful_candidate_url(&url) && self.deps.net.check_url_offline(&url).is_ok() {
                match self.fetch.structured(&url, cx).await {
                    Ok(meta) => {
                        url_meta_ok += 1;
                        extracted_title = extract_ids_from_structured_meta(
                            &meta,
                            &mut ids,
                            &mut url_deferred_pdfs,
                        );
                    }
                    Err(FsmError::Timeout(_)) if cx.cancel.is_cancelled() => {
                        return Err(FsmError::Timeout(elapsed_ms_since(started)));
                    }
                    Err(e) => {
                        url_meta_err += 1;
                        debug!("FetchApi::structured failed for {url}: {e}");
                    }
                }
            }
            if ids.has_scholarly_pid() {
                for key in ids.store_lookup_keys() {
                    if let Ok(Some(work)) = self.deps.store.scholar_get(&key).await {
                        ids.merge_work(&work);
                        break;
                    }
                }
            } else {
                let query = extracted_title.as_deref().unwrap_or(url.as_str());
                match self.core.work(query, cx).await {
                    Ok(Some(work)) => ids.merge_work(&work),
                    Ok(None) => {}
                    Err(FsmError::Timeout(_)) if cx.cancel.is_cancelled() => {
                        return Err(FsmError::Timeout(elapsed_ms_since(started)));
                    }
                    Err(e) => {
                        debug!("ScholarCore::work fallback for URL failed: {e}");
                    }
                }
            }
        }

        // If caller passed a bare title (no PID or URL), resolve via ScholarCore::work.
        if !ids.has_any_id() {
            match self.core.work(&ids.raw, cx).await? {
                Some(work) => {
                    ids.merge_work(&work);
                }
                None => {
                    let mut res = FulltextResult::new(ids.canonical_id, false);
                    res.why = Some("work not found; pass a DOI, arXiv ID, or PMCID".to_owned());
                    return Ok(FulltextReport::new(res));
                }
            }
        }

        // Check positive AND negative full-text cache (B1, M3) before hitting upstreams.
        let cache_key = fulltext_cache_key(&ids.canonical_id, opts.prefer_structured);
        let has_contact_email = valid_contact_email(&self.deps.cfg.contact_email).is_some();
        if let Ok(Some((bytes, _, _))) = self.deps.store.api_cache_get(&cache_key).await
            && let Ok(entry) = serde_json::from_slice::<CachedEntry>(&bytes)
        {
            match entry {
                CachedEntry::Positive(cached)
                    if self.deps.net.check_url_offline(&cached.url).is_ok()
                        && is_lawful_candidate_url(&cached.url) =>
                {
                    let (format, source, license, url, oa_status, landing_url, doc) =
                        cached.into_parsed();
                    let result = self.build_available_result(
                        &ids.canonical_id,
                        &format,
                        &source,
                        license,
                        url,
                        oa_status,
                        landing_url,
                        &doc,
                        &opts,
                    );
                    let mut report = FulltextReport::new(result);
                    report.steps_tried.push("cache:hit".to_owned());
                    return Ok(report);
                }
                CachedEntry::Negative(neg)
                    if neg.negative && !(neg.unpaywall_skipped && has_contact_email) =>
                {
                    let mut res = FulltextResult::new(ids.canonical_id.clone(), false);
                    res.why = neg.why;
                    res.oa_status = neg.oa_status;
                    res.landing_url = neg.landing_url;
                    res.url = neg.url;
                    res.notes = neg.notes.clone();
                    let mut report = FulltextReport::new(res);
                    report.notes = neg.notes;
                    report.steps_tried.push("cache:negative_hit".to_owned());
                    return Ok(report);
                }
                _ => {}
            }
        }

        let mut st = ChainState::new(ids, opts, started, initial_rem);
        st.upstream_ok_count += url_meta_ok;
        st.upstream_err_count += url_meta_err;
        st.deferred_pdfs.extend(url_deferred_pdfs);

        // Step 1: PMC ID Converter (DOI/PMID -> PMCID)
        if st.check_step_budget(cx)? {
            self.step1_pmc_idconv(&mut st, cx).await?;
        }

        // Step 2: Europe PMC JATS XML
        if st.check_step_budget(cx)?
            && let Some(res) = self.step2_europepmc_jats(&mut st, cx).await?
        {
            return Ok(self.finish_hit(&cache_key, res, st).await);
        }

        // Step 3: PMC BioC JSON
        if st.check_step_budget(cx)?
            && let Some(res) = self.step3_pmc_bioc(&mut st, cx).await?
        {
            return Ok(self.finish_hit(&cache_key, res, st).await);
        }

        // Step 4: bioRxiv / medRxiv JATS (and direct PDF fallback)
        if st.check_step_budget(cx)?
            && let Some(res) = self.step4_biorxiv_medrxiv(&mut st, cx).await?
        {
            return Ok(self.finish_hit(&cache_key, res, st).await);
        }

        // Step 5: arXiv HTML5 (`ar5iv` fallback, plus arXiv PDF fallback)
        if st.check_step_budget(cx)?
            && let Some(res) = self.step5_arxiv_html5(&mut st, cx).await?
        {
            return Ok(self.finish_hit(&cache_key, res, st).await);
        }

        // Step 6: PLOS / Springer OA JATS
        if st.check_step_budget(cx)?
            && let Some(res) = self.step6_plos_springer_jats(&mut st, cx).await?
        {
            return Ok(self.finish_hit(&cache_key, res, st).await);
        }

        // Step 7: Unpaywall (skipped cleanly with notes when CONTACT_EMAIL unset — I16)
        if st.check_step_budget(cx)?
            && let Some(res) = self.step7_unpaywall(&mut st, cx).await?
        {
            return Ok(self.finish_hit(&cache_key, res, st).await);
        }

        // Step 8: OpenAlex $0 `best_oa_location` + Semantic Scholar `openAccessPdf` (concurrent fetch, ordered evaluation — I6)
        if st.check_step_budget(cx)?
            && let Some(res) = self.step8_openalex_s2(&mut st, cx).await?
        {
            return Ok(self.finish_hit(&cache_key, res, st).await);
        }

        // Steps 9 + 10a–10e: CORE v3 + DOAJ / Crossref TDM / DataCite / HAL / Zenodo
        // (concurrent metadata fetch, strict spec-ordered candidate evaluation — I6)
        if st.check_step_budget(cx)?
            && let Some(res) = self.step9_and_10_repos(&mut st, cx).await?
        {
            return Ok(self.finish_hit(&cache_key, res, st).await);
        }

        // Step 10f: FAIR Signposting HEAD on real publisher/repository landing URL
        if st.check_step_budget(cx)?
            && let Some(res) = self.step10f_signposting(&mut st, cx).await?
        {
            return Ok(self.finish_hit(&cache_key, res, st).await);
        }

        // Try any deferred OA PDFs (e.g. arXiv PDF or publisher `citation_pdf_url` deferred while structured JATS ran).
        let deferred = std::mem::take(&mut st.deferred_pdfs);
        for (pdf_url, source, lic) in deferred {
            if !st.check_step_budget(cx)? {
                break;
            }
            if let Some(res) = self
                .try_pdf_candidate(&mut st, &pdf_url, &source, lic, Some("green"), cx)
                .await?
            {
                return Ok(self.finish_hit(&cache_key, res, st).await);
            }
        }

        // Step 11: Wayback CDX last-resort fallback for link-rotted OA URLs
        if st.check_step_budget(cx)?
            && let Some(res) = self.step11_wayback_cdx(&mut st, cx).await?
        {
            return Ok(self.finish_hit(&cache_key, res, st).await);
        }

        Ok(self.finish_miss(&cache_key, st).await)
    }

    // -----------------------------------------------------------------------
    // Step 1: PMC ID Converter
    // -----------------------------------------------------------------------
    async fn step1_pmc_idconv(&self, st: &mut ChainState, cx: &CallCx) -> Result<(), FsmError> {
        if st.ids.pmcid.is_some() {
            return Ok(());
        }
        let Some(query_id) = st
            .ids
            .non_arxiv_doi()
            .or(st.ids.pmid.as_deref())
            .map(str::to_owned)
        else {
            return Ok(());
        };

        st.steps_tried.push("step1:pmc_idconv".to_owned());
        let mut req =
            HttpRequest::get("https://pmc.ncbi.nlm.nih.gov/tools/idconv/api/v1/articles/")
                .query_param("ids", query_id)
                .query_param("format", "json")
                .query_param("tool", "free-search-mcp")
                .header("Accept", "application/json")
                .for_source("pmc_idconv")
                .with_max_bytes(512 * 1024);

        if let Some(email) = valid_contact_email(&self.deps.cfg.contact_email) {
            req = req.query_param("email", email).redact_query_key("email");
        }
        if let Some(key) = self.deps.cfg.api_key("SEARCH_MCP_NCBI_API_KEY") {
            req = req.query_param("api_key", key).redact_query_key("api_key");
        }

        let Some(resp) = self.send_ok(req, st, cx).await? else {
            return Ok(());
        };
        let Ok(json) = serde_json::from_slice::<Value>(&resp.body) else {
            return Ok(());
        };
        let Some(rec) = json
            .get("records")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
        else {
            return Ok(());
        };
        if rec.get("status").and_then(Value::as_str) == Some("error") {
            return Ok(());
        }
        if let Some(pmc) = rec
            .get("pmcid")
            .and_then(Value::as_str)
            .and_then(normalize_pmcid)
        {
            st.ids.pmcid = Some(pmc);
        }
        if st.ids.pmid.is_none()
            && let Some(pmid_val) = rec.get("pmid")
        {
            let pmid_str = pmid_val
                .as_str()
                .map(str::to_owned)
                .or_else(|| pmid_val.as_u64().map(|n| n.to_string()));
            if let Some(pmid) = pmid_str.as_deref().and_then(normalize_pmid) {
                st.ids.pmid = Some(pmid);
            }
        }
        if st.ids.doi.is_none()
            && let Some(doi) = rec
                .get("doi")
                .and_then(Value::as_str)
                .and_then(normalize_doi)
        {
            st.ids.doi = Some(doi);
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Step 2: Europe PMC JATS XML
    // -----------------------------------------------------------------------
    async fn step2_europepmc_jats(
        &self,
        st: &mut ChainState,
        cx: &CallCx,
    ) -> Result<Option<ResolvedHit>, FsmError> {
        if !st.opts.prefer_structured {
            return Ok(None);
        }
        let Some(pmcid) = st.ids.pmcid.clone() else {
            return Ok(None);
        };
        if st.tried_epmc_pmcid.as_deref() == Some(pmcid.as_str()) {
            return Ok(None);
        }
        if !st.check_step_budget(cx)? {
            return Ok(None);
        }
        st.tried_epmc_pmcid = Some(pmcid.clone());
        st.steps_tried.push("step2:europepmc_jats".to_owned());

        let url = format!("https://www.ebi.ac.uk/europepmc/webservices/rest/{pmcid}/fullTextXML");
        let req = HttpRequest::get(&url)
            .header("Accept", "application/xml")
            .for_source("europepmc")
            .with_max_bytes(MAX_FULLTEXT_BYTES);

        let Some(resp) = self.send_ok(req, st, cx).await? else {
            return Ok(None);
        };
        let xml = decode_response_text(&resp);
        if !xml.contains("<article") {
            return Ok(None);
        }
        match parse_jats_blocking(xml, self.deps.cfg.max_document_chars).await {
            Ok(doc) => {
                let landing = st
                    .landing_url
                    .clone()
                    .unwrap_or_else(|| format!("https://europepmc.org/article/PMC/{pmcid}"));
                let lic = doc.license.clone().or_else(|| st.license.clone());
                Ok(Some(ResolvedHit::complete(
                    "jats",
                    "europepmc",
                    lic,
                    url,
                    Some("green".to_owned()),
                    Some(landing),
                    doc,
                )))
            }
            Err(e) => {
                debug!("Europe PMC JATS parse failed for {pmcid}: {e}");
                Ok(None)
            }
        }
    }

    // -----------------------------------------------------------------------
    // Step 3: PMC BioC JSON
    // -----------------------------------------------------------------------
    async fn step3_pmc_bioc(
        &self,
        st: &mut ChainState,
        cx: &CallCx,
    ) -> Result<Option<ResolvedHit>, FsmError> {
        if !st.opts.prefer_structured {
            return Ok(None);
        }
        let Some(pmcid) = st.ids.pmcid.clone() else {
            return Ok(None);
        };
        if st.tried_bioc_pmcid.as_deref() == Some(pmcid.as_str()) {
            return Ok(None);
        }
        if !st.check_step_budget(cx)? {
            return Ok(None);
        }
        st.tried_bioc_pmcid = Some(pmcid.clone());
        st.steps_tried.push("step3:pmc_bioc".to_owned());

        let url = format!(
            "https://www.ncbi.nlm.nih.gov/research/bionlp/RESTful/pmcoa.cgi/BioC_json/{pmcid}/unicode"
        );
        let req = HttpRequest::get(&url)
            .header("Accept", "application/json")
            .for_source("pmc_bioc")
            .with_max_bytes(MAX_FULLTEXT_BYTES);

        let Some(resp) = self.send_ok(req, st, cx).await? else {
            return Ok(None);
        };
        let text = decode_response_text(&resp);
        match parse_bioc_blocking(text).await {
            Ok(doc) => {
                let landing = st
                    .landing_url
                    .clone()
                    .unwrap_or_else(|| format!("https://pmc.ncbi.nlm.nih.gov/articles/{pmcid}/"));
                let lic = doc.license.clone().or_else(|| st.license.clone());
                Ok(Some(ResolvedHit::complete(
                    "bioc",
                    "pmc_bioc",
                    lic,
                    url,
                    Some("green".to_owned()),
                    Some(landing),
                    doc,
                )))
            }
            Err(e) => {
                debug!("PMC BioC parse failed for {pmcid}: {e}");
                Ok(None)
            }
        }
    }

    // -----------------------------------------------------------------------
    // Step 4: bioRxiv / medRxiv JATS XML
    // -----------------------------------------------------------------------
    async fn step4_biorxiv_medrxiv(
        &self,
        st: &mut ChainState,
        cx: &CallCx,
    ) -> Result<Option<ResolvedHit>, FsmError> {
        let Some(doi) = st
            .ids
            .non_arxiv_doi()
            .filter(|d| is_biorxiv_doi(d))
            .map(str::to_owned)
        else {
            return Ok(None);
        };
        st.steps_tried.push("step4:biorxiv_medrxiv".to_owned());

        let servers: [&str; 2] = if st.ids.biorxiv_server_hint == Some("medrxiv") {
            ["medrxiv", "biorxiv"]
        } else {
            ["biorxiv", "medrxiv"]
        };

        for server in servers {
            if !st.check_step_budget(cx)? {
                return Ok(None);
            }
            let api_url = format!("https://api.biorxiv.org/details/{server}/{doi}");
            let req = HttpRequest::get(&api_url)
                .header("Accept", "application/json")
                .for_source(server)
                .with_max_bytes(1024 * 1024);

            let Some(resp) = self.send_ok(req, st, cx).await? else {
                continue;
            };
            let Ok(json) = serde_json::from_slice::<Value>(&resp.body) else {
                continue;
            };
            let Some(collection) = json.get("collection").and_then(Value::as_array) else {
                continue;
            };
            if collection.is_empty() {
                continue;
            }

            // Pick latest version (preferring one with a `jatsxml` link).
            let chosen = collection
                .iter()
                .max_by_key(|item| {
                    let ver = item
                        .get("version")
                        .and_then(|v| {
                            v.as_u64()
                                .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
                        })
                        .unwrap_or(0);
                    let has_xml = item
                        .get("jatsxml")
                        .and_then(Value::as_str)
                        .is_some_and(|s| s.starts_with("http"));
                    (has_xml, ver)
                })
                .expect("collection non-empty");

            let version = chosen
                .get("version")
                .and_then(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .or_else(|| v.as_u64().map(|n| n.to_string()))
                })
                .unwrap_or_else(|| "1".to_owned());
            let landing = format!("https://www.{server}.org/content/{doi}v{version}");
            st.set_landing_if_empty(Some(&landing));
            let lic = chosen
                .get("license")
                .and_then(Value::as_str)
                .and_then(normalize_license);
            st.set_license_if_empty(lic.as_deref());
            if st.oa_positive.is_none() {
                st.record_oa_status("green");
            }

            let pdf_url = format!("{landing}.full.pdf");
            let jats_url = chosen
                .get("jatsxml")
                .and_then(Value::as_str)
                .filter(|u| is_lawful_candidate_url(u))
                .map(str::to_owned);

            if st.opts.prefer_structured
                && let Some(ref xml_url) = jats_url
                && let Some(hit) = self
                    .try_jats_url(
                        st,
                        xml_url,
                        server,
                        lic.clone(),
                        Some("green"),
                        Some(landing.clone()),
                        cx,
                    )
                    .await?
            {
                return Ok(Some(hit));
            }

            // Fall back to direct bioRxiv/medRxiv PDF (or primary path when prefer_structured=false).
            if let Some(hit) = self
                .try_pdf_candidate(st, &pdf_url, server, lic.clone(), Some("green"), cx)
                .await?
            {
                return Ok(Some(hit));
            }

            // If prefer_structured=false and PDF failed, try JATS as backup.
            if !st.opts.prefer_structured
                && let Some(ref xml_url) = jats_url
                && let Some(hit) = self
                    .try_jats_url(st, xml_url, server, lic, Some("green"), Some(landing), cx)
                    .await?
            {
                return Ok(Some(hit));
            }
            break;
        }

        Ok(None)
    }

    // -----------------------------------------------------------------------
    // Step 5: arXiv HTML5 (`ar5iv` fallback) + PDF fallback
    // -----------------------------------------------------------------------
    async fn step5_arxiv_html5(
        &self,
        st: &mut ChainState,
        cx: &CallCx,
    ) -> Result<Option<ResolvedHit>, FsmError> {
        let Some(fetch_id) = st.ids.arxiv_fetch_id().map(str::to_owned) else {
            return Ok(None);
        };
        let base_id = st.ids.arxiv.clone().unwrap_or_else(|| fetch_id.clone());
        if st.tried_arxiv_id.as_deref() == Some(base_id.as_str()) {
            return Ok(None);
        }
        if !st.check_step_budget(cx)? {
            return Ok(None);
        }
        st.tried_arxiv_id = Some(base_id.clone());
        st.steps_tried.push("step5:arxiv_html5".to_owned());

        let landing = format!("https://arxiv.org/abs/{base_id}");
        if st.ids.non_arxiv_doi().is_none() {
            st.set_landing_if_empty(Some(&landing));
        }
        if st.oa_positive.is_none() {
            st.record_oa_status("green");
        }

        let pdf_url = format!("https://arxiv.org/pdf/{fetch_id}");

        // When prefer_structured == false, try arXiv PDF first via DocApi.
        if !st.opts.prefer_structured
            && let Some(hit) = self
                .try_pdf_candidate(st, &pdf_url, "arxiv_pdf", None, Some("green"), cx)
                .await?
        {
            return Ok(Some(hit));
        }

        // 5a: Official arXiv native HTML5 (`Allow: /html` in robots.txt)
        let native_url = format!("https://arxiv.org/html/{fetch_id}");
        if let Some(hit) = self
            .try_arxiv_html_url(st, &native_url, "arxiv_html5", &landing, cx)
            .await?
        {
            return Ok(Some(hit));
        }

        // 5b: ar5iv historical HTML5 mirror
        if !st.check_step_budget(cx)? {
            return Ok(None);
        }
        let ar5iv_url = format!("https://ar5iv.labs.arxiv.org/html/{fetch_id}");
        if let Some(hit) = self
            .try_arxiv_html_url(st, &ar5iv_url, "ar5iv", &landing, cx)
            .await?
        {
            return Ok(Some(hit));
        }

        // 5c: arXiv PDF fallback (never /e-print or /src per robots.txt).
        if st.opts.prefer_structured && st.ids.non_arxiv_doi().is_some() {
            // Defer arXiv PDF until after Step 6 structured JATS has a chance.
            st.deferred_pdfs
                .push((pdf_url, "arxiv_pdf".to_owned(), None));
            return Ok(None);
        }
        if st.opts.prefer_structured
            && st.check_step_budget(cx)?
            && let Some(hit) = self
                .try_pdf_candidate(st, &pdf_url, "arxiv_pdf", None, Some("green"), cx)
                .await?
        {
            return Ok(Some(hit));
        }

        Ok(None)
    }

    async fn try_arxiv_html_url(
        &self,
        st: &mut ChainState,
        url: &str,
        source: &str,
        landing: &str,
        cx: &CallCx,
    ) -> Result<Option<ResolvedHit>, FsmError> {
        let req = HttpRequest::get(url)
            .header("Accept", "text/html")
            .for_source(source)
            .with_max_bytes(MAX_FULLTEXT_BYTES);
        let Some(resp) = self.send_ok(req, st, cx).await? else {
            return Ok(None);
        };
        let html = decode_response_text(&resp);
        match parse_arxiv_html5_blocking(html).await {
            Ok(doc) => Ok(Some(ResolvedHit::complete(
                "html5",
                source,
                doc.license.clone(),
                url.to_owned(),
                Some("green".to_owned()),
                Some(landing.to_owned()),
                doc,
            ))),
            Err(e) => {
                debug!("{source} HTML5 parse failed for {url}: {e}");
                Ok(None)
            }
        }
    }

    // -----------------------------------------------------------------------
    // Step 6: PLOS / Springer OA JATS
    // -----------------------------------------------------------------------
    async fn step6_plos_springer_jats(
        &self,
        st: &mut ChainState,
        cx: &CallCx,
    ) -> Result<Option<ResolvedHit>, FsmError> {
        let Some(doi) = st.ids.non_arxiv_doi().map(str::to_owned) else {
            return Ok(None);
        };

        // 6a: PLOS JATS manuscript XML (GET only; HEAD returns 403 on R2 presigned redirects)
        if is_plos_doi(&doi) {
            st.steps_tried.push("step6:plos_jats".to_owned());
            let slug = plos_journal_slug(&doi);
            let landing = format!("https://journals.plos.org/{slug}/article?id={doi}");
            st.set_landing_if_empty(Some(&landing));
            if st.oa_positive.is_none() {
                st.record_oa_status("gold");
            }

            let xml_url =
                format!("https://journals.plos.org/{slug}/article/file?id={doi}&type=manuscript");
            let pdf_url =
                format!("https://journals.plos.org/{slug}/article/file?id={doi}&type=printable");

            if st.opts.prefer_structured
                && let Some(hit) = self
                    .try_jats_url(
                        st,
                        &xml_url,
                        "plos",
                        Some("CC-BY-4.0".to_owned()),
                        Some("gold"),
                        Some(landing.clone()),
                        cx,
                    )
                    .await?
            {
                return Ok(Some(hit));
            }
            if let Some(hit) = self
                .try_pdf_candidate(
                    st,
                    &pdf_url,
                    "plos",
                    Some("CC-BY-4.0".to_owned()),
                    Some("gold"),
                    cx,
                )
                .await?
            {
                return Ok(Some(hit));
            }
            if !st.opts.prefer_structured
                && let Some(hit) = self
                    .try_jats_url(
                        st,
                        &xml_url,
                        "plos",
                        Some("CC-BY-4.0".to_owned()),
                        Some("gold"),
                        Some(landing),
                        cx,
                    )
                    .await?
            {
                return Ok(Some(hit));
            }
            return Ok(None);
        }

        // 6b: Springer Nature OpenAccess JATS (Tier 3 key required)
        if st.opts.prefer_structured
            && is_springer_nature_doi(&doi)
            && let Some(key) = self
                .deps
                .cfg
                .api_key("SEARCH_MCP_SPRINGER_API_KEY")
                .or_else(|| self.deps.cfg.api_key("SEARCH_MCP_SPRINGER_OA_API_KEY"))
        {
            st.steps_tried.push("step6:springer_oa_jats".to_owned());
            let base = "https://api.springernature.com/openaccess/jats";
            let req = HttpRequest::get(base)
                .query_param("q", format!("doi:{doi}"))
                .query_param("api_key", key)
                .redact_query_key("api_key")
                .header("Accept", "application/xml")
                .for_source("springer_oa")
                .with_max_bytes(MAX_FULLTEXT_BYTES);

            if let Some(resp) = self.send_ok(req, st, cx).await? {
                let xml = decode_response_text(&resp);
                if xml.contains("<article")
                    && let Ok(doc) =
                        parse_jats_blocking(xml, self.deps.cfg.max_document_chars).await
                {
                    let lic = doc.license.clone().or_else(|| st.license.clone());
                    let display_url = format!("{base}?q=doi:{doi}");
                    return Ok(Some(ResolvedHit::complete(
                        "jats",
                        "springer_oa",
                        lic,
                        display_url,
                        Some("gold".to_owned()),
                        st.default_landing_url(),
                        doc,
                    )));
                }
            }
        }

        Ok(None)
    }

    // -----------------------------------------------------------------------
    // Step 7: Unpaywall (I16: skip cleanly with notes when CONTACT_EMAIL unset)
    // -----------------------------------------------------------------------
    async fn step7_unpaywall(
        &self,
        st: &mut ChainState,
        cx: &CallCx,
    ) -> Result<Option<ResolvedHit>, FsmError> {
        let Some(doi) = st.ids.non_arxiv_doi().map(str::to_owned) else {
            return Ok(None);
        };
        let Some(email) = valid_contact_email(&self.deps.cfg.contact_email) else {
            st.unpaywall_skipped_no_email = true;
            st.notes.push(UNPAYWALL_SKIP_NOTE.to_owned());
            st.steps_tried
                .push("step7:unpaywall:skipped_no_email".to_owned());
            return Ok(None);
        };

        st.steps_tried.push("step7:unpaywall".to_owned());
        let encoded_doi = encode_doi_path(&doi);
        let url = format!("https://api.unpaywall.org/v2/{encoded_doi}");
        let req = HttpRequest::get(&url)
            .query_param("email", email)
            .redact_query_key("email")
            .header("Accept", "application/json")
            .for_source("unpaywall")
            .with_max_bytes(512 * 1024);

        let Some(resp) = self.send_ok(req, st, cx).await? else {
            return Ok(None);
        };
        let Ok(json) = serde_json::from_slice::<Value>(&resp.body) else {
            return Ok(None);
        };

        if let Some(status) = json.get("oa_status").and_then(Value::as_str) {
            st.record_oa_status(status);
        }
        if let Some(is_oa) = json.get("is_oa").and_then(Value::as_bool) {
            st.record_is_oa(is_oa);
        }
        let is_oa = json.get("is_oa").and_then(Value::as_bool).unwrap_or(false);
        st.set_landing_if_empty(json.get("doi_url").and_then(Value::as_str));

        // Gather ordered locations: best_oa_location first, then oa_locations[].
        let mut locations: Vec<&Value> = Vec::new();
        if let Some(best) = json.get("best_oa_location").filter(|v| v.is_object()) {
            locations.push(best);
        }
        if let Some(arr) = json.get("oa_locations").and_then(Value::as_array) {
            for loc in arr {
                if loc.is_object() {
                    locations.push(loc);
                }
            }
        }

        let mut pdf_candidates: Vec<(String, Option<String>)> = Vec::new();
        let mut html_candidates: Vec<(String, Option<String>)> = Vec::new();

        for loc in locations {
            let lic = loc
                .get("license")
                .and_then(Value::as_str)
                .and_then(normalize_license);
            st.set_license_if_empty(lic.as_deref());
            if let Some(lp) = loc.get("url_for_landing_page").and_then(Value::as_str) {
                st.set_landing_if_empty(Some(lp));
                self.discover_ids_from_url(st, lp);
            }
            if let Some(pdf) = loc.get("url_for_pdf").and_then(Value::as_str)
                && is_lawful_candidate_url(pdf)
            {
                self.discover_ids_from_url(st, pdf);
                pdf_candidates.push((pdf.to_owned(), lic.clone()));
            }
            if is_oa
                && let Some(u) = loc
                    .get("url_for_landing_page")
                    .or_else(|| loc.get("url"))
                    .and_then(Value::as_str)
                && is_lawful_candidate_url(u)
            {
                html_candidates.push((u.to_owned(), lic));
            }
        }

        // I7: If Unpaywall revealed a PMCID or arXiv ID, try EPMC JATS + PMC BioC + arXiv HTML5!
        if let Some(hit) = self.try_newly_discovered_structured(st, cx).await? {
            return Ok(Some(hit));
        }

        let oa_status = st.effective_oa_status();
        for (pdf, lic) in pdf_candidates {
            if !st.check_step_budget(cx)? {
                return Ok(None);
            }
            if let Some(hit) = self
                .try_pdf_candidate(st, &pdf, "unpaywall", lic, oa_status.as_deref(), cx)
                .await?
            {
                return Ok(Some(hit));
            }
        }
        for (page_url, lic) in html_candidates {
            if !st.check_step_budget(cx)? {
                return Ok(None);
            }
            if let Some(hit) = self
                .try_oa_html_candidate(st, &page_url, "unpaywall", lic, oa_status.as_deref(), cx)
                .await?
            {
                return Ok(Some(hit));
            }
        }

        Ok(None)
    }

    // -----------------------------------------------------------------------
    // Step 8: OpenAlex `$0` `best_oa_location` / Semantic Scholar `openAccessPdf`
    // (concurrent metadata fetch, strict 8a -> 8b evaluation, shared W3a bench — I6, I7, I9)
    // -----------------------------------------------------------------------
    async fn step8_openalex_s2(
        &self,
        st: &mut ChainState,
        cx: &CallCx,
    ) -> Result<Option<ResolvedHit>, FsmError> {
        let oa_benched = self.core.benched_until(Provider::OpenAlex).await.is_some();
        let s2_benched = self
            .core
            .benched_until(Provider::SemanticScholar)
            .await
            .is_some();

        let oa_req = if !oa_benched {
            self.build_openalex_request(&st.ids)
        } else {
            None
        };
        let s2_req = if !s2_benched {
            self.build_s2_request(&st.ids)
        } else {
            None
        };

        let reserve = st.budget_reserve;
        let started = st.started;
        let (oa_res, s2_res) = tokio::join!(
            async {
                match oa_req {
                    Some(req) => Some(self.send_raw(req, cx, reserve, started).await),
                    None => None,
                }
            },
            async {
                match s2_req {
                    Some(req) => Some(self.send_raw(req, cx, reserve, started).await),
                    None => None,
                }
            },
        );

        let oa_outcome = oa_res.transpose()?;
        let s2_outcome = s2_res.transpose()?;

        // Immediately record any 429 benching from either concurrent lookup (I9).
        if let Some(ref o) = oa_outcome {
            st.record_outcome_stats(o);
            if matches!(o, HttpOutcome::RateLimited) {
                self.bench_scholar_provider(Provider::OpenAlex, st).await;
            }
        }
        if let Some(ref o) = s2_outcome {
            st.record_outcome_stats(o);
            if matches!(o, HttpOutcome::RateLimited) {
                self.bench_scholar_provider(Provider::SemanticScholar, st)
                    .await;
            }
        }

        // 8a: Evaluate OpenAlex first in spec order.
        if oa_benched {
            st.notes
                .push("OpenAlex skipped: benched until UTC midnight after HTTP 429".to_owned());
            st.steps_tried.push("step8a:openalex:benched".to_owned());
        } else if let Some(outcome) = oa_outcome {
            st.steps_tried.push("step8a:openalex".to_owned());
            if let HttpOutcome::Ok(resp) = outcome
                && let Some(hit) = self.process_openalex_response(&resp, st, cx).await?
            {
                return Ok(Some(hit));
            }
        }

        // 8b: Evaluate Semantic Scholar second in spec order.
        if !st.check_step_budget(cx)? {
            return Ok(None);
        }
        if s2_benched {
            st.notes.push(
                "Semantic Scholar skipped: benched until UTC midnight after HTTP 429".to_owned(),
            );
            st.steps_tried.push("step8b:s2:benched".to_owned());
        } else if let Some(outcome) = s2_outcome {
            st.steps_tried.push("step8b:semanticscholar".to_owned());
            if let HttpOutcome::Ok(resp) = outcome
                && let Some(hit) = self.process_s2_response(&resp, st, cx).await?
            {
                return Ok(Some(hit));
            }
        }

        Ok(None)
    }

    fn build_openalex_request(&self, ids: &ParsedPaperId) -> Option<HttpRequest> {
        let url = if let Some(doi) = ids.non_arxiv_doi() {
            format!(
                "https://api.openalex.org/works/https://doi.org/{}",
                encode_doi_path(doi)
            )
        } else if let Some(ref oa_id) = ids.openalex {
            format!("https://api.openalex.org/works/{oa_id}")
        } else {
            let pmid = ids.pmid.as_deref()?;
            format!("https://api.openalex.org/works/pmid:{pmid}")
        };

        let mut req = HttpRequest::get(&url)
            .query_param("select", OPENALEX_SELECT)
            .header("Accept", "application/json")
            .for_source("openalex")
            .with_max_bytes(2 * 1024 * 1024);
        if let Some(key) = self.deps.cfg.api_key("SEARCH_MCP_OPENALEX_API_KEY") {
            req = req.query_param("api_key", key).redact_query_key("api_key");
        }
        Some(req)
    }

    fn build_s2_request(&self, ids: &ParsedPaperId) -> Option<HttpRequest> {
        let paper_id = if let Some(doi) = ids.non_arxiv_doi() {
            format!("DOI:{}", encode_doi_path(doi))
        } else if let Some(ref a) = ids.arxiv {
            format!("ARXIV:{a}")
        } else if let Some(ref pmid) = ids.pmid {
            format!("PMID:{pmid}")
        } else if let Some(ref pmcid) = ids.pmcid {
            format!("PMCID:{pmcid}")
        } else {
            ids.s2.clone()?
        };

        let url = format!("https://api.semanticscholar.org/graph/v1/paper/{paper_id}");
        let mut req = HttpRequest::get(&url)
            .query_param("fields", "openAccessPdf,externalIds,url,isOpenAccess")
            .header("Accept", "application/json")
            .for_source("semanticscholar")
            .with_max_bytes(512 * 1024);
        if let Some(key) = self
            .deps
            .cfg
            .api_key("SEARCH_MCP_S2_API_KEY")
            .or_else(|| self.deps.cfg.api_key("SEARCH_MCP_SEMANTICSCHOLAR_API_KEY"))
        {
            req = req.header("x-api-key", key).redact_header("x-api-key");
        }
        Some(req)
    }

    async fn process_openalex_response(
        &self,
        resp: &HttpBytes,
        st: &mut ChainState,
        cx: &CallCx,
    ) -> Result<Option<ResolvedHit>, FsmError> {
        let Ok(json) = serde_json::from_slice::<Value>(&resp.body) else {
            return Ok(None);
        };

        // Enrich IDs (pmcid, pmid, arxiv)
        if let Some(ids_obj) = json.get("ids").and_then(Value::as_object) {
            if st.ids.pmcid.is_none()
                && let Some(pmc) = ids_obj
                    .get("pmcid")
                    .and_then(Value::as_str)
                    .and_then(normalize_pmcid)
            {
                st.ids.pmcid = Some(pmc);
            }
            if st.ids.pmid.is_none()
                && let Some(pmid) = ids_obj
                    .get("pmid")
                    .and_then(Value::as_str)
                    .and_then(|s| s.rsplit('/').next())
                    .and_then(normalize_pmid)
            {
                st.ids.pmid = Some(pmid);
            }
            if st.ids.arxiv.is_none()
                && let Some((base, fetch)) = ids_obj
                    .get("arxiv")
                    .and_then(Value::as_str)
                    .and_then(normalize_arxiv_id)
            {
                st.ids.arxiv = Some(base);
                st.ids.arxiv_fetch = Some(fetch);
            }
        }

        if let Some(oa) = json.get("open_access").and_then(Value::as_object) {
            if let Some(status) = oa.get("oa_status").and_then(Value::as_str) {
                st.record_oa_status(status);
            }
            if let Some(is_oa) = oa.get("is_oa").and_then(Value::as_bool) {
                st.record_is_oa(is_oa);
            }
        }

        st.set_landing_if_empty(
            json.pointer("/primary_location/landing_page_url")
                .and_then(Value::as_str),
        );

        let mut pdf_candidates: Vec<(String, Option<String>)> = Vec::new();
        let mut html_candidates: Vec<(String, Option<String>)> = Vec::new();

        let mut locs: Vec<&Value> = Vec::new();
        if let Some(best) = json.get("best_oa_location").filter(|v| v.is_object()) {
            locs.push(best);
        }
        if let Some(arr) = json.get("locations").and_then(Value::as_array) {
            for loc in arr {
                if loc.get("is_oa").and_then(Value::as_bool) == Some(true) {
                    locs.push(loc);
                }
            }
        }

        for loc in locs {
            let lic = loc
                .get("license")
                .and_then(Value::as_str)
                .and_then(normalize_license);
            st.set_license_if_empty(lic.as_deref());
            if let Some(pdf) = loc.get("pdf_url").and_then(Value::as_str)
                && is_lawful_candidate_url(pdf)
            {
                self.discover_ids_from_url(st, pdf);
                pdf_candidates.push((pdf.to_owned(), lic.clone()));
            }
            if loc.get("is_oa").and_then(Value::as_bool) == Some(true)
                && let Some(lp) = loc.get("landing_page_url").and_then(Value::as_str)
                && is_lawful_candidate_url(lp)
            {
                self.discover_ids_from_url(st, lp);
                html_candidates.push((lp.to_owned(), lic));
            }
        }

        if let Some(oa_url) = json
            .pointer("/open_access/oa_url")
            .and_then(Value::as_str)
            .filter(|u| is_lawful_candidate_url(u))
        {
            if oa_url.to_ascii_lowercase().contains(".pdf") {
                pdf_candidates.push((oa_url.to_owned(), st.license.clone()));
            } else {
                html_candidates.push((oa_url.to_owned(), st.license.clone()));
            }
        }

        // If OpenAlex revealed a PMCID or arXiv ID, try structured routes first!
        if let Some(hit) = self.try_newly_discovered_structured(st, cx).await? {
            return Ok(Some(hit));
        }

        let oa_status = st.effective_oa_status();
        for (pdf, lic) in pdf_candidates {
            if !st.check_step_budget(cx)? {
                return Ok(None);
            }
            if let Some(hit) = self
                .try_pdf_candidate(st, &pdf, "openalex", lic, oa_status.as_deref(), cx)
                .await?
            {
                return Ok(Some(hit));
            }
        }
        for (page_url, lic) in html_candidates {
            if !st.check_step_budget(cx)? {
                return Ok(None);
            }
            if let Some(hit) = self
                .try_oa_html_candidate(st, &page_url, "openalex", lic, oa_status.as_deref(), cx)
                .await?
            {
                return Ok(Some(hit));
            }
        }

        Ok(None)
    }

    async fn process_s2_response(
        &self,
        resp: &HttpBytes,
        st: &mut ChainState,
        cx: &CallCx,
    ) -> Result<Option<ResolvedHit>, FsmError> {
        let Ok(json) = serde_json::from_slice::<Value>(&resp.body) else {
            return Ok(None);
        };

        // I5: S2's `url` is `semanticscholar.org/paper/<sha>` — store ONLY as fallback landing URL.
        st.set_fallback_landing_if_empty(json.get("url").and_then(Value::as_str));

        // Enrich external IDs (ArXiv, PubMedCentral)
        if let Some(ext) = json.get("externalIds").and_then(Value::as_object) {
            if st.ids.arxiv.is_none()
                && let Some(ax) = ext
                    .get("ArXiv")
                    .and_then(Value::as_str)
                    .and_then(normalize_arxiv_id)
            {
                st.ids.arxiv = Some(ax.0);
                st.ids.arxiv_fetch = Some(ax.1);
            }
            if st.ids.pmcid.is_none()
                && let Some(pmc) = ext
                    .get("PubMedCentral")
                    .and_then(Value::as_str)
                    .map(|s| {
                        if s.starts_with("PMC") {
                            s.to_owned()
                        } else {
                            format!("PMC{s}")
                        }
                    })
                    .and_then(|s| normalize_pmcid(&s))
            {
                st.ids.pmcid = Some(pmc);
            }
        }

        // I7: Try EPMC JATS + PMC BioC + arXiv HTML5 on newly revealed IDs!
        if let Some(hit) = self.try_newly_discovered_structured(st, cx).await? {
            return Ok(Some(hit));
        }

        if let Some(oa_pdf) = json.get("openAccessPdf").and_then(Value::as_object) {
            if let Some(status) = oa_pdf.get("status").and_then(Value::as_str) {
                st.record_oa_status(status);
            }
            let lic = oa_pdf
                .get("license")
                .and_then(Value::as_str)
                .and_then(normalize_license);
            let status_is_closed = oa_pdf
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(|s| s.eq_ignore_ascii_case("closed"));
            if !status_is_closed
                && let Some(pdf_url) = oa_pdf.get("url").and_then(Value::as_str)
                && is_lawful_candidate_url(pdf_url)
            {
                if st.oa_positive.is_none() {
                    st.record_oa_status("green");
                }
                let oa_status = st.effective_oa_status();
                if let Some(hit) = self
                    .try_pdf_candidate(
                        st,
                        pdf_url,
                        "semanticscholar",
                        lic,
                        oa_status.as_deref(),
                        cx,
                    )
                    .await?
                {
                    return Ok(Some(hit));
                }
            }
        } else if let Some(is_oa) = json.get("isOpenAccess").and_then(Value::as_bool) {
            st.record_is_oa(is_oa);
        }

        Ok(None)
    }

    /// Try Europe PMC JATS, PMC BioC, and arXiv HTML5 when a downstream metadata step
    /// reveals a new PMCID or arXiv ID (I7).
    async fn try_newly_discovered_structured(
        &self,
        st: &mut ChainState,
        cx: &CallCx,
    ) -> Result<Option<ResolvedHit>, FsmError> {
        if !st.opts.prefer_structured {
            return Ok(None);
        }
        if st.ids.pmcid.is_some() {
            if let Some(hit) = self.step2_europepmc_jats(st, cx).await? {
                return Ok(Some(hit));
            }
            if let Some(hit) = self.step3_pmc_bioc(st, cx).await? {
                return Ok(Some(hit));
            }
        }
        if st.ids.arxiv.is_some()
            && let Some(hit) = self.step5_arxiv_html5(st, cx).await?
        {
            return Ok(Some(hit));
        }
        Ok(None)
    }

    // -----------------------------------------------------------------------
    // Steps 9 + 10a–10e: Concurrent metadata fetch, strict spec-order evaluation (I6)
    // -----------------------------------------------------------------------
    async fn step9_and_10_repos(
        &self,
        st: &mut ChainState,
        cx: &CallCx,
    ) -> Result<Option<ResolvedHit>, FsmError> {
        let core_benched = self.is_core_benched().await;
        let core_req = if !core_benched {
            self.build_core_request(&st.ids)
        } else {
            None
        };
        let doaj_req = self.build_doaj_request(&st.ids);
        let crossref_req = self.build_crossref_request(&st.ids);
        let datacite_req = self.build_datacite_request(&st.ids);
        let hal_req = self.build_hal_request(&st.ids);
        let zenodo_req = self.build_zenodo_request(&st.ids);

        let reserve = st.budget_reserve;
        let started = st.started;
        let (core_res, doaj_res, crossref_res, datacite_res, hal_res, zenodo_res) = tokio::join!(
            async {
                match core_req {
                    Some(req) => Some(self.send_raw(req, cx, reserve, started).await),
                    None => None,
                }
            },
            async {
                match doaj_req {
                    Some(req) => Some(self.send_raw(req, cx, reserve, started).await),
                    None => None,
                }
            },
            async {
                match crossref_req {
                    Some(req) => Some(self.send_raw(req, cx, reserve, started).await),
                    None => None,
                }
            },
            async {
                match datacite_req {
                    Some(req) => Some(self.send_raw(req, cx, reserve, started).await),
                    None => None,
                }
            },
            async {
                match hal_req {
                    Some(req) => Some(self.send_raw(req, cx, reserve, started).await),
                    None => None,
                }
            },
            async {
                match zenodo_req {
                    Some(req) => Some(self.send_raw(req, cx, reserve, started).await),
                    None => None,
                }
            },
        );

        let core_outcome = core_res.transpose()?;
        let doaj_outcome = doaj_res.transpose()?;
        let crossref_outcome = crossref_res.transpose()?;
        let datacite_outcome = datacite_res.transpose()?;
        let hal_outcome = hal_res.transpose()?;
        let zenodo_outcome = zenodo_res.transpose()?;

        // Record stats and CORE 429 bench immediately (M3, M4).
        if let Some(ref o) = core_outcome {
            st.record_outcome_stats(o);
            if matches!(o, HttpOutcome::RateLimited) {
                self.bench_core(st).await;
            }
        }
        for outcome in [
            &doaj_outcome,
            &crossref_outcome,
            &datacite_outcome,
            &hal_outcome,
            &zenodo_outcome,
        ]
        .into_iter()
        .flatten()
        {
            st.record_outcome_stats(outcome);
        }

        // Step 9: CORE v3
        if core_benched && self.build_core_request(&st.ids).is_some() {
            st.notes
                .push("CORE v3 skipped: temporarily benched after HTTP 429".to_owned());
            st.steps_tried.push("step9:core_v3:benched".to_owned());
        } else if let Some(outcome) = core_outcome {
            st.steps_tried.push("step9:core_v3".to_owned());
            if let HttpOutcome::Ok(resp) = outcome
                && let Some(hit) = self.process_core_response(&resp, st, cx).await?
            {
                return Ok(Some(hit));
            }
        }

        // Step 10a: DOAJ
        if !st.check_step_budget(cx)? {
            return Ok(None);
        }
        if let Some(outcome) = doaj_outcome {
            st.steps_tried.push("step10a:doaj".to_owned());
            if let HttpOutcome::Ok(resp) = outcome
                && let Some(hit) = self.process_doaj_response(&resp, st, cx).await?
            {
                return Ok(Some(hit));
            }
        }

        // Step 10b: Crossref TDM
        if !st.check_step_budget(cx)? {
            return Ok(None);
        }
        if let Some(outcome) = crossref_outcome {
            st.steps_tried.push("step10b:crossref_tdm".to_owned());
            if let HttpOutcome::Ok(resp) = outcome
                && let Some(hit) = self.process_crossref_response(&resp, st, cx).await?
            {
                return Ok(Some(hit));
            }
        }

        // Step 10c: DataCite
        if !st.check_step_budget(cx)? {
            return Ok(None);
        }
        let zenodo_id_before_datacite = st.ids.zenodo.clone();
        if let Some(outcome) = datacite_outcome {
            st.steps_tried.push("step10c:datacite".to_owned());
            if let HttpOutcome::Ok(resp) = outcome
                && let Some(hit) = self.process_datacite_response(&resp, st, cx).await?
            {
                return Ok(Some(hit));
            }
        }

        // Step 10d: HAL
        if !st.check_step_budget(cx)? {
            return Ok(None);
        }
        if let Some(outcome) = hal_outcome {
            st.steps_tried.push("step10d:hal".to_owned());
            if let HttpOutcome::Ok(resp) = outcome
                && let Some(hit) = self.process_hal_response(&resp, st, cx).await?
            {
                return Ok(Some(hit));
            }
        }

        // Step 10e: Zenodo
        if !st.check_step_budget(cx)? {
            return Ok(None);
        }
        if let Some(outcome) = zenodo_outcome {
            st.steps_tried.push("step10e:zenodo".to_owned());
            if let HttpOutcome::Ok(resp) = outcome
                && let Some(hit) = self.process_zenodo_response(&resp, st, cx).await?
            {
                return Ok(Some(hit));
            }
        } else if zenodo_id_before_datacite.is_none()
            && st.ids.zenodo.is_some()
            && let Some(req) = self.build_zenodo_request(&st.ids)
        {
            // DataCite newly discovered a Zenodo ID; fetch that record now.
            st.steps_tried.push("step10e:zenodo".to_owned());
            if let Some(resp) = self.send_ok(req, st, cx).await?
                && let Some(hit) = self.process_zenodo_response(&resp, st, cx).await?
            {
                return Ok(Some(hit));
            }
        }

        Ok(None)
    }

    fn build_core_request(&self, ids: &ParsedPaperId) -> Option<HttpRequest> {
        let query = if let Some(doi) = ids.non_arxiv_doi() {
            format!("doi:\"{doi}\"")
        } else {
            let ax = ids.arxiv.as_deref()?;
            format!("arxivId:\"{ax}\"")
        };

        let mut req = HttpRequest::get("https://api.core.ac.uk/v3/search/works")
            .query_param("q", query)
            .query_param("limit", "3")
            .header("Accept", "application/json")
            .for_source("core_ac")
            .with_max_bytes(4 * 1024 * 1024);

        if let Some(key) = self.deps.cfg.api_key("SEARCH_MCP_CORE_API_KEY") {
            req = req
                .header("Authorization", format!("Bearer {key}"))
                .redact_header("authorization");
        }
        Some(req)
    }

    async fn process_core_response(
        &self,
        resp: &HttpBytes,
        st: &mut ChainState,
        cx: &CallCx,
    ) -> Result<Option<ResolvedHit>, FsmError> {
        let Ok(json) = serde_json::from_slice::<Value>(&resp.body) else {
            return Ok(None);
        };
        let Some(results) = json.get("results").and_then(Value::as_array) else {
            return Ok(None);
        };
        if results.is_empty() {
            return Ok(None);
        }

        // Prefer an item whose DOI matches ours, else first result if single hit.
        let chosen = if let Some(target_doi) = st.ids.non_arxiv_doi() {
            results
                .iter()
                .find(|r| {
                    r.get("doi")
                        .and_then(Value::as_str)
                        .and_then(normalize_doi)
                        .as_deref()
                        == Some(target_doi)
                })
                .or_else(|| {
                    if results.len() == 1 {
                        results.first()
                    } else {
                        None
                    }
                })
        } else {
            results.first()
        };
        let Some(work) = chosen else {
            return Ok(None);
        };

        let display_url = work
            .get("links")
            .and_then(Value::as_array)
            .and_then(|links| {
                links
                    .iter()
                    .find(|l| l.get("type").and_then(Value::as_str) == Some("display"))
                    .and_then(|l| l.get("url").and_then(Value::as_str))
            })
            .map(str::to_owned)
            .or_else(|| {
                work.get("id")
                    .and_then(|v| {
                        v.as_u64()
                            .map(|n| n.to_string())
                            .or_else(|| v.as_str().map(str::to_owned))
                    })
                    .map(|id| format!("https://core.ac.uk/works/{id}"))
            });
        // I5: CORE display page is an aggregator page — store ONLY as fallback landing URL.
        st.set_fallback_landing_if_empty(display_url.as_deref());

        let title = work
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);

        // 9a: Inline `fullText` string (must reject "Not available for public API users.")
        if let Some(ft) = work.get("fullText").and_then(Value::as_str) {
            let trimmed = ft.trim();
            if trimmed.chars().count() >= 200 && !looks_like_paywall_or_stub(trimmed) {
                let doc = extract_text_sections_blocking(
                    trimmed.to_owned(),
                    title.clone(),
                    st.license.clone(),
                )
                .await?;
                if !doc.is_empty() {
                    return Ok(Some(ResolvedHit::complete(
                        "text",
                        "core_v3",
                        st.license.clone(),
                        display_url
                            .clone()
                            .unwrap_or_else(|| "https://api.core.ac.uk/v3/search/works".to_owned()),
                        Some("green".to_owned()),
                        st.default_landing_url(),
                        doc,
                    )));
                }
            }
        }

        // 9b: Repository PDF download URLs
        let mut pdfs: Vec<String> = Vec::new();
        if let Some(dl) = work.get("downloadUrl").and_then(Value::as_str)
            && is_lawful_candidate_url(dl)
        {
            pdfs.push(dl.to_owned());
        }
        if let Some(arr) = work.get("sourceFulltextUrls").and_then(Value::as_array) {
            for u in arr.iter().filter_map(Value::as_str) {
                if is_lawful_candidate_url(u) && !pdfs.iter().any(|p| p == u) {
                    pdfs.push(u.to_owned());
                }
            }
        }
        if let Some(links) = work.get("links").and_then(Value::as_array) {
            for l in links {
                if l.get("type").and_then(Value::as_str) == Some("download")
                    && let Some(u) = l.get("url").and_then(Value::as_str)
                    && is_lawful_candidate_url(u)
                    && !pdfs.iter().any(|p| p == u)
                {
                    pdfs.push(u.to_owned());
                }
            }
        }

        for pdf in pdfs {
            if !st.check_step_budget(cx)? {
                return Ok(None);
            }
            if let Some(hit) = self
                .try_pdf_candidate(st, &pdf, "core_v3", st.license.clone(), Some("green"), cx)
                .await?
            {
                return Ok(Some(hit));
            }
        }

        Ok(None)
    }

    fn build_doaj_request(&self, ids: &ParsedPaperId) -> Option<HttpRequest> {
        let doi = ids.non_arxiv_doi()?;
        let seg = utf8_percent_encode(doi, SEGMENT_ENCODE).to_string();
        let url = format!("https://doaj.org/api/search/articles/doi:{seg}");
        Some(
            HttpRequest::get(&url)
                .header("Accept", "application/json")
                .for_source("doaj")
                .with_max_bytes(512 * 1024),
        )
    }

    async fn process_doaj_response(
        &self,
        resp: &HttpBytes,
        st: &mut ChainState,
        cx: &CallCx,
    ) -> Result<Option<ResolvedHit>, FsmError> {
        let Ok(json) = serde_json::from_slice::<Value>(&resp.body) else {
            return Ok(None);
        };
        let Some(bib) = json
            .get("results")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|r| r.get("bibjson"))
        else {
            return Ok(None);
        };

        let lic = bib
            .pointer("/journal/license/0/type")
            .and_then(Value::as_str)
            .and_then(normalize_license)
            .or_else(|| st.license.clone());
        st.set_license_if_empty(lic.as_deref());
        st.record_oa_status("gold");

        let mut xml_urls = Vec::new();
        let mut pdf_urls = Vec::new();
        let mut html_urls = Vec::new();

        if let Some(links) = bib.get("link").and_then(Value::as_array) {
            for l in links {
                if l.get("type").and_then(Value::as_str) != Some("fulltext") {
                    continue;
                }
                let Some(u) = l.get("url").and_then(Value::as_str) else {
                    continue;
                };
                if !is_lawful_candidate_url(u) {
                    continue;
                }
                st.set_landing_if_empty(Some(u));
                self.discover_ids_from_url(st, u);
                let ct = l
                    .get("content_type")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let u_lower = u.to_ascii_lowercase();
                if ct.contains("xml") || u_lower.ends_with(".xml") {
                    xml_urls.push(u.to_owned());
                } else if ct.contains("pdf")
                    || u_lower.ends_with(".pdf")
                    || u_lower.contains("pdf=render")
                {
                    pdf_urls.push(u.to_owned());
                } else {
                    html_urls.push(u.to_owned());
                }
            }
        }

        if st.opts.prefer_structured {
            for u in xml_urls {
                if !st.check_step_budget(cx)? {
                    return Ok(None);
                }
                if let Some(hit) = self
                    .try_jats_url(
                        st,
                        &u,
                        "doaj",
                        lic.clone(),
                        Some("gold"),
                        st.default_landing_url(),
                        cx,
                    )
                    .await?
                {
                    return Ok(Some(hit));
                }
            }
        }
        for u in pdf_urls {
            if !st.check_step_budget(cx)? {
                return Ok(None);
            }
            if let Some(hit) = self
                .try_pdf_candidate(st, &u, "doaj", lic.clone(), Some("gold"), cx)
                .await?
            {
                return Ok(Some(hit));
            }
        }
        for u in html_urls {
            if !st.check_step_budget(cx)? {
                return Ok(None);
            }
            if let Some(hit) = self
                .try_oa_html_candidate(st, &u, "doaj", lic.clone(), Some("gold"), cx)
                .await?
            {
                return Ok(Some(hit));
            }
        }
        Ok(None)
    }

    fn build_crossref_request(&self, ids: &ParsedPaperId) -> Option<HttpRequest> {
        let doi = ids.non_arxiv_doi()?;
        let url = format!("https://api.crossref.org/works/{}", encode_doi_path(doi));
        let mut req = HttpRequest::get(&url)
            .header("Accept", "application/json")
            .for_source("crossref")
            .with_max_bytes(CROSSREF_MAX_BYTES);
        if let Some(email) = valid_contact_email(&self.deps.cfg.contact_email) {
            req = req.query_param("mailto", email).redact_query_key("mailto");
        }
        Some(req)
    }

    async fn process_crossref_response(
        &self,
        resp: &HttpBytes,
        st: &mut ChainState,
        cx: &CallCx,
    ) -> Result<Option<ResolvedHit>, FsmError> {
        let Ok(json) = serde_json::from_slice::<Value>(&resp.body) else {
            return Ok(None);
        };
        let Some(msg) = json.get("message") else {
            return Ok(None);
        };

        st.set_landing_if_empty(msg.get("URL").and_then(Value::as_str));
        let lic = msg
            .pointer("/license/0/URL")
            .and_then(Value::as_str)
            .and_then(normalize_license)
            .or_else(|| st.license.clone());
        st.set_license_if_empty(lic.as_deref());

        let Some(links) = msg.get("link").and_then(Value::as_array) else {
            return Ok(None);
        };

        let mut xml_urls = Vec::new();
        let mut pdf_urls = Vec::new();
        for l in links {
            // M6: Keep only `text-mining` and `unspecified` (or omitted) links; skip `similarity-checking`.
            if let Some(app) = l.get("intended-application").and_then(Value::as_str) {
                let app_norm = app.trim().to_ascii_lowercase();
                if !app_norm.is_empty() && app_norm != "text-mining" && app_norm != "unspecified" {
                    continue;
                }
            }
            let Some(u) = l.get("URL").and_then(Value::as_str) else {
                continue;
            };
            if !is_lawful_candidate_url(u) || is_subscriber_tdm_host(u) {
                continue;
            }
            let ct = l
                .get("content-type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_ascii_lowercase();
            if ct.contains("xml") {
                xml_urls.push(u.to_owned());
            } else if ct.contains("pdf") {
                pdf_urls.push(u.to_owned());
            }
        }

        let oa_status = st.effective_oa_status();
        if st.opts.prefer_structured {
            for u in xml_urls {
                if !st.check_step_budget(cx)? {
                    return Ok(None);
                }
                let landing = st.default_landing_url();
                if let Some(hit) = self
                    .try_jats_url(
                        st,
                        &u,
                        "crossref_tdm",
                        lic.clone(),
                        oa_status.as_deref(),
                        landing,
                        cx,
                    )
                    .await?
                {
                    return Ok(Some(hit));
                }
            }
        }
        for u in pdf_urls {
            if !st.check_step_budget(cx)? {
                return Ok(None);
            }
            if let Some(hit) = self
                .try_pdf_candidate(
                    st,
                    &u,
                    "crossref_tdm",
                    lic.clone(),
                    oa_status.as_deref(),
                    cx,
                )
                .await?
            {
                return Ok(Some(hit));
            }
        }

        Ok(None)
    }

    fn build_datacite_request(&self, ids: &ParsedPaperId) -> Option<HttpRequest> {
        let doi = ids.doi.as_deref()?;
        let url = format!("https://api.datacite.org/dois/{}", encode_doi_path(doi));
        Some(
            HttpRequest::get(&url)
                .header("Accept", "application/vnd.api+json")
                .for_source("datacite")
                .with_max_bytes(1024 * 1024),
        )
    }

    async fn process_datacite_response(
        &self,
        resp: &HttpBytes,
        st: &mut ChainState,
        cx: &CallCx,
    ) -> Result<Option<ResolvedHit>, FsmError> {
        let Ok(json) = serde_json::from_slice::<Value>(&resp.body) else {
            return Ok(None);
        };
        let Some(attrs) = json.pointer("/data/attributes") else {
            return Ok(None);
        };

        if let Some(landing) = attrs.get("url").and_then(Value::as_str) {
            st.set_landing_if_empty(Some(landing));
            self.discover_ids_from_url(st, landing);
        }
        let lic = attrs
            .pointer("/rightsList/0/rightsIdentifier")
            .or_else(|| attrs.pointer("/rightsList/0/rightsUri"))
            .and_then(Value::as_str)
            .and_then(normalize_license)
            .or_else(|| st.license.clone());
        st.set_license_if_empty(lic.as_deref());

        if let Some(content_urls) = attrs.get("contentUrl").and_then(Value::as_array) {
            for u in content_urls.iter().filter_map(Value::as_str) {
                if !is_lawful_candidate_url(u) {
                    continue;
                }
                if !st.check_step_budget(cx)? {
                    return Ok(None);
                }
                let lower = u.to_ascii_lowercase();
                if lower.ends_with(".xml") && st.opts.prefer_structured {
                    if let Some(hit) = self
                        .try_jats_url(
                            st,
                            u,
                            "datacite",
                            lic.clone(),
                            Some("green"),
                            st.default_landing_url(),
                            cx,
                        )
                        .await?
                    {
                        return Ok(Some(hit));
                    }
                } else if lower.ends_with(".pdf")
                    && let Some(hit) = self
                        .try_pdf_candidate(st, u, "datacite", lic.clone(), Some("green"), cx)
                        .await?
                {
                    return Ok(Some(hit));
                }
            }
        }
        Ok(None)
    }

    fn build_hal_request(&self, ids: &ParsedPaperId) -> Option<HttpRequest> {
        let query = if let Some(ref hal) = ids.hal {
            format!("halId_s:{hal}")
        } else {
            let doi = ids.non_arxiv_doi()?;
            format!("doiId_s:\"{doi}\"")
        };

        Some(
            HttpRequest::get("https://api.archives-ouvertes.fr/search/")
                .query_param("q", query)
                .query_param("fl", "halId_s,uri_s,fileMain_s,openAccess_bool,doiId_s")
                .query_param("rows", "1")
                .query_param("wt", "json")
                .header("Accept", "application/json")
                .for_source("hal")
                .with_max_bytes(512 * 1024),
        )
    }

    async fn process_hal_response(
        &self,
        resp: &HttpBytes,
        st: &mut ChainState,
        cx: &CallCx,
    ) -> Result<Option<ResolvedHit>, FsmError> {
        let Ok(json) = serde_json::from_slice::<Value>(&resp.body) else {
            return Ok(None);
        };
        let Some(doc) = json.pointer("/response/docs/0").filter(|v| v.is_object()) else {
            return Ok(None);
        };

        st.set_landing_if_empty(doc.get("uri_s").and_then(Value::as_str));
        if let Some(file_url) = doc.get("fileMain_s").and_then(Value::as_str)
            && is_lawful_candidate_url(file_url)
        {
            st.record_oa_status("green");
            if let Some(hit) = self
                .try_pdf_candidate(st, file_url, "hal", st.license.clone(), Some("green"), cx)
                .await?
            {
                return Ok(Some(hit));
            }
        }
        Ok(None)
    }

    fn build_zenodo_request(&self, ids: &ParsedPaperId) -> Option<HttpRequest> {
        if let Some(ref zid) = ids.zenodo {
            Some(
                HttpRequest::get(format!("https://zenodo.org/api/records/{zid}"))
                    .header("Accept", "application/json")
                    .for_source("zenodo")
                    .with_max_bytes(1024 * 1024),
            )
        } else {
            ids.non_arxiv_doi().map(|doi| {
                HttpRequest::get("https://zenodo.org/api/records")
                    .query_param("q", format!("doi:\"{doi}\""))
                    .query_param("size", "1")
                    .header("Accept", "application/json")
                    .for_source("zenodo")
                    .with_max_bytes(1024 * 1024)
            })
        }
    }

    async fn process_zenodo_response(
        &self,
        resp: &HttpBytes,
        st: &mut ChainState,
        cx: &CallCx,
    ) -> Result<Option<ResolvedHit>, FsmError> {
        let Ok(json) = serde_json::from_slice::<Value>(&resp.body) else {
            return Ok(None);
        };

        // Single record (`/records/{id}`) vs search result (`/records?q=...`)
        let record = if json.get("files").is_some() {
            Some(&json)
        } else {
            json.pointer("/hits/hits/0")
        };
        let Some(rec) = record.filter(|v| v.is_object()) else {
            return Ok(None);
        };

        st.set_landing_if_empty(
            rec.pointer("/links/self_html")
                .or_else(|| rec.pointer("/links/html"))
                .and_then(Value::as_str),
        );
        let lic = rec
            .pointer("/metadata/license/id")
            .and_then(Value::as_str)
            .and_then(normalize_license)
            .or_else(|| st.license.clone());
        st.set_license_if_empty(lic.as_deref());

        let Some(files) = rec.get("files").and_then(Value::as_array) else {
            return Ok(None);
        };
        let mut xml_urls = Vec::new();
        let mut pdf_urls = Vec::new();
        let mut text_urls = Vec::new();

        for f in files {
            let key = f
                .get("key")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_ascii_lowercase();
            let link = f
                .pointer("/links/self")
                .or_else(|| f.pointer("/links/content"))
                .or_else(|| f.pointer("/links/download"))
                .and_then(Value::as_str);
            let Some(u) = link.filter(|u| is_lawful_candidate_url(u)) else {
                continue;
            };
            if key.ends_with(".xml") {
                xml_urls.push(u.to_owned());
            } else if key.ends_with(".pdf") {
                pdf_urls.push(u.to_owned());
            } else if key.ends_with(".txt") || key.ends_with(".md") {
                text_urls.push(u.to_owned());
            }
        }

        if st.opts.prefer_structured {
            for u in xml_urls {
                if !st.check_step_budget(cx)? {
                    return Ok(None);
                }
                if let Some(hit) = self
                    .try_jats_url(
                        st,
                        &u,
                        "zenodo",
                        lic.clone(),
                        Some("green"),
                        st.default_landing_url(),
                        cx,
                    )
                    .await?
                {
                    return Ok(Some(hit));
                }
            }
        }
        for u in pdf_urls {
            if !st.check_step_budget(cx)? {
                return Ok(None);
            }
            if let Some(hit) = self
                .try_pdf_candidate(st, &u, "zenodo", lic.clone(), Some("green"), cx)
                .await?
            {
                return Ok(Some(hit));
            }
        }
        for u in text_urls {
            if !st.check_step_budget(cx)? {
                return Ok(None);
            }
            let req = HttpRequest::get(&u)
                .header("Accept", "text/plain")
                .for_source("zenodo")
                .with_max_bytes(MAX_FULLTEXT_BYTES);
            if let Some(r) = self.send_ok(req, st, cx).await? {
                let text = decode_response_text(&r);
                if text.trim().chars().count() >= 100 && !looks_like_paywall_or_stub(&text) {
                    let doc = extract_text_sections_blocking(text, None, lic.clone()).await?;
                    if !doc.is_empty() {
                        return Ok(Some(ResolvedHit::complete(
                            "text",
                            "zenodo",
                            lic,
                            u,
                            Some("green".to_owned()),
                            st.default_landing_url(),
                            doc,
                        )));
                    }
                }
            }
        }

        Ok(None)
    }

    // -----------------------------------------------------------------------
    // Step 10f: FAIR Signposting HEAD on real landing URL (never fallback S2/CORE — I5)
    // -----------------------------------------------------------------------
    async fn step10f_signposting(
        &self,
        st: &mut ChainState,
        cx: &CallCx,
    ) -> Result<Option<ResolvedHit>, FsmError> {
        if st.is_confirmed_closed() {
            return Ok(None);
        }
        let Some(landing) = st.landing_url.clone() else {
            return Ok(None);
        };
        // PLOS presigned R2 URLs reject HEAD with 403 (`academic.md` L457).
        if !is_lawful_candidate_url(&landing) || landing.contains("journals.plos.org") {
            return Ok(None);
        }

        st.steps_tried.push("step10f:signposting".to_owned());
        let req = HttpRequest::head(&landing)
            .for_source("signposting")
            .with_max_bytes(64 * 1024);

        let Some(resp) = self.send_ok(req, st, cx).await? else {
            return Ok(None);
        };

        let mut xml_items = Vec::new();
        let mut pdf_items = Vec::new();

        for (k, v) in &resp.headers {
            if !k.eq_ignore_ascii_case("link") {
                continue;
            }
            for item in parse_link_header(v) {
                if item.rel.eq_ignore_ascii_case("license") {
                    st.set_license_if_empty(Some(&item.url));
                    continue;
                }
                if !item.rel.eq_ignore_ascii_case("item") {
                    continue;
                }
                if !is_lawful_candidate_url(&item.url) {
                    continue;
                }
                let ty = item.media_type.to_ascii_lowercase();
                if ty.contains("xml") || item.url.to_ascii_lowercase().ends_with(".xml") {
                    xml_items.push(item.url);
                } else if ty.contains("pdf") || item.url.to_ascii_lowercase().ends_with(".pdf") {
                    pdf_items.push(item.url);
                }
            }
        }

        if st.opts.prefer_structured {
            for u in xml_items {
                if !st.check_step_budget(cx)? {
                    return Ok(None);
                }
                if let Some(hit) = self
                    .try_jats_url(
                        st,
                        &u,
                        "signposting",
                        st.license.clone(),
                        Some("green"),
                        Some(landing.clone()),
                        cx,
                    )
                    .await?
                {
                    return Ok(Some(hit));
                }
            }
        }
        for u in pdf_items {
            if !st.check_step_budget(cx)? {
                return Ok(None);
            }
            if let Some(hit) = self
                .try_pdf_candidate(st, &u, "signposting", st.license.clone(), Some("green"), cx)
                .await?
            {
                return Ok(Some(hit));
            }
        }

        Ok(None)
    }

    // -----------------------------------------------------------------------
    // Step 11: Wayback CDX (last-resort fallback for link-rotted OA URLs — I3, M9)
    // -----------------------------------------------------------------------
    async fn step11_wayback_cdx(
        &self,
        st: &mut ChainState,
        cx: &CallCx,
    ) -> Result<Option<ResolvedHit>, FsmError> {
        // I3: Only archive failed OA URLs. Never archive a bare landing page unless
        // some source positively reported open-access status.
        let mut targets: Vec<String> = st.failed_oa_urls.iter().take(2).cloned().collect();
        if targets.is_empty()
            && st.oa_positive.is_some()
            && let Some(ref lp) = st.landing_url
            && is_lawful_candidate_url(lp)
        {
            targets.push(lp.clone());
        }
        if targets.is_empty() {
            return Ok(None);
        }

        st.steps_tried.push("step11:wayback_cdx".to_owned());

        for target in targets {
            if !st.check_step_budget(cx)? {
                return Ok(None);
            }
            // M9: `limit=-1` returns the *latest* HTTP 200 capture rather than the oldest.
            let req = HttpRequest::get("https://web.archive.org/cdx/search/cdx")
                .query_param("url", &target)
                .query_param("output", "json")
                .query_param("fl", "timestamp,original,mimetype,statuscode")
                .query_param("filter", "statuscode:200")
                .query_param("limit", "-1")
                .header("Accept", "application/json")
                .for_source("wayback")
                .with_max_bytes(256 * 1024);

            let Some(resp) = self.send_ok(req, st, cx).await? else {
                continue;
            };
            let Ok(json) = serde_json::from_slice::<Value>(&resp.body) else {
                continue;
            };
            let Some(rows) = json.as_array() else {
                continue;
            };
            if rows.len() < 2 {
                continue;
            }
            let Some(row) = rows[1].as_array() else {
                continue;
            };
            let timestamp = row.first().and_then(Value::as_str).unwrap_or("");
            let original = row.get(1).and_then(Value::as_str).unwrap_or("");
            let mimetype = row
                .get(2)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_ascii_lowercase();
            if timestamp.is_empty() || original.is_empty() || !is_lawful_candidate_url(original) {
                continue;
            }

            let archive_url = format!("https://web.archive.org/web/{timestamp}id_/{original}");
            if mimetype.contains("pdf") || original.to_ascii_lowercase().ends_with(".pdf") {
                if let Some(hit) = self
                    .try_pdf_candidate(
                        st,
                        &archive_url,
                        "wayback",
                        st.license.clone(),
                        Some("green"),
                        cx,
                    )
                    .await?
                {
                    return Ok(Some(hit));
                }
            } else if mimetype.contains("xml") {
                if let Some(hit) = self
                    .try_jats_url(
                        st,
                        &archive_url,
                        "wayback",
                        st.license.clone(),
                        Some("green"),
                        st.default_landing_url(),
                        cx,
                    )
                    .await?
                {
                    return Ok(Some(hit));
                }
            } else if (mimetype.contains("html") || mimetype.contains("text"))
                && let Some(hit) = self
                    .try_oa_html_candidate(
                        st,
                        &archive_url,
                        "wayback",
                        st.license.clone(),
                        Some("green"),
                        cx,
                    )
                    .await?
            {
                return Ok(Some(hit));
            }
        }

        Ok(None)
    }

    // -----------------------------------------------------------------------
    // Candidate fetchers & shared helpers
    // -----------------------------------------------------------------------
    #[allow(clippy::too_many_arguments)]
    async fn try_jats_url(
        &self,
        st: &mut ChainState,
        url: &str,
        source: &str,
        license: Option<String>,
        oa_status: Option<&str>,
        landing_url: Option<String>,
        cx: &CallCx,
    ) -> Result<Option<ResolvedHit>, FsmError> {
        if !is_lawful_candidate_url(url) || !st.tried_urls.insert(url.to_owned()) {
            return Ok(None);
        }
        let req = HttpRequest::get(url)
            .header("Accept", "application/xml")
            .for_source(source)
            .with_max_bytes(MAX_FULLTEXT_BYTES);
        let Some(resp) = self.send_ok(req, st, cx).await? else {
            st.record_failed_oa_url(url);
            return Ok(None);
        };
        let xml = decode_response_text(&resp);
        if !xml.contains("<article") {
            st.record_failed_oa_url(url);
            return Ok(None);
        }
        match parse_jats_blocking(xml, self.deps.cfg.max_document_chars).await {
            Ok(doc) => {
                let lic = doc
                    .license
                    .clone()
                    .or(license)
                    .or_else(|| st.license.clone());
                Ok(Some(ResolvedHit::complete(
                    "jats",
                    source,
                    lic,
                    url.to_owned(),
                    oa_status.map(str::to_owned),
                    landing_url,
                    doc,
                )))
            }
            Err(e) => {
                debug!("JATS parse failed for {url}: {e}");
                st.record_failed_oa_url(url);
                Ok(None)
            }
        }
    }

    async fn try_pdf_candidate(
        &self,
        st: &mut ChainState,
        pdf_url: &str,
        source: &str,
        license: Option<String>,
        oa_status: Option<&str>,
        cx: &CallCx,
    ) -> Result<Option<ResolvedHit>, FsmError> {
        if !st.check_step_budget(cx)? {
            return Ok(None);
        }
        if !is_lawful_candidate_url(pdf_url) || !st.tried_urls.insert(pdf_url.to_owned()) {
            return Ok(None);
        }
        if self.deps.net.check_url_offline(pdf_url).is_err() {
            return Ok(None);
        }

        // B1: Always read up to `cfg.max_document_chars` so small `opts.max_chars` preview calls
        // never poison the 30-day cache with a truncated stub; `filter_and_budget` slices afterwards.
        let read_len = Some(self.deps.cfg.max_document_chars);

        match self.docs.read(pdf_url, 0, read_len, cx).await {
            Ok(slice) => {
                st.upstream_ok_count += 1;
                let content = slice.content.trim();
                if content.is_empty() || looks_like_paywall_or_stub(content) {
                    st.record_failed_oa_url(pdf_url);
                    return Ok(None);
                }
                let returned = if slice.returned_chars > 0 {
                    slice.returned_chars
                } else {
                    slice.content.chars().count()
                };
                // B1: Refuse to cache a partial document slice when DocApi truncated below `max_document_chars`.
                let cacheable = !(slice.truncated && returned < self.deps.cfg.max_document_chars);
                let doc = extract_text_sections_blocking(
                    content.to_owned(),
                    slice.title,
                    license.clone().or_else(|| st.license.clone()),
                )
                .await?;
                if doc.is_empty() {
                    st.record_failed_oa_url(pdf_url);
                    return Ok(None);
                }
                // M7: SPEC enum is `jats | bioc | html5 | pdf | text` — map non-PDF slices to `text`.
                let fmt = if slice.format.eq_ignore_ascii_case("pdf") || slice.format.is_empty() {
                    "pdf".to_owned()
                } else {
                    "text".to_owned()
                };
                Ok(Some(ResolvedHit {
                    format: fmt,
                    source: source.to_owned(),
                    license: license.or_else(|| st.license.clone()),
                    url: pdf_url.to_owned(),
                    oa_status: oa_status.map(str::to_owned),
                    landing_url: st.default_landing_url(),
                    truncated: slice.truncated,
                    cacheable,
                    doc,
                }))
            }
            Err(FsmError::Timeout(_)) => {
                // I6: A per-document timeout before the overall call deadline must NOT abort the chain.
                if cx.cancel.is_cancelled() {
                    return Err(FsmError::Timeout(st.elapsed_ms()));
                }
                st.upstream_err_count += 1;
                st.record_failed_oa_url(pdf_url);
                if cx.remaining() <= st.budget_reserve {
                    st.mark_budget_exhausted();
                }
                Ok(None)
            }
            Err(e) => {
                st.upstream_err_count += 1;
                debug!("DocApi::read failed for {pdf_url}: {e}");
                st.record_failed_oa_url(pdf_url);
                Ok(None)
            }
        }
    }

    async fn try_oa_html_candidate(
        &self,
        st: &mut ChainState,
        page_url: &str,
        source: &str,
        license: Option<String>,
        oa_status: Option<&str>,
        cx: &CallCx,
    ) -> Result<Option<ResolvedHit>, FsmError> {
        if !st.check_step_budget(cx)? {
            return Ok(None);
        }
        if !is_lawful_candidate_url(page_url) || !st.tried_urls.insert(page_url.to_owned()) {
            return Ok(None);
        }
        if self.deps.net.check_url_offline(page_url).is_err() {
            return Ok(None);
        }
        match self.fetch.fetch(page_url, FetchOpts::default(), cx).await {
            Ok(FetchOutcome::Page(page)) => {
                st.upstream_ok_count += 1;
                let text = page.content.trim().to_owned();
                if text.chars().count() < 300 || looks_like_paywall_or_stub(&text) {
                    st.record_failed_oa_url(page_url);
                    return Ok(None);
                }
                let doc = if text.contains("<article") || text.contains("<section") {
                    match parse_generic_html5_blocking(text.clone()).await {
                        Ok(d) => d,
                        Err(_) => {
                            extract_text_sections_blocking(
                                text,
                                Some(page.title.clone()),
                                license.clone(),
                            )
                            .await?
                        }
                    }
                } else {
                    let title = if page.title.trim().is_empty() {
                        None
                    } else {
                        Some(page.title.clone())
                    };
                    extract_text_sections_blocking(text, title, license.clone()).await?
                };

                // I3: Require real article structure (not a repository handle / abstract / paywall page).
                let combined_text: String = doc
                    .sections
                    .iter()
                    .map(|s| s.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                if doc.is_empty()
                    || looks_like_paywall_or_stub(&combined_text)
                    || !has_fulltext_article_structure(&doc)
                {
                    st.record_failed_oa_url(page_url);
                    return Ok(None);
                }
                Ok(Some(ResolvedHit::complete(
                    "html5",
                    source,
                    license.or_else(|| st.license.clone()),
                    page.url,
                    oa_status.map(str::to_owned),
                    st.default_landing_url(),
                    doc,
                )))
            }
            Ok(_) => Ok(None),
            Err(FsmError::Timeout(_)) => {
                // I6: A per-page timeout before the overall call deadline must NOT abort the chain.
                if cx.cancel.is_cancelled() {
                    return Err(FsmError::Timeout(st.elapsed_ms()));
                }
                st.upstream_err_count += 1;
                st.record_failed_oa_url(page_url);
                if cx.remaining() <= st.budget_reserve {
                    st.mark_budget_exhausted();
                }
                Ok(None)
            }
            Err(e) => {
                st.upstream_err_count += 1;
                debug!("FetchApi::fetch failed for {page_url}: {e}");
                st.record_failed_oa_url(page_url);
                Ok(None)
            }
        }
    }

    fn discover_ids_from_url(&self, st: &mut ChainState, url: &str) {
        if st.ids.pmcid.is_none()
            && let Some(pmc) = normalize_pmcid(url)
        {
            st.ids.pmcid = Some(pmc);
        }
        if st.ids.arxiv.is_none()
            && let Some((base, fetch)) = normalize_arxiv_id(url)
        {
            st.ids.arxiv = Some(base);
            st.ids.arxiv_fetch = Some(fetch);
        }
        if st.ids.zenodo.is_none()
            && let Ok(parsed) = parse_paper_id(url)
        {
            if let Some(z) = parsed.zenodo {
                st.ids.zenodo = Some(z);
            }
            if st.ids.hal.is_none() {
                st.ids.hal = parsed.hal;
            }
        }
        if st.ids.zenodo.is_none()
            && let Some(ref d) = st.ids.doi
        {
            st.ids.zenodo = extract_zenodo_id_from_doi(d);
        }
    }

    /// Low-level HTTP send safe for `tokio::join!` concurrent batches (no `&mut ChainState`).
    async fn send_raw(
        &self,
        req: HttpRequest,
        cx: &CallCx,
        reserve: Duration,
        started: Instant,
    ) -> Result<HttpOutcome, FsmError> {
        if cx.cancel.is_cancelled() {
            return Err(FsmError::Timeout(elapsed_ms_since(started)));
        }
        let ncx = net_cx_from_call(cx, reserve);
        let timeout = ncx.timeout;
        let sent = tokio::time::timeout(
            timeout,
            self.deps.net.send(req, TransportProfile::Plain, &ncx),
        )
        .await;
        match sent {
            Ok(Ok(resp)) if resp.is_success() => Ok(HttpOutcome::Ok(resp)),
            Ok(Ok(resp)) if resp.status == 429 => Ok(HttpOutcome::RateLimited),
            Ok(Ok(resp)) if resp.status == 404 || resp.status == 410 => Ok(HttpOutcome::NotFound),
            Ok(Ok(_)) => Ok(HttpOutcome::Error),
            Ok(Err(FsmError::RateLimited { .. }))
            | Ok(Err(FsmError::Upstream {
                status: Some(429), ..
            })) => Ok(HttpOutcome::RateLimited),
            Ok(Err(FsmError::Upstream {
                status: Some(404 | 410),
                ..
            })) => Ok(HttpOutcome::NotFound),
            Ok(Err(FsmError::Timeout(_))) | Err(_) => {
                if cx.cancel.is_cancelled() {
                    Err(FsmError::Timeout(elapsed_ms_since(started)))
                } else {
                    Ok(HttpOutcome::Error)
                }
            }
            Ok(Err(_)) => Ok(HttpOutcome::Error),
        }
    }

    /// Send an HTTP request via `NetClient`, updating M4 stats and propagating explicit cancellation.
    async fn send_ok(
        &self,
        req: HttpRequest,
        st: &mut ChainState,
        cx: &CallCx,
    ) -> Result<Option<HttpBytes>, FsmError> {
        let outcome = self
            .send_raw(req, cx, st.budget_reserve, st.started)
            .await?;
        st.record_outcome_stats(&outcome);
        match outcome {
            HttpOutcome::Ok(resp) => Ok(Some(resp)),
            _ => Ok(None),
        }
    }

    /// Share OpenAlex / S2 429 benching bidirectionally with `ScholarCore` (in-memory + `api_cache` — I9).
    async fn bench_scholar_provider(&self, provider: Provider, st: &mut ChainState) {
        let now = now_unix_secs();
        let until = self
            .core
            .note_rate_limited(provider)
            .await
            .unwrap_or_else(|| next_utc_midnight_secs(now));
        let source_id = provider.id();
        st.notes.push(format!(
            "{source_id} returned HTTP 429; benched until UTC midnight"
        ));
        let mut rec = HealthRecord::new(source_id, "benched", now);
        rec.benched_until = Some(until);
        rec.last_error = Some("HTTP 429 rate limited".to_owned());
        let _ = self.deps.store.health_save(&rec).await;
    }

    /// Check if CORE v3 is temporarily benched after an HTTP 429 (M3).
    async fn is_core_benched(&self) -> bool {
        let now = now_unix_secs();
        let cached = self.core_benched_until.load(Ordering::Relaxed);
        if cached > now {
            return true;
        }
        if let Ok(Some((body, _, _))) = self.deps.store.api_cache_get(CORE_BENCH_CACHE_KEY).await
            && let Some(until) = std::str::from_utf8(&body)
                .ok()
                .and_then(|s| s.trim().parse::<i64>().ok())
            && until > now
        {
            self.core_benched_until.store(until, Ordering::Relaxed);
            return true;
        }
        false
    }

    /// Bench CORE v3 for [`CORE_BENCH_TTL_S`] seconds after HTTP 429 (M3).
    async fn bench_core(&self, st: &mut ChainState) {
        let now = now_unix_secs();
        let until = now.saturating_add(i64::try_from(CORE_BENCH_TTL_S).unwrap_or(300));
        self.core_benched_until.store(until, Ordering::Relaxed);
        st.notes
            .push("core_ac returned HTTP 429; temporarily benched".to_owned());
        let _ = self
            .deps
            .store
            .api_cache_put(
                CORE_BENCH_CACHE_KEY,
                "core_ac",
                until.to_string().as_bytes(),
                None,
                None,
                CORE_BENCH_TTL_S,
            )
            .await;
        let mut rec = HealthRecord::new("core_ac", "benched", now);
        rec.benched_until = Some(until);
        rec.last_error = Some("HTTP 429 rate limited".to_owned());
        let _ = self.deps.store.health_save(&rec).await;
    }

    async fn finish_hit(
        &self,
        cache_key: &str,
        hit: ResolvedHit,
        st: ChainState,
    ) -> FulltextReport {
        // B1: Persist full document to `api_cache` only when complete (not a partial slice).
        if hit.cacheable {
            let cached = CachedFulltext::from_parsed(
                &hit.format,
                &hit.source,
                hit.license.clone(),
                hit.url.clone(),
                hit.oa_status.clone(),
                hit.landing_url.clone(),
                hit.truncated,
                &hit.doc,
            );
            if let Ok(bytes) = serde_json::to_vec(&CachedEntry::Positive(cached)) {
                let _ = self
                    .deps
                    .store
                    .api_cache_put(
                        cache_key,
                        &hit.source,
                        &bytes,
                        None,
                        None,
                        FULLTEXT_CACHE_TTL_S,
                    )
                    .await;
            }
        }

        let mut result = self.build_available_result(
            &st.ids.canonical_id,
            &hit.format,
            &hit.source,
            hit.license,
            hit.url,
            hit.oa_status,
            hit.landing_url,
            &hit.doc,
            &st.opts,
        );
        result.notes = st.notes.clone();

        let mut report = FulltextReport::new(result);
        report.notes = st.notes;
        report.steps_tried = st.steps_tried;
        report
    }

    async fn finish_miss(&self, cache_key: &str, st: ChainState) -> FulltextReport {
        let mut res = FulltextResult::new(st.ids.canonical_id.clone(), false);
        res.landing_url = st.default_landing_url();

        let mut should_negative_cache = false;

        if let Some(ref step) = st.budget_exhausted_after {
            // I6: Deadline budget exhausted gracefully — never negative-cache.
            res.why = Some(format!("time budget exhausted after {step}"));
            res.oa_status = st
                .effective_oa_status()
                .or_else(|| Some("unknown".to_owned()));
            if st.oa_positive.is_some() {
                res.url = st.failed_oa_urls.first().cloned();
            }
        } else if let Some(ref oa_status) = st.oa_positive {
            // I4: Positive OA evidence existed, but all candidate fetches failed — NOT paywalled!
            res.why = Some("open-access copy listed but could not be retrieved".to_owned());
            res.oa_status = Some(oa_status.clone());
            res.url = st.failed_oa_urls.first().cloned();
        } else if st.is_confirmed_closed() {
            res.why = Some("paywalled".to_owned());
            res.oa_status = Some("closed".to_owned());
            should_negative_cache = st.upstream_ok_count > 0;
        } else if st.upstream_ok_count == 0 && st.upstream_err_count > 0 {
            // M4: Every upstream failed (network / 5xx / 429) — transient error, do not negative-cache.
            res.why = Some(
                "all upstream lookups failed (network or rate limits); retry later".to_owned(),
            );
            res.oa_status = Some("unknown".to_owned());
        } else {
            res.why = Some(if st.unpaywall_skipped_no_email {
                "no open-access full text found (Unpaywall skipped: set SEARCH_MCP_CONTACT_EMAIL to enable)".to_owned()
            } else {
                "no open-access full text found".to_owned()
            });
            res.oa_status = Some("unknown".to_owned());
            should_negative_cache = st.upstream_ok_count > 0;
        }

        // M3: Cache confirmed-closed / not-found misses for 24 h under the same key with `negative: true`.
        if should_negative_cache {
            let neg = CachedNegative {
                negative: true,
                why: res.why.clone(),
                oa_status: res.oa_status.clone(),
                landing_url: res.landing_url.clone(),
                url: res.url.clone(),
                unpaywall_skipped: st.unpaywall_skipped_no_email,
                notes: st.notes.clone(),
            };
            if let Ok(bytes) = serde_json::to_vec(&CachedEntry::Negative(neg)) {
                let _ = self
                    .deps
                    .store
                    .api_cache_put(
                        cache_key,
                        "fulltext_negative",
                        &bytes,
                        None,
                        None,
                        NEGATIVE_CACHE_TTL_S,
                    )
                    .await;
            }
        }

        res.notes = st.notes.clone();
        let mut report = FulltextReport::new(res);
        report.notes = st.notes;
        report.steps_tried = st.steps_tried;
        report
    }

    #[allow(clippy::too_many_arguments)]
    fn build_available_result(
        &self,
        canonical_id: &str,
        format: &str,
        source: &str,
        license: Option<String>,
        url: String,
        oa_status: Option<String>,
        landing_url: Option<String>,
        doc: &ParsedDocument,
        opts: &FulltextOpts,
    ) -> FulltextResult {
        // I11: Default budget is `cfg.max_content_chars` (50k), with explicit caller requests
        // clamped at `cfg.max_document_chars`.
        let mut effective_opts = opts.clone();
        if let Some(m) = effective_opts.max_chars {
            effective_opts.max_chars = Some(m.min(self.deps.cfg.max_document_chars));
        }
        let (sections, content, _matched) =
            filter_and_budget(doc, &effective_opts, self.deps.cfg.max_content_chars);
        let mut res = FulltextResult::new(canonical_id, true);
        res.format = Some(format.to_owned());
        res.source = Some(source.to_owned());
        res.license = license.or_else(|| doc.license.clone());
        res.url = Some(url);
        res.oa_status = oa_status;
        res.landing_url = landing_url;
        if !sections.is_empty() || opts.sections.is_some() {
            res.sections = Some(sections);
        }
        res.content = Some(content);
        res
    }
}

struct ResolvedHit {
    format: String,
    source: String,
    license: Option<String>,
    url: String,
    oa_status: Option<String>,
    landing_url: Option<String>,
    truncated: bool,
    cacheable: bool,
    doc: ParsedDocument,
}

impl ResolvedHit {
    #[allow(clippy::too_many_arguments)]
    fn complete(
        format: &str,
        source: &str,
        license: Option<String>,
        url: String,
        oa_status: Option<String>,
        landing_url: Option<String>,
        doc: ParsedDocument,
    ) -> Self {
        Self {
            format: format.to_owned(),
            source: source.to_owned(),
            license,
            url,
            oa_status,
            landing_url,
            truncated: false,
            cacheable: true,
            doc,
        }
    }
}

// ---------------------------------------------------------------------------
// Blocking parser wrappers (`tokio::task::spawn_blocking` — B2)
// ---------------------------------------------------------------------------

async fn parse_jats_blocking(xml: String, max_chars: usize) -> Result<ParsedDocument, FsmError> {
    tokio::task::spawn_blocking(move || parse_jats_xml_with_limit(&xml, max_chars))
        .await
        .map_err(|e| FsmError::internal("parse_jats", e.to_string()))?
}

async fn parse_bioc_blocking(text: String) -> Result<ParsedDocument, FsmError> {
    tokio::task::spawn_blocking(move || parse_bioc_json(&text))
        .await
        .map_err(|e| FsmError::internal("parse_bioc", e.to_string()))?
}

async fn parse_arxiv_html5_blocking(html: String) -> Result<ParsedDocument, FsmError> {
    tokio::task::spawn_blocking(move || parse_arxiv_html5(&html))
        .await
        .map_err(|e| FsmError::internal("parse_arxiv_html5", e.to_string()))?
}

async fn parse_generic_html5_blocking(html: String) -> Result<ParsedDocument, FsmError> {
    tokio::task::spawn_blocking(move || parse_generic_html5(&html))
        .await
        .map_err(|e| FsmError::internal("parse_generic_html5", e.to_string()))?
}

async fn extract_text_sections_blocking(
    text: String,
    title: Option<String>,
    license: Option<String>,
) -> Result<ParsedDocument, FsmError> {
    tokio::task::spawn_blocking(move || extract_text_sections(&text, title, license))
        .await
        .map_err(|e| FsmError::internal("extract_text_sections", e.to_string()))
}

// ---------------------------------------------------------------------------
// I8: Extract scholarly PIDs, `citation_pdf_url`, and title from `StructuredMeta`
// ---------------------------------------------------------------------------

fn extract_ids_from_structured_meta(
    meta: &StructuredMeta,
    ids: &mut ParsedPaperId,
    deferred_pdfs: &mut Vec<(String, String, Option<String>)>,
) -> Option<String> {
    let mut title: Option<String> = None;

    for (raw_key, val) in &meta.meta_fallback {
        let key = raw_key.trim().to_ascii_lowercase();
        let val = val.trim();
        if val.is_empty() {
            continue;
        }
        match key.as_str() {
            "citation_doi"
            | "dc.identifier"
            | "dc.identifier.doi"
            | "prism.doi"
            | "bepress_citation_doi"
            | "scheme.doi"
            | "doi" => {
                if ids.doi.is_none()
                    && let Some(doi) = normalize_doi(val)
                {
                    ids.is_arxiv_doi = doi.starts_with("10.48550/arxiv.");
                    if ids.zenodo.is_none() {
                        ids.zenodo = extract_zenodo_id_from_doi(&doi);
                    }
                    ids.doi = Some(doi);
                }
            }
            "citation_arxiv_id" => {
                if ids.arxiv.is_none()
                    && let Some((base, fetch)) = normalize_arxiv_id(val)
                {
                    ids.arxiv = Some(base);
                    ids.arxiv_fetch = Some(fetch);
                }
            }
            "citation_pmid" => {
                if ids.pmid.is_none()
                    && let Some(pmid) = normalize_pmid(val)
                {
                    ids.pmid = Some(pmid);
                }
            }
            "citation_pmcid" => {
                if ids.pmcid.is_none()
                    && let Some(pmc) = normalize_pmcid(val)
                {
                    ids.pmcid = Some(pmc);
                }
            }
            "citation_pdf_url" | "bepress_citation_pdf_url" => {
                if is_lawful_candidate_url(val) && !deferred_pdfs.iter().any(|(u, _, _)| u == val) {
                    deferred_pdfs.push((val.to_owned(), "publisher_meta".to_owned(), None));
                }
            }
            "citation_title" | "dc.title" if title.is_none() => {
                title = Some(val.to_owned());
            }
            _ => {}
        }
    }

    for node in &meta.json_ld {
        scan_json_ld_for_ids(node, ids, &mut title);
    }

    if title.is_none() {
        title = meta
            .opengraph
            .get("og:title")
            .or_else(|| meta.opengraph.get("title"))
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
    }

    ids.refresh_canonical();
    title
}

fn scan_json_ld_for_ids(val: &Value, ids: &mut ParsedPaperId, title: &mut Option<String>) {
    match val {
        Value::Array(items) => {
            for item in items {
                scan_json_ld_for_ids(item, ids, title);
            }
        }
        Value::Object(map) => {
            if let Some(graph) = map.get("@graph") {
                scan_json_ld_for_ids(graph, ids, title);
            }
            for key in ["identifier", "sameAs", "@id"] {
                if let Some(v) = map.get(key) {
                    apply_json_ld_id_value(v, ids);
                }
            }
            if title.is_none() {
                for key in ["headline", "name"] {
                    if let Some(s) = map.get(key).and_then(Value::as_str) {
                        let t = s.trim();
                        if !t.is_empty() {
                            *title = Some(t.to_owned());
                            break;
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

fn apply_json_ld_id_value(v: &Value, ids: &mut ParsedPaperId) {
    match v {
        Value::String(s) => {
            if ids.doi.is_none()
                && let Some(doi) = normalize_doi(s)
            {
                ids.is_arxiv_doi = doi.starts_with("10.48550/arxiv.");
                if ids.zenodo.is_none() {
                    ids.zenodo = extract_zenodo_id_from_doi(&doi);
                }
                ids.doi = Some(doi);
            }
            if ids.pmcid.is_none()
                && let Some(pmc) = normalize_pmcid(s)
            {
                ids.pmcid = Some(pmc);
            }
            if ids.arxiv.is_none()
                && let Some((base, fetch)) = normalize_arxiv_id(s)
            {
                ids.arxiv = Some(base);
                ids.arxiv_fetch = Some(fetch);
            }
        }
        Value::Array(arr) => {
            for item in arr {
                apply_json_ld_id_value(item, ids);
            }
        }
        Value::Object(obj) => {
            if let Some(inner) = obj.get("value") {
                apply_json_ld_id_value(inner, ids);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Utility helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) struct LinkHeaderItem {
    pub(crate) url: String,
    pub(crate) rel: String,
    pub(crate) media_type: String,
}

/// Parse RFC 8288 HTTP `Link:` header values (FAIR Signposting).
pub(crate) fn parse_link_header(header: &str) -> Vec<LinkHeaderItem> {
    let mut out = Vec::new();
    for entry in header.split(',') {
        let trimmed = entry.trim();
        let Some(rest) = trimmed.strip_prefix('<') else {
            continue;
        };
        let Some((url_part, params_part)) = rest.split_once('>') else {
            continue;
        };
        let url = url_part.trim().to_owned();
        if url.is_empty() {
            continue;
        }
        let mut rel = String::new();
        let mut media_type = String::new();
        for param in params_part.split(';') {
            let p = param.trim();
            if let Some((k, v)) = p.split_once('=') {
                let k = k.trim();
                let v = v.trim().trim_matches('"');
                if k.eq_ignore_ascii_case("rel") {
                    rel = v.to_owned();
                } else if k.eq_ignore_ascii_case("type") {
                    media_type = v.to_owned();
                }
            }
        }
        if !rel.is_empty() {
            out.push(LinkHeaderItem {
                url,
                rel,
                media_type,
            });
        }
    }
    out
}

fn valid_contact_email(opt: &Option<String>) -> Option<&str> {
    let s = opt.as_deref()?.trim();
    if s.len() >= 5 && s.contains('@') && s.contains('.') && !s.contains("${") {
        Some(s)
    } else {
        None
    }
}

fn encode_doi_path(doi: &str) -> String {
    utf8_percent_encode(doi, DOI_PATH_ENCODE).to_string()
}

fn is_bare_doi_resolver_url(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    parsed
        .host_str()
        .is_some_and(|h| h.eq_ignore_ascii_case("doi.org") || h.eq_ignore_ascii_case("dx.doi.org"))
}

fn is_subscriber_tdm_host(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    parsed.host_str().is_some_and(|h| {
        SUBSCRIBER_TDM_HOSTS
            .iter()
            .any(|s| h.eq_ignore_ascii_case(s))
    })
}

fn fulltext_cache_key(canonical_id: &str, prefer_structured: bool) -> String {
    let mode = if prefer_structured { "s" } else { "p" };
    format!("fulltext:v1:{mode}:{canonical_id}")
}

fn compute_budget_reserve(initial_rem: Duration) -> Duration {
    if initial_rem >= Duration::from_secs(3) {
        Duration::from_millis(1_500)
    } else if initial_rem >= Duration::from_millis(1_500) {
        Duration::from_millis(1_000)
    } else {
        initial_rem / 3
    }
}

fn net_cx_from_call(cx: &CallCx, reserve: Duration) -> NetCx {
    let avail = cx.remaining().saturating_sub(reserve / 2);
    let timeout = avail
        .min(Duration::from_secs(STEP_TIMEOUT_S))
        .max(Duration::from_millis(100));
    NetCx::new(cx.cancel.child_token(), timeout)
}

fn elapsed_ms_since(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn now_unix_secs() -> i64 {
    jiff::Timestamp::now().as_second()
}

pub(crate) fn next_utc_midnight_secs(now_secs: i64) -> i64 {
    fsm_scholar::next_utc_midnight(now_secs)
}
