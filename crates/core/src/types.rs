//! Wire & result data types (`SPEC-contracts.md` §1.1).
//!
//! Every struct is `#[non_exhaustive]` + `Default` (construct outside this
//! crate with `T::default()` + field assignment, or the `new` constructors),
//! serializes camelCase (1:1 with the TypeScript interfaces in `SPEC.md`
//! §3.2), and omits every absent `Option` (TS `?:`).
//!
//! These types are **JSON-only**: `skip_serializing_if`, `flatten` and
//! `serde_json::Value` fields do not round-trip through non-self-describing
//! formats such as postcard/bincode. Persist them as (zstd-compressed) JSON.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// What kind of thing a search hit is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum HitKind {
    #[default]
    Web,
    News,
    Academic,
    Record,
    Vertical,
}

/// How a page's content was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum FetchMethod {
    #[default]
    Http,
    Browser,
    Cache,
    Document,
    Asset,
}

/// One search result.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Hit {
    pub title: String,
    pub url: String,
    #[serde(default)]
    pub snippet: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date_source: Option<String>,
    /// M3: the manifest interpreter injects `source_id`.
    #[serde(default)]
    pub source: String,
    /// M3: the manifest interpreter injects `[source_id]`.
    #[serde(default)]
    pub sources: Vec<String>,
    #[serde(default)]
    pub score: f64,
    /// M3: the manifest interpreter injects `Record` / `Web`.
    #[serde(default)]
    pub kind: HitKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<Value>,
}

impl Hit {
    /// A hit with `title` and `url`; everything else default.
    pub fn new(title: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            url: url.into(),
            ..Self::default()
        }
    }
}

/// Result of `search` / `web.search`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SearchResponse {
    pub query: String,
    pub scope: String,
    pub results: Vec<Hit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lead: Option<String>,
    pub engines: Vec<String>,
    pub cached: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_age_s: Option<u64>,
    pub elapsed_ms: u64,
    pub notes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<SearchAgentSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_lane: Option<AgentLaneReport>,
}

/// Optional answer-agent block attached to a `SearchResponse` when
/// `SEARCH_MCP_AGENT_IN_SEARCH` and an answer backend are configured (ruling R5).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SearchAgentSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub elapsed_ms: u64,
}

impl SearchAgentSummary {
    pub fn answered(
        answer: impl Into<String>,
        backend: impl Into<String>,
        model: impl Into<String>,
        elapsed_ms: u64,
    ) -> Self {
        Self {
            answer: Some(answer.into()),
            backend: Some(backend.into()),
            model: Some(model.into()),
            error: None,
            elapsed_ms,
        }
    }

    pub fn failed(error: impl Into<String>, backend: impl Into<String>, elapsed_ms: u64) -> Self {
        Self {
            answer: None,
            backend: Some(backend.into()),
            model: None,
            error: Some(error.into()),
            elapsed_ms,
        }
    }
}

/// Status of the signed-in agent-search lane for a search response (ruling R5).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct AgentLaneReport {
    #[serde(default)]
    pub seated: Vec<String>,
    #[serde(default)]
    pub answered: Vec<String>,
    #[serde(default)]
    pub pending: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// Mode for the `ask` tool / `web.ask` (ruling R5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum AskMode {
    #[default]
    Auto,
    Fast,
    Agent,
}

impl AskMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Fast => "fast",
            Self::Agent => "agent",
        }
    }
}

/// One ranked passage returned by `ask` (fast or direct mode).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct AskPassage {
    pub text: String,
    pub title: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_age_s: Option<u64>,
    #[serde(default)]
    pub score: f64,
}

impl AskPassage {
    pub fn new(text: impl Into<String>, title: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            title: title.into(),
            url: url.into(),
            ..Self::default()
        }
    }
}

/// One source cited by `ask`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct AskSource {
    pub title: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_age_s: Option<u64>,
}

impl AskSource {
    pub fn new(title: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            url: url.into(),
            ..Self::default()
        }
    }
}

/// Result of `ask` / `web.ask` (ruling R5).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct AskResponse {
    pub question: String,
    /// `"direct" | "fast" | "agent"`.
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    #[serde(default)]
    pub passages: Vec<AskPassage>,
    #[serde(default)]
    pub sources: Vec<AskSource>,
    pub retrieved_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_age_s: Option<u64>,
    pub elapsed_ms: u64,
    #[serde(default)]
    pub partial: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date_note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_error: Option<String>,
    #[serde(default)]
    pub notes: Vec<String>,
}

impl AskResponse {
    pub fn new(question: impl Into<String>, mode: impl Into<String>) -> Self {
        Self {
            question: question.into(),
            mode: mode.into(),
            ..Self::default()
        }
    }
}

impl SearchResponse {
    pub fn new(query: impl Into<String>, scope: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            scope: scope.into(),
            ..Self::default()
        }
    }
}

/// A fetched (or cached / document / asset) page.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct PageResult {
    pub url: String,
    pub title: String,
    pub content: String,
    pub method: FetchMethod,
    pub tokens_estimated: usize,
    pub total_chars: usize,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sitename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    pub retrieved_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_age_s: Option<u64>,
}

impl PageResult {
    /// A page with identity and content; `total_chars` and
    /// `tokens_estimated` are computed from `content`.
    pub fn new(
        url: impl Into<String>,
        title: impl Into<String>,
        content: impl Into<String>,
        method: FetchMethod,
    ) -> Self {
        let content = content.into();
        Self {
            url: url.into(),
            title: title.into(),
            total_chars: content.chars().count(),
            tokens_estimated: crate::text::estimate_tokens(&content),
            content,
            method,
            ..Self::default()
        }
    }
}

/// Result of `web.research`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ResearchBrief {
    pub query: String,
    pub search: SearchResponse,
    pub pages: Vec<PageResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date_note: Option<String>,
    pub tokens_estimated: usize,
}

impl ResearchBrief {
    pub fn new(query: impl Into<String>, search: SearchResponse) -> Self {
        Self {
            query: query.into(),
            search,
            ..Self::default()
        }
    }
}

/// Metadata of an inline image returned by `web.fetch(url, {inlineImage:true})`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct InlineImageMeta {
    pub url: String,
    pub media_type: String,
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    pub sha256: String,
}

impl InlineImageMeta {
    pub fn new(
        url: impl Into<String>,
        media_type: impl Into<String>,
        bytes: u64,
        sha256: impl Into<String>,
    ) -> Self {
        Self {
            url: url.into(),
            media_type: media_type.into(),
            bytes,
            sha256: sha256.into(),
            ..Self::default()
        }
    }
}

/// Result of `web.request` (N1: the text is `body`, never `text`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct HttpResponse {
    pub url: String,
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    /// N1: NEVER `.text`; HEAD => `""`; non-UTF-8 decoded via encoding_rs;
    /// binary content types => `InvalidInput`.
    pub body: String,
    /// Some iff Content-Type is `application/json` or `*+json` AND `body`
    /// parses as valid JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json: Option<Value>,
    pub truncated: bool,
}

impl HttpResponse {
    pub fn new(url: impl Into<String>, status: u16, body: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            status,
            body: body.into(),
            ..Self::default()
        }
    }
}

/// Result of `web.structured`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct StructuredMeta {
    pub url: String,
    pub json_ld: Vec<Value>,
    pub opengraph: BTreeMap<String, String>,
    pub microdata: Vec<Value>,
    pub meta_fallback: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl StructuredMeta {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Self::default()
        }
    }
}

/// One row of `web.compare`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct CompareItem {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl CompareItem {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Self::default()
        }
    }
}

/// Result of `web.download`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct DownloadResult {
    pub url: String,
    pub saved_path: String,
    pub media_type: String,
    pub bytes: u64,
    pub sha256: String,
    pub expires_in_hours: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

impl DownloadResult {
    pub fn new(
        url: impl Into<String>,
        saved_path: impl Into<String>,
        media_type: impl Into<String>,
        bytes: u64,
        sha256: impl Into<String>,
    ) -> Self {
        Self {
            url: url.into(),
            saved_path: saved_path.into(),
            media_type: media_type.into(),
            bytes,
            sha256: sha256.into(),
            ..Self::default()
        }
    }
}

/// A slice of a document (`doc.read`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct DocSlice {
    pub source: String,
    pub format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pages: Option<u32>,
    pub start: usize,
    pub returned_chars: usize,
    pub total_chars: usize,
    pub truncated: bool,
    pub tokens_estimated: usize,
}

impl DocSlice {
    pub fn new(
        source: impl Into<String>,
        format: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        let content = content.into();
        Self {
            source: source.into(),
            format: format.into(),
            returned_chars: content.chars().count(),
            tokens_estimated: crate::text::estimate_tokens(&content),
            content,
            ..Self::default()
        }
    }
}

/// One heading of a document outline.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct DocSection {
    pub title: String,
    pub level: u8,
    pub start_char: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<u32>,
}

impl DocSection {
    pub fn new(title: impl Into<String>, level: u8, start_char: usize) -> Self {
        Self {
            title: title.into(),
            level,
            start_char,
            ..Self::default()
        }
    }
}

/// One spreadsheet sheet in an outline.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SheetInfo {
    pub name: String,
    pub rows: usize,
    pub cols: usize,
}

impl SheetInfo {
    pub fn new(name: impl Into<String>, rows: usize, cols: usize) -> Self {
        Self {
            name: name.into(),
            rows,
            cols,
        }
    }
}

/// Result of `doc.outline`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct DocOutline {
    pub source: String,
    pub format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pages: Option<u32>,
    pub sections: Vec<DocSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sheets: Option<Vec<SheetInfo>>,
}

impl DocOutline {
    pub fn new(source: impl Into<String>, format: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            format: format.into(),
            ..Self::default()
        }
    }
}

/// Persistent identifiers of a scholarly work.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct WorkIds {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doi: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arxiv: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pmid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pmcid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openalex: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s2: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dblp: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openaire: Option<String>,
}

impl WorkIds {
    /// True when no identifier is set.
    pub fn is_empty(&self) -> bool {
        self.doi.is_none()
            && self.arxiv.is_none()
            && self.pmid.is_none()
            && self.pmcid.is_none()
            && self.openalex.is_none()
            && self.s2.is_none()
            && self.dblp.is_none()
            && self.openaire.is_none()
    }
}

/// An author as cited on a work.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct AuthorRef {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orcid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affiliation: Option<String>,
}

impl AuthorRef {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }
}

/// Open-access location of a work.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct OaLocation {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pdf_url: Option<String>,
    /// `"jats" | "bioc" | "html5"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
}

impl OaLocation {
    pub fn new(status: impl Into<String>) -> Self {
        Self {
            status: status.into(),
            ..Self::default()
        }
    }
}

/// A preprint/published version of a work.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct WorkVersion {
    /// `"preprint" | "published"`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doi: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arxiv: Option<String>,
    pub url: String,
}

impl WorkVersion {
    pub fn new(kind: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            url: url.into(),
            ..Self::default()
        }
    }
}

/// A merged scholarly work record.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ScholarlyWork {
    pub title: String,
    pub authors: Vec<AuthorRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub venue: Option<String>,
    pub work_type: String,
    pub ids: WorkIds,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abstract_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cited_by: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cited_by_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub influential_citations: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oa: Option<OaLocation>,
    #[serde(default)]
    pub retracted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retraction_reason: Option<String>,
    #[serde(default)]
    pub versions: Vec<WorkVersion>,
    pub url: String,
    #[serde(default)]
    pub sources: Vec<String>,
}

impl ScholarlyWork {
    pub fn new(title: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            url: url.into(),
            ..Self::default()
        }
    }
}

/// Provenance/diagnostics of a scholar search.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SearchMeta {
    pub query: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscope: Option<String>,
    pub sources: Vec<String>,
    pub elapsed_ms: u64,
    pub notes: Vec<String>,
    pub cached: bool,
}

impl SearchMeta {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            ..Self::default()
        }
    }
}

/// Result of `scholar.search`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct WorkResult {
    pub works: Vec<ScholarlyWork>,
    pub meta: SearchMeta,
}

impl WorkResult {
    pub fn new(works: Vec<ScholarlyWork>, meta: SearchMeta) -> Self {
        Self { works, meta }
    }
}

/// A notice (retraction, correction, ...) attached to a citation graph.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct GraphNotice {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doi: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

impl GraphNotice {
    pub fn new(kind: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            ..Self::default()
        }
    }
}

/// Result of `scholar.graph`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct CitationGraph {
    pub query: String,
    pub resolved_as: String,
    pub paper: ScholarlyWork,
    pub references: Vec<ScholarlyWork>,
    pub citations: Vec<ScholarlyWork>,
    pub notices: Vec<GraphNotice>,
    pub notes: Vec<String>,
}

impl CitationGraph {
    pub fn new(
        query: impl Into<String>,
        resolved_as: impl Into<String>,
        paper: ScholarlyWork,
    ) -> Self {
        Self {
            query: query.into(),
            resolved_as: resolved_as.into(),
            paper,
            ..Self::default()
        }
    }
}

/// One section of a structured full text.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct FulltextSection {
    pub heading: String,
    pub text: String,
}

impl FulltextSection {
    pub fn new(heading: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            heading: heading.into(),
            text: text.into(),
        }
    }
}

/// Result of `scholar.fulltext`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct FulltextResult {
    pub id: String,
    pub available: bool,
    /// `"jats" | "bioc" | "html5" | "pdf" | "text"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sections: Option<Vec<FulltextSection>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oa_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landing_url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl FulltextResult {
    pub fn new(id: impl Into<String>, available: bool) -> Self {
        Self {
            id: id.into(),
            available,
            ..Self::default()
        }
    }
}

/// One retraction/correction notice.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct RetractionNotice {
    pub nature: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice_doi: Option<String>,
    pub source: String,
}

impl RetractionNotice {
    pub fn new(nature: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            nature: nature.into(),
            source: source.into(),
            ..Self::default()
        }
    }
}

/// scite.ai citation-statement tallies.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SciteTallies {
    pub supporting: u64,
    pub contradicting: u64,
    pub mentioning: u64,
}

impl SciteTallies {
    pub fn new(supporting: u64, contradicting: u64, mentioning: u64) -> Self {
        Self {
            supporting,
            contradicting,
            mentioning,
        }
    }
}

/// Result of `scholar.integrity`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct IntegrityReport {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doi: Option<String>,
    pub is_retracted: bool,
    pub retraction_notices: Vec<RetractionNotice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scite: Option<SciteTallies>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doaj_vetted: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openalex_fwci: Option<f64>,
}

impl IntegrityReport {
    pub fn new(doi: Option<String>) -> Self {
        Self {
            doi,
            ..Self::default()
        }
    }
}

/// Result of `scholar.author` (cap `author`, M13).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct AuthorProfile {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orcid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openalex_id: Option<String>,
    pub affiliations: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub works_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cited_by_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub h_index: Option<u32>,
    pub top_works: Vec<ScholarlyWork>,
    pub sources: Vec<String>,
}

impl AuthorProfile {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }
}

/// Result of `scholar.org` (cap `org`, M13).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct OrgProfile {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ror: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openalex_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub works_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cited_by_count: Option<u64>,
    pub sources: Vec<String>,
}

impl OrgProfile {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }
}

/// Result of `scholar.venue` (cap `venue`, M13).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct VenueProfile {
    pub name: String,
    pub issn: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doaj_vetted: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icore_rank: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openalex_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub works_count: Option<u64>,
    pub sources: Vec<String>,
}

impl VenueProfile {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }
}

/// One grant (cap `grant`, M13).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct GrantRecord {
    pub id: String,
    pub title: String,
    pub funder: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pi: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub source: String,
}

impl GrantRecord {
    pub fn new(
        id: impl Into<String>,
        title: impl Into<String>,
        funder: impl Into<String>,
        source: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            funder: funder.into(),
            source: source.into(),
            ..Self::default()
        }
    }
}

/// Result of `source.call`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SourceOutput {
    pub source: String,
    pub op: String,
    pub items: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<Value>,
    pub elapsed_ms: u64,
}

impl SourceOutput {
    pub fn new(source: impl Into<String>, op: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            op: op.into(),
            ..Self::default()
        }
    }
}

/// Result of `source.protocol`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ProtocolOutput {
    pub kind: String,
    pub base_url: String,
    pub op: String,
    pub items: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

impl ProtocolOutput {
    pub fn new(
        kind: impl Into<String>,
        base_url: impl Into<String>,
        op: impl Into<String>,
    ) -> Self {
        Self {
            kind: kind.into(),
            base_url: base_url.into(),
            op: op.into(),
            ..Self::default()
        }
    }
}

/// One FTS5 hit over cached pages (`cache.search`, `scope="cache"`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct CacheHit {
    pub url: String,
    pub title: String,
    pub snippet: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sitename: Option<String>,
}

impl CacheHit {
    pub fn new(
        url: impl Into<String>,
        title: impl Into<String>,
        snippet: impl Into<String>,
    ) -> Self {
        Self {
            url: url.into(),
            title: title.into(),
            snippet: snippet.into(),
            ..Self::default()
        }
    }
}

/// One catalog entry (`catalog.search`, `scope="catalog"`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct CatalogHit {
    pub id: String,
    /// `"source" | "protocol" | "api"`.
    pub kind: String,
    pub summary: String,
    pub signature: String,
    pub example: String,
    pub auth: String,
    pub tier: u8,
    pub health: String,
}

impl CatalogHit {
    pub fn new(id: impl Into<String>, kind: impl Into<String>, summary: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind: kind.into(),
            summary: summary.into(),
            ..Self::default()
        }
    }
}

/// One parameter in a catalog detail.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct CatalogParamDoc {
    pub name: String,
    pub ty: String,
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    pub doc: String,
}

impl CatalogParamDoc {
    pub fn new(name: impl Into<String>, ty: impl Into<String>, doc: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ty: ty.into(),
            doc: doc.into(),
            ..Self::default()
        }
    }
}

/// Result of `catalog.describe` (M2: flat JSON, TS `extends CatalogHit`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct CatalogDetail {
    #[serde(flatten)]
    pub hit: CatalogHit,
    pub params: Vec<CatalogParamDoc>,
    pub scopes: Vec<String>,
    pub caps: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_url: Option<String>,
}

impl CatalogDetail {
    pub fn new(hit: CatalogHit) -> Self {
        Self {
            hit,
            ..Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn keys(v: &Value) -> Vec<String> {
        let mut k: Vec<String> = v.as_object().expect("object").keys().cloned().collect();
        k.sort();
        k
    }

    #[test]
    fn absent_options_are_omitted_and_keys_are_camel_case() {
        let mut p = PageResult::new("https://e.org/", "T", "hello world", FetchMethod::Http);
        p.retrieved_at = "2026-11-25T00:00:00Z".into();
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(
            keys(&v),
            [
                "content",
                "method",
                "retrievedAt",
                "title",
                "tokensEstimated",
                "totalChars",
                "truncated",
                "url"
            ]
        );
        assert_eq!(v["method"], "http");
        assert_eq!(v["totalChars"], 11);
        p.cache_age_s = Some(3);
        p.media_type = Some("text/html".into());
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["cacheAgeS"], 3);
        assert_eq!(v["mediaType"], "text/html");
    }

    #[test]
    fn hit_defaults_fill_on_deserialize() {
        // M3: a manifest map that produces only title/url still deserializes.
        let h: Hit = serde_json::from_value(json!({"title": "t", "url": "https://x"})).unwrap();
        assert_eq!(h.kind, HitKind::Web);
        assert!(h.sources.is_empty() && h.snippet.is_empty() && h.score == 0.0);
        let h: Hit = serde_json::from_value(
            json!({"title": "t", "url": "u", "dateSource": "api", "kind": "record"}),
        )
        .unwrap();
        assert_eq!(h.date_source.as_deref(), Some("api"));
        assert_eq!(h.kind, HitKind::Record);
    }

    #[test]
    fn http_response_has_body_never_text() {
        let mut r = HttpResponse::new("https://api.example/x", 200, "{\"a\":1}");
        r.json = Some(json!({"a": 1}));
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(
            keys(&v),
            ["body", "headers", "json", "status", "truncated", "url"]
        );
        assert!(v.get("text").is_none());
        let r2 = HttpResponse::new("u", 204, "");
        assert!(serde_json::to_value(&r2).unwrap().get("json").is_none());
    }

    #[test]
    fn catalog_detail_is_flat() {
        let mut hit = CatalogHit::new("arxiv", "source", "e-prints");
        hit.tier = 1;
        let mut d = CatalogDetail::new(hit);
        d.scopes = vec!["academic".into()];
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["id"], "arxiv");
        assert_eq!(v["tier"], 1);
        assert!(v.get("hit").is_none());
        assert_eq!(
            keys(&v),
            [
                "auth",
                "caps",
                "example",
                "health",
                "id",
                "kind",
                "params",
                "scopes",
                "signature",
                "summary",
                "tier"
            ]
        );
        let back: CatalogDetail = serde_json::from_value(v).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn scholarly_work_round_trips() {
        let mut w = ScholarlyWork::new(
            "Attention Is All You Need",
            "https://arxiv.org/abs/1706.03762",
        );
        w.authors = vec![AuthorRef::new("Ashish Vaswani")];
        w.year = Some(2017);
        w.work_type = "preprint".into();
        w.ids.arxiv = Some("1706.03762".into());
        w.ids.doi = Some("10.48550/arxiv.1706.03762".into());
        let mut oa = OaLocation::new("green");
        oa.structured_format = Some("html5".into());
        w.oa = Some(oa);
        w.versions = vec![WorkVersion::new(
            "preprint",
            "https://arxiv.org/abs/1706.03762v7",
        )];
        w.sources = vec!["arxiv".into()];
        let s = serde_json::to_string(&w).unwrap();
        assert!(s.contains("\"workType\":\"preprint\""));
        assert!(s.contains("\"structuredFormat\":\"html5\""));
        assert!(!s.contains("abstractText"));
        let back: ScholarlyWork = serde_json::from_str(&s).unwrap();
        assert_eq!(back, w);
        // retracted/versions/sources default when absent (JS may omit them).
        let min: ScholarlyWork = serde_json::from_value(json!({
            "title": "x", "authors": [], "workType": "article", "ids": {}, "url": "u"
        }))
        .unwrap();
        assert!(!min.retracted && min.versions.is_empty());
    }

    #[test]
    fn every_result_type_round_trips() {
        fn rt<T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug>(v: T) {
            let s = serde_json::to_string(&v).unwrap();
            let back: T = serde_json::from_str(&s).unwrap();
            assert_eq!(back, v, "{s}");
        }
        let mut sr = SearchResponse::new("rust", "web");
        sr.results
            .push(Hit::new("Rust", "https://www.rust-lang.org/"));
        sr.engines.push("duckduckgo".into());
        sr.cache_key = Some("v4:abc".into());
        rt(sr.clone());
        let mut rb = ResearchBrief::new("rust", sr);
        rb.date_note = Some("spread".into());
        rt(rb);
        rt(InlineImageMeta::new("u", "image/png", 10, "ab"));
        let mut sm = StructuredMeta::new("u");
        sm.opengraph.insert("og:title".into(), "T".into());
        sm.json_ld.push(json!({"@type": "Article"}));
        rt(sm);
        let mut ci = CompareItem::new("u");
        ci.truncated = Some(false);
        rt(ci);
        rt(DownloadResult::new(
            "u",
            "/tmp/x",
            "application/pdf",
            3,
            "ff",
        ));
        let mut ds = DocSlice::new("file.pdf", "pdf", "abc");
        ds.pages = Some(2);
        rt(ds);
        let mut ol = DocOutline::new("f.xlsx", "xlsx");
        ol.sections.push(DocSection::new("Intro", 1, 0));
        ol.sheets = Some(vec![SheetInfo::new("S1", 3, 4)]);
        rt(ol);
        let mut wr = WorkResult::new(vec![ScholarlyWork::new("t", "u")], SearchMeta::new("q"));
        wr.meta.subscope = Some("academic.cs".into());
        rt(wr);
        let mut cg = CitationGraph::new("q", "doi", ScholarlyWork::new("t", "u"));
        cg.notices.push(GraphNotice::new("retraction"));
        rt(cg);
        let mut ft = FulltextResult::new("10.1/x", true);
        ft.sections = Some(vec![FulltextSection::new("Methods", "...")]);
        rt(ft);
        let mut ir = IntegrityReport::new(Some("10.1/x".into()));
        ir.retraction_notices
            .push(RetractionNotice::new("Retraction", "retraction_watch"));
        ir.scite = Some(SciteTallies::new(1, 2, 3));
        ir.openalex_fwci = Some(1.5);
        rt(ir);
        rt(AuthorProfile::new("A"));
        rt(OrgProfile::new("O"));
        rt(VenueProfile::new("V"));
        let mut g = GrantRecord::new("R01", "T", "NIH", "nih_reporter");
        g.amount = Some(1.0e6);
        rt(g);
        let mut so = SourceOutput::new("arxiv", "search");
        so.items.push(json!({"a": 1}));
        so.raw = Some(json!([1, 2]));
        rt(so);
        rt(ProtocolOutput::new("oai_pmh", "https://x/oai", "identify"));
        rt(CacheHit::new("u", "t", "s"));
        let mut pd = CatalogParamDoc::new("limit", "int", "Max rows");
        pd.default = Some(json!(10));
        rt(pd);
        rt(WorkIds::default());
    }

    #[test]
    fn enum_wire_names() {
        assert_eq!(serde_json::to_value(HitKind::Vertical).unwrap(), "vertical");
        assert_eq!(
            serde_json::to_value(FetchMethod::Document).unwrap(),
            "document"
        );
        assert!(WorkIds::default().is_empty());
    }
}
