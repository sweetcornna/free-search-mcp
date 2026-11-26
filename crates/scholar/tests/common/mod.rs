//! Shared test doubles: an in-memory store, scripted sources and a scripted
//! network. Nothing here touches the network or the disk (fixtures aside).
#![allow(dead_code)]

use async_trait::async_trait;
use fsm_core::fixture::{ReplayNet, request_url};
use fsm_core::{
    AuthMode, Availability, Bytes, CacheHit, Capability, Config, DownloadResult, FsmError,
    HealthRecord, HttpBytes, HttpRequest, NetClient, NetCx, OpDecl, PageResult, Param, ParamType,
    PoolRole, Registry, RetractionNotice, ReturnKind, ScholarlyWork, SearchResponse, Source,
    SourceDecl, SourceDeps, SourceLimits, SrcCx, SrcOutput, StoreApi, TransportProfile,
};
use fsm_scholar::ScholarCore;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 2026-11-25T13:30:00Z.
pub const NOW: i64 = 1_795_613_400;
/// 2026-11-26T00:00:00Z.
pub const NEXT_MIDNIGHT: i64 = 1_795_651_200;

pub fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// The recorded response body of one fixture file.
pub fn fixture_body(rel: &str) -> String {
    let text = std::fs::read_to_string(fixtures_dir().join(rel)).expect("fixture exists");
    let f = fsm_core::fixture::Fixture::from_json(&text).expect("fixture parses");
    f.exchanges[0]
        .response
        .body_text
        .clone()
        .expect("text body")
}

/// Every recorded fixture.
pub fn replay() -> Arc<ReplayNet> {
    Arc::new(ReplayNet::from_dir(fixtures_dir()).expect("fixtures load"))
}

pub fn config(keys: &[(&str, &str)]) -> Config {
    let mut cfg = Config::defaults_without_home();
    cfg.contact_email = None;
    cfg.api_keys.clear();
    for (k, v) in keys {
        cfg.api_keys.insert((*k).to_owned(), (*v).to_owned());
    }
    cfg
}

pub fn deps(net: Arc<dyn NetClient>, store: Arc<MemStore>, cfg: Config) -> SourceDeps {
    SourceDeps::new(net, store, Arc::new(cfg))
}

pub fn core_with(
    sources: Vec<Arc<dyn Source>>,
    net: Arc<dyn NetClient>,
    store: Arc<MemStore>,
    cfg: Config,
) -> ScholarCore {
    let mut reg = Registry::new();
    for s in sources {
        reg.register(s).expect("register");
    }
    ScholarCore::new(Arc::new(reg), deps(net, store, cfg)).with_clock(|| NOW)
}

/// In-memory store: `api_cache`, `scholar_works` and retractions are real;
/// everything else is inert.
#[derive(Default)]
pub struct MemStore {
    pub api: Mutex<HashMap<String, (Vec<u8>, u64)>>,
    pub works: Mutex<HashMap<String, ScholarlyWork>>,
    pub retractions: Mutex<HashMap<String, RetractionNotice>>,
    pub scholar_puts: AtomicUsize,
    pub api_get_delay: Mutex<Duration>,
    pub api_get_calls: AtomicUsize,
    pub api_get_fail_next: AtomicUsize,
}

impl MemStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn set_api_get_delay(&self, d: Duration) {
        *self.api_get_delay.lock().unwrap() = d;
    }

    pub fn api_keys(&self) -> Vec<String> {
        let mut k: Vec<String> = self.api.lock().unwrap().keys().cloned().collect();
        k.sort();
        k
    }

    pub fn api_get(&self, key: &str) -> Option<(Vec<u8>, u64)> {
        self.api.lock().unwrap().get(key).cloned()
    }
}

#[async_trait]
impl StoreApi for MemStore {
    async fn cache_search(&self, _q: &str, _limit: u32) -> Result<Vec<CacheHit>, FsmError> {
        Ok(Vec::new())
    }
    async fn get_page(&self, _url: &str, _a: Option<u64>) -> Result<Option<PageResult>, FsmError> {
        Ok(None)
    }
    async fn put_page(&self, _page: &PageResult, _ttl: u64) -> Result<(), FsmError> {
        Ok(())
    }
    async fn kv_put(&self, _s: &str, _k: &str, _v: &Value, _ttl: u64) -> Result<(), FsmError> {
        Ok(())
    }
    async fn kv_get(&self, _s: &str, _k: &str) -> Result<Option<Value>, FsmError> {
        Ok(None)
    }
    async fn search_cache_get(
        &self,
        _key: &str,
        _a: Option<u64>,
    ) -> Result<Option<(SearchResponse, u64)>, FsmError> {
        Ok(None)
    }
    async fn search_cache_put(
        &self,
        _key: &str,
        _r: &SearchResponse,
        _ttl: u64,
    ) -> Result<(), FsmError> {
        Ok(())
    }
    async fn api_cache_get(
        &self,
        key: &str,
    ) -> Result<Option<(Bytes, Option<String>, Option<String>)>, FsmError> {
        self.api_get_calls.fetch_add(1, Ordering::SeqCst);
        let delay = *self.api_get_delay.lock().unwrap();
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        if self
            .api_get_fail_next
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(FsmError::internal("store", "simulated api_cache_get error"));
        }
        Ok(self
            .api
            .lock()
            .unwrap()
            .get(key)
            .map(|(b, _)| (Bytes::from(b.clone()), None, None)))
    }
    async fn api_cache_put(
        &self,
        key: &str,
        _source_id: &str,
        body: &[u8],
        _etag: Option<&str>,
        _last_mod: Option<&str>,
        ttl_s: u64,
    ) -> Result<(), FsmError> {
        self.api
            .lock()
            .unwrap()
            .insert(key.to_owned(), (body.to_vec(), ttl_s));
        Ok(())
    }
    async fn scholar_get(&self, key: &str) -> Result<Option<ScholarlyWork>, FsmError> {
        Ok(self.works.lock().unwrap().get(key).cloned())
    }
    async fn scholar_put(
        &self,
        key: &str,
        work: &ScholarlyWork,
        _ttl_s: u64,
    ) -> Result<(), FsmError> {
        self.scholar_puts.fetch_add(1, Ordering::SeqCst);
        self.works
            .lock()
            .unwrap()
            .insert(key.to_owned(), work.clone());
        Ok(())
    }
    async fn retraction_lookup(&self, doi: &str) -> Result<Option<RetractionNotice>, FsmError> {
        Ok(self
            .retractions
            .lock()
            .unwrap()
            .get(&doi.to_ascii_lowercase())
            .cloned())
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
        _started: i64,
        _finished: Option<i64>,
    ) -> Result<(), FsmError> {
        Ok(())
    }
    async fn health_load_all(&self) -> Result<Vec<HealthRecord>, FsmError> {
        Ok(Vec::new())
    }
    async fn health_save(&self, _rec: &HealthRecord) -> Result<(), FsmError> {
        Ok(())
    }
    async fn download_record(&self, _rec: &DownloadResult, _ttl: u64) -> Result<(), FsmError> {
        Ok(())
    }
}

/// One call a [`FakeSource`] received.
#[derive(Debug, Clone)]
pub struct SeenCall {
    pub op: String,
    pub params: Value,
    pub api_key: Option<String>,
    pub timeout: Duration,
}

#[derive(Clone)]
enum FailKind {
    Upstream500(String),
    RateLimited(Option<u64>),
    Upstream429,
}

/// A scripted scholarly source with a manifest-like `search` op.
pub struct FakeSource {
    decl: SourceDecl,
    works: Vec<ScholarlyWork>,
    delay: Duration,
    fail: Option<FailKind>,
    pub calls: Mutex<Vec<SeenCall>>,
}

impl FakeSource {
    pub fn new(id: &str) -> Self {
        let op = OpDecl::new("search", "search", ReturnKind::Works)
            .with_param(Param::new("query", ParamType::String, "q").required())
            .with_param(Param::new("limit", ParamType::Int, "n").with_range(1.0, 20.0));
        let mut decl = SourceDecl::new(id, id, "fake")
            .with_scopes(&["academic"])
            .with_caps(&[Capability::Search])
            .with_op(op);
        decl.limits = SourceLimits::new(1.0, 1, 1, 10_000, 0);
        Self {
            decl,
            works: Vec::new(),
            delay: Duration::ZERO,
            fail: None,
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Replace the declared ops.
    pub fn op(mut self, op: OpDecl) -> Self {
        self.decl.ops = vec![op];
        self
    }

    pub fn caps(mut self, caps: &[Capability]) -> Self {
        self.decl.caps = caps.to_vec();
        self
    }

    pub fn scopes(mut self, scopes: &[&str]) -> Self {
        self.decl.scopes = scopes.iter().map(|s| (*s).to_owned()).collect();
        self
    }

    pub fn pool_role(mut self, role: PoolRole) -> Self {
        self.decl.pool_role = role;
        self
    }

    pub fn limits(mut self, limits: SourceLimits) -> Self {
        self.decl.limits = limits;
        self
    }

    pub fn key(mut self, env: &str) -> Self {
        self.decl.auth = AuthMode::RequiredKey { env: env.into() };
        self
    }

    /// Works keyless, routes only with a key (matching real `openalex.toml`,
    /// `semanticscholar.toml`, `core_ac.toml`, `google_books.toml`).
    pub fn optional_key(mut self, env: &str) -> Self {
        self.decl.auth = AuthMode::OptionalKey {
            env: env.into(),
            route_requires_key: true,
        };
        self
    }

    /// Optional key with `route_requires_key: false` (to test CORE's explicit id guard).
    pub fn optional_key_auto(mut self, env: &str) -> Self {
        self.decl.auth = AuthMode::OptionalKey {
            env: env.into(),
            route_requires_key: false,
        };
        self
    }

    pub fn works(mut self, works: Vec<ScholarlyWork>) -> Self {
        self.works = works;
        self
    }

    pub fn delay(mut self, d: Duration) -> Self {
        self.delay = d;
        self
    }

    pub fn failing(mut self, why: &str) -> Self {
        self.fail = Some(FailKind::Upstream500(why.into()));
        self
    }

    pub fn failing_rate_limited(mut self, retry_after_s: Option<u64>) -> Self {
        self.fail = Some(FailKind::RateLimited(retry_after_s));
        self
    }

    pub fn failing_429(mut self) -> Self {
        self.fail = Some(FailKind::Upstream429);
        self
    }

    pub fn arc(self) -> Arc<Self> {
        Arc::new(self)
    }

    pub fn seen(&self) -> Vec<SeenCall> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl Source for FakeSource {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    fn availability(&self, cfg: &Config) -> Availability {
        match &self.decl.auth {
            AuthMode::RequiredKey { env } if !cfg.api_keys.contains_key(env) => {
                Availability::NeedsKey { env: env.clone() }
            }
            _ => Availability::Available,
        }
    }

    async fn call(&self, op: &str, params: Value, cx: &SrcCx) -> Result<SrcOutput, FsmError> {
        self.calls.lock().unwrap().push(SeenCall {
            op: op.to_owned(),
            params,
            api_key: cx.api_key.clone(),
            timeout: cx.timeout,
        });
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        match &self.fail {
            Some(FailKind::Upstream500(why)) => Err(FsmError::upstream(
                self.decl.id.clone(),
                Some(500),
                why.clone(),
            )),
            Some(FailKind::RateLimited(retry_after_s)) => Err(FsmError::RateLimited {
                source_id: self.decl.id.clone(),
                retry_after_s: *retry_after_s,
            }),
            Some(FailKind::Upstream429) => Err(FsmError::upstream(
                self.decl.id.clone(),
                Some(429),
                "Too Many Requests",
            )),
            None => Ok(SrcOutput::Works(self.works.clone())),
        }
    }
}

pub fn as_sources(v: Vec<Arc<FakeSource>>) -> Vec<Arc<dyn Source>> {
    v.into_iter().map(|s| s as Arc<dyn Source>).collect()
}

/// A work as a search source would return it.
pub fn work(title: &str, year: Option<u16>, first_author: Option<&str>) -> ScholarlyWork {
    let mut w = ScholarlyWork::new(title, "");
    w.year = year;
    w.work_type = "article".into();
    w.authors = first_author
        .into_iter()
        .map(fsm_core::AuthorRef::new)
        .collect();
    w
}

/// Full request metadata captured by [`ScriptNet`].
#[derive(Debug, Clone)]
pub struct CapturedReq {
    pub redacted_url: String,
    pub raw_query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub redact_query_keys: Vec<String>,
    pub host_bucket: Option<String>,
    pub rps: Option<f64>,
}

type Responder = Box<dyn Fn(&str) -> (Duration, u16, String) + Send + Sync>;

/// A network that answers by URL after a (virtual-time) delay, counting
/// requests in flight and capturing `HttpRequest` / `NetCx` fields.
pub struct ScriptNet {
    respond: Responder,
    in_flight: AtomicUsize,
    pub max_in_flight: AtomicUsize,
    pub urls: Mutex<Vec<String>>,
    pub captured: Mutex<Vec<CapturedReq>>,
}

impl ScriptNet {
    pub fn new(f: impl Fn(&str) -> (Duration, u16, String) + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            respond: Box::new(f),
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
            urls: Mutex::new(Vec::new()),
            captured: Mutex::new(Vec::new()),
        })
    }

    pub fn requests(&self) -> Vec<String> {
        self.urls.lock().unwrap().clone()
    }

    pub fn captured(&self) -> Vec<CapturedReq> {
        self.captured.lock().unwrap().clone()
    }
}

#[async_trait]
impl NetClient for ScriptNet {
    async fn send(
        &self,
        req: HttpRequest,
        _p: TransportProfile,
        cx: &NetCx,
    ) -> Result<HttpBytes, FsmError> {
        let url = request_url(&req)?.to_string();
        self.urls.lock().unwrap().push(url.clone());
        self.captured.lock().unwrap().push(CapturedReq {
            redacted_url: url.clone(),
            raw_query: req.query.clone(),
            headers: req.headers.clone(),
            redact_query_keys: req.redact_query_keys.clone(),
            host_bucket: cx.host_bucket.clone(),
            rps: cx.rps,
        });
        let (delay, status, body) = (self.respond)(&url);
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(now, Ordering::SeqCst);
        let out = tokio::select! {
            () = tokio::time::sleep(delay) => match body.as_str() {
                // Local GCRA wait from fsm-net HttpClient::send (NOT an upstream 429).
                "!rate-limited" => Err(FsmError::RateLimited {
                    source_id: req.source_id.clone().unwrap_or_default(),
                    retry_after_s: Some(1),
                }),
                "!upstream-429" => Err(FsmError::upstream(
                    req.source_id.clone().unwrap_or_default(),
                    Some(429),
                    "Too Many Requests",
                )),
                "!truncated" => {
                    let mut resp = HttpBytes::new(url, status, Vec::new(), b"{\"partial\":true".to_vec());
                    resp.truncated = true;
                    Ok(resp)
                }
                "!timeout" => Err(FsmError::Timeout(1)),
                _ => Ok(HttpBytes::new(url, status, Vec::new(), body.into_bytes())),
            },
            () = cx.cancel.cancelled() => Err(FsmError::Timeout(0)),
        };
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        out
    }

    fn check_url_offline(&self, _url: &str) -> Result<(), FsmError> {
        Ok(())
    }
}
