//! `rmcp 3.11` 2-tool MCP server (`search` and `exec`) with all mandatory I4 shims,
//! scope routing, tool-call budget, progress keepalive, and `catch_unwind` boundary
//! (`SPEC.md` §2, §3.3–§3.4).

use crate::host::{DefaultHostServices, panic_detail};
use base64::Engine as _;
use fsm_core::text::{clip_chars, estimate_tokens, truncate_to_tokens};
use fsm_core::{
    AskMode, AskOpts, AskResponse, CallCx, CatalogQuery, Config, Freshness, FsmError, HostServices,
    RenderKind, ScholarQuery, SearchRequest, Services,
};
use fsm_script::{
    ExecOutcome, ExecValue, Limits, Renderer, ReplyOptions, ReplyPart, Sandbox, compose_reply,
    escape_delimiters, exec_call_cx, new_exec_id, new_nonce,
};
use fsm_surface::{
    RenderOpts, ToolDef, instructions, render, render_cache_hits, render_error, render_untagged,
};
use futures::FutureExt;
use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
    DiscoverResult, Icon, Implementation, JsonObject, ListToolsResult, MetaObject,
    PaginatedRequestParams, ProgressNotificationParam, ProtocolVersion, ServerCapabilities,
    ServerConfig, Tool, ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use serde::Deserialize;
use serde_json::Value;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// Server name advertised in MCP `Implementation` (`SPEC.md` §2.1).
pub const SERVER_NAME: &str = "search-mcp";
/// Human-readable server title (`SPEC.md` §2.1).
pub const SERVER_TITLE: &str = "Free Search";
/// Server website URL (`SPEC.md` §2.1).
pub const SERVER_WEBSITE_URL: &str = "https://github.com/sweetcornna/free-search-mcp";
/// Public HTTPS icon URL (never a `data:` URI; rides in 2026 `_meta.serverInfo`).
pub const SERVER_ICON_URL: &str =
    "https://raw.githubusercontent.com/sweetcornna/free-search-mcp/main/assets/icon.png";
/// Cache TTL for static list / discovery results (1 hour in ms).
pub const STATIC_LIST_TTL_MS: u64 = 3_600_000;
/// Default interval for `notifications/progress` keepalive during `exec` (`SPEC.md` §3.3).
pub const DEFAULT_PROGRESS_KEEPALIVE: Duration = Duration::from_secs(10);

/// `http` header carrying the Streamable HTTP session ID (`SPEC.md` §3.4 I13).
const HEADER_MCP_SESSION_ID: &str = "mcp-session-id";

/// Canonical `Implementation` identity shared across `initialize`, `server/discover`,
/// and 2026-era `_meta["io.modelcontextprotocol/serverInfo"]` stamps.
pub fn server_implementation() -> Implementation {
    Implementation::new(SERVER_NAME, env!("CARGO_PKG_VERSION"))
        .with_title(SERVER_TITLE)
        .with_description(
            "Keyless multi-engine web search, academic research, page fetching, and document reading. Runs locally; no account, no API key.",
        )
        .with_website_url(SERVER_WEBSITE_URL)
        .with_icons(vec![
            Icon::new(SERVER_ICON_URL)
                .with_mime_type("image/png")
                .with_sizes(vec!["128x128".to_owned()]),
        ])
}

fn is_modern(version: Option<&ProtocolVersion>) -> bool {
    version.is_some_and(|v| v.as_str() >= ProtocolVersion::V_2026_07_28.as_str())
}

/// Stamp `_meta["io.modelcontextprotocol/serverInfo"]` on every `2026-07-28+` result
/// (rmcp 3.11 stamps only `server/discover` by default; `SPEC.md` §2.1 shim #3).
pub fn stamp_server_info(meta: &mut Option<MetaObject>, version: Option<&ProtocolVersion>) {
    if is_modern(version) {
        let mut m = meta.take().unwrap_or_default();
        m.0.insert(
            "io.modelcontextprotocol/serverInfo".to_owned(),
            serde_json::to_value(server_implementation())
                .expect("Implementation serialization cannot fail"),
        );
        *meta = Some(m);
    }
}

/// Convert a [`ToolDef`] into an `rmcp` [`Tool`].
///
/// Always includes `title` and `annotations` across every protocol era so all 5 eras
/// see identical tool definitions (2024-11-05 compact `result.tools` = 5,954 B <= 6,000 B).
/// Never sets `output_schema`, and strips any top-level `"$schema"`.
pub fn to_rmcp_tool(def: &ToolDef, _version: Option<&ProtocolVersion>) -> Tool {
    let mut schema: JsonObject = def.input_schema.as_object().cloned().unwrap_or_default();
    schema.remove("$schema");

    let a = def.annotations();
    Tool::new(def.name, def.description.clone(), Arc::new(schema))
        .with_title(def.title())
        .with_annotations(
            ToolAnnotations::new()
                .read_only(a.read_only_hint)
                .destructive(a.destructive_hint)
                .idempotent(a.idempotent_hint)
                .open_world(a.open_world_hint),
        )
}

/// Snake_case input schema for MCP tool `search` (`SPEC.md` §2.3, `w0a-review.md` Note #7).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchToolInput {
    pub query: String,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub sources: Option<Vec<String>>,
    #[serde(default)]
    pub freshness: Option<Freshness>,
    #[serde(default)]
    pub domains: Option<Vec<String>>,
    #[serde(default)]
    pub lang: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub max_age_hours: Option<f64>,
}

/// Input schema for MCP tool `exec` (`SPEC.md` §2.4).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecToolInput {
    pub code: String,
    #[serde(default)]
    pub max_tokens: Option<i64>,
    #[serde(default)]
    pub timeout_s: Option<i64>,
}

/// Snake_case input schema for MCP tool `ask` (ruling R5).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AskToolInput {
    pub question: String,
    #[serde(default)]
    pub freshness: Option<Freshness>,
    #[serde(default)]
    pub lang: Option<String>,
    #[serde(default)]
    pub domains: Option<Vec<String>>,
    #[serde(default)]
    pub mode: Option<AskMode>,
    #[serde(default)]
    pub max_tokens: Option<i64>,
}

/// `fsm_script::Renderer` adapter backed by `fsm_surface::render`.
///
/// Passes a large token cap so `fsm_surface::render` produces complete Markdown
/// and `fsm_script::compose_reply` owns final truncation + `exec:<id>` overflow storage.
struct SurfaceRenderer;

impl Renderer for SurfaceRenderer {
    fn render(&self, kind: RenderKind, value: &Value) -> Option<String> {
        let opts = RenderOpts::default().with_max_tokens(1_000_000);
        Some(render(kind, value, &opts))
    }
}

/// Transport mode (governs `session_id` resolution per `SPEC.md` §3.4 I13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    Stdio,
    StreamableHttp,
}

struct ReadyState {
    services: Services,
    host: Arc<dyn HostServices>,
    sandbox: Sandbox,
    live_store: Option<Arc<fsm_store::SqliteStore>>,
}

type InitTaskHandle = tokio::task::JoinHandle<Result<Arc<ReadyState>, FsmError>>;

struct LazyRuntime {
    cell: tokio::sync::OnceCell<Arc<ReadyState>>,
    init_task: tokio::sync::Mutex<Option<InitTaskHandle>>,
}

#[derive(Clone)]
enum AppRuntime {
    Ready(Arc<ReadyState>),
    Lazy(Arc<LazyRuntime>),
}

/// Shared state of the MCP server. Cheap to clone across HTTP requests.
#[derive(Clone)]
pub struct AppServer {
    cfg: Arc<Config>,
    runtime: AppRuntime,
    transport: TransportKind,
    stdio_session_id: Arc<String>,
    tool_calls_used: Arc<AtomicU32>,
    keepalive_interval: Duration,
}

impl AppServer {
    /// Construct an [`AppServer`] backed by `services` and the default [`DefaultHostServices`].
    pub fn new(services: Services, transport: TransportKind) -> Result<Self, FsmError> {
        let host: Arc<dyn HostServices> = Arc::new(DefaultHostServices::new(services.clone()));
        Self::with_host(services, host, transport)
    }

    /// Construct an [`AppServer`] with a custom [`HostServices`] (useful in unit/integration tests).
    pub fn with_host(
        services: Services,
        host: Arc<dyn HostServices>,
        transport: TransportKind,
    ) -> Result<Self, FsmError> {
        let sandbox = Sandbox::new(fsm_surface::HOST_API)?;
        let cfg = Arc::clone(&services.cfg);
        let ready = Arc::new(ReadyState {
            services,
            host,
            sandbox,
            live_store: None,
        });
        Ok(Self {
            cfg,
            runtime: AppRuntime::Ready(ready),
            transport,
            stdio_session_id: Arc::new(uuid::Uuid::new_v4().to_string()),
            tool_calls_used: Arc::new(AtomicU32::new(0)),
            keepalive_interval: DEFAULT_PROGRESS_KEEPALIVE,
        })
    }

    /// Construct a fast-starting [`AppServer`] that answers `initialize`, `server/discover`,
    /// and `tools/list` immediately while assembling the SQLite store, HTTP client, source
    /// registry, catalog BM25 index, and QuickJS sandbox concurrently in a background thread.
    pub fn new_lazy(cfg: Arc<Config>, transport: TransportKind) -> Self {
        let bg_cfg = Arc::clone(&cfg);
        let is_http = transport == TransportKind::StreamableHttp;
        let handle = tokio::spawn(async move {
            let build_cfg = Arc::clone(&bg_cfg);
            let (rt, sandbox) = tokio::task::spawn_blocking(move || {
                let _ = fsm_store::downloads::purge_expired(&build_cfg, None);
                let rt = crate::assembly::assemble_live_for_transport(build_cfg, is_http)?;
                let sandbox = Sandbox::new(fsm_surface::HOST_API)?;
                Ok::<_, FsmError>((rt, sandbox))
            })
            .await
            .map_err(|e| FsmError::internal("startup", format!("startup task failed: {e}")))??;

            if bg_cfg.prewarm {
                let http = Arc::clone(&rt.http);
                tokio::spawn(async move {
                    let _ = http.prewarm(crate::assembly::PREWARM_TARGETS).await;
                });
            }
            let host: Arc<dyn HostServices> =
                Arc::new(DefaultHostServices::new(rt.services.clone()));
            Ok(Arc::new(ReadyState {
                services: rt.services,
                host,
                sandbox,
                live_store: Some(rt.store),
            }))
        });

        let lazy = Arc::new(LazyRuntime {
            cell: tokio::sync::OnceCell::new(),
            init_task: tokio::sync::Mutex::new(Some(handle)),
        });
        Self {
            cfg,
            runtime: AppRuntime::Lazy(lazy),
            transport,
            stdio_session_id: Arc::new(uuid::Uuid::new_v4().to_string()),
            tool_calls_used: Arc::new(AtomicU32::new(0)),
            keepalive_interval: DEFAULT_PROGRESS_KEEPALIVE,
        }
    }

    /// Await the assembled runtime state (instant when already built).
    async fn ready(&self) -> Result<Arc<ReadyState>, FsmError> {
        match &self.runtime {
            AppRuntime::Ready(st) => Ok(Arc::clone(st)),
            AppRuntime::Lazy(lazy) => lazy
                .cell
                .get_or_try_init(|| async {
                    let handle = lazy.init_task.lock().await.take().ok_or_else(|| {
                        FsmError::internal("startup", "server runtime initialization failed")
                    })?;
                    handle.await.map_err(|e| {
                        FsmError::internal("startup", format!("startup task join error: {e}"))
                    })?
                })
                .await
                .map(Arc::clone),
        }
    }

    /// Gracefully close the underlying SQLite store (or cancel in-flight background init on early exit).
    pub async fn shutdown(&self) {
        if let AppRuntime::Lazy(lazy) = &self.runtime {
            if let Some(st) = lazy.cell.get() {
                if let Some(store) = &st.live_store {
                    let _ = store.close().await;
                }
                return;
            }
            if let Some(handle) = lazy.init_task.lock().await.take() {
                if handle.is_finished() {
                    if let Ok(Ok(st)) = handle.await
                        && let Some(store) = &st.live_store
                    {
                        let _ = store.close().await;
                    }
                } else {
                    handle.abort();
                }
            }
        }
    }

    /// Override the `notifications/progress` keepalive interval (for fast tests).
    #[must_use]
    pub fn with_keepalive_interval(mut self, interval: Duration) -> Self {
        self.keepalive_interval = interval;
        self
    }

    pub fn services(&self) -> &Services {
        match &self.runtime {
            AppRuntime::Ready(st) => &st.services,
            AppRuntime::Lazy(lazy) => {
                &lazy
                    .cell
                    .get()
                    .expect("AppServer::services() requires an initialized runtime")
                    .services
            }
        }
    }

    pub fn host(&self) -> &Arc<dyn HostServices> {
        match &self.runtime {
            AppRuntime::Ready(st) => &st.host,
            AppRuntime::Lazy(lazy) => {
                &lazy
                    .cell
                    .get()
                    .expect("AppServer::host() requires an initialized runtime")
                    .host
            }
        }
    }

    /// Resolve `session_id` per `SPEC.md` §3.4 (I13):
    /// - Stdio: process UUID.
    /// - Streamable HTTP: `Mcp-Session-Id` header when present, else a per-request random UUID.
    fn resolve_session_id(&self, ctx: &RequestContext<RoleServer>) -> String {
        match self.transport {
            TransportKind::Stdio => (*self.stdio_session_id).clone(),
            TransportKind::StreamableHttp => ctx
                .extensions
                .get::<http::request::Parts>()
                .and_then(|p| p.headers.get(HEADER_MCP_SESSION_ID))
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        }
    }

    /// Check `SEARCH_MCP_TOOL_CALL_BUDGET`. Returns `Some(notice)` once exhausted.
    fn check_tool_budget(&self) -> Option<String> {
        let budget = self.cfg.tool_call_budget;
        if budget == 0 {
            return None;
        }
        let prev = self.tool_calls_used.fetch_add(1, Ordering::SeqCst);
        if prev >= budget {
            Some(fsm_surface::tool_budget_notice(budget))
        } else {
            None
        }
    }

    /// Execute the `search` MCP tool directly (used by both MCP `call_tool` and tests).
    pub async fn run_search_tool(&self, raw_args: Value, cx: &CallCx) -> CallToolResult {
        let input: SearchToolInput = match serde_json::from_value(raw_args) {
            Ok(v) => v,
            Err(e) => {
                return CallToolResult::error(vec![ContentBlock::text(render_error(
                    &FsmError::invalid(format!("invalid arguments for `search`: {e}")),
                ))]);
            }
        };

        let result = AssertUnwindSafe(self.execute_search(input, cx))
            .catch_unwind()
            .await;
        match result {
            Ok(Ok(markdown)) => CallToolResult::success(vec![ContentBlock::text(markdown)]),
            Ok(Err(err)) => {
                if matches!(err, FsmError::Internal { .. }) {
                    tracing::error!(error = %err, "search tool internal error");
                }
                CallToolResult::error(vec![ContentBlock::text(render_error(&err))])
            }
            Err(payload) => {
                let detail = panic_detail(payload);
                tracing::error!(panic = %detail, "search tool panicked");
                CallToolResult::error(vec![ContentBlock::text(render_error(&FsmError::internal(
                    "search_panic",
                    "search panicked unexpectedly",
                )))])
            }
        }
    }

    async fn execute_search(
        &self,
        input: SearchToolInput,
        cx: &CallCx,
    ) -> Result<String, FsmError> {
        let raw_scope = input
            .scope
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("web")
            .to_owned();
        let scope = fsm_core::scopes::resolve(&raw_scope).ok_or_else(|| {
            FsmError::invalid(format!(
                "unknown scope '{raw_scope}'; valid scopes: {}",
                fsm_core::scopes::valid_scope_list()
            ))
        })?;

        let query = input.query.trim().to_owned();
        if query.is_empty() && scope.id != "catalog" {
            return Err(FsmError::invalid("query must not be empty"));
        }

        if let Some(h) = input.max_age_hours
            && (!h.is_finite() || h < 0.0)
        {
            return Err(FsmError::invalid("max_age_hours must be >= 0"));
        }

        let state = self.ready().await?;
        let services = &state.services;

        let default_limit = services.cfg.max_results_per_engine.clamp(1, 50);
        let limit = match input.limit {
            None => default_limit,
            Some(n) => n.clamp(1, 50) as u32,
        };

        let render_opts = RenderOpts::default().with_query(query.clone());

        match scope.id {
            // Zero-network source/protocol/API catalog search
            "catalog" => {
                let mut cq = CatalogQuery::new(query);
                cq.limit = limit;
                let hits = services.sources.catalog_search(cq).await?;
                let val = serde_json::to_value(hits)
                    .map_err(|e| FsmError::internal("catalog_json", e.to_string()))?;
                Ok(render(RenderKind::CatalogList, &val, &render_opts))
            }
            // Zero-network local FTS5 cache search
            "cache" => {
                let hits = services.store.cache_search(&query, limit).await?;
                Ok(render_cache_hits(&hits, &render_opts))
            }
            // Academic scopes route directly to ScholarApi unless explicit non-scholar sources are requested.
            id if id == "academic" || id.starts_with("academic.") => {
                let explicit = input.sources.as_deref().unwrap_or(&[]);
                let has_non_scholar = !explicit.is_empty()
                    && explicit.iter().any(|s| !is_scholar_source(services, s));
                if has_non_scholar {
                    run_web_search(services, query, &raw_scope, input, limit, cx, &render_opts)
                        .await
                } else {
                    let mut sq = ScholarQuery::new(query);
                    sq.subscope = id.strip_prefix("academic.").map(str::to_owned);
                    sq.sources = input.sources.filter(|v| !v.is_empty());
                    sq.limit = limit;
                    let works = services.scholar.search(sq, cx).await?;
                    let val = serde_json::to_value(works)
                        .map_err(|e| FsmError::internal("scholar_json", e.to_string()))?;
                    Ok(render(RenderKind::WorkResult, &val, &render_opts))
                }
            }
            // All other verticals & web search go through SearchApi
            _ => run_web_search(services, query, &raw_scope, input, limit, cx, &render_opts).await,
        }
    }

    /// Execute the `ask` MCP tool directly (used by MCP `call_tool`, CLI `ask`, HTTP `/v1/ask`, and tests).
    pub async fn execute_ask(
        &self,
        input: AskToolInput,
        cx: &CallCx,
    ) -> Result<(AskResponse, String), FsmError> {
        let q = input.question.trim();
        if q.is_empty() {
            return Err(FsmError::invalid("question must not be empty"));
        }
        let max_tokens = match input.max_tokens {
            None => 1500usize,
            Some(m) if (200..=8000).contains(&m) => m as usize,
            Some(_) => {
                return Err(FsmError::invalid("max_tokens must be between 200 and 8000"));
            }
        };
        let state = self.ready().await?;
        let Some(ask_svc) = &state.services.ask else {
            return Err(FsmError::internal(
                "ask_missing",
                "ask service is not wired into Services",
            ));
        };
        let mut opts = AskOpts::default();
        opts.mode = input.mode.unwrap_or(AskMode::Auto);
        opts.freshness = input.freshness;
        opts.lang = input.lang.filter(|s| !s.trim().is_empty());
        opts.domains = input.domains.unwrap_or_default();
        opts.max_tokens = Some(max_tokens);

        let resp = ask_svc.ask(q, opts, cx).await?;
        let val = serde_json::to_value(&resp)
            .map_err(|e| FsmError::internal("ask_json", e.to_string()))?;
        let render_opts = RenderOpts::default()
            .with_max_tokens(max_tokens)
            .with_query(q.to_owned());
        let markdown = render(RenderKind::AskResponse, &val, &render_opts);
        Ok((resp, markdown))
    }

    /// Execute the `ask` MCP tool returning a `CallToolResult` with panic boundary.
    pub async fn run_ask_tool(&self, raw_args: Value, cx: &CallCx) -> CallToolResult {
        let input: AskToolInput = match serde_json::from_value(raw_args) {
            Ok(v) => v,
            Err(e) => {
                return CallToolResult::error(vec![ContentBlock::text(render_error(
                    &FsmError::invalid(format!("invalid arguments for `ask`: {e}")),
                ))]);
            }
        };

        let result = AssertUnwindSafe(self.execute_ask(input, cx))
            .catch_unwind()
            .await;
        match result {
            Ok(Ok((_resp, markdown))) => {
                CallToolResult::success(vec![ContentBlock::text(markdown)])
            }
            Ok(Err(err)) => {
                if matches!(err, FsmError::Internal { .. }) {
                    tracing::error!(error = %err, "ask tool internal error");
                }
                CallToolResult::error(vec![ContentBlock::text(render_error(&err))])
            }
            Err(payload) => {
                let detail = panic_detail(payload);
                tracing::error!(panic = %detail, "ask tool panicked");
                CallToolResult::error(vec![ContentBlock::text(render_error(&FsmError::internal(
                    "ask_panic",
                    "ask panicked unexpectedly",
                )))])
            }
        }
    }

    /// Execute the `exec` MCP tool directly (used by both MCP `call_tool` and tests).
    pub async fn run_exec_tool(
        &self,
        raw_args: Value,
        session_id: String,
        cancel: tokio_util::sync::CancellationToken,
        progress: Option<(rmcp::service::Peer<RoleServer>, rmcp::model::ProgressToken)>,
    ) -> CallToolResult {
        let input: ExecToolInput = match serde_json::from_value(raw_args) {
            Ok(v) => v,
            Err(e) => {
                return CallToolResult::error(vec![ContentBlock::text(render_error(
                    &FsmError::invalid(format!("invalid arguments for `exec`: {e}")),
                ))]);
            }
        };

        if let Some(t) = input.timeout_s
            && !(1..=120).contains(&t)
        {
            return CallToolResult::error(vec![ContentBlock::text(render_error(
                &FsmError::invalid("timeout_s must be between 1 and 120"),
            ))]);
        }
        if let Some(m) = input.max_tokens
            && !(500..=20_000).contains(&m)
        {
            return CallToolResult::error(vec![ContentBlock::text(render_error(
                &FsmError::invalid("max_tokens must be between 500 and 20000"),
            ))]);
        }

        let state = match self.ready().await {
            Ok(s) => s,
            Err(err) => {
                tracing::error!(error = %err, "exec tool startup failed");
                return CallToolResult::error(vec![ContentBlock::text(render_error(&err))]);
            }
        };

        let limits = Limits::from_config(&state.services.cfg, input.timeout_s.map(|t| t as u64));
        let mut cx = exec_call_cx(session_id, &state.services.cfg, &limits);
        cx.cancel = cancel;

        // Wire notifications/progress keepalive every `keepalive_interval` (default 10 s)
        // plus forward explicit progress events when a client supplied a progressToken.
        let keepalive_guard = if let Some((peer, token)) = progress {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<(u64, u64, String)>(32);
            cx.progress_tx = Some(tx);
            let interval_dur = self.keepalive_interval;
            let stop_ct = cx.cancel.child_token();
            let handle = tokio::spawn(async move {
                let started = Instant::now();
                let mut tick = tokio::time::interval(interval_dur);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                // Skip the immediate t=0 tick; only emit keepalive while still running.
                tick.tick().await;
                let mut beats: u64 = 0;
                loop {
                    tokio::select! {
                        biased;
                        () = stop_ct.cancelled() => break,
                        msg = rx.recv() => {
                            let Some((cur, total, text)) = msg else { break };
                            let mut param = ProgressNotificationParam::new(token.clone(), cur as f64);
                            if total > 0 {
                                param.total = Some(total as f64);
                            }
                            if !text.is_empty() {
                                param.message = Some(text);
                            }
                            let _ = peer.notify_progress(param).await;
                        }
                        _ = tick.tick() => {
                            beats += 1;
                            let elapsed_s = started.elapsed().as_secs();
                            let mut param = ProgressNotificationParam::new(token.clone(), beats as f64);
                            param.message = Some(format!("exec running ({elapsed_s}s elapsed)"));
                            if peer.notify_progress(param).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
            Some(AbortHandleGuard(handle))
        } else {
            None
        };

        let cx = Arc::new(cx);
        let outcome = AssertUnwindSafe(state.sandbox.run(
            &input.code,
            Arc::clone(&state.host),
            Arc::clone(&cx),
            &limits,
        ))
        .catch_unwind()
        .await;

        drop(keepalive_guard);

        let outcome = match outcome {
            Ok(o) => o,
            Err(payload) => {
                let detail = panic_detail(payload);
                tracing::error!(panic = %detail, "exec sandbox panicked");
                return CallToolResult::error(vec![ContentBlock::text(render_error(
                    &FsmError::internal("exec_panic", "exec sandbox panicked unexpectedly"),
                ))]);
            }
        };

        let reply_opts = ReplyOptions::new(input.max_tokens.map(|m| m as usize), &cx.nonce);

        // Render recognized untagged domain results (e.g. IntegrityReport, scholar entities,
        // DocOutline, CompareItem[], DownloadResult, CatalogDetail, StructuredMeta, CacheHit[])
        // as Markdown while preserving Guardrail 2 (no unknown fields dropped) and Guardrail 4
        // (overflow saves raw JSON under `exec:<id>`).
        if let Ok(ExecValue::Json(ref raw_val)) = outcome.result
            && let Some(md) =
                render_untagged(raw_val, &RenderOpts::default().with_max_tokens(1_000_000))
        {
            let text = compose_untagged_markdown_reply(
                &outcome,
                raw_val,
                &md,
                state.services.store.as_ref(),
                &reply_opts,
            )
            .await;
            return CallToolResult::success(vec![ContentBlock::text(text)]);
        }

        let reply = compose_reply(
            &outcome,
            &SurfaceRenderer,
            state.services.store.as_ref(),
            &reply_opts,
        )
        .await;

        let content: Vec<ContentBlock> = reply
            .parts
            .into_iter()
            .map(|part| match part {
                ReplyPart::Text(s) => ContentBlock::text(s),
                ReplyPart::Image(handle) => {
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&handle.bytes);
                    ContentBlock::image(b64, handle.media_type)
                }
                _ => ContentBlock::text(String::new()),
            })
            .collect();

        if reply.is_error {
            CallToolResult::error(content)
        } else {
            CallToolResult::success(content)
        }
    }
}

async fn run_web_search(
    services: &Services,
    query: String,
    raw_scope: &str,
    input: SearchToolInput,
    limit: u32,
    cx: &CallCx,
    render_opts: &RenderOpts,
) -> Result<String, FsmError> {
    let mut req = SearchRequest::new(query.clone()).with_scope(raw_scope);
    req.sources = input.sources;
    req.freshness = input.freshness;
    req.domains = input.domains;
    req.lang = input.lang;
    req.limit = limit;
    req.max_age_hours = input.max_age_hours;

    let mut resp = services.search.search(req, cx).await?;
    if let Some(ask_svc) = &services.ask
        && services.cfg.agent_in_search
        && !services.cfg.agent_backend.eq_ignore_ascii_case("off")
    {
        resp.agent = ask_svc.answer_search(&query, &resp, cx).await;
    }
    let val =
        serde_json::to_value(resp).map_err(|e| FsmError::internal("search_json", e.to_string()))?;
    Ok(render(RenderKind::SearchResponse, &val, render_opts))
}

fn is_scholar_source(services: &Services, id: &str) -> bool {
    let low = id.trim().to_ascii_lowercase();
    if let Some(src) = services.registry.get(&low) {
        return src
            .decl()
            .scopes
            .iter()
            .any(|s| fsm_core::scopes::root(s) == "academic");
    }
    matches!(
        low.as_str(),
        "arxiv"
            | "crossref"
            | "openalex"
            | "semanticscholar"
            | "europepmc"
            | "openaire"
            | "doaj"
            | "pubmed"
            | "dblp"
            | "zbmath"
            | "datacite"
            | "clinicaltrials"
            | "core"
            | "inspire"
            | "openreview"
            | "biorxiv"
            | "medrxiv"
            | "hal"
            | "zenodo"
    )
}

const EXEC_OVERFLOW_BYTES: usize = 2 << 20;
const EXEC_OVERFLOW_TTL_S: u64 = 24 * 3600;

/// Format an untagged domain Markdown reply with exact parity to `fsm_script::compose_reply`:
/// nonced delimiters, token-budget truncation, raw-JSON `exec:<id>` overflow storage (Guardrail 4),
/// and console capture section.
async fn compose_untagged_markdown_reply(
    outcome: &ExecOutcome,
    raw_val: &Value,
    markdown: &str,
    store: &dyn fsm_core::StoreApi,
    opts: &ReplyOptions,
) -> String {
    let nonce = if opts.nonce.is_empty() {
        new_nonce()
    } else {
        opts.nonce.clone()
    };
    let console = format_exec_console(outcome, &nonce, opts.max_tokens / 2);
    let full_budget = opts.max_tokens.saturating_sub(estimate_tokens(&console));
    let escaped = escape_delimiters(markdown);

    let mut text = if estimate_tokens(&escaped) <= full_budget {
        escaped
    } else {
        let json_str = serde_json::to_string(raw_val).unwrap_or_default();
        let raw_tokens = estimate_tokens(&json_str);
        let key = format!("exec:{}", new_exec_id());
        let (stored, partial, kept_bytes) = if json_str.len() <= EXEC_OVERFLOW_BYTES {
            (raw_val.clone(), false, json_str.len())
        } else {
            let cut = clip_chars(&json_str, EXEC_OVERFLOW_BYTES / 4);
            let len = cut.len();
            (Value::String(cut.to_owned()), true, len)
        };
        let notice = match store.kv_put("", &key, &stored, EXEC_OVERFLOW_TTL_S).await {
            Ok(()) if partial => format!(
                "Full result saved [{nonce}]: cache.get(\"{key}\") returns the first {:.1} MiB of its JSON text as a string (partial; the whole result is {:.1} MiB), kept 24 h.",
                kept_bytes as f64 / (1 << 20) as f64,
                json_str.len() as f64 / (1 << 20) as f64
            ),
            Ok(()) => format!(
                "Full result saved [{nonce}]: cache.get(\"{key}\") returns the raw JSON (~{raw_tokens} tokens), kept 24 h."
            ),
            Err(e) => {
                let msg = escape_delimiters(clip_chars(&e.to_string(), 200));
                format!(
                    "Full result not saved [{nonce}]: {msg}. Return less, or cache.put() parts yourself."
                )
            }
        };
        let footer = format!("\n--- TRUNCATED [{nonce}] ---\n{notice}");
        let body_budget = full_budget.saturating_sub(estimate_tokens(&footer));
        let mut body = truncate_to_tokens(&escaped, body_budget).text;
        body.push_str(&footer);
        body
    };
    text.push_str(&console);
    text
}

fn format_exec_console(outcome: &ExecOutcome, nonce: &str, max_tokens: usize) -> String {
    if outcome.console.is_empty() && !outcome.console_truncated {
        return String::new();
    }
    let header = format!("\n--- console [{nonce}] ---\n");
    let mut body = escape_delimiters(&outcome.console.join("\n"));
    if outcome.console_truncated {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str("[console output cut at 2 KiB]");
    }
    let full = format!("{header}{body}");
    if estimate_tokens(&full) <= max_tokens {
        return full;
    }
    let body_budget = max_tokens.saturating_sub(estimate_tokens(&header));
    if body_budget == 0 {
        return String::new();
    }
    format!("{header}{}", truncate_to_tokens(&body, body_budget).text)
}

struct AbortHandleGuard(tokio::task::JoinHandle<()>);

impl Drop for AbortHandleGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl ServerHandler for AppServer {
    fn get_info(&self) -> ServerConfig {
        // SPEC §2.1: ONLY 2 tools; 0 prompts, 0 resources/templates, 0 completions.
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(server_implementation())
            .with_instructions(instructions())
    }

    async fn discover(
        &self,
        _context: RequestContext<RoleServer>,
    ) -> Result<DiscoverResult, McpError> {
        Ok(DiscoverResult::from_server_info(
            self.supported_protocol_versions().into_owned(),
            self.get_info(),
        )
        .with_ttl_ms(STATIC_LIST_TTL_MS)
        .with_cache_scope(CacheScope::Public))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let ver = context.protocol_version();
        let tools: Vec<Tool> = fsm_surface::tool_defs()
            .iter()
            .map(|def| to_rmcp_tool(def, ver.as_ref()))
            .collect();
        let mut res = ListToolsResult::with_all_items(tools);
        if is_modern(ver.as_ref()) {
            res = res
                .with_ttl_ms(STATIC_LIST_TTL_MS)
                .with_cache_scope(CacheScope::Public);
        }
        stamp_server_info(&mut res.meta, ver.as_ref());
        Ok(res)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let ver = context.protocol_version();
        let name = request.name.as_ref();
        if !matches!(name, "search" | "exec" | "ask") {
            return Err(McpError::invalid_params(
                format!("tool not found: {name}"),
                None,
            ));
        }

        if let Some(notice) = self.check_tool_budget() {
            let mut r = CallToolResult::success(vec![ContentBlock::text(notice)]);
            stamp_server_info(&mut r.meta, ver.as_ref());
            return Ok(CallToolResponse::from(r));
        }

        let session_id = self.resolve_session_id(&context);
        let raw_args = Value::Object(request.arguments.unwrap_or_default());

        let mut result = match name {
            "search" => {
                let extra = if self.cfg.agent_in_search
                    && !self.cfg.agent_backend.eq_ignore_ascii_case("off")
                {
                    self.cfg.agent_search_timeout_seconds.clamp(1.0, 60.0)
                } else {
                    0.0
                };
                let deadline = Instant::now()
                    + Duration::from_secs_f64(self.cfg.search_deadline_s.clamp(1.0, 30.0) + extra)
                    + Duration::from_secs(2);
                let mut cx = CallCx::new(session_id, deadline);
                cx.cancel = context.ct.child_token();
                self.run_search_tool(raw_args, &cx).await
            }
            "exec" => {
                let progress = context
                    .meta
                    .get_progress_token()
                    .map(|tok| (context.peer.clone(), tok));
                self.run_exec_tool(raw_args, session_id, context.ct.child_token(), progress)
                    .await
            }
            "ask" => {
                let secs = if !self.cfg.agent_backend.eq_ignore_ascii_case("off") {
                    self.cfg.agent_timeout_seconds.clamp(10.0, 180.0) + 5.0
                } else {
                    10.0
                };
                let deadline = Instant::now() + Duration::from_secs_f64(secs);
                let mut cx = CallCx::new(session_id, deadline);
                cx.cancel = context.ct.child_token();
                self.run_ask_tool(raw_args, &cx).await
            }
            _ => unreachable!(),
        };

        // SPEC §2.1: tools MUST NOT emit structuredContent.
        result.structured_content = None;
        stamp_server_info(&mut result.meta, ver.as_ref());
        Ok(CallToolResponse::from(result))
    }
}
