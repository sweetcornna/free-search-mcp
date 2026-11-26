//! Shared offline test doubles for `fsm-extract` integration tests.
#![allow(dead_code)]

use async_trait::async_trait;
use fsm_core::config::Config;
use fsm_core::url_canon::host_of;
use fsm_core::{
    Bytes, CacheHit, CallCx, DocApi, DocOutline, DocSlice, DownloadResult, FsmError, HealthRecord,
    HttpBytes, HttpRequest, NetClient, NetCx, PageResult, RetractionNotice, ScholarlyWork,
    SearchApi, SearchRequest, SearchResponse, SourceDeps, StoreApi, TransportProfile,
};
use fsm_extract::FetchService;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use url::Url;

/// Offline SSRF layers 1–3 checker for test doubles (mirrors `url_safety.py`).
pub fn offline_ssrf_check(url: &str, allow_private: bool) -> Result<(), FsmError> {
    let parsed =
        Url::parse(url.trim()).map_err(|e| FsmError::UnsafeUrl(format!("invalid URL: {e}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(FsmError::UnsafeUrl(format!(
            "scheme {} blocked",
            parsed.scheme()
        )));
    }
    if allow_private {
        return Ok(());
    }
    let host = host_of(url);
    if host.is_empty() {
        return Err(FsmError::UnsafeUrl("missing host".into()));
    }
    let trimmed = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = trimmed.parse::<IpAddr>() {
        let blocked = match ip {
            IpAddr::V4(v4) => {
                v4.is_loopback()
                    || v4.is_private()
                    || v4.is_link_local()
                    || v4.is_unspecified()
                    || (v4.octets()[0] == 100 && (v4.octets()[1] & 0b1100_0000) == 64)
            }
            IpAddr::V6(v6) => {
                v6.is_loopback()
                    || v6.is_unspecified()
                    || (v6.segments()[0] & 0xfe00) == 0xfc00
                    || (v6.segments()[0] & 0xffc0) == 0xfe80
            }
        };
        if blocked {
            return Err(FsmError::UnsafeUrl(format!("private/blocked IP {ip}")));
        }
    }
    let h = host.trim_end_matches('.');
    if matches!(
        h,
        "localhost"
            | "metadata"
            | "metadata.google.internal"
            | "metadata.goog"
            | "instance-data"
            | "instance-data.ec2.internal"
    ) || h.ends_with(".localhost")
        || h.ends_with(".local")
        || h.ends_with(".internal")
        || h.ends_with(".lan")
    {
        return Err(FsmError::UnsafeUrl(format!("internal host {h}")));
    }
    Ok(())
}

type NetHandler = Box<
    dyn Fn(&HttpRequest, TransportProfile, &NetCx) -> Result<HttpBytes, FsmError> + Send + Sync,
>;

/// Scriptable offline [`NetClient`] that records every request and enforces L1–L3 SSRF.
pub struct MockNet {
    pub allow_private: bool,
    pub requests: Mutex<Vec<(HttpRequest, TransportProfile)>>,
    queue: Mutex<VecDeque<Result<HttpBytes, FsmError>>>,
    handler: Mutex<Option<NetHandler>>,
}

impl Default for MockNet {
    fn default() -> Self {
        Self::new()
    }
}

impl MockNet {
    pub fn new() -> Self {
        Self {
            allow_private: false,
            requests: Mutex::new(Vec::new()),
            queue: Mutex::new(VecDeque::new()),
            handler: Mutex::new(None),
        }
    }

    pub fn with_responses(responses: Vec<Result<HttpBytes, FsmError>>) -> Self {
        Self {
            allow_private: false,
            requests: Mutex::new(Vec::new()),
            queue: Mutex::new(responses.into()),
            handler: Mutex::new(None),
        }
    }

    pub fn with_handler<F>(f: F) -> Self
    where
        F: Fn(&HttpRequest, TransportProfile, &NetCx) -> Result<HttpBytes, FsmError>
            + Send
            + Sync
            + 'static,
    {
        Self {
            allow_private: false,
            requests: Mutex::new(Vec::new()),
            queue: Mutex::new(VecDeque::new()),
            handler: Mutex::new(Some(Box::new(f))),
        }
    }

    pub fn push(&self, resp: Result<HttpBytes, FsmError>) {
        self.queue.lock().unwrap().push_back(resp);
    }

    pub fn recorded(&self) -> Vec<(HttpRequest, TransportProfile)> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl NetClient for MockNet {
    async fn send(
        &self,
        req: HttpRequest,
        profile: TransportProfile,
        cx: &NetCx,
    ) -> Result<HttpBytes, FsmError> {
        if cx.cancel.is_cancelled() {
            return Err(FsmError::Timeout(0));
        }
        offline_ssrf_check(&req.url, self.allow_private)?;
        self.requests.lock().unwrap().push((req.clone(), profile));
        if let Some(ref f) = *self.handler.lock().unwrap() {
            return f(&req, profile, cx);
        }
        self.queue
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err(FsmError::internal("mock_net", "no queued response")))
    }

    fn check_url_offline(&self, url: &str) -> Result<(), FsmError> {
        offline_ssrf_check(url, self.allow_private)
    }
}

/// In-memory [`StoreApi`] tracking page cache reads/writes and download records.
#[derive(Default)]
pub struct MemStore {
    pub pages: Mutex<HashMap<String, (PageResult, u64)>>,
    pub page_gets: Mutex<Vec<(String, Option<u64>)>>,
    pub downloads: Mutex<Vec<DownloadResult>>,
}

impl MemStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn seed_page(&self, page: PageResult, age_s: u64) {
        self.pages
            .lock()
            .unwrap()
            .insert(page.url.clone(), (page, age_s));
    }

    pub fn stored_page(&self, url: &str) -> Option<PageResult> {
        self.pages.lock().unwrap().get(url).map(|(p, _)| p.clone())
    }
}

#[async_trait]
impl StoreApi for MemStore {
    async fn cache_search(&self, _fts: &str, _limit: u32) -> Result<Vec<CacheHit>, FsmError> {
        Ok(Vec::new())
    }

    async fn get_page(
        &self,
        url: &str,
        max_age_s: Option<u64>,
    ) -> Result<Option<PageResult>, FsmError> {
        self.page_gets
            .lock()
            .unwrap()
            .push((url.to_owned(), max_age_s));
        let map = self.pages.lock().unwrap();
        let Some((page, age_s)) = map.get(url) else {
            return Ok(None);
        };
        if let Some(limit) = max_age_s
            && *age_s > limit
        {
            return Ok(None);
        }
        let mut p = page.clone();
        p.cache_age_s = Some(*age_s);
        Ok(Some(p))
    }

    async fn put_page(&self, page: &PageResult, _ttl_s: u64) -> Result<(), FsmError> {
        self.pages
            .lock()
            .unwrap()
            .insert(page.url.clone(), (page.clone(), 0));
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
        _k: &str,
        _m: Option<u64>,
    ) -> Result<Option<(SearchResponse, u64)>, FsmError> {
        Ok(None)
    }
    async fn search_cache_put(
        &self,
        _k: &str,
        _r: &SearchResponse,
        _ttl: u64,
    ) -> Result<(), FsmError> {
        Ok(())
    }
    async fn api_cache_get(
        &self,
        _k: &str,
    ) -> Result<Option<(Bytes, Option<String>, Option<String>)>, FsmError> {
        Ok(None)
    }
    async fn api_cache_put(
        &self,
        _k: &str,
        _s: &str,
        _b: &[u8],
        _e: Option<&str>,
        _l: Option<&str>,
        _ttl: u64,
    ) -> Result<(), FsmError> {
        Ok(())
    }
    async fn scholar_get(&self, _k: &str) -> Result<Option<ScholarlyWork>, FsmError> {
        Ok(None)
    }
    async fn scholar_put(&self, _k: &str, _w: &ScholarlyWork, _ttl: u64) -> Result<(), FsmError> {
        Ok(())
    }
    async fn retraction_lookup(&self, _d: &str) -> Result<Option<RetractionNotice>, FsmError> {
        Ok(None)
    }
    async fn retractions_replace_batch(
        &self,
        _r: Vec<(String, RetractionNotice)>,
        _e: Option<String>,
    ) -> Result<(), FsmError> {
        Ok(())
    }
    async fn sync_state_get(
        &self,
        _n: &str,
    ) -> Result<Option<(Option<String>, i64, Option<i64>)>, FsmError> {
        Ok(None)
    }
    async fn sync_state_mark(
        &self,
        _n: &str,
        _e: Option<&str>,
        _s: i64,
        _f: Option<i64>,
    ) -> Result<(), FsmError> {
        Ok(())
    }
    async fn health_load_all(&self) -> Result<Vec<HealthRecord>, FsmError> {
        Ok(Vec::new())
    }
    async fn health_save(&self, _r: &HealthRecord) -> Result<(), FsmError> {
        Ok(())
    }
    async fn download_record(&self, rec: &DownloadResult, _ttl_s: u64) -> Result<(), FsmError> {
        self.downloads.lock().unwrap().push(rec.clone());
        Ok(())
    }
}

/// Fake [`DocApi`] that records `read` calls and returns a configurable [`DocSlice`].
pub struct MockDocs {
    pub calls: Mutex<Vec<(String, usize, Option<usize>)>>,
    slice: Mutex<DocSlice>,
}

impl Default for MockDocs {
    fn default() -> Self {
        let mut s = DocSlice::new("doc", "pdf", "Default PDF content");
        s.title = Some("Sample Doc".into());
        s.total_chars = 19;
        Self::new(s)
    }
}

impl MockDocs {
    pub fn new(slice: DocSlice) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            slice: Mutex::new(slice),
        }
    }
}

#[async_trait]
impl DocApi for MockDocs {
    async fn read(
        &self,
        source: &str,
        start: usize,
        length: Option<usize>,
        _cx: &CallCx,
    ) -> Result<DocSlice, FsmError> {
        self.calls
            .lock()
            .unwrap()
            .push((source.to_owned(), start, length));
        let mut s = self.slice.lock().unwrap().clone();
        s.source = source.to_owned();
        Ok(s)
    }

    async fn outline(&self, source: &str, _cx: &CallCx) -> Result<DocOutline, FsmError> {
        Ok(DocOutline::new(source, "pdf"))
    }
}

/// Fake [`SearchApi`] returning a fixed [`SearchResponse`].
pub struct MockSearch {
    pub requests: Mutex<Vec<SearchRequest>>,
    resp: Mutex<SearchResponse>,
}

impl Default for MockSearch {
    fn default() -> Self {
        Self::new(SearchResponse::new("", "web"))
    }
}

impl MockSearch {
    pub fn new(resp: SearchResponse) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            resp: Mutex::new(resp),
        }
    }
}

#[async_trait]
impl SearchApi for MockSearch {
    async fn search(&self, req: SearchRequest, _cx: &CallCx) -> Result<SearchResponse, FsmError> {
        self.requests.lock().unwrap().push(req);
        Ok(self.resp.lock().unwrap().clone())
    }
}

pub fn make_service(
    net: Arc<dyn NetClient>,
    store: Arc<dyn StoreApi>,
    cfg: Config,
    docs: Arc<dyn DocApi>,
    search: Arc<dyn SearchApi>,
) -> FetchService {
    let deps = SourceDeps::new(net, store, Arc::new(cfg));
    FetchService::new(deps, docs, search)
}

/// Build a valid minimal PNG with `width × height` IHDR header (for `imagesize`).
pub fn make_png(width: u32, height: u32) -> Vec<u8> {
    fn chunk(tag: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(tag);
        out.extend_from_slice(data);
        // CRC32 (standard IEEE)
        let mut crc: u32 = 0xffff_ffff;
        for &b in tag.iter().chain(data.iter()) {
            crc ^= u32::from(b);
            for _ in 0..8 {
                if (crc & 1) != 0 {
                    crc = (crc >> 1) ^ 0xedb8_8320;
                } else {
                    crc >>= 1;
                }
            }
        }
        out.extend_from_slice(&(!crc).to_be_bytes());
        out
    }

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);

    let mut out = vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
    out.extend_from_slice(&chunk(b"IHDR", &ihdr));
    out.extend_from_slice(&chunk(b"IEND", &[]));
    out
}
