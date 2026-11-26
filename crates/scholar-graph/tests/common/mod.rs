#![allow(dead_code)]

use async_trait::async_trait;
use fsm_core::{
    CacheHit, CallCx, CatalogDetail, CatalogHit, CatalogQuery, Config, DownloadResult, FsmError,
    HealthRecord, HttpBytes, HttpRequest, NetClient, NetCx, PageResult, ProtocolOutput, Registry,
    RetractionNotice, ScholarlyWork, SearchResponse, SourceDeps, SourceOutput, SourcesApi,
    StoreApi, TransportProfile,
};
use fsm_scholar::ScholarCore;
use fsm_scholar_graph::ScholarGraph;
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

pub type SyncStateRow = (Option<String>, i64, Option<i64>);
pub type NetFuture = futures::future::BoxFuture<'static, Result<HttpBytes, FsmError>>;
pub type BoxNetHandler = Box<dyn Fn(HttpRequest, NetCx) -> NetFuture + Send + Sync>;

/// In-memory `StoreApi` implementing retraction storage, sync state, and scholar cache.
#[derive(Default)]
pub struct MemoryStore {
    retractions: Mutex<HashMap<String, RetractionNotice>>,
    sync_state: Mutex<HashMap<String, SyncStateRow>>,
    scholar: Mutex<HashMap<String, ScholarlyWork>>,
    replace_batches: Mutex<usize>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_retraction(&self, norm_doi: &str, notice: RetractionNotice) {
        self.retractions
            .lock()
            .unwrap()
            .insert(norm_doi.to_ascii_lowercase(), notice);
    }

    pub fn set_sync_state(
        &self,
        name: &str,
        etag: Option<String>,
        started_at: i64,
        finished_at: Option<i64>,
    ) {
        self.sync_state
            .lock()
            .unwrap()
            .insert(name.to_owned(), (etag, started_at, finished_at));
    }

    pub fn get_sync_state(&self, name: &str) -> Option<(Option<String>, i64, Option<i64>)> {
        self.sync_state.lock().unwrap().get(name).cloned()
    }

    pub fn insert_work(&self, key: &str, work: ScholarlyWork) {
        self.scholar.lock().unwrap().insert(key.to_owned(), work);
    }

    pub fn retraction_count(&self) -> usize {
        self.retractions.lock().unwrap().len()
    }

    pub fn replace_batch_calls(&self) -> usize {
        *self.replace_batches.lock().unwrap()
    }
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
        _key: &str,
    ) -> Result<Option<(bytes::Bytes, Option<String>, Option<String>)>, FsmError> {
        Ok(None)
    }
    async fn api_cache_put(
        &self,
        _key: &str,
        _source_id: &str,
        _body: &[u8],
        _etag: Option<&str>,
        _last_mod: Option<&str>,
        _ttl_s: u64,
    ) -> Result<(), FsmError> {
        Ok(())
    }
    async fn scholar_get(
        &self,
        canonical_key_or_pid: &str,
    ) -> Result<Option<ScholarlyWork>, FsmError> {
        Ok(self
            .scholar
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
        self.scholar
            .lock()
            .unwrap()
            .insert(canonical_key.to_owned(), work.clone());
        Ok(())
    }
    async fn retraction_lookup(
        &self,
        norm_doi: &str,
    ) -> Result<Option<RetractionNotice>, FsmError> {
        Ok(self
            .retractions
            .lock()
            .unwrap()
            .get(&norm_doi.to_ascii_lowercase())
            .cloned())
    }
    async fn retractions_replace_batch(
        &self,
        rows: Vec<(String, RetractionNotice)>,
        _etag: Option<String>,
    ) -> Result<(), FsmError> {
        let mut map = self.retractions.lock().unwrap();
        map.clear();
        for (doi, notice) in rows {
            map.insert(doi.to_ascii_lowercase(), notice);
        }
        *self.replace_batches.lock().unwrap() += 1;
        Ok(())
    }
    async fn sync_state_get(
        &self,
        name: &str,
    ) -> Result<Option<(Option<String>, i64, Option<i64>)>, FsmError> {
        Ok(self.sync_state.lock().unwrap().get(name).cloned())
    }
    async fn sync_state_mark(
        &self,
        name: &str,
        etag: Option<&str>,
        started_at: i64,
        finished_at: Option<i64>,
    ) -> Result<(), FsmError> {
        self.sync_state.lock().unwrap().insert(
            name.to_owned(),
            (etag.map(str::to_owned), started_at, finished_at),
        );
        Ok(())
    }
    async fn health_load_all(&self) -> Result<Vec<HealthRecord>, FsmError> {
        Ok(Vec::new())
    }
    async fn health_save(&self, _rec: &HealthRecord) -> Result<(), FsmError> {
        Ok(())
    }
    async fn download_record(&self, _rec: &DownloadResult, _ttl_s: u64) -> Result<(), FsmError> {
        Ok(())
    }
}

/// Replayable mock `SourcesApi` capturing `(op_id, params)` invocations,
/// with optional manifest-style `limit` bounds validation and catalog details.
#[derive(Default)]
pub struct MockSources {
    handlers: Mutex<HashMap<String, Result<Vec<Value>, String>>>,
    limit_bounds: Mutex<HashMap<String, (u64, u64)>>,
    catalog: Mutex<HashMap<String, CatalogDetail>>,
    calls: Mutex<Vec<(String, Value)>>,
}

impl MockSources {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_ok(&self, op_id: &str, items: Vec<Value>) {
        self.handlers
            .lock()
            .unwrap()
            .insert(op_id.to_owned(), Ok(items));
    }

    pub fn on_err(&self, op_id: &str, msg: &str) {
        self.handlers
            .lock()
            .unwrap()
            .insert(op_id.to_owned(), Err(msg.to_owned()));
    }

    /// Enforce `[min, max]` bounds on `params["limit"]` for `op_id`, returning
    /// `FsmError::InvalidInput` just like `fsm-sources`'s `manifest_source.rs`.
    pub fn set_limit_bounds(&self, op_id: &str, min: u64, max: u64) {
        self.limit_bounds
            .lock()
            .unwrap()
            .insert(op_id.to_owned(), (min, max));
    }

    pub fn set_catalog_detail(&self, id: &str, detail: CatalogDetail) {
        self.catalog.lock().unwrap().insert(id.to_owned(), detail);
    }

    pub fn calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl SourcesApi for MockSources {
    async fn call(
        &self,
        op_id: &str,
        params: Value,
        _cx: &CallCx,
    ) -> Result<SourceOutput, FsmError> {
        self.calls
            .lock()
            .unwrap()
            .push((op_id.to_owned(), params.clone()));
        if let Some((min, max)) = self.limit_bounds.lock().unwrap().get(op_id).copied()
            && let Some(lim) = params.get("limit").and_then(Value::as_u64)
            && (lim < min || lim > max)
        {
            return Err(FsmError::invalid(format!(
                "{op_id}: limit {lim} outside manifest bounds {min}..={max}"
            )));
        }
        let entry = self.handlers.lock().unwrap().get(op_id).cloned();
        let (src, op) = op_id.split_once('.').unwrap_or((op_id, "search"));
        match entry {
            Some(Ok(items)) => {
                let mut out = SourceOutput::new(src, op);
                out.items = items;
                Ok(out)
            }
            Some(Err(msg)) => Err(FsmError::upstream(src, Some(502), msg)),
            None => Err(FsmError::invalid(format!("unknown source op '{op_id}'"))),
        }
    }

    async fn protocol(
        &self,
        kind: &str,
        base_url: &str,
        op: &str,
        _params: Value,
        _cx: &CallCx,
    ) -> Result<ProtocolOutput, FsmError> {
        Ok(ProtocolOutput::new(kind, base_url, op))
    }

    async fn catalog_search(&self, _q: CatalogQuery) -> Result<Vec<CatalogHit>, FsmError> {
        Ok(Vec::new())
    }

    async fn catalog_describe(&self, id: &str) -> Result<CatalogDetail, FsmError> {
        if let Some(d) = self.catalog.lock().unwrap().get(id).cloned() {
            return Ok(d);
        }
        Ok(CatalogDetail::new(CatalogHit::new(id, "source", "mock")))
    }
}

/// Programmable offline `NetClient` that records every outgoing `HttpRequest`.
pub struct FnNet<F = BoxNetHandler>
where
    F: Fn(HttpRequest, NetCx) -> NetFuture + Send + Sync,
{
    handler: F,
    requests: Mutex<Vec<HttpRequest>>,
}

impl<F> FnNet<F>
where
    F: Fn(HttpRequest, NetCx) -> NetFuture + Send + Sync,
{
    pub fn new(handler: F) -> Self {
        Self {
            handler,
            requests: Mutex::new(Vec::new()),
        }
    }

    pub fn requests(&self) -> Vec<HttpRequest> {
        self.requests.lock().unwrap().clone()
    }

    pub fn urls(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| {
                fsm_core::fixture::request_url(r).map_or_else(|_| r.url.clone(), |u| u.to_string())
            })
            .collect()
    }

    pub fn clear(&self) {
        self.requests.lock().unwrap().clear();
    }
}

#[async_trait]
impl<F> NetClient for FnNet<F>
where
    F: Fn(HttpRequest, NetCx) -> NetFuture + Send + Sync,
{
    async fn send(
        &self,
        req: HttpRequest,
        _profile: TransportProfile,
        cx: &NetCx,
    ) -> Result<HttpBytes, FsmError> {
        if cx.cancel.is_cancelled() {
            return Err(FsmError::Timeout(0));
        }
        self.requests.lock().unwrap().push(req.clone());
        (self.handler)(req, cx.clone()).await
    }

    fn check_url_offline(&self, _url: &str) -> Result<(), FsmError> {
        Ok(())
    }
}

pub fn json_ok(url: &str, val: &Value) -> HttpBytes {
    HttpBytes::new(
        url,
        200,
        vec![("content-type".to_owned(), "application/json".to_owned())],
        serde_json::to_vec(val).unwrap(),
    )
}

pub fn text_ok(url: &str, content_type: &str, body: &str) -> HttpBytes {
    HttpBytes::new(
        url,
        200,
        vec![("content-type".to_owned(), content_type.to_owned())],
        body.as_bytes().to_vec(),
    )
}

pub fn status_resp(url: &str, status: u16, body: &str) -> HttpBytes {
    HttpBytes::new(url, status, Vec::new(), body.as_bytes().to_vec())
}

pub fn test_config(cache_dir: &Path, offline_sync: bool) -> Config {
    let mut cfg = Config::defaults_without_home();
    cfg.cache_dir = cache_dir.to_path_buf();
    cfg.download_dir = cache_dir.join("downloads");
    cfg.offline_sync = offline_sync;
    cfg.contact_email = None;
    cfg
}

pub fn make_scholar_graph(
    net: Arc<dyn NetClient>,
    store: Arc<dyn StoreApi>,
    sources: Arc<dyn SourcesApi>,
    cfg: Config,
) -> ScholarGraph {
    let deps = SourceDeps::new(net, store, Arc::new(cfg));
    let core = Arc::new(ScholarCore::new(Arc::new(Registry::new()), deps.clone()));
    ScholarGraph::new(core, sources, deps)
}
