//! Five-era MCP protocol compliance, wire byte-budget gates, cancellation, and
//! `notifications/progress` keepalive tests (ports of `test_protocol_eras.py`,
//! `test_tool_schemas.py`, and W0e/W6a byte-budget & I4 shim assertions).

use fsm_core::fixture::ReplayNet;
use fsm_core::{Config, NetClient, NullStore, StoreApi};
use fsm_server::{AppServer, TransportKind, assemble_services};
use rmcp::ServiceExt;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

struct DuplexClient {
    writer: tokio::io::WriteHalf<DuplexStream>,
    reader: BufReader<tokio::io::ReadHalf<DuplexStream>>,
    next_id: i64,
    notes: Vec<Value>,
    _server: tokio::task::JoinHandle<()>,
}

impl DuplexClient {
    async fn spawn(app: AppServer) -> Self {
        let (client_io, server_io) = tokio::io::duplex(256 * 1024);
        let server_task = tokio::spawn(async move {
            let (sr, sw) = tokio::io::split(server_io);
            if let Ok(svc) = app.serve((sr, sw)).await {
                let _ = svc.waiting().await;
            }
        });
        let (cr, cw) = tokio::io::split(client_io);
        Self {
            writer: cw,
            reader: BufReader::new(cr),
            next_id: 1,
            notes: Vec::new(),
            _server: server_task,
        }
    }

    async fn send(&mut self, msg: &Value) {
        let mut line = serde_json::to_string(msg).expect("serialize json-rpc");
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .await
            .expect("write json-rpc");
        self.writer.flush().await.expect("flush json-rpc");
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await;
        self.wait_for(id, Duration::from_secs(10))
            .await
            .expect("response within timeout")
    }

    async fn wait_for(&mut self, id: i64, timeout: Duration) -> Option<Value> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let mut line = String::new();
            let read = tokio::time::timeout(remaining, self.reader.read_line(&mut line)).await;
            match read {
                Ok(Ok(0)) | Err(_) => return None,
                Ok(Ok(_)) => {
                    let msg: Value = serde_json::from_str(&line).expect("valid server json line");
                    if msg.get("id") == Some(&json!(id))
                        && (msg.get("result").is_some() || msg.get("error").is_some())
                    {
                        return Some(msg);
                    }
                    self.notes.push(msg);
                }
                Ok(Err(_)) => return None,
            }
        }
    }

    async fn initialize(&mut self, version: &str) -> Value {
        let init = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": version,
                    "capabilities": {},
                    "clientInfo": {"name": "fsm-test-client", "version": "1.0"}
                }),
            )
            .await;
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))
        .await;
        init
    }
}

fn modern_envelope(version: &str) -> Value {
    json!({
        "_meta": {
            "io.modelcontextprotocol/protocolVersion": version,
            "io.modelcontextprotocol/clientCapabilities": {},
            "io.modelcontextprotocol/clientInfo": {"name": "fsm-modern-test", "version": "1.0"}
        }
    })
}

fn offline_app(cfg: Config) -> AppServer {
    let net: Arc<dyn NetClient> = Arc::new(ReplayNet::from_fixtures(Vec::new()).unwrap());
    let store: Arc<dyn StoreApi> = Arc::new(NullStore::new());
    let services = assemble_services(Arc::new(cfg), net, store).expect("assemble services");
    AppServer::new(services, TransportKind::Stdio).expect("app server")
}

#[tokio::test]
async fn five_era_handshake_and_byte_budget_gates() {
    // Legacy handshake eras (2024-11-05 .. 2025-11-25) + 2026-07-28 initialize fallback + unknown fallback.
    let cases = [
        ("2024-11-05", "2024-11-05", true, true),
        ("2025-03-26", "2025-03-26", true, true),
        ("2025-06-18", "2025-06-18", true, true),
        ("2025-11-25", "2025-11-25", true, true),
        ("2026-07-28", "2025-11-25", true, true),
        ("1999-01-01", "2025-11-25", true, true),
    ];

    let mut measured_2024_bytes: usize = 0;

    for (requested, expected_ver, expect_title, expect_annotations) in cases {
        let mut client = DuplexClient::spawn(offline_app(Config::default())).await;
        let init = client.initialize(requested).await;
        let result = &init["result"];
        assert_eq!(
            result["protocolVersion"], expected_ver,
            "requested={requested}"
        );

        // Server identity and instructions <= 450 B (SPEC §2.1–§2.2).
        let info = &result["serverInfo"];
        assert_eq!(info["name"], "search-mcp");
        assert_eq!(info["title"], "Free Search");
        assert_eq!(info["version"], env!("CARGO_PKG_VERSION"));
        assert!(
            info["websiteUrl"]
                .as_str()
                .is_some_and(|s| s.starts_with("https://"))
        );
        assert!(
            info["icons"][0]["src"]
                .as_str()
                .is_some_and(|s| s.starts_with("https://"))
        );
        let instr = result["instructions"]
            .as_str()
            .expect("instructions present");
        assert!(
            instr.len() <= 450,
            "instructions = {} B exceeds 450 B budget",
            instr.len()
        );

        // Zero prompts, zero resources, zero completions advertised (SPEC §2.1).
        let caps = &result["capabilities"];
        assert!(caps.get("tools").is_some());
        assert!(caps.get("prompts").is_none());
        assert!(caps.get("resources").is_none());
        assert!(caps.get("completions").is_none());

        // tools/list: deterministic [search, exec, ask], no outputSchema, no $schema (ruling R5).
        let tl = client.request("tools/list", json!({})).await;
        let tl_res = &tl["result"];
        assert!(
            tl_res.get("resultType").is_none(),
            "legacy era omits resultType"
        );
        let tools = tl_res["tools"].as_array().expect("tools array");
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["search", "exec", "ask"]);

        for tool in tools {
            assert!(tool.get("outputSchema").is_none(), "must omit outputSchema");
            assert!(
                tool["inputSchema"].get("$schema").is_none(),
                "must strip top-level $schema"
            );
            assert_eq!(tool["inputSchema"]["type"], "object");
            assert_eq!(tool["inputSchema"]["additionalProperties"], false);
            assert_eq!(tool.get("title").is_some(), expect_title, "era={requested}");
            assert_eq!(
                tool.get("annotations").is_some(),
                expect_annotations,
                "era={requested}"
            );
            if expect_annotations {
                let expected_ro = tool["name"] == "search" || tool["name"] == "ask";
                assert_eq!(tool["annotations"]["readOnlyHint"], expected_ro);
                assert_eq!(tool["annotations"]["destructiveHint"], false);
                assert_eq!(tool["annotations"]["openWorldHint"], true);
            }
        }

        if requested == "2024-11-05" {
            let compact = serde_json::to_string(&tl_res["tools"]).expect("compact tools");
            measured_2024_bytes = compact.len();
            assert!(
                measured_2024_bytes <= 6_800,
                "2024-11-05 result.tools = {measured_2024_bytes} B exceeds 6,800 B hard gate"
            );
        }

        // Legacy ping succeeds; unknown tool is -32602.
        let ping = client.request("ping", json!({})).await;
        assert!(ping.get("result").is_some(), "legacy ping ok: {ping}");

        let unknown = client
            .request(
                "tools/call",
                json!({"name": "no_such_tool", "arguments": {}}),
            )
            .await;
        assert_eq!(unknown["error"]["code"], -32602);

        // Exec callable on every era; no structuredContent emitted.
        let exec = client
            .request(
                "tools/call",
                json!({"name": "exec", "arguments": {"code": "async () => ({ ok: true, era: 1 })"}}),
            )
            .await;
        assert!(exec["result"].get("isError") != Some(&json!(true)));
        assert!(exec["result"].get("structuredContent").is_none());
        let text = exec["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("\"ok\":true"), "{text}");
    }

    // Modern 2026-07-28 stateless era (server/discover + per-request envelope).
    let mut modern = DuplexClient::spawn(offline_app(Config::default())).await;
    let discover = modern
        .request("server/discover", modern_envelope("2026-07-28"))
        .await;
    let d_res = &discover["result"];
    assert_eq!(d_res["resultType"], "complete");
    assert_eq!(d_res["ttlMs"], 3_600_000);
    assert_eq!(d_res["cacheScope"], "public");
    assert_eq!(
        d_res["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "search-mcp"
    );
    let supported: Vec<&str> = d_res["supportedVersions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    for v in [
        "2024-11-05",
        "2025-03-26",
        "2025-06-18",
        "2025-11-25",
        "2026-07-28",
    ] {
        assert!(supported.contains(&v), "missing supported version {v}");
    }

    let tl = modern
        .request("tools/list", modern_envelope("2026-07-28"))
        .await;
    let tl_wire_bytes = serde_json::to_string(&tl).unwrap().len();
    let tl_res = &tl["result"];
    assert_eq!(tl_res["resultType"], "complete");
    assert_eq!(tl_res["ttlMs"], 3_600_000);
    assert_eq!(tl_res["cacheScope"], "public");
    // Mandatory shim #3: serverInfo stamped on EVERY 2026 result, not only discover.
    assert_eq!(
        tl_res["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "search-mcp"
    );
    let tools = tl_res["tools"].as_array().unwrap();
    assert_eq!(
        tools
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["search", "exec", "ask"]
    );
    assert!(
        tools
            .iter()
            .all(|t| t["title"].as_str().is_some_and(|s| !s.is_empty()))
    );

    // Modern tools/call carries resultType:"complete", serverInfo stamp, no structuredContent.
    let mut call_params = modern_envelope("2026-07-28");
    call_params["name"] = json!("exec");
    call_params["arguments"] = json!({"code": "1 + 2"});
    let call = modern.request("tools/call", call_params).await;
    assert_eq!(call["result"]["resultType"], "complete");
    assert!(call["result"].get("structuredContent").is_none());
    assert_eq!(
        call["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["title"],
        "Free Search"
    );
    assert_eq!(call["result"]["content"][0]["text"], "3");

    // 2026-07-28 removes ping (-32601).
    let ping = modern.request("ping", modern_envelope("2026-07-28")).await;
    assert_eq!(ping["error"]["code"], -32601);

    // Unsupported version in envelope -> -32022.
    let bad_ver = modern
        .request("tools/list", modern_envelope("2099-01-01"))
        .await;
    assert_eq!(bad_ver["error"]["code"], -32022);

    eprintln!(
        "WIRE_METRICS tools_2024_11_05_bytes={measured_2024_bytes} tools_2026_07_28_wire_bytes={tl_wire_bytes}"
    );
}

#[tokio::test]
async fn cancellation_stops_running_exec_and_keeps_server_alive() {
    let mut client = DuplexClient::spawn(offline_app(Config::default())).await;
    client.initialize("2025-11-25").await;

    let cancel_id = 777;
    client
        .send(&json!({
            "jsonrpc": "2.0",
            "id": cancel_id,
            "method": "tools/call",
            "params": {
                "name": "exec",
                "arguments": {"code": "async () => { while (true) {} }"}
            }
        }))
        .await;

    tokio::time::sleep(Duration::from_millis(120)).await;
    client
        .send(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {"requestId": cancel_id, "reason": "test cancel"}
        }))
        .await;

    let cancelled_resp = client.wait_for(cancel_id, Duration::from_millis(400)).await;
    assert!(
        cancelled_resp.is_none(),
        "cancelled request must not emit a response: {cancelled_resp:?}"
    );

    let followup = client
        .request(
            "tools/call",
            json!({"name": "exec", "arguments": {"code": "async () => 'still alive'"}}),
        )
        .await;
    assert_eq!(followup["result"]["content"][0]["text"], "still alive");
}

#[tokio::test]
async fn exec_emits_progress_keepalive_notifications() {
    let app = offline_app(Config::default()).with_keepalive_interval(Duration::from_millis(60));
    let mut client = DuplexClient::spawn(app).await;
    client.initialize("2025-11-25").await;

    let resp = client
        .request(
            "tools/call",
            json!({
                "name": "exec",
                "arguments": {"code": "async () => { await util.sleep(200); return 'done'; }"},
                "_meta": {"progressToken": "keepalive-tok-1"}
            }),
        )
        .await;
    assert_eq!(resp["result"]["content"][0]["text"], "done");

    let progress_notes: Vec<&Value> = client
        .notes
        .iter()
        .filter(|n| n.get("method") == Some(&json!("notifications/progress")))
        .collect();
    assert!(
        !progress_notes.is_empty(),
        "expected >=1 notifications/progress keepalive, got notes={:?}",
        client.notes
    );
    assert!(
        progress_notes
            .iter()
            .all(|n| n["params"]["progressToken"] == "keepalive-tok-1"),
        "wrong progressToken: {progress_notes:?}"
    );
}

#[tokio::test]
async fn lazy_stdio_server_answers_initialize_and_tools_list_without_blocking_on_assembly() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut cfg = Config::default();
    cfg.cache_dir = tmp.path().to_path_buf();
    let app = AppServer::new_lazy(Arc::new(cfg), TransportKind::Stdio);
    let shutdown_app = app.clone();

    let start = std::time::Instant::now();
    let mut client = DuplexClient::spawn(app).await;
    let init = client.initialize("2025-11-25").await;
    let init_ms = start.elapsed().as_millis();
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(init["result"]["serverInfo"]["name"], "search-mcp");

    let tl = client.request("tools/list", json!({})).await;
    let list_ms = start.elapsed().as_millis();
    let names: Vec<&str> = tl["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["search", "exec", "ask"]);
    assert!(
        list_ms < 500,
        "lazy initialize ({init_ms} ms) + tools/list ({list_ms} ms) must not block on heavy assembly"
    );

    // First tool call transparently awaits background assembly and succeeds.
    let call = client
        .request(
            "tools/call",
            json!({"name": "exec", "arguments": {"code": "async () => 40 + 2"}}),
        )
        .await;
    assert_ne!(call["result"]["isError"], json!(true), "{call}");
    assert_eq!(call["result"]["content"][0]["text"], "42");

    shutdown_app.shutdown().await;
}
