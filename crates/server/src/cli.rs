//! CLI parser and subcommands (`--transport/--host/--port/--path`, `config`, `doctor`,
//! `probe`, `sources`, `eval-replay`, hidden `__pdf-worker`) (`SPEC.md` §9.1, §12).

use crate::assembly::{
    PREWARM_TARGETS, assemble_live_for_transport, assemble_services, build_registry,
};
use crate::http::serve_http;
use crate::mcp::{AppServer, AskToolInput, TransportKind};
use clap::{Args, Parser, Subcommand, ValueEnum};
use fsm_agent::login as agent_login;
use fsm_agent::oauth::OAuthManager;
use fsm_catalog::CatalogService;
use fsm_core::{
    AskMode, Availability, CallCx, CatalogQuery, Config, FetchOpts, FetchOutcome, Freshness,
    FsmError, NetClient, NullStore, Registry, Source, SourceDeps, SrcCx, SrcOutput, StoreApi,
};
use fsm_net::{HttpClient, default_lookup, ssrf::CANARY_HOSTS};
use fsm_search::eval_replay;
use rmcp::ServiceExt;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Exit code for `doctor --require-fake-ip` when fake-IP is absent on the host (M10).
pub const EXIT_INCONCLUSIVE: u8 = 3;

/// Transport selector for `--transport`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CliTransport {
    Stdio,
    #[value(name = "streamable-http")]
    StreamableHttp,
}

impl CliTransport {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::StreamableHttp => "streamable-http",
        }
    }
}

/// Top-level CLI for `free-search-mcp` (and `search-mcp` alias).
#[derive(Debug, Parser)]
#[command(
    name = "free-search-mcp",
    version = env!("CARGO_PKG_VERSION"),
    about = "Local-first, keyless multi-engine search & exec MCP server."
)]
pub struct Cli {
    /// Transport to serve (overrides SEARCH_MCP_TRANSPORT; default: stdio).
    #[arg(long, value_enum)]
    pub transport: Option<CliTransport>,

    /// Streamable HTTP bind host (overrides SEARCH_MCP_HTTP_HOST; default: 127.0.0.1).
    #[arg(long)]
    pub host: Option<String>,

    /// Streamable HTTP bind port (overrides SEARCH_MCP_HTTP_PORT; default: 8000; 0 picks a free port).
    #[arg(long)]
    pub port: Option<u16>,

    /// Streamable HTTP endpoint path (overrides SEARCH_MCP_HTTP_PATH; default: /mcp).
    #[arg(long)]
    pub path: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Quick factual lookup printing the exact same text as the `ask` MCP tool.
    Ask(AskArgs),
    /// Sign in, check status, or sign out of OAuth-backed agent engines (`codex`, `antigravity`).
    Login(LoginArgs),
    /// Print effective configuration (secrets redacted) and any deprecation notices.
    Config(ConfigArgs),
    /// Check local store, registry, DNS fake-IP canary, and optional live source probe / fetch.
    Doctor(DoctorArgs),
    /// Probe one or more cataloged sources live and report results/latency.
    Probe(ProbeArgs),
    /// List, search, or describe the zero-network source, protocol, and host-API catalog.
    Sources(SourcesArgs),
    /// Replay offline ranking fixtures and verify hit@1, hit@3, MRR, and decoy guard against baseline.
    #[command(name = "eval-replay")]
    EvalReplay(EvalReplayArgs),
    /// Internal isolated PDF worker (never invoke directly).
    #[command(name = "__pdf-worker", hide = true)]
    PdfWorker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CliAskMode {
    Auto,
    Fast,
    Agent,
}

impl From<CliAskMode> for AskMode {
    fn from(m: CliAskMode) -> Self {
        match m {
            CliAskMode::Auto => AskMode::Auto,
            CliAskMode::Fast => AskMode::Fast,
            CliAskMode::Agent => AskMode::Agent,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CliFreshness {
    Day,
    Week,
    Month,
    Year,
}

impl From<CliFreshness> for Freshness {
    fn from(f: CliFreshness) -> Self {
        match f {
            CliFreshness::Day => Freshness::Day,
            CliFreshness::Week => Freshness::Week,
            CliFreshness::Month => Freshness::Month,
            CliFreshness::Year => Freshness::Year,
        }
    }
}

#[derive(Debug, Args)]
pub struct AskArgs {
    /// Question to answer.
    pub question: String,

    /// Mode: auto (agent when configured, else fast), fast (no LLM), or agent (LLM).
    #[arg(long, value_enum, default_value_t = CliAskMode::Auto)]
    pub mode: CliAskMode,

    /// Optional freshness filter (`day`, `week`, `month`, `year`).
    #[arg(long, value_enum)]
    pub freshness: Option<CliFreshness>,

    /// Optional BCP-47 locale hint (e.g. `zh`, `en`, `ja`).
    #[arg(long)]
    pub lang: Option<String>,

    /// Restrict or exclude domains (repeatable, e.g. `--domain docs.rs --domain -spam.example`).
    #[arg(long = "domain", value_name = "DOMAIN")]
    pub domains: Vec<String>,

    /// Maximum output tokens (200..=8000, default 1500).
    #[arg(long)]
    pub max_tokens: Option<i64>,

    /// Emit machine-readable JSON instead of Markdown.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct LoginArgs {
    #[command(subcommand)]
    pub action: LoginAction,
}

#[derive(Debug, Subcommand)]
pub enum LoginAction {
    /// Sign in to ChatGPT / Codex via OAuth PKCE or link an existing Codex CLI auth.json read-only.
    Codex {
        /// Print the sign-in URL and accept a pasted callback URL instead of opening a browser.
        #[arg(long = "no-browser")]
        no_browser: bool,
        /// Link read-only to an existing Codex CLI sign-in (`$CODEX_HOME/auth.json` or explicit path).
        #[arg(long = "use-codex-cli", num_args = 0..=1, default_missing_value = "")]
        use_codex_cli: Option<String>,
    },
    /// Sign in to Google Antigravity (opt-in; displays account-suspension warning first).
    Antigravity {
        /// Print the sign-in URL and accept a pasted callback URL instead of opening a browser.
        #[arg(long = "no-browser")]
        no_browser: bool,
    },
    /// Show sign-in status for OAuth-backed agent engines (tokens are never shown).
    Status,
    /// Remove the stored OAuth credential for a provider (`codex` or `antigravity`).
    Logout {
        /// Provider name (`codex` or `antigravity`).
        provider: String,
    },
}

#[derive(Debug, Args)]
pub struct ConfigArgs {
    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Live-probe one or more source IDs (e.g. `--probe duckduckgo`).
    #[arg(long = "probe", value_name = "SOURCE_ID")]
    pub probes: Vec<String>,

    /// Live-fetch a URL through the full SSRF-guarded fetch stack (e.g. `--fetch https://example.com`).
    #[arg(long = "fetch", value_name = "URL")]
    pub fetch: Option<String>,

    /// Require fake-IP TUN detection (canary in 198.18.0.0/15); exits 3 ("inconclusive") if absent (M10).
    #[arg(long = "require-fake-ip")]
    pub require_fake_ip: bool,

    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct ProbeArgs {
    /// Explicit source IDs to probe (e.g. `duckduckgo arxiv`).
    pub sources: Vec<String>,

    /// Probe all sources in a scope (e.g. `academic`, `security`, `web`).
    #[arg(long)]
    pub scope: Option<String>,

    /// Override query string used for search probes.
    #[arg(long, default_value = "rust programming language")]
    pub query: String,

    /// Result limit passed to search probes.
    #[arg(long, default_value_t = 3)]
    pub limit: u32,

    /// Skip sources that require an API key (`NeedsKey`).
    #[arg(long)]
    pub keyless_only: bool,

    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct SourcesArgs {
    /// Optional search query over the catalog (empty lists top entries).
    pub query: Option<String>,

    /// Filter by scope/group (e.g. `academic`, `security`, `api`).
    #[arg(long)]
    pub group: Option<String>,

    /// Filter by capability (e.g. `search`, `graph`, `grant`).
    #[arg(long)]
    pub capability: Option<String>,

    /// Show only keyless sources.
    #[arg(long)]
    pub keyless_only: bool,

    /// Describe a single source, protocol, or host API in full detail.
    #[arg(long)]
    pub describe: Option<String>,

    /// Maximum entries returned (default 25).
    #[arg(long, default_value_t = 25)]
    pub limit: u32,

    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct EvalReplayArgs {
    /// Path(s) or glob(s) to ranking fixture JSON file(s).
    #[arg(long = "data", num_args = 1.., value_name = "PATH")]
    pub data: Vec<String>,

    /// Print the report without enforcing the baseline regression gate.
    #[arg(long)]
    pub no_check: bool,
}

/// Effective server transport settings after applying CLI overrides on top of [`Config`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTransport {
    pub transport: CliTransport,
    pub host: String,
    pub port: u16,
    pub path: String,
}

/// Resolve transport settings: CLI arguments beat env (`Config`), which beats built-in defaults.
///
/// Crucially, `port = Some(0)` is honored verbatim (requests an OS-assigned ephemeral port).
pub fn resolve_transport(cli: &Cli, cfg: &Config) -> Result<ResolvedTransport, FsmError> {
    let transport = match cli.transport {
        Some(t) => t,
        None => match cfg.transport.trim().to_ascii_lowercase().as_str() {
            "" | "stdio" => CliTransport::Stdio,
            "streamable-http" | "http" => CliTransport::StreamableHttp,
            other => {
                return Err(FsmError::invalid(format!(
                    "unknown SEARCH_MCP_TRANSPORT {other:?}; expected \"stdio\" or \"streamable-http\""
                )));
            }
        },
    };
    let host = cli
        .host
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| cfg.http_host.clone());
    let port = cli.port.unwrap_or(cfg.http_port);
    let path = cli
        .path
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| cfg.http_path.clone());
    Ok(ResolvedTransport {
        transport,
        host,
        port,
        path,
    })
}

/// Initialize stderr-only structured logging (`stdout` is reserved for MCP JSON-RPC).
pub fn init_logging(cfg: &Config) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        let level = match cfg.log_level.trim().to_ascii_uppercase().as_str() {
            "TRACE" => "trace",
            "DEBUG" => "debug",
            "WARN" | "WARNING" => "warn",
            "ERROR" | "CRITICAL" => "error",
            _ => "info",
        };
        tracing_subscriber::EnvFilter::new(format!("fsm_server={level},fsm_net=warn,{level}"))
    });
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

/// Print config deprecation / rename / security notices once to `stderr`.
pub fn emit_config_notices(cfg: &Config) {
    for note in &cfg.deprecation_warnings {
        eprintln!("free-search-mcp: notice: {note}");
    }
}

/// Main async entry point invoked by `src/main.rs` (after `__pdf-worker` short-circuit).
pub async fn run_cli(cli: Cli) -> ExitCode {
    if matches!(cli.command, Some(Command::PdfWorker)) {
        return fsm_docs::DocService::run_pdf_worker_cli();
    }

    let cfg = match Config::load() {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("free-search-mcp: configuration error: {e}");
            return ExitCode::from(2);
        }
    };

    init_logging(&cfg);
    emit_config_notices(&cfg);

    match cli.command {
        None => run_server(&cli, cfg).await,
        Some(Command::Ask(args)) => run_ask(cfg, &args).await,
        Some(Command::Login(args)) => run_login(cfg, &args).await,
        Some(Command::Config(args)) => run_config(&cfg, &args),
        Some(Command::Doctor(args)) => run_doctor(cfg, &args).await,
        Some(Command::Probe(args)) => run_probe(cfg, &args).await,
        Some(Command::Sources(args)) => run_sources(cfg, &args).await,
        Some(Command::EvalReplay(args)) => run_eval_replay(&args),
        Some(Command::PdfWorker) => fsm_docs::DocService::run_pdf_worker_cli(),
    }
}

#[allow(clippy::print_stdout)]
async fn run_ask(cfg: Arc<Config>, args: &AskArgs) -> ExitCode {
    let runtime = match assemble_live_for_transport(Arc::clone(&cfg), false) {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("ask: startup failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let app = match AppServer::new(runtime.services, TransportKind::Stdio) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("ask: failed to initialize handler: {e}");
            let _ = runtime.store.close().await;
            return ExitCode::FAILURE;
        }
    };

    let input = AskToolInput {
        question: args.question.clone(),
        freshness: args.freshness.map(Into::into),
        lang: args.lang.clone(),
        domains: (!args.domains.is_empty()).then(|| args.domains.clone()),
        mode: Some(args.mode.into()),
        max_tokens: args.max_tokens,
    };
    let secs = if args.mode == CliAskMode::Fast {
        10.0
    } else if !cfg.agent_backend.eq_ignore_ascii_case("off") {
        cfg.agent_timeout_seconds.clamp(10.0, 180.0) + 5.0
    } else {
        10.0
    };
    let cx = CallCx::new(
        uuid::Uuid::new_v4().to_string(),
        Instant::now() + Duration::from_secs_f64(secs),
    );
    let code = match app.execute_ask(input, &cx).await {
        Ok((resp, markdown)) => {
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&resp).unwrap_or_default()
                );
            } else {
                print!("{markdown}");
                if !markdown.ends_with('\n') {
                    println!();
                }
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("{}", fsm_surface::render_error(&err));
            ExitCode::FAILURE
        }
    };
    let _ = runtime.store.close().await;
    code
}

#[allow(clippy::print_stdout)]
async fn run_login(cfg: Arc<Config>, args: &LoginArgs) -> ExitCode {
    let mgr = Arc::new(OAuthManager::from_config(&cfg));
    let out = match &args.action {
        LoginAction::Status => agent_login::run_status(&mgr),
        LoginAction::Logout { provider } => agent_login::run_logout(&mgr, provider),
        LoginAction::Codex {
            no_browser,
            use_codex_cli,
        } => {
            if let Some(raw_path) = use_codex_cli {
                let p = (!raw_path.trim().is_empty()).then(|| PathBuf::from(raw_path));
                agent_login::run_link_codex_cli(&mgr, p.as_deref())
            } else {
                agent_login::run_login(&mgr, "codex", *no_browser, 180.0, |s| {
                    print!("{s}");
                })
                .await
            }
        }
        LoginAction::Antigravity { no_browser } => {
            agent_login::run_login(&mgr, "antigravity", *no_browser, 180.0, |s| {
                print!("{s}");
            })
            .await
        }
    };
    // Status, logout, and --use-codex-cli don't stream via callback; print their stdout here.
    let streamed = matches!(
        &args.action,
        LoginAction::Antigravity { .. }
            | LoginAction::Codex {
                use_codex_cli: None,
                ..
            }
    );
    if !streamed && !out.stdout.is_empty() {
        print!("{}", out.stdout);
    }
    if !out.stderr.is_empty() {
        eprint!("{}", out.stderr);
    }
    if out.exit_code == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(out.exit_code.clamp(1, 255) as u8)
    }
}

async fn run_server(cli: &Cli, cfg: Arc<Config>) -> ExitCode {
    let resolved = match resolve_transport(cli, &cfg) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("free-search-mcp: {e}");
            return ExitCode::from(2);
        }
    };

    if cfg.tool_call_budget > 0 && resolved.transport == CliTransport::StreamableHttp {
        tracing::warn!(
            budget = cfg.tool_call_budget,
            "SEARCH_MCP_TOOL_CALL_BUDGET on a long-lived HTTP server applies until process restart"
        );
    }

    match resolved.transport {
        CliTransport::Stdio => {
            // Defer store open, HTTP client/TLS CA load, 535-manifest registry assembly,
            // BM25 catalog indexing, download purge, and QuickJS init onto a background
            // thread so `initialize`, `server/discover`, and `tools/list` reply immediately.
            let app = AppServer::new_lazy(Arc::clone(&cfg), TransportKind::Stdio);
            let shutdown_app = app.clone();
            let code = match app.serve(rmcp::transport::stdio()).await {
                Ok(service) => match service.waiting().await {
                    Ok(_) => ExitCode::SUCCESS,
                    Err(e) => {
                        tracing::error!(error = %e, "stdio MCP service terminated with error");
                        ExitCode::FAILURE
                    }
                },
                Err(e) => {
                    eprintln!("free-search-mcp: stdio transport error: {e}");
                    ExitCode::FAILURE
                }
            };
            shutdown_app.shutdown().await;
            code
        }
        CliTransport::StreamableHttp => {
            let _ = fsm_store::downloads::purge_expired(&cfg, None);
            let runtime = match assemble_live_for_transport(Arc::clone(&cfg), true) {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("free-search-mcp: startup failed: {e}");
                    return ExitCode::FAILURE;
                }
            };
            if cfg.prewarm {
                let http = Arc::clone(&runtime.http);
                tokio::spawn(async move {
                    let _ = http.prewarm(PREWARM_TARGETS).await;
                });
            }
            let app = match AppServer::new(runtime.services, TransportKind::StreamableHttp) {
                Ok(a) => a,
                Err(e) => {
                    eprintln!("free-search-mcp: failed to initialize MCP handler: {e}");
                    let _ = runtime.store.close().await;
                    return ExitCode::FAILURE;
                }
            };
            let cancel = CancellationToken::new();
            let cancel_sig = cancel.clone();
            tokio::spawn(async move {
                wait_for_shutdown_signal().await;
                cancel_sig.cancel();
            });
            let code = match serve_http(
                app,
                &resolved.host,
                resolved.port,
                &resolved.path,
                &cfg,
                cancel,
            )
            .await
            {
                Ok(_) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("free-search-mcp: {e}");
                    ExitCode::FAILURE
                }
            };
            let _ = runtime.store.close().await;
            code
        }
    }
}

async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm = signal(SignalKind::terminate()).ok();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = async {
                if let Some(ref mut s) = sigterm {
                    let _ = s.recv().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Redacted JSON snapshot of [`Config`] for `free-search-mcp config`.
pub fn config_summary_json(cfg: &Config) -> Value {
    let mut configured_keys: Vec<&str> = cfg.api_keys.keys().map(String::as_str).collect();
    configured_keys.sort_unstable();
    let mut v = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "config_dir": cfg.config_dir,
        "cache_dir": cfg.cache_dir,
        "cache_ttl_s": cfg.cache_ttl_s,
        "cache_max_mb": cfg.cache_max_mb,
        "transport": cfg.transport,
        "http_host": cfg.http_host,
        "http_port": cfg.http_port,
        "http_path": cfg.http_path,
        "http_allowed_origins": cfg.http_allowed_origins,
        "http_token_set": cfg.http_token.is_some(),
        "tool_call_budget": cfg.tool_call_budget,
        "default_engines": cfg.default_engines,
        "reserve_engines": cfg.reserve_engines,
        "fresh_engines": cfg.fresh_engines,
        "locale_engines": cfg.locale_engines,
        "search_deadline_s": cfg.search_deadline_s,
        "ssrf_resolve_addresses": cfg.ssrf_resolve_addresses.as_str(),
        "allow_private_hosts": cfg.allow_private_hosts,
        "proxy_set": cfg.proxy.is_some(),
        "proxy_engines": cfg.proxy_engines,
        "no_proxy": cfg.no_proxy,
        "document_root": cfg.document_root,
        "download_enabled": cfg.download_enabled,
        "download_dir": cfg.download_dir,
        "download_ttl_hours": cfg.download_ttl_hours,
        "download_max_mb": cfg.download_max_mb,
        "download_dir_max_mb": cfg.download_dir_max_mb,
        "exec_timeout_s": cfg.exec_timeout_s,
        "exec_max_host_calls": cfg.exec_max_host_calls,
        "exec_heap_mb": cfg.exec_heap_mb,
        "exec_raw_http": cfg.exec_raw_http.as_str(),
        "exec_auth_hosts": cfg.exec_auth_hosts,
        "offline_sync": cfg.offline_sync,
        "contact_email_set": cfg.contact_email.is_some(),
        "configured_api_keys": configured_keys,
        "notices": cfg.deprecation_warnings,
    });
    if let Some(obj) = v.as_object_mut() {
        obj.insert("agent_backend".to_owned(), json!(cfg.agent_backend));
        obj.insert("agent_model".to_owned(), json!(cfg.agent_model));
        obj.insert(
            "agent_api_key_set".to_owned(),
            json!(cfg.agent_api_key.is_some()),
        );
        obj.insert("agent_in_search".to_owned(), json!(cfg.agent_in_search));
        obj.insert("agent_engines".to_owned(), json!(cfg.agent_engines));
        obj.insert(
            "agent_lane_wait_seconds".to_owned(),
            json!(cfg.agent_lane_wait_seconds),
        );
        obj.insert(
            "agent_lane_grace_seconds".to_owned(),
            json!(cfg.agent_lane_grace_seconds),
        );
        obj.insert("codex_auto_signin".to_owned(), json!(cfg.codex_auto_signin));
        obj.insert(
            "antigravity_client_secret_set".to_owned(),
            json!(cfg.antigravity_client_secret.is_some()),
        );
    }
    v
}

#[allow(clippy::print_stdout)]
fn run_config(cfg: &Config, args: &ConfigArgs) -> ExitCode {
    let summary = config_summary_json(cfg);
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&summary).unwrap_or_default()
        );
        return ExitCode::SUCCESS;
    }

    println!("free-search-mcp {}", env!("CARGO_PKG_VERSION"));
    println!("config_dir:            {}", cfg.config_dir.display());
    println!("cache_dir:             {}", cfg.cache_dir.display());
    println!(
        "transport:             {} ({}:{}{})",
        cfg.transport, cfg.http_host, cfg.http_port, cfg.http_path
    );
    println!(
        "http_token:            {}",
        if cfg.http_token.is_some() {
            "<set>"
        } else {
            "<unset>"
        }
    );
    println!("default_engines:       {}", cfg.default_engines.join(", "));
    println!("reserve_engines:       {}", cfg.reserve_engines.join(", "));
    println!("search_deadline_s:     {}", cfg.search_deadline_s);
    println!(
        "ssrf_resolve:          {} (allow_private_hosts={})",
        cfg.ssrf_resolve_addresses.as_str(),
        cfg.allow_private_hosts
    );
    println!(
        "proxy:                 {}",
        if cfg.proxy.is_some() {
            "<set>"
        } else {
            "<unset>"
        }
    );
    println!(
        "document_root:         {}",
        cfg.document_root
            .as_ref()
            .map_or_else(|| "<disabled>".to_owned(), |p| p.display().to_string())
    );
    println!(
        "downloads:             enabled={} dir={} ttl={}h max={}MB dir_max={}MB",
        cfg.download_enabled,
        cfg.download_dir.display(),
        cfg.download_ttl_hours,
        cfg.download_max_mb,
        cfg.download_dir_max_mb
    );
    println!(
        "exec:                  timeout={}s max_calls={} heap={}MB raw_http={}",
        cfg.exec_timeout_s,
        cfg.exec_max_host_calls,
        cfg.exec_heap_mb,
        cfg.exec_raw_http.as_str()
    );
    let mut keys: Vec<&str> = cfg.api_keys.keys().map(String::as_str).collect();
    keys.sort_unstable();
    println!(
        "configured_api_keys:   {}",
        if keys.is_empty() {
            "<none (keyless mode)>".to_owned()
        } else {
            keys.join(", ")
        }
    );
    if !cfg.deprecation_warnings.is_empty() {
        println!("notices:");
        for w in &cfg.deprecation_warnings {
            println!("  - {w}");
        }
    }
    ExitCode::SUCCESS
}

/// True iff `ip` is an IPv4 address in the RFC 2544 / Clash TUN fake-IP range `198.18.0.0/15`.
pub fn is_ipv4_198_18(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, _, _] = v4.octets();
            a == 198 && (b == 18 || b == 19)
        }
        IpAddr::V6(_) => false,
    }
}

#[allow(clippy::print_stdout)]
async fn run_doctor(cfg: Arc<Config>, args: &DoctorArgs) -> ExitCode {
    let http = match HttpClient::new(&cfg) {
        Ok(h) => Arc::new(h),
        Err(e) => {
            eprintln!("doctor: failed to construct HTTP client: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Inspect canary DNS resolution directly for M10 reporting.
    let dns = default_lookup();
    let mut canary_ips: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut any_in_198_18 = false;
    for host in CANARY_HOSTS {
        match dns.lookup(host).await {
            Ok(ips) => {
                if ips.iter().any(|ip| is_ipv4_198_18(*ip)) {
                    any_in_198_18 = true;
                }
                canary_ips.insert(
                    host.to_owned(),
                    ips.into_iter().map(|ip| ip.to_string()).collect(),
                );
            }
            Err(e) => {
                canary_ips.insert(host.to_owned(), vec![format!("error: {e}")]);
            }
        }
    }
    let verdict = http.fake_ip_verdict().await;
    let fake_ip_present = verdict == Some(true) && any_in_198_18;

    // Build services (NullStore keeps doctor side-effect-free on the user's SQLite cache).
    let net: Arc<dyn NetClient> = Arc::clone(&http) as Arc<dyn NetClient>;
    let store: Arc<dyn StoreApi> = Arc::new(NullStore::new());
    let services = match assemble_services(Arc::clone(&cfg), net, store) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("doctor: failed to assemble services: {e}");
            return ExitCode::FAILURE;
        }
    };

    // M10: when --require-fake-ip is set and fake-IP is absent, exit 3 ("inconclusive").
    if args.require_fake_ip && !fake_ip_present {
        let report = json!({
            "status": "inconclusive",
            "reason": "fake-IP (198.18.0.0/15) absent on this host",
            "fake_ip_verdict": verdict,
            "canary_in_198_18": any_in_198_18,
            "canaries": canary_ips,
            "sources_registered": services.registry.len(),
            "protocols_registered": services.registry.protocols().count(),
        });
        if args.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&report).unwrap_or_default()
            );
        } else {
            println!("doctor: inconclusive (fake-IP 198.18.0.0/15 absent on this host)");
            println!("  canaries: {canary_ips:?}");
            println!("  fake_ip_verdict: {verdict:?}");
        }
        return ExitCode::from(EXIT_INCONCLUSIVE);
    }

    let mut all_ok = true;
    let mut probe_results = Vec::new();
    for id in &args.probes {
        let outcome =
            probe_one_source(&services.registry, &cfg, id, "rust programming language", 3).await;
        if !outcome.ok {
            all_ok = false;
        }
        probe_results.push(outcome);
    }

    let mut fetch_report: Option<Value> = None;
    if let Some(url) = &args.fetch {
        let started = Instant::now();
        let cx = CallCx::new("doctor", Instant::now() + Duration::from_secs(20));
        match services.fetch.fetch(url, FetchOpts::default(), &cx).await {
            Ok(FetchOutcome::Page(page)) => {
                let ok = !page.content.trim().is_empty();
                if !ok {
                    all_ok = false;
                }
                fetch_report = Some(json!({
                    "url": page.url,
                    "ok": ok,
                    "title": page.title,
                    "chars": page.content.chars().count(),
                    "elapsed_ms": started.elapsed().as_millis() as u64,
                }));
            }
            Ok(FetchOutcome::InlineImage { meta, .. }) => {
                fetch_report = Some(json!({
                    "url": meta.url,
                    "ok": true,
                    "media_type": meta.media_type,
                    "bytes": meta.bytes,
                    "elapsed_ms": started.elapsed().as_millis() as u64,
                }));
            }
            Ok(_) => {
                all_ok = false;
                fetch_report =
                    Some(json!({"url": url, "ok": false, "error": "unexpected outcome"}));
            }
            Err(e) => {
                all_ok = false;
                fetch_report = Some(json!({
                    "url": url,
                    "ok": false,
                    "error": e.to_string(),
                    "elapsed_ms": started.elapsed().as_millis() as u64,
                }));
            }
        }
    }

    let status = if all_ok { "ok" } else { "failed" };
    let report = json!({
        "status": status,
        "version": env!("CARGO_PKG_VERSION"),
        "sources_registered": services.registry.len(),
        "protocols_registered": services.registry.protocols().count(),
        "host_api_count": fsm_surface::HOST_API.len(),
        "ssrf_resolve_mode": cfg.ssrf_resolve_addresses.as_str(),
        "fake_ip_verdict": verdict,
        "canary_in_198_18": any_in_198_18,
        "canaries": canary_ips,
        "probes": probe_results.iter().map(ProbeResult::to_json).collect::<Vec<_>>(),
        "fetch": fetch_report,
    });

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
    } else {
        println!(
            "doctor: {status} (sources={}, protocols={}, host_api={})",
            services.registry.len(),
            services.registry.protocols().count(),
            fsm_surface::HOST_API.len()
        );
        println!(
            "  dns canary: fake_ip_verdict={verdict:?} in_198_18={any_in_198_18} ips={canary_ips:?}"
        );
        for p in &probe_results {
            if p.ok {
                println!(
                    "  probe {}: OK (op={} items={} {}ms)",
                    p.id, p.op, p.items, p.elapsed_ms
                );
            } else {
                println!(
                    "  probe {}: FAIL (op={} {}ms) {}",
                    p.id,
                    p.op,
                    p.elapsed_ms,
                    p.error.as_deref().unwrap_or("")
                );
            }
        }
        if let Some(fr) = &fetch_report {
            let ok = fr.get("ok").and_then(Value::as_bool).unwrap_or(false);
            let url = fr.get("url").and_then(Value::as_str).unwrap_or("");
            if ok {
                println!(
                    "  fetch {url}: OK (title={:?} chars={} {}ms)",
                    fr.get("title").and_then(Value::as_str).unwrap_or(""),
                    fr.get("chars").and_then(Value::as_u64).unwrap_or(0),
                    fr.get("elapsed_ms").and_then(Value::as_u64).unwrap_or(0)
                );
            } else {
                println!(
                    "  fetch {url}: FAIL {}",
                    fr.get("error").and_then(Value::as_str).unwrap_or("")
                );
            }
        }
    }

    if all_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

struct ProbeResult {
    id: String,
    op: String,
    ok: bool,
    skipped_needs_key: bool,
    items: usize,
    elapsed_ms: u64,
    error: Option<String>,
}

impl ProbeResult {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "op": self.op,
            "ok": self.ok,
            "skipped_needs_key": self.skipped_needs_key,
            "items": self.items,
            "elapsed_ms": self.elapsed_ms,
            "error": self.error,
        })
    }
}

fn count_src_output(out: &SrcOutput) -> usize {
    match out {
        SrcOutput::Hits(h) => h.len(),
        SrcOutput::Works(w) => w.len(),
        SrcOutput::Records { items, .. } => items.len(),
        SrcOutput::Text(t) => usize::from(!t.trim().is_empty()),
        SrcOutput::Raw(v) => usize::from(!v.is_null()),
        _ => 0,
    }
}

async fn probe_one_source(
    reg: &Registry,
    cfg: &Config,
    id: &str,
    default_query: &str,
    default_limit: u32,
) -> ProbeResult {
    let started = Instant::now();
    let Some(src) = reg.get(id) else {
        return ProbeResult {
            id: id.to_owned(),
            op: "search".to_owned(),
            ok: false,
            skipped_needs_key: false,
            items: 0,
            elapsed_ms: 0,
            error: Some(format!("unknown source {id:?}")),
        };
    };

    if let Availability::NeedsKey { env } = src.availability(cfg) {
        return ProbeResult {
            id: id.to_owned(),
            op: "search".to_owned(),
            ok: false,
            skipped_needs_key: true,
            items: 0,
            elapsed_ms: 0,
            error: Some(format!("needs key ({env})")),
        };
    }

    let decl = src.decl();
    let (op, params, min_results) =
        if let Some(op_decl) = decl.ops.iter().find(|o| o.probe.is_some()) {
            let p = op_decl.probe.as_ref().expect("checked");
            (op_decl.name.clone(), p.params.clone(), p.min_results.max(1))
        } else {
            (
                "search".to_owned(),
                json!({"query": default_query, "limit": default_limit}),
                1usize,
            )
        };

    let timeout = Duration::from_millis(decl.limits.timeout_ms.clamp(3_000, 15_000));
    let cx = SrcCx::from_config(cfg, CancellationToken::new(), timeout);
    match src.call(&op, params, &cx).await {
        Ok(out) => {
            let items = count_src_output(&out);
            let ok = items >= min_results;
            ProbeResult {
                id: id.to_owned(),
                op,
                ok,
                skipped_needs_key: false,
                items,
                elapsed_ms: started.elapsed().as_millis() as u64,
                error: if ok {
                    None
                } else {
                    Some(format!("got {items} items, expected >= {min_results}"))
                },
            }
        }
        Err(e) => ProbeResult {
            id: id.to_owned(),
            op,
            ok: false,
            skipped_needs_key: false,
            items: 0,
            elapsed_ms: started.elapsed().as_millis() as u64,
            error: Some(e.to_string()),
        },
    }
}

#[allow(clippy::print_stdout)]
async fn run_probe(cfg: Arc<Config>, args: &ProbeArgs) -> ExitCode {
    let http = match HttpClient::new(&cfg) {
        Ok(h) => Arc::new(h),
        Err(e) => {
            eprintln!("probe: failed to build HTTP client: {e}");
            return ExitCode::FAILURE;
        }
    };
    let deps = SourceDeps::new(
        http as Arc<dyn NetClient>,
        Arc::new(NullStore::new()),
        Arc::clone(&cfg),
    );
    let reg = match build_registry(&deps) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("probe: failed to build registry: {e}");
            return ExitCode::FAILURE;
        }
    };

    let targets: Vec<String> = if !args.sources.is_empty() {
        args.sources.clone()
    } else if let Some(scope) = &args.scope {
        reg.by_scope(scope)
            .into_iter()
            .filter(|s| {
                !args.keyless_only || matches!(s.availability(&cfg), Availability::Available)
            })
            .map(|s| s.decl().id.clone())
            .collect()
    } else {
        cfg.default_engines.clone()
    };

    if targets.is_empty() {
        eprintln!("probe: no matching sources to probe");
        return ExitCode::FAILURE;
    }

    let mut results = Vec::with_capacity(targets.len());
    let mut failed = 0usize;
    let mut passed = 0usize;
    let mut skipped = 0usize;
    for id in &targets {
        let r = probe_one_source(&reg, &cfg, id, &args.query, args.limit).await;
        if r.skipped_needs_key && args.keyless_only {
            skipped += 1;
        } else if r.ok {
            passed += 1;
        } else {
            failed += 1;
        }
        results.push(r);
    }

    if args.json {
        let payload = json!({
            "passed": passed,
            "failed": failed,
            "skipped": skipped,
            "results": results.iter().map(ProbeResult::to_json).collect::<Vec<_>>(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );
    } else {
        for r in &results {
            let tag = if r.ok {
                "OK"
            } else if r.skipped_needs_key {
                "NEEDS_KEY"
            } else {
                "FAIL"
            };
            println!(
                "{tag:9} {:24} op={:10} items={:3} {:5}ms {}",
                r.id,
                r.op,
                r.items,
                r.elapsed_ms,
                r.error.as_deref().unwrap_or("")
            );
        }
        println!("summary: passed={passed} failed={failed} skipped={skipped}");
    }

    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[allow(clippy::print_stdout)]
async fn run_sources(cfg: Arc<Config>, args: &SourcesArgs) -> ExitCode {
    let store: Arc<dyn StoreApi> = Arc::new(NullStore::new());
    let net = match fsm_core::fixture::ReplayNet::from_fixtures(Vec::new()) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            eprintln!("sources: failed to initialize offline net: {e}");
            return ExitCode::FAILURE;
        }
    };
    let deps = SourceDeps::new(net, Arc::clone(&store), Arc::clone(&cfg));
    let reg = match build_registry(&deps) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            eprintln!("sources: failed to build registry: {e}");
            return ExitCode::FAILURE;
        }
    };
    let catalog = CatalogService::new(Arc::clone(&reg), fsm_surface::HOST_API, store);

    if let Some(id) = &args.describe {
        match catalog.describe(id).await {
            Ok(detail) => {
                if args.json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&detail).unwrap_or_default()
                    );
                } else {
                    println!(
                        "{} ({}) — {}",
                        detail.hit.id, detail.hit.kind, detail.hit.summary
                    );
                    println!("  auth:      {}", detail.hit.auth);
                    println!("  tier:      {}", detail.hit.tier);
                    println!("  scopes:    {}", detail.scopes.join(", "));
                    println!("  caps:      {}", detail.caps.join(", "));
                    println!("  signature: {}", detail.hit.signature);
                    println!("  example:   {}", detail.hit.example);
                    if let Some(url) = &detail.home_url {
                        println!("  home_url:  {url}");
                    }
                }
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                eprintln!("sources: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    let mut q = CatalogQuery::new(args.query.clone().unwrap_or_default());
    q.group = args.group.clone();
    q.capability = args.capability.clone();
    q.keyless_only = args.keyless_only;
    q.limit = args.limit.clamp(1, 500);

    match catalog.search(q).await {
        Ok(hits) => {
            if args.json {
                let payload = json!({
                    "total_sources": reg.len(),
                    "total_protocols": reg.protocols().count(),
                    "total_host_apis": fsm_surface::HOST_API.len(),
                    "returned": hits.len(),
                    "hits": hits,
                });
                println!(
                    "{}",
                    serde_json::to_string_pretty(&payload).unwrap_or_default()
                );
            } else {
                println!(
                    "catalog: {} sources, {} protocols, {} host APIs (showing {})",
                    reg.len(),
                    reg.protocols().count(),
                    fsm_surface::HOST_API.len(),
                    hits.len()
                );
                for h in &hits {
                    println!(
                        "  {:26} {:8} tier={} {:10} {}",
                        h.id, h.kind, h.tier, h.auth, h.summary
                    );
                }
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("sources: {e}");
            ExitCode::FAILURE
        }
    }
}

#[allow(clippy::print_stdout)]
fn run_eval_replay(args: &EvalReplayArgs) -> ExitCode {
    let inputs: Vec<String> = if args.data.is_empty() {
        vec!["evals/ranking/fixtures/decoy_and_dupes.json".to_owned()]
    } else {
        args.data.clone()
    };

    let mut files: Vec<PathBuf> = Vec::new();
    for pattern in &inputs {
        let p = PathBuf::from(pattern);
        if p.exists() {
            files.push(p);
            continue;
        }
        match glob::glob(pattern) {
            Ok(entries) => {
                let mut matched = false;
                for entry in entries.flatten() {
                    files.push(entry);
                    matched = true;
                }
                if !matched {
                    eprintln!("eval-replay: no files matched {pattern:?}");
                    return ExitCode::FAILURE;
                }
            }
            Err(e) => {
                eprintln!("eval-replay: invalid pattern {pattern:?}: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    let mut all_passed = true;
    for path in &files {
        let fixture = match eval_replay::load(path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("eval-replay: {}: {e}", path.display());
                return ExitCode::FAILURE;
            }
        };
        let report = eval_replay::replay(&fixture);
        println!("=== {} ===", path.display());
        print!("{}", report.render(Some(&eval_replay::PYTHON_BASELINE)));
        if !args.no_check
            && let Err(errs) = report.check(&eval_replay::PYTHON_BASELINE)
        {
            all_passed = false;
            for e in errs {
                eprintln!("eval-replay FAIL ({}): {e}", path.display());
            }
        }
    }

    if all_passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Expose `Registry::all` summary count helper for diagnostics.
pub fn source_counts(reg: &Registry) -> (usize, usize) {
    (reg.len(), reg.protocols().count())
}

/// Helper verifying whether `src` declares a `search` op or is keyless-available.
pub fn is_keyless_source(src: &Arc<dyn Source>, cfg: &Config) -> bool {
    matches!(src.availability(cfg), Availability::Available)
}
