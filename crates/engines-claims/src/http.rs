//! Shared HTTP, parameter parsing, and `Hit` construction helpers for
//! `fsm-engines-claims`.

use encoding_rs::Encoding;
use fsm_core::fixture::canonical_json;
use fsm_core::{
    Capability, FsmError, Hit, HitKind, HttpBytes, HttpRequest, NetCx, OpDecl, Param, ParamType,
    PoolRole, ProbeSpec, ReturnKind, SourceDecl, SourceDeps, SourceLimits, SrcCx, SrcOutput,
    TransportProfile,
};
use serde_json::Value;
use std::time::Duration;
use tokio::time::Instant;

/// Honest, generic User-Agent for plain API calls (contact-aware when configured).
pub(crate) fn user_agent(cx: &SrcCx) -> String {
    match cx
        .contact_email
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(email) => format!("free-search-mcp/1.0 ({email})"),
        None => "free-search-mcp/1.0".to_owned(),
    }
}

/// Validate `op == "search"` and extract `(query, limit)` from `params`.
pub(crate) fn parse_search_params(
    source_id: &str,
    op: &str,
    params: &Value,
    default_limit: usize,
    max_limit: usize,
) -> Result<(String, usize), FsmError> {
    if op != "search" {
        return Err(FsmError::invalid(format!(
            "source '{source_id}' has no op '{op}'; call '{source_id}.search'"
        )));
    }
    let query = params
        .get("query")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            FsmError::invalid(format!(
                "source '{source_id}.search' requires string parameter 'query'"
            ))
        })?
        .to_owned();
    let limit = match params.get("limit") {
        None | Some(Value::Null) => default_limit,
        Some(v) => {
            let n = v.as_u64().ok_or_else(|| {
                FsmError::invalid(format!(
                    "source '{source_id}.search' parameter 'limit' must be a positive integer"
                ))
            })?;
            usize::try_from(n).unwrap_or(max_limit)
        }
    }
    .clamp(1, max_limit);
    Ok((query, limit))
}

/// Build a standard `SourceDecl` for a claim or direct-fact engine.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_source_decl(
    id: &str,
    name: &str,
    summary: &str,
    aliases: &[&str],
    home_url: &str,
    scopes: &[&str],
    pool_role: PoolRole,
    limits: SourceLimits,
    example: &str,
    probe: ProbeSpec,
) -> SourceDecl {
    let mut decl = SourceDecl::new(id, name, summary)
        .with_scopes(scopes)
        .with_caps(&[Capability::Search, Capability::Lookup])
        .with_op(
            OpDecl::new("search", summary, ReturnKind::Hits)
                .with_param(
                    Param::new(
                        "query",
                        ParamType::String,
                        "Natural-language or identifier query",
                    )
                    .required(),
                )
                .with_param(
                    Param::new("limit", ParamType::Int, "Maximum results to return")
                        .with_default(serde_json::json!(10))
                        .with_range(1.0, 50.0),
                )
                .with_example(example)
                .with_probe(probe),
        );
    decl.aliases = aliases.iter().map(|s| (*s).to_owned()).collect();
    decl.home_url = Some(home_url.to_owned());
    decl.transport = TransportProfile::Plain;
    decl.pool_role = pool_role;
    decl.limits = limits;
    decl
}

/// Build a direct-fact `Hit` with consistent source attribution and `HitKind::Record`.
pub(crate) fn make_hit(
    source_id: &str,
    title: impl Into<String>,
    url: impl Into<String>,
    snippet: impl Into<String>,
    date: Option<String>,
) -> Hit {
    let date = date.filter(|d| !d.is_empty());
    let mut hit = Hit::new(title, url);
    hit.snippet = snippet.into();
    hit.date_source = date.as_ref().map(|_| "api".to_owned());
    hit.date = date;
    hit.source = source_id.to_owned();
    hit.sources = vec![source_id.to_owned()];
    hit.kind = HitKind::Record;
    hit
}

/// Wrap hits into `SrcOutput::Hits`.
pub(crate) fn hits_output(hits: Vec<Hit>) -> SrcOutput {
    SrcOutput::Hits(hits)
}

/// Shared deadline budget across all sequential/parallel HTTP requests of one `Source::call`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CallBudget {
    started: Instant,
    total: Duration,
}

impl CallBudget {
    pub(crate) fn from_cx(cx: &SrcCx, decl: &SourceDecl) -> Self {
        let total = if !cx.timeout.is_zero() {
            cx.timeout
        } else if decl.limits.timeout_ms > 0 {
            Duration::from_millis(decl.limits.timeout_ms)
        } else {
            Duration::from_secs(5)
        };
        Self {
            started: Instant::now(),
            total,
        }
    }

    pub(crate) fn total_ms(&self) -> u64 {
        u64::try_from(self.total.as_millis()).unwrap_or(u64::MAX)
    }

    /// Build a [`NetCx`] carrying the remaining budget, or fail fast on cancel / expiry.
    pub(crate) fn net_cx(&self, cx: &SrcCx, decl: &SourceDecl) -> Result<NetCx, FsmError> {
        if cx.cancel.is_cancelled() {
            return Err(FsmError::Timeout(0));
        }
        let elapsed = self.started.elapsed();
        if elapsed >= self.total {
            return Err(FsmError::Timeout(self.total_ms()));
        }
        let remaining = self.total.saturating_sub(elapsed);
        let mut ncx = NetCx::new(cx.cancel.clone(), remaining);
        ncx.host_bucket = decl
            .limits
            .host_bucket
            .clone()
            .or_else(|| Some(decl.id.clone()));
        if decl.limits.rps > 0.0 {
            ncx.rps = Some(decl.limits.rps);
        }
        Ok(ncx)
    }
}

/// Decode HTTP response bytes to UTF-8 text honoring any `Content-Type` charset.
fn decode_body(resp: &HttpBytes) -> String {
    if let Some(ct) = resp.header("content-type")
        && let Some(charset) = extract_charset(ct)
        && let Some(enc) = Encoding::for_label(charset.as_bytes())
    {
        let (decoded, _, _) = enc.decode(&resp.body);
        return decoded.into_owned();
    }
    String::from_utf8_lossy(&resp.body).into_owned()
}

fn extract_charset(content_type: &str) -> Option<&str> {
    for part in content_type.split(';').skip(1) {
        let Some((k, v)) = part.split_once('=') else {
            continue;
        };
        if k.trim().eq_ignore_ascii_case("charset") {
            let cs = v.trim().trim_matches('"').trim_matches('\'');
            if !cs.is_empty() {
                return Some(cs);
            }
        }
    }
    None
}

fn parse_retry_after(resp: &HttpBytes) -> Option<u64> {
    resp.header("retry-after")
        .and_then(|v| v.trim().parse::<u64>().ok())
}

fn build_get_request(
    decl: &SourceDecl,
    url: impl Into<String>,
    extra_headers: &[(&str, &str)],
    max_bytes: Option<u64>,
) -> HttpRequest {
    let mut req = HttpRequest::get(url)
        .header("Accept", "application/json")
        .for_source(&decl.id);
    for (k, v) in extra_headers {
        req = req.header(*k, *v);
    }
    if let Some(cap) = max_bytes {
        req = req.with_max_bytes(cap);
    }
    req
}

/// Send a strict JSON `GET` request. Any non-2xx (including 404) and any empty
/// non-204 body returns `Err(FsmError::Upstream | RateLimited)`.
pub(crate) async fn get_json(
    deps: &SourceDeps,
    decl: &SourceDecl,
    cx: &SrcCx,
    budget: &CallBudget,
    url: impl Into<String>,
    extra_headers: &[(&str, &str)],
    max_bytes: Option<u64>,
) -> Result<Option<Value>, FsmError> {
    let req = build_get_request(decl, url, extra_headers, max_bytes);
    send_json(deps, decl, cx, budget, req, false).await
}

/// Send a lookup-by-key JSON `GET` request where HTTP `404` legitimately means
/// "resource not found" (`Ok(None)`). All other non-2xx and empty non-204 bodies
/// return an error.
pub(crate) async fn get_json_opt(
    deps: &SourceDeps,
    decl: &SourceDecl,
    cx: &SrcCx,
    budget: &CallBudget,
    url: impl Into<String>,
    extra_headers: &[(&str, &str)],
    max_bytes: Option<u64>,
) -> Result<Option<Value>, FsmError> {
    let req = build_get_request(decl, url, extra_headers, max_bytes);
    send_json(deps, decl, cx, budget, req, true).await
}

/// Send a strict JSON `POST` request with canonical sorted-key JSON body.
pub(crate) async fn post_json(
    deps: &SourceDeps,
    decl: &SourceDecl,
    cx: &SrcCx,
    budget: &CallBudget,
    url: impl Into<String>,
    body: &Value,
) -> Result<Option<Value>, FsmError> {
    let wire = canonical_json(body);
    let req = HttpRequest::post(url, wire)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .for_source(&decl.id);
    send_json(deps, decl, cx, budget, req, false).await
}

async fn send_json(
    deps: &SourceDeps,
    decl: &SourceDecl,
    cx: &SrcCx,
    budget: &CallBudget,
    req: HttpRequest,
    allow_404: bool,
) -> Result<Option<Value>, FsmError> {
    let ncx = budget.net_cx(cx, decl)?;
    let remaining = ncx.timeout;
    let resp = tokio::time::timeout(remaining, deps.net.send(req, decl.transport, &ncx))
        .await
        .map_err(|_| {
            if cx.cancel.is_cancelled() {
                FsmError::Timeout(0)
            } else {
                FsmError::Timeout(budget.total_ms())
            }
        })??;
    if allow_404 && resp.status == 404 {
        return Ok(None);
    }
    if resp.status == 429 {
        return Err(FsmError::RateLimited {
            source_id: decl.id.clone(),
            retry_after_s: parse_retry_after(&resp),
        });
    }
    if !resp.is_success() {
        let preview: String = decode_body(&resp).chars().take(200).collect();
        return Err(FsmError::upstream(
            &decl.id,
            Some(resp.status),
            format!("HTTP {}: {}", resp.status, preview.trim()),
        ));
    }
    if resp.status == 204 && resp.body.is_empty() {
        return Ok(None);
    }
    let text = decode_body(&resp);
    let val: Value = serde_json::from_str(&text).map_err(|e| {
        FsmError::upstream(&decl.id, Some(resp.status), format!("invalid JSON: {e}"))
    })?;
    Ok(Some(val))
}

/// Check whether an error is a fatal harness/cancellation signal that must never
/// be swallowed even when fanning out across multiple sub-requests.
pub(crate) fn is_fatal_fanout_error(err: &FsmError) -> bool {
    matches!(err, FsmError::Internal { .. } | FsmError::Timeout(0))
}

/// Percent-encode a single path segment (RFC 3986 unreserved + `-._~`, matching
/// Python `urllib.parse.quote(s, safe="")`).
pub(crate) fn encode_path_segment(s: &str) -> String {
    use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
    // Python `quote(s, safe="")` leaves A-Z a-z 0-9 _ . - ~ unencoded.
    const PATH_SEG: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~');
    utf8_percent_encode(s, PATH_SEG).to_string()
}

/// Percent-encode a path keeping `/` unencoded (matching Python default `quote(s)`).
pub(crate) fn encode_path_keep_slash(s: &str) -> String {
    use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
    const PATH_KEEP_SLASH: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~')
        .remove(b'/');
    utf8_percent_encode(s, PATH_KEEP_SLASH).to_string()
}

/// Form-urlencoded query value (space -> `+`, matching Python `quote_plus`).
pub(crate) fn encode_query_plus(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// Return the first JSON value among `keys` that is Python-truthy for scalar
/// metadata (neither `null` nor an empty string `""`).
pub(crate) fn first_truthy_value<'a>(
    obj: &'a serde_json::Map<String, Value>,
    keys: &[&str],
) -> Option<&'a Value> {
    keys.iter().find_map(|k| {
        obj.get(*k).filter(|v| match v {
            Value::Null => false,
            Value::String(s) => !s.is_empty(),
            _ => true,
        })
    })
}

/// Return the first non-empty JSON string among `keys` (matching Python's
/// `obj.get(a) or obj.get(b)` before stripping whitespace).
pub(crate) fn first_nonempty_str<'a>(
    obj: &'a serde_json::Map<String, Value>,
    keys: &[&str],
) -> Option<&'a str> {
    keys.iter().find_map(|k| {
        obj.get(*k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    })
}
