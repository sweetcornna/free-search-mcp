//! Contract, boundary, security, replay, and no-key positioning integration tests:
//! - M2 `HOST_API` bijection + TS↔Rust camelCase key-set verification
//! - M6 centralized `egress` taint rule (incl. refusing `web.search` & `source.protocol` after local `doc.read`)
//! - M7 `exec_kv` session isolation & cross-session `exec:<id>` overflow handles
//! - M16 `search({query:"tokio crate latest stable version", scope:"code"})` #1 ranking replay
//! - N1 NVD `HttpResponse.body`/`.json` equality replay through `exec`
//! - `SEARCH_MCP_TOOL_CALL_BUDGET`, `catch_unwind` panic isolation, scope routing, inline image,
//!   and `test_no_key_positioning.py` / `test_mcp_boundary.py` / `test_tool_contracts.py` ports.

use async_trait::async_trait;
use fsm_core::fixture::ReplayNet;
use fsm_core::{
    AuthMode, AuthorProfile, AuthorRef, Availability, BinaryHandle, CacheHit, CallCx,
    CatalogDetail, CitationGraph, CompareItem, Config, DocOutline, DocSlice, DownloadResult,
    EntityKind, FetchApi, FetchMethod, FetchOpts, FetchOutcome, FsmError, FulltextResult,
    GrantRecord, GraphOpts, Hit, HostServices, HostValue, HttpResponse, InlineImageMeta,
    IntegrityReport, NetClient, NullStore, PageResult, RawRequestOpts, ResearchBrief, ResearchOpts,
    ScholarApi, ScholarQuery, ScholarlyWork, SearchApi, SearchRequest, SearchResponse, SourceDeps,
    StoreApi, StructuredMeta, WorkResult,
};
use fsm_server::{
    AppServer, DISPATCHED_HOST_FNS, DefaultHostServices, TransportKind, assemble_services,
    build_registry,
};
use fsm_store::SqliteStore;
use fsm_surface::{HOST_API, dts, render_error};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

fn text_of(res: &rmcp::model::CallToolResult) -> String {
    res.content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
}

// ---------------------------------------------------------------------------
// M2: HOST_API bijection + TS <-> Rust camelCase key sets
// ---------------------------------------------------------------------------

#[test]
fn host_api_bijection_with_dispatcher_and_sandbox_prelude() {
    let api_async: BTreeSet<&str> = HOST_API
        .iter()
        .filter(|f| !f.is_prelude_sync)
        .map(|f| f.name)
        .collect();
    let dispatched: BTreeSet<&str> = DISPATCHED_HOST_FNS.iter().copied().collect();
    assert_eq!(
        api_async, dispatched,
        "non-prelude HOST_API entries must match DISPATCHED_HOST_FNS 1:1"
    );

    let api_sync: BTreeSet<&str> = HOST_API
        .iter()
        .filter(|f| f.is_prelude_sync)
        .map(|f| f.name)
        .collect();
    let prelude_sync: BTreeSet<&str> = fsm_script::PRELUDE_SYNC_FNS.iter().copied().collect();
    assert_eq!(
        api_sync, prelude_sync,
        "prelude-sync HOST_API entries must match fsm_script::PRELUDE_SYNC_FNS 1:1"
    );

    // Every sandbox-native function is also declared in HOST_API.
    for name in fsm_script::SANDBOX_NATIVE_FNS {
        assert!(
            api_async.contains(name),
            "sandbox-native {name} missing from HOST_API"
        );
    }

    // Building the real QuickJS sandbox prelude from HOST_API must succeed.
    assert!(fsm_script::Sandbox::new(HOST_API).is_ok());
}

fn extract_ts_interface_keys(dts_src: &str, name: &str) -> BTreeSet<String> {
    let prefix = format!("interface {name} ");
    let line = dts_src
        .lines()
        .find(|l| l.trim_start().starts_with(&prefix))
        .unwrap_or_else(|| panic!("interface {name} not found in dts()"));
    let open = line.find('{').expect("opening brace");
    let close = line.rfind('}').expect("closing brace");
    let body = &line[open + 1..close];
    let mut keys = BTreeSet::new();
    let mut depth = 0usize;
    let mut current = String::new();
    for ch in body.chars() {
        match ch {
            '{' | '[' | '(' => depth += 1,
            '}' | ']' | ')' => depth = depth.saturating_sub(1),
            ';' if depth == 0 => {
                if let Some((lhs, _)) = current.split_once(':') {
                    let k = lhs.trim().trim_end_matches('?').trim();
                    if !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                        keys.insert(k.to_owned());
                    }
                }
                current.clear();
            }
            _ if depth == 0 => current.push(ch),
            _ => {}
        }
    }
    if let Some((lhs, _)) = current.split_once(':') {
        let k = lhs.trim().trim_end_matches('?').trim();
        if !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            keys.insert(k.to_owned());
        }
    }
    keys
}

fn rust_keys<T: serde::Serialize>(val: &T) -> BTreeSet<String> {
    serde_json::to_value(val)
        .unwrap()
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect()
}

#[test]
fn ts_interfaces_match_rust_camel_case_keys_m2() {
    let src = dts();

    let mut hit = Hit::default();
    hit.date = Some("2026-01-01".into());
    hit.date_source = Some("api".into());
    hit.extra = Some(json!({}));
    assert_eq!(extract_ts_interface_keys(&src, "Hit"), rust_keys(&hit));

    let mut sr = SearchResponse::default();
    sr.lead = Some("l".into());
    sr.cache_age_s = Some(1);
    sr.cache_key = Some("k".into());
    sr.agent = Some(fsm_core::SearchAgentSummary::default());
    sr.agent_lane = Some(fsm_core::AgentLaneReport::default());
    assert_eq!(
        extract_ts_interface_keys(&src, "SearchResponse"),
        rust_keys(&sr)
    );

    let mut ar = fsm_core::AskResponse::new("q", "fast");
    ar.answer = Some("a".into());
    ar.cache_age_s = Some(1);
    ar.date_note = Some("d".into());
    ar.backend = Some("api".into());
    ar.model = Some("m".into());
    ar.steps = Some(1);
    ar.agent_error = Some("e".into());
    assert_eq!(
        extract_ts_interface_keys(&src, "AskResponse"),
        rust_keys(&ar)
    );

    let mut page = PageResult::default();
    page.published_date = Some("2026".into());
    page.author = Some("a".into());
    page.sitename = Some("s".into());
    page.media_type = Some("text/html".into());
    page.bytes = Some(1);
    page.width = Some(1);
    page.height = Some(1);
    page.sha256 = Some("h".into());
    page.cache_age_s = Some(1);
    assert_eq!(
        extract_ts_interface_keys(&src, "PageResult"),
        rust_keys(&page)
    );

    let mut http = HttpResponse::default();
    http.json = Some(json!({}));
    let http_keys = rust_keys(&http);
    assert!(http_keys.contains("body") && !http_keys.contains("text"));
    assert_eq!(extract_ts_interface_keys(&src, "HttpResponse"), http_keys);

    let mut meta = StructuredMeta::default();
    meta.hint = Some("h".into());
    assert_eq!(
        extract_ts_interface_keys(&src, "StructuredMeta"),
        rust_keys(&meta)
    );

    let mut cmp = CompareItem::default();
    cmp.title = Some("t".into());
    cmp.published_date = Some("d".into());
    cmp.excerpt = Some("e".into());
    cmp.truncated = Some(false);
    cmp.error = Some("err".into());
    assert_eq!(
        extract_ts_interface_keys(&src, "CompareItem"),
        rust_keys(&cmp)
    );

    let mut dl = DownloadResult::default();
    dl.width = Some(1);
    dl.height = Some(1);
    assert_eq!(
        extract_ts_interface_keys(&src, "DownloadResult"),
        rust_keys(&dl)
    );

    let mut ds = DocSlice::default();
    ds.title = Some("t".into());
    ds.pages = Some(1);
    assert_eq!(extract_ts_interface_keys(&src, "DocSlice"), rust_keys(&ds));

    let mut doutline = DocOutline::default();
    doutline.title = Some("t".into());
    doutline.pages = Some(1);
    doutline.sheets = Some(vec![]);
    assert_eq!(
        extract_ts_interface_keys(&src, "DocOutline"),
        rust_keys(&doutline)
    );

    let mut ft = FulltextResult::default();
    ft.format = Some("jats".into());
    ft.source = Some("pmc".into());
    ft.license = Some("cc-by".into());
    ft.url = Some("u".into());
    ft.sections = Some(vec![]);
    ft.content = Some("c".into());
    ft.why = Some("w".into());
    ft.oa_status = Some("gold".into());
    ft.landing_url = Some("l".into());
    ft.notes.push("n".into());
    assert_eq!(
        extract_ts_interface_keys(&src, "FulltextResult"),
        rust_keys(&ft)
    );

    let mut ch = CacheHit::default();
    ch.author = Some("a".into());
    ch.date = Some("d".into());
    ch.sitename = Some("s".into());
    assert_eq!(extract_ts_interface_keys(&src, "CacheHit"), rust_keys(&ch));

    // CatalogDetail flattens CatalogHit (M2).
    let mut cd = CatalogDetail::default();
    cd.home_url = Some("https://example.org".into());
    let mut expected_cd = extract_ts_interface_keys(&src, "CatalogHit");
    expected_cd.extend(extract_ts_interface_keys(&src, "CatalogDetail"));
    assert_eq!(expected_cd, rust_keys(&cd));

    // Check remaining entity structs.
    let mut ap = AuthorProfile::default();
    ap.orcid = Some("0".into());
    ap.openalex_id = Some("A".into());
    ap.works_count = Some(1);
    ap.cited_by_count = Some(1);
    ap.h_index = Some(1);
    assert_eq!(
        extract_ts_interface_keys(&src, "AuthorProfile"),
        rust_keys(&ap)
    );

    let mut gr = GrantRecord::default();
    gr.pi = Some("p".into());
    gr.org = Some("o".into());
    gr.amount = Some(1.0);
    gr.currency = Some("USD".into());
    gr.start_date = Some("2025".into());
    gr.end_date = Some("2026".into());
    gr.url = Some("u".into());
    assert_eq!(
        extract_ts_interface_keys(&src, "GrantRecord"),
        rust_keys(&gr)
    );

    let mut img = InlineImageMeta::default();
    img.width = Some(1);
    img.height = Some(1);
    assert_eq!(
        extract_ts_interface_keys(&src, "InlineImage"),
        rust_keys(&img)
    );
}

// ---------------------------------------------------------------------------
// M6: Centralized egress taint rule
// ---------------------------------------------------------------------------

#[tokio::test]
async fn m6_taint_blocks_web_search_source_protocol_and_new_hosts_after_local_doc_read() {
    let tmp = TempDir::new().unwrap();
    let secret_file = tmp.path().join("secret.txt");
    std::fs::write(&secret_file, "TOP SECRET PAYLOAD 42\n").unwrap();

    let mut cfg = Config::default();
    cfg.document_root = Some(tmp.path().to_path_buf());
    cfg.offline_sync = false;

    let fixture: fsm_core::fixture::Fixture = serde_json::from_value(json!({
        "version": 1,
        "source": "taint",
        "op": "fetch",
        "recordedAt": "2026-11-25T00:00:00Z",
        "exchanges": [
            {
                "request": {
                    "method": "GET",
                    "url": "https://allowed.example/page1",
                    "headersSubset": {},
                    "bodySha256": null
                },
                "response": {
                    "status": 200,
                    "headers": [["content-type", "text/plain"]],
                    "bodyText": "Allowed page 1 body",
                    "bodyB64": null
                }
            },
            {
                "request": {
                    "method": "GET",
                    "url": "https://allowed.example/page2",
                    "headersSubset": {},
                    "bodySha256": null
                },
                "response": {
                    "status": 200,
                    "headers": [["content-type", "text/plain"]],
                    "bodyText": "Allowed page 2 body",
                    "bodyB64": null
                }
            }
        ]
    }))
    .unwrap();

    let net: Arc<dyn NetClient> = Arc::new(ReplayNet::from_fixtures(vec![fixture]).unwrap());
    let store: Arc<dyn StoreApi> = Arc::new(NullStore::new());
    let services = assemble_services(Arc::new(cfg), net, store).unwrap();
    let app = AppServer::new(services, TransportKind::Stdio).unwrap();

    let code = r#"async () => {
        // 1. Pre-taint fetch records allowed.example in cx.fetched_hosts
        const p1 = await web.fetch("https://allowed.example/page1");
        // 2. Local doc read taints this exec
        const local = await doc.read("secret.txt");
        const tryCall = async (fn) => {
            try { await fn(); return "ok"; }
            catch (e) { return e.kind + ":" + e.message; }
        };
        return {
            p1Ok: p1.content.includes("Allowed page 1"),
            localOk: local.content.includes("TOP SECRET"),
            webSearch: await tryCall(() => web.search("TOP SECRET")),
            sourceProtocol: await tryCall(() => source.protocol("mediawiki", "https://en.wikipedia.org", "search", {query: "rust"})),
            sourceCall: await tryCall(() => source.call("arxiv.search", {query: "rust"})),
            scholarSearch: await tryCall(() => scholar.search("rust")),
            webRequestSameHost: await tryCall(() => web.request({url: "https://allowed.example/page2"})),
            webFetchNewHost: await tryCall(() => web.fetch("https://evil.example/leak")),
            remoteDocRead: await tryCall(() => doc.read("https://evil.example/paper.pdf")),
            // Narrow exception: GET web.fetch to already-fetched host succeeds
            webFetchAllowedHost: await tryCall(() => web.fetch("https://allowed.example/page2")),
            // Local doc.outline and zero-network catalog.search remain allowed
            localOutline: await tryCall(() => doc.outline("secret.txt")),
            catalogSearch: await tryCall(() => catalog.search("arxiv", {limit: 2})),
        };
    }"#;

    let res = app
        .run_exec_tool(
            json!({"code": code}),
            "taint-sess".into(),
            CancellationToken::new(),
            None,
        )
        .await;
    assert_ne!(
        res.is_error,
        Some(true),
        "unexpected error: {}",
        text_of(&res)
    );
    let out: Value = serde_json::from_str(&text_of(&res)).expect("json output");
    assert_eq!(out["p1Ok"], true);
    assert_eq!(out["localOk"], true);
    for blocked in [
        "webSearch",
        "sourceProtocol",
        "sourceCall",
        "scholarSearch",
        "webRequestSameHost",
        "webFetchNewHost",
        "remoteDocRead",
    ] {
        let v = out[blocked].as_str().unwrap_or("");
        assert!(
            v.starts_with("PermissionDenied:"),
            "{blocked} must be refused with PermissionDenied, got: {v:?}"
        );
    }
    assert_eq!(out["webFetchAllowedHost"], "ok");
    assert_eq!(out["localOutline"], "ok");
    assert_eq!(out["catalogSearch"], "ok");
}

// ---------------------------------------------------------------------------
// N1: NVD HttpResponse.body / .json equality replay through exec
// ---------------------------------------------------------------------------

#[tokio::test]
async fn n1_nvd_replay_http_response_body_equals_json_and_has_no_text() {
    let fixture_path = workspace_root().join("crates/manifests/fixtures/w5c/nvd/cve/default.json");
    let net: Arc<dyn NetClient> = Arc::new(ReplayNet::from_file(&fixture_path).unwrap());
    let store: Arc<dyn StoreApi> = Arc::new(NullStore::new());
    let services = assemble_services(Arc::new(Config::default()), net, store).unwrap();
    let app = AppServer::new(services, TransportKind::Stdio).unwrap();

    let code = r#"async () => {
        const r = await web.request({
            url: "https://services.nvd.nist.gov/rest/json/cves/2.0?cveId=CVE-2024-3094",
            headers: { Accept: "application/json" }
        });
        const parsedFromBody = JSON.parse(r.body);
        return {
            status: r.status,
            hasBody: typeof r.body === "string" && r.body.length > 0,
            hasText: "text" in r,
            hasJson: r.json !== undefined && r.json !== null,
            equal: JSON.stringify(parsedFromBody) === JSON.stringify(r.json),
            cveId: r.json.vulnerabilities[0].cve.id
        };
    }"#;

    let res = app
        .run_exec_tool(
            json!({"code": code}),
            "n1-sess".into(),
            CancellationToken::new(),
            None,
        )
        .await;
    assert_ne!(
        res.is_error,
        Some(true),
        "N1 replay failed: {}",
        text_of(&res)
    );
    let out: Value = serde_json::from_str(&text_of(&res)).expect("json output");
    assert_eq!(out["status"], 200);
    assert_eq!(out["hasBody"], true);
    assert_eq!(out["hasText"], false);
    assert_eq!(out["hasJson"], true);
    assert_eq!(out["equal"], true);
    assert_eq!(out["cveId"], "CVE-2024-3094");
}

// ---------------------------------------------------------------------------
// M16: search({query: "tokio crate latest stable version", scope: "code"}) #1
// ---------------------------------------------------------------------------

#[tokio::test]
async fn m16_code_scope_routes_crates_claimant_to_number_one() {
    let crates_dir = workspace_root().join("crates/engines-claims/tests/fixtures/crates");
    let net: Arc<dyn NetClient> = Arc::new(ReplayNet::from_dir(&crates_dir).unwrap());
    let store: Arc<dyn StoreApi> = Arc::new(NullStore::new());
    let mut cfg = Config::default();
    cfg.health_enabled = false;
    cfg.hedge_enabled = false;
    cfg.max_results_per_engine = 5;
    let services = assemble_services(Arc::new(cfg), net, store).unwrap();
    let app = AppServer::new(services, TransportKind::Stdio).unwrap();

    // Direct SearchApi check so we can assert exact #1 URL/source.
    let mut req = SearchRequest::new("tokio crate latest stable version").with_scope("code");
    req.limit = 5;
    let resp = app
        .services()
        .search
        .search(req, &CallCx::default())
        .await
        .expect("code search");
    assert!(
        !resp.results.is_empty(),
        "expected crates hit, notes={:?}",
        resp.notes
    );
    assert_eq!(
        resp.results[0].source, "crates",
        "M16: crates claimant must win #1 in scope=code; got {:?}",
        resp.results
    );
    assert!(
        resp.results[0].url.contains("crates.io/crates/tokio"),
        "unexpected #1 url: {}",
        resp.results[0].url
    );

    // Also run through MCP `search` tool boundary.
    let tool_res = app
        .run_search_tool(
            json!({"query": "tokio crate latest stable version", "scope": "code", "limit": 5}),
            &CallCx::default(),
        )
        .await;
    assert_ne!(tool_res.is_error, Some(true), "{}", text_of(&tool_res));
    let md = text_of(&tool_res);
    assert!(md.contains("crates.io/crates/tokio"), "{md}");
}

// ---------------------------------------------------------------------------
// Scope routing, snake_case inputs, cache/catalog, budget, catch_unwind
// ---------------------------------------------------------------------------

struct SpySearchApi {
    seen: Mutex<Vec<SearchRequest>>,
    panic_msg: Option<&'static str>,
}

#[async_trait]
impl SearchApi for SpySearchApi {
    async fn search(&self, req: SearchRequest, _cx: &CallCx) -> Result<SearchResponse, FsmError> {
        if let Some(msg) = self.panic_msg {
            panic!("{msg}");
        }
        self.seen.lock().unwrap().push(req.clone());
        let mut resp = SearchResponse::new(req.query, req.scope);
        let mut hit = Hit::new("Spy hit", "https://example.org/spy");
        hit.snippet = "A spy snippet from SearchApi".into();
        hit.source = "spy".into();
        hit.sources = vec!["spy".into()];
        resp.results = vec![hit];
        resp.engines = vec!["spy".into()];
        Ok(resp)
    }
}

struct SpyScholarApi {
    seen: Mutex<Vec<ScholarQuery>>,
}

#[async_trait]
impl ScholarApi for SpyScholarApi {
    async fn search(&self, q: ScholarQuery, _cx: &CallCx) -> Result<WorkResult, FsmError> {
        self.seen.lock().unwrap().push(q.clone());
        let mut w = ScholarlyWork::default();
        w.title = "Attention Is All You Need".into();
        w.authors = vec![AuthorRef::new("Ashish Vaswani")];
        w.year = Some(2017);
        w.work_type = "article".into();
        w.url = "https://arxiv.org/abs/1706.03762".into();
        w.sources = vec!["arxiv".into()];
        let mut wr = WorkResult::default();
        wr.works = vec![w];
        wr.meta.query = q.query;
        wr.meta.subscope = q.subscope;
        wr.meta.sources = vec!["arxiv".into()];
        Ok(wr)
    }
    async fn work(&self, _: &str, _: &CallCx) -> Result<Option<ScholarlyWork>, FsmError> {
        Ok(None)
    }
    async fn graph(&self, _: &str, _: GraphOpts, _: &CallCx) -> Result<CitationGraph, FsmError> {
        Err(FsmError::invalid("unused"))
    }
    async fn fulltext(
        &self,
        _: &str,
        _: fsm_core::FulltextOpts,
        _: &CallCx,
    ) -> Result<FulltextResult, FsmError> {
        Err(FsmError::invalid("unused"))
    }
    async fn integrity(&self, _: &str, _: &CallCx) -> Result<IntegrityReport, FsmError> {
        Err(FsmError::invalid("unused"))
    }
    async fn cite(
        &self,
        _: fsm_core::CiteInput,
        _: fsm_core::CiteStyle,
        _: &CallCx,
    ) -> Result<String, FsmError> {
        Err(FsmError::invalid("unused"))
    }
    async fn entity(
        &self,
        _: EntityKind,
        _: &str,
        _: Value,
        _: &CallCx,
    ) -> Result<Value, FsmError> {
        Err(FsmError::invalid("unused"))
    }
}

#[tokio::test]
async fn scope_routing_snake_case_cache_catalog_and_academic_dispatch() {
    let tmp = TempDir::new().unwrap();
    let store = Arc::new(SqliteStore::open_path(tmp.path().join("test.sqlite")).unwrap());
    let mut page = PageResult::default();
    page.url = "https://example.org/cached-rust".into();
    page.title = "Cached Rust Guide".into();
    page.content = "Tokio async runtime guide for Rust developers.".into();
    page.method = FetchMethod::Http;
    page.retrieved_at = "2026-11-25T00:00:00Z".into();
    store.put_page(&page, 3600).await.unwrap();

    let net: Arc<dyn NetClient> = Arc::new(ReplayNet::from_fixtures(Vec::new()).unwrap());
    let mut services = assemble_services(
        Arc::new(Config::default()),
        net,
        store.clone() as Arc<dyn StoreApi>,
    )
    .unwrap();
    let spy_search = Arc::new(SpySearchApi {
        seen: Mutex::new(Vec::new()),
        panic_msg: None,
    });
    let spy_scholar = Arc::new(SpyScholarApi {
        seen: Mutex::new(Vec::new()),
    });
    services.search = spy_search.clone();
    services.scholar = spy_scholar.clone();
    let app = AppServer::new(services, TransportKind::Stdio).unwrap();

    // 1. Catalog scope + synonym `sdk`
    let cat = app
        .run_search_tool(
            json!({"query": "clinical trials", "scope": "sdk"}),
            &CallCx::default(),
        )
        .await;
    assert_ne!(cat.is_error, Some(true));
    assert!(
        text_of(&cat).contains("clinicaltrials"),
        "{}",
        text_of(&cat)
    );

    // 2. Cache scope (valid hit + invalid FTS5 hint)
    let cache_hit = app
        .run_search_tool(
            json!({"query": "tokio", "scope": "cache"}),
            &CallCx::default(),
        )
        .await;
    assert_ne!(cache_hit.is_error, Some(true));
    assert!(
        text_of(&cache_hit).contains("Cached Rust Guide"),
        "{}",
        text_of(&cache_hit)
    );

    let bad_fts = app
        .run_search_tool(
            json!({"query": "\"unterminated phrase", "scope": "cache"}),
            &CallCx::default(),
        )
        .await;
    assert_ne!(bad_fts.is_error, Some(true));
    assert!(
        text_of(&bad_fts).to_lowercase().contains("quote"),
        "{}",
        text_of(&bad_fts)
    );

    // 3. Academic scope & sub-scope route directly to ScholarApi
    let acad = app
        .run_search_tool(
            json!({"query": "attention", "scope": "academic.cs", "limit": 5}),
            &CallCx::default(),
        )
        .await;
    assert_ne!(acad.is_error, Some(true));
    assert!(text_of(&acad).contains("Attention Is All You Need"));
    {
        let sqs = spy_scholar.seen.lock().unwrap();
        assert_eq!(sqs.len(), 1);
        assert_eq!(sqs[0].subscope.as_deref(), Some("cs"));
        assert_eq!(sqs[0].limit, 5);
    }

    // 4. Academic scope WITH explicit non-scholar source falls back to SearchApi
    let non_scholar = app
        .run_search_tool(
            json!({"query": "attention", "scope": "academic", "sources": ["duckduckgo"]}),
            &CallCx::default(),
        )
        .await;
    assert_ne!(non_scholar.is_error, Some(true));
    assert_eq!(spy_search.seen.lock().unwrap().len(), 1);

    // 5. Snake_case `max_age_hours: 0` converts to SearchRequest.max_age_hours == Some(0.0)
    let web = app
        .run_search_tool(
            json!({"query": "rust", "max_age_hours": 0, "limit": 999}),
            &CallCx::default(),
        )
        .await;
    assert_ne!(web.is_error, Some(true));
    {
        let reqs = spy_search.seen.lock().unwrap();
        let last = reqs.last().unwrap();
        assert_eq!(last.max_age_hours, Some(0.0));
        assert_eq!(last.limit, 50, "limit clamped to 50");
    }

    // 6. Unknown scope, empty query, unknown field rejected with actionable isError: true
    let bad_scope = app
        .run_search_tool(
            json!({"query": "x", "scope": "nonexistent"}),
            &CallCx::default(),
        )
        .await;
    assert_eq!(bad_scope.is_error, Some(true));
    assert!(text_of(&bad_scope).contains("valid scopes"));

    let empty_q = app
        .run_search_tool(json!({"query": "   "}), &CallCx::default())
        .await;
    assert_eq!(empty_q.is_error, Some(true));
    assert!(text_of(&empty_q).contains("query must not be empty"));

    let unknown_arg = app
        .run_search_tool(json!({"query": "x", "bogus": 1}), &CallCx::default())
        .await;
    assert_eq!(unknown_arg.is_error, Some(true));

    store.close().await.unwrap();
}

#[tokio::test]
async fn catch_unwind_contains_panics_without_leaking_secrets() {
    let net: Arc<dyn NetClient> = Arc::new(ReplayNet::from_fixtures(Vec::new()).unwrap());
    let store: Arc<dyn StoreApi> = Arc::new(NullStore::new());
    let mut services = assemble_services(Arc::new(Config::default()), net, store).unwrap();
    services.search = Arc::new(SpySearchApi {
        seen: Mutex::new(Vec::new()),
        panic_msg: Some("SUPER_SECRET_INTERNAL_PANIC_KEY"),
    });
    let app = AppServer::new(services, TransportKind::Stdio).unwrap();

    // Panic in `search` tool
    let r = app
        .run_search_tool(json!({"query": "boom"}), &CallCx::default())
        .await;
    assert_eq!(r.is_error, Some(true));
    let txt = text_of(&r);
    assert!(!txt.contains("SUPER_SECRET"), "panic leaked: {txt}");
    assert!(txt.contains("internal error (search_panic)"), "{txt}");
    assert!(
        txt.contains("retrying the same call will not help"),
        "{txt}"
    );

    // Panic inside `HostServices` called from `exec`
    let r2 = app
        .run_exec_tool(
            json!({"code": "async () => await web.search('boom')"}),
            "panic-sess".into(),
            CancellationToken::new(),
            None,
        )
        .await;
    assert_eq!(r2.is_error, Some(true));
    let txt2 = text_of(&r2);
    assert!(!txt2.contains("SUPER_SECRET"), "panic leaked: {txt2}");
    assert!(txt2.contains("internal error"), "{txt2}");
}

#[tokio::test]
async fn tool_call_budget_returns_non_error_stop_notice() {
    use rmcp::ServiceExt;
    let mut cfg = Config::default();
    cfg.tool_call_budget = 2;
    let net: Arc<dyn NetClient> = Arc::new(ReplayNet::from_fixtures(Vec::new()).unwrap());
    let store: Arc<dyn StoreApi> = Arc::new(NullStore::new());
    let services = assemble_services(Arc::new(cfg), net, store).unwrap();
    let app = AppServer::new(services, TransportKind::Stdio).unwrap();

    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let _srv = tokio::spawn(async move {
        let (sr, sw) = tokio::io::split(server_io);
        if let Ok(svc) = app.serve((sr, sw)).await {
            let _ = svc.waiting().await;
        }
    });
    let (cr, mut cw) = tokio::io::split(client_io);
    let mut reader = tokio::io::BufReader::new(cr);

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    async fn send_req(
        cw: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
        reader: &mut tokio::io::BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
        msg: Value,
    ) -> Value {
        let mut s = serde_json::to_string(&msg).unwrap();
        s.push('\n');
        cw.write_all(s.as_bytes()).await.unwrap();
        cw.flush().await.unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        serde_json::from_str::<Value>(&line).unwrap()
    }

    // Init
    let _ = send_req(
        &mut cw,
        &mut reader,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}),
    )
    .await;
    cw.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await
        .unwrap();
    cw.flush().await.unwrap();

    // Calls 1 & 2 succeed; call 3 returns non-error budget notice.
    for id in [2, 3] {
        let r = send_req(
            &mut cw,
            &mut reader,
            json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"exec","arguments":{"code":"1+1"}}}),
        )
        .await;
        assert_eq!(r["result"]["content"][0]["text"], "2");
    }
    let third = send_req(
        &mut cw,
        &mut reader,
        json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"exec","arguments":{"code":"1+1"}}}),
    )
    .await;
    assert_ne!(
        third["result"]["isError"],
        json!(true),
        "budget notice must be is_error=false"
    );
    let body = third["result"]["content"][0]["text"].as_str().unwrap();
    assert!(body.contains("Tool budget used up (2 calls)"), "{body}");
    assert!(body.contains("Do not call another tool"), "{body}");
}

#[tokio::test]
async fn exec_truncation_saves_raw_json_under_exec_handle_and_respects_m7_sessions() {
    let tmp = TempDir::new().unwrap();
    let store = Arc::new(SqliteStore::open_path(tmp.path().join("kv.sqlite")).unwrap());
    let net: Arc<dyn NetClient> = Arc::new(ReplayNet::from_fixtures(Vec::new()).unwrap());
    let services = assemble_services(
        Arc::new(Config::default()),
        net,
        store.clone() as Arc<dyn StoreApi>,
    )
    .unwrap();
    let app = AppServer::new(services, TransportKind::Stdio).unwrap();

    // 1. Produce large structured JSON that overflows max_tokens=500.
    let big = app
        .run_exec_tool(
            json!({
                "code": "async () => Array.from({length: 120}, (_, i) => ({idx: i, payload: 'alpha-beta-gamma-'.repeat(8)}))",
                "max_tokens": 500
            }),
            "session-a".into(),
            CancellationToken::new(),
            None,
        )
        .await;
    assert_ne!(big.is_error, Some(true));
    let txt = text_of(&big);
    assert!(txt.contains("--- TRUNCATED ["), "{txt}");
    let re = regex::Regex::new(r#"cache\.get\("(exec:[0-9a-z]{8})"\)"#).unwrap();
    let handle = re
        .captures(&txt)
        .expect("overflow handle in output")
        .get(1)
        .unwrap()
        .as_str()
        .to_owned();

    // 2. Read back the exec:<id> handle from a DIFFERENT session (M7: exec:* ignores session_id).
    let read_back = app
        .run_exec_tool(
            json!({"code": format!("async () => {{ const d = await cache.get({handle:?}); return {{ len: d.length, lastIdx: d[119].idx }}; }}")}),
            "session-b".into(),
            CancellationToken::new(),
            None,
        )
        .await;
    assert_ne!(read_back.is_error, Some(true), "{}", text_of(&read_back));
    let parsed: Value = serde_json::from_str(&text_of(&read_back)).unwrap();
    assert_eq!(parsed["len"], 120);
    assert_eq!(parsed["lastIdx"], 119);

    // 3. M7 private vs shared cache.put isolation + rejection of `exec:` key in cache.put.
    let put_priv = app
        .run_exec_tool(
            json!({"code": "async () => { await cache.put('priv.k', {v: 1}); await cache.put('shared.k', {v: 2}, {shared: true}); return 'ok'; }"}),
            "session-a".into(),
            CancellationToken::new(),
            None,
        )
        .await;
    assert_ne!(put_priv.is_error, Some(true), "{}", text_of(&put_priv));

    let from_b = app
        .run_exec_tool(
            json!({"code": "async () => ({ priv: await cache.get('priv.k'), shared: await cache.get('shared.k') })"}),
            "session-b".into(),
            CancellationToken::new(),
            None,
        )
        .await;
    let out_b: Value = serde_json::from_str(&text_of(&from_b)).unwrap();
    assert!(
        out_b["priv"].is_null(),
        "private key must stay isolated across sessions"
    );
    assert_eq!(
        out_b["shared"]["v"], 2,
        "shared key visible across sessions"
    );

    let bad_exec_key = app
        .run_exec_tool(
            json!({"code": "async () => { await cache.put('exec:forged', 1); }"}),
            "session-a".into(),
            CancellationToken::new(),
            None,
        )
        .await;
    assert_eq!(bad_exec_key.is_error, Some(true));

    // 4. cache.getPage enforces offline SSRF layers 1–3 before touching cache.
    let ssrf_cache = app
        .run_exec_tool(
            json!({"code": "async () => await cache.getPage('http://127.0.0.1/secret')"}),
            "session-a".into(),
            CancellationToken::new(),
            None,
        )
        .await;
    assert_eq!(ssrf_cache.is_error, Some(true));
    assert!(
        text_of(&ssrf_cache).to_lowercase().contains("refusing")
            || text_of(&ssrf_cache).to_lowercase().contains("unsafe")
    );

    // 5. Negative start/length on doc.read rejected with exact oracle phrase.
    let neg_doc = app
        .run_exec_tool(
            json!({"code": "async () => await doc.read('https://example.org/a.pdf', {start: -1})"}),
            "session-a".into(),
            CancellationToken::new(),
            None,
        )
        .await;
    assert_eq!(neg_doc.is_error, Some(true));
    assert!(
        text_of(&neg_doc).contains("start must be >= 0"),
        "{}",
        text_of(&neg_doc)
    );

    store.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// Inline image returns MCP ImageContent block + metadata TextContent
// ---------------------------------------------------------------------------

struct FakeImageFetch;

#[async_trait]
impl FetchApi for FakeImageFetch {
    async fn fetch(
        &self,
        url: &str,
        opts: FetchOpts,
        _cx: &CallCx,
    ) -> Result<FetchOutcome, FsmError> {
        assert!(opts.inline_image);
        let png: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let mut meta = InlineImageMeta::default();
        meta.url = url.to_owned();
        meta.media_type = "image/png".into();
        meta.bytes = png.len() as u64;
        meta.width = Some(1);
        meta.height = Some(1);
        meta.sha256 = "deadbeef".into();
        let handle = BinaryHandle::new("img-1", "image/png", fsm_core::Bytes::copy_from_slice(png));
        Ok(FetchOutcome::InlineImage {
            meta,
            binary: handle,
        })
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
    async fn request(&self, _: RawRequestOpts, _: &CallCx) -> Result<HttpResponse, FsmError> {
        Err(FsmError::invalid("unused"))
    }
    async fn structured(&self, _: &str, _: &CallCx) -> Result<StructuredMeta, FsmError> {
        Err(FsmError::invalid("unused"))
    }
    async fn research(
        &self,
        _: &str,
        _: ResearchOpts,
        _: &CallCx,
    ) -> Result<ResearchBrief, FsmError> {
        Err(FsmError::invalid("unused"))
    }
    async fn compare(
        &self,
        _: &str,
        _: Vec<String>,
        _: &CallCx,
    ) -> Result<Vec<CompareItem>, FsmError> {
        Ok(vec![])
    }
    async fn download(&self, _: &str, _: &CallCx) -> Result<DownloadResult, FsmError> {
        Err(FsmError::invalid("unused"))
    }
    async fn html_to_md(&self, html: &str) -> Result<String, FsmError> {
        Ok(html.to_owned())
    }
}

#[tokio::test]
async fn inline_image_arrives_as_mcp_image_content_block() {
    let net: Arc<dyn NetClient> = Arc::new(ReplayNet::from_fixtures(Vec::new()).unwrap());
    let store: Arc<dyn StoreApi> = Arc::new(NullStore::new());
    let mut services = assemble_services(Arc::new(Config::default()), net, store).unwrap();
    services.fetch = Arc::new(FakeImageFetch);
    let app = AppServer::new(services, TransportKind::Stdio).unwrap();

    let res = app
        .run_exec_tool(
            json!({"code": "async () => await web.fetch('https://example.org/a.png', {inlineImage: true})"}),
            "img-sess".into(),
            CancellationToken::new(),
            None,
        )
        .await;
    assert_ne!(res.is_error, Some(true), "{}", text_of(&res));
    let has_image = res.content.iter().any(|c| {
        c.as_image()
            .is_some_and(|img| img.mime_type == "image/png" && !img.data.is_empty())
    });
    assert!(
        has_image,
        "expected ImageContent block, got {:?}",
        res.content
    );
}

// ---------------------------------------------------------------------------
// test_no_key_positioning.py port
// ---------------------------------------------------------------------------

#[test]
fn no_key_positioning_invariants_hold_on_full_registry() {
    let cfg = Config::default();
    let deps = SourceDeps::new(
        Arc::new(ReplayNet::from_fixtures(Vec::new()).unwrap()),
        Arc::new(NullStore::new()),
        Arc::new(cfg.clone()),
    );
    let reg = build_registry(&deps).expect("full registry builds cleanly");

    // 1.0 line: >= 240 registered sources + 18 generic protocol adapters.
    assert!(
        reg.len() >= 240,
        "expected >= 240 sources registered, got {}",
        reg.len()
    );
    assert_eq!(reg.protocols().count(), 18, "expected 18 protocol adapters");

    // Every automatic pool (default, reserve, fresh, locale) must be keyless & registered.
    let mut auto_ids: Vec<&str> = Vec::new();
    auto_ids.extend(cfg.default_engines.iter().map(String::as_str));
    auto_ids.extend(cfg.reserve_engines.iter().map(String::as_str));
    auto_ids.extend(cfg.fresh_engines.iter().map(String::as_str));
    for list in cfg.locale_engines.values() {
        auto_ids.extend(list.iter().map(String::as_str));
    }
    for id in auto_ids {
        let src = reg
            .get(id)
            .unwrap_or_else(|| panic!("automatic pool engine {id:?} is not registered"));
        assert!(
            !matches!(src.decl().auth, AuthMode::RequiredKey { .. }),
            "automatic pool engine {id:?} requires an API key"
        );
        assert_eq!(
            src.availability(&cfg),
            Availability::Available,
            "automatic pool engine {id:?} not keyless-available"
        );
    }

    // AuthRequired error contract: points away from asking user for keys.
    let msg = render_error(&FsmError::AuthRequired {
        source_id: "serper".into(),
        env_var: "SEARCH_MCP_SERPER_API_KEY".into(),
    });
    let fine = msg.find("Nothing is wrong with the search").expect("fine");
    let do_not_ask = msg
        .find("Do not ask the user for an API key")
        .expect("do_not_ask");
    let operator = msg.find("Operator note").expect("operator");
    assert!(fine < do_not_ask && do_not_ask < operator, "{msg}");
    assert!(msg.contains("SEARCH_MCP_SERPER_API_KEY"), "{msg}");
    assert!(!msg.contains("uv run"), "{msg}");
}

#[tokio::test]
async fn host_services_rejects_unknown_and_prelude_sync_names() {
    let net: Arc<dyn NetClient> = Arc::new(ReplayNet::from_fixtures(Vec::new()).unwrap());
    let store: Arc<dyn StoreApi> = Arc::new(NullStore::new());
    let services = assemble_services(Arc::new(Config::default()), net, store).unwrap();
    let host = DefaultHostServices::new(services);
    let cx = CallCx::default();

    assert!(matches!(
        host.call("nope.fn", json!([]), &cx).await,
        Err(FsmError::InvalidInput(_))
    ));
    assert!(matches!(
        host.call("util.estimateTokens", json!(["hi"]), &cx).await,
        Err(FsmError::InvalidInput(_))
    ));
    // Direct util.sleep works outside sandbox too.
    let v: HostValue = host.call("util.sleep", json!([1]), &cx).await.unwrap();
    assert_eq!(v.json, Value::Null);
}

#[tokio::test]
async fn exec_renders_integrity_report_as_markdown_preserves_guardrail2_and_overflow_raw_json() {
    let tmp = TempDir::new().unwrap();
    let store = Arc::new(SqliteStore::open_path(tmp.path().join("kv.sqlite")).unwrap());
    let net: Arc<dyn NetClient> = Arc::new(ReplayNet::from_fixtures(Vec::new()).unwrap());
    let services = assemble_services(
        Arc::new(Config::default()),
        net,
        store.clone() as Arc<dyn StoreApi>,
    )
    .unwrap();
    let app = AppServer::new(services, TransportKind::Stdio).unwrap();

    // 1. IntegrityReport shape renders as Markdown, not raw JSON.
    let rep = app
        .run_exec_tool(
            json!({
                "code": "async () => ({ doi: '10.1038/s41586-020-2649-2', isRetracted: false, retractionNotices: [], scite: { supporting: 12, contradicting: 0, mentioning: 95 }, doajVetted: false, openalexFwci: 22.4 })"
            }),
            "sess-int".into(),
            CancellationToken::new(),
            None,
        )
        .await;
    assert_ne!(rep.is_error, Some(true), "{}", text_of(&rep));
    let rep_txt = text_of(&rep);
    assert!(
        rep_txt.starts_with("# Integrity report — `10.1038/s41586-020-2649-2`"),
        "expected Markdown heading, got: {rep_txt}"
    );
    assert!(rep_txt.contains("Not retracted"), "{rep_txt}");
    assert!(rep_txt.contains("12 supporting"), "{rep_txt}");

    // 2. Guardrail 2: extra user-added key falls back to JSON with zero field loss.
    let extended = app
        .run_exec_tool(
            json!({
                "code": "async () => ({ doi: '10.1038/s41586-020-2649-2', isRetracted: false, retractionNotices: [], customField: 'preserved' })"
            }),
            "sess-int".into(),
            CancellationToken::new(),
            None,
        )
        .await;
    assert_ne!(extended.is_error, Some(true), "{}", text_of(&extended));
    let ext_txt = text_of(&extended);
    let parsed: Value = serde_json::from_str(&ext_txt).expect("extended object falls back to JSON");
    assert_eq!(parsed["customField"], "preserved");

    // 3. Guardrail 4: overflowing untagged Markdown result truncates with exec:<id>
    //    and cache.get("exec:<id>") returns the full raw JSON value.
    let big_outline = app
        .run_exec_tool(
            json!({
                "code": "async () => ({ source: 'https://example.org/huge.pdf', format: 'pdf', title: 'Huge Report', pages: 400, sections: Array.from({length: 300}, (_, i) => ({ title: 'Section ' + i + ' Detailed Analysis of Long Subsystem Title', level: 1, startChar: i * 1000, page: i + 1 })) })",
                "max_tokens": 500
            }),
            "sess-int".into(),
            CancellationToken::new(),
            None,
        )
        .await;
    assert_ne!(
        big_outline.is_error,
        Some(true),
        "{}",
        text_of(&big_outline)
    );
    let out_txt = text_of(&big_outline);
    assert!(out_txt.starts_with("# Outline: Huge Report"), "{out_txt}");
    assert!(out_txt.contains("--- TRUNCATED ["), "{out_txt}");
    let re = regex::Regex::new(r#"cache\.get\("(exec:[0-9a-z]{8})"\)"#).unwrap();
    let handle = re
        .captures(&out_txt)
        .expect("exec handle in truncated markdown output")
        .get(1)
        .unwrap()
        .as_str()
        .to_owned();

    let fetched = app
        .run_exec_tool(
            json!({
                "code": format!("async () => {{ const o = await cache.get({handle:?}); return {{ title: o.title, n: o.sections.length, lastPage: o.sections[299].page }}; }}")
            }),
            "sess-other".into(),
            CancellationToken::new(),
            None,
        )
        .await;
    assert_ne!(fetched.is_error, Some(true), "{}", text_of(&fetched));
    let back: Value = serde_json::from_str(&text_of(&fetched)).unwrap();
    assert_eq!(back["title"], "Huge Report");
    assert_eq!(back["n"], 300);
    assert_eq!(back["lastPage"], 300);

    store.close().await.unwrap();
}
