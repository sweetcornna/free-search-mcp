//! Port of `tests/test_antigravity.py`: Antigravity engine wiring, platform headers,
//! UTF-8 byte offsets + `partIndex` + `thought: true` skipping, redirect resolution
//! without `Authorization`, ungrounded second-model retry, multi-host failover, and 401 retry.

use fsm_agent::engines::antigravity::{
    AntigravityEngine, parse_grounding, pick_models, slice_utf8_bytes,
};
use fsm_agent::oauth::{
    Credential, OAuthManager, OAuthProvider, antigravity_headers_for, now_unix,
};
use fsm_core::ext::w7::is_lasting_agent_failure;
use fsm_core::{Availability, Config, PoolRole, Source, SrcCx, SrcOutput};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::TempDir;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

fn signed_in_ag(cfg: &Config, token_url: &str) -> Arc<OAuthManager> {
    let mgr = Arc::new(OAuthManager::from_config(cfg));
    let mut spec = OAuthProvider::antigravity();
    spec.token_url = token_url.to_owned();
    spec.client_id = "cid".into();
    spec.client_secret = "csec".into();
    mgr.override_provider(spec);
    mgr.save(&Credential {
        provider: "antigravity".into(),
        access_token: "ya29.token1".into(),
        refresh_token: "g-ref1".into(),
        expires_at: now_unix() + 3600.0,
        account_id: "proj-1".into(),
        email: "me@gmail.com".into(),
        plan: "Google AI Pro".into(),
        source: "oauth".into(),
        client_id: "cid".into(),
        client_secret: "csec".into(),
        ..Credential::default()
    })
    .unwrap();
    mgr
}

#[test]
fn test_antigravity_headers_pick_models_and_utf8_grounding() {
    for (plat, arch, ua_suffix, meta_plat) in [
        ("darwin", "aarch64", "darwin/arm64", "DARWIN_ARM64"),
        ("linux", "x86_64", "linux/amd64", "LINUX_AMD64"),
        ("linux", "aarch64", "linux/arm64", "LINUX_ARM64"),
        ("windows", "x86_64", "windows/amd64", "WINDOWS_AMD64"),
    ] {
        let h = antigravity_headers_for("TOK", "1.107.0", plat, arch);
        assert_eq!(h["User-Agent"], format!("antigravity/1.107.0 {ua_suffix}"));
        let meta: serde_json::Value = serde_json::from_str(&h["Client-Metadata"]).unwrap();
        assert_eq!(meta["platform"], meta_plat);
        assert_eq!(meta["ideType"], "ANTIGRAVITY");
        assert_eq!(meta["pluginType"], "GEMINI");
        assert_eq!(h["Authorization"], "Bearer TOK");
    }

    // UTF-8 byte offsets + skipping thought part (partIndex=0 is thought, partIndex=1 is answer).
    let text = "中文前缀 Rust 1.85 发布于 2025年2月20日。";
    let start = text.find("Rust").unwrap();
    let end = text.len();
    assert_eq!(
        slice_utf8_bytes(text, start, end),
        "Rust 1.85 发布于 2025年2月20日。"
    );

    let payload = json!({
        "response": {
            "candidates": [{
                "content": {
                    "parts": [
                        {"thought": true, "text": "internal thought should never be sliced"},
                        {"text": text}
                    ]
                },
                "groundingMetadata": {
                    "groundingChunks": [
                        {"web": {"uri": "https://blog.rust-lang.org/1.85", "title": "Rust 1.85 Blog"}}
                    ],
                    "groundingSupports": [
                        {
                            "segment": {"partIndex": 1, "startIndex": start, "endIndex": end},
                            "groundingChunkIndices": [0]
                        }
                    ]
                }
            }]
        }
    });
    let hits = parse_grounding(&payload, 5);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].title, "Rust 1.85 Blog");
    assert_eq!(hits[0].url, "https://blog.rust-lang.org/1.85");
    assert_eq!(hits[0].snippet, "Rust 1.85 发布于 2025年2月20日。");

    // pick_models prefers webSearchModelIds first.
    let avail = json!({
        "webSearchModelIds": ["gemini-3.8-flash-tiered", "gemini-3.5-pro"],
        "models": {
            "gemini-2.5-flash": {"isInternal": false},
            "internal-only": {"isInternal": true}
        }
    });
    let picked = pick_models(&avail);
    assert_eq!(picked[0], "gemini-3.8-flash-tiered");
    assert_eq!(picked[1], "gemini-3.5-pro");
    assert!(picked.contains(&"gemini-2.5-flash".to_owned()));
    assert!(!picked.contains(&"internal-only".to_owned()));
}

#[tokio::test]
async fn test_failover_on_5xx_ungrounded_retry_and_403_lasting() {
    let bad_host = MockServer::start().await;
    let good_host = MockServer::start().await;

    // First host returns 503 -> must fail over to second host.
    Mock::given(method("POST"))
        .and(path("/v1internal:generateContent"))
        .respond_with(ResponseTemplate::new(503).set_body_string("busy"))
        .mount(&bad_host)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1internal:fetchAvailableModels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "webSearchModelIds": ["model-a", "model-b"]
        })))
        .mount(&good_host)
        .await;

    // First model ("model-a") answers ungrounded; engine must retry once with "model-b".
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_c = Arc::clone(&calls);
    struct GroundedRetry(Arc<AtomicUsize>);
    impl Respond for GroundedRetry {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let n = self.0.fetch_add(1, Ordering::SeqCst);
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            if n == 0 {
                assert_eq!(body["model"], "model-a");
                ResponseTemplate::new(200).set_body_json(json!({
                    "response": {"candidates": [{"content": {"parts": [{"text": "no grounding"}]}}]}
                }))
            } else {
                assert_eq!(body["model"], "model-b");
                ResponseTemplate::new(200).set_body_json(json!({
                    "response": {
                        "candidates": [{
                            "content": {"parts": [{"text": "Rust is memory safe."}]},
                            "groundingMetadata": {
                                "groundingChunks": [{"web": {"uri": "https://rust-lang.org", "title": "Rust"}}],
                                "groundingSupports": [{"segment": {"partIndex": 0, "startIndex": 0, "endIndex": 20}, "groundingChunkIndices": [0]}]
                            }
                        }]
                    }
                }))
            }
        }
    }

    Mock::given(method("POST"))
        .and(path("/v1internal:generateContent"))
        .and(header("authorization", "Bearer ya29.token1"))
        .respond_with(GroundedRetry(calls_c))
        .mount(&good_host)
        .await;

    let tmp = TempDir::new().unwrap();
    let mut cfg = Config::defaults_without_home();
    cfg.config_dir = tmp.path().to_path_buf();
    cfg.antigravity_base_urls = vec![bad_host.uri(), good_host.uri()];
    let mgr = signed_in_ag(&cfg, &format!("{}/token", good_host.uri()));
    let eng = AntigravityEngine::new(Arc::new(cfg.clone()), Arc::clone(&mgr));

    assert_eq!(eng.decl().id, "antigravity");
    assert_eq!(eng.decl().pool_role, PoolRole::Explicit);
    assert_eq!(eng.availability(&cfg), Availability::Available);

    let out = eng
        .call("search", json!({"query": "rust"}), &SrcCx::default())
        .await
        .unwrap();
    let SrcOutput::Hits(hits) = out else { panic!() };
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].url, "https://rust-lang.org");
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    // 403 must stop failover immediately and return a lasting error.
    let forbid_host = MockServer::start().await;
    let second_calls = Arc::new(AtomicUsize::new(0));
    let second_c = Arc::clone(&second_calls);
    Mock::given(method("POST"))
        .and(path("/v1internal:generateContent"))
        .respond_with(ResponseTemplate::new(403).set_body_string("forbidden license"))
        .mount(&forbid_host)
        .await;
    struct Count(Arc<AtomicUsize>);
    impl Respond for Count {
        fn respond(&self, _: &Request) -> ResponseTemplate {
            self.0.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200)
        }
    }
    let never_hit = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1internal:generateContent"))
        .respond_with(Count(second_c))
        .mount(&never_hit)
        .await;

    let mut cfg2 = Config::defaults_without_home();
    cfg2.config_dir = tmp.path().to_path_buf();
    cfg2.antigravity_model = "gemini-pinned".into();
    cfg2.antigravity_base_urls = vec![forbid_host.uri(), never_hit.uri()];
    let eng2 = AntigravityEngine::new(Arc::new(cfg2), Arc::clone(&mgr));
    let err = eng2
        .call("search", json!({"query": "x"}), &SrcCx::default())
        .await
        .unwrap_err();
    assert!(is_lasting_agent_failure(&err), "403 must be lasting: {err}");
    assert_eq!(
        second_calls.load(Ordering::SeqCst),
        0,
        "403 must not fail over"
    );
}

#[tokio::test]
async fn test_401_refreshes_once_and_never_auto_opens_browser() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_c = Arc::clone(&calls);

    struct RefreshSeq(Arc<AtomicUsize>);
    impl Respond for RefreshSeq {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let n = self.0.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                ResponseTemplate::new(401).set_body_string("expired")
            } else {
                let auth = req.headers.get("authorization").unwrap().to_str().unwrap();
                assert_eq!(auth, "Bearer ya29.fresh");
                ResponseTemplate::new(200).set_body_json(json!({
                    "response": {
                        "candidates": [{
                            "content": {"parts": [{"text": "Refreshed hit."}]},
                            "groundingMetadata": {
                                "groundingChunks": [{"web": {"uri": "https://example.org/ref", "title": "Ref"}}],
                                "groundingSupports": [{"segment": {"partIndex": 0, "startIndex": 0, "endIndex": 14}, "groundingChunkIndices": [0]}]
                            }
                        }]
                    }
                }))
            }
        }
    }

    Mock::given(method("POST"))
        .and(path("/v1internal:generateContent"))
        .respond_with(RefreshSeq(calls_c))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "ya29.fresh",
            "expires_in": 3600
        })))
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    let mut cfg = Config::defaults_without_home();
    cfg.config_dir = tmp.path().to_path_buf();
    cfg.antigravity_model = "gemini-3.8-flash-tiered".into();
    cfg.antigravity_base_urls = vec![server.uri()];
    let mgr = signed_in_ag(&cfg, &format!("{}/token", server.uri()));
    let eng = AntigravityEngine::new(Arc::new(cfg.clone()), Arc::clone(&mgr));

    let out = eng
        .call("search", json!({"query": "test"}), &SrcCx::default())
        .await
        .unwrap();
    let SrcOutput::Hits(hits) = out else { panic!() };
    assert_eq!(hits[0].url, "https://example.org/ref");

    // Signed out: never opens browser.
    mgr.sign_out("antigravity");
    let opened = Arc::new(AtomicUsize::new(0));
    let o_c = Arc::clone(&opened);
    mgr.set_browser_opener(move |_| {
        o_c.fetch_add(1, Ordering::SeqCst);
        true
    });
    let err = eng
        .call("search", json!({"query": "test"}), &SrcCx::default())
        .await
        .unwrap_err();
    assert!(is_lasting_agent_failure(&err));
    assert_eq!(opened.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_antigravity_utf8_slice_and_multibyte_error_body_do_not_panic() {
    // Byte range strictly inside a multibyte UTF-8 character must return empty, not panic.
    assert_eq!(slice_utf8_bytes("€", 1, 2), "");
    assert_eq!(slice_utf8_bytes("€€", 1, 4), "");
    assert_eq!(slice_utf8_bytes("a€b", 1, 4), "€");

    let server = MockServer::start().await;
    let body = format!("{}€€€€€", "a".repeat(239));
    Mock::given(method("POST"))
        .and(path("/v1internal:generateContent"))
        .respond_with(ResponseTemplate::new(400).set_body_string(body))
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    let mut cfg = Config::defaults_without_home();
    cfg.config_dir = tmp.path().to_path_buf();
    cfg.antigravity_base_urls = vec![server.uri()];
    let mgr = signed_in_ag(&cfg, &format!("{}/token", server.uri()));
    let eng = AntigravityEngine::new(Arc::new(cfg), mgr);

    let err = eng
        .call("search", json!({"query": "utf8 err"}), &SrcCx::default())
        .await
        .expect_err("HTTP 400 should error cleanly without UTF-8 slice panic");
    assert!(err.to_string().contains("400"), "got {err}");
}
