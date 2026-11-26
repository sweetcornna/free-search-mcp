//! Offline ReplayNet integration tests covering all 11 steps of the lawful
//! structured-first full-text chain, plus paywalled/closed responses, Unpaywall
//! I16 email-skip diagnostics, UTC-midnight 429 benching, caching, cancellation,
//! and Review Round 1 regression suites (B1, I3–I9, I11–I12, M1, M3–M7, DOAJ oracle).

use async_trait::async_trait;
use fsm_core::fixture::ReplayNet;
use fsm_core::{
    Bytes, CacheHit, CallCx, CompareItem, Config, DocApi, DocOutline, DocSlice, DownloadResult,
    FetchApi, FetchOpts, FetchOutcome, FsmError, FulltextOpts, HealthRecord, HttpBytes,
    HttpRequest, HttpResponse, NetClient, NetCx, NullStore, PageResult, RawRequestOpts, Registry,
    ResearchBrief, RetractionNotice, ScholarlyWork, SearchResponse, SourceDeps, StoreApi,
    StructuredMeta, TransportProfile,
};
use fsm_scholar::{Provider, ScholarCore, next_utc_midnight};
use fsm_scholar_fulltext::{ScholarFulltext, UNPAYWALL_SKIP_NOTE};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const OA_SELECT_Q: &str =
    "select=id%2Cids%2Cdoi%2Copen_access%2Cbest_oa_location%2Clocations%2Cprimary_location";

// ---------------------------------------------------------------------------
// In-test fakes for DocApi, FetchApi, NetClient, and StoreApi
// ---------------------------------------------------------------------------

enum FakeDocOutcome {
    Slice(DocSlice),
    Err(String),
    Timeout(u64),
}

struct FakeDocApi {
    responses: HashMap<String, FakeDocOutcome>,
    calls: Mutex<Vec<String>>,
    call_lengths: Mutex<Vec<(String, Option<usize>)>>,
}

impl FakeDocApi {
    fn empty() -> Self {
        Self {
            responses: HashMap::new(),
            calls: Mutex::new(Vec::new()),
            call_lengths: Mutex::new(Vec::new()),
        }
    }

    fn with_pdf(self, url: &str, title: &str, content: &str) -> Self {
        self.with_format_doc(url, "pdf", title, content)
    }

    fn with_format_doc(mut self, url: &str, format: &str, title: &str, content: &str) -> Self {
        let mut slice = DocSlice::new(url, format, content);
        slice.title = Some(title.to_owned());
        slice.total_chars = content.chars().count();
        self.responses
            .insert(url.to_owned(), FakeDocOutcome::Slice(slice));
        self
    }

    fn with_truncated_pdf(mut self, url: &str, title: &str, content: &str, total: usize) -> Self {
        let mut slice = DocSlice::new(url, "pdf", content);
        slice.title = Some(title.to_owned());
        slice.total_chars = total;
        slice.truncated = true;
        self.responses
            .insert(url.to_owned(), FakeDocOutcome::Slice(slice));
        self
    }

    fn with_err(mut self, url: &str, msg: &str) -> Self {
        self.responses
            .insert(url.to_owned(), FakeDocOutcome::Err(msg.to_owned()));
        self
    }

    fn with_timeout(mut self, url: &str, ms: u64) -> Self {
        self.responses
            .insert(url.to_owned(), FakeDocOutcome::Timeout(ms));
        self
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn call_lengths(&self) -> Vec<(String, Option<usize>)> {
        self.call_lengths.lock().unwrap().clone()
    }
}

#[async_trait]
impl DocApi for FakeDocApi {
    async fn read(
        &self,
        source: &str,
        start: usize,
        length: Option<usize>,
        _cx: &CallCx,
    ) -> Result<DocSlice, FsmError> {
        self.calls.lock().unwrap().push(source.to_owned());
        self.call_lengths
            .lock()
            .unwrap()
            .push((source.to_owned(), length));
        match self.responses.get(source) {
            Some(FakeDocOutcome::Slice(base)) => {
                // Honour `start` and `length` just like a real DocApi (catches B1!).
                let chars: Vec<char> = base.content.chars().collect();
                let total = base.total_chars.max(chars.len());
                let slice_start = start.min(chars.len());
                let max_take = length.unwrap_or(chars.len().saturating_sub(slice_start));
                let slice_end = (slice_start + max_take).min(chars.len());
                let sliced: String = chars[slice_start..slice_end].iter().collect();
                let truncated = base.truncated || slice_end < total;
                let mut out = base.clone();
                out.start = slice_start;
                out.returned_chars = sliced.chars().count();
                out.total_chars = total;
                out.truncated = truncated;
                out.content = sliced;
                Ok(out)
            }
            Some(FakeDocOutcome::Timeout(ms)) => Err(FsmError::Timeout(*ms)),
            Some(FakeDocOutcome::Err(msg)) => Err(FsmError::upstream("fake_doc", Some(404), msg)),
            None => Err(FsmError::upstream(
                "fake_doc",
                Some(404),
                format!("no fake PDF configured for {source}"),
            )),
        }
    }

    async fn outline(&self, source: &str, _cx: &CallCx) -> Result<DocOutline, FsmError> {
        Ok(DocOutline::new(source, "pdf"))
    }
}

struct FakeFetchApi {
    pages: HashMap<String, PageResult>,
    structured_map: HashMap<String, StructuredMeta>,
    calls: Mutex<Vec<String>>,
}

impl FakeFetchApi {
    fn empty() -> Self {
        Self {
            pages: HashMap::new(),
            structured_map: HashMap::new(),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn with_page(mut self, url: &str, title: &str, content: &str) -> Self {
        let page = PageResult::new(url, title, content, fsm_core::FetchMethod::Http);
        self.pages.insert(url.to_owned(), page);
        self
    }

    fn with_structured(mut self, url: &str, meta: StructuredMeta) -> Self {
        self.structured_map.insert(url.to_owned(), meta);
        self
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl FetchApi for FakeFetchApi {
    async fn fetch(
        &self,
        url: &str,
        _opts: FetchOpts,
        _cx: &CallCx,
    ) -> Result<FetchOutcome, FsmError> {
        self.calls.lock().unwrap().push(url.to_owned());
        match self.pages.get(url) {
            Some(p) => Ok(FetchOutcome::Page(p.clone())),
            None => Err(FsmError::upstream("fake_fetch", Some(404), "not found")),
        }
    }

    async fn fetch_batch(
        &self,
        _urls: Vec<String>,
        _opts: FetchOpts,
        _max_concurrency: u8,
        _cx: &CallCx,
    ) -> Result<Vec<PageResult>, FsmError> {
        Ok(Vec::new())
    }

    async fn request(&self, opts: RawRequestOpts, _cx: &CallCx) -> Result<HttpResponse, FsmError> {
        Ok(HttpResponse::new(opts.url, 200, ""))
    }

    async fn structured(&self, url: &str, _cx: &CallCx) -> Result<StructuredMeta, FsmError> {
        Ok(self
            .structured_map
            .get(url)
            .cloned()
            .unwrap_or_else(|| StructuredMeta::new(url)))
    }

    async fn research(
        &self,
        query: &str,
        _opts: fsm_core::ResearchOpts,
        _cx: &CallCx,
    ) -> Result<ResearchBrief, FsmError> {
        Ok(ResearchBrief::new(query, SearchResponse::new(query, "web")))
    }

    async fn compare(
        &self,
        _query: &str,
        _urls: Vec<String>,
        _cx: &CallCx,
    ) -> Result<Vec<CompareItem>, FsmError> {
        Ok(Vec::new())
    }

    async fn download(&self, url: &str, _cx: &CallCx) -> Result<DownloadResult, FsmError> {
        Ok(DownloadResult::new(
            url,
            "/tmp/x",
            "application/pdf",
            0,
            "00",
        ))
    }

    async fn html_to_md(&self, html: &str) -> Result<String, FsmError> {
        Ok(html.to_owned())
    }
}

struct SlowNotFoundNet {
    delay: Duration,
}

#[async_trait]
impl NetClient for SlowNotFoundNet {
    async fn send(
        &self,
        req: HttpRequest,
        _profile: TransportProfile,
        cx: &NetCx,
    ) -> Result<HttpBytes, FsmError> {
        tokio::select! {
            _ = tokio::time::sleep(self.delay) => {
                Ok(HttpBytes::new(req.url, 404, vec![], Vec::new()))
            }
            _ = cx.cancel.cancelled() => {
                Err(FsmError::Timeout(0))
            }
        }
    }

    fn check_url_offline(&self, _url: &str) -> Result<(), FsmError> {
        Ok(())
    }
}

struct AlwaysErrorNet;

#[async_trait]
impl NetClient for AlwaysErrorNet {
    async fn send(
        &self,
        _req: HttpRequest,
        _profile: TransportProfile,
        _cx: &NetCx,
    ) -> Result<HttpBytes, FsmError> {
        Err(FsmError::upstream(
            "offline_sim",
            Some(503),
            "service unavailable",
        ))
    }

    fn check_url_offline(&self, _url: &str) -> Result<(), FsmError> {
        Ok(())
    }
}

#[derive(Default)]
struct MemoryStore {
    api_cache: Mutex<HashMap<String, Bytes>>,
    health: Mutex<Vec<HealthRecord>>,
    works: Mutex<HashMap<String, ScholarlyWork>>,
}

#[async_trait]
impl StoreApi for MemoryStore {
    async fn cache_search(&self, _fts_query: &str, _limit: u32) -> Result<Vec<CacheHit>, FsmError> {
        Ok(Vec::new())
    }
    async fn get_page(
        &self,
        _url: &str,
        _max_age_s: Option<u64>,
    ) -> Result<Option<PageResult>, FsmError> {
        Ok(None)
    }
    async fn put_page(&self, _page: &PageResult, _ttl_s: u64) -> Result<(), FsmError> {
        Ok(())
    }
    async fn kv_put(
        &self,
        _session_id: &str,
        _key: &str,
        _val: &Value,
        _ttl_s: u64,
    ) -> Result<(), FsmError> {
        Ok(())
    }
    async fn kv_get(&self, _session_id: &str, _key: &str) -> Result<Option<Value>, FsmError> {
        Ok(None)
    }
    async fn search_cache_get(
        &self,
        _key: &str,
        _max_age_s: Option<u64>,
    ) -> Result<Option<(SearchResponse, u64)>, FsmError> {
        Ok(None)
    }
    async fn search_cache_put(
        &self,
        _key: &str,
        _resp: &SearchResponse,
        _ttl_s: u64,
    ) -> Result<(), FsmError> {
        Ok(())
    }
    async fn api_cache_get(
        &self,
        key: &str,
    ) -> Result<Option<(Bytes, Option<String>, Option<String>)>, FsmError> {
        Ok(self
            .api_cache
            .lock()
            .unwrap()
            .get(key)
            .cloned()
            .map(|b| (b, None, None)))
    }
    async fn api_cache_put(
        &self,
        key: &str,
        _source_id: &str,
        body: &[u8],
        _etag: Option<&str>,
        _last_mod: Option<&str>,
        _ttl_s: u64,
    ) -> Result<(), FsmError> {
        self.api_cache
            .lock()
            .unwrap()
            .insert(key.to_owned(), Bytes::copy_from_slice(body));
        Ok(())
    }
    async fn scholar_get(
        &self,
        canonical_key_or_pid: &str,
    ) -> Result<Option<ScholarlyWork>, FsmError> {
        Ok(self
            .works
            .lock()
            .unwrap()
            .get(canonical_key_or_pid)
            .cloned())
    }
    async fn scholar_put(
        &self,
        canonical_key: &str,
        work: &ScholarlyWork,
        _ttl_s: u64,
    ) -> Result<(), FsmError> {
        self.works
            .lock()
            .unwrap()
            .insert(canonical_key.to_owned(), work.clone());
        Ok(())
    }
    async fn retraction_lookup(
        &self,
        _norm_doi: &str,
    ) -> Result<Option<RetractionNotice>, FsmError> {
        Ok(None)
    }
    async fn retractions_replace_batch(
        &self,
        _rows: Vec<(String, RetractionNotice)>,
        _etag: Option<String>,
    ) -> Result<(), FsmError> {
        Ok(())
    }
    async fn sync_state_get(
        &self,
        _name: &str,
    ) -> Result<Option<(Option<String>, i64, Option<i64>)>, FsmError> {
        Ok(None)
    }
    async fn sync_state_mark(
        &self,
        _name: &str,
        _etag: Option<&str>,
        _started_at: i64,
        _finished_at: Option<i64>,
    ) -> Result<(), FsmError> {
        Ok(())
    }
    async fn health_load_all(&self) -> Result<Vec<HealthRecord>, FsmError> {
        Ok(self.health.lock().unwrap().clone())
    }
    async fn health_save(&self, rec: &HealthRecord) -> Result<(), FsmError> {
        self.health.lock().unwrap().push(rec.clone());
        Ok(())
    }
    async fn download_record(&self, _rec: &DownloadResult, _ttl_s: u64) -> Result<(), FsmError> {
        Ok(())
    }
}

fn fixture_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(rel)
}

fn build_service(
    net: Arc<dyn NetClient>,
    docs: Arc<dyn DocApi>,
    fetch: Arc<dyn FetchApi>,
    store: Arc<dyn StoreApi>,
    cfg: Config,
) -> ScholarFulltext {
    let deps = SourceDeps::new(net, store, Arc::new(cfg));
    let core = Arc::new(ScholarCore::new(Arc::new(Registry::new()), deps.clone()));
    ScholarFulltext::new(core, docs, fetch, deps)
}

fn make_exchange(
    url: &str,
    status: u16,
    ct: &str,
    body: &str,
    accept: Option<&str>,
) -> fsm_core::fixture::Exchange {
    let mut req = fsm_core::fixture::FixtureRequest::new("GET", url);
    if let Some(a) = accept {
        req.headers_subset.insert("accept".into(), a.into());
    }
    let resp = fsm_core::fixture::FixtureResponse::from_bytes(
        status,
        vec![("content-type".into(), ct.into())],
        body.as_bytes(),
    );
    fsm_core::fixture::Exchange::new(req, resp)
}

fn empty_repo_exchanges(doi: &str) -> Vec<fsm_core::fixture::Exchange> {
    let enc = doi.replace('/', "%2F");
    vec![
        make_exchange(
            &format!("https://api.core.ac.uk/v3/search/works?limit=3&q=doi%3A%22{enc}%22"),
            200,
            "application/json",
            r#"{"totalHits":0,"results":[]}"#,
            Some("application/json"),
        ),
        make_exchange(
            &format!("https://doaj.org/api/search/articles/doi:{enc}"),
            200,
            "application/json",
            r#"{"total":0,"results":[]}"#,
            Some("application/json"),
        ),
        make_exchange(
            &format!("https://api.crossref.org/works/{doi}"),
            404,
            "text/plain",
            "Not Found",
            Some("application/json"),
        ),
        make_exchange(
            &format!("https://api.datacite.org/dois/{doi}"),
            404,
            "application/vnd.api+json",
            "{}",
            Some("application/vnd.api+json"),
        ),
        make_exchange(
            &format!(
                "https://api.archives-ouvertes.fr/search/?fl=halId_s%2Curi_s%2CfileMain_s%2CopenAccess_bool%2CdoiId_s&q=doiId_s%3A%22{enc}%22&rows=1&wt=json"
            ),
            200,
            "application/json",
            r#"{"response":{"numFound":0,"docs":[]}}"#,
            Some("application/json"),
        ),
        make_exchange(
            &format!("https://zenodo.org/api/records?q=doi%3A%22{enc}%22&size=1"),
            200,
            "application/json",
            r#"{"hits":{"total":0,"hits":[]}}"#,
            Some("application/json"),
        ),
    ]
}

// ---------------------------------------------------------------------------
// Step 1 + Step 2: Live PMC idconv -> Live Europe PMC PMC3257301 JATS XML
// ---------------------------------------------------------------------------
#[tokio::test]
async fn step1_and_step2_pmc_idconv_to_europepmc_jats_with_section_filter() {
    let net =
        Arc::new(ReplayNet::from_file(fixture_path("step1_2_pmc_jats/default.json")).unwrap());
    let ft = build_service(
        net.clone(),
        Arc::new(FakeDocApi::empty()),
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        Config::default(),
    );

    let mut opts = FulltextOpts::default();
    opts.sections = Some(vec!["abstract".into(), "methods".into()]);
    opts.max_chars = Some(4000);
    let report = ft
        .fulltext_report("10.1371/journal.ppat.1002485", opts, &CallCx::default())
        .await
        .unwrap();

    assert!(
        net.unmatched().is_empty(),
        "unmatched: {:?}",
        net.unmatched()
    );
    assert_eq!(
        report.steps_tried,
        vec!["step1:pmc_idconv", "step2:europepmc_jats"]
    );
    let res = report.result;
    assert!(res.available);
    assert_eq!(res.format.as_deref(), Some("jats"));
    assert_eq!(res.source.as_deref(), Some("europepmc"));
    assert_eq!(res.license.as_deref(), Some("CC-BY"));
    assert_eq!(
        res.url.as_deref(),
        Some("https://www.ebi.ac.uk/europepmc/webservices/rest/PMC3257301/fullTextXML")
    );
    let secs = res.sections.as_ref().unwrap();
    assert!(secs.len() >= 2, "expected abstract + methods, got {secs:?}");
    let content = res.content.as_deref().unwrap();
    assert!(content.chars().count() <= 4050);
    // Real live PMC3257301 is a Candida albicans / CO2 sensing paper (I12)!
    assert!(
        content.contains("CO2") || content.contains("Candida") || content.contains("carbonic"),
        "expected real PMC3257301 CO2/Candida content, got: {}",
        &content[..content.len().min(300)]
    );
}

// ---------------------------------------------------------------------------
// Step 1 + Step 3: Europe PMC 500 -> Real PMC3257301 BioC JSON
// ---------------------------------------------------------------------------
#[tokio::test]
async fn step3_pmc_bioc_fallback_parses_real_ncbi_bioc_json() {
    let net =
        Arc::new(ReplayNet::from_file(fixture_path("step1_3_pmc_bioc/default.json")).unwrap());
    let ft = build_service(
        net.clone(),
        Arc::new(FakeDocApi::empty()),
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        Config::default(),
    );

    let mut opts = FulltextOpts::default();
    opts.sections = Some(vec!["abstract".into(), "results".into()]);
    let report = ft
        .fulltext_report("10.1371/journal.ppat.1002485", opts, &CallCx::default())
        .await
        .unwrap();

    assert!(
        net.unmatched().is_empty(),
        "unmatched: {:?}",
        net.unmatched()
    );
    assert_eq!(
        report.steps_tried,
        vec!["step1:pmc_idconv", "step2:europepmc_jats", "step3:pmc_bioc"]
    );
    let res = report.result;
    assert!(res.available);
    assert_eq!(res.format.as_deref(), Some("bioc"));
    assert_eq!(res.source.as_deref(), Some("pmc_bioc"));
    assert_eq!(res.license.as_deref(), Some("CC-BY"));
    let secs = res.sections.as_ref().unwrap();
    assert!(secs.len() >= 2, "got {} sections", secs.len());
    assert!(res.content.as_deref().unwrap().len() > 200);
}

// ---------------------------------------------------------------------------
// Step 4: Real bioRxiv API + real .source.xml JATS XML
// ---------------------------------------------------------------------------
#[tokio::test]
async fn step4_biorxiv_jats_parses_real_source_xml() {
    let net = Arc::new(ReplayNet::from_file(fixture_path("step4_biorxiv/default.json")).unwrap());
    let ft = build_service(
        net.clone(),
        Arc::new(FakeDocApi::empty()),
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        Config::default(),
    );

    let report = ft
        .fulltext_report(
            "https://www.biorxiv.org/content/10.1101/2020.01.30.927871v1",
            FulltextOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();

    assert!(
        net.unmatched().is_empty(),
        "unmatched: {:?}",
        net.unmatched()
    );
    assert_eq!(
        report.steps_tried,
        vec!["step1:pmc_idconv", "step4:biorxiv_medrxiv"]
    );
    let res = report.result;
    assert!(res.available);
    assert_eq!(res.format.as_deref(), Some("jats"));
    assert_eq!(res.source.as_deref(), Some("biorxiv"));
    assert_eq!(res.oa_status.as_deref(), Some("green"));
    assert_eq!(res.license.as_deref(), Some("CC-BY-NC-ND-4.0"));
    let secs = res.sections.as_ref().unwrap();
    assert!(secs.len() >= 3, "expected >=3 sections, got {}", secs.len());
}

// ---------------------------------------------------------------------------
// Step 5a: Real arXiv native HTML5 (2401.00001v1)
// ---------------------------------------------------------------------------
#[tokio::test]
async fn step5a_arxiv_native_html5_parses_real_latexml() {
    let net =
        Arc::new(ReplayNet::from_file(fixture_path("step5_arxiv_native/default.json")).unwrap());
    let ft = build_service(
        net.clone(),
        Arc::new(FakeDocApi::empty()),
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        Config::default(),
    );

    let report = ft
        .fulltext_report(
            "arXiv:2401.00001v1",
            FulltextOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();

    assert!(
        net.unmatched().is_empty(),
        "unmatched: {:?}",
        net.unmatched()
    );
    assert_eq!(report.steps_tried, vec!["step5:arxiv_html5"]);
    let res = report.result;
    assert!(res.available);
    assert_eq!(res.format.as_deref(), Some("html5"));
    assert_eq!(res.source.as_deref(), Some("arxiv_html5"));
    assert_eq!(res.oa_status.as_deref(), Some("green"));
    assert_eq!(
        res.landing_url.as_deref(),
        Some("https://arxiv.org/abs/2401.00001")
    );
    let secs = res.sections.as_ref().unwrap();
    assert!(secs.len() >= 3, "expected >=3 sections, got {}", secs.len());
}

// ---------------------------------------------------------------------------
// Step 5b: arXiv 404 -> real ar5iv HTML5 (1706.03762 Attention Is All You Need)
// ---------------------------------------------------------------------------
#[tokio::test]
async fn step5b_arxiv_falls_back_to_real_ar5iv_html5_for_1706_03762() {
    let net =
        Arc::new(ReplayNet::from_file(fixture_path("step5_arxiv_ar5iv/default.json")).unwrap());
    let ft = build_service(
        net.clone(),
        Arc::new(FakeDocApi::empty()),
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        Config::default(),
    );

    let mut opts = FulltextOpts::default();
    opts.sections = Some(vec!["abstract".into(), "methods".into()]);
    let report = ft
        .fulltext_report("10.48550/arXiv.1706.03762", opts, &CallCx::default())
        .await
        .unwrap();

    assert!(
        net.unmatched().is_empty(),
        "unmatched: {:?}",
        net.unmatched()
    );
    assert_eq!(report.steps_tried, vec!["step5:arxiv_html5"]);
    let res = report.result;
    assert!(res.available);
    assert_eq!(res.format.as_deref(), Some("html5"));
    assert_eq!(res.source.as_deref(), Some("ar5iv"));
    let secs = res.sections.as_ref().unwrap();
    assert!(
        secs.iter()
            .any(|s| s.heading.to_ascii_lowercase().contains("abstract")),
        "missing abstract in {secs:?}"
    );
    assert!(
        secs.iter()
            .any(|s| s.heading.to_ascii_lowercase().contains("architecture")),
        "methods alias should match Model Architecture, got: {:?}",
        secs.iter().map(|s| &s.heading).collect::<Vec<_>>()
    );
    let content = res.content.as_deref().unwrap();
    assert!(content.contains("Transformer"));
}

// ---------------------------------------------------------------------------
// Step 5c: arXiv PDF fallback via DocApi & prefer_structured = false
// ---------------------------------------------------------------------------
#[tokio::test]
async fn step5c_arxiv_pdf_fallback_and_prefer_structured_false() {
    let pdf_url = "https://arxiv.org/pdf/1706.03762";
    let docs = Arc::new(FakeDocApi::empty().with_pdf(
        pdf_url,
        "Attention Is All You Need",
        "Abstract\nThe dominant sequence transduction models.\n\n1. Introduction\nRecurrent neural networks.",
    ));

    // A: prefer_structured = true, both HTML5 & ar5iv return 404 -> DocApi PDF fallback
    let net = Arc::new(ReplayNet::from_file(fixture_path("step5_arxiv_pdf/default.json")).unwrap());
    let ft = build_service(
        net.clone(),
        docs.clone(),
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        Config::default(),
    );
    let res = ft
        .fulltext("1706.03762", FulltextOpts::default(), &CallCx::default())
        .await
        .unwrap();
    assert!(res.available);
    assert_eq!(res.format.as_deref(), Some("pdf"));
    assert_eq!(res.source.as_deref(), Some("arxiv_pdf"));
    assert_eq!(docs.calls(), vec![pdf_url.to_owned()]);

    // B: prefer_structured = false -> skips HTML5 network hits completely!
    let net_empty = Arc::new(ReplayNet::from_fixtures(vec![]).unwrap());
    let docs2 = Arc::new(FakeDocApi::empty().with_pdf(
        pdf_url,
        "Attention Is All You Need",
        "Abstract\nDirect PDF read without HTML5 fetch.",
    ));
    let ft2 = build_service(
        net_empty.clone(),
        docs2.clone(),
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        Config::default(),
    );
    let mut opts = FulltextOpts::default();
    opts.prefer_structured = false;
    let res2 = ft2
        .fulltext("1706.03762", opts, &CallCx::default())
        .await
        .unwrap();
    assert!(res2.available && res2.format.as_deref() == Some("pdf"));
    assert!(
        net_empty.hits().is_empty(),
        "must not hit HTML5 when prefer_structured=false"
    );
    assert_eq!(docs2.calls(), vec![pdf_url.to_owned()]);
}

// ---------------------------------------------------------------------------
// Step 6a (PLOS JATS, GET-only) & Step 6b (Springer Nature OA JATS, redacted key)
// ---------------------------------------------------------------------------
#[tokio::test]
async fn step6_plos_and_springer_oa_jats() {
    let net = Arc::new(ReplayNet::from_file(fixture_path("step6_plos/default.json")).unwrap());
    let ft = build_service(
        net.clone(),
        Arc::new(FakeDocApi::empty()),
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        Config::default(),
    );
    let report = ft
        .fulltext_report(
            "10.1371/journal.pgen.1002441",
            FulltextOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(
        net.unmatched().is_empty(),
        "unmatched: {:?}",
        net.unmatched()
    );
    assert!(net.hits().iter().all(|h| h.method == "GET"));
    assert_eq!(
        report.steps_tried,
        vec!["step1:pmc_idconv", "step6:plos_jats"]
    );
    assert!(report.result.available);
    assert_eq!(report.result.format.as_deref(), Some("jats"));
    assert_eq!(report.result.source.as_deref(), Some("plos"));
    assert_eq!(report.result.oa_status.as_deref(), Some("gold"));
    assert!(report.result.sections.as_ref().unwrap().len() >= 4);

    // 6b: Springer Nature OA JATS with API key configured (redacted from ReplayNet & output URL!)
    let net_sp =
        Arc::new(ReplayNet::from_file(fixture_path("step6_springer_oa/default.json")).unwrap());
    let mut cfg = Config::default();
    cfg.api_keys.insert(
        "SEARCH_MCP_SPRINGER_API_KEY".to_owned(),
        "secret-test-key".to_owned(),
    );
    let ft_sp = build_service(
        net_sp.clone(),
        Arc::new(FakeDocApi::empty()),
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        cfg,
    );
    let report_sp = ft_sp
        .fulltext_report(
            "10.1186/s13059-024-03199-x",
            FulltextOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(
        net_sp.unmatched().is_empty(),
        "unmatched: {:?}",
        net_sp.unmatched()
    );
    assert!(report_sp.result.available);
    assert_eq!(report_sp.result.format.as_deref(), Some("jats"));
    assert_eq!(report_sp.result.source.as_deref(), Some("springer_oa"));
    assert_eq!(report_sp.result.license.as_deref(), Some("CC-BY-4.0"));
    assert!(
        !report_sp
            .result
            .url
            .as_deref()
            .unwrap()
            .contains("secret-test-key"),
        "API key must never leak into FulltextResult.url"
    );
}

// ---------------------------------------------------------------------------
// Step 7: Unpaywall (I16 email-skip AND enabled OA PDF resolution)
// ---------------------------------------------------------------------------
#[tokio::test]
async fn step7_unpaywall_skips_cleanly_without_email_and_resolves_with_email() {
    let pdf_url = "https://academic.oup.com/nar/article-pdf/52/1/100/gkad999.pdf";
    let docs = Arc::new(FakeDocApi::empty().with_pdf(
        pdf_url,
        "NAR Open Access Article",
        "Abstract\nOpen access nucleic acids analysis.\n\nMethods\nSequencing protocol details.",
    ));

    let net = Arc::new(ReplayNet::from_file(fixture_path("step7_unpaywall/oa_pdf.json")).unwrap());
    let mut cfg = Config::default();
    cfg.contact_email = Some("researcher@example.edu".to_owned());
    let ft = build_service(
        net.clone(),
        docs.clone(),
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        cfg,
    );
    let report = ft
        .fulltext_report(
            "10.1093/nar/gkad999",
            FulltextOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(
        net.unmatched().is_empty(),
        "unmatched: {:?}",
        net.unmatched()
    );
    assert_eq!(
        report.steps_tried,
        vec!["step1:pmc_idconv", "step7:unpaywall"]
    );
    assert!(report.notes.is_empty());
    assert!(report.result.available);
    assert_eq!(report.result.format.as_deref(), Some("pdf"));
    assert_eq!(report.result.source.as_deref(), Some("unpaywall"));
    assert_eq!(report.result.oa_status.as_deref(), Some("gold"));
    assert_eq!(docs.calls(), vec![pdf_url.to_owned()]);
}

// ---------------------------------------------------------------------------
// Step 8a (OpenAlex $0) & Step 8b (OpenAlex 429 bench + Semantic Scholar)
// ---------------------------------------------------------------------------
#[tokio::test]
async fn step8a_openalex_free_lookup_and_step7_skip_note() {
    let pdf_url = "https://www.nature.com/articles/nature12373.pdf";
    let docs = Arc::new(FakeDocApi::empty().with_pdf(
        pdf_url,
        "Nanometre-scale thermometry in a living cell",
        "Abstract\nQuantum control of nitrogen-vacancy centres in nanodiamonds.\n\nMethods\nOptical trapping.",
    ));
    let net = Arc::new(ReplayNet::from_file(fixture_path("step8_openalex/default.json")).unwrap());
    let ft = build_service(
        net.clone(),
        docs.clone(),
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        Config::default(),
    );

    let report = ft
        .fulltext_report(
            "10.1038/nature12373",
            FulltextOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();

    assert!(
        net.unmatched().is_empty(),
        "unmatched: {:?}",
        net.unmatched()
    );
    assert!(
        report.notes.iter().any(|n| n == UNPAYWALL_SKIP_NOTE),
        "expected Unpaywall skip note, got {:?}",
        report.notes
    );
    assert!(
        net.hits()
            .iter()
            .all(|h| !h.url.contains("api.unpaywall.org")),
        "must not call Unpaywall when CONTACT_EMAIL is unset"
    );
    assert!(report.result.available);
    assert_eq!(report.result.format.as_deref(), Some("pdf"));
    assert_eq!(report.result.source.as_deref(), Some("openalex"));
    assert_eq!(report.result.oa_status.as_deref(), Some("bronze"));
    assert_eq!(docs.calls(), vec![pdf_url.to_owned()]);
}

#[tokio::test]
async fn step8b_openalex_429_benches_until_midnight_and_s2_resolves_pdf() {
    let pdf_url = "https://www.nature.com/articles/nature12373.pdf";
    let docs = Arc::new(FakeDocApi::empty().with_pdf(
        pdf_url,
        "Nanometre-scale thermometry",
        "Abstract\nThermometry via NV centers.",
    ));
    let net = Arc::new(ReplayNet::from_file(fixture_path("step8_s2/default.json")).unwrap());
    let store = Arc::new(MemoryStore::default());
    let ft = build_service(
        net.clone(),
        docs.clone(),
        Arc::new(FakeFetchApi::empty()),
        store.clone(),
        Config::default(),
    );

    let report1 = ft
        .fulltext_report(
            "10.1038/nature12373",
            FulltextOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(
        net.unmatched().is_empty(),
        "unmatched: {:?}",
        net.unmatched()
    );
    assert!(report1.result.available);
    assert_eq!(report1.result.source.as_deref(), Some("semanticscholar"));
    assert!(
        report1.notes.iter().any(|n| n.contains("429")),
        "expected 429 bench note, got {:?}",
        report1.notes
    );
    assert_eq!(store.health.lock().unwrap().len(), 1);
    assert_eq!(store.health.lock().unwrap()[0].state, "benched");
    // I5: S2 landing URL must NOT leak when DOI is known!
    assert_eq!(
        report1.result.landing_url.as_deref(),
        Some("https://doi.org/10.1038/nature12373")
    );

    // Clear only fulltext positive cache so second call re-runs chain and proves OpenAlex stays benched!
    store
        .api_cache
        .lock()
        .unwrap()
        .retain(|k, _| !k.starts_with("fulltext:"));
    let oa_hits_before = net
        .hits()
        .iter()
        .filter(|h| h.url.contains("api.openalex.org"))
        .count();
    let report2 = ft
        .fulltext_report(
            "10.1038/nature12373",
            FulltextOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    let oa_hits_after = net
        .hits()
        .iter()
        .filter(|h| h.url.contains("api.openalex.org"))
        .count();
    assert_eq!(
        oa_hits_before, oa_hits_after,
        "benched OpenAlex must not be hit again before UTC midnight"
    );
    assert!(
        report2
            .steps_tried
            .iter()
            .any(|s| s == "step8a:openalex:benched"),
        "steps: {:?}",
        report2.steps_tried
    );
}

// ---------------------------------------------------------------------------
// Step 9: CORE v3 inline `fullText` AND placeholder rejection -> PDF fallback
// ---------------------------------------------------------------------------
#[tokio::test]
async fn step9_core_inline_fulltext_and_placeholder_pdf_fallback() {
    let net =
        Arc::new(ReplayNet::from_file(fixture_path("step9_core/inline_fulltext.json")).unwrap());
    let ft = build_service(
        net.clone(),
        Arc::new(FakeDocApi::empty()),
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        Config::default(),
    );
    let mut opts = FulltextOpts::default();
    opts.sections = Some(vec!["methods".into()]);
    let report = ft
        .fulltext_report("10.1145/3580305.3599501", opts, &CallCx::default())
        .await
        .unwrap();
    assert!(
        net.unmatched().is_empty(),
        "unmatched: {:?}",
        net.unmatched()
    );
    assert!(report.result.available);
    assert_eq!(report.result.format.as_deref(), Some("text"));
    assert_eq!(report.result.source.as_deref(), Some("core_v3"));
    assert!(
        report
            .result
            .content
            .as_deref()
            .unwrap()
            .contains("Horvitz-Thompson")
    );

    let core_pdf = "https://core.ac.uk/download/24001234.pdf";
    let docs = Arc::new(FakeDocApi::empty().with_pdf(
        core_pdf,
        "CORE Repository PDF",
        "Abstract\nFull repository PDF extracted after rejecting public API placeholder.",
    ));
    let net2 =
        Arc::new(ReplayNet::from_file(fixture_path("step9_core/placeholder_pdf.json")).unwrap());
    let ft2 = build_service(
        net2.clone(),
        docs.clone(),
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        Config::default(),
    );
    let report2 = ft2
        .fulltext_report(
            "10.1038/nature12373",
            FulltextOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(
        net2.unmatched().is_empty(),
        "unmatched: {:?}",
        net2.unmatched()
    );
    assert!(report2.result.available);
    assert_eq!(report2.result.format.as_deref(), Some("pdf"));
    assert_eq!(report2.result.source.as_deref(), Some("core_v3"));
    assert_eq!(docs.calls(), vec![core_pdf.to_owned()]);
}

// ---------------------------------------------------------------------------
// Step 10: DOAJ, Crossref TDM, DataCite+Zenodo, HAL, and FAIR Signposting
// ---------------------------------------------------------------------------
#[tokio::test]
async fn step10_doaj_crossref_datacite_zenodo_hal_and_signposting() {
    // 10a: DOAJ
    let doaj_pdf = "http://europepmc.org/articles/PMC3374615?pdf=render";
    let docs =
        Arc::new(FakeDocApi::empty().with_pdf(doaj_pdf, "DOAJ Paper", "Abstract\nCRISPR loci."));
    let net = Arc::new(ReplayNet::from_file(fixture_path("step10_doaj/default.json")).unwrap());
    let ft = build_service(
        net.clone(),
        docs,
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        Config::default(),
    );
    let r = ft
        .fulltext_report(
            "10.3390/genes14010001",
            FulltextOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(
        net.unmatched().is_empty(),
        "unmatched: {:?}",
        net.unmatched()
    );
    assert!(r.result.available && r.result.source.as_deref() == Some("doaj"));

    // 10b: Crossref TDM link[]
    let cr_pdf = "http://www.nature.com/articles/nature12373.pdf";
    let docs =
        Arc::new(FakeDocApi::empty().with_pdf(cr_pdf, "Crossref TDM", "Abstract\nTDM link PDF."));
    let net = Arc::new(ReplayNet::from_file(fixture_path("step10_crossref/default.json")).unwrap());
    let ft = build_service(
        net.clone(),
        docs,
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        Config::default(),
    );
    let r = ft
        .fulltext_report(
            "10.1038/nature12373",
            FulltextOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(
        net.unmatched().is_empty(),
        "unmatched: {:?}",
        net.unmatched()
    );
    assert!(r.result.available && r.result.source.as_deref() == Some("crossref_tdm"));

    // 10c + 10e: DataCite + Zenodo (M8: canonical SPDX CC-BY-4.0)
    let zen_pdf = "https://zenodo.org/api/records/10900000/files/40-47.pdf/content";
    let docs = Arc::new(FakeDocApi::empty().with_pdf(
        zen_pdf,
        "Zenodo Record",
        "Abstract\nZenodo OA PDF.",
    ));
    let net = Arc::new(
        ReplayNet::from_file(fixture_path("step10_datacite_zenodo/default.json")).unwrap(),
    );
    let ft = build_service(
        net.clone(),
        docs,
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        Config::default(),
    );
    let r = ft
        .fulltext_report(
            "10.5281/zenodo.10900000",
            FulltextOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(
        net.unmatched().is_empty(),
        "unmatched: {:?}",
        net.unmatched()
    );
    assert!(r.result.available && r.result.source.as_deref() == Some("zenodo"));
    assert_eq!(r.result.license.as_deref(), Some("CC-BY-4.0"));

    // 10d: HAL direct ID (live fixture)
    let hal_pdf = "https://hal.science/hal-04642650/document";
    let docs =
        Arc::new(FakeDocApi::empty().with_pdf(hal_pdf, "HAL Paper", "Abstract\nGNN tutorial."));
    let net = Arc::new(ReplayNet::from_file(fixture_path("step10_hal/default.json")).unwrap());
    let ft = build_service(
        net.clone(),
        docs,
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        Config::default(),
    );
    let r = ft
        .fulltext_report("hal-04642650", FulltextOpts::default(), &CallCx::default())
        .await
        .unwrap();
    assert!(
        net.unmatched().is_empty(),
        "unmatched: {:?}",
        net.unmatched()
    );
    assert!(r.result.available && r.result.source.as_deref() == Some("hal"));

    // 10f: FAIR Signposting HEAD Link: rel="item"
    let sp_pdf = "https://zenodo.org/records/10900000/files/40-47.pdf";
    let docs = Arc::new(FakeDocApi::empty().with_pdf(
        sp_pdf,
        "Signposted PDF",
        "Abstract\nVia Link header.",
    ));
    let net =
        Arc::new(ReplayNet::from_file(fixture_path("step10_signposting/default.json")).unwrap());
    let ft = build_service(
        net.clone(),
        docs,
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        Config::default(),
    );
    let r = ft
        .fulltext_report(
            "https://repo.example.edu/records/10900000",
            FulltextOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(
        net.unmatched().is_empty(),
        "unmatched: {:?}",
        net.unmatched()
    );
    assert!(r.result.available && r.result.source.as_deref() == Some("signposting"));
    assert_eq!(r.result.license.as_deref(), Some("CC-BY-4.0"));
}

// ---------------------------------------------------------------------------
// Step 11: Wayback CDX fallback (`limit=-1` latest capture — M9)
// ---------------------------------------------------------------------------
#[tokio::test]
async fn step11_wayback_cdx_recovers_vanished_oa_pdf() {
    let orig_pdf = "https://arxiv.org/pdf/1706.03762";
    let archive_pdf =
        "https://web.archive.org/web/20261118024931id_/https://arxiv.org/pdf/1706.03762";
    let docs = Arc::new(
        FakeDocApi::empty()
            .with_err(orig_pdf, "404 vanished")
            .with_pdf(
                archive_pdf,
                "Wayback Preserved Paper",
                "Abstract\nRecovered cleanly from Internet Archive Wayback Machine.",
            ),
    );
    let net = Arc::new(ReplayNet::from_file(fixture_path("step11_wayback/default.json")).unwrap());
    let ft = build_service(
        net.clone(),
        docs.clone(),
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        Config::default(),
    );

    let report = ft
        .fulltext_report("1706.03762", FulltextOpts::default(), &CallCx::default())
        .await
        .unwrap();

    assert!(
        net.unmatched().is_empty(),
        "unmatched: {:?}",
        net.unmatched()
    );
    assert!(
        net.hits()
            .iter()
            .any(|h| h.url.contains("web.archive.org/cdx") && h.url.contains("limit=-1")),
        "Wayback CDX must pass limit=-1 for latest capture"
    );
    assert!(report.steps_tried.iter().any(|s| s == "step11:wayback_cdx"));
    assert!(report.result.available);
    assert_eq!(report.result.format.as_deref(), Some("pdf"));
    assert_eq!(report.result.source.as_deref(), Some("wayback"));
    assert_eq!(report.result.url.as_deref(), Some(archive_pdf));
    assert_eq!(
        docs.calls(),
        vec![orig_pdf.to_owned(), archive_pdf.to_owned()]
    );
}

// ---------------------------------------------------------------------------
// Paywalled / Closed Paper Response (live 10.1126/science.aaa8415 — I12, I5, M6)
// ---------------------------------------------------------------------------
#[tokio::test]
async fn paywalled_paper_returns_closed_contract_and_skips_unpaywall_without_email() {
    let doi = "10.1126/science.aaa8415";
    let docs = Arc::new(FakeDocApi::empty());
    // A: Live recording of 10.1126/science.aaa8415 with CONTACT_EMAIL unset
    let net = Arc::new(ReplayNet::from_file(fixture_path("paywalled/closed.json")).unwrap());
    let ft = build_service(
        net.clone(),
        docs.clone(),
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        Config::default(),
    );
    let report = ft
        .fulltext_report(doi, FulltextOpts::default(), &CallCx::default())
        .await
        .unwrap();

    assert!(
        net.unmatched().is_empty(),
        "unmatched: {:?}",
        net.unmatched()
    );
    assert!(
        net.hits()
            .iter()
            .all(|h| !h.url.contains("api.unpaywall.org")),
        "Unpaywall must be skipped when CONTACT_EMAIL is unset"
    );
    // M6: Crossref live payload for science.aaa8415 contains a similarity-checking link; must NOT be fetched!
    assert!(
        docs.calls().is_empty(),
        "similarity-checking Crossref link must never be passed to DocApi: {:?}",
        docs.calls()
    );
    assert!(report.notes.iter().any(|n| n == UNPAYWALL_SKIP_NOTE));
    let res = report.result;
    assert!(!res.available);
    assert_eq!(res.why.as_deref(), Some("paywalled"));
    assert_eq!(res.oa_status.as_deref(), Some("closed"));
    // I5: landing_url is https://doi.org/10.1126/science.aaa8415, NEVER semanticscholar.org!
    assert_eq!(
        res.landing_url.as_deref(),
        Some("https://doi.org/10.1126/science.aaa8415")
    );

    let wire = serde_json::to_value(&res).unwrap();
    assert_eq!(wire["available"], false);
    assert_eq!(wire["why"], "paywalled");
    assert_eq!(wire["oaStatus"], "closed");
    assert_eq!(
        wire["landingUrl"],
        "https://doi.org/10.1126/science.aaa8415"
    );
    assert!(wire.get("format").is_none());
    assert!(wire.get("sections").is_none());

    // B: CONTACT_EMAIL set -> calls Unpaywall, zero skip notes, still closed/paywalled
    let net2 = Arc::new(
        ReplayNet::from_file(fixture_path("paywalled/closed_with_unpaywall.json")).unwrap(),
    );
    let mut cfg2 = Config::default();
    cfg2.contact_email = Some("scholar@example.edu".to_owned());
    let ft2 = build_service(
        net2.clone(),
        Arc::new(FakeDocApi::empty()),
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        cfg2,
    );
    let report2 = ft2
        .fulltext_report(doi, FulltextOpts::default(), &CallCx::default())
        .await
        .unwrap();
    assert!(
        net2.unmatched().is_empty(),
        "unmatched: {:?}",
        net2.unmatched()
    );
    assert!(
        net2.hits()
            .iter()
            .any(|h| h.url.contains("api.unpaywall.org"))
    );
    assert!(report2.notes.is_empty());
    assert!(!report2.result.available);
    assert_eq!(report2.result.why.as_deref(), Some("paywalled"));
    assert_eq!(report2.result.oa_status.as_deref(), Some("closed"));
}

// ---------------------------------------------------------------------------
// Caching (zero-network section reslicing), cancellation, and validation
// ---------------------------------------------------------------------------
#[tokio::test]
async fn cache_hit_reslices_sections_with_zero_extra_network_calls() {
    let net =
        Arc::new(ReplayNet::from_file(fixture_path("step5_arxiv_native/default.json")).unwrap());
    let store = Arc::new(MemoryStore::default());
    let ft = build_service(
        net.clone(),
        Arc::new(FakeDocApi::empty()),
        Arc::new(FakeFetchApi::empty()),
        store,
        Config::default(),
    );

    let mut opts1 = FulltextOpts::default();
    opts1.sections = Some(vec!["abstract".into()]);
    let r1 = ft
        .fulltext_report("2401.00001v1", opts1, &CallCx::default())
        .await
        .unwrap();
    assert!(r1.result.available);
    assert_eq!(net.hits().len(), 1);

    let mut opts2 = FulltextOpts::default();
    opts2.sections = Some(vec!["introduction".into()]);
    opts2.max_chars = Some(600);
    let r2 = ft
        .fulltext_report("2401.00001v1", opts2, &CallCx::default())
        .await
        .unwrap();
    assert!(r2.result.available);
    assert_eq!(r2.steps_tried, vec!["cache:hit"]);
    assert_eq!(
        net.hits().len(),
        1,
        "cache hit must perform 0 network requests"
    );
    assert!(r2.result.content.as_deref().unwrap().chars().count() <= 640);
}

#[tokio::test]
async fn cancellation_and_invalid_inputs_fail_fast_with_zero_network() {
    let net = Arc::new(ReplayNet::from_fixtures(vec![]).unwrap());
    let ft = build_service(
        net.clone(),
        Arc::new(FakeDocApi::empty()),
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        Config::default(),
    );

    let cx = CallCx::default();
    cx.cancel.cancel();
    let err = ft
        .fulltext("10.1038/nature12373", FulltextOpts::default(), &cx)
        .await
        .unwrap_err();
    assert!(matches!(err, FsmError::Timeout(0)));

    let mut bad_opts = FulltextOpts::default();
    bad_opts.max_chars = Some(0);
    let err = ft
        .fulltext("10.1038/nature12373", bad_opts, &CallCx::default())
        .await
        .unwrap_err();
    assert!(matches!(err, FsmError::InvalidInput(_)));

    let err = ft
        .fulltext(
            "https://sci-hub.se/10.1038/nature12373",
            FulltextOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, FsmError::PermissionDenied(_)));
    assert!(net.hits().is_empty());
}

// ---------------------------------------------------------------------------
// Guardrails, FetchApi OA HTML, medRxiv 10.64898, S2 429
// ---------------------------------------------------------------------------
#[tokio::test]
async fn openalex_paid_content_and_shadow_urls_are_filtered_out() {
    let lawful_pdf = "https://repo.example.edu/papers/safe.pdf";
    let paid_oa = "https://api.openalex.org/content/W123.pdf";
    let shadow_pdf = "https://sci-hub.se/10.1234/bad.pdf";
    let mut fix = fsm_core::fixture::Fixture::new("synthetic_guardrails", "guardrails", "");
    fix.exchanges = vec![
        make_exchange(
            "https://pmc.ncbi.nlm.nih.gov/tools/idconv/api/v1/articles/?format=json&ids=10.1234%2Fguardrail&tool=free-search-mcp",
            200,
            "application/json",
            r#"{"status":"ok","records":[{"status":"error"}]}"#,
            Some("application/json"),
        ),
        make_exchange(
            &format!("https://api.openalex.org/works/https://doi.org/10.1234/guardrail?{OA_SELECT_Q}"),
            200,
            "application/json",
            &serde_json::json!({
                "id": "https://openalex.org/W123456",
                "doi": "https://doi.org/10.1234/guardrail",
                "open_access": {"is_oa": true, "oa_status": "green", "oa_url": paid_oa},
                "best_oa_location": {"is_oa": true, "pdf_url": paid_oa, "landing_page_url": "https://repo.example.edu/123"},
                "locations": [
                    {"is_oa": true, "pdf_url": shadow_pdf},
                    {"is_oa": true, "pdf_url": lawful_pdf, "license": "cc-by"}
                ]
            })
            .to_string(),
            Some("application/json"),
        ),
        make_exchange(
            "https://api.semanticscholar.org/graph/v1/paper/DOI:10.1234/guardrail?fields=openAccessPdf%2CexternalIds%2Curl%2CisOpenAccess",
            404,
            "application/json",
            "{}",
            Some("application/json"),
        ),
    ];

    let net = Arc::new(ReplayNet::from_fixtures(vec![fix]).unwrap());
    let docs = Arc::new(
        FakeDocApi::empty()
            .with_pdf(paid_oa, "SHOULD NOT READ", "paid")
            .with_pdf(shadow_pdf, "SHOULD NOT READ", "shadow")
            .with_pdf(
                lawful_pdf,
                "Lawful Repo PDF",
                "Abstract\nLawful repository content.",
            ),
    );
    let ft = build_service(
        net.clone(),
        docs.clone(),
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        Config::default(),
    );

    let res = ft
        .fulltext(
            "10.1234/guardrail",
            FulltextOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(res.available);
    assert_eq!(res.url.as_deref(), Some(lawful_pdf));
    assert_eq!(
        docs.calls(),
        vec![lawful_pdf.to_owned()],
        "must skip both paid OpenAlex /content and shadow library URLs"
    );
}

#[tokio::test]
async fn oa_html_fallback_via_fetch_api_rejects_paywall_stub_and_accepts_article() {
    let stub_url = "https://publisher.example.org/stub";
    let oa_html_url = "https://oa.example.org/articles/good";

    let mut fix = fsm_core::fixture::Fixture::new("synthetic_oa_html", "html_fallback", "");
    fix.exchanges = vec![
        make_exchange(
            "https://pmc.ncbi.nlm.nih.gov/tools/idconv/api/v1/articles/?format=json&ids=10.5555%2Foahtml&tool=free-search-mcp",
            200,
            "application/json",
            r#"{"status":"ok","records":[{"status":"error"}]}"#,
            Some("application/json"),
        ),
        make_exchange(
            "https://api.unpaywall.org/v2/10.5555/oahtml",
            200,
            "application/json",
            &serde_json::json!({
                "doi": "10.5555/oahtml",
                "is_oa": true,
                "oa_status": "gold",
                "best_oa_location": {"url_for_pdf": null, "url_for_landing_page": stub_url, "license": "cc-by"},
                "oa_locations": [
                    {"url_for_pdf": null, "url_for_landing_page": oa_html_url, "license": "cc-by"}
                ]
            })
            .to_string(),
            Some("application/json"),
        ),
    ];

    let net = Arc::new(ReplayNet::from_fixtures(vec![fix]).unwrap());
    let long_article = format!(
        "Abstract\n{}\n\n1. Introduction\n{}\n\n2. Methods\n{}\n\n3. Results\n{}",
        "This open-access study evaluates climate models with reproducible datasets. ".repeat(4),
        "Regional climate projections require calibrated surface observations across hemispheres. "
            .repeat(5),
        "Measurements were calibrated across 42 stations with cross-validated regression. "
            .repeat(5),
        "Cross-validation error decreased by 18% relative to uncalibrated ensemble baselines. "
            .repeat(5),
    );
    let fetch = Arc::new(
        FakeFetchApi::empty()
            .with_page(
                stub_url,
                "Access Required",
                "Purchase PDF or sign in via Institutional access to read this article.",
            )
            .with_page(oa_html_url, "Open Climate Study", &long_article),
    );
    let mut cfg = Config::default();
    cfg.contact_email = Some("qa@example.edu".into());
    let ft = build_service(
        net,
        Arc::new(FakeDocApi::empty()),
        fetch.clone(),
        Arc::new(NullStore::new()),
        cfg,
    );

    let res = ft
        .fulltext(
            "10.5555/oahtml",
            FulltextOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(res.available);
    assert_eq!(res.format.as_deref(), Some("html5"));
    assert_eq!(res.url.as_deref(), Some(oa_html_url));
    assert_eq!(
        fetch.calls(),
        vec![stub_url.to_owned(), oa_html_url.to_owned()]
    );
}

#[tokio::test]
async fn medrxiv_2026_openrxiv_doi_prefix_and_store_enrichment_work() {
    let jats = r#"<article><front><article-meta><title-group><article-title>Clinical Trial Meta-Analysis</article-title></title-group><abstract><p>Summary of 18 randomized trials.</p></abstract></article-meta></front><body><sec><title>Methods</title><p>Random-effects DerSimonian-Laird model.</p></sec></body></article>"#;
    let mut fix = fsm_core::fixture::Fixture::new("synthetic_medrxiv", "2026_prefix", "");
    fix.exchanges = vec![
        make_exchange(
            "https://pmc.ncbi.nlm.nih.gov/tools/idconv/api/v1/articles/?format=json&ids=10.64898%2F2026.08.11.744243&tool=free-search-mcp",
            200,
            "application/json",
            r#"{"status":"ok","records":[{"status":"error"}]}"#,
            Some("application/json"),
        ),
        make_exchange(
            "https://api.biorxiv.org/details/biorxiv/10.64898/2026.08.11.744243",
            200,
            "application/json",
            r#"{"messages":[{"status":"no posts found"}],"collection":[]}"#,
            Some("application/json"),
        ),
        make_exchange(
            "https://api.biorxiv.org/details/medrxiv/10.64898/2026.08.11.744243",
            200,
            "application/json",
            r#"{"messages":[{"status":"ok"}],"collection":[{"doi":"10.64898/2026.08.11.744243","version":"1","license":"cc_by","jatsxml":"https://www.medrxiv.org/content/early/2026/08/19/2026.08.11.744243.source.xml"}]}"#,
            Some("application/json"),
        ),
        make_exchange(
            "https://www.medrxiv.org/content/early/2026/08/19/2026.08.11.744243.source.xml",
            200,
            "application/xml",
            jats,
            Some("application/xml"),
        ),
    ];

    let net = Arc::new(ReplayNet::from_fixtures(vec![fix]).unwrap());
    let ft = build_service(
        net.clone(),
        Arc::new(FakeDocApi::empty()),
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        Config::default(),
    );
    let res = ft
        .fulltext(
            "10.64898/2026.08.11.744243",
            FulltextOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(net.unmatched().is_empty());
    assert!(
        res.available
            && res.source.as_deref() == Some("medrxiv")
            && res.format.as_deref() == Some("jats")
    );

    // StoreApi::scholar_get enrichment: cached WorkIds.pmcid skips Step 1 idconv completely!
    let mut work = ScholarlyWork::new("Pre-cached Paper", "https://doi.org/10.9999/cached");
    work.ids.doi = Some("10.9999/cached".into());
    work.ids.pmcid = Some("PMC9876543".into());
    let store = Arc::new(MemoryStore::default());
    store
        .scholar_put("10.9999/cached", &work, 3600)
        .await
        .unwrap();

    let mut fix2 = fsm_core::fixture::Fixture::new("synthetic_epmc", "store_skip_idconv", "");
    fix2.exchanges = vec![make_exchange(
        "https://www.ebi.ac.uk/europepmc/webservices/rest/PMC9876543/fullTextXML",
        200,
        "application/xml",
        jats,
        Some("application/xml"),
    )];
    let net2 = Arc::new(ReplayNet::from_fixtures(vec![fix2]).unwrap());
    let ft2 = build_service(
        net2.clone(),
        Arc::new(FakeDocApi::empty()),
        Arc::new(FakeFetchApi::empty()),
        store,
        Config::default(),
    );
    let r2 = ft2
        .fulltext_report(
            "10.9999/cached",
            FulltextOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(net2.unmatched().is_empty());
    assert_eq!(r2.steps_tried, vec!["step2:europepmc_jats"]);
    assert!(r2.result.available);
}

#[tokio::test]
async fn s2_429_benches_until_midnight_and_unknown_paper_reports_unpaywall_hint() {
    let doi = "10.9999/unknown.404";
    let mut fix = fsm_core::fixture::Fixture::new("synthetic_s2_bench", "unknown", "");
    let mut exchanges = vec![
        make_exchange(
            "https://pmc.ncbi.nlm.nih.gov/tools/idconv/api/v1/articles/?format=json&ids=10.9999%2Funknown.404&tool=free-search-mcp",
            200,
            "application/json",
            r#"{"status":"ok","records":[{"status":"error"}]}"#,
            Some("application/json"),
        ),
        make_exchange(
            &format!("https://api.openalex.org/works/https://doi.org/{doi}?{OA_SELECT_Q}"),
            404,
            "application/json",
            "{}",
            Some("application/json"),
        ),
        make_exchange(
            "https://api.semanticscholar.org/graph/v1/paper/DOI:10.9999/unknown.404?fields=openAccessPdf%2CexternalIds%2Curl%2CisOpenAccess",
            429,
            "application/json",
            r#"{"message":"Too Many Requests"}"#,
            Some("application/json"),
        ),
    ];
    exchanges.extend(empty_repo_exchanges(doi));
    fix.exchanges = exchanges;

    let net = Arc::new(ReplayNet::from_fixtures(vec![fix]).unwrap());
    let ft = build_service(
        net.clone(),
        Arc::new(FakeDocApi::empty()),
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        Config::default(),
    );

    let r1 = ft
        .fulltext_report(doi, FulltextOpts::default(), &CallCx::default())
        .await
        .unwrap();
    assert!(
        net.unmatched().is_empty(),
        "unmatched: {:?}",
        net.unmatched()
    );
    assert!(!r1.result.available);
    assert_eq!(r1.result.oa_status.as_deref(), Some("unknown"));
    assert!(
        r1.result
            .why
            .as_deref()
            .unwrap()
            .contains("SEARCH_MCP_CONTACT_EMAIL")
    );
    assert!(
        r1.notes
            .iter()
            .any(|n| n.contains("semanticscholar") && n.contains("429"))
    );

    // Second call skips S2 because it is benched until UTC midnight!
    let s2_before = net
        .hits()
        .iter()
        .filter(|h| h.url.contains("api.semanticscholar.org"))
        .count();
    let r2 = ft
        .fulltext_report(doi, FulltextOpts::default(), &CallCx::default())
        .await
        .unwrap();
    let s2_after = net
        .hits()
        .iter()
        .filter(|h| h.url.contains("api.semanticscholar.org"))
        .count();
    assert_eq!(s2_before, s2_after, "benched S2 must not be hit again");
    assert!(r2.steps_tried.iter().any(|s| s == "step8b:s2:benched"));
}

// ===========================================================================
// Review Round 1 Regression & Missing Tests (B1, I3–I9, I11, M1, M3–M7, DOAJ)
// ===========================================================================

/// B1: Previewing a PDF with small `maxChars: 500` must NOT poison the 30-day cache!
#[tokio::test]
async fn review_b1_small_max_chars_does_not_poison_pdf_cache() {
    let pdf_url = "https://arxiv.org/pdf/1706.03762";
    let full_pdf_text = format!(
        "Abstract\n{}\n\n1. Introduction\n{}\n\n2. Methods\nScaled dot-product attention with multi-head projections.\n{}",
        "Sequence transduction with pure attention. ".repeat(15),
        "Recurrent models hinder parallelization across sequence positions. ".repeat(15),
        "Training details: Adam optimizer with warmup over 4000 steps. ".repeat(10),
    );
    assert!(full_pdf_text.chars().count() > 2_000);

    let docs = Arc::new(FakeDocApi::empty().with_pdf(
        pdf_url,
        "Attention Is All You Need",
        &full_pdf_text,
    ));
    let net = Arc::new(ReplayNet::from_file(fixture_path("step5_arxiv_pdf/default.json")).unwrap());
    let store = Arc::new(MemoryStore::default());
    let cfg = Config::default();
    let expected_doc_cap = cfg.max_document_chars;
    let ft = build_service(
        net.clone(),
        docs.clone(),
        Arc::new(FakeFetchApi::empty()),
        store.clone(),
        cfg,
    );

    // 1. Small preview call: maxChars = 500
    let mut opts1 = FulltextOpts::default();
    opts1.max_chars = Some(500);
    let r1 = ft
        .fulltext_report("1706.03762", opts1, &CallCx::default())
        .await
        .unwrap();
    assert!(r1.result.available);
    assert!(r1.result.content.as_deref().unwrap().chars().count() <= 550);
    assert_eq!(
        docs.call_lengths(),
        vec![(pdf_url.to_owned(), Some(expected_doc_cap))],
        "DocApi::read must always receive cfg.max_document_chars, never opts.max_chars"
    );

    // 2. Subsequent section query hits cache AND finds Methods (which lives past char 1200!)
    let mut opts2 = FulltextOpts::default();
    opts2.sections = Some(vec!["methods".into()]);
    let r2 = ft
        .fulltext_report("1706.03762", opts2, &CallCx::default())
        .await
        .unwrap();
    assert_eq!(r2.steps_tried, vec!["cache:hit"]);
    let methods_text = r2.result.content.as_deref().unwrap();
    assert!(
        methods_text.contains("Scaled dot-product attention"),
        "cache must hold full Methods section, got: {methods_text}"
    );

    // 3. Subsequent default call hits cache AND returns the full paper (> 1,500 chars)
    let r3 = ft
        .fulltext_report("1706.03762", FulltextOpts::default(), &CallCx::default())
        .await
        .unwrap();
    assert_eq!(r3.steps_tried, vec!["cache:hit"]);
    assert!(r3.result.content.as_deref().unwrap().chars().count() > 1_500);

    // 4. Truncated partial slice (truncated=true with returned_chars < max_document_chars) is NOT cached!
    let store2 = Arc::new(MemoryStore::default());
    let docs_trunc = Arc::new(FakeDocApi::empty().with_truncated_pdf(
        pdf_url,
        "Partial PDF",
        "Abstract\nOnly first page extracted.",
        50_000,
    ));
    let net2 =
        Arc::new(ReplayNet::from_file(fixture_path("step5_arxiv_pdf/default.json")).unwrap());
    let ft2 = build_service(
        net2,
        docs_trunc,
        Arc::new(FakeFetchApi::empty()),
        store2.clone(),
        Config::default(),
    );
    let r_trunc = ft2
        .fulltext_report("1706.03762", FulltextOpts::default(), &CallCx::default())
        .await
        .unwrap();
    assert!(r_trunc.result.available);
    assert!(
        store2.api_cache.lock().unwrap().is_empty(),
        "partial truncated DocSlice must NOT be written to api_cache"
    );
}

/// I3: Repository DSpace abstract/handle pages AND Wayback paywalled landing pages MUST be rejected!
#[tokio::test]
async fn review_i3_rejects_repository_handle_and_wayback_paywalled_landing() {
    // Part A (Probe I): Unpaywall green, PDF 503, 3.2k-char DSpace handle/abstract page -> REJECTED!
    let doi = "10.7777/dspace.handle";
    let repo_pdf = "https://repo.example.edu/bitstream/handle/1234/paper.pdf";
    let repo_handle = "https://repo.example.edu/handle/1234/5678";

    let mut fix = fsm_core::fixture::Fixture::new("synthetic_i3", "dspace_handle", "");
    let mut exchanges = vec![
        make_exchange(
            "https://pmc.ncbi.nlm.nih.gov/tools/idconv/api/v1/articles/?format=json&ids=10.7777%2Fdspace.handle&tool=free-search-mcp",
            200,
            "application/json",
            r#"{"status":"ok","records":[{"status":"error"}]}"#,
            Some("application/json"),
        ),
        make_exchange(
            "https://api.unpaywall.org/v2/10.7777/dspace.handle",
            200,
            "application/json",
            &serde_json::json!({
                "doi": doi,
                "is_oa": true,
                "oa_status": "green",
                "best_oa_location": {
                    "url_for_pdf": repo_pdf,
                    "url_for_landing_page": repo_handle,
                    "license": "cc-by"
                }
            })
            .to_string(),
            Some("application/json"),
        ),
        make_exchange(
            &format!("https://api.openalex.org/works/https://doi.org/{doi}?{OA_SELECT_Q}"),
            404,
            "application/json",
            "{}",
            Some("application/json"),
        ),
        make_exchange(
            &format!(
                "https://api.semanticscholar.org/graph/v1/paper/DOI:{doi}?fields=openAccessPdf%2CexternalIds%2Curl%2CisOpenAccess"
            ),
            404,
            "application/json",
            "{}",
            Some("application/json"),
        ),
    ];
    exchanges.extend(empty_repo_exchanges(doi));
    // Signposting HEAD 404 + Wayback CDX empty for failed OA URLs
    let mut head_req = fsm_core::fixture::FixtureRequest::new("HEAD", repo_handle);
    head_req.headers_subset.clear();
    exchanges.push(fsm_core::fixture::Exchange::new(
        head_req,
        fsm_core::fixture::FixtureResponse::from_bytes(404, vec![], b""),
    ));
    for target in [repo_pdf, repo_handle] {
        let enc_u = url::form_urlencoded::byte_serialize(target.as_bytes()).collect::<String>();
        exchanges.push(make_exchange(
            &format!(
                "https://web.archive.org/cdx/search/cdx?filter=statuscode%3A200&fl=timestamp%2Coriginal%2Cmimetype%2Cstatuscode&limit=-1&output=json&url={enc_u}"
            ),
            200,
            "application/json",
            "[]",
            Some("application/json"),
        ));
    }
    fix.exchanges = exchanges;

    let dspace_page = format!(
        "URI: http://hdl.handle.net/1234/5678\nCollections: Departmental Preprints\n{}\nAbstract\n{}",
        "Metadata field dc.contributor.author: Example Researcher; dc.date.issued: 2024. "
            .repeat(25),
        "This record summarizes the archived thesis abstract and bibliographic fields. ".repeat(18),
    );
    assert!(dspace_page.chars().count() > 2_500);

    let net = Arc::new(ReplayNet::from_fixtures(vec![fix]).unwrap());
    let docs = Arc::new(FakeDocApi::empty().with_err(repo_pdf, "503 unavailable"));
    let fetch = Arc::new(FakeFetchApi::empty().with_page(repo_handle, "DSpace Item", &dspace_page));
    let mut cfg = Config::default();
    cfg.contact_email = Some("qa@example.edu".into());
    let ft = build_service(net, docs, fetch, Arc::new(NullStore::new()), cfg);

    let rep = ft
        .fulltext_report(doi, FulltextOpts::default(), &CallCx::default())
        .await
        .unwrap();
    assert!(
        !rep.result.available,
        "DSpace handle/abstract page must NOT be accepted as full text"
    );

    // Part B (Probe K): Bare publisher URL with unknown OA status must NOT be archived by Wayback;
    // AND a 6k-char Wayback capture of a paywalled landing page MUST be rejected even if reached!
    let pub_url = "https://publisher.example.com/articles/paywalled-123";
    let mut fix_k = fsm_core::fixture::Fixture::new("synthetic_i3_k", "wayback_paywall", "");
    let head_pub = fsm_core::fixture::FixtureRequest::new("HEAD", pub_url);
    fix_k.exchanges = vec![fsm_core::fixture::Exchange::new(
        head_pub,
        fsm_core::fixture::FixtureResponse::from_bytes(200, vec![], b""),
    )];
    let net_k = Arc::new(ReplayNet::from_fixtures(vec![fix_k]).unwrap());
    let ft_k = build_service(
        net_k.clone(),
        Arc::new(FakeDocApi::empty()),
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        Config::default(),
    );
    let rep_k = ft_k
        .fulltext_report(pub_url, FulltextOpts::default(), &CallCx::default())
        .await
        .unwrap();
    assert!(!rep_k.result.available);
    assert!(
        net_k
            .hits()
            .iter()
            .all(|h| !h.url.contains("web.archive.org")),
        "Step 11 must NEVER query Wayback for a bare landing page without positive OA evidence"
    );

    // Even if a failed OA URL resolves via Wayback to a 6k-char paywalled HTML page, reject it!
    let wb_html = format!(
        "Abstract\n{}\nAccess through your institution or Buy this article.\nReferences\n{}",
        "This paper investigates quantum error correction thresholds. ".repeat(12),
        "[1] Author et al. Science 2022. ".repeat(180),
    );
    assert!(wb_html.chars().count() > 6_000);
    let archive_html_url =
        "https://web.archive.org/web/20250101000000id_/https://arxiv.org/pdf/1706.03762";
    let mut fix_wb = fsm_core::fixture::Fixture::new("synthetic_i3_wb", "wb_html_wall", "");
    fix_wb.exchanges = vec![
        make_exchange(
            "https://arxiv.org/html/1706.03762",
            404,
            "text/html",
            "Not Found",
            Some("text/html"),
        ),
        make_exchange(
            "https://ar5iv.labs.arxiv.org/html/1706.03762",
            404,
            "text/html",
            "Not Found",
            Some("text/html"),
        ),
        make_exchange(
            "https://api.semanticscholar.org/graph/v1/paper/ARXIV:1706.03762?fields=openAccessPdf%2CexternalIds%2Curl%2CisOpenAccess",
            404,
            "application/json",
            "{}",
            Some("application/json"),
        ),
        make_exchange(
            "https://api.core.ac.uk/v3/search/works?limit=3&q=arxivId%3A%221706.03762%22",
            200,
            "application/json",
            r#"{"totalHits":0,"results":[]}"#,
            Some("application/json"),
        ),
        fsm_core::fixture::Exchange::new(
            fsm_core::fixture::FixtureRequest::new("HEAD", "https://arxiv.org/abs/1706.03762"),
            fsm_core::fixture::FixtureResponse::from_bytes(200, vec![], b""),
        ),
        make_exchange(
            "https://web.archive.org/cdx/search/cdx?filter=statuscode%3A200&fl=timestamp%2Coriginal%2Cmimetype%2Cstatuscode&limit=-1&output=json&url=https%3A%2F%2Farxiv.org%2Fpdf%2F1706.03762",
            200,
            "application/json",
            r#"[["timestamp","original","mimetype","statuscode"],["20250101000000","https://arxiv.org/pdf/1706.03762","text/html","200"]]"#,
            Some("application/json"),
        ),
    ];
    let net_wb = Arc::new(ReplayNet::from_fixtures(vec![fix_wb]).unwrap());
    let docs_wb = Arc::new(FakeDocApi::empty().with_err("https://arxiv.org/pdf/1706.03762", "503"));
    let fetch_wb =
        Arc::new(FakeFetchApi::empty().with_page(archive_html_url, "Paywalled", &wb_html));
    let ft_wb = build_service(
        net_wb,
        docs_wb,
        fetch_wb,
        Arc::new(NullStore::new()),
        Config::default(),
    );
    let rep_wb = ft_wb
        .fulltext_report("1706.03762", FulltextOpts::default(), &CallCx::default())
        .await
        .unwrap();
    assert!(
        !rep_wb.result.available,
        "6k-char paywalled Wayback HTML capture must NEVER be accepted as full text"
    );
}

/// I4: S2's `{"url":"","status":"CLOSED"}` must NOT override positive Unpaywall green OA evidence!
#[tokio::test]
async fn review_i4_closed_signal_does_not_override_positive_oa_evidence() {
    let doi = "10.8888/green.503";
    let repo_pdf = "https://repo.example.edu/green-503.pdf";

    let mut fix = fsm_core::fixture::Fixture::new("synthetic_i4", "green_vs_closed", "");
    let mut exchanges = vec![
        make_exchange(
            "https://pmc.ncbi.nlm.nih.gov/tools/idconv/api/v1/articles/?format=json&ids=10.8888%2Fgreen.503&tool=free-search-mcp",
            200,
            "application/json",
            r#"{"status":"ok","records":[{"status":"error"}]}"#,
            Some("application/json"),
        ),
        make_exchange(
            "https://api.unpaywall.org/v2/10.8888/green.503",
            200,
            "application/json",
            &serde_json::json!({
                "doi": doi,
                "is_oa": true,
                "oa_status": "green",
                "best_oa_location": {"url_for_pdf": repo_pdf, "url_for_landing_page": null}
            })
            .to_string(),
            Some("application/json"),
        ),
        make_exchange(
            &format!("https://api.openalex.org/works/https://doi.org/{doi}?{OA_SELECT_Q}"),
            404,
            "application/json",
            "{}",
            Some("application/json"),
        ),
        make_exchange(
            &format!(
                "https://api.semanticscholar.org/graph/v1/paper/DOI:{doi}?fields=openAccessPdf%2CexternalIds%2Curl%2CisOpenAccess"
            ),
            200,
            "application/json",
            r#"{"paperId":"abc123","url":"https://www.semanticscholar.org/paper/abc123","isOpenAccess":false,"openAccessPdf":{"url":"","status":"CLOSED","license":null}}"#,
            Some("application/json"),
        ),
    ];
    exchanges.extend(empty_repo_exchanges(doi));
    let enc_u = url::form_urlencoded::byte_serialize(repo_pdf.as_bytes()).collect::<String>();
    exchanges.push(make_exchange(
        &format!(
            "https://web.archive.org/cdx/search/cdx?filter=statuscode%3A200&fl=timestamp%2Coriginal%2Cmimetype%2Cstatuscode&limit=-1&output=json&url={enc_u}"
        ),
        200,
        "application/json",
        "[]",
        Some("application/json"),
    ));
    fix.exchanges = exchanges;

    let net = Arc::new(ReplayNet::from_fixtures(vec![fix]).unwrap());
    let docs = Arc::new(FakeDocApi::empty().with_err(repo_pdf, "503 unavailable"));
    let mut cfg = Config::default();
    cfg.contact_email = Some("qa@example.edu".into());
    let ft = build_service(
        net.clone(),
        docs,
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        cfg,
    );

    let rep = ft
        .fulltext_report(doi, FulltextOpts::default(), &CallCx::default())
        .await
        .unwrap();
    assert!(
        net.unmatched().is_empty(),
        "unmatched: {:?}",
        net.unmatched()
    );
    assert!(!rep.result.available);
    assert_eq!(
        rep.result.oa_status.as_deref(),
        Some("green"),
        "S2 CLOSED must not overwrite Unpaywall green OA status"
    );
    assert_eq!(
        rep.result.why.as_deref(),
        Some("open-access copy listed but could not be retrieved")
    );
    assert_eq!(rep.result.url.as_deref(), Some(repo_pdf));
}

/// I5: OpenAlex 429 + S2 closed gives `landingUrl == "https://doi.org/<doi>"` (never semanticscholar.org).
#[tokio::test]
async fn review_i5_s2_url_never_becomes_landing_url_when_doi_exists() {
    let doi = "10.1126/science.aaa8415";
    let mut fix = fsm_core::fixture::Fixture::new("synthetic_i5", "oa429_s2_closed", "");
    let mut exchanges = vec![
        make_exchange(
            "https://pmc.ncbi.nlm.nih.gov/tools/idconv/api/v1/articles/?format=json&ids=10.1126%2Fscience.aaa8415&tool=free-search-mcp",
            200,
            "application/json",
            r#"{"status":"ok","records":[{"status":"error"}]}"#,
            Some("application/json"),
        ),
        make_exchange(
            &format!("https://api.openalex.org/works/https://doi.org/{doi}?{OA_SELECT_Q}"),
            429,
            "application/json",
            r#"{"error":"rate limited"}"#,
            Some("application/json"),
        ),
        make_exchange(
            &format!(
                "https://api.semanticscholar.org/graph/v1/paper/DOI:{doi}?fields=openAccessPdf%2CexternalIds%2Curl%2CisOpenAccess"
            ),
            200,
            "application/json",
            r#"{"paperId":"d422df8bff4e677a3077635db116679d25142bfc","url":"https://www.semanticscholar.org/paper/d422df8bff4e677a3077635db116679d25142bfc","isOpenAccess":false,"openAccessPdf":{"url":"","status":"CLOSED","license":null}}"#,
            Some("application/json"),
        ),
    ];
    exchanges.extend(empty_repo_exchanges(doi));
    fix.exchanges = exchanges;

    let net = Arc::new(ReplayNet::from_fixtures(vec![fix]).unwrap());
    let ft = build_service(
        net.clone(),
        Arc::new(FakeDocApi::empty()),
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        Config::default(),
    );

    let rep = ft
        .fulltext_report(doi, FulltextOpts::default(), &CallCx::default())
        .await
        .unwrap();
    assert!(net.unmatched().is_empty());
    assert!(!rep.result.available);
    assert_eq!(rep.result.why.as_deref(), Some("paywalled"));
    assert_eq!(rep.result.oa_status.as_deref(), Some("closed"));
    assert_eq!(
        rep.result.landing_url.as_deref(),
        Some("https://doi.org/10.1126/science.aaa8415"),
        "landing_url must be doi.org, NEVER semanticscholar.org"
    );
    assert!(
        net.hits()
            .iter()
            .all(|h| !(h.method == "HEAD" && h.url.contains("semanticscholar.org"))),
        "Signposting must NEVER send HEAD to semanticscholar.org"
    );
}

/// I6: Slow NetClient with short deadline returns `Ok(available:false)` with budget note;
/// DocApi `Timeout` before call deadline does NOT abort the chain.
#[tokio::test]
async fn review_i6_deadline_returns_budget_response_and_doc_timeout_does_not_abort() {
    // Part A1: Concurrency (I6a) — 400 ms/req with 2.5 s CallCx deadline completes the
    // entire 11-step chain in ~1.2 s (3 concurrent RTTs instead of 11 serial RTTs = 4.4 s)!
    let fast_enough_net = Arc::new(SlowNotFoundNet {
        delay: Duration::from_millis(400),
    });
    let ft_fast = build_service(
        fast_enough_net,
        Arc::new(FakeDocApi::empty()),
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        Config::default(),
    );
    let mut cx_fast = CallCx::default();
    cx_fast.deadline = Instant::now() + Duration::from_millis(2_500);
    let t_fast = Instant::now();
    let rep_fast = ft_fast
        .fulltext_report(
            "10.1234/concurrent.paper",
            FulltextOpts::default(),
            &cx_fast,
        )
        .await
        .expect("concurrent chain must finish well inside 2.5 s deadline");
    assert!(!rep_fast.result.available);
    assert!(
        t_fast.elapsed() < Duration::from_millis(2_000),
        "concurrent chain took {:?}, expected < 2.0 s",
        t_fast.elapsed()
    );

    // Part A2 (Probe F): 750 ms/req with 2.5 s CallCx deadline -> stops gracefully before deadline
    // and returns Ok(available:false) with `"time budget exhausted after ..."` (never Timeout(0))!
    let slow_net = Arc::new(SlowNotFoundNet {
        delay: Duration::from_millis(750),
    });
    let ft = build_service(
        slow_net,
        Arc::new(FakeDocApi::empty()),
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        Config::default(),
    );
    let mut cx = CallCx::default();
    cx.deadline = Instant::now() + Duration::from_millis(2_500);

    let t0 = Instant::now();
    let rep = ft
        .fulltext_report("10.1234/slow.paper", FulltextOpts::default(), &cx)
        .await
        .expect("deadline exhaustion must return graceful Ok(available:false), not Timeout(0)");
    assert!(
        t0.elapsed() < Duration::from_millis(2_450),
        "must stop cleanly before hard deadline, elapsed {:?}",
        t0.elapsed()
    );
    assert!(!rep.result.available);
    let why = rep.result.why.as_deref().unwrap_or("");
    assert!(
        why.starts_with("time budget exhausted after "),
        "expected budget exhausted message, got: {why:?}"
    );

    // Part B: DocApi returning FsmError::Timeout(15000) before overall call deadline does NOT abort chain!
    let doi = "10.4444/doc.timeout";
    let slow_pdf = "https://slow.example.edu/timeout.pdf";
    let fast_pdf = "https://fast.example.edu/ok.pdf";
    let mut fix = fsm_core::fixture::Fixture::new("synthetic_i6_doc", "doc_timeout", "");
    fix.exchanges = vec![
        make_exchange(
            "https://pmc.ncbi.nlm.nih.gov/tools/idconv/api/v1/articles/?format=json&ids=10.4444%2Fdoc.timeout&tool=free-search-mcp",
            200,
            "application/json",
            r#"{"status":"ok","records":[{"status":"error"}]}"#,
            Some("application/json"),
        ),
        make_exchange(
            "https://api.unpaywall.org/v2/10.4444/doc.timeout",
            200,
            "application/json",
            &serde_json::json!({
                "doi": doi,
                "is_oa": true,
                "oa_status": "green",
                "best_oa_location": {"url_for_pdf": slow_pdf}
            })
            .to_string(),
            Some("application/json"),
        ),
        make_exchange(
            &format!("https://api.openalex.org/works/https://doi.org/{doi}?{OA_SELECT_Q}"),
            200,
            "application/json",
            &serde_json::json!({
                "id": "https://openalex.org/W4444",
                "open_access": {"is_oa": true, "oa_status": "green"},
                "best_oa_location": {"is_oa": true, "pdf_url": fast_pdf}
            })
            .to_string(),
            Some("application/json"),
        ),
        make_exchange(
            &format!(
                "https://api.semanticscholar.org/graph/v1/paper/DOI:{doi}?fields=openAccessPdf%2CexternalIds%2Curl%2CisOpenAccess"
            ),
            404,
            "application/json",
            "{}",
            Some("application/json"),
        ),
    ];
    let net_b = Arc::new(ReplayNet::from_fixtures(vec![fix]).unwrap());
    let docs_b = Arc::new(FakeDocApi::empty().with_timeout(slow_pdf, 15_000).with_pdf(
        fast_pdf,
        "Fallback PDF",
        "Abstract\nRecovered after per-doc timeout.",
    ));
    let mut cfg = Config::default();
    cfg.contact_email = Some("qa@example.edu".into());
    let ft_b = build_service(
        net_b,
        docs_b,
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        cfg,
    );
    let rep_b = ft_b
        .fulltext_report(doi, FulltextOpts::default(), &CallCx::default())
        .await
        .expect("per-doc Timeout must not abort the chain");
    assert!(rep_b.result.available);
    assert_eq!(rep_b.result.source.as_deref(), Some("openalex"));
    assert_eq!(rep_b.result.url.as_deref(), Some(fast_pdf));
}

/// I7: A PMCID revealed by S2 OR Unpaywall with Europe PMC 404 falls through to Step 3 BioC!
#[tokio::test]
async fn review_i7_pmcid_from_s2_and_unpaywall_tries_bioc() {
    let bioc_body = r#"[{"source":"PMC","documents":[{"id":"PMC4221854","passages":[{"infons":{"section_type":"TITLE","type":"front"},"text":"Author Manuscript Title"},{"infons":{"section_type":"ABSTRACT","type":"abstract"},"text":"Author manuscript recovered from PMC BioC."}]}]}]"#;

    // Part A (Probe G): S2 reveals PMC4221854 -> EPMC 404 -> BioC 200!
    let doi_a = "10.1038/s2-bioc-test";
    let mut fix_a = fsm_core::fixture::Fixture::new("synthetic_i7_s2", "s2_bioc", "");
    fix_a.exchanges = vec![
        make_exchange(
            "https://pmc.ncbi.nlm.nih.gov/tools/idconv/api/v1/articles/?format=json&ids=10.1038%2Fs2-bioc-test&tool=free-search-mcp",
            200,
            "application/json",
            r#"{"status":"ok","records":[{"status":"error"}]}"#,
            Some("application/json"),
        ),
        make_exchange(
            &format!("https://api.openalex.org/works/https://doi.org/{doi_a}?{OA_SELECT_Q}"),
            404,
            "application/json",
            "{}",
            Some("application/json"),
        ),
        make_exchange(
            &format!(
                "https://api.semanticscholar.org/graph/v1/paper/DOI:{doi_a}?fields=openAccessPdf%2CexternalIds%2Curl%2CisOpenAccess"
            ),
            200,
            "application/json",
            r#"{"paperId":"abc","externalIds":{"PubMedCentral":"4221854"},"openAccessPdf":{"url":"https://example.org/p.pdf","status":"GREEN"}}"#,
            Some("application/json"),
        ),
        make_exchange(
            "https://www.ebi.ac.uk/europepmc/webservices/rest/PMC4221854/fullTextXML",
            404,
            "text/plain",
            "Not Found",
            Some("application/xml"),
        ),
        make_exchange(
            "https://www.ncbi.nlm.nih.gov/research/bionlp/RESTful/pmcoa.cgi/BioC_json/PMC4221854/unicode",
            200,
            "application/json",
            bioc_body,
            Some("application/json"),
        ),
    ];
    let net_a = Arc::new(ReplayNet::from_fixtures(vec![fix_a]).unwrap());
    let ft_a = build_service(
        net_a.clone(),
        Arc::new(FakeDocApi::empty()),
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        Config::default(),
    );
    let rep_a = ft_a
        .fulltext_report(doi_a, FulltextOpts::default(), &CallCx::default())
        .await
        .unwrap();
    assert!(net_a.unmatched().is_empty());
    assert!(rep_a.result.available);
    assert_eq!(rep_a.result.format.as_deref(), Some("bioc"));
    assert_eq!(rep_a.result.source.as_deref(), Some("pmc_bioc"));
    assert!(rep_a.steps_tried.iter().any(|s| s == "step3:pmc_bioc"));

    // Part B: Unpaywall reveals PMC4221854 in landing URL -> EPMC 404 -> BioC 200!
    let doi_b = "10.1038/unp-bioc-test";
    let mut fix_b = fsm_core::fixture::Fixture::new("synthetic_i7_unp", "unp_bioc", "");
    fix_b.exchanges = vec![
        make_exchange(
            "https://pmc.ncbi.nlm.nih.gov/tools/idconv/api/v1/articles/?format=json&ids=10.1038%2Funp-bioc-test&tool=free-search-mcp",
            200,
            "application/json",
            r#"{"status":"ok","records":[{"status":"error"}]}"#,
            Some("application/json"),
        ),
        make_exchange(
            &format!("https://api.unpaywall.org/v2/{doi_b}"),
            200,
            "application/json",
            r#"{"doi":"10.1038/unp-bioc-test","is_oa":true,"oa_status":"green","best_oa_location":{"url_for_landing_page":"https://pmc.ncbi.nlm.nih.gov/articles/PMC4221854/"}}"#,
            Some("application/json"),
        ),
        make_exchange(
            "https://www.ebi.ac.uk/europepmc/webservices/rest/PMC4221854/fullTextXML",
            404,
            "text/plain",
            "Not Found",
            Some("application/xml"),
        ),
        make_exchange(
            "https://www.ncbi.nlm.nih.gov/research/bionlp/RESTful/pmcoa.cgi/BioC_json/PMC4221854/unicode",
            200,
            "application/json",
            bioc_body,
            Some("application/json"),
        ),
    ];
    let net_b = Arc::new(ReplayNet::from_fixtures(vec![fix_b]).unwrap());
    let mut cfg_b = Config::default();
    cfg_b.contact_email = Some("qa@example.edu".into());
    let ft_b = build_service(
        net_b.clone(),
        Arc::new(FakeDocApi::empty()),
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        cfg_b,
    );
    let rep_b = ft_b
        .fulltext_report(doi_b, FulltextOpts::default(), &CallCx::default())
        .await
        .unwrap();
    assert!(net_b.unmatched().is_empty());
    assert!(rep_b.result.available);
    assert_eq!(rep_b.result.format.as_deref(), Some("bioc"));
    assert_eq!(rep_b.result.source.as_deref(), Some("pmc_bioc"));
}

/// I8: A bare publisher URL resolves its DOI via `FetchApi::structured` (`citation_doi`) and runs the chain!
#[tokio::test]
async fn review_i8_publisher_url_resolves_doi_via_fetch_structured() {
    let pub_url = "https://www.nature.com/articles/s41586-020-2649-2";
    let mut meta = StructuredMeta::new(pub_url);
    meta.meta_fallback
        .insert("citation_doi".to_owned(), "10.1038/nature12373".to_owned());

    let net = Arc::new(
        ReplayNet::from_file(fixture_path("step1_3_nature12373_bioc/default.json")).unwrap(),
    );
    let fetch = Arc::new(FakeFetchApi::empty().with_structured(pub_url, meta));
    let ft = build_service(
        net.clone(),
        Arc::new(FakeDocApi::empty()),
        fetch,
        Arc::new(NullStore::new()),
        Config::default(),
    );

    let rep = ft
        .fulltext_report(pub_url, FulltextOpts::default(), &CallCx::default())
        .await
        .unwrap();
    assert!(
        net.unmatched().is_empty(),
        "unmatched: {:?}",
        net.unmatched()
    );
    assert!(rep.result.available);
    assert_eq!(rep.result.id, "10.1038/nature12373");
    assert_eq!(rep.result.format.as_deref(), Some("bioc"));
    assert_eq!(rep.result.source.as_deref(), Some("pmc_bioc"));
    assert_eq!(rep.result.landing_url.as_deref(), Some(pub_url));
}

/// I9: 429 bench is shared bidirectionally between `ScholarCore` (W3a) and `ScholarFulltext` (W3c),
/// and persisted in `api_cache` under `scholar:bench:openalex`.
#[tokio::test]
async fn review_i9_429_bench_shared_bidirectionally_with_scholar_core() {
    // Direction 1: `scholar:bench:openalex` already in `api_cache` (written by ScholarCore / previous run)
    // -> ScholarFulltext skips OpenAlex immediately!
    let store1 = Arc::new(MemoryStore::default());
    let now = jiff::Timestamp::now().as_second();
    let until = next_utc_midnight(now);
    store1
        .api_cache_put(
            "scholar:bench:openalex",
            "scholar",
            until.to_string().as_bytes(),
            None,
            None,
            86_400,
        )
        .await
        .unwrap();

    let doi = "10.1038/nature12373";
    let pdf_url = "https://www.nature.com/articles/nature12373.pdf";
    let mut fix1 = fsm_core::fixture::Fixture::new("synthetic_i9_d1", "prebenched_oa", "");
    fix1.exchanges = vec![
        make_exchange(
            "https://pmc.ncbi.nlm.nih.gov/tools/idconv/api/v1/articles/?format=json&ids=10.1038%2Fnature12373&tool=free-search-mcp",
            200,
            "application/json",
            r#"{"status":"ok","records":[{"status":"error"}]}"#,
            Some("application/json"),
        ),
        make_exchange(
            &format!(
                "https://api.semanticscholar.org/graph/v1/paper/DOI:{doi}?fields=openAccessPdf%2CexternalIds%2Curl%2CisOpenAccess"
            ),
            200,
            "application/json",
            &serde_json::json!({
                "paperId": "a1",
                "openAccessPdf": {"url": pdf_url, "status": "BRONZE"}
            })
            .to_string(),
            Some("application/json"),
        ),
    ];
    let net1 = Arc::new(ReplayNet::from_fixtures(vec![fix1]).unwrap());
    let docs1 = Arc::new(FakeDocApi::empty().with_pdf(pdf_url, "Paper", "Abstract\nFrom S2."));
    let ft1 = build_service(
        net1.clone(),
        docs1,
        Arc::new(FakeFetchApi::empty()),
        store1,
        Config::default(),
    );
    let rep1 = ft1
        .fulltext_report(doi, FulltextOpts::default(), &CallCx::default())
        .await
        .unwrap();
    assert!(rep1.result.available);
    assert!(
        net1.hits()
            .iter()
            .all(|h| !h.url.contains("api.openalex.org")),
        "ScholarFulltext must honour scholar:bench:openalex written to api_cache"
    );
    assert!(
        rep1.steps_tried
            .iter()
            .any(|s| s == "step8a:openalex:benched")
    );

    // Direction 2: 429 inside ScholarFulltext IMMEDIATELY benches shared ScholarCore in-process
    // AND persists `scholar:bench:openalex` to api_cache!
    let store2 = Arc::new(MemoryStore::default());
    let net2 = Arc::new(ReplayNet::from_file(fixture_path("step8_s2/default.json")).unwrap());
    let deps2 = SourceDeps::new(net2.clone(), store2.clone(), Arc::new(Config::default()));
    let core2 = Arc::new(ScholarCore::new(Arc::new(Registry::new()), deps2.clone()));
    assert!(core2.benched_until(Provider::OpenAlex).await.is_none());

    let docs2 = Arc::new(FakeDocApi::empty().with_pdf(pdf_url, "Paper", "Abstract\nVia S2."));
    let ft2 = ScholarFulltext::new(core2.clone(), docs2, Arc::new(FakeFetchApi::empty()), deps2);
    let rep2 = ft2
        .fulltext_report(doi, FulltextOpts::default(), &CallCx::default())
        .await
        .unwrap();
    assert!(rep2.result.available);
    assert!(
        core2.benched_until(Provider::OpenAlex).await.is_some(),
        "OpenAlex 429 in ScholarFulltext must immediately bench ScholarCore in the same process"
    );
    assert!(
        store2
            .api_cache_get("scholar:bench:openalex")
            .await
            .unwrap()
            .is_some(),
        "OpenAlex 429 bench must be persisted under scholar:bench:openalex"
    );
}

/// I11: Default `max_chars` uses `cfg.max_content_chars` (50k) and compacts `sections` on huge PDFs.
#[tokio::test]
async fn review_i11_default_max_chars_caps_at_50k_without_doubling() {
    let pdf_url = "https://arxiv.org/pdf/1706.03762";
    let huge_pdf = format!(
        "Abstract\nShort overview.\n\n1. Introduction\n{}\n\n2. Methods\n{}\n\n3. Results\n{}",
        "Long introduction sentence covering prior theoretical work. ".repeat(850),
        "Detailed experimental methodology and instrumentation details. ".repeat(850),
        "Quantitative experimental findings across all benchmark suites. ".repeat(850),
    );
    assert!(huge_pdf.chars().count() > 140_000);

    let net = Arc::new(ReplayNet::from_file(fixture_path("step5_arxiv_pdf/default.json")).unwrap());
    let docs = Arc::new(FakeDocApi::empty().with_pdf(pdf_url, "Big Thesis", &huge_pdf));
    let ft = build_service(
        net,
        docs,
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        Config::default(),
    );

    let res = ft
        .fulltext("1706.03762", FulltextOpts::default(), &CallCx::default())
        .await
        .unwrap();
    assert!(res.available);
    let wire_json = serde_json::to_string(&res).unwrap();
    assert!(
        res.content.as_deref().unwrap().chars().count() <= 50_500,
        "default content cap must be ~50k"
    );
    assert!(
        wire_json.len() < 60_000,
        "serialized FulltextResult must not duplicate content into sections, got {} bytes",
        wire_json.len()
    );
    let secs = res.sections.as_ref().unwrap();
    assert_eq!(secs.len(), 4);
    assert!(secs.iter().all(|s| s.text.is_empty()));
}

/// M1 + I12: Live Europe PMC PMC3257301 preserves all References & CC-BY license;
/// live 10.1038/nature12373 resolves through live idconv -> PMC4221854 -> live BioC!
#[tokio::test]
async fn review_m1_live_europepmc_pmc3257301_and_live_nature12373_bioc() {
    // 1. Live Europe PMC PMC3257301 (no section filter, large max_chars so References aren't truncated)
    let net =
        Arc::new(ReplayNet::from_file(fixture_path("step1_2_pmc_jats/default.json")).unwrap());
    let ft = build_service(
        net.clone(),
        Arc::new(FakeDocApi::empty()),
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        Config::default(),
    );
    let mut opts = FulltextOpts::default();
    opts.sections = Some(vec!["references".into()]);
    opts.max_chars = Some(100_000);
    let rep = ft
        .fulltext_report("10.1371/journal.ppat.1002485", opts, &CallCx::default())
        .await
        .unwrap();
    assert!(net.unmatched().is_empty());
    assert!(rep.result.available);
    assert_eq!(rep.result.license.as_deref(), Some("CC-BY"));
    let refs_secs = rep.result.sections.as_ref().unwrap();
    assert!(
        !refs_secs.is_empty() && refs_secs[0].text.lines().count() >= 30,
        "real Europe PMC PMC3257301 must preserve body <ref-list> (~44 refs), got {refs_secs:?}"
    );

    // 2. Live 10.1038/nature12373 -> live idconv (PMC4221854) -> live NCBI BioC JSON!
    let net2 = Arc::new(
        ReplayNet::from_file(fixture_path("step1_3_nature12373_bioc/default.json")).unwrap(),
    );
    let ft2 = build_service(
        net2.clone(),
        Arc::new(FakeDocApi::empty()),
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        Config::default(),
    );
    let rep2 = ft2
        .fulltext_report(
            "10.1038/nature12373",
            FulltextOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(
        net2.unmatched().is_empty(),
        "unmatched: {:?}",
        net2.unmatched()
    );
    assert_eq!(
        rep2.steps_tried,
        vec!["step1:pmc_idconv", "step2:europepmc_jats", "step3:pmc_bioc"]
    );
    assert!(rep2.result.available);
    assert_eq!(rep2.result.format.as_deref(), Some("bioc"));
    assert_eq!(rep2.result.source.as_deref(), Some("pmc_bioc"));
    assert!(
        rep2.result
            .content
            .as_deref()
            .unwrap()
            .to_ascii_lowercase()
            .contains("thermometry")
    );
}

/// M3: Negative cache avoids repeat upstream fan-out on closed papers; CORE 429 benches CORE.
#[tokio::test]
async fn review_m3_negative_cache_and_core_429_bench() {
    // Part A: Repeat closed paper call hits negative cache with 0 network calls!
    let doi = "10.1126/science.aaa8415";
    let net = Arc::new(ReplayNet::from_file(fixture_path("paywalled/closed.json")).unwrap());
    let store = Arc::new(MemoryStore::default());
    let ft = build_service(
        net.clone(),
        Arc::new(FakeDocApi::empty()),
        Arc::new(FakeFetchApi::empty()),
        store.clone(),
        Config::default(),
    );
    let r1 = ft
        .fulltext_report(doi, FulltextOpts::default(), &CallCx::default())
        .await
        .unwrap();
    assert!(!r1.result.available && r1.result.why.as_deref() == Some("paywalled"));
    let hits_after_1 = net.hits().len();
    assert!(hits_after_1 >= 8);

    let r2 = ft
        .fulltext_report(doi, FulltextOpts::default(), &CallCx::default())
        .await
        .unwrap();
    assert_eq!(
        net.hits().len(),
        hits_after_1,
        "repeat closed-paper lookup must hit negative cache with 0 network calls"
    );
    assert_eq!(r2.steps_tried, vec!["cache:negative_hit"]);
    assert_eq!(r2.result.why.as_deref(), Some("paywalled"));
    assert_eq!(r2.result.oa_status.as_deref(), Some("closed"));

    // Part B: CORE v3 HTTP 429 benches CORE for subsequent calls!
    let doi_b = "10.5555/core.429";
    let mut fix_b = fsm_core::fixture::Fixture::new("synthetic_m3_core", "core_429", "");
    let enc_b = doi_b.replace('/', "%2F");
    fix_b.exchanges = vec![
        make_exchange(
            &format!(
                "https://pmc.ncbi.nlm.nih.gov/tools/idconv/api/v1/articles/?format=json&ids={enc_b}&tool=free-search-mcp"
            ),
            200,
            "application/json",
            r#"{"status":"ok","records":[{"status":"error"}]}"#,
            Some("application/json"),
        ),
        make_exchange(
            &format!("https://api.openalex.org/works/https://doi.org/{doi_b}?{OA_SELECT_Q}"),
            404,
            "application/json",
            "{}",
            Some("application/json"),
        ),
        make_exchange(
            &format!(
                "https://api.semanticscholar.org/graph/v1/paper/DOI:{doi_b}?fields=openAccessPdf%2CexternalIds%2Curl%2CisOpenAccess"
            ),
            404,
            "application/json",
            "{}",
            Some("application/json"),
        ),
        make_exchange(
            &format!("https://api.core.ac.uk/v3/search/works?limit=3&q=doi%3A%22{enc_b}%22"),
            429,
            "application/json",
            r#"{"message":"Too Many Requests"}"#,
            Some("application/json"),
        ),
        make_exchange(
            &format!("https://doaj.org/api/search/articles/doi:{enc_b}"),
            200,
            "application/json",
            r#"{"total":0,"results":[]}"#,
            Some("application/json"),
        ),
        make_exchange(
            &format!("https://api.crossref.org/works/{doi_b}"),
            404,
            "text/plain",
            "Not Found",
            Some("application/json"),
        ),
        make_exchange(
            &format!("https://api.datacite.org/dois/{doi_b}"),
            404,
            "application/vnd.api+json",
            "{}",
            Some("application/vnd.api+json"),
        ),
        make_exchange(
            &format!(
                "https://api.archives-ouvertes.fr/search/?fl=halId_s%2Curi_s%2CfileMain_s%2CopenAccess_bool%2CdoiId_s&q=doiId_s%3A%22{enc_b}%22&rows=1&wt=json"
            ),
            200,
            "application/json",
            r#"{"response":{"numFound":0,"docs":[]}}"#,
            Some("application/json"),
        ),
        make_exchange(
            &format!("https://zenodo.org/api/records?q=doi%3A%22{enc_b}%22&size=1"),
            200,
            "application/json",
            r#"{"hits":{"total":0,"hits":[]}}"#,
            Some("application/json"),
        ),
    ];
    let net_b = Arc::new(ReplayNet::from_fixtures(vec![fix_b]).unwrap());
    let ft_b = build_service(
        net_b.clone(),
        Arc::new(FakeDocApi::empty()),
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        Config::default(),
    );
    let _ = ft_b
        .fulltext_report(doi_b, FulltextOpts::default(), &CallCx::default())
        .await
        .unwrap();
    let core_hits_1 = net_b
        .hits()
        .iter()
        .filter(|h| h.url.contains("api.core.ac.uk"))
        .count();
    assert_eq!(core_hits_1, 1);

    let r_b2 = ft_b
        .fulltext_report(doi_b, FulltextOpts::default(), &CallCx::default())
        .await
        .unwrap();
    let core_hits_2 = net_b
        .hits()
        .iter()
        .filter(|h| h.url.contains("api.core.ac.uk"))
        .count();
    assert_eq!(core_hits_2, 1, "benched CORE v3 must not be called again");
    assert!(
        r_b2.steps_tried
            .iter()
            .any(|s| s == "step9:core_v3:benched")
    );
}

/// M4: When every upstream fails (503/network), return transient error message and do NOT negative-cache!
#[tokio::test]
async fn review_m4_all_upstreams_failed_reports_transient_error() {
    let store = Arc::new(MemoryStore::default());
    let ft = build_service(
        Arc::new(AlwaysErrorNet),
        Arc::new(FakeDocApi::empty()),
        Arc::new(FakeFetchApi::empty()),
        store.clone(),
        Config::default(),
    );
    let rep = ft
        .fulltext_report(
            "10.1234/all.down",
            FulltextOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(!rep.result.available);
    assert_eq!(
        rep.result.why.as_deref(),
        Some("all upstream lookups failed (network or rate limits); retry later")
    );
    assert!(
        store.api_cache.lock().unwrap().is_empty(),
        "all-upstream-failed responses must NOT be negative-cached"
    );
}

/// M6: Crossref TDM skips `intended-application: "similarity-checking"` and uses `text-mining`.
#[tokio::test]
async fn review_m6_crossref_skips_similarity_checking_links() {
    let doi = "10.3333/crossref.m6";
    let sim_pdf = "https://publisher.example.org/similarity-only.pdf";
    let tdm_pdf = "https://publisher.example.org/tdm-lawful.pdf";
    let enc = doi.replace('/', "%2F");

    let mut fix = fsm_core::fixture::Fixture::new("synthetic_m6", "crossref_sim_skip", "");
    let mut exchanges = vec![
        make_exchange(
            &format!(
                "https://pmc.ncbi.nlm.nih.gov/tools/idconv/api/v1/articles/?format=json&ids={enc}&tool=free-search-mcp"
            ),
            200,
            "application/json",
            r#"{"status":"ok","records":[{"status":"error"}]}"#,
            Some("application/json"),
        ),
        make_exchange(
            &format!("https://api.openalex.org/works/https://doi.org/{doi}?{OA_SELECT_Q}"),
            404,
            "application/json",
            "{}",
            Some("application/json"),
        ),
        make_exchange(
            &format!(
                "https://api.semanticscholar.org/graph/v1/paper/DOI:{doi}?fields=openAccessPdf%2CexternalIds%2Curl%2CisOpenAccess"
            ),
            404,
            "application/json",
            "{}",
            Some("application/json"),
        ),
    ];
    exchanges.extend(
        empty_repo_exchanges(doi)
            .into_iter()
            .filter(|e| !e.request.url.contains("api.crossref.org")),
    );
    exchanges.push(make_exchange(
        &format!("https://api.crossref.org/works/{doi}"),
        200,
        "application/json",
        &serde_json::json!({
            "message": {
                "URL": format!("https://doi.org/{doi}"),
                "link": [
                    {"URL": sim_pdf, "content-type": "application/pdf", "intended-application": "similarity-checking"},
                    {"URL": tdm_pdf, "content-type": "application/pdf", "intended-application": "text-mining"}
                ]
            }
        })
        .to_string(),
        Some("application/json"),
    ));
    fix.exchanges = exchanges;

    let net = Arc::new(ReplayNet::from_fixtures(vec![fix]).unwrap());
    let docs = Arc::new(
        FakeDocApi::empty()
            .with_pdf(sim_pdf, "SHOULD NOT FETCH", "Abstract\nSimilarity link.")
            .with_pdf(tdm_pdf, "Lawful TDM", "Abstract\nText-mining lawful link."),
    );
    let ft = build_service(
        net.clone(),
        docs.clone(),
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        Config::default(),
    );

    let rep = ft
        .fulltext_report(doi, FulltextOpts::default(), &CallCx::default())
        .await
        .unwrap();
    assert!(net.unmatched().is_empty());
    assert!(rep.result.available);
    assert_eq!(rep.result.url.as_deref(), Some(tdm_pdf));
    assert_eq!(
        docs.calls(),
        vec![tdm_pdf.to_owned()],
        "must skip similarity-checking link and only read text-mining link"
    );
}

/// M7: Non-PDF `DocSlice.format` (e.g. `"docx"`) maps to SPEC enum `"text"`.
#[tokio::test]
async fn review_m7_non_pdf_doc_slice_format_maps_to_text() {
    let url = "https://arxiv.org/pdf/1706.03762";
    let net = Arc::new(ReplayNet::from_file(fixture_path("step5_arxiv_pdf/default.json")).unwrap());
    let docs = Arc::new(FakeDocApi::empty().with_format_doc(
        url,
        "docx",
        "Converted Manuscript",
        "Abstract\nDocument extracted from Word/text backend.",
    ));
    let ft = build_service(
        net,
        docs,
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        Config::default(),
    );

    let res = ft
        .fulltext("1706.03762", FulltextOpts::default(), &CallCx::default())
        .await
        .unwrap();
    assert!(res.available);
    assert_eq!(
        res.format.as_deref(),
        Some("text"),
        "non-pdf DocSlice.format must map to 'text' per SPEC enum"
    );
}

/// Python oracle port (`tests/test_academic_engines.py::test_doaj_links_the_publisher_full_text_then_the_doi`):
/// Step 10a prefers `link[type="fulltext"]` and falls back cleanly to `https://doi.org/<doi>` landing URL
/// when a DOAJ record has no fulltext link.
#[tokio::test]
async fn test_doaj_links_the_publisher_full_text_then_the_doi() {
    // Record 1: DOAJ record has `link[type="fulltext"]` -> https://journal.example/article/1
    let doi1 = "10.1234/oss.2023.1";
    let ft_url = "https://journal.example/article/1";
    let enc1 = doi1.replace('/', "%2F");
    let mut fix1 = fsm_core::fixture::Fixture::new("synthetic_doaj_oracle_1", "doaj_ft", "");
    let mut ex1 = vec![
        make_exchange(
            &format!(
                "https://pmc.ncbi.nlm.nih.gov/tools/idconv/api/v1/articles/?format=json&ids={enc1}&tool=free-search-mcp"
            ),
            200,
            "application/json",
            r#"{"status":"ok","records":[{"status":"error"}]}"#,
            Some("application/json"),
        ),
        make_exchange(
            &format!("https://api.openalex.org/works/https://doi.org/{doi1}?{OA_SELECT_Q}"),
            404,
            "application/json",
            "{}",
            Some("application/json"),
        ),
        make_exchange(
            &format!(
                "https://api.semanticscholar.org/graph/v1/paper/DOI:{doi1}?fields=openAccessPdf%2CexternalIds%2Curl%2CisOpenAccess"
            ),
            404,
            "application/json",
            "{}",
            Some("application/json"),
        ),
    ];
    ex1.extend(
        empty_repo_exchanges(doi1)
            .into_iter()
            .filter(|e| !e.request.url.contains("doaj.org")),
    );
    ex1.push(make_exchange(
        &format!("https://doaj.org/api/search/articles/doi:{enc1}"),
        200,
        "application/json",
        &serde_json::json!({
            "total": 1,
            "results": [{
                "id": "0a1b2c3d",
                "bibjson": {
                    "title": "Machine learning for\n  soil mapping",
                    "identifier": [{"type": "doi", "id": doi1}],
                    "link": [
                        {"type": "other", "url": "https://journal.example/ignored"},
                        {"type": "fulltext", "url": ft_url}
                    ]
                }
            }]
        })
        .to_string(),
        Some("application/json"),
    ));
    fix1.exchanges = ex1;

    let article_body = format!(
        "Abstract\nWe compare random forests and gradient boosting.\n\n1. Introduction\n{}\n\n2. Methods\n{}",
        "Soil mapping combines remote sensing covariates with ground survey cores. ".repeat(5),
        "Random forest and gradient boosting models were evaluated via spatial CV. ".repeat(5),
    );
    let net1 = Arc::new(ReplayNet::from_fixtures(vec![fix1]).unwrap());
    let fetch1 = Arc::new(FakeFetchApi::empty().with_page(
        ft_url,
        "Machine learning for soil mapping",
        &article_body,
    ));
    let ft_svc1 = build_service(
        net1.clone(),
        Arc::new(FakeDocApi::empty()),
        fetch1.clone(),
        Arc::new(NullStore::new()),
        Config::default(),
    );
    let r1 = ft_svc1
        .fulltext_report(doi1, FulltextOpts::default(), &CallCx::default())
        .await
        .unwrap();
    assert!(net1.unmatched().is_empty());
    assert!(r1.result.available);
    assert_eq!(r1.result.source.as_deref(), Some("doaj"));
    assert_eq!(r1.result.url.as_deref(), Some(ft_url));
    assert_eq!(fetch1.calls(), vec![ft_url.to_owned()]);

    // Record 2: DOAJ record has NO `link` (only DOI `10.1234/x`) -> landingUrl falls back to https://doi.org/10.1234/x!
    let doi2 = "10.1234/x";
    let enc2 = doi2.replace('/', "%2F");
    let mut fix2 = fsm_core::fixture::Fixture::new("synthetic_doaj_oracle_2", "doaj_doi_only", "");
    let mut ex2 = vec![
        make_exchange(
            &format!(
                "https://pmc.ncbi.nlm.nih.gov/tools/idconv/api/v1/articles/?format=json&ids={enc2}&tool=free-search-mcp"
            ),
            200,
            "application/json",
            r#"{"status":"ok","records":[{"status":"error"}]}"#,
            Some("application/json"),
        ),
        make_exchange(
            &format!("https://api.openalex.org/works/https://doi.org/{doi2}?{OA_SELECT_Q}"),
            404,
            "application/json",
            "{}",
            Some("application/json"),
        ),
        make_exchange(
            &format!(
                "https://api.semanticscholar.org/graph/v1/paper/DOI:{doi2}?fields=openAccessPdf%2CexternalIds%2Curl%2CisOpenAccess"
            ),
            404,
            "application/json",
            "{}",
            Some("application/json"),
        ),
    ];
    ex2.extend(
        empty_repo_exchanges(doi2)
            .into_iter()
            .filter(|e| !e.request.url.contains("doaj.org")),
    );
    ex2.push(make_exchange(
        &format!("https://doaj.org/api/search/articles/doi:{enc2}"),
        200,
        "application/json",
        &serde_json::json!({
            "total": 1,
            "results": [{
                "id": "deadbeef",
                "bibjson": {
                    "title": "No full text link",
                    "identifier": [{"type": "doi", "id": doi2}]
                }
            }]
        })
        .to_string(),
        Some("application/json"),
    ));
    fix2.exchanges = ex2;

    let net2 = Arc::new(ReplayNet::from_fixtures(vec![fix2]).unwrap());
    let ft_svc2 = build_service(
        net2.clone(),
        Arc::new(FakeDocApi::empty()),
        Arc::new(FakeFetchApi::empty()),
        Arc::new(NullStore::new()),
        Config::default(),
    );
    let r2 = ft_svc2
        .fulltext_report(doi2, FulltextOpts::default(), &CallCx::default())
        .await
        .unwrap();
    assert!(net2.unmatched().is_empty());
    assert!(!r2.result.available);
    assert_eq!(
        r2.result.landing_url.as_deref(),
        Some("https://doi.org/10.1234/x")
    );
}
