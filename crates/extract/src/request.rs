//! `web.request` implementation (N1 + all 5 hard guardrails) and shared
//! SSRF-checked manual-redirect & per-host GCRA helpers.
//!
//! 5 Hard Guardrails (`SPEC.md` §3.4):
//! 1. Tier-0 cookie-less `TransportProfile::Plain` ONLY + SSRF check + ≤ 5 manual redirects.
//! 2. `GET`/`HEAD`/`POST` only; `POST` body ≤ 128 KiB; allowed Content-Types:
//!    `application/json`, `application/x-www-form-urlencoded`, `application/sparql-query`,
//!    `text/plain` (no `multipart/form-data`); kill switch `SEARCH_MCP_EXEC_RAW_HTTP`.
//! 3. Strips `Authorization`, `Proxy-Authorization`, `Cookie`, `X-Api-Key`, `Api-Key`,
//!    `X-Auth-Token` unless host matches `SEARCH_MCP_EXEC_AUTH_HOSTS`; strips `Set-Cookie`
//!    on response.
//! 4. 4 MiB response cap + default 2 req/s/host GCRA.
//! 5. `exec` declares `readOnlyHint: false` (enforced by `fsm-server`).
//!
//! N1 `HttpResponse` rules (`SPEC.md` §3.2):
//! - Exposes `body: String` only (no `.text`).
//! - `json` is `Some(val)` iff Content-Type is `application/json` or `*+json` AND `body`
//!   parses as valid JSON.
//! - `HEAD` returns `body: ""` and `json: None`.
//! - Non-UTF-8 text decoded via `encoding_rs` + `chardetng`.
//! - Binary Content-Type fails with `InvalidInput("binary response; use web.download or doc.read")`.

use crate::charset::decode_body;
use fsm_core::config::RawHttpPolicy;
use fsm_core::url_canon::{host_matches, host_of};
use fsm_core::{
    Bytes, CallCx, FsmError, HttpBytes, HttpMethod, HttpRequest, HttpResponse, NetCx,
    RawRequestOpts, SourceDeps, TransportProfile,
};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use url::Url;

pub const MAX_REDIRECTS: usize = 5;
pub const MAX_POST_BODY_BYTES: usize = 128 * 1024; // 128 KiB
pub const MAX_RAW_RESPONSE_BYTES: u64 = 4 * 1024 * 1024; // 4 MiB

const STRIPPED_REQUEST_HEADERS: [&str; 6] = [
    "authorization",
    "proxy-authorization",
    "cookie",
    "x-api-key",
    "api-key",
    "x-auth-token",
];

const HOP_BY_HOP_REQUEST_HEADERS: [&str; 3] = ["host", "content-length", "transfer-encoding"];

const ALLOWED_POST_CONTENT_TYPES: [&str; 4] = [
    "application/json",
    "application/x-www-form-urlencoded",
    "application/sparql-query",
    "text/plain",
];

const REDIRECT_STATUSES: [u16; 5] = [301, 302, 303, 307, 308];

/// Verify `url` is a syntactically valid `http://` or `https://` URL and passes
/// the offline SSRF guard (layers 1–3).
pub fn check_http_url_offline(url: &str, deps: &SourceDeps) -> Result<Url, FsmError> {
    let trimmed = url.trim();
    let parsed = Url::parse(trimmed)
        .map_err(|e| FsmError::UnsafeUrl(format!("invalid URL {trimmed:?}: {e}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(FsmError::UnsafeUrl(format!(
            "scheme {:?} is not allowed; only http and https URLs are permitted",
            parsed.scheme()
        )));
    }
    deps.net.check_url_offline(trimmed)?;
    Ok(parsed)
}

/// Resolve a `Location` header relative to `base_url`.
pub fn resolve_redirect_location(base_url: &str, location: &str) -> Result<String, FsmError> {
    let base = Url::parse(base_url)
        .map_err(|e| FsmError::UnsafeUrl(format!("invalid base URL {base_url:?}: {e}")))?;
    let next = base
        .join(location.trim())
        .map_err(|e| FsmError::UnsafeUrl(format!("invalid redirect Location {location:?}: {e}")))?;
    Ok(next.into())
}

/// True if `media_type` (lowercased, parameter-free) represents a binary
/// resource that `web.request` must reject per N1.
pub fn is_binary_media_type(media_type: &str) -> bool {
    let m = media_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if m.is_empty() {
        return false;
    }
    if m.starts_with("image/svg") {
        return false;
    }
    if m.starts_with("image/")
        || m.starts_with("audio/")
        || m.starts_with("video/")
        || m.starts_with("font/")
    {
        return true;
    }
    matches!(
        m.as_str(),
        "application/octet-stream"
            | "application/pdf"
            | "application/zip"
            | "application/x-zip-compressed"
            | "application/gzip"
            | "application/x-gzip"
            | "application/x-tar"
            | "application/x-7z-compressed"
            | "application/x-rar"
            | "application/x-rar-compressed"
            | "application/wasm"
            | "application/msword"
            | "application/epub+zip"
            | "application/vnd.ms-excel"
            | "application/vnd.ms-powerpoint"
    ) || m.starts_with("application/vnd.openxmlformats-officedocument.")
}

/// True if `media_type` is JSON (`application/json` or `*+json`).
pub fn is_json_media_type(media_type: &str) -> bool {
    let m = media_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    m == "application/json" || m.ends_with("+json")
}

/// Parse an integer `Retry-After` header in seconds, if present.
pub fn parse_retry_after(resp: &HttpBytes) -> Option<u64> {
    resp.header("retry-after")?.trim().parse::<u64>().ok()
}

/// Enforce the M6 taint rule: once an exec is tainted by reading a local document,
/// outbound requests and redirect hops are only permitted to hosts already fetched
/// before the local document read (`cx.fetched_hosts`).
pub async fn check_tainted_host(url: &str, cx: &CallCx) -> Result<(), FsmError> {
    if !cx.is_tainted() {
        return Ok(());
    }
    let host = host_of(url);
    let allowed = !host.is_empty() && cx.fetched_hosts.lock().await.contains(&host);
    if !allowed {
        return Err(FsmError::PermissionDenied(format!(
            "redirect or outbound request to host {host:?} not fetched before the local document read"
        )));
    }
    Ok(())
}

/// Simple per-host GCRA (Generic Cell Rate Algorithm) rate limiter.
///
/// Default for `web.request`: 2 req/s/host (`interval = 500 ms`), burst = 4.
#[derive(Clone)]
pub struct HostRateLimiter {
    interval: Duration,
    burst_offset: Duration,
    tats: Arc<Mutex<HashMap<String, Instant>>>,
}

impl HostRateLimiter {
    pub fn new(rps: f64, burst: u32) -> Self {
        let interval = Duration::from_secs_f64(1.0 / rps.max(1e-6));
        let burst_offset = interval.saturating_mul(burst.saturating_sub(1));
        Self {
            interval,
            burst_offset,
            tats: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Wait until a token is available for `host`, respecting `cx.cancel` and `cx.remaining()`.
    ///
    /// Rejected calls (expired deadline or `wait > remaining`) never mutate the TAT,
    /// and mid-wait cancellation rolls back the reserved interval so later callers are
    /// not penalized by dropped waiters.
    pub async fn acquire(&self, host: &str, cx: &CallCx) -> Result<(), FsmError> {
        if cx.cancel.is_cancelled() {
            return Err(FsmError::Timeout(0));
        }
        let key = host.to_ascii_lowercase();
        let wait = {
            let mut map = self.tats.lock().await;
            let now = Instant::now();
            let tat = map.get(&key).copied().unwrap_or(now);
            let allow_at = tat.checked_sub(self.burst_offset).unwrap_or(now);
            let (wait, new_tat) = if now >= allow_at {
                (Duration::ZERO, tat.max(now) + self.interval)
            } else {
                (allow_at - now, tat + self.interval)
            };
            if !wait.is_zero() {
                let rem = cx.remaining();
                if rem.is_zero() || wait > rem {
                    return Err(FsmError::Timeout(rem.as_millis() as u64));
                }
            }
            // Bound map size gently in long-running processes.
            if map.len() > 4096 && !map.contains_key(&key) {
                map.retain(|_, v| *v > now);
            }
            map.insert(key.clone(), new_tat);
            wait
        };

        if wait.is_zero() {
            return Ok(());
        }
        tokio::select! {
            _ = cx.cancel.cancelled() => {
                let mut map = self.tats.lock().await;
                if let Some(cur) = map.get_mut(&key) {
                    if let Some(rolled) = cur.checked_sub(self.interval) {
                        *cur = rolled;
                    } else {
                        map.remove(&key);
                    }
                }
                Err(FsmError::Timeout(0))
            }
            _ = tokio::time::sleep(wait) => Ok(()),
        }
    }
}

/// Shared GET helper with manual redirect following (≤ 5 hops), per-hop offline
/// SSRF validation, per-hop M6 taint checks, and per-host rate-limit isolation.
/// Used by `structured`, `downloads`, and `fetch`.
pub async fn send_get_with_redirects(
    initial_url: &str,
    headers: &[(String, String)],
    profile: TransportProfile,
    max_bytes: u64,
    source_id: &str,
    deps: &SourceDeps,
    cx: &CallCx,
) -> Result<(String, HttpBytes), FsmError> {
    check_http_url_offline(initial_url, deps)?;
    check_tainted_host(initial_url, cx).await?;
    let mut current = initial_url.trim().to_owned();

    for hop in 0..=MAX_REDIRECTS {
        if cx.cancel.is_cancelled() {
            return Err(FsmError::Timeout(0));
        }
        let rem = cx.remaining();
        if rem.is_zero() {
            return Err(FsmError::Timeout(0));
        }
        let timeout = Duration::from_secs_f64(deps.cfg.fetch_timeout_s.max(0.5)).min(rem);
        let mut ncx = NetCx::new(cx.cancel.clone(), timeout);
        let host = host_of(&current);
        if !host.is_empty() {
            ncx.host_bucket = Some(format!("{source_id}:{host}"));
        }

        let mut req = HttpRequest::get(&current)
            .for_source(source_id)
            .with_max_bytes(max_bytes);
        req.follow_redirects = false;
        req.cookie_jar_id = None;
        for (k, v) in headers {
            req = req.header(k, v);
        }

        let resp = deps.net.send(req, profile, &ncx).await?;
        if REDIRECT_STATUSES.contains(&resp.status) {
            if hop == MAX_REDIRECTS {
                return Err(FsmError::upstream(
                    source_id,
                    Some(resp.status),
                    format!("too many redirects (>{MAX_REDIRECTS}) fetching {initial_url}"),
                ));
            }
            let loc = resp.header("location").ok_or_else(|| {
                FsmError::upstream(
                    source_id,
                    Some(resp.status),
                    format!("redirect with no Location header from {current}"),
                )
            })?;
            let next = resolve_redirect_location(&current, loc)?;
            check_http_url_offline(&next, deps)?;
            check_tainted_host(&next, cx).await?;
            current = next;
            continue;
        }
        let final_url = if resp.final_url.is_empty() {
            current
        } else {
            resp.final_url.clone()
        };
        return Ok((final_url, resp));
    }

    Err(FsmError::upstream(
        source_id,
        None,
        format!("too many redirects (>{MAX_REDIRECTS}) fetching {initial_url}"),
    ))
}

fn prepare_post_body(
    body: Option<Value>,
    caller_ctype: Option<&str>,
) -> Result<(Bytes, Option<String>), FsmError> {
    let Some(val) = body else {
        // POST with no body: still validate caller Content-Type if given.
        if let Some(ct) = caller_ctype {
            let media = ct
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            if !ALLOWED_POST_CONTENT_TYPES.contains(&media.as_str()) {
                return Err(FsmError::invalid(format!(
                    "disallowed POST Content-Type {media:?}; allowed: {}",
                    ALLOWED_POST_CONTENT_TYPES.join(", ")
                )));
            }
        }
        return Ok((Bytes::new(), None));
    };

    let (bytes, inferred_ctype) = if let Some(ct) = caller_ctype {
        let media = ct
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if !ALLOWED_POST_CONTENT_TYPES.contains(&media.as_str()) {
            return Err(FsmError::invalid(format!(
                "disallowed POST Content-Type {media:?}; allowed: {}",
                ALLOWED_POST_CONTENT_TYPES.join(", ")
            )));
        }
        let encoded = match media.as_str() {
            "application/json" => match val {
                Value::String(s) => s.into_bytes(),
                other => serde_json::to_vec(&other)
                    .map_err(|e| FsmError::invalid(format!("invalid JSON body: {e}")))?,
            },
            "application/x-www-form-urlencoded" => match val {
                Value::String(s) => s.into_bytes(),
                Value::Object(map) => {
                    let mut ser = url::form_urlencoded::Serializer::new(String::new());
                    for (k, v) in map {
                        let s = match v {
                            Value::String(s) => s,
                            Value::Null => String::new(),
                            other => other.to_string(),
                        };
                        ser.append_pair(&k, &s);
                    }
                    ser.finish().into_bytes()
                }
                _ => {
                    return Err(FsmError::invalid(
                        "application/x-www-form-urlencoded body must be a string or object",
                    ));
                }
            },
            "application/sparql-query" | "text/plain" => match val {
                Value::String(s) => s.into_bytes(),
                _ => {
                    return Err(FsmError::invalid(format!("{media} body must be a string")));
                }
            },
            _ => unreachable!(),
        };
        (Bytes::from(encoded), None)
    } else {
        match val {
            Value::String(s) => (
                Bytes::from(s.into_bytes()),
                Some("text/plain; charset=utf-8".to_owned()),
            ),
            other => {
                let encoded = serde_json::to_vec(&other)
                    .map_err(|e| FsmError::invalid(format!("invalid JSON body: {e}")))?;
                (Bytes::from(encoded), Some("application/json".to_owned()))
            }
        }
    };

    if bytes.len() > MAX_POST_BODY_BYTES {
        return Err(FsmError::invalid(format!(
            "POST body exceeds 128 KiB limit ({} bytes > {MAX_POST_BODY_BYTES})",
            bytes.len()
        )));
    }
    Ok((bytes, inferred_ctype))
}

fn filter_request_headers(
    headers: &[(String, String)],
    host: &str,
    auth_hosts: &[String],
) -> Vec<(String, String)> {
    let allow_auth = !auth_hosts.is_empty() && {
        let refs: Vec<&str> = auth_hosts.iter().map(String::as_str).collect();
        host_matches(host, &refs)
    };
    headers
        .iter()
        .filter(|(k, _)| {
            let lower = k.trim().to_ascii_lowercase();
            if HOP_BY_HOP_REQUEST_HEADERS.contains(&lower.as_str()) {
                return false;
            }
            if !allow_auth && STRIPPED_REQUEST_HEADERS.contains(&lower.as_str()) {
                return false;
            }
            true
        })
        .cloned()
        .collect()
}

/// Execute `web.request` enforcing N1 and all 5 hard guardrails.
pub async fn execute_raw_request(
    opts: RawRequestOpts,
    deps: &SourceDeps,
    limiter: &HostRateLimiter,
    cx: &CallCx,
) -> Result<HttpResponse, FsmError> {
    let mut method = opts.method.unwrap_or(HttpMethod::Get);

    // Guardrail 2: kill switch.
    match deps.cfg.exec_raw_http {
        RawHttpPolicy::Off => {
            return Err(FsmError::PermissionDenied(
                "web.request is disabled (SEARCH_MCP_EXEC_RAW_HTTP=off)".into(),
            ));
        }
        RawHttpPolicy::GetOnly if method == HttpMethod::Post => {
            return Err(FsmError::PermissionDenied(
                "POST via web.request is disabled (SEARCH_MCP_EXEC_RAW_HTTP=get-only; automatically set when DOCUMENT_ROOT is configured)".into(),
            ));
        }
        _ => {}
    }

    // Guardrail 1: scheme + offline SSRF check before any socket/limiter.
    check_http_url_offline(&opts.url, deps)?;
    check_tainted_host(&opts.url, cx).await?;

    // Validate caller headers and extract Content-Type if present.
    let mut base_headers: Vec<(String, String)> =
        opts.headers.unwrap_or_default().into_iter().collect();

    let caller_ctype = base_headers
        .iter()
        .rev()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v.clone());

    // Guardrail 2: method/body rules.
    let mut body_bytes: Option<Bytes> = match method {
        HttpMethod::Get | HttpMethod::Head => {
            if let Some(ref b) = opts.body
                && !b.is_null()
            {
                return Err(FsmError::invalid(format!(
                    "{} requests cannot carry a body",
                    method.as_str()
                )));
            }
            None
        }
        HttpMethod::Post => {
            let (bytes, inferred_ctype) = prepare_post_body(opts.body, caller_ctype.as_deref())?;
            if let Some(ct) = inferred_ctype {
                base_headers.push(("Content-Type".to_owned(), ct));
            }
            Some(bytes)
        }
        _ => {
            return Err(FsmError::invalid(
                "only GET, HEAD, and POST methods are allowed",
            ));
        }
    };

    let mut current_url = opts.url.trim().to_owned();
    let mut query_params: Vec<(String, String)> =
        opts.query.unwrap_or_default().into_iter().collect();

    for hop in 0..=MAX_REDIRECTS {
        if cx.cancel.is_cancelled() {
            return Err(FsmError::Timeout(0));
        }
        let rem = cx.remaining();
        if rem.is_zero() {
            return Err(FsmError::Timeout(0));
        }

        let host = host_of(&current_url);
        // Guardrail 4: default 2 req/s/host GCRA.
        limiter.acquire(&host, cx).await?;

        // Guardrail 3: strip auth/cookie headers unless host matches EXEC_AUTH_HOSTS.
        let filtered_headers =
            filter_request_headers(&base_headers, &host, &deps.cfg.exec_auth_hosts);

        let mut req = match method {
            HttpMethod::Get => HttpRequest::get(&current_url),
            HttpMethod::Head => HttpRequest::head(&current_url),
            HttpMethod::Post => {
                HttpRequest::post(&current_url, body_bytes.clone().unwrap_or_default())
            }
            _ => unreachable!(),
        };
        req.follow_redirects = false;
        req.cookie_jar_id = None; // Guardrail 1: cookie-less
        req.max_bytes = Some(MAX_RAW_RESPONSE_BYTES); // Guardrail 4: 4 MiB cap
        req.source_id = Some("web.request".to_owned());
        for (k, v) in filtered_headers {
            req = req.header(k, v);
        }
        // Only attach opts.query on the initial request; redirect Location already carries its own query.
        if hop == 0 {
            for (k, v) in &query_params {
                req = req.query_param(k, v);
            }
            query_params.clear();
        }

        let timeout =
            Duration::from_secs_f64(deps.cfg.fetch_timeout_s.max(0.5)).min(cx.remaining());
        let mut ncx = NetCx::new(cx.cancel.clone(), timeout);
        if !host.is_empty() {
            ncx.host_bucket = Some(format!("web.request:{host}"));
        }

        // Guardrail 1: Tier-0 Plain client ONLY.
        let resp = deps.net.send(req, TransportProfile::Plain, &ncx).await?;

        if REDIRECT_STATUSES.contains(&resp.status) {
            if hop == MAX_REDIRECTS {
                return Err(FsmError::upstream(
                    "web.request",
                    Some(resp.status),
                    format!("too many redirects (>{MAX_REDIRECTS})"),
                ));
            }
            let loc = resp.header("location").ok_or_else(|| {
                FsmError::upstream(
                    "web.request",
                    Some(resp.status),
                    format!("redirect with no Location header from {current_url}"),
                )
            })?;
            let next = resolve_redirect_location(&current_url, loc)?;
            check_http_url_offline(&next, deps)?;
            check_tainted_host(&next, cx).await?;
            // RFC 9110: 303 switches non-HEAD methods to GET (HEAD stays HEAD so body stays "");
            // 301/302 switch POST to GET and drop body.
            if (resp.status == 303 && method != HttpMethod::Head)
                || (matches!(resp.status, 301 | 302) && method == HttpMethod::Post)
            {
                method = HttpMethod::Get;
                body_bytes = None;
                base_headers.retain(|(k, _)| !k.eq_ignore_ascii_case("content-type"));
            }
            current_url = next;
            continue;
        }

        let final_url = if resp.final_url.is_empty() {
            current_url
        } else {
            resp.final_url.clone()
        };

        // Guardrail 3: strip Set-Cookie on response.
        let mut resp_headers: BTreeMap<String, String> = BTreeMap::new();
        for (k, v) in &resp.headers {
            let lower = k.to_ascii_lowercase();
            if lower == "set-cookie" || lower == "set-cookie2" {
                continue;
            }
            resp_headers
                .entry(lower)
                .and_modify(|existing| {
                    existing.push_str(", ");
                    existing.push_str(v);
                })
                .or_insert_with(|| v.clone());
        }

        let raw_ctype = resp.header("content-type").unwrap_or("").to_owned();
        let media = resp.media_type().unwrap_or_default();

        // N1: HEAD always returns empty body and no json.
        if method == HttpMethod::Head {
            let mut out = HttpResponse::new(final_url, resp.status, "");
            out.headers = resp_headers;
            out.json = None;
            out.truncated = false;
            return Ok(out);
        }

        // N1: Binary Content-Type MUST fail with exact InvalidInput message.
        if is_binary_media_type(&media) {
            return Err(FsmError::InvalidInput(
                "binary response; use web.download or doc.read".into(),
            ));
        }

        // Guardrail 4: enforce 4 MiB response cap defensively even if a mock NetClient ignored max_bytes.
        let cap = MAX_RAW_RESPONSE_BYTES as usize;
        let (slice, capped) = if resp.body.len() > cap {
            (&resp.body[..cap], true)
        } else {
            (resp.body.as_ref(), false)
        };
        let truncated = resp.truncated || capped;

        // N1: decode non-UTF-8 text via encoding_rs / chardetng.
        let body = decode_body(slice, &raw_ctype, Some(&final_url));

        // N1: json is Some iff Content-Type is application/json or *+json AND body parses as valid JSON.
        let json = if is_json_media_type(&media) {
            serde_json::from_str::<Value>(&body).ok()
        } else {
            None
        };

        let mut out = HttpResponse::new(final_url, resp.status, body);
        out.headers = resp_headers;
        out.json = json;
        out.truncated = truncated;
        return Ok(out);
    }

    Err(FsmError::upstream(
        "web.request",
        None,
        format!("too many redirects (>{MAX_REDIRECTS})"),
    ))
}
