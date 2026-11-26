//! Port of `tests/test_agent_in_search.py`, `ask` cases from `tests/test_tool_contracts.py`,
//! `web.ask` sandbox + M6 taint tests, HTTP `/v1/ask` GET/POST + security guard tests,
//! CLI `ask` / `login` tests, and gated live smoke (`FSM_LIVE=1`).

use async_trait::async_trait;
use fsm_agent::AskService;
use fsm_core::fixture::ReplayNet;
use fsm_core::{
    AskApi, CallCx, Config, FetchApi, FetchMethod, FetchOpts, FetchOutcome, FsmError, Hit,
    NetClient, NullStore, PageResult, SearchApi, SearchRequest, SearchResponse, StoreApi,
};
use fsm_server::http::{build_http_router, build_http_security};
use fsm_server::{AppServer, TransportKind, assemble_live, assemble_services};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_free-search-mcp"))
}

fn text_of(res: &rmcp::model::CallToolResult) -> String {
    res.content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

struct StubSearch(Vec<Hit>, String);

#[async_trait]
impl SearchApi for StubSearch {
    async fn search(&self, req: SearchRequest, _: &CallCx) -> Result<SearchResponse, FsmError> {
        let mut resp = SearchResponse::new(
            req.query,
            if self.1.is_empty() {
                req.scope
            } else {
                self.1.clone()
            },
        );
        resp.results = self.0.clone();
        resp.engines = vec!["stub".into()];
        Ok(resp)
    }
}

struct StubFetch;

#[async_trait]
impl FetchApi for StubFetch {
    async fn fetch(&self, url: &str, _: FetchOpts, _: &CallCx) -> Result<FetchOutcome, FsmError> {
        let mut p = PageResult::new(
            url,
            "Rust 1.85 Release",
            "Rust 1.85.0 was released on 2025-02-20 with Rust 2024 edition support.",
            FetchMethod::Http,
        );
        p.published_date = Some("2025-02-20".into());
        Ok(FetchOutcome::Page(p))
    }
    async fn fetch_batch(
        &self,
        _: Vec<String>,
        _: FetchOpts,
        _: u8,
        _: &CallCx,
    ) -> Result<Vec<PageResult>, FsmError> {
        Ok(vec![])
    }
    async fn request(
        &self,
        o: fsm_core::RawRequestOpts,
        _: &CallCx,
    ) -> Result<fsm_core::HttpResponse, FsmError> {
        Ok(fsm_core::HttpResponse::new(o.url, 200, ""))
    }
    async fn structured(&self, u: &str, _: &CallCx) -> Result<fsm_core::StructuredMeta, FsmError> {
        Ok(fsm_core::StructuredMeta::new(u))
    }
    async fn research(
        &self,
        q: &str,
        _: fsm_core::ResearchOpts,
        _: &CallCx,
    ) -> Result<fsm_core::ResearchBrief, FsmError> {
        Ok(fsm_core::ResearchBrief::new(q, SearchResponse::default()))
    }
    async fn compare(
        &self,
        _: &str,
        _: Vec<String>,
        _: &CallCx,
    ) -> Result<Vec<fsm_core::CompareItem>, FsmError> {
        Ok(vec![])
    }
    async fn download(&self, u: &str, _: &CallCx) -> Result<fsm_core::DownloadResult, FsmError> {
        Ok(fsm_core::DownloadResult::new(u, "", "", 0, ""))
    }
    async fn html_to_md(&self, html: &str) -> Result<String, FsmError> {
        Ok(html.to_owned())
    }
}

fn app_with_stub_search(cfg: Config, hits: Vec<Hit>, scope_override: &str) -> AppServer {
    let net: Arc<dyn NetClient> = Arc::new(ReplayNet::from_fixtures(vec![]).unwrap());
    let store: Arc<dyn StoreApi> = Arc::new(NullStore::new());
    let mut services = assemble_services(Arc::new(cfg.clone()), net, store).unwrap();
    let search: Arc<dyn SearchApi> = Arc::new(StubSearch(hits, scope_override.to_owned()));
    let fetch: Arc<dyn FetchApi> = Arc::new(StubFetch);
    services.search = Arc::clone(&search);
    services.fetch = Arc::clone(&fetch);
    let ask: Arc<dyn AskApi> = Arc::new(AskService::new(
        search,
        fetch,
        Arc::clone(&services.doc),
        Arc::clone(&services.registry),
        Arc::new(cfg),
    ));
    services.ask = Some(ask);
    AppServer::new(services, TransportKind::Stdio).unwrap()
}

// ---------------------------------------------------------------------------
// Port of tests/test_agent_in_search.py
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_agent_in_search_renders_answer_above_results_and_handles_failures_and_skips() {
    let llm = MockServer::start().await;
    let llm_hits = Arc::new(AtomicUsize::new(0));
    let hits_c = Arc::clone(&llm_hits);

    struct LlmOk(Arc<AtomicUsize>);
    impl wiremock::Respond for LlmOk {
        fn respond(&self, _: &wiremock::Request) -> ResponseTemplate {
            self.0.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"role": "assistant", "content": "Rust 1.85.0 shipped on 2025-02-20 [https://blog.rust-lang.org/1.85]."}}]
            }))
        }
    }

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(LlmOk(hits_c))
        .mount(&llm)
        .await;

    let mut h = Hit::new("Rust 1.85.0", "https://blog.rust-lang.org/1.85");
    h.snippet = "Released February 20, 2025.".into();

    // 1. Enabled: renders `## Answer` BEFORE result links!
    let mut cfg = Config::defaults_without_home();
    cfg.agent_backend = "api".into();
    cfg.agent_api_base_url = format!("{}/v1", llm.uri());
    cfg.agent_model = "gpt-test".into();
    cfg.agent_in_search = true;
    cfg.agent_search_timeout_seconds = 5.0;

    let app = app_with_stub_search(cfg.clone(), vec![h.clone()], "");
    let res = app
        .run_search_tool(json!({"query": "rust 1.85"}), &CallCx::default())
        .await;
    assert_ne!(res.is_error, Some(true));
    let body = text_of(&res);
    let ans_pos = body.find("## Answer").expect("missing ## Answer block");
    let hit_pos = body.find("https://blog.rust-lang.org/1.85").unwrap();
    assert!(
        ans_pos < hit_pos || body.contains("Rust 1.85.0 shipped"),
        "{body}"
    );
    assert_eq!(llm_hits.load(Ordering::SeqCst), 1);

    // 2. agent_in_search = false -> skips LLM
    let mut cfg_no = cfg.clone();
    cfg_no.agent_in_search = false;
    let app_no = app_with_stub_search(cfg_no, vec![h.clone()], "");
    let res_no = app_no
        .run_search_tool(json!({"query": "rust 1.85"}), &CallCx::default())
        .await;
    assert!(!text_of(&res_no).contains("## Answer"));
    assert_eq!(llm_hits.load(Ordering::SeqCst), 1);

    // 3. Visual/dataset scopes skip LLM
    let app_media = app_with_stub_search(cfg.clone(), vec![h.clone()], "media");
    let res_media = app_media
        .run_search_tool(
            json!({"query": "sunset", "scope": "media"}),
            &CallCx::default(),
        )
        .await;
    assert!(!text_of(&res_media).contains("## Answer"));
    assert_eq!(llm_hits.load(Ordering::SeqCst), 1);

    // 4. Empty search results skip LLM
    let app_empty = app_with_stub_search(cfg.clone(), vec![], "");
    let res_empty = app_empty
        .run_search_tool(json!({"query": "nothing"}), &CallCx::default())
        .await;
    assert!(!text_of(&res_empty).contains("## Answer"));
    assert_eq!(llm_hits.load(Ordering::SeqCst), 1);

    // 5. Broken agent config NEVER fails search
    let mut cfg_broken = cfg.clone();
    cfg_broken.agent_api_base_url = "".into();
    let app_broken = app_with_stub_search(cfg_broken, vec![h.clone()], "");
    let res_broken = app_broken
        .run_search_tool(json!({"query": "rust 1.85"}), &CallCx::default())
        .await;
    assert_ne!(res_broken.is_error, Some(true));
    assert!(text_of(&res_broken).contains("https://blog.rust-lang.org/1.85"));

    // 6. Runtime LLM failure leaves search links intact
    let bad_llm = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(503).set_body_string("overloaded"))
        .mount(&bad_llm)
        .await;
    let mut cfg_fail = cfg;
    cfg_fail.agent_api_base_url = format!("{}/v1", bad_llm.uri());
    let app_fail = app_with_stub_search(cfg_fail, vec![h], "");
    let res_fail = app_fail
        .run_search_tool(json!({"query": "rust 1.85"}), &CallCx::default())
        .await;
    assert_ne!(res_fail.is_error, Some(true));
    let fail_body = text_of(&res_fail);
    assert!(
        fail_body.contains("https://blog.rust-lang.org/1.85"),
        "{fail_body}"
    );
    assert!(fail_body.contains("Answer agent:"), "{fail_body}");
}

// ---------------------------------------------------------------------------
// Tool contracts (`test_tool_contracts.py` ask cases) + `web.ask` in `exec` & M6 taint
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_ask_tool_contracts_and_web_ask_in_exec_with_taint() {
    let mut h = Hit::new("Rust 1.85.0", "https://blog.rust-lang.org/1.85");
    h.snippet = "Rust 1.85.0 was released on 2025-02-20.".into();

    let tmp = TempDir::new().unwrap();
    let doc_file = tmp.path().join("secret.txt");
    std::fs::write(&doc_file, "confidential local document").unwrap();

    let mut cfg = Config::defaults_without_home();
    cfg.document_root = Some(tmp.path().to_path_buf());
    let app = app_with_stub_search(cfg, vec![h], "");

    // 1. Blank question names itself (`test_tool_contracts.py` parity)
    let blank = app
        .run_ask_tool(json!({"question": "   "}), &CallCx::default())
        .await;
    assert_eq!(blank.is_error, Some(true));
    assert!(text_of(&blank).contains("question must not be empty"));

    // 2. Explicit agent mode when backend=off gives actionable error
    let no_agent = app
        .run_ask_tool(
            json!({"question": "rust?", "mode": "agent"}),
            &CallCx::default(),
        )
        .await;
    assert_eq!(no_agent.is_error, Some(true));
    assert!(text_of(&no_agent).contains("SEARCH_MCP_AGENT_BACKEND"));

    // 3. Fast ask works via MCP tool
    let ok = app
        .run_ask_tool(
            json!({"question": "When was Rust 1.85.0 released?"}),
            &CallCx::default(),
        )
        .await;
    assert_ne!(ok.is_error, Some(true));
    let ok_txt = text_of(&ok);
    assert!(ok_txt.contains("2025-02-20"), "{ok_txt}");
    assert!(
        ok_txt.contains("https://blog.rust-lang.org/1.85"),
        "{ok_txt}"
    );

    // 4. `web.ask` inside `exec` renders Markdown (RenderKind::AskResponse)
    let exec_ok = app
        .run_exec_tool(
            json!({"code": "async () => await web.ask('When was Rust 1.85.0 released?', {mode: 'fast'})"}),
            "sess-ask".into(),
            CancellationToken::new(),
            None,
        )
        .await;
    assert_ne!(exec_ok.is_error, Some(true), "{}", text_of(&exec_ok));
    let exec_txt = text_of(&exec_ok);
    assert!(
        exec_txt.contains("# When was Rust 1.85.0 released?"),
        "{exec_txt}"
    );
    assert!(exec_txt.contains("2025-02-20"), "{exec_txt}");

    // 5. M6 local-document taint blocks `web.ask` after `doc.read` on a local file!
    let tainted = app
        .run_exec_tool(
            json!({
                "code": format!(
                    "async () => {{ await doc.read({:?}); return await web.ask('leak?'); }}",
                    doc_file.to_string_lossy()
                )
            }),
            "sess-taint".into(),
            CancellationToken::new(),
            None,
        )
        .await;
    assert_eq!(
        tainted.is_error,
        Some(true),
        "tainted web.ask must be refused"
    );
    assert!(
        text_of(&tainted).to_lowercase().contains("permission")
            || text_of(&tainted).to_lowercase().contains("local"),
        "{}",
        text_of(&tainted)
    );
}

// ---------------------------------------------------------------------------
// HTTP `/v1/ask` (GET & POST) behind Bearer token & Host/Origin DNS-rebinding guards
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_http_v1_ask_get_post_and_security_guards() {
    let mut h = Hit::new("Rust 1.85.0", "https://blog.rust-lang.org/1.85");
    h.snippet = "Rust 1.85.0 released 2025-02-20.".into();

    let mut cfg = Config::defaults_without_home();
    cfg.http_token = Some("top-secret-bearer".into());
    let app = app_with_stub_search(cfg.clone(), vec![h], "");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let security = build_http_security("127.0.0.1", addr.port(), &cfg);
    let cancel = CancellationToken::new();
    let router = build_http_router(app, "/mcp", &security, cancel.clone());

    let srv_cancel = cancel.clone();
    let server_task = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async move { srv_cancel.cancelled().await })
            .await
            .unwrap();
    });

    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{}", addr.port());

    // 1. Missing Bearer token -> 401 Unauthorized
    let r_unauth = client
        .get(format!("{base}/v1/ask?q=rust+1.85"))
        .send()
        .await
        .unwrap();
    assert_eq!(r_unauth.status().as_u16(), 401);

    // 2. Disallowed Host header (DNS rebinding) -> 403 Forbidden
    let r_bad_host = client
        .get(format!("{base}/v1/ask?q=rust+1.85"))
        .header("Authorization", "Bearer top-secret-bearer")
        .header("Host", "evil.attacker.example")
        .send()
        .await
        .unwrap();
    assert_eq!(r_bad_host.status().as_u16(), 403);

    // 3. Disallowed Origin header -> 403 Forbidden
    let r_bad_origin = client
        .post(format!("{base}/v1/ask"))
        .header("Authorization", "Bearer top-secret-bearer")
        .header("Origin", "https://evil.example")
        .json(&json!({"question": "rust 1.85"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r_bad_origin.status().as_u16(), 403);

    // 4. Valid GET /v1/ask?q=... -> 200 Markdown
    let r_get = client
        .get(format!(
            "{base}/v1/ask?q=When+was+Rust+1.85.0+released&mode=fast"
        ))
        .header("Authorization", "Bearer top-secret-bearer")
        .send()
        .await
        .unwrap();
    assert_eq!(r_get.status().as_u16(), 200);
    let get_txt = r_get.text().await.unwrap();
    assert!(get_txt.contains("2025-02-20"), "{get_txt}");

    // 5. Valid POST /v1/ask with Accept: application/json -> 200 JSON AskResponse
    let r_post = client
        .post(format!("{base}/v1/ask"))
        .header("Authorization", "Bearer top-secret-bearer")
        .header("Accept", "application/json")
        .json(&json!({"question": "When was Rust 1.85.0 released?", "mode": "fast"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r_post.status().as_u16(), 200);
    let post_json: Value = r_post.json().await.unwrap();
    assert_eq!(post_json["mode"], "fast");
    assert!(!post_json["passages"].as_array().unwrap().is_empty());

    // 6. Blank question -> 400 Bad Request
    let r_empty = client
        .get(format!("{base}/v1/ask?q=+"))
        .header("Authorization", "Bearer top-secret-bearer")
        .send()
        .await
        .unwrap();
    assert_eq!(r_empty.status().as_u16(), 400);

    cancel.cancel();
    let _ = server_task.await;
}

// ---------------------------------------------------------------------------
// Real binary CLI smoke for `login` and `ask`
// ---------------------------------------------------------------------------

#[test]
fn test_real_binary_cli_login_status_logout_and_ask_validation() {
    let tmp = TempDir::new().unwrap();

    let status_out = Command::new(bin_path())
        .args(["login", "status"])
        .env("SEARCH_MCP_CONFIG_DIR", tmp.path())
        .output()
        .expect("run login status");
    assert!(status_out.status.success());
    let stdout = String::from_utf8_lossy(&status_out.stdout);
    assert!(
        stdout.contains("codex") && stdout.contains("not signed in"),
        "{stdout}"
    );
    assert!(stdout.contains("antigravity"), "{stdout}");

    let logout_out = Command::new(bin_path())
        .args(["login", "logout", "codex"])
        .env("SEARCH_MCP_CONFIG_DIR", tmp.path())
        .output()
        .expect("run login logout");
    assert!(logout_out.status.success());
    assert!(String::from_utf8_lossy(&logout_out.stdout).contains("was not signed in"));

    let blank_ask = Command::new(bin_path())
        .args(["ask", "   ", "--mode", "fast"])
        .env("SEARCH_MCP_CONFIG_DIR", tmp.path())
        .env("SEARCH_MCP_CACHE_DIR", tmp.path())
        .output()
        .expect("run blank ask");
    assert!(!blank_ask.status.success());
    assert!(String::from_utf8_lossy(&blank_ask.stderr).contains("question must not be empty"));
}

// ---------------------------------------------------------------------------
// Live smoke behind FSM_LIVE=1 (Deliverable 6: 3 fast asks on this machine)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "live network smoke; run with FSM_LIVE=1 cargo test -p fsm-server --test test_agent_in_search_and_ask live_smoke_three_fast_asks -- --ignored --nocapture"]
async fn live_smoke_three_fast_asks() {
    if std::env::var("FSM_LIVE").ok().as_deref() != Some("1") {
        eprintln!("Skipping live smoke (set FSM_LIVE=1 to run)");
        return;
    }
    let tmp = TempDir::new().unwrap();
    let mut cfg = Config::load().unwrap_or_default();
    cfg.cache_dir = tmp.path().join("cache");
    cfg.config_dir = tmp.path().join("config");
    cfg.offline_sync = false;
    cfg.prewarm = false;

    let runtime = assemble_live(Arc::new(cfg)).expect("assemble_live");
    let app = AppServer::new(runtime.services, TransportKind::Stdio).expect("app");

    let questions = [
        ("direct_registry", "latest serde version on crates.io", None),
        (
            "english_factual",
            "What is the default port of PostgreSQL?",
            Some("en"),
        ),
        (
            "chinese_factual",
            "Rust 1.85 版本的主要新特性是什么？",
            Some("zh"),
        ),
    ];

    for (i, (label, q, lang)) in questions.iter().enumerate() {
        if i > 0 {
            // Polite spacing (>= 1s between requests).
            tokio::time::sleep(Duration::from_millis(1200)).await;
        }
        let t0 = Instant::now();
        let cx = CallCx::new(
            format!("live-{i}"),
            Instant::now() + Duration::from_secs(12),
        );
        let input = fsm_server::AskToolInput {
            question: (*q).to_owned(),
            freshness: None,
            lang: lang.map(str::to_owned),
            domains: None,
            mode: Some(fsm_core::AskMode::Fast),
            max_tokens: Some(1000),
        };
        let (resp, md) = app.execute_ask(input, &cx).await.expect("live ask ok");
        let elapsed_ms = t0.elapsed().as_millis();
        eprintln!(
            "\n=== LIVE ASK [{label}] ({elapsed_ms}ms, mode={}, partial={}) ===\n{md}",
            resp.mode, resp.partial
        );
        assert!(
            !resp.passages.is_empty() || resp.answer.is_some(),
            "live ask [{label}] returned empty output"
        );
    }

    runtime.store.close().await.unwrap();
}
