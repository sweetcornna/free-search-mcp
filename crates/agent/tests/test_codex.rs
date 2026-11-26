//! Port of `tests/test_codex.py`: Codex engine wiring, `/alpha/search`, `/responses`
//! SSE fallback, `clean_url`, `split_line`, 401 refresh retry, usage limits, model
//! catalogue selection/invalidation, and auto-signin guards.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use fsm_agent::engines::codex::{
    CodexEngine, clean_url, parse_client_version, pick_model, split_line,
};
use fsm_agent::oauth::{Credential, OAuthManager, OAuthProvider, now_unix};
use fsm_core::ext::w7::{is_lasting_agent_failure, scope_agent_lane};
use fsm_core::{Availability, Config, PoolRole, Source, SrcCx, SrcOutput};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::TempDir;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

fn make_jwt(claims: serde_json::Value) -> String {
    let h = URL_SAFE_NO_PAD.encode(r#"{"alg":"none"}"#);
    let p = URL_SAFE_NO_PAD.encode(claims.to_string());
    format!("{h}.{p}.sig")
}

fn signed_in_manager(cfg: &Config, token_url: &str) -> Arc<OAuthManager> {
    let mgr = Arc::new(OAuthManager::from_config(cfg));
    let mut spec = OAuthProvider::codex();
    spec.token_url = token_url.to_owned();
    mgr.override_provider(spec);
    let cred = Credential {
        provider: "codex".into(),
        access_token: "tok-1".into(),
        refresh_token: "ref-1".into(),
        expires_at: now_unix() + 3600.0,
        account_id: "acct-123".into(),
        email: "me@example.com".into(),
        plan: "plus".into(),
        source: "oauth".into(),
        ..Credential::default()
    };
    mgr.save(&cred).unwrap();
    mgr
}

#[test]
fn test_clean_url_split_line_and_pick_model() {
    // Strips ONLY utm_source=openai / chatgpt.com without re-encoding other params.
    assert_eq!(
        clean_url("https://example.org/p?a=1%2F2&utm_source=openai&b=3#top"),
        "https://example.org/p?a=1%2F2&b=3#top"
    );
    assert_eq!(
        clean_url("https://example.org/p?utm_source=chatgpt.com"),
        "https://example.org/p"
    );
    assert_eq!(
        clean_url("https://example.org/p?utm_source=google&k=v"),
        "https://example.org/p?utm_source=google&k=v"
    );

    // split_line cleans markdown links and extracts the cited line.
    let txt =
        "Intro line.\n- Rust 1.85 adds [Edition 2024](https://rust-lang.org) support.\nFooter.";
    let start = txt.find("Rust 1.85").unwrap();
    assert_eq!(
        split_line(txt, Some(start), Some(start + 10)),
        "Rust 1.85 adds Edition 2024 support."
    );

    // pick_model respects visibility, supported_in_api, and priority.
    let cat = json!({
        "models": [
            {"slug": "hidden-m", "visibility": "hide", "priority": 0},
            {"slug": "whisper-1", "visibility": "list", "priority": 1},
            {"slug": "gpt-5.3-codex", "visibility": "list", "priority": 2, "supported_in_api": true},
            {"slug": "gpt-5.1", "visibility": "list", "priority": 5, "supported_in_api": true}
        ]
    });
    assert_eq!(pick_model(&cat).as_deref(), Some("gpt-5.3-codex"));
    assert_eq!(
        parse_client_version("codex-cli 0.105.2\n").as_deref(),
        Some("0.105.2")
    );
}

#[test]
fn test_codex_wiring_and_availability() {
    let tmp = TempDir::new().unwrap();
    let mut cfg = Config::defaults_without_home();
    cfg.config_dir = tmp.path().to_path_buf();
    let mgr = Arc::new(OAuthManager::from_config(&cfg));
    let eng = CodexEngine::new(Arc::new(cfg.clone()), Arc::clone(&mgr), false);

    assert_eq!(eng.decl().id, "codex");
    assert_eq!(eng.decl().pool_role, PoolRole::Explicit);
    assert!(matches!(eng.availability(&cfg), Availability::Disabled(_)));

    mgr.save(&Credential {
        provider: "codex".into(),
        access_token: "a".into(),
        refresh_token: "r".into(),
        expires_at: now_unix() + 3600.0,
        account_id: "acct".into(),
        source: "oauth".into(),
        ..Credential::default()
    })
    .unwrap();
    assert_eq!(eng.availability(&cfg), Availability::Available);
}

#[tokio::test]
async fn test_alpha_search_primary_path() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/alpha/search"))
        .and(header("authorization", "Bearer tok-1"))
        .and(header("chatgpt-account-id", "acct-123"))
        .and(header("originator", "codex_cli_rs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [
                {
                    "title": "Tokio Docs",
                    "url": "https://tokio.rs/docs?utm_source=openai&v=1",
                    "snippet": "Async runtime for Rust.",
                    "published_date": "2026-01-15"
                }
            ]
        })))
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    let mut cfg = Config::defaults_without_home();
    cfg.config_dir = tmp.path().to_path_buf();
    cfg.codex_base_url = server.uri();
    let mgr = signed_in_manager(&cfg, &format!("{}/oauth/token", server.uri()));
    let eng = CodexEngine::new(Arc::new(cfg), mgr, false);

    let out = eng
        .call(
            "search",
            json!({"query": "tokio rust", "limit": 5}),
            &SrcCx::default(),
        )
        .await
        .unwrap();
    let SrcOutput::Hits(hits) = out else {
        panic!("expected Hits");
    };
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].url, "https://tokio.rs/docs?v=1");
    assert_eq!(hits[0].title, "Tokio Docs");
    assert_eq!(hits[0].date.as_deref(), Some("2026-01-15"));
}

#[tokio::test]
async fn test_responses_sse_fallback_and_catalogue_pick() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/alpha/search"))
        .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "models": [{"slug": "gpt-5.4-codex", "visibility": "list", "priority": 1}]
        })))
        .mount(&server)
        .await;

    let sse_body = concat!(
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"web_search_call\",\"action\":{\"sources\":[{\"url\":\"https://example.org/s1?utm_source=chatgpt.com\",\"title\":\"Source One\",\"snippet\":\"First source\"}]}}}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"See Rust release notes here.\",\"annotations\":[{\"type\":\"url_citation\",\"url\":\"https://blog.rust-lang.org/1.85?utm_source=openai\",\"title\":\"Rust 1.85\",\"start_index\":4,\"end_index\":22}]}]}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[]}}\n\n",
        "data: [DONE]\n\n"
    );

    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body),
        )
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    let mut cfg = Config::defaults_without_home();
    cfg.config_dir = tmp.path().to_path_buf();
    cfg.codex_base_url = server.uri();
    let mgr = signed_in_manager(&cfg, &format!("{}/oauth/token", server.uri()));
    let eng = CodexEngine::new(Arc::new(cfg), mgr, false).with_client_version("0.104.0");

    let out = eng
        .call(
            "search",
            json!({"query": "rust 1.85", "limit": 5}),
            &SrcCx::default(),
        )
        .await
        .unwrap();
    let SrcOutput::Hits(hits) = out else {
        panic!("expected Hits");
    };
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].url, "https://example.org/s1");
    assert_eq!(hits[1].url, "https://blog.rust-lang.org/1.85");
    assert!(hits[1].snippet.contains("Rust release notes"));
}

#[tokio::test]
async fn test_401_refreshes_once_and_usage_limit_is_lasting() {
    let server = MockServer::start().await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let att_clone = Arc::clone(&attempts);

    struct AuthRetry(Arc<AtomicUsize>);
    impl Respond for AuthRetry {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let n = self.0.fetch_add(1, Ordering::SeqCst);
            let auth = req
                .headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if n == 0 {
                assert_eq!(auth, "Bearer tok-1");
                ResponseTemplate::new(401).set_body_string("expired")
            } else {
                assert!(auth.starts_with("Bearer "));
                assert_ne!(auth, "Bearer tok-1");
                ResponseTemplate::new(200).set_body_json(json!({
                    "results": [{"title": "After Refresh", "url": "https://example.org/ok", "snippet": "ok"}]
                }))
            }
        }
    }

    Mock::given(method("POST"))
        .and(path("/alpha/search"))
        .respond_with(AuthRetry(att_clone))
        .mount(&server)
        .await;

    let new_access = make_jwt(json!({"exp": (now_unix() + 7200.0) as i64}));
    let new_id = make_jwt(json!({
        "email": "me@example.com",
        "https://api.openai.com/auth": {"chatgpt_account_id": "acct-123", "chatgpt_plan_type": "plus"}
    }));
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": new_access,
            "refresh_token": "ref-2",
            "id_token": new_id
        })))
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    let mut cfg = Config::defaults_without_home();
    cfg.config_dir = tmp.path().to_path_buf();
    cfg.codex_base_url = server.uri();
    let mgr = signed_in_manager(&cfg, &format!("{}/oauth/token", server.uri()));
    let eng = CodexEngine::new(Arc::new(cfg), Arc::clone(&mgr), false);

    let out = eng
        .call(
            "search",
            json!({"query": "refresh test"}),
            &SrcCx::default(),
        )
        .await
        .unwrap();
    let SrcOutput::Hits(hits) = out else { panic!() };
    assert_eq!(hits[0].title, "After Refresh");
    assert_eq!(mgr.load("codex").unwrap().refresh_token, "ref-2");

    // Usage limit on another server must classify as lasting agent failure (1-strike bench).
    let limit_srv = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/alpha/search"))
        .respond_with(
            ResponseTemplate::new(429).set_body_json(
                json!({"error": {"code": "usage_limit_reached", "message": "limit"}}),
            ),
        )
        .mount(&limit_srv)
        .await;
    let mut cfg2 = Config::defaults_without_home();
    cfg2.config_dir = tmp.path().to_path_buf();
    cfg2.codex_base_url = limit_srv.uri();
    let eng2 = CodexEngine::new(Arc::new(cfg2), Arc::clone(&mgr), false);
    let err = eng2
        .call("search", json!({"query": "limit"}), &SrcCx::default())
        .await
        .unwrap_err();
    assert!(
        is_lasting_agent_failure(&err),
        "usage limit must be lasting: {err}"
    );
}

#[tokio::test]
async fn test_auto_signin_never_fires_in_agent_lane_or_http() {
    let tmp = TempDir::new().unwrap();
    let mut cfg = Config::defaults_without_home();
    cfg.config_dir = tmp.path().to_path_buf();
    cfg.codex_auto_signin = true;
    let mgr = Arc::new(OAuthManager::from_config(&cfg));
    let opened = Arc::new(AtomicUsize::new(0));
    let opened_clone = Arc::clone(&opened);
    mgr.set_browser_opener(move |_| {
        opened_clone.fetch_add(1, Ordering::SeqCst);
        true
    });

    // 1. Inside agent lane -> never launches browser!
    let eng_stdio = CodexEngine::new(Arc::new(cfg.clone()), Arc::clone(&mgr), false);
    let err1 = scope_agent_lane(eng_stdio.call("search", json!({"query": "q"}), &SrcCx::default()))
        .await
        .unwrap_err();
    assert!(is_lasting_agent_failure(&err1));
    assert_eq!(opened.load(Ordering::SeqCst), 0);

    // 2. On HTTP transport -> never launches browser!
    let eng_http = CodexEngine::new(Arc::new(cfg), Arc::clone(&mgr), true);
    let err2 = eng_http
        .call("search", json!({"query": "q"}), &SrcCx::default())
        .await
        .unwrap_err();
    assert!(is_lasting_agent_failure(&err2));
    assert_eq!(opened.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_alpha_search_sends_codex_cli_schema_and_catalogue_pick_matches_python() {
    use fsm_agent::engines::codex::DEFAULT_CLIENT_VERSION;
    use fsm_core::Freshness;

    assert!(DEFAULT_CLIENT_VERSION >= "0.155.0");

    // pick_model picks the newest generation's lightest (highest priority / last) search-capable model,
    // skipping retired (`upgrade`) and non-search (`supports_search_tool: false`) models.
    let cat = json!({
        "models": [
            {"slug": "gpt-5.4-codex", "visibility": "list", "priority": 5},
            {"slug": "gpt-6", "visibility": "list", "priority": 0, "supports_search_tool": true},
            {"slug": "gpt-6-mini", "visibility": "list", "priority": 1, "supports_search_tool": true},
            {"slug": "gpt-6-luna", "visibility": "list", "priority": 2, "supports_search_tool": true},
            {"slug": "gpt-6-retired", "visibility": "list", "priority": 9, "upgrade": "gpt-7"},
            {"slug": "gpt-6-nosearch", "visibility": "list", "priority": 9, "supports_search_tool": false}
        ]
    });
    assert_eq!(pick_model(&cat).as_deref(), Some("gpt-6-luna"));

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(cat))
        .mount(&server)
        .await;

    struct InspectAlpha;
    impl Respond for InspectAlpha {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            assert!(
                req.headers.get("session-id").is_some(),
                "missing session-id header"
            );
            assert!(
                req.headers
                    .get("user-agent")
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|ua| ua.starts_with("free-search-mcp/")),
                "missing free-search-mcp User-Agent"
            );
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            assert!(
                body["id"].as_str().is_some_and(|s| !s.is_empty()),
                "missing required parameter 'id': {body}"
            );
            assert_eq!(body["model"], "gpt-6-luna");
            assert_eq!(body["commands"]["search_query"][0]["q"], "rust async");
            assert_eq!(body["commands"]["search_query"][0]["recency"], 7);
            assert_eq!(
                body["commands"]["search_query"][0]["domains"],
                json!(["rust-lang.org"])
            );
            assert_eq!(body["settings"]["external_web_access"], true);
            assert_eq!(body["settings"]["allowed_callers"], json!(["direct"]));
            assert_eq!(body["settings"]["search_context_size"], "low");
            assert_eq!(body["max_output_tokens"], 10000);

            ResponseTemplate::new(200).set_body_json(json!({
                "results": [
                    {"type": "image_result", "url": "https://img.example/a.png", "title": "Skip Me"},
                    {
                        "type": "text_result",
                        "title": "Rust Async Book",
                        "url": "https://rust-lang.org/async?utm_source=openai",
                        "snippet": "Published 2026-02-01. Async programming in Rust."
                    }
                ]
            }))
        }
    }

    Mock::given(method("POST"))
        .and(path("/alpha/search"))
        .respond_with(InspectAlpha)
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    let mut cfg = Config::defaults_without_home();
    cfg.config_dir = tmp.path().to_path_buf();
    cfg.codex_base_url = server.uri();
    // "latest" must follow the catalogue rather than pinning literal "latest"!
    cfg.codex_model = "latest".into();
    let mgr = signed_in_manager(&cfg, &format!("{}/oauth/token", server.uri()));
    let eng = CodexEngine::new(Arc::new(cfg), mgr, false).with_client_version("0.155.0");

    let mut cx = SrcCx::default();
    cx.freshness = Some(Freshness::Week);
    cx.domains = vec!["rust-lang.org".into(), "-spam.example".into()];
    let out = eng
        .call("search", json!({"query": "rust async", "limit": 5}), &cx)
        .await
        .unwrap();
    let SrcOutput::Hits(hits) = out else {
        panic!("expected Hits");
    };
    assert_eq!(hits.len(), 1, "non-text_result must be filtered out");
    assert_eq!(hits[0].url, "https://rust-lang.org/async");
    assert_eq!(hits[0].date.as_deref(), Some("2026-02-01"));
}

#[tokio::test]
async fn test_codex_multibyte_error_body_and_oversized_sse_bounded() {
    // 1. Non-ASCII error body spanning byte 240 must not panic.
    let server = MockServer::start().await;
    let err_body = format!("{}€€€€€", "x".repeat(239));
    Mock::given(method("POST"))
        .and(path("/alpha/search"))
        .respond_with(ResponseTemplate::new(400).set_body_string(err_body))
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    let mut cfg = Config::defaults_without_home();
    cfg.config_dir = tmp.path().to_path_buf();
    cfg.codex_base_url = server.uri();
    let mgr = signed_in_manager(&cfg, &format!("{}/oauth/token", server.uri()));
    let eng = CodexEngine::new(Arc::new(cfg), mgr, false);

    let err = eng
        .call("search", json!({"query": "utf8"}), &SrcCx::default())
        .await
        .expect_err("HTTP 400 must fail cleanly without panic");
    assert!(err.to_string().contains("400"), "got {err}");

    // 2. Oversized newline-free SSE line on /responses must be rejected by cap.
    let server2 = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/alpha/search"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server2)
        .await;
    let huge_sse_line = format!("data: {}", "A".repeat(2 * 1024 * 1024 + 64));
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_string(huge_sse_line))
        .mount(&server2)
        .await;

    let tmp2 = TempDir::new().unwrap();
    let mut cfg2 = Config::defaults_without_home();
    cfg2.config_dir = tmp2.path().to_path_buf();
    cfg2.codex_base_url = server2.uri();
    let mgr2 = signed_in_manager(&cfg2, &format!("{}/oauth/token", server2.uri()));
    let eng2 = CodexEngine::new(Arc::new(cfg2), mgr2, false);

    let err2 = eng2
        .call("search", json!({"query": "huge sse"}), &SrcCx::default())
        .await
        .expect_err("oversized SSE line must be rejected");
    assert!(err2.to_string().contains("cap"), "got {err2}");
}
