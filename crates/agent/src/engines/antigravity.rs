//! Google Antigravity (Cloud Code Assist) grounded web-search engine
//! (port of `src/search_mcp/engines/antigravity.py`).
//!
//! Opt-in only (excluded from `SEARCH_MCP_AGENT_ENGINES=auto`) and NEVER
//! auto-opens a browser sign-in page.

use crate::oauth::{OAuthError, OAuthManager, antigravity_headers};
use async_trait::async_trait;
use fsm_core::ext::w7::agent_lasting_error;
use fsm_core::{
    AuthMode, Availability, Capability, Config, FsmError, Hit, HitKind, OpDecl, Param, ParamType,
    PoolRole, ReturnKind, Source, SourceDecl, SourceLimits, SrcCx, SrcOutput,
};
use futures::future::join_all;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const DEFAULT_FALLBACK_MODEL: &str = "gemini-3.8-flash-tiered";
const CATALOG_OK_TTL: Duration = Duration::from_secs(6 * 3600);
const CATALOG_FAIL_TTL: Duration = Duration::from_secs(5 * 60);
const REDIRECT_HOST: &str = "vertexaisearch.cloud.google.com";
const REDIRECT_TIMEOUT: Duration = Duration::from_secs(5);

struct ModelCache {
    models: Vec<String>,
    fetched_at: Instant,
    ttl: Duration,
}

/// Google Antigravity grounded search engine implementing [`fsm_core::Source`].
pub struct AntigravityEngine {
    decl: SourceDecl,
    cfg: Arc<Config>,
    oauth: Arc<OAuthManager>,
    http: reqwest::Client,
    redirect_http: reqwest::Client,
    model_cache: Mutex<Option<ModelCache>>,
}

impl AntigravityEngine {
    pub fn new(cfg: Arc<Config>, oauth: Arc<OAuthManager>) -> Self {
        let http = oauth.http.clone();
        Self::with_http(cfg, oauth, http.clone(), http)
    }

    pub fn with_http(
        cfg: Arc<Config>,
        oauth: Arc<OAuthManager>,
        http: reqwest::Client,
        redirect_http: reqwest::Client,
    ) -> Self {
        let mut limits = SourceLimits::new(
            10.0 / 60.0,
            2,
            1,
            (cfg.antigravity_timeout_s * 1000.0) as u64,
            60,
        );
        limits.host_bucket = Some("antigravity".to_owned());
        let mut decl = SourceDecl::new(
            "antigravity",
            "Google Antigravity Search",
            "Opt-in Google Antigravity grounded search (OAuth PKCE via `free-search-mcp login antigravity`).",
        )
        .with_scopes(&["web"])
        .with_caps(&[Capability::Search])
        .with_op(
            OpDecl::new("search", "Search the web via Antigravity", ReturnKind::Hits)
                .with_param(Param::new("query", ParamType::String, "Search query").required())
                .with_param(
                    Param::new("limit", ParamType::Int, "Maximum results")
                        .with_default(json!(10))
                        .with_range(1.0, 25.0),
                )
                .with_example(r#"source.call("antigravity.search", {query: "rust async runtime"})"#),
        );
        decl.pool_role = PoolRole::Explicit;
        decl.auth = AuthMode::None;
        decl.limits = limits;

        Self {
            decl,
            cfg,
            oauth,
            http,
            redirect_http,
            model_cache: Mutex::new(None),
        }
    }

    pub fn invalidate_model_cache(&self) {
        if let Ok(mut guard) = self.model_cache.lock() {
            *guard = None;
        }
    }

    async fn acquire_credential(
        &self,
        force_refresh: bool,
    ) -> Result<crate::oauth::Credential, FsmError> {
        match self.oauth.credential("antigravity", force_refresh).await {
            Ok(cred) => Ok(cred),
            Err(OAuthError::NotSignedIn(msg)) => {
                // Antigravity NEVER auto-opens a browser sign-in page.
                Err(agent_lasting_error("antigravity", Some(401), msg))
            }
            Err(OAuthError::Unreachable(msg)) => Err(FsmError::upstream("antigravity", None, msg)),
            Err(OAuthError::SignInPending(msg)) => {
                Err(FsmError::upstream("antigravity", None, msg))
            }
            Err(OAuthError::Failed(msg)) => Err(agent_lasting_error("antigravity", Some(401), msg)),
        }
    }

    async fn resolve_models(
        &self,
        cred: &crate::oauth::Credential,
        timeout: Duration,
    ) -> (Vec<String>, bool) {
        let pinned = self.cfg.antigravity_model.trim();
        if !pinned.is_empty() {
            return (vec![pinned.to_owned()], true);
        }
        if let Ok(guard) = self.model_cache.lock()
            && let Some(entry) = guard.as_ref()
            && entry.fetched_at.elapsed() < entry.ttl
        {
            return (entry.models.clone(), false);
        }

        let headers = antigravity_headers(&cred.access_token, &self.cfg.antigravity_version);
        let mut fetched: Vec<String> = Vec::new();
        for base in &self.cfg.antigravity_base_urls {
            let url = format!(
                "{}/v1internal:fetchAvailableModels",
                base.trim_end_matches('/')
            );
            let mut body = json!({});
            if !cred.account_id.is_empty() {
                body["project"] = Value::String(cred.account_id.clone());
            }
            let mut req = self
                .http
                .post(&url)
                .timeout(timeout.min(Duration::from_secs(10)))
                .json(&body);
            for (k, v) in &headers {
                req = req.header(k.as_str(), v.as_str());
            }
            if let Ok(resp) = req.send().await
                && resp.status().is_success()
                && let Ok(val) = resp.json::<Value>().await
            {
                let picked = pick_models(&val);
                if !picked.is_empty() {
                    fetched = picked;
                    break;
                }
            }
        }

        let (models, ttl) = if fetched.is_empty() {
            (vec![DEFAULT_FALLBACK_MODEL.to_owned()], CATALOG_FAIL_TTL)
        } else {
            (fetched, CATALOG_OK_TTL)
        };
        if let Ok(mut guard) = self.model_cache.lock() {
            *guard = Some(ModelCache {
                models: models.clone(),
                fetched_at: Instant::now(),
                ttl,
            });
        }
        (models, false)
    }

    async fn generate_grounded(
        &self,
        query: &str,
        limit: usize,
        model: &str,
        is_pinned: bool,
        cred: &crate::oauth::Credential,
        timeout: Duration,
    ) -> Result<Vec<Hit>, AgAttemptErr> {
        let headers = antigravity_headers(&cred.access_token, &self.cfg.antigravity_version);
        let mut body = json!({
            "model": model,
            "request": {
                "contents": [{
                    "role": "user",
                    "parts": [{
                        "text": format!("Search the web for: {query}\nGive a concise factual summary grounded in web search results.")
                    }]
                }],
                "tools": [{"googleSearch": {}}],
                "generationConfig": {"temperature": 0.0}
            }
        });
        if !cred.account_id.is_empty() {
            body["project"] = Value::String(cred.account_id.clone());
        }

        let mut last_err: Option<AgAttemptErr> = None;
        for base in &self.cfg.antigravity_base_urls {
            let url = format!("{}/v1internal:generateContent", base.trim_end_matches('/'));
            let mut req = self.http.post(&url).timeout(timeout).json(&body);
            for (k, v) in &headers {
                req = req.header(k.as_str(), v.as_str());
            }
            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    last_err = Some(AgAttemptErr::Transient(format!(
                        "antigravity network error on {base}: {e}"
                    )));
                    continue;
                }
            };
            let status = resp.status().as_u16();
            let text = if (200..300).contains(&status) {
                match crate::oauth::read_response_text_capped(
                    resp,
                    crate::oauth::MAX_AGENT_HTTP_BODY_BYTES,
                    false,
                )
                .await
                {
                    Ok(t) => t,
                    Err(e) => {
                        return Err(AgAttemptErr::Transient(format!("antigravity {e}")));
                    }
                }
            } else {
                crate::oauth::read_response_text_capped(
                    resp,
                    crate::oauth::MAX_AGENT_ERR_BODY_BYTES,
                    true,
                )
                .await
                .unwrap_or_default()
            };

            if status == 401 {
                return Err(AgAttemptErr::Unauthorized);
            }
            if status == 403 {
                return Err(AgAttemptErr::Lasting(
                    Some(403),
                    format!("antigravity refused (HTTP 403): {}", compact_error(&text)),
                ));
            }
            if status == 400 || status == 404 {
                if is_model_refused(&text) {
                    self.invalidate_model_cache();
                    if is_pinned {
                        return Err(AgAttemptErr::Lasting(
                            Some(status),
                            format!(
                                "pinned antigravity model {model:?} refused: {}",
                                compact_error(&text)
                            ),
                        ));
                    }
                    return Err(AgAttemptErr::Transient(format!(
                        "catalogue antigravity model {model:?} refused: {}",
                        compact_error(&text)
                    )));
                }
                // Stop failover on 400/404 (not transient host outage).
                return Err(AgAttemptErr::Lasting(
                    Some(status),
                    format!("antigravity HTTP {status}: {}", compact_error(&text)),
                ));
            }
            if status == 429 {
                last_err = Some(AgAttemptErr::RateLimited);
                continue;
            }
            if status >= 500 {
                last_err = Some(AgAttemptErr::Transient(format!(
                    "antigravity HTTP {status}: {}",
                    compact_error(&text)
                )));
                continue;
            }
            if !(200..300).contains(&status) {
                return Err(AgAttemptErr::Transient(format!(
                    "antigravity HTTP {status}: {}",
                    compact_error(&text)
                )));
            }

            let val: Value = serde_json::from_str(&text).map_err(|e| {
                AgAttemptErr::Transient(format!("antigravity invalid JSON response: {e}"))
            })?;
            let hits = parse_grounding(&val, limit);
            return Ok(hits);
        }

        Err(last_err.unwrap_or_else(|| {
            AgAttemptErr::Transient("antigravity: no base URLs configured".to_owned())
        }))
    }

    async fn search_with_cred(
        &self,
        query: &str,
        limit: usize,
        cred: &crate::oauth::Credential,
        timeout: Duration,
    ) -> Result<Vec<Hit>, AgAttemptErr> {
        let (models, is_pinned) = self.resolve_models(cred, timeout).await;
        let first = models
            .first()
            .map(String::as_str)
            .unwrap_or(DEFAULT_FALLBACK_MODEL);
        let mut hits = self
            .generate_grounded(query, limit, first, is_pinned, cred, timeout)
            .await?;

        // Retry once with second model if ungrounded (empty groundingMetadata).
        if hits.is_empty()
            && let Some(second) = models.get(1)
        {
            hits = self
                .generate_grounded(query, limit, second, is_pinned, cred, timeout)
                .await?;
        }

        if !hits.is_empty() {
            resolve_redirects(&self.redirect_http, &mut hits, limit).await;
        }
        Ok(hits)
    }
}

enum AgAttemptErr {
    Unauthorized,
    RateLimited,
    Transient(String),
    Lasting(Option<u16>, String),
}

impl AgAttemptErr {
    fn into_fsm_error(self) -> FsmError {
        match self {
            Self::Unauthorized => agent_lasting_error(
                "antigravity",
                Some(401),
                "antigravity rejected the refreshed sign-in; run `free-search-mcp login antigravity` to sign in again",
            ),
            Self::RateLimited => FsmError::RateLimited {
                source_id: "antigravity".to_owned(),
                retry_after_s: None,
            },
            Self::Transient(msg) => FsmError::upstream("antigravity", None, msg),
            Self::Lasting(status, msg) => agent_lasting_error("antigravity", status, msg),
        }
    }
}

#[async_trait]
impl Source for AntigravityEngine {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    fn availability(&self, _cfg: &Config) -> Availability {
        if self.oauth.is_signed_in("antigravity") {
            Availability::Available
        } else {
            Availability::Disabled(
                "not signed in; run `free-search-mcp login antigravity`".to_owned(),
            )
        }
    }

    async fn call(&self, op: &str, params: Value, cx: &SrcCx) -> Result<SrcOutput, FsmError> {
        if op != "search" {
            return Err(FsmError::invalid(format!(
                "antigravity only supports op \"search\", got {op:?}"
            )));
        }
        let query = params
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if query.is_empty() {
            return Err(FsmError::invalid("query must not be empty"));
        }
        let limit = params
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| n.clamp(1, 25) as usize)
            .unwrap_or(10);
        let max_timeout = Duration::from_secs_f64(self.cfg.antigravity_timeout_s.max(1.0));
        let timeout = if cx.timeout.is_zero() {
            max_timeout
        } else {
            cx.timeout.min(max_timeout)
        };

        let cred = self.acquire_credential(false).await?;
        match self.search_with_cred(query, limit, &cred, timeout).await {
            Ok(hits) => Ok(SrcOutput::Hits(hits)),
            Err(AgAttemptErr::Unauthorized) => {
                let refreshed = self.acquire_credential(true).await?;
                match self
                    .search_with_cred(query, limit, &refreshed, timeout)
                    .await
                {
                    Ok(hits) => Ok(SrcOutput::Hits(hits)),
                    Err(e) => Err(e.into_fsm_error()),
                }
            }
            Err(e) => Err(e.into_fsm_error()),
        }
    }
}

// ---------------------------------------------------------------------------
// Pure helpers ported from antigravity.py
// ---------------------------------------------------------------------------

/// Pick ordered web-search-capable models from `:fetchAvailableModels` output.
pub fn pick_models(payload: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    if let Some(ids) = payload.get("webSearchModelIds").and_then(Value::as_array) {
        for v in ids {
            if let Some(s) = v.as_str().map(str::trim).filter(|s| !s.is_empty())
                && seen.insert(s.to_owned())
            {
                out.push(s.to_owned());
            }
        }
    }

    if let Some(models_obj) = payload.get("models").and_then(Value::as_object) {
        // Prefer Gemini flash models first, then other Gemini models.
        let mut flash = Vec::new();
        let mut other = Vec::new();
        for (id, info) in models_obj {
            if info.get("isInternal").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            let lower = id.to_ascii_lowercase();
            if !lower.contains("gemini") {
                continue;
            }
            if lower.contains("flash") {
                flash.push(id.clone());
            } else {
                other.push(id.clone());
            }
        }
        flash.sort();
        flash.reverse();
        other.sort();
        other.reverse();
        for id in flash.into_iter().chain(other) {
            if seen.insert(id.clone()) {
                out.push(id);
            }
        }
    }

    out
}

/// Parse `groundingMetadata` (`groundingChunks` + `groundingSupports`) from a
/// Gemini/Cloud Code Assist response using **UTF-8 byte offsets** and `partIndex`,
/// skipping `thought: true` parts.
pub fn parse_grounding(payload: &Value, limit: usize) -> Vec<Hit> {
    let root = payload.get("response").unwrap_or(payload);
    let Some(candidate) = root
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
    else {
        return Vec::new();
    };
    let Some(gm) = candidate.get("groundingMetadata") else {
        return Vec::new();
    };
    let Some(chunks) = gm.get("groundingChunks").and_then(Value::as_array) else {
        return Vec::new();
    };

    // Map original partIndex -> text for non-thought parts.
    let parts = candidate
        .get("content")
        .and_then(|c| c.get("parts"))
        .and_then(Value::as_array);
    let mut part_texts: HashMap<usize, &str> = HashMap::new();
    let mut first_answer_part: Option<usize> = None;
    if let Some(parts) = parts {
        for (idx, part) in parts.iter().enumerate() {
            if part.get("thought").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            if let Some(txt) = part.get("text").and_then(Value::as_str) {
                part_texts.insert(idx, txt);
                if first_answer_part.is_none() {
                    first_answer_part = Some(idx);
                }
            }
        }
    }

    // Collect grounded snippet segments per chunk index.
    let mut chunk_snippets: HashMap<usize, Vec<String>> = HashMap::new();
    if let Some(supports) = gm.get("groundingSupports").and_then(Value::as_array) {
        for sup in supports {
            let Some(seg) = sup.get("segment") else {
                continue;
            };
            let snippet = if let Some(t) = seg
                .get("text")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                t.to_owned()
            } else {
                let pidx = seg
                    .get("partIndex")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize)
                    .or(first_answer_part)
                    .unwrap_or(0);
                let Some(&text) = part_texts.get(&pidx) else {
                    continue;
                };
                let start = seg
                    .get("startIndex")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize)
                    .unwrap_or(0);
                let end = seg
                    .get("endIndex")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize)
                    .unwrap_or(text.len());
                slice_utf8_bytes(text, start, end)
            };
            if snippet.is_empty() {
                continue;
            }
            if let Some(indices) = sup.get("groundingChunkIndices").and_then(Value::as_array) {
                for ci in indices {
                    if let Some(idx) = ci.as_u64().map(|n| n as usize) {
                        let list = chunk_snippets.entry(idx).or_default();
                        if !list.contains(&snippet) {
                            list.push(snippet.clone());
                        }
                    }
                }
            }
        }
    }

    let mut hits = Vec::new();
    let mut seen_urls = HashSet::new();
    for (idx, chunk) in chunks.iter().enumerate() {
        let Some(web) = chunk.get("web") else {
            continue;
        };
        let uri = web.get("uri").and_then(Value::as_str).unwrap_or("").trim();
        if uri.is_empty() || !seen_urls.insert(uri.to_owned()) {
            continue;
        }
        let title = web
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let snippet = chunk_snippets
            .remove(&idx)
            .map(|parts| parts.join(" "))
            .unwrap_or_default();

        let mut hit = Hit::new(if title.is_empty() { uri } else { title }, uri);
        hit.snippet = snippet;
        hit.source = "antigravity".to_owned();
        hit.sources = vec!["antigravity".to_owned()];
        hit.kind = HitKind::Web;
        hits.push(hit);
        if hits.len() >= limit {
            break;
        }
    }
    hits
}

/// Safely slice a UTF-8 string using byte offsets (Google's `groundingSupports`
/// `startIndex`/`endIndex` are UTF-8 byte offsets), snapping to valid UTF-8 char boundaries.
pub fn slice_utf8_bytes(text: &str, start: usize, end: usize) -> String {
    let bytes = text.as_bytes();
    let mut s = start.min(bytes.len());
    let mut e = end.min(bytes.len());
    if e < s {
        return String::new();
    }
    while s < bytes.len() && !text.is_char_boundary(s) {
        s += 1;
    }
    while e > s && !text.is_char_boundary(e) {
        e -= 1;
    }
    if s >= e {
        return String::new();
    }
    text[s..e].trim().to_owned()
}

/// Resolve `vertexaisearch.cloud.google.com` redirect URLs in parallel without
/// sending any `Authorization` header (HEAD first, fallback to GET on 405).
pub async fn resolve_redirects(http: &reqwest::Client, hits: &mut [Hit], limit: usize) {
    let cap = (limit * 2).max(10);
    let mut targets: Vec<(usize, String)> = Vec::new();
    for (idx, hit) in hits.iter().enumerate() {
        if targets.len() >= cap {
            break;
        }
        if is_vertex_redirect(&hit.url) {
            targets.push((idx, hit.url.clone()));
        }
    }
    if targets.is_empty() {
        return;
    }

    let futs = targets.into_iter().map(|(idx, url)| {
        let http = http.clone();
        async move {
            let resolved = resolve_one_redirect(&http, &url).await;
            (idx, resolved)
        }
    });
    for (idx, resolved) in join_all(futs).await {
        if let Some(target_url) = resolved {
            hits[idx].url = target_url;
        }
    }
}

fn is_vertex_redirect(url: &str) -> bool {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.eq_ignore_ascii_case(REDIRECT_HOST)))
        .unwrap_or(false)
}

async fn resolve_one_redirect(http: &reqwest::Client, url: &str) -> Option<String> {
    let resp = http.head(url).timeout(REDIRECT_TIMEOUT).send().await.ok()?;
    if resp.status().is_redirection()
        && let Some(loc) = resp.headers().get(reqwest::header::LOCATION)
        && let Ok(s) = loc.to_str()
        && (s.starts_with("http://") || s.starts_with("https://"))
    {
        return Some(s.to_owned());
    }
    if resp.status().as_u16() == 405 {
        let resp2 = http.get(url).timeout(REDIRECT_TIMEOUT).send().await.ok()?;
        if resp2.status().is_redirection()
            && let Some(loc) = resp2.headers().get(reqwest::header::LOCATION)
            && let Ok(s) = loc.to_str()
            && (s.starts_with("http://") || s.starts_with("https://"))
        {
            return Some(s.to_owned());
        }
    }
    None
}

fn is_model_refused(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("not_found")
        || lower.contains("model")
            && (lower.contains("not found")
                || lower.contains("unsupported")
                || lower.contains("invalid"))
}

fn compact_error(text: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(text)
        && let Some(msg) = v
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
    {
        return msg.to_owned();
    }
    let trimmed = text.trim();
    let clipped = fsm_core::text::clip_chars(trimmed, 240);
    if clipped.len() < trimmed.len() {
        format!("{clipped}...")
    } else {
        trimmed.to_owned()
    }
}
