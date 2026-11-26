//! Streamable HTTP transport with DNS-rebinding guards (`allowed_hosts` / `allowed_origins`)
//! and optional `SEARCH_MCP_HTTP_TOKEN` Bearer authentication (`SPEC.md` §2.1 #4, §3.4 I13).

use crate::mcp::{AppServer, AskToolInput};
use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use fsm_core::{AskMode, CallCx, Config, Freshness, FsmError};
use fsm_surface::render_error;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde::Deserialize;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Loopback host spellings admitted by the DNS-rebinding guard.
const LOOPBACK_HOSTS: [&str; 3] = ["127.0.0.1", "localhost", "[::1]"];

/// Wildcard bind addresses that MUST NEVER become allowed `Host` headers.
const WILDCARD_HOSTS: [&str; 4] = ["0.0.0.0", "::", "[::]", "*"];

/// Effective HTTP transport configuration (after CLI overrides).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpSecurityConfig {
    pub allowed_hosts: Vec<String>,
    pub allowed_origins: Vec<String>,
    pub enforce_origin_validation: bool,
    pub bearer_token: Option<String>,
}

/// Hard cap on `/v1/ask` JSON POST request body size (64 KiB).
const MAX_ASK_BODY_BYTES: usize = 64 * 1024;
/// Hard cap on `/v1/ask` question length in Unicode scalar values.
const MAX_ASK_QUESTION_CHARS: usize = 8_192;

fn is_loopback_host(host: &str) -> bool {
    let trimmed = host.trim();
    LOOPBACK_HOSTS
        .iter()
        .any(|h| h.eq_ignore_ascii_case(trimmed))
        || trimmed == "::1"
}

/// Build the DNS-rebinding and auth policy for a Streamable HTTP server (`SPEC.md` §2.1 #4).
///
/// - `allowed_hosts`: loopback (`127.0.0.1:<port>`, `localhost:<port>`, `[::1]:<port>`)
///   plus explicit non-wildcard bind host (`<host>:<port>`). Wildcard binds (`0.0.0.0`,
///   `::`) are NEVER added to `allowed_hosts`.
/// - `allowed_origins`: strictly from `SEARCH_MCP_HTTP_ALLOWED_ORIGINS` (empty by default),
///   with origin validation ALWAYS enforced so browser cross-origin requests are rejected
///   unless explicitly allowlisted.
pub fn build_http_security(host: &str, port: u16, cfg: &Config) -> HttpSecurityConfig {
    let mut allowed_hosts: Vec<String> = if port > 0 {
        LOOPBACK_HOSTS
            .iter()
            .map(|h| format!("{h}:{port}"))
            .collect()
    } else {
        LOOPBACK_HOSTS.iter().map(|h| (*h).to_owned()).collect()
    };

    let trimmed = host.trim();
    let is_loopback = is_loopback_host(trimmed);
    let is_wildcard = WILDCARD_HOSTS.contains(&trimmed);

    if !trimmed.is_empty() && !is_loopback && !is_wildcard {
        let formatted = if trimmed.contains(':') && !trimmed.starts_with('[') {
            format!("[{trimmed}]")
        } else {
            trimmed.to_owned()
        };
        let entry = if port > 0 {
            format!("{formatted}:{port}")
        } else {
            formatted
        };
        if !allowed_hosts.contains(&entry) {
            allowed_hosts.push(entry);
        }
    }

    let allowed_origins: Vec<String> = cfg
        .http_allowed_origins
        .iter()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();

    HttpSecurityConfig {
        allowed_hosts,
        allowed_origins,
        enforce_origin_validation: true,
        bearer_token: cfg
            .http_token
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
    }
}

/// Normalize the Streamable HTTP endpoint path (ensures a leading `/`, default `"/mcp"`).
pub fn normalize_http_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return "/mcp".to_owned();
    }
    if trimmed.starts_with('/') {
        trimmed.to_owned()
    } else {
        format!("/{trimmed}")
    }
}

/// Constant-time byte slice equality for bearer token verification.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Build an [`axum::Router`] serving `app` over Streamable HTTP at `path` plus `/v1/ask`
/// (GET `?q=...` and POST JSON) with full DNS-rebinding protection and optional Bearer token auth.
pub fn build_http_router(
    app: AppServer,
    path: &str,
    security: &HttpSecurityConfig,
    cancel: CancellationToken,
) -> Router {
    let mut rmcp_cfg = StreamableHttpServerConfig::default()
        .with_allowed_hosts(security.allowed_hosts.clone())
        .with_allowed_origins(security.allowed_origins.clone())
        .with_cancellation_token(cancel);
    if security.enforce_origin_validation {
        rmcp_cfg = rmcp_cfg.enforce_origin_validation();
    }

    let app_for_mcp = app.clone();
    let session_manager = Arc::new(LocalSessionManager::default());
    let service =
        StreamableHttpService::new(move || Ok(app_for_mcp.clone()), session_manager, rmcp_cfg);

    // `/v1/ask` is ONLY exposed on the HTTP transport and guarded by the same Host/Origin
    // DNS-rebinding policy and Bearer token guard as `/mcp` (ruling R5).
    let ask_security = Arc::new(security.clone());
    let ask_route = Router::new()
        .route("/v1/ask", get(http_ask_get).post(http_ask_post))
        .layer(DefaultBodyLimit::max(MAX_ASK_BODY_BYTES))
        .with_state(app)
        .layer(middleware::from_fn(
            move |req: Request<Body>, next: Next| {
                let sec = Arc::clone(&ask_security);
                async move { host_origin_guard_middleware(req, next, &sec).await }
            },
        ));

    let endpoint = normalize_http_path(path);
    let mut router = Router::new()
        .nest_service(&endpoint, service)
        .merge(ask_route);

    if let Some(expected) = security.bearer_token.clone() {
        let expected = Arc::new(expected);
        router = router.layer(middleware::from_fn(
            move |req: Request<Body>, next: Next| {
                let expected = Arc::clone(&expected);
                async move { bearer_auth_middleware(req, next, &expected).await }
            },
        ));
    }

    router
}

/// Enforce the same DNS-rebinding Host/Origin allowlist on `/v1/ask` that `rmcp` enforces on `/mcp`,
/// plus duplicate-header and `Sec-Fetch-Site: cross-site` CSRF guards for browser GET requests.
async fn host_origin_guard_middleware(
    req: Request<Body>,
    next: Next,
    security: &HttpSecurityConfig,
) -> Response {
    let mut host_iter = req.headers().get_all(header::HOST).iter();
    let host_val = match (host_iter.next(), host_iter.next()) {
        (Some(v), None) => v,
        _ => {
            return (
                StatusCode::FORBIDDEN,
                "Forbidden: Host header is not allowed",
            )
                .into_response();
        }
    };
    let host_hdr = host_val.to_str().map(str::trim).unwrap_or("");
    if host_hdr.is_empty() || !host_is_allowed(host_hdr, &security.allowed_hosts) {
        return (
            StatusCode::FORBIDDEN,
            "Forbidden: Host header is not allowed",
        )
            .into_response();
    }

    if security.enforce_origin_validation {
        let mut origin_iter = req.headers().get_all(header::ORIGIN).iter();
        let origin_allowlisted = match (origin_iter.next(), origin_iter.next()) {
            (None, _) => false,
            (Some(_), Some(_)) => {
                return (
                    StatusCode::FORBIDDEN,
                    "Forbidden: multiple Origin headers are not allowed",
                )
                    .into_response();
            }
            (Some(val), None) => {
                let Ok(origin) = val.to_str().map(str::trim) else {
                    return (StatusCode::FORBIDDEN, "Forbidden: Invalid Origin header")
                        .into_response();
                };
                if origin.is_empty() || !origin_is_allowed(origin, &security.allowed_origins) {
                    return (
                        StatusCode::FORBIDDEN,
                        "Forbidden: Origin header is not allowed",
                    )
                        .into_response();
                }
                true
            }
        };

        // Browsers omit `Origin` on cross-site GET (e.g. `<img>` or `fetch(..., {mode:'no-cors'})`)
        // but send `Sec-Fetch-Site: cross-site`. Refuse unless an allowlisted `Origin` matched.
        if !origin_allowlisted {
            for val in req.headers().get_all("sec-fetch-site") {
                match val.to_str().map(str::trim) {
                    Ok(s) if s.eq_ignore_ascii_case("cross-site") => {
                        return (
                            StatusCode::FORBIDDEN,
                            "Forbidden: cross-site request is not allowed",
                        )
                            .into_response();
                    }
                    Err(_) => {
                        return (
                            StatusCode::FORBIDDEN,
                            "Forbidden: invalid Sec-Fetch-Site header",
                        )
                            .into_response();
                    }
                    Ok(_) => {}
                }
            }
        }
    }

    next.run(req).await
}

fn host_is_allowed(host: &str, allowed: &[String]) -> bool {
    let h_lower = host.trim().to_ascii_lowercase();
    let (h_bare, _) = split_host_and_port(&h_lower);
    allowed.iter().any(|entry| {
        let e = entry.trim().to_ascii_lowercase();
        if e == h_lower {
            return true;
        }
        let (e_bare, e_port) = split_host_and_port(&e);
        e_port.is_none() && e_bare == h_bare
    })
}

/// Split a lowercased authority into `(canonical_host, optional_port)`.
/// IPv6 literals are normalized to their bracketed form (e.g. `[::1]`).
fn split_host_and_port(host: &str) -> (String, Option<&str>) {
    if let Some(rest) = host.strip_prefix('[') {
        if let Some((ip, after)) = rest.split_once(']') {
            let port = after.strip_prefix(':').filter(|p| !p.is_empty());
            return (format!("[{ip}]"), port);
        }
        return (host.to_owned(), None);
    }
    // Unbracketed IPv6 contains multiple colons (e.g. "::1").
    if host.bytes().filter(|&b| b == b':').count() > 1 {
        return (format!("[{host}]"), None);
    }
    match host.split_once(':') {
        Some((h, p)) => (h.to_owned(), (!p.is_empty()).then_some(p)),
        None => (host.to_owned(), None),
    }
}

fn origin_is_allowed(origin: &str, allowed: &[String]) -> bool {
    allowed
        .iter()
        .any(|a| a.trim().eq_ignore_ascii_case(origin))
}

#[derive(Debug, Deserialize)]
struct HttpAskQuery {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    question: Option<String>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    freshness: Option<String>,
    #[serde(default)]
    lang: Option<String>,
    #[serde(default)]
    domains: Option<String>,
    #[serde(default)]
    max_tokens: Option<i64>,
    #[serde(default)]
    format: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HttpAskBody {
    #[serde(default)]
    question: Option<String>,
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    mode: Option<AskMode>,
    #[serde(default)]
    freshness: Option<Freshness>,
    #[serde(default)]
    lang: Option<String>,
    #[serde(default)]
    domains: Option<Vec<String>>,
    #[serde(default)]
    max_tokens: Option<i64>,
    #[serde(default)]
    format: Option<String>,
}

async fn http_ask_get(
    State(app): State<AppServer>,
    headers: HeaderMap,
    Query(q): Query<HttpAskQuery>,
) -> Response {
    let question = q.q.or(q.question).unwrap_or_default();
    let mode = match q.mode.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => None,
        Some("auto") => Some(AskMode::Auto),
        Some("fast") => Some(AskMode::Fast),
        Some("agent") => Some(AskMode::Agent),
        Some(other) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("Invalid mode {other:?}; expected auto, fast, or agent"),
            )
                .into_response();
        }
    };
    let freshness = match q
        .freshness
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        None => None,
        Some("day") => Some(Freshness::Day),
        Some("week") => Some(Freshness::Week),
        Some("month") => Some(Freshness::Month),
        Some("year") => Some(Freshness::Year),
        Some(other) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("Invalid freshness {other:?}; expected day, week, month, or year"),
            )
                .into_response();
        }
    };
    let domains = q.domains.map(|d| {
        d.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>()
    });
    let want_json = wants_json(q.format.as_deref(), &headers);
    let input = AskToolInput {
        question,
        freshness,
        lang: q.lang,
        domains,
        mode,
        max_tokens: q.max_tokens,
    };
    run_http_ask(&app, input, want_json).await
}

async fn http_ask_post(
    State(app): State<AppServer>,
    headers: HeaderMap,
    body: Result<axum::Json<HttpAskBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let axum::Json(payload) = match body {
        Ok(b) => b,
        Err(axum::extract::rejection::JsonRejection::BytesRejection(br))
            if br.status() == StatusCode::PAYLOAD_TOO_LARGE =>
        {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                "Payload Too Large: /v1/ask request body exceeds 64 KiB",
            )
                .into_response();
        }
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("Invalid JSON body: {e}")).into_response();
        }
    };
    let question = payload.question.or(payload.q).unwrap_or_default();
    let want_json = wants_json(payload.format.as_deref(), &headers);
    let input = AskToolInput {
        question,
        freshness: payload.freshness,
        lang: payload.lang,
        domains: payload.domains,
        mode: payload.mode,
        max_tokens: payload.max_tokens,
    };
    run_http_ask(&app, input, want_json).await
}

fn wants_json(format_param: Option<&str>, headers: &HeaderMap) -> bool {
    if let Some(f) = format_param {
        return f.trim().eq_ignore_ascii_case("json");
    }
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.contains("application/json"))
}

async fn run_http_ask(app: &AppServer, input: AskToolInput, want_json: bool) -> Response {
    if input.question.chars().count() > MAX_ASK_QUESTION_CHARS {
        return (
            StatusCode::BAD_REQUEST,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            format!("Invalid input: question exceeds {MAX_ASK_QUESTION_CHARS} characters"),
        )
            .into_response();
    }
    let secs = if !app.services().cfg.agent_backend.eq_ignore_ascii_case("off") {
        app.services().cfg.agent_timeout_seconds.clamp(10.0, 180.0) + 5.0
    } else {
        10.0
    };
    let cx = CallCx::new(
        uuid::Uuid::new_v4().to_string(),
        Instant::now() + Duration::from_secs_f64(secs),
    );
    let mut resp = match app.execute_ask(input, &cx).await {
        Ok((resp, markdown)) => {
            if want_json {
                let body = serde_json::to_string(&resp).unwrap_or_else(|_| "{}".to_owned());
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
                    body,
                )
                    .into_response()
            } else {
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
                    markdown,
                )
                    .into_response()
            }
        }
        Err(err) => {
            let status = match &err {
                FsmError::InvalidInput(_) => StatusCode::BAD_REQUEST,
                FsmError::Timeout(_) => StatusCode::GATEWAY_TIMEOUT,
                FsmError::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
                FsmError::Internal { .. } => StatusCode::INTERNAL_SERVER_ERROR,
                _ => StatusCode::BAD_GATEWAY,
            };
            (
                status,
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                render_error(&err),
            )
                .into_response()
        }
    };
    resp.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

async fn bearer_auth_middleware(req: Request<Body>, next: Next, expected_token: &str) -> Response {
    let mut auth_iter = req.headers().get_all(header::AUTHORIZATION).iter();
    let authorized = match (auth_iter.next(), auth_iter.next()) {
        (Some(val), None) => val
            .to_str()
            .ok()
            .and_then(|s| {
                let s = s.trim();
                (s.len() >= 7 && s[..7].eq_ignore_ascii_case("bearer ")).then(|| &s[7..])
            })
            .is_some_and(|tok| constant_time_eq(tok.as_bytes(), expected_token.as_bytes())),
        _ => false,
    };

    if !authorized {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "Unauthorized: valid Bearer token required",
        )
            .into_response();
    }
    next.run(req).await
}

/// Bind and serve Streamable HTTP until `shutdown` resolves (or SIGINT/SIGTERM).
pub async fn serve_http(
    app: AppServer,
    host: &str,
    port: u16,
    path: &str,
    cfg: &Config,
    cancel: CancellationToken,
) -> Result<SocketAddr, FsmError> {
    let trimmed = host.trim();
    let bind_addr = if trimmed.contains(':') && !trimmed.starts_with('[') {
        format!("[{trimmed}]:{port}")
    } else {
        format!("{trimmed}:{port}")
    };
    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .map_err(|e| FsmError::internal("http_bind", format!("failed to bind {bind_addr}: {e}")))?;
    let local_addr = listener
        .local_addr()
        .map_err(|e| FsmError::internal("http_bind", format!("local_addr failed: {e}")))?;

    // Use the real bound port (so `--port 0` populates `allowed_hosts` with the chosen port).
    let security = build_http_security(trimmed, local_addr.port(), cfg);
    if !is_loopback_host(trimmed) && security.bearer_token.is_none() {
        tracing::warn!(
            addr = %local_addr,
            "streamable-http bound to non-loopback address without SEARCH_MCP_HTTP_TOKEN; set SEARCH_MCP_HTTP_TOKEN to require authentication"
        );
    }
    let router = build_http_router(app, path, &security, cancel.clone());
    let endpoint = normalize_http_path(path);

    tracing::info!(
        addr = %local_addr,
        path = %endpoint,
        token_auth = security.bearer_token.is_some(),
        "free-search-mcp listening over streamable-http"
    );

    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            cancel.cancelled().await;
        })
        .await
        .map_err(|e| FsmError::internal("http_serve", format!("HTTP server error: {e}")))?;

    Ok(local_addr)
}

#[cfg(test)]
mod security_tests {
    use super::*;
    use crate::{TransportKind, assemble_services};
    use axum::http::HeaderValue;
    use fsm_core::fixture::ReplayNet;
    use fsm_core::{NetClient, NullStore, StoreApi};
    use tower::ServiceExt;

    fn test_router(cfg: Config, host: &str, port: u16) -> Router {
        let net: Arc<dyn NetClient> = Arc::new(ReplayNet::from_fixtures(Vec::new()).unwrap());
        let store: Arc<dyn StoreApi> = Arc::new(NullStore::new());
        let services = assemble_services(Arc::new(cfg.clone()), net, store).unwrap();
        let app = AppServer::new(services, TransportKind::StreamableHttp).unwrap();
        let sec = build_http_security(host, port, &cfg);
        build_http_router(app, "/mcp", &sec, CancellationToken::new())
    }

    #[tokio::test]
    async fn ask_security_hardening_regression() {
        let router = test_router(Config::default(), "127.0.0.1", 8000);

        // 1. Cross-site GET without Origin (browser <img>/no-cors CSRF) -> 403
        let req_cross = Request::builder()
            .method("GET")
            .uri("/v1/ask?q=test")
            .header(header::HOST, "127.0.0.1:8000")
            .header("Sec-Fetch-Site", "cross-site")
            .body(Body::empty())
            .unwrap();
        let resp = router.clone().oneshot(req_cross).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "Sec-Fetch-Site: cross-site on GET /v1/ask must be rejected"
        );

        // 2. Empty Origin header on /v1/ask -> 403 (must not silently skip validation)
        let req_empty_origin = Request::builder()
            .method("GET")
            .uri("/v1/ask?q=test")
            .header(header::HOST, "127.0.0.1:8000")
            .header(header::ORIGIN, "")
            .body(Body::empty())
            .unwrap();
        let resp = router.clone().oneshot(req_empty_origin).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "empty Origin header must be rejected"
        );

        // 3. Non-UTF-8 Origin header -> 403
        let req_bad_utf8_origin = Request::builder()
            .method("GET")
            .uri("/v1/ask?q=test")
            .header(header::HOST, "127.0.0.1:8000")
            .header(
                header::ORIGIN,
                HeaderValue::from_bytes(b"\xff\xfe").unwrap(),
            )
            .body(Body::empty())
            .unwrap();
        let resp = router.clone().oneshot(req_bad_utf8_origin).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "non-UTF-8 Origin header must be rejected"
        );

        // 4. Duplicate Host header -> 403
        let req_dup_host = Request::builder()
            .method("GET")
            .uri("/v1/ask?q=test")
            .header(header::HOST, "127.0.0.1:8000")
            .header(header::HOST, "evil.example")
            .body(Body::empty())
            .unwrap();
        let resp = router.clone().oneshot(req_dup_host).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "duplicate Host header must be rejected"
        );

        // 5. Oversized POST /v1/ask body (> 64 KiB) -> 413 Payload Too Large
        let big_body = format!(r#"{{"q":"{}"}}"#, "a".repeat(128 * 1024));
        let req_big = Request::builder()
            .method("POST")
            .uri("/v1/ask")
            .header(header::HOST, "127.0.0.1:8000")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(big_body))
            .unwrap();
        let resp = router.clone().oneshot(req_big).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::PAYLOAD_TOO_LARGE,
            "oversized /v1/ask POST body must be rejected with 413"
        );

        // 6. IPv6 [::1]:8000 matches port==0 security config
        let sec_zero = build_http_security("::1", 0, &Config::default());
        assert!(
            host_is_allowed("[::1]:8000", &sec_zero.allowed_hosts),
            "IPv6 [::1]:8000 must match port==0 allowed_hosts {:?}",
            sec_zero.allowed_hosts
        );

        // 7. Case-insensitive Bearer scheme + duplicate Authorization rejected
        let mut tok_cfg = Config::default();
        tok_cfg.http_token = Some("secret-tok".into());
        let tok_router = test_router(tok_cfg, "127.0.0.1", 8000);

        let req_dup_auth = Request::builder()
            .method("GET")
            .uri("/v1/ask?q=")
            .header(header::HOST, "127.0.0.1:8000")
            .header(header::AUTHORIZATION, "Bearer secret-tok")
            .header(header::AUTHORIZATION, "Bearer wrong")
            .body(Body::empty())
            .unwrap();
        let resp = tok_router.clone().oneshot(req_dup_auth).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "duplicate Authorization header must be rejected"
        );

        let req_upper_bearer = Request::builder()
            .method("GET")
            .uri("/v1/ask?q=")
            .header(header::HOST, "127.0.0.1:8000")
            .header(header::AUTHORIZATION, "BEARER secret-tok")
            .body(Body::empty())
            .unwrap();
        let resp = tok_router.oneshot(req_upper_bearer).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "case-insensitive BEARER scheme should authenticate (reaching empty-q 400)"
        );
    }
}
