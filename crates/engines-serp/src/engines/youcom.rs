//! You.com keyless MCP search engine (`https://api.you.com/mcp?profile=free`).
//!
//! Port of `src/search_mcp/engines/youcom.py`:
//! - Stateless JSON-RPC `tools/call` POST for `you-search` (canonical sorted-key
//!   JSON via `fsm_core::fixture::canonical_json` for deterministic `bodySha256`);
//! - Handles both plain JSON and SSE (`data: ...`) responses, skipping
//!   notifications;
//! - Maps `web` (`HitKind::Web`, unconfident `"snippet"` date) and `news`
//!   (`HitKind::News`, confident `"api"` date);
//! - Never sends `exclude_domains` together with `include_domains` or inline `site:`;
//! - Raises `FsmError::Upstream` on JSON-RPC / tool errors and
//!   `FsmError::RateLimited` on HTTP 429.

use crate::common::{
    SerpQuery, decode_body, finalize_hits, freshness_as_str, make_hit, net_cx_for,
    parse_retry_after, search_op_decl,
};
use async_trait::async_trait;
use fsm_core::fixture::canonical_json;
use fsm_core::net::HttpRequest;
use fsm_core::source::{
    AuthMode, Capability, PoolRole, Source, SourceDecl, SourceLimits, SrcCx, SrcOutput,
    TransportProfile,
};
use fsm_core::text::clip_chars;
use fsm_core::types::{Hit, HitKind};
use fsm_core::{FsmError, SourceDeps};
use serde_json::{Map, Value, json};

pub const ENDPOINT: &str = "https://api.you.com/mcp?profile=free";
const TOOL_NAME: &str = "you-search";
const META_KEY: &str = "com.you.com/search";
pub const COUNT_MAX: usize = 50;
pub const SNIPPET_CAP: usize = 400;

const COUNTRIES: &[&str] = &[
    "AR", "AU", "AT", "BE", "BR", "CA", "CL", "DK", "FI", "FR", "DE", "HK", "IN", "ID", "IT", "JP",
    "KR", "MY", "MX", "NL", "NZ", "NO", "CN", "PL", "PT", "PH", "RU", "SA", "ZA", "ES", "SE", "CH",
    "TW", "TR", "GB", "US",
];

const LANGUAGES: &[&str] = &[
    "AR", "EU", "BN", "BG", "CA", "ZH-HANS", "ZH-HANT", "HR", "CS", "DA", "NL", "EN", "EN-GB",
    "ET", "FI", "FR", "GL", "DE", "EL", "GU", "HE", "HI", "HU", "IS", "IT", "JA", "KN", "KO", "LV",
    "LT", "MS", "ML", "MR", "NB", "PL", "PT-BR", "PT-PT", "PA", "RO", "RU", "SR", "SK", "SL", "ES",
    "SV", "TA", "TE", "TH", "TR", "UK", "VI",
];

/// Map `'cc-lang'` region token to You.com's accepted `{country?, language?}`.
pub fn region_meta(region: &str) -> Map<String, Value> {
    let lower = region.to_ascii_lowercase();
    let (cc, lang) = lower.split_once('-').unwrap_or((lower.as_str(), ""));
    let country = if cc == "uk" {
        "GB".to_owned()
    } else {
        cc.to_ascii_uppercase()
    };
    let mut language = lang.to_ascii_uppercase();
    if language == "ZH" {
        language = if matches!(country.as_str(), "TW" | "HK") {
            "ZH-HANT".to_owned()
        } else {
            "ZH-HANS".to_owned()
        };
    } else if language == "PT" {
        language = if country == "BR" {
            "PT-BR".to_owned()
        } else {
            "PT-PT".to_owned()
        };
    } else if language == "EN" && country == "GB" {
        language = "EN-GB".to_owned();
    }

    let mut out = Map::new();
    if COUNTRIES.contains(&country.as_str()) {
        out.insert("country".to_owned(), Value::String(country));
    }
    if LANGUAGES.contains(&language.as_str()) {
        out.insert("language".to_owned(), Value::String(language));
    }
    out
}

/// Construct the JSON-RPC `tools/call` request body for `q`.
pub fn build_request_body(q: &SerpQuery) -> Value {
    let query = if q
        .filetype
        .as_deref()
        .is_some_and(|ft| ft.eq_ignore_ascii_case("pdf"))
    {
        format!("{} filetype:pdf", q.query)
    } else {
        q.query.clone()
    };

    let mut args = Map::new();
    args.insert("query".to_owned(), Value::String(query.clone()));
    args.insert("count".to_owned(), json!(q.limit.clamp(1, COUNT_MAX)));
    args.insert("extraction".to_owned(), Value::String("none".to_owned()));
    args.insert(
        "safesearch".to_owned(),
        Value::String(q.safesearch.as_str().to_owned()),
    );
    if let Some(f) = q.freshness {
        args.insert(
            "freshness".to_owned(),
            Value::String(freshness_as_str(f).to_owned()),
        );
    }

    let mut meta = region_meta(&q.region);
    if !q.include_domains.is_empty() {
        meta.insert("include_domains".to_owned(), json!(q.include_domains));
    } else if !q.exclude_domains.is_empty() && !query.contains("site:") {
        args.insert("exclude_domains".to_owned(), json!(q.exclude_domains));
    }

    let mut outer_meta = Map::new();
    outer_meta.insert(META_KEY.to_owned(), Value::Object(meta));

    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": TOOL_NAME,
            "arguments": Value::Object(args),
            "_meta": Value::Object(outer_meta),
        }
    })
}

fn rpc_reply(body: &str) -> Option<Map<String, Value>> {
    let lines: Vec<&str> = if body.trim_start().starts_with('{') {
        vec![body]
    } else {
        body.lines()
            .filter_map(|line| line.strip_prefix("data:").map(str::trim))
            .collect()
    };
    for line in lines {
        if let Ok(Value::Object(msg)) = serde_json::from_str::<Value>(line)
            && (msg.contains_key("result") || msg.contains_key("error"))
        {
            return Some(msg);
        }
    }
    None
}

/// Extract the search payload JSON from a You.com MCP response (plain JSON or SSE).
///
/// Returns `Err(FsmError::Upstream)` when the RPC or tool explicitly reports an
/// error (quota spent, invalid params), and `Ok(None)` for malformed payloads.
pub fn parse_tool_payload(body: &str) -> Result<Option<Value>, FsmError> {
    let Some(reply) = rpc_reply(body) else {
        return Ok(None);
    };
    if let Some(err) = reply
        .get("error")
        .filter(|v| !v.is_null() && **v != Value::Bool(false))
    {
        let msg = if let Some(s) = err.as_str() {
            s.trim().to_owned()
        } else if let Some(s) = err
            .as_object()
            .and_then(|o| o.get("message"))
            .and_then(Value::as_str)
        {
            s.trim().to_owned()
        } else {
            err.to_string()
        };
        let clipped = clip_chars(&msg, 300);
        return Err(FsmError::upstream(
            "youcom",
            Some(200),
            format!("youcom: {clipped}"),
        ));
    }
    let Some(result) = reply.get("result").and_then(Value::as_object) else {
        return Ok(None);
    };
    let texts: Vec<&str> = result
        .get("content")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|block| {
                    let obj = block.as_object()?;
                    if obj.get("type").and_then(Value::as_str) == Some("text") {
                        obj.get("text").and_then(Value::as_str)
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        let joined = texts.iter().map(|t| t.trim()).collect::<Vec<_>>().join(" ");
        let clipped = clip_chars(&joined, 300);
        return Err(FsmError::upstream(
            "youcom",
            Some(200),
            format!("youcom: {clipped}"),
        ));
    }

    for text in texts {
        if let Ok(val) = serde_json::from_str::<Value>(text) {
            return Ok(Some(val));
        }
    }
    Ok(None)
}

fn text_field(v: Option<&Value>) -> String {
    v.and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_owned()
}

fn map_item(item: &Value, is_news: bool) -> Option<Hit> {
    let obj = item.as_object()?;
    let title = text_field(obj.get("title"));
    let url = text_field(obj.get("url"));
    if title.is_empty() || !(url.starts_with("http://") || url.starts_with("https://")) {
        return None;
    }
    let mut snippet = text_field(obj.get("description"));
    if snippet.is_empty()
        && let Some(first) = obj
            .get("snippets")
            .and_then(Value::as_array)
            .and_then(|arr| arr.first())
    {
        snippet = text_field(Some(first));
    }
    if snippet.chars().count() > SNIPPET_CAP {
        let head = clip_chars(&snippet, SNIPPET_CAP).trim_end();
        snippet = format!("{head} …");
    }
    let page_age = text_field(obj.get("page_age"));
    let date = if page_age.is_empty() {
        None
    } else {
        Some(
            page_age
                .split_once('T')
                .map_or(page_age.as_str(), |(d, _)| d)
                .to_owned(),
        )
    };
    let (kind, date_source) = if is_news {
        (HitKind::News, Some("api"))
    } else {
        (HitKind::Web, Some("snippet"))
    };
    Some(make_hit(
        "youcom",
        kind,
        title,
        url,
        snippet,
        date,
        date_source,
    ))
}

/// Map the inner You.com search JSON payload into `Hit`s.
pub fn map_results(payload: Option<&Value>) -> Vec<Hit> {
    let Some(groups) = payload
        .and_then(Value::as_object)
        .and_then(|o| o.get("results"))
        .and_then(Value::as_object)
    else {
        return Vec::new();
    };
    let mut results = Vec::new();
    for (section, is_news) in [("web", false), ("news", true)] {
        if let Some(items) = groups.get(section).and_then(Value::as_array) {
            for item in items {
                if let Some(hit) = map_item(item, is_news) {
                    results.push(hit);
                }
            }
        }
    }
    results
}

pub struct YouComEngine {
    decl: SourceDecl,
    deps: SourceDeps,
}

impl YouComEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let mut decl = SourceDecl::new(
            "youcom",
            "You.com",
            "You.com's own web and news index, keyless; about 100 searches a day per IP.",
        )
        .with_scopes(&["web"])
        .with_caps(&[Capability::Search])
        .with_op(search_op_decl(
            "Search the web and news via You.com's free MCP profile.",
            "tokio runtime",
            "youcom",
        ));
        decl.aliases = vec!["you".into()];
        decl.home_url = Some("https://you.com".into());
        decl.tier = 2;
        decl.auth = AuthMode::None;
        decl.transport = TransportProfile::Chrome;
        decl.pool_role = PoolRole::Reserve;
        decl.limits = SourceLimits::new(0.5, 2, 2, 5_000, 21_600);
        Self { decl, deps }
    }
}

#[async_trait]
impl Source for YouComEngine {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    async fn call(&self, op: &str, params: Value, cx: &SrcCx) -> Result<SrcOutput, FsmError> {
        let q = SerpQuery::from_call(&self.decl.id, op, &params, cx, &self.deps.cfg)?;
        let body_val = build_request_body(&q);
        let body_bytes = canonical_json(&body_val);
        let ncx = net_cx_for(cx, &q, &self.decl.id, self.decl.limits.rps);
        let req = HttpRequest::post(ENDPOINT, body_bytes)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("Accept-Language", &q.accept_language)
            .for_source(&self.decl.id);

        let resp = self.deps.net.send(req, self.decl.transport, &ncx).await?;
        if resp.status == 429 {
            return Err(FsmError::RateLimited {
                source_id: self.decl.id.clone(),
                retry_after_s: parse_retry_after(&resp),
            });
        }
        if resp.status != 200 {
            return Err(FsmError::upstream(
                &self.decl.id,
                Some(resp.status),
                format!("youcom: HTTP {}", resp.status),
            ));
        }

        let text = decode_body(&resp);
        let payload = parse_tool_payload(&text)?;
        let raw_hits = map_results(payload.as_ref());
        let hits = finalize_hits(
            raw_hits,
            &q.include_domains,
            &q.exclude_domains,
            q.filetype.as_deref(),
            q.limit,
        );
        Ok(SrcOutput::Hits(hits))
    }
}
