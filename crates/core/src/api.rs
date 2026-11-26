//! Facade traits and the `Services` bundle (`SPEC-contracts.md` §1.4).
//!
//! Request structs deserialize from the camelCase objects the exec prelude
//! passes; every field may be omitted (container `#[serde(default)]`), and
//! `Default` carries the documented tool defaults (limit 10, depth 3, ...).

use crate::config::Config;
use crate::deps::Registry;
use crate::error::FsmError;
use crate::host::{BinaryHandle, CallCx};
use crate::net::HttpMethod;
use crate::types::{
    AskMode, AskResponse, CacheHit, CatalogDetail, CatalogHit, CitationGraph, CompareItem,
    DocOutline, DocSlice, DownloadResult, FulltextResult, HttpResponse, InlineImageMeta,
    IntegrityReport, PageResult, ProtocolOutput, ResearchBrief, RetractionNotice, ScholarlyWork,
    SearchAgentSummary, SearchResponse, SourceOutput, StructuredMeta, WorkResult,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

/// `freshness` filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Freshness {
    Day,
    #[default]
    Week,
    Month,
    Year,
}

/// `render` mode of `web.fetch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum RenderMode {
    #[default]
    Auto,
    Http,
    Browser,
}

/// `direction` of `scholar.graph`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum GraphDirection {
    #[default]
    Both,
    References,
    Citations,
}

/// Arguments of `search` / `web.search`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
#[non_exhaustive]
pub struct SearchRequest {
    pub query: String,
    /// Raw scope as given (synonyms resolved by the service via `core::scopes`).
    pub scope: String,
    pub sources: Option<Vec<String>>,
    pub freshness: Option<Freshness>,
    pub domains: Option<Vec<String>>,
    pub lang: Option<String>,
    /// 1..=50; default 10.
    pub limit: u32,
    pub max_age_hours: Option<f64>,
}

impl Default for SearchRequest {
    fn default() -> Self {
        Self {
            query: String::new(),
            scope: "web".to_owned(),
            sources: None,
            freshness: None,
            domains: None,
            lang: None,
            limit: 10,
            max_age_hours: None,
        }
    }
}

impl SearchRequest {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            ..Self::default()
        }
    }

    /// Builder: scope.
    pub fn with_scope(mut self, scope: impl Into<String>) -> Self {
        self.scope = scope.into();
        self
    }
}

/// Options of `web.fetch` / `web.fetchBatch`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
#[non_exhaustive]
pub struct FetchOpts {
    pub render: RenderMode,
    pub force_refresh: bool,
    pub max_age_hours: Option<f64>,
    pub inline_image: bool,
}

/// Arguments of `web.request`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
#[non_exhaustive]
pub struct RawRequestOpts {
    pub url: String,
    /// Default GET.
    pub method: Option<HttpMethod>,
    pub headers: Option<BTreeMap<String, String>>,
    pub query: Option<BTreeMap<String, String>>,
    pub body: Option<Value>,
}

impl RawRequestOpts {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Self::default()
        }
    }
}

/// Options of `web.research`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
#[non_exhaustive]
pub struct ResearchOpts {
    /// 1..=8; default 3.
    pub depth: u8,
    pub scope: Option<String>,
    pub freshness: Option<Freshness>,
    pub domains: Option<Vec<String>>,
    pub lang: Option<String>,
}

impl Default for ResearchOpts {
    fn default() -> Self {
        Self {
            depth: 3,
            scope: None,
            freshness: None,
            domains: None,
            lang: None,
        }
    }
}

/// Arguments of `scholar.search`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
#[non_exhaustive]
pub struct ScholarQuery {
    pub query: String,
    pub subscope: Option<String>,
    pub sources: Option<Vec<String>>,
    /// `"relevance" | "citations" | "recent"`.
    pub sort: Option<String>,
    pub year_from: Option<u16>,
    pub year_to: Option<u16>,
    pub oa_only: bool,
    /// Default 10.
    pub limit: u32,
}

impl Default for ScholarQuery {
    fn default() -> Self {
        Self {
            query: String::new(),
            subscope: None,
            sources: None,
            sort: None,
            year_from: None,
            year_to: None,
            oa_only: false,
            limit: 10,
        }
    }
}

impl ScholarQuery {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            ..Self::default()
        }
    }
}

/// Options of `scholar.graph`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
#[non_exhaustive]
pub struct GraphOpts {
    pub direction: GraphDirection,
    /// 1..=50; default 10 (Python `paper_graph` parity).
    pub limit: u32,
}

impl Default for GraphOpts {
    fn default() -> Self {
        Self {
            direction: GraphDirection::Both,
            limit: 10,
        }
    }
}

/// Options of `scholar.fulltext`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
#[non_exhaustive]
pub struct FulltextOpts {
    /// Default true (structured JATS/BioC/HTML5 before PDF).
    pub prefer_structured: bool,
    pub sections: Option<Vec<String>>,
    pub max_chars: Option<usize>,
}

impl Default for FulltextOpts {
    fn default() -> Self {
        Self {
            prefer_structured: true,
            sections: None,
            max_chars: None,
        }
    }
}

/// Input of `scholar.cite`: an id/DOI/title, or a work object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
#[non_exhaustive]
pub enum CiteInput {
    Id(String),
    Work(Box<ScholarlyWork>),
}

impl Default for CiteInput {
    fn default() -> Self {
        Self::Id(String::new())
    }
}

/// Citation output style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum CiteStyle {
    #[default]
    Bibtex,
    Csl,
    Apa,
    Ieee,
    Chicago,
    Vancouver,
}

/// Entity kind of `scholar.{author,org,venue,grants}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum EntityKind {
    #[default]
    Author,
    Org,
    Venue,
    Grants,
}

/// Arguments of `catalog.search`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
#[non_exhaustive]
pub struct CatalogQuery {
    pub query: String,
    pub group: Option<String>,
    pub capability: Option<String>,
    pub keyless_only: bool,
    /// Default 10.
    pub limit: u32,
}

impl Default for CatalogQuery {
    fn default() -> Self {
        Self {
            query: String::new(),
            group: None,
            capability: None,
            keyless_only: false,
            limit: 10,
        }
    }
}

impl CatalogQuery {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            ..Self::default()
        }
    }
}

/// Persisted circuit-breaker state of one source (`health_state` row).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct HealthRecord {
    pub source_id: String,
    pub state: String,
    pub failures: u32,
    pub benched_until: Option<i64>,
    pub last_error: Option<String>,
    pub updated_at: i64,
}

impl HealthRecord {
    pub fn new(source_id: impl Into<String>, state: impl Into<String>, updated_at: i64) -> Self {
        Self {
            source_id: source_id.into(),
            state: state.into(),
            updated_at,
            ..Self::default()
        }
    }
}

/// What `web.fetch` returns: a page, or an inline image (opaque binary handle).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum FetchOutcome {
    Page(PageResult),
    InlineImage {
        meta: InlineImageMeta,
        binary: BinaryHandle,
    },
}

impl Default for FetchOutcome {
    fn default() -> Self {
        Self::Page(PageResult::default())
    }
}

#[async_trait::async_trait]
pub trait SearchApi: Send + Sync {
    async fn search(&self, req: SearchRequest, cx: &CallCx) -> Result<SearchResponse, FsmError>;
}

#[async_trait::async_trait]
pub trait ScholarApi: Send + Sync {
    async fn search(&self, q: ScholarQuery, cx: &CallCx) -> Result<WorkResult, FsmError>;
    async fn work(&self, id_or_title: &str, cx: &CallCx)
    -> Result<Option<ScholarlyWork>, FsmError>;
    async fn graph(
        &self,
        id_or_title: &str,
        opts: GraphOpts,
        cx: &CallCx,
    ) -> Result<CitationGraph, FsmError>;
    async fn fulltext(
        &self,
        id_or_doi: &str,
        opts: FulltextOpts,
        cx: &CallCx,
    ) -> Result<FulltextResult, FsmError>;
    async fn integrity(&self, doi_or_id: &str, cx: &CallCx) -> Result<IntegrityReport, FsmError>;
    async fn cite(
        &self,
        input: CiteInput,
        style: CiteStyle,
        cx: &CallCx,
    ) -> Result<String, FsmError>;
    async fn entity(
        &self,
        kind: EntityKind,
        query: &str,
        opts: Value,
        cx: &CallCx,
    ) -> Result<Value, FsmError>;
}

#[async_trait::async_trait]
pub trait FetchApi: Send + Sync {
    async fn fetch(
        &self,
        url: &str,
        opts: FetchOpts,
        cx: &CallCx,
    ) -> Result<FetchOutcome, FsmError>;
    async fn fetch_batch(
        &self,
        urls: Vec<String>,
        opts: FetchOpts,
        max_concurrency: u8,
        cx: &CallCx,
    ) -> Result<Vec<PageResult>, FsmError>;
    async fn request(&self, opts: RawRequestOpts, cx: &CallCx) -> Result<HttpResponse, FsmError>;
    async fn structured(&self, url: &str, cx: &CallCx) -> Result<StructuredMeta, FsmError>;
    async fn research(
        &self,
        query: &str,
        opts: ResearchOpts,
        cx: &CallCx,
    ) -> Result<ResearchBrief, FsmError>;
    async fn compare(
        &self,
        query: &str,
        urls: Vec<String>,
        cx: &CallCx,
    ) -> Result<Vec<CompareItem>, FsmError>;
    async fn download(&self, url: &str, cx: &CallCx) -> Result<DownloadResult, FsmError>;
    async fn html_to_md(&self, html: &str) -> Result<String, FsmError>;
}

#[async_trait::async_trait]
pub trait DocApi: Send + Sync {
    async fn read(
        &self,
        source: &str,
        start: usize,
        length: Option<usize>,
        cx: &CallCx,
    ) -> Result<DocSlice, FsmError>;
    async fn outline(&self, source: &str, cx: &CallCx) -> Result<DocOutline, FsmError>;
}

#[async_trait::async_trait]
pub trait SourcesApi: Send + Sync {
    async fn call(&self, op_id: &str, params: Value, cx: &CallCx)
    -> Result<SourceOutput, FsmError>;
    async fn protocol(
        &self,
        kind: &str,
        base_url: &str,
        op: &str,
        params: Value,
        cx: &CallCx,
    ) -> Result<ProtocolOutput, FsmError>;
    async fn catalog_search(&self, q: CatalogQuery) -> Result<Vec<CatalogHit>, FsmError>;
    async fn catalog_describe(&self, id: &str) -> Result<CatalogDetail, FsmError>;
}

#[async_trait::async_trait]
pub trait StoreApi: Send + Sync {
    async fn cache_search(&self, fts_query: &str, limit: u32) -> Result<Vec<CacheHit>, FsmError>;
    async fn get_page(
        &self,
        url: &str,
        max_age_s: Option<u64>,
    ) -> Result<Option<PageResult>, FsmError>;
    async fn put_page(&self, page: &PageResult, ttl_s: u64) -> Result<(), FsmError>;
    /// M7: `exec:*` stored under session_id=""; shared:true stored under "shared"; else caller session_id.
    async fn kv_put(
        &self,
        session_id: &str,
        key: &str,
        val: &Value,
        ttl_s: u64,
    ) -> Result<(), FsmError>;
    /// M7: ignores session_id when key starts with `exec:`.
    async fn kv_get(&self, session_id: &str, key: &str) -> Result<Option<Value>, FsmError>;
    async fn search_cache_get(
        &self,
        key: &str,
        max_age_s: Option<u64>,
    ) -> Result<Option<(SearchResponse, u64)>, FsmError>;
    async fn search_cache_put(
        &self,
        key: &str,
        resp: &SearchResponse,
        ttl_s: u64,
    ) -> Result<(), FsmError>;
    async fn api_cache_get(
        &self,
        key: &str,
    ) -> Result<Option<(bytes::Bytes, Option<String>, Option<String>)>, FsmError>;
    async fn api_cache_put(
        &self,
        key: &str,
        source_id: &str,
        body: &[u8],
        etag: Option<&str>,
        last_mod: Option<&str>,
        ttl_s: u64,
    ) -> Result<(), FsmError>;
    async fn scholar_get(
        &self,
        canonical_key_or_pid: &str,
    ) -> Result<Option<ScholarlyWork>, FsmError>;
    async fn scholar_put(
        &self,
        canonical_key: &str,
        work: &ScholarlyWork,
        ttl_s: u64,
    ) -> Result<(), FsmError>;
    async fn retraction_lookup(&self, norm_doi: &str)
    -> Result<Option<RetractionNotice>, FsmError>;
    async fn retractions_replace_batch(
        &self,
        rows: Vec<(String, RetractionNotice)>,
        etag: Option<String>,
    ) -> Result<(), FsmError>;
    async fn sync_state_get(
        &self,
        name: &str,
    ) -> Result<Option<(Option<String>, i64, Option<i64>)>, FsmError>;
    async fn sync_state_mark(
        &self,
        name: &str,
        etag: Option<&str>,
        started_at: i64,
        finished_at: Option<i64>,
    ) -> Result<(), FsmError>;
    async fn health_load_all(&self) -> Result<Vec<HealthRecord>, FsmError>;
    async fn health_save(&self, rec: &HealthRecord) -> Result<(), FsmError>;
    async fn download_record(&self, rec: &DownloadResult, ttl_s: u64) -> Result<(), FsmError>;
}

/// No-op [`StoreApi`]: every read misses, every write is dropped. For offline
/// tests and tools (`xtask`) that do not open SQLite.
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct NullStore;

impl NullStore {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl StoreApi for NullStore {
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
        _canonical_key_or_pid: &str,
    ) -> Result<Option<ScholarlyWork>, FsmError> {
        Ok(None)
    }

    async fn scholar_put(
        &self,
        _canonical_key: &str,
        _work: &ScholarlyWork,
        _ttl_s: u64,
    ) -> Result<(), FsmError> {
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
        Ok(Vec::new())
    }

    async fn health_save(&self, _rec: &HealthRecord) -> Result<(), FsmError> {
        Ok(())
    }

    async fn download_record(&self, _rec: &DownloadResult, _ttl_s: u64) -> Result<(), FsmError> {
        Ok(())
    }
}

/// Options for `ask` / `web.ask` (ruling R5).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct AskOpts {
    #[serde(default)]
    pub mode: AskMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freshness: Option<Freshness>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    #[serde(default)]
    pub domains: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<usize>,
}

/// Quick factual lookup and answer-agent subsystem (`fsm-agent`, ruling R5).
#[async_trait::async_trait]
pub trait AskApi: Send + Sync {
    async fn ask(
        &self,
        question: &str,
        opts: AskOpts,
        cx: &CallCx,
    ) -> Result<AskResponse, FsmError>;

    /// Optional short sourced answer rendered above `search` results when
    /// `SEARCH_MCP_AGENT_IN_SEARCH` and an answer backend are configured.
    /// Returns `None` when disabled or skipped; never fails the search.
    async fn answer_search(
        &self,
        question: &str,
        search: &SearchResponse,
        cx: &CallCx,
    ) -> Option<SearchAgentSummary>;
}

/// Everything the host dispatcher and the `search` tool need, assembled
/// once by `fsm-server`. Holds trait objects, so it is built with
/// [`Services::new`] (no `Default`).
#[derive(Clone)]
#[non_exhaustive]
pub struct Services {
    pub search: Arc<dyn SearchApi>,
    pub scholar: Arc<dyn ScholarApi>,
    pub fetch: Arc<dyn FetchApi>,
    pub doc: Arc<dyn DocApi>,
    pub sources: Arc<dyn SourcesApi>,
    pub store: Arc<dyn StoreApi>,
    pub registry: Arc<Registry>,
    pub cfg: Arc<Config>,
    pub ask: Option<Arc<dyn AskApi>>,
}

impl Services {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        search: Arc<dyn SearchApi>,
        scholar: Arc<dyn ScholarApi>,
        fetch: Arc<dyn FetchApi>,
        doc: Arc<dyn DocApi>,
        sources: Arc<dyn SourcesApi>,
        store: Arc<dyn StoreApi>,
        registry: Arc<Registry>,
        cfg: Arc<Config>,
    ) -> Self {
        Self {
            search,
            scholar,
            fetch,
            doc,
            sources,
            store,
            registry,
            cfg,
            ask: None,
        }
    }

    pub fn with_ask(mut self, ask: Arc<dyn AskApi>) -> Self {
        self.ask = Some(ask);
        self
    }
}

impl fmt::Debug for Services {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Services")
            .field("registry", &self.registry)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_defaults_apply_to_omitted_fields() {
        let r: SearchRequest = serde_json::from_value(json!({"query": "rust"})).unwrap();
        assert_eq!((r.scope.as_str(), r.limit), ("web", 10));
        let r: SearchRequest =
            serde_json::from_value(json!({"query": "q", "maxAgeHours": 0, "freshness": "day"}))
                .unwrap();
        assert_eq!(r.max_age_hours, Some(0.0));
        assert_eq!(r.freshness, Some(Freshness::Day));
        let o: ResearchOpts = serde_json::from_value(json!({})).unwrap();
        assert_eq!(o.depth, 3);
        let g: GraphOpts = serde_json::from_value(json!({"direction": "citations"})).unwrap();
        assert_eq!((g.direction, g.limit), (GraphDirection::Citations, 10));
        let f: FulltextOpts = serde_json::from_value(json!({"maxChars": 5000})).unwrap();
        assert!(f.prefer_structured);
        let q: ScholarQuery =
            serde_json::from_value(json!({"query": "x", "yearFrom": 2020, "oaOnly": true}))
                .unwrap();
        assert_eq!((q.year_from, q.oa_only, q.limit), (Some(2020), true, 10));
        let c: CatalogQuery =
            serde_json::from_value(json!({"query": "patents", "keylessOnly": true})).unwrap();
        assert!(c.keyless_only);
        let fo: FetchOpts =
            serde_json::from_value(json!({"render": "http", "inlineImage": true})).unwrap();
        assert_eq!(fo.render, RenderMode::Http);
        let rr: RawRequestOpts =
            serde_json::from_value(json!({"url": "u", "method": "HEAD", "body": {"a": 1}}))
                .unwrap();
        assert_eq!(rr.method, Some(HttpMethod::Head));
    }

    #[test]
    fn cite_input_is_untagged() {
        let a: CiteInput = serde_json::from_value(json!("10.1038/nature14539")).unwrap();
        assert_eq!(a, CiteInput::Id("10.1038/nature14539".into()));
        let b: CiteInput = serde_json::from_value(json!({
            "title": "t", "authors": [], "workType": "article", "ids": {}, "url": "u"
        }))
        .unwrap();
        assert!(matches!(b, CiteInput::Work(_)));
        assert_eq!(
            serde_json::to_value(CiteStyle::Vancouver).unwrap(),
            "vancouver"
        );
        assert_eq!(serde_json::to_value(EntityKind::Grants).unwrap(), "grants");
    }
}
