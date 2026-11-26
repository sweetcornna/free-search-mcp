//! Transport selection, HTTP DNS-rebinding & Bearer-token guards, CLI subcommands,
//! `__pdf-worker` isolation wiring, real-binary stdio E2E, and gated live smoke
//! (ports of `test_transport.py` + W6a CLI/HTTP/stdio deliverables).

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use clap::Parser;
use fsm_core::fixture::ReplayNet;
use fsm_core::{Config, NetClient, NullStore, StoreApi};
use fsm_server::cli::{Cli, CliTransport, resolve_transport};
use fsm_server::{
    AppServer, TransportKind, assemble_services, build_http_router, build_http_security,
};
use fsm_store::SqliteStore;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

fn bin_path() -> &'static str {
    env!("CARGO_BIN_EXE_free-search-mcp")
}

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
}

// ---------------------------------------------------------------------------
// test_transport.py ports: CLI/env resolution & DNS-rebinding config
// ---------------------------------------------------------------------------

#[test]
fn transport_defaults_to_stdio_and_cli_beats_env() {
    let cfg = Config::default();
    let cli = Cli::try_parse_from(["free-search-mcp"]).unwrap();
    let r = resolve_transport(&cli, &cfg).unwrap();
    assert_eq!(r.transport, CliTransport::Stdio);
    assert_eq!(r.host, "127.0.0.1");
    assert_eq!(r.port, 8000);
    assert_eq!(r.path, "/mcp");

    // Env selects streamable-http when CLI is silent.
    let mut env_cfg = Config::default();
    env_cfg.transport = "streamable-http".into();
    env_cfg.http_port = 4321;
    let r2 = resolve_transport(&cli, &env_cfg).unwrap();
    assert_eq!(r2.transport, CliTransport::StreamableHttp);
    assert_eq!(r2.port, 4321);

    // CLI beats env; --port 0 is honoured verbatim (not treated as missing).
    let cli_override = Cli::try_parse_from([
        "free-search-mcp",
        "--transport",
        "streamable-http",
        "--host",
        "127.0.0.1",
        "--port",
        "0",
        "--path",
        "/custom",
    ])
    .unwrap();
    let r3 = resolve_transport(&cli_override, &env_cfg).unwrap();
    assert_eq!(r3.transport, CliTransport::StreamableHttp);
    assert_eq!(r3.port, 0);
    assert_eq!(r3.path, "/custom");
}

#[test]
fn http_security_guard_rules_match_spec_and_python_oracle() {
    let cfg = Config::default();

    // Default loopback hosts admitted; allowed_origins strictly empty (and enforced).
    let s = build_http_security("127.0.0.1", 8000, &cfg);
    assert!(s.allowed_hosts.contains(&"127.0.0.1:8000".to_owned()));
    assert!(s.allowed_hosts.contains(&"localhost:8000".to_owned()));
    assert!(s.allowed_hosts.contains(&"[::1]:8000".to_owned()));
    assert!(s.allowed_origins.is_empty());
    assert!(s.enforce_origin_validation);

    // Explicit non-wildcard bind host admitted.
    let s2 = build_http_security("search.internal", 8080, &cfg);
    assert!(
        s2.allowed_hosts
            .contains(&"search.internal:8080".to_owned())
    );

    // Wildcard bind (0.0.0.0, ::) MUST NEVER become an allowed host.
    let s3 = build_http_security("0.0.0.0", 8000, &cfg);
    assert!(!s3.allowed_hosts.iter().any(|h| h.contains("0.0.0.0")));
    assert_eq!(
        s3.allowed_hosts,
        vec!["127.0.0.1:8000", "localhost:8000", "[::1]:8000"]
    );
    let s4 = build_http_security("::", 8000, &cfg);
    assert!(
        !s4.allowed_hosts
            .iter()
            .any(|h| h.starts_with("[::]:") || h == "::")
    );

    // SEARCH_MCP_HTTP_ALLOWED_ORIGINS forwarded verbatim.
    let mut cfg_orig = Config::default();
    cfg_orig.http_allowed_origins = vec![
        "https://a.example:443".into(),
        "https://b.example:8443".into(),
    ];
    let s5 = build_http_security("127.0.0.1", 8000, &cfg_orig);
    assert_eq!(
        s5.allowed_origins,
        vec!["https://a.example:443", "https://b.example:8443"]
    );
}

#[tokio::test]
async fn http_router_enforces_host_origin_and_bearer_token() {
    let mut cfg = Config::default();
    cfg.http_token = Some("super-secret-token".into());
    cfg.http_allowed_origins = vec!["https://trusted.example:443".into()];

    let net: Arc<dyn NetClient> = Arc::new(ReplayNet::from_fixtures(Vec::new()).unwrap());
    let store: Arc<dyn StoreApi> = Arc::new(NullStore::new());
    let services = assemble_services(Arc::new(cfg.clone()), net, store).unwrap();
    let app = AppServer::new(services, TransportKind::StreamableHttp).unwrap();
    let sec = build_http_security("127.0.0.1", 8000, &cfg);
    let router = build_http_router(app, "/mcp", &sec, CancellationToken::new());

    let discover_body = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "server/discover",
        "params": {
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {},
                "io.modelcontextprotocol/clientInfo": {"name": "http-test", "version": "1"}
            }
        }
    }))
    .unwrap();

    // 1. Missing bearer token -> 401
    let req_no_auth = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header(header::HOST, "127.0.0.1:8000")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream")
        .body(Body::from(discover_body.clone()))
        .unwrap();
    let resp = router.clone().oneshot(req_no_auth).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // 2. Wrong bearer token -> 401
    let req_bad_auth = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header(header::HOST, "127.0.0.1:8000")
        .header(header::AUTHORIZATION, "Bearer wrong-token")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream")
        .body(Body::from(discover_body.clone()))
        .unwrap();
    let resp = router.clone().oneshot(req_bad_auth).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // 3. Valid token + disallowed Host (DNS rebinding) -> 403
    let req_bad_host = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header(header::HOST, "evil.attacker.example:8000")
        .header(header::AUTHORIZATION, "Bearer super-secret-token")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream")
        .body(Body::from(discover_body.clone()))
        .unwrap();
    let resp = router.clone().oneshot(req_bad_host).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // 4. Valid token + valid Host + disallowed Origin -> 403
    let req_bad_origin = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header(header::HOST, "127.0.0.1:8000")
        .header(header::ORIGIN, "https://evil.example:443")
        .header(header::AUTHORIZATION, "Bearer super-secret-token")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream")
        .body(Body::from(discover_body.clone()))
        .unwrap();
    let resp = router.clone().oneshot(req_bad_origin).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // 5. Valid token + valid Host + allowlisted Origin -> 200 OK
    let req_ok = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header(header::HOST, "127.0.0.1:8000")
        .header(header::ORIGIN, "https://trusted.example:443")
        .header(header::AUTHORIZATION, "Bearer super-secret-token")
        .header("MCP-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", "server/discover")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream")
        .body(Body::from(discover_body))
        .unwrap();
    let resp = router.oneshot(req_ok).await.unwrap();
    let status = resp.status();
    let body_bytes = axum::body::to_bytes(resp.into_body(), 16 * 1024)
        .await
        .unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "unexpected status body={}",
        String::from_utf8_lossy(&body_bytes)
    );
}

#[tokio::test]
async fn http_empty_allowed_origins_still_rejects_browser_origin_header() {
    let cfg = Config::default(); // http_allowed_origins is empty
    let net: Arc<dyn NetClient> = Arc::new(ReplayNet::from_fixtures(Vec::new()).unwrap());
    let store: Arc<dyn StoreApi> = Arc::new(NullStore::new());
    let services = assemble_services(Arc::new(cfg.clone()), net, store).unwrap();
    let app = AppServer::new(services, TransportKind::StreamableHttp).unwrap();
    let sec = build_http_security("127.0.0.1", 8000, &cfg);
    let router = build_http_router(app, "/mcp", &sec, CancellationToken::new());

    let body = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "server/discover",
        "params": {
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {},
                "io.modelcontextprotocol/clientInfo": {"name": "t", "version": "1"}
            }
        }
    }))
    .unwrap();

    let with_origin = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header(header::HOST, "127.0.0.1:8000")
        .header(header::ORIGIN, "http://evil.example")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream")
        .body(Body::from(body.clone()))
        .unwrap();
    assert_eq!(
        router.clone().oneshot(with_origin).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );

    // Non-browser client (no Origin header) succeeds.
    let no_origin = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header(header::HOST, "127.0.0.1:8000")
        .header("MCP-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", "server/discover")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream")
        .body(Body::from(body))
        .unwrap();
    let resp = router.oneshot(no_origin).await.unwrap();
    let status = resp.status();
    let body_bytes = axum::body::to_bytes(resp.into_body(), 16 * 1024)
        .await
        .unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "unexpected status body={}",
        String::from_utf8_lossy(&body_bytes)
    );
}

#[tokio::test]
async fn http_session_id_header_isolates_exec_kv_between_sessions() {
    let tmp = TempDir::new().unwrap();
    let mut cfg = Config::default();
    cfg.cache_dir = tmp.path().to_path_buf();

    let net: Arc<dyn NetClient> = Arc::new(ReplayNet::from_fixtures(Vec::new()).unwrap());
    let store: Arc<dyn StoreApi> = Arc::new(SqliteStore::open(&cfg).unwrap());
    let services = assemble_services(Arc::new(cfg.clone()), net, store).unwrap();
    let app = AppServer::new(services, TransportKind::StreamableHttp).unwrap();
    let sec = build_http_security("127.0.0.1", 8000, &cfg);
    let router = build_http_router(app, "/mcp", &sec, CancellationToken::new());

    // Helper: initialize a legacy Streamable HTTP session and return its `Mcp-Session-Id`.
    async fn init_session(router: &axum::Router) -> String {
        let init_body = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "http-sess-test", "version": "1"}
            }
        }))
        .unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::HOST, "127.0.0.1:8000")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .body(Body::from(init_body))
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let sid = resp
            .headers()
            .get("mcp-session-id")
            .expect("Mcp-Session-Id response header")
            .to_str()
            .unwrap()
            .to_owned();

        // Send notifications/initialized for the session.
        let notif = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::HOST, "127.0.0.1:8000")
            .header("mcp-session-id", &sid)
            .header("MCP-Protocol-Version", "2025-11-25")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            ))
            .unwrap();
        let nresp = router.clone().oneshot(notif).await.unwrap();
        assert_eq!(nresp.status(), StatusCode::ACCEPTED);
        sid
    }

    async fn exec_in_session(router: &axum::Router, sid: &str, code: &str) -> String {
        let call_body = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "exec",
                "arguments": { "code": code }
            }
        }))
        .unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::HOST, "127.0.0.1:8000")
            .header("mcp-session-id", sid)
            .header("MCP-Protocol-Version", "2025-11-25")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .body(Body::from(call_body))
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let raw = String::from_utf8(
            axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        // Response is SSE (`data: {...}`) or JSON; find the JSON-RPC result payload.
        for line in raw.lines() {
            let trimmed = line
                .strip_prefix("data:")
                .map(str::trim)
                .unwrap_or(line.trim());
            if let Ok(v) = serde_json::from_str::<Value>(trimmed)
                && let Some(text) = v["result"]["content"][0]["text"].as_str()
            {
                return text.to_owned();
            }
        }
        panic!("no tool text in HTTP response: {raw}");
    }

    let s1 = init_session(&router).await;
    let s2 = init_session(&router).await;
    assert_ne!(s1, s2);

    let put_out = exec_in_session(
        &router,
        &s1,
        r#"async () => { await cache.put("http.priv", "only-s1"); await cache.put("http.pub", "both", {shared: true}); return await cache.get("http.priv"); }"#,
    )
    .await;
    assert_eq!(put_out, "only-s1");

    let s2_out = exec_in_session(
        &router,
        &s2,
        r#"async () => ({ priv: await cache.get("http.priv"), pub: await cache.get("http.pub") })"#,
    )
    .await;
    let parsed: Value = serde_json::from_str(&s2_out).expect("s2 json");
    assert!(parsed["priv"].is_null(), "session leak over HTTP: {parsed}");
    assert_eq!(parsed["pub"], "both");
}

// ---------------------------------------------------------------------------
// CLI subcommands & real binary E2E stdio tests
// ---------------------------------------------------------------------------

#[test]
fn cli_help_config_sources_and_eval_replay() {
    let tmp = TempDir::new().unwrap();

    // --help hides __pdf-worker
    let help = Command::new(bin_path()).arg("--help").output().unwrap();
    assert!(help.status.success());
    let help_txt = String::from_utf8_lossy(&help.stdout);
    assert!(
        help_txt.contains("config")
            && help_txt.contains("doctor")
            && help_txt.contains("eval-replay")
    );
    assert!(
        !help_txt.contains("__pdf-worker"),
        "__pdf-worker must be hidden"
    );

    // config --json redacts secrets
    let cfg_out = Command::new(bin_path())
        .args(["config", "--json"])
        .env("SEARCH_MCP_CACHE_DIR", tmp.path())
        .env("SEARCH_MCP_HTTP_TOKEN", "DO_NOT_LEAK_HTTP_TOK")
        .env("SEARCH_MCP_BRAVE_API_KEY", "DO_NOT_LEAK_BRAVE_KEY")
        .output()
        .unwrap();
    assert!(
        cfg_out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&cfg_out.stderr)
    );
    let cfg_txt = String::from_utf8_lossy(&cfg_out.stdout);
    assert!(!cfg_txt.contains("DO_NOT_LEAK"), "secret leaked: {cfg_txt}");
    let cfg_json: Value = serde_json::from_str(&cfg_txt).expect("valid config json");
    assert_eq!(cfg_json["http_token_set"], true);
    assert!(
        cfg_json["configured_api_keys"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "SEARCH_MCP_BRAVE_API_KEY")
    );

    // sources --json (zero network)
    let src_out = Command::new(bin_path())
        .args(["sources", "--json", "--limit", "5", "clinical"])
        .env("SEARCH_MCP_CACHE_DIR", tmp.path())
        .output()
        .unwrap();
    assert!(
        src_out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&src_out.stderr)
    );
    let src_json: Value = serde_json::from_str(&String::from_utf8_lossy(&src_out.stdout)).unwrap();
    assert!(src_json["total_sources"].as_u64().unwrap() >= 240);
    assert_eq!(src_json["total_protocols"], 18);
    assert_eq!(
        src_json["total_host_apis"],
        fsm_surface::HOST_API.len() as u64
    );

    // sources --describe clinicaltrials
    let desc_out = Command::new(bin_path())
        .args(["sources", "--describe", "clinicaltrials", "--json"])
        .env("SEARCH_MCP_CACHE_DIR", tmp.path())
        .output()
        .unwrap();
    assert!(desc_out.status.success());
    let desc_json: Value =
        serde_json::from_str(&String::from_utf8_lossy(&desc_out.stdout)).unwrap();
    assert_eq!(desc_json["id"], "clinicaltrials");

    // eval-replay against shipped ranking fixture
    let fixture = workspace_root().join("evals/ranking/fixtures/decoy_and_dupes.json");
    let eval_out = Command::new(bin_path())
        .args(["eval-replay", "--data", fixture.to_str().unwrap()])
        .env("SEARCH_MCP_CACHE_DIR", tmp.path())
        .output()
        .unwrap();
    assert!(
        eval_out.status.success(),
        "eval-replay failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&eval_out.stdout),
        String::from_utf8_lossy(&eval_out.stderr)
    );
    let eval_txt = String::from_utf8_lossy(&eval_out.stdout);
    assert!(
        eval_txt.contains("with guard:") && eval_txt.contains("MRR="),
        "{eval_txt}"
    );
}

#[test]
fn hidden_pdf_worker_subcommand_runs_isolated_worker() {
    // Passing empty stdin causes the PDF worker to exit cleanly with an error line
    // on its protocol channel without starting Tokio or emitting non-protocol banners.
    let mut child = Command::new(bin_path())
        .arg("__pdf-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn __pdf-worker");
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait __pdf-worker");
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Worker emits either protocol lines starting with LINE_PREFIX or nothing on empty stdin;
    // it must never print clap usage or tracing logs to stdout.
    assert!(
        stdout.is_empty()
            || stdout
                .lines()
                .all(|l| l.starts_with(fsm_docs::testing::LINE_PREFIX)),
        "unexpected non-protocol stdout from __pdf-worker: {stdout:?}"
    );
}

#[test]
fn real_binary_stdio_e2e_offline_tools_list_search_and_exec() {
    let tmp = TempDir::new().unwrap();
    let mut child = Command::new(bin_path())
        .env("SEARCH_MCP_CACHE_DIR", tmp.path())
        .env("SEARCH_MCP_PREWARM", "false")
        .env("SEARCH_MCP_OFFLINE_SYNC", "false")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn free-search-mcp");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    let send = |stdin: &mut std::process::ChildStdin,
                reader: &mut BufReader<std::process::ChildStdout>,
                msg: &Value|
     -> Value {
        let mut s = serde_json::to_string(msg).unwrap();
        s.push('\n');
        stdin.write_all(s.as_bytes()).unwrap();
        stdin.flush().unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("non-JSON stdout line ({e}): {line:?}"))
    };

    // 1. Initialize (2024-11-05 to verify wire byte budget on real binary!)
    let init = send(
        &mut stdin,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "stdio-e2e", "version": "1.0"}
            }
        }),
    );
    assert_eq!(init["result"]["protocolVersion"], "2024-11-05");
    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .unwrap();
    stdin.flush().unwrap();

    // 2. tools/list byte budget on real binary
    let tl = send(
        &mut stdin,
        &mut reader,
        &json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    );
    let tools_compact = serde_json::to_string(&tl["result"]["tools"]).unwrap();
    assert!(
        tools_compact.len() <= 6_800,
        "real binary 2024-11-05 result.tools = {} B > 6800 B",
        tools_compact.len()
    );
    let names: Vec<&str> = tl["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["search", "exec", "ask"]);

    // 3. Offline `search` (scope = catalog)
    let search = send(
        &mut stdin,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "search", "arguments": {"query": "arxiv", "scope": "catalog", "limit": 3}}
        }),
    );
    assert_ne!(search["result"]["isError"], json!(true));
    assert!(
        search["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("arxiv")
    );

    // 4. Offline `exec` exercising catalog + cache.put/get + prelude util
    let exec = send(
        &mut stdin,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "exec",
                "arguments": {
                    "code": "async () => { const hits = await catalog.search('clinical trials', {limit: 2}); await cache.put('smoke.k', {n: hits.length}); const back = await cache.get('smoke.k'); return { n: back.n, top: hits[0].id, tok: util.estimateTokens('hello world') }; }"
                }
            }
        }),
    );
    assert_ne!(exec["result"]["isError"], json!(true), "{exec}");
    let payload: Value =
        serde_json::from_str(exec["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert!(payload["n"].as_u64().unwrap() >= 1);
    assert!(payload["tok"].as_u64().unwrap() >= 2);

    drop(stdin);
    let status = child.wait().unwrap();
    assert!(status.success(), "server exited with {status}");
}

// ---------------------------------------------------------------------------
// Live smoke test (gated behind #[ignore] + FSM_LIVE=1)
// ---------------------------------------------------------------------------

#[test]
#[ignore = "live network smoke; run with FSM_LIVE=1 cargo test -p fsm-server --test http_and_cli live_stdio_smoke -- --ignored --nocapture"]
fn live_stdio_smoke() {
    if std::env::var("FSM_LIVE").ok().as_deref() != Some("1") {
        eprintln!("skipping live_stdio_smoke (set FSM_LIVE=1)");
        return;
    }
    let tmp = TempDir::new().unwrap();
    let mut child = Command::new(bin_path())
        .env("SEARCH_MCP_CACHE_DIR", tmp.path())
        .env("SEARCH_MCP_PREWARM", "false")
        .env("SEARCH_MCP_OFFLINE_SYNC", "false")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn free-search-mcp");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    let send = |stdin: &mut std::process::ChildStdin,
                reader: &mut BufReader<std::process::ChildStdout>,
                msg: &Value|
     -> Value {
        let mut s = serde_json::to_string(msg).unwrap();
        s.push('\n');
        stdin.write_all(s.as_bytes()).unwrap();
        stdin.flush().unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        serde_json::from_str(&line).expect("json response")
    };

    let init = send(
        &mut stdin,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "live-smoke", "version": "1.0"}
            }
        }),
    );
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .unwrap();
    stdin.flush().unwrap();

    // Real live `search`
    let search = send(
        &mut stdin,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": "search", "arguments": {"query": "rust programming language", "limit": 3}}
        }),
    );
    let search_txt = search["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    eprintln!("=== LIVE SEARCH ===\n{search_txt}");
    assert_ne!(search["result"]["isError"], json!(true), "{search}");
    assert!(
        search_txt.contains("http"),
        "expected URLs in live search output"
    );

    // Real live `exec`
    let exec = send(
        &mut stdin,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "exec",
                "arguments": {
                    "code": "async () => { const p = await web.fetch('https://example.com'); return { title: p.title, url: p.url, chars: p.content.length }; }"
                }
            }
        }),
    );
    let exec_txt = exec["result"]["content"][0]["text"].as_str().unwrap_or("");
    eprintln!("=== LIVE EXEC ===\n{exec_txt}");
    assert_ne!(exec["result"]["isError"], json!(true), "{exec}");
    let out: Value = serde_json::from_str(exec_txt).expect("exec json");
    assert!(out["chars"].as_u64().unwrap() > 20, "{out}");

    drop(stdin);
    let _ = child.wait();
}
