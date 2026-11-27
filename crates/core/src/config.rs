//! Runtime configuration (`SPEC-contracts.md` §1.7, `SPEC.md` §9.3, §3.4 I12).
//!
//! Sources and precedence (first wins):
//!
//! | key class | process env | `./.env` (CWD) | `<config_dir>/.env` | `<config_dir>/config.json` |
//! |---|---|---|---|---|
//! | ordinary | 1 | 2 | 3 | – |
//! | security-sensitive ([`is_security_sensitive`]) | 1 | **ignored** (+ notice) | 2 | 3 (keys, `proxy`, `proxy_engines`) |
//!
//! Every value that is empty/whitespace or still contains an unresolved
//! `${...}` MCPB placeholder is treated as unset (`drop_unfilled`).
//! Loading is a pure function of [`EnvSources`], so tests inject sources
//! instead of mutating the process environment.

use crate::error::FsmError;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};

/// Prefix of every setting.
pub const ENV_PREFIX: &str = "SEARCH_MCP_";

/// `SEARCH_MCP_SSRF_RESOLVE_ADDRESSES`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum SsrfResolveMode {
    #[default]
    Auto,
    Always,
    Never,
}

impl SsrfResolveMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Always => "always",
            Self::Never => "never",
        }
    }
}

/// `SEARCH_MCP_EXEC_RAW_HTTP` kill switch for `web.request`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum RawHttpPolicy {
    #[default]
    GetPost,
    GetOnly,
    Off,
}

impl RawHttpPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::GetPost => "get-post",
            Self::GetOnly => "get-only",
            Self::Off => "off",
        }
    }
}

/// `SEARCH_MCP_SAFESEARCH`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum SafeSearch {
    Strict,
    #[default]
    Moderate,
    Off,
}

impl SafeSearch {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::Moderate => "moderate",
            Self::Off => "off",
        }
    }
}

/// Fully resolved configuration.
///
/// `Default` is the built-in default set (paths under `$HOME`), not zeros.
/// `Debug` redacts `http_token`, `proxy` and `api_keys` values.
#[derive(Clone)]
#[non_exhaustive]
pub struct Config {
    pub config_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub cache_ttl_s: u64,
    pub cache_max_mb: u64,
    pub default_engines: Vec<String>,
    pub reserve_engines: Vec<String>,
    pub min_healthy_engines: usize,
    pub fresh_engines: Vec<String>,
    pub locale_engines: HashMap<String, Vec<String>>,
    pub max_results_per_engine: u32,
    /// `SEARCH_MCP_SCOPE_ENGINE_LIMIT` (alias: `CATEGORY_ENGINE_LIMIT`).
    pub scope_engine_limit: usize,
    pub auto_route_enabled: bool,
    /// < 0 = unbounded.
    pub claim_engine_limit: isize,
    /// Hard ceiling 8.0 s.
    pub search_deadline_s: f64,
    pub coherence_guard_enabled: bool,
    /// `SEARCH_MCP_HEDGE_ENABLED` (alias: `RESCUE_ENABLED`).
    pub hedge_enabled: bool,
    pub health_enabled: bool,
    pub engine_cooldown_s: f64,
    pub engine_cooldown_max_s: f64,
    pub rate_limit_rpm: u32,
    pub fetch_rate_limit_rpm: u32,
    pub request_timeout_s: f64,
    pub fetch_timeout_s: f64,
    /// Chrome-profile UA override. Empty (default) keeps the wreq emulation
    /// profile's own UA so UA, `sec-ch-ua` and `Accept-Encoding` stay a
    /// consistent set (Spike N). The plain profile always sends its fixed honest UA.
    pub user_agent: String,
    pub accept_language: String,
    pub region: String,
    pub safesearch: SafeSearch,
    pub contact_email: Option<String>,
    pub prewarm: bool,
    pub max_content_chars: usize,
    /// Python-style level (`INFO`, `DEBUG`, ...); the server maps it to `RUST_LOG`.
    pub log_level: String,
    /// `stdio` | `streamable-http` (CLI flags override).
    pub transport: String,
    pub http_host: String,
    pub http_port: u16,
    pub http_path: String,
    pub http_allowed_origins: Vec<String>,
    pub http_token: Option<String>,
    /// 0 = unlimited.
    pub tool_call_budget: u32,
    /// `auto` | `http` | `browser` (browser inactive until P1).
    pub fetch_strategy: String,
    pub allow_private_hosts: bool,
    pub ssrf_resolve_addresses: SsrfResolveMode,
    pub proxy: Option<String>,
    pub proxy_engines: Vec<String>,
    /// Host suffixes that bypass the proxy.
    pub no_proxy: Vec<String>,
    pub document_root: Option<PathBuf>,
    pub download_enabled: bool,
    /// Resolved: `SEARCH_MCP_DOWNLOAD_DIR` or `<cache_dir>/downloads`.
    pub download_dir: PathBuf,
    pub download_ttl_hours: u32,
    pub download_max_mb: u64,
    pub download_dir_max_mb: u64,
    pub max_response_bytes: u64,
    pub max_pdf_pages: u32,
    pub max_document_chars: usize,
    /// Ceiling for the `exec` `timeout_s` argument (env may only lower 120).
    pub exec_timeout_s: u64,
    /// Per-exec host-call budget (default 100, max 300).
    pub exec_max_host_calls: u32,
    /// QuickJS heap (default 96, max 128).
    pub exec_heap_mb: usize,
    /// Resolved policy (downgraded to `GetOnly` when `DOCUMENT_ROOT` is set and
    /// `EXEC_RAW_HTTP` was not given explicitly).
    pub exec_raw_http: RawHttpPolicy,
    pub exec_auth_hosts: Vec<String>,
    pub offline_sync: bool,
    // ---- Answer agent (ruling R5) ----
    /// `off` | `api` | `claude-code` | `codex`.
    pub agent_backend: String,
    pub agent_model: String,
    /// `openai` | `anthropic`.
    pub agent_api_protocol: String,
    pub agent_api_base_url: String,
    pub agent_api_key: Option<String>,
    pub agent_command: String,
    pub agent_cli_args: String,
    pub agent_depth: u8,
    pub agent_read_seconds: f64,
    pub agent_max_steps: u32,
    pub agent_max_source_chars: usize,
    pub agent_timeout_seconds: f64,
    pub agent_in_search: bool,
    pub agent_search_timeout_seconds: f64,
    // ---- Agent lane (ruling R5) ----
    /// `auto` (= codex only) | `off` | comma/space-separated agent engine names.
    pub agent_engines: String,
    pub agent_lane_wait_seconds: f64,
    pub agent_lane_grace_seconds: f64,
    // ---- Codex engine (ruling R5) ----
    pub codex_timeout_s: f64,
    pub codex_model: String,
    pub codex_reasoning_effort: String,
    pub codex_base_url: String,
    pub codex_auto_signin: bool,
    pub codex_signin_wait_seconds: f64,
    pub codex_home: Option<PathBuf>,
    // ---- Antigravity engine (ruling R5) ----
    pub antigravity_timeout_s: f64,
    pub antigravity_model: String,
    pub antigravity_base_urls: Vec<String>,
    pub antigravity_version: String,
    pub antigravity_client_id: Option<String>,
    pub antigravity_client_secret: Option<String>,
    /// Opt-in instance endpoints, accepted only from trusted configuration sources.
    pub searxng_base_url: Option<String>,
    pub degoog_base_url: Option<String>,
    /// Every credential value ([`is_key_var`]), keyed by the full
    /// variable name (manifests' `auth.env`), excluding `HTTP_TOKEN`, agent/oauth secrets and dropped names.
    pub api_keys: HashMap<String, String>,
    /// One-time startup notices (deprecations, renames, drops, P1-inactive
    /// settings, ignored CWD `.env` keys, clamps). The server prints each
    /// once to stderr.
    pub deprecation_warnings: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        match system_home() {
            Some(home) => Self::defaults_for_home(&home),
            None => Self::defaults_without_home(),
        }
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let redact = |o: &Option<String>| o.as_ref().map(|_| "<redacted>");
        let keys: BTreeSet<&str> = self.api_keys.keys().map(String::as_str).collect();
        f.debug_struct("Config")
            .field("config_dir", &self.config_dir)
            .field("cache_dir", &self.cache_dir)
            .field("default_engines", &self.default_engines)
            .field("reserve_engines", &self.reserve_engines)
            .field("search_deadline_s", &self.search_deadline_s)
            .field("transport", &self.transport)
            .field("http_token", &redact(&self.http_token))
            .field("proxy", &redact(&self.proxy))
            .field("agent_backend", &self.agent_backend)
            .field("agent_api_key", &redact(&self.agent_api_key))
            .field("agent_engines", &self.agent_engines)
            .field(
                "antigravity_client_secret",
                &redact(&self.antigravity_client_secret),
            )
            .field("ssrf_resolve_addresses", &self.ssrf_resolve_addresses)
            .field("document_root", &self.document_root)
            .field("exec_raw_http", &self.exec_raw_http)
            .field("api_keys(names)", &keys)
            .field("deprecation_warnings", &self.deprecation_warnings)
            .finish_non_exhaustive()
    }
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_owned()).collect()
}

/// The user's home directory (`HOME`; `USERPROFILE` on Windows), or `None`
/// when it is unknown, empty or relative. Never falls back to the CWD: that
/// would turn `./.config/...` into a trusted config dir and defeat I12.
pub fn system_home() -> Option<PathBuf> {
    std::env::home_dir().filter(|p| p.is_absolute())
}

/// Cache/config base used when there is no home directory.
fn homeless_base() -> PathBuf {
    std::env::temp_dir().join("search-mcp")
}

impl Config {
    /// Built-in defaults for a given home directory.
    pub fn defaults_for_home(home: &Path) -> Self {
        let cache_dir = home.join(".cache").join("search-mcp");
        let mut locale_engines = HashMap::new();
        locale_engines.insert("zh".to_owned(), strings(&["so360", "quark", "baidu"]));
        locale_engines.insert("ko".to_owned(), strings(&["naver", "daum"]));
        Self {
            config_dir: home.join(".config").join("search-mcp"),
            download_dir: cache_dir.join("downloads"),
            cache_dir,
            cache_ttl_s: 604_800,
            cache_max_mb: 512,
            default_engines: strings(&["duckduckgo", "bing", "yahoojp", "seznam"]),
            reserve_engines: strings(&["yahoo", "youcom", "mwmbl"]),
            min_healthy_engines: 3,
            fresh_engines: strings(&["googlenews"]),
            locale_engines,
            max_results_per_engine: 10,
            scope_engine_limit: 3,
            auto_route_enabled: true,
            claim_engine_limit: 3,
            search_deadline_s: MAX_SEARCH_DEADLINE_S,
            coherence_guard_enabled: true,
            hedge_enabled: true,
            health_enabled: true,
            engine_cooldown_s: 600.0,
            engine_cooldown_max_s: 3600.0,
            rate_limit_rpm: 30,
            fetch_rate_limit_rpm: 20,
            request_timeout_s: 5.0,
            fetch_timeout_s: 25.0,
            user_agent: String::new(),
            accept_language: "en-US,en;q=0.9".to_owned(),
            region: "us-en".to_owned(),
            safesearch: SafeSearch::Moderate,
            contact_email: None,
            prewarm: true,
            max_content_chars: 50_000,
            log_level: "INFO".to_owned(),
            transport: "stdio".to_owned(),
            http_host: "127.0.0.1".to_owned(),
            http_port: 8000,
            http_path: "/mcp".to_owned(),
            http_allowed_origins: Vec::new(),
            http_token: None,
            tool_call_budget: 0,
            fetch_strategy: "auto".to_owned(),
            allow_private_hosts: false,
            ssrf_resolve_addresses: SsrfResolveMode::Auto,
            proxy: None,
            proxy_engines: Vec::new(),
            no_proxy: Vec::new(),
            document_root: None,
            download_enabled: true,
            download_ttl_hours: 24,
            download_max_mb: 100,
            download_dir_max_mb: 500,
            max_response_bytes: 25_000_000,
            max_pdf_pages: 200,
            max_document_chars: 2_000_000,
            exec_timeout_s: MAX_EXEC_TIMEOUT_S,
            exec_max_host_calls: 100,
            exec_heap_mb: 96,
            exec_raw_http: RawHttpPolicy::GetPost,
            exec_auth_hosts: Vec::new(),
            offline_sync: true,
            agent_backend: "off".to_owned(),
            agent_model: String::new(),
            agent_api_protocol: "openai".to_owned(),
            agent_api_base_url: String::new(),
            agent_api_key: None,
            agent_command: String::new(),
            agent_cli_args: String::new(),
            agent_depth: 3,
            agent_read_seconds: 15.0,
            agent_max_steps: 5,
            agent_max_source_chars: 12_000,
            agent_timeout_seconds: 90.0,
            agent_in_search: true,
            agent_search_timeout_seconds: 15.0,
            agent_engines: "auto".to_owned(),
            agent_lane_wait_seconds: 6.0,
            agent_lane_grace_seconds: 2.0,
            codex_timeout_s: 60.0,
            codex_model: String::new(),
            codex_reasoning_effort: "low".to_owned(),
            codex_base_url: "https://chatgpt.com/backend-api/codex".to_owned(),
            codex_auto_signin: true,
            codex_signin_wait_seconds: 120.0,
            codex_home: None,
            antigravity_timeout_s: 60.0,
            antigravity_model: String::new(),
            antigravity_base_urls: strings(&[
                "https://daily-cloudcode-pa.sandbox.googleapis.com",
                "https://cloudcode-pa.googleapis.com",
            ]),
            antigravity_version: "1.107.0".to_owned(),
            antigravity_client_id: None,
            antigravity_client_secret: None,
            searxng_base_url: None,
            degoog_base_url: None,
            api_keys: HashMap::new(),
            deprecation_warnings: Vec::new(),
        }
    }

    /// Defaults when there is no home directory: cache and config under
    /// `<temp>/search-mcp/{cache,config}`.
    pub fn defaults_without_home() -> Self {
        let base = homeless_base();
        let mut cfg = Self::defaults_for_home(&base);
        cfg.config_dir = base.join("config");
        cfg.cache_dir = base.join("cache");
        cfg.download_dir = cfg.cache_dir.join("downloads");
        cfg
    }

    /// Load from the real process environment and the files it points at.
    pub fn load() -> Result<Self, FsmError> {
        Self::from_sources(&EnvSources::from_system())
    }

    /// Value of a key variable (`auth.env` of a manifest, e.g. `SEARCH_MCP_ADS_API_KEY`).
    pub fn api_key(&self, env: &str) -> Option<&str> {
        self.api_keys.get(env).map(String::as_str)
    }

    /// Resolve a configuration from explicit sources (pure; used by tests).
    pub fn from_sources(src: &EnvSources) -> Result<Self, FsmError> {
        let merged = merge_sources(src);
        let mut notes = merged.notes;
        let vars = merged.vars;
        let home = src.home.as_deref();
        let mut cfg = match home {
            Some(h) => Self::defaults_for_home(h),
            None => {
                notes.push(format!(
                    "no home directory (HOME/USERPROFILE unset or relative): cache and config default to {}; no <config_dir> files are read unless SEARCH_MCP_CONFIG_DIR is set in the process environment",
                    homeless_base().display()
                ));
                Self::defaults_without_home()
            }
        };
        let get = |name: &str| vars.get(name).map(String::as_str);

        // Notices for renamed / dropped / P1-inactive names (any source).
        for spec in ENV_TABLE {
            if !vars.contains_key(spec.name) {
                continue;
            }
            match spec.disposition {
                EnvDisposition::Rename { to } => notes.push(format!(
                    "{} is deprecated; use {to} (the old name is still honoured)",
                    spec.name
                )),
                EnvDisposition::Drop => notes.push(format!(
                    "{} is no longer used in 1.0 and is ignored",
                    spec.name
                )),
                EnvDisposition::P1Notice => notes.push(format!(
                    "{} is accepted but inactive until browser rendering ships (P1)",
                    spec.name
                )),
                _ => {}
            }
        }

        if let Some(v) = get("SEARCH_MCP_CONFIG_DIR") {
            cfg.config_dir = expand_home("SEARCH_MCP_CONFIG_DIR", v, home)?;
        }
        if let Some(v) = get("SEARCH_MCP_CACHE_DIR") {
            cfg.cache_dir = expand_home("SEARCH_MCP_CACHE_DIR", v, home)?;
        }
        set(
            &mut cfg.cache_ttl_s,
            &vars,
            "SEARCH_MCP_CACHE_TTL_SECONDS",
            parse_num,
        )?;
        set(
            &mut cfg.cache_max_mb,
            &vars,
            "SEARCH_MCP_CACHE_MAX_MB",
            parse_num,
        )?;
        set(
            &mut cfg.default_engines,
            &vars,
            "SEARCH_MCP_DEFAULT_ENGINES",
            parse_list,
        )?;
        set(
            &mut cfg.reserve_engines,
            &vars,
            "SEARCH_MCP_RESERVE_ENGINES",
            parse_list,
        )?;
        if let Some(v) = get("SEARCH_MCP_RESCUE_ENGINES") {
            for e in parse_list("SEARCH_MCP_RESCUE_ENGINES", v)? {
                if !cfg.reserve_engines.contains(&e) {
                    cfg.reserve_engines.push(e);
                }
            }
        }
        set(
            &mut cfg.min_healthy_engines,
            &vars,
            "SEARCH_MCP_MIN_HEALTHY_ENGINES",
            parse_num,
        )?;
        set(
            &mut cfg.fresh_engines,
            &vars,
            "SEARCH_MCP_FRESH_ENGINES",
            parse_list,
        )?;
        set(
            &mut cfg.locale_engines,
            &vars,
            "SEARCH_MCP_LOCALE_ENGINES",
            parse_locale_map,
        )?;
        set(
            &mut cfg.max_results_per_engine,
            &vars,
            "SEARCH_MCP_MAX_RESULTS_PER_ENGINE",
            parse_num,
        )?;
        set(
            &mut cfg.scope_engine_limit,
            &vars,
            "SEARCH_MCP_CATEGORY_ENGINE_LIMIT",
            parse_num,
        )?;
        set(
            &mut cfg.scope_engine_limit,
            &vars,
            "SEARCH_MCP_SCOPE_ENGINE_LIMIT",
            parse_num,
        )?;
        set(
            &mut cfg.auto_route_enabled,
            &vars,
            "SEARCH_MCP_AUTO_ROUTE_ENABLED",
            parse_bool,
        )?;
        set(
            &mut cfg.claim_engine_limit,
            &vars,
            "SEARCH_MCP_CLAIM_ENGINE_LIMIT",
            parse_num,
        )?;
        if let Some(v) = get("SEARCH_MCP_SEARCH_DEADLINE_SECONDS") {
            let d: f64 = parse_num("SEARCH_MCP_SEARCH_DEADLINE_SECONDS", v)?;
            cfg.search_deadline_s = if d > 0.0 && d <= MAX_SEARCH_DEADLINE_S {
                d
            } else {
                notes.push(format!(
                    "SEARCH_MCP_SEARCH_DEADLINE_SECONDS={v} clamped to {MAX_SEARCH_DEADLINE_S} (hard ceiling in 1.0)"
                ));
                MAX_SEARCH_DEADLINE_S
            };
        }
        set(
            &mut cfg.coherence_guard_enabled,
            &vars,
            "SEARCH_MCP_COHERENCE_GUARD_ENABLED",
            parse_bool,
        )?;
        set(
            &mut cfg.hedge_enabled,
            &vars,
            "SEARCH_MCP_RESCUE_ENABLED",
            parse_bool,
        )?;
        set(
            &mut cfg.hedge_enabled,
            &vars,
            "SEARCH_MCP_HEDGE_ENABLED",
            parse_bool,
        )?;
        set(
            &mut cfg.health_enabled,
            &vars,
            "SEARCH_MCP_HEALTH_ENABLED",
            parse_bool,
        )?;
        set(
            &mut cfg.engine_cooldown_s,
            &vars,
            "SEARCH_MCP_ENGINE_COOLDOWN_SECONDS",
            parse_num,
        )?;
        set(
            &mut cfg.engine_cooldown_max_s,
            &vars,
            "SEARCH_MCP_ENGINE_COOLDOWN_MAX_SECONDS",
            parse_num,
        )?;
        set(
            &mut cfg.rate_limit_rpm,
            &vars,
            "SEARCH_MCP_RATE_LIMIT_PER_MINUTE",
            parse_positive,
        )?;
        set(
            &mut cfg.fetch_rate_limit_rpm,
            &vars,
            "SEARCH_MCP_FETCH_RATE_LIMIT_PER_MINUTE",
            parse_positive,
        )?;
        set(
            &mut cfg.request_timeout_s,
            &vars,
            "SEARCH_MCP_REQUEST_TIMEOUT",
            parse_positive_f64,
        )?;
        set(
            &mut cfg.fetch_timeout_s,
            &vars,
            "SEARCH_MCP_FETCH_TIMEOUT",
            parse_positive_f64,
        )?;
        set(
            &mut cfg.user_agent,
            &vars,
            "SEARCH_MCP_USER_AGENT",
            parse_string,
        )?;
        set(
            &mut cfg.accept_language,
            &vars,
            "SEARCH_MCP_ACCEPT_LANGUAGE",
            parse_string,
        )?;
        set(&mut cfg.region, &vars, "SEARCH_MCP_REGION", |_, v| {
            Ok(v.trim().to_ascii_lowercase())
        })?;
        set(
            &mut cfg.safesearch,
            &vars,
            "SEARCH_MCP_SAFESEARCH",
            parse_safesearch,
        )?;
        cfg.contact_email = get("SEARCH_MCP_CONTACT_EMAIL").map(|v| v.trim().to_owned());
        set(&mut cfg.prewarm, &vars, "SEARCH_MCP_PREWARM", parse_bool)?;
        set(
            &mut cfg.max_content_chars,
            &vars,
            "SEARCH_MCP_MAX_CONTENT_CHARS",
            parse_num,
        )?;
        set(&mut cfg.log_level, &vars, "SEARCH_MCP_LOG_LEVEL", |_, v| {
            Ok(v.trim().to_ascii_uppercase())
        })?;
        set(&mut cfg.transport, &vars, "SEARCH_MCP_TRANSPORT", |n, v| {
            parse_choice(n, v, &["stdio", "streamable-http"])
        })?;
        set(
            &mut cfg.http_host,
            &vars,
            "SEARCH_MCP_HTTP_HOST",
            parse_string,
        )?;
        set(&mut cfg.http_port, &vars, "SEARCH_MCP_HTTP_PORT", parse_num)?;
        set(
            &mut cfg.http_path,
            &vars,
            "SEARCH_MCP_HTTP_PATH",
            parse_string,
        )?;
        set(
            &mut cfg.http_allowed_origins,
            &vars,
            "SEARCH_MCP_HTTP_ALLOWED_ORIGINS",
            parse_list,
        )?;
        cfg.http_token = get("SEARCH_MCP_HTTP_TOKEN").map(|v| v.trim().to_owned());
        set(
            &mut cfg.tool_call_budget,
            &vars,
            "SEARCH_MCP_TOOL_CALL_BUDGET",
            parse_num,
        )?;
        set(
            &mut cfg.fetch_strategy,
            &vars,
            "SEARCH_MCP_FETCH_STRATEGY",
            |n, v| parse_choice(n, v, &["auto", "http", "browser"]),
        )?;
        if cfg.fetch_strategy == "browser" {
            notes.push("SEARCH_MCP_FETCH_STRATEGY=browser is inactive until browser rendering ships (P1); using http".to_owned());
        }
        set(
            &mut cfg.allow_private_hosts,
            &vars,
            "SEARCH_MCP_ALLOW_PRIVATE_HOSTS",
            parse_bool,
        )?;
        set(
            &mut cfg.ssrf_resolve_addresses,
            &vars,
            "SEARCH_MCP_SSRF_RESOLVE_ADDRESSES",
            parse_ssrf,
        )?;
        if let Some(v) = get("SEARCH_MCP_PROXY") {
            cfg.proxy = Some(parse_proxy("SEARCH_MCP_PROXY", v)?);
        }
        set(
            &mut cfg.proxy_engines,
            &vars,
            "SEARCH_MCP_PROXY_ENGINES",
            parse_list,
        )?;
        set(&mut cfg.no_proxy, &vars, "SEARCH_MCP_NO_PROXY", parse_list)?;
        cfg.document_root = get("SEARCH_MCP_DOCUMENT_ROOT")
            .map(|v| expand_home("SEARCH_MCP_DOCUMENT_ROOT", v, home))
            .transpose()?;
        set(
            &mut cfg.download_enabled,
            &vars,
            "SEARCH_MCP_DOWNLOAD_ENABLED",
            parse_bool,
        )?;
        cfg.download_dir = match get("SEARCH_MCP_DOWNLOAD_DIR") {
            Some(v) => expand_home("SEARCH_MCP_DOWNLOAD_DIR", v, home)?,
            None => cfg.cache_dir.join("downloads"),
        };
        set(
            &mut cfg.download_ttl_hours,
            &vars,
            "SEARCH_MCP_DOWNLOAD_TTL_HOURS",
            parse_num,
        )?;
        set(
            &mut cfg.download_max_mb,
            &vars,
            "SEARCH_MCP_DOWNLOAD_MAX_MB",
            parse_num,
        )?;
        set(
            &mut cfg.download_dir_max_mb,
            &vars,
            "SEARCH_MCP_DOWNLOAD_DIR_MAX_MB",
            parse_num,
        )?;
        set(
            &mut cfg.max_response_bytes,
            &vars,
            "SEARCH_MCP_MAX_RESPONSE_BYTES",
            parse_num,
        )?;
        set(
            &mut cfg.max_pdf_pages,
            &vars,
            "SEARCH_MCP_MAX_PDF_PAGES",
            parse_num,
        )?;
        set(
            &mut cfg.max_document_chars,
            &vars,
            "SEARCH_MCP_MAX_DOCUMENT_CHARS",
            parse_num,
        )?;
        if let Some(v) = get("SEARCH_MCP_EXEC_TIMEOUT_S") {
            cfg.exec_timeout_s = clamp_note(
                &mut notes,
                "SEARCH_MCP_EXEC_TIMEOUT_S",
                parse_num("SEARCH_MCP_EXEC_TIMEOUT_S", v)?,
                1,
                MAX_EXEC_TIMEOUT_S,
            );
        }
        if let Some(v) = get("SEARCH_MCP_EXEC_MAX_HOST_CALLS") {
            cfg.exec_max_host_calls = clamp_note(
                &mut notes,
                "SEARCH_MCP_EXEC_MAX_HOST_CALLS",
                parse_num("SEARCH_MCP_EXEC_MAX_HOST_CALLS", v)?,
                1,
                300,
            );
        }
        if let Some(v) = get("SEARCH_MCP_EXEC_HEAP_MB") {
            cfg.exec_heap_mb = clamp_note(
                &mut notes,
                "SEARCH_MCP_EXEC_HEAP_MB",
                parse_num("SEARCH_MCP_EXEC_HEAP_MB", v)?,
                16,
                128,
            );
        }
        match get("SEARCH_MCP_EXEC_RAW_HTTP") {
            Some(v) => cfg.exec_raw_http = parse_raw_http("SEARCH_MCP_EXEC_RAW_HTTP", v)?,
            None if cfg.document_root.is_some() => cfg.exec_raw_http = RawHttpPolicy::GetOnly,
            None => {}
        }
        set(
            &mut cfg.exec_auth_hosts,
            &vars,
            "SEARCH_MCP_EXEC_AUTH_HOSTS",
            parse_list,
        )?;
        set(
            &mut cfg.offline_sync,
            &vars,
            "SEARCH_MCP_OFFLINE_SYNC",
            parse_bool,
        )?;

        // ---- Answer agent, agent lane, Codex & Antigravity (ruling R5) ----
        set(
            &mut cfg.agent_backend,
            &vars,
            "SEARCH_MCP_AGENT_BACKEND",
            parse_string,
        )?;
        set(
            &mut cfg.agent_model,
            &vars,
            "SEARCH_MCP_AGENT_MODEL",
            parse_string,
        )?;
        set(
            &mut cfg.agent_api_protocol,
            &vars,
            "SEARCH_MCP_AGENT_API_PROTOCOL",
            parse_string,
        )?;
        set(
            &mut cfg.agent_api_base_url,
            &vars,
            "SEARCH_MCP_AGENT_API_BASE_URL",
            parse_string,
        )?;
        if let Some(v) = get("SEARCH_MCP_AGENT_API_KEY") {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                cfg.agent_api_key = Some(trimmed.to_owned());
            }
        }
        set(
            &mut cfg.agent_command,
            &vars,
            "SEARCH_MCP_AGENT_COMMAND",
            parse_string,
        )?;
        set(
            &mut cfg.agent_cli_args,
            &vars,
            "SEARCH_MCP_AGENT_CLI_ARGS",
            parse_string,
        )?;
        set(
            &mut cfg.agent_depth,
            &vars,
            "SEARCH_MCP_AGENT_DEPTH",
            parse_num,
        )?;
        set(
            &mut cfg.agent_read_seconds,
            &vars,
            "SEARCH_MCP_AGENT_READ_SECONDS",
            parse_nonneg_f64,
        )?;
        set(
            &mut cfg.agent_max_steps,
            &vars,
            "SEARCH_MCP_AGENT_MAX_STEPS",
            parse_num,
        )?;
        set(
            &mut cfg.agent_max_source_chars,
            &vars,
            "SEARCH_MCP_AGENT_MAX_SOURCE_CHARS",
            parse_num,
        )?;
        set(
            &mut cfg.agent_timeout_seconds,
            &vars,
            "SEARCH_MCP_AGENT_TIMEOUT_SECONDS",
            parse_nonneg_f64,
        )?;
        set(
            &mut cfg.agent_in_search,
            &vars,
            "SEARCH_MCP_AGENT_IN_SEARCH",
            parse_bool,
        )?;
        set(
            &mut cfg.agent_search_timeout_seconds,
            &vars,
            "SEARCH_MCP_AGENT_SEARCH_TIMEOUT_SECONDS",
            parse_nonneg_f64,
        )?;
        set(
            &mut cfg.agent_engines,
            &vars,
            "SEARCH_MCP_AGENT_ENGINES",
            parse_string,
        )?;
        set(
            &mut cfg.agent_lane_wait_seconds,
            &vars,
            "SEARCH_MCP_AGENT_LANE_WAIT_SECONDS",
            parse_nonneg_f64,
        )?;
        set(
            &mut cfg.agent_lane_grace_seconds,
            &vars,
            "SEARCH_MCP_AGENT_LANE_GRACE_SECONDS",
            parse_nonneg_f64,
        )?;
        set(
            &mut cfg.codex_timeout_s,
            &vars,
            "SEARCH_MCP_CODEX_TIMEOUT",
            parse_positive_f64,
        )?;
        set(
            &mut cfg.codex_model,
            &vars,
            "SEARCH_MCP_CODEX_MODEL",
            parse_string,
        )?;
        set(
            &mut cfg.codex_reasoning_effort,
            &vars,
            "SEARCH_MCP_CODEX_REASONING_EFFORT",
            parse_string,
        )?;
        set(
            &mut cfg.codex_base_url,
            &vars,
            "SEARCH_MCP_CODEX_BASE_URL",
            parse_string,
        )?;
        set(
            &mut cfg.codex_auto_signin,
            &vars,
            "SEARCH_MCP_CODEX_AUTO_SIGNIN",
            parse_bool,
        )?;
        set(
            &mut cfg.codex_signin_wait_seconds,
            &vars,
            "SEARCH_MCP_CODEX_SIGNIN_WAIT_SECONDS",
            parse_nonneg_f64,
        )?;
        if let Some(v) = get("CODEX_HOME") {
            cfg.codex_home = Some(expand_home("CODEX_HOME", v, home)?);
        }
        set(
            &mut cfg.antigravity_timeout_s,
            &vars,
            "SEARCH_MCP_ANTIGRAVITY_TIMEOUT",
            parse_positive_f64,
        )?;
        set(
            &mut cfg.antigravity_model,
            &vars,
            "SEARCH_MCP_ANTIGRAVITY_MODEL",
            parse_string,
        )?;
        set(
            &mut cfg.antigravity_base_urls,
            &vars,
            "SEARCH_MCP_ANTIGRAVITY_BASE_URLS",
            parse_list,
        )?;
        set(
            &mut cfg.antigravity_version,
            &vars,
            "SEARCH_MCP_ANTIGRAVITY_VERSION",
            parse_string,
        )?;
        if let Some(v) = get("SEARCH_MCP_ANTIGRAVITY_CLIENT_ID") {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                cfg.antigravity_client_id = Some(trimmed.to_owned());
            }
        }
        if let Some(v) = get("SEARCH_MCP_ANTIGRAVITY_CLIENT_SECRET") {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                cfg.antigravity_client_secret = Some(trimmed.to_owned());
            }
        }

        cfg.searxng_base_url = get("SEARCH_MCP_SEARXNG_BASE_URL")
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned);
        cfg.degoog_base_url = get("SEARCH_MCP_DEGOOG_BASE_URL")
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned);

        for (name, value) in &vars {
            if is_key_var(name)
                && !matches!(
                    name.as_str(),
                    "SEARCH_MCP_HTTP_TOKEN"
                        | "SEARCH_MCP_AGENT_API_KEY"
                        | "SEARCH_MCP_ANTIGRAVITY_CLIENT_ID"
                        | "SEARCH_MCP_ANTIGRAVITY_CLIENT_SECRET"
                )
                && !is_dropped(name)
            {
                cfg.api_keys.insert(name.clone(), value.trim().to_owned());
            }
        }

        let mut seen = BTreeSet::new();
        notes.retain(|n| seen.insert(n.clone()));
        cfg.deprecation_warnings = notes;
        Ok(cfg)
    }
}

/// Ceiling of `SEARCH_MCP_SEARCH_DEADLINE_SECONDS`.
pub const MAX_SEARCH_DEADLINE_S: f64 = 8.0;
/// Ceiling of the `exec` `timeout_s` argument.
pub const MAX_EXEC_TIMEOUT_S: u64 = 120;

fn set<T>(
    slot: &mut T,
    vars: &BTreeMap<String, String>,
    name: &str,
    parse: impl Fn(&str, &str) -> Result<T, FsmError>,
) -> Result<(), FsmError> {
    if let Some(v) = vars.get(name) {
        *slot = parse(name, v)?;
    }
    Ok(())
}

fn clamp_note<T: PartialOrd + Copy + fmt::Display>(
    notes: &mut Vec<String>,
    name: &str,
    v: T,
    lo: T,
    hi: T,
) -> T {
    if !(lo..=hi).contains(&v) {
        let c = if v < lo { lo } else { hi };
        notes.push(format!("{name}={v} clamped to {c} (allowed {lo}..={hi})"));
        c
    } else {
        v
    }
}

fn bad(name: &str, value: &str, expected: &str) -> FsmError {
    FsmError::invalid(format!("{name}={value:?} is invalid: expected {expected}"))
}

fn parse_string(_: &str, v: &str) -> Result<String, FsmError> {
    Ok(v.trim().to_owned())
}

/// pydantic-compatible booleans.
pub fn parse_bool(name: &str, v: &str) -> Result<bool, FsmError> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "t" | "yes" | "y" | "on" => Ok(true),
        "0" | "false" | "f" | "no" | "n" | "off" => Ok(false),
        _ => Err(bad(name, v, "a boolean (true/false, 1/0, yes/no, on/off)")),
    }
}

fn parse_num<T: std::str::FromStr>(name: &str, v: &str) -> Result<T, FsmError> {
    v.trim()
        .parse()
        .map_err(|_| bad(name, v, "a number in range"))
}

fn parse_positive(name: &str, v: &str) -> Result<u32, FsmError> {
    match parse_num::<u32>(name, v)? {
        0 => Err(bad(name, v, "a number > 0")),
        n => Ok(n),
    }
}

fn parse_positive_f64(name: &str, v: &str) -> Result<f64, FsmError> {
    let f: f64 = parse_num(name, v)?;
    if f > 0.0 && f.is_finite() {
        Ok(f)
    } else {
        Err(bad(name, v, "a number > 0"))
    }
}

fn parse_nonneg_f64(name: &str, v: &str) -> Result<f64, FsmError> {
    let f: f64 = parse_num(name, v)?;
    if f >= 0.0 && f.is_finite() {
        Ok(f)
    } else {
        Err(bad(name, v, "a number >= 0"))
    }
}

/// A JSON array of strings, or a comma/whitespace separated list.
pub fn parse_list(name: &str, v: &str) -> Result<Vec<String>, FsmError> {
    let t = v.trim();
    if t.starts_with('[') {
        let list: Vec<String> =
            serde_json::from_str(t).map_err(|_| bad(name, v, "a JSON array of strings"))?;
        return Ok(list
            .into_iter()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect());
    }
    Ok(t.split(|c: char| c == ',' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect())
}

fn parse_locale_map(name: &str, v: &str) -> Result<HashMap<String, Vec<String>>, FsmError> {
    serde_json::from_str(v.trim())
        .map_err(|_| bad(name, v, r#"a JSON object like {"zh":["so360"]}"#))
}

fn parse_choice(name: &str, v: &str, choices: &[&str]) -> Result<String, FsmError> {
    let t = v.trim().to_ascii_lowercase();
    if choices.contains(&t.as_str()) {
        Ok(t)
    } else {
        Err(bad(name, v, &choices.join(" | ")))
    }
}

fn parse_safesearch(name: &str, v: &str) -> Result<SafeSearch, FsmError> {
    Ok(
        match parse_choice(name, v, &["strict", "moderate", "off"])?.as_str() {
            "strict" => SafeSearch::Strict,
            "off" => SafeSearch::Off,
            _ => SafeSearch::Moderate,
        },
    )
}

fn parse_ssrf(name: &str, v: &str) -> Result<SsrfResolveMode, FsmError> {
    Ok(
        match parse_choice(name, v, &["auto", "always", "never"])?.as_str() {
            "always" => SsrfResolveMode::Always,
            "never" => SsrfResolveMode::Never,
            _ => SsrfResolveMode::Auto,
        },
    )
}

fn parse_raw_http(name: &str, v: &str) -> Result<RawHttpPolicy, FsmError> {
    Ok(
        match parse_choice(name, v, &["get-post", "get-only", "off"])?.as_str() {
            "get-only" => RawHttpPolicy::GetOnly,
            "off" => RawHttpPolicy::Off,
            _ => RawHttpPolicy::GetPost,
        },
    )
}

fn parse_proxy(name: &str, v: &str) -> Result<String, FsmError> {
    let t = v.trim();
    let lower = t.to_ascii_lowercase();
    if ["http://", "https://", "socks5://", "socks5h://"]
        .iter()
        .any(|p| lower.starts_with(p))
    {
        Ok(t.to_owned())
    } else {
        Err(FsmError::invalid(format!(
            "{name} is invalid: expected an http://, https://, socks5:// or socks5h:// URL"
        )))
    }
}

fn expand_home(name: &str, v: &str, home: Option<&Path>) -> Result<PathBuf, FsmError> {
    let t = v.trim();
    let rest = if t == "~" {
        Some("")
    } else {
        t.strip_prefix("~/")
    };
    match (rest, home) {
        (None, _) => Ok(PathBuf::from(t)),
        (Some(""), Some(h)) => Ok(h.to_path_buf()),
        (Some(r), Some(h)) => Ok(h.join(r)),
        (Some(_), None) => Err(FsmError::invalid(format!(
            "{name}={v:?} uses ~ but there is no home directory"
        ))),
    }
}

/// Where a variable came from / how 1.0 treats it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum EnvOrigin {
    /// A field of the Python `Settings` model (84).
    #[default]
    LegacySettings,
    /// Read by keystore / installer / tests outside `Settings`.
    LegacyNonSettings,
    /// Introduced in 1.0.
    New,
}

/// Disposition of a variable in 1.0 (`SPEC.md` §9.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum EnvDisposition {
    #[default]
    Keep,
    /// Honoured as an alias of `to`, with a one-time notice.
    Rename { to: &'static str },
    /// Ignored, with a one-time notice.
    Drop,
    /// Accepted without error, inactive until P1, with a notice.
    P1Notice,
    /// New in 1.0.
    New,
}

/// One row of the environment disposition table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct EnvVarSpec {
    /// Full variable name (`SEARCH_MCP_` prefix except `CODEX_HOME`).
    pub name: &'static str,
    pub origin: EnvOrigin,
    pub category: &'static str,
    pub disposition: EnvDisposition,
    /// Banned from the CWD `./.env` (I12). Key-pattern names are also
    /// security-sensitive whether listed or not, see [`is_security_sensitive`].
    pub security: bool,
}

/// Complete disposition table: 84 legacy `Settings` fields, 22 non-`Settings`
/// variables (14 kept, 8 dropped), 14 new 1.0 variables plus the 2 rename targets.
pub static ENV_TABLE: &[EnvVarSpec] = &[
    EnvVarSpec {
        name: "SEARCH_MCP_CACHE_DIR",
        origin: EnvOrigin::LegacySettings,
        category: "Cache & Storage",
        disposition: EnvDisposition::Keep,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_CACHE_TTL_SECONDS",
        origin: EnvOrigin::LegacySettings,
        category: "Cache & Storage",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_CACHE_MAX_MB",
        origin: EnvOrigin::LegacySettings,
        category: "Cache & Storage",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_DEFAULT_ENGINES",
        origin: EnvOrigin::LegacySettings,
        category: "Pools & Routing",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_RESERVE_ENGINES",
        origin: EnvOrigin::LegacySettings,
        category: "Pools & Routing",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_MIN_HEALTHY_ENGINES",
        origin: EnvOrigin::LegacySettings,
        category: "Pools & Routing",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_FRESH_ENGINES",
        origin: EnvOrigin::LegacySettings,
        category: "Pools & Routing",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_LOCALE_ENGINES",
        origin: EnvOrigin::LegacySettings,
        category: "Pools & Routing",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_MAX_RESULTS_PER_ENGINE",
        origin: EnvOrigin::LegacySettings,
        category: "Pools & Routing",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_AUTO_ROUTE_ENABLED",
        origin: EnvOrigin::LegacySettings,
        category: "Pools & Routing",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_CLAIM_ENGINE_LIMIT",
        origin: EnvOrigin::LegacySettings,
        category: "Pools & Routing",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_COHERENCE_GUARD_ENABLED",
        origin: EnvOrigin::LegacySettings,
        category: "Pools & Routing",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_CATEGORY_ENGINE_LIMIT",
        origin: EnvOrigin::LegacySettings,
        category: "Pool Renames",
        disposition: EnvDisposition::Rename {
            to: "SEARCH_MCP_SCOPE_ENGINE_LIMIT",
        },
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_RESCUE_ENGINES",
        origin: EnvOrigin::LegacySettings,
        category: "Pool Renames",
        disposition: EnvDisposition::Rename {
            to: "SEARCH_MCP_RESERVE_ENGINES",
        },
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_RESCUE_ENABLED",
        origin: EnvOrigin::LegacySettings,
        category: "Pool Renames",
        disposition: EnvDisposition::Rename {
            to: "SEARCH_MCP_HEDGE_ENABLED",
        },
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_RESCUE_TIMEOUT",
        origin: EnvOrigin::LegacySettings,
        category: "Rescue Drops",
        disposition: EnvDisposition::Drop,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_RESCUE_STAGGER_SECONDS",
        origin: EnvOrigin::LegacySettings,
        category: "Rescue Drops",
        disposition: EnvDisposition::Drop,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_HEALTH_ENABLED",
        origin: EnvOrigin::LegacySettings,
        category: "Health & Limits",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_ENGINE_COOLDOWN_SECONDS",
        origin: EnvOrigin::LegacySettings,
        category: "Health & Limits",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_ENGINE_COOLDOWN_MAX_SECONDS",
        origin: EnvOrigin::LegacySettings,
        category: "Health & Limits",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_SEARCH_DEADLINE_SECONDS",
        origin: EnvOrigin::LegacySettings,
        category: "Health & Limits",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_RATE_LIMIT_PER_MINUTE",
        origin: EnvOrigin::LegacySettings,
        category: "Health & Limits",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_FETCH_RATE_LIMIT_PER_MINUTE",
        origin: EnvOrigin::LegacySettings,
        category: "Health & Limits",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_REQUEST_TIMEOUT",
        origin: EnvOrigin::LegacySettings,
        category: "Health & Limits",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_FETCH_TIMEOUT",
        origin: EnvOrigin::LegacySettings,
        category: "Health & Limits",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_USER_AGENT",
        origin: EnvOrigin::LegacySettings,
        category: "Locale & UA",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_ACCEPT_LANGUAGE",
        origin: EnvOrigin::LegacySettings,
        category: "Locale & UA",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_REGION",
        origin: EnvOrigin::LegacySettings,
        category: "Locale & UA",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_SAFESEARCH",
        origin: EnvOrigin::LegacySettings,
        category: "Locale & UA",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_CONTACT_EMAIL",
        origin: EnvOrigin::LegacySettings,
        category: "Locale & UA",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_PREWARM",
        origin: EnvOrigin::LegacySettings,
        category: "Locale & UA",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_MAX_CONTENT_CHARS",
        origin: EnvOrigin::LegacySettings,
        category: "Locale & UA",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_LOG_LEVEL",
        origin: EnvOrigin::LegacySettings,
        category: "Locale & UA",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_TRANSPORT",
        origin: EnvOrigin::LegacySettings,
        category: "Transport & HTTP",
        disposition: EnvDisposition::Keep,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_HTTP_HOST",
        origin: EnvOrigin::LegacySettings,
        category: "Transport & HTTP",
        disposition: EnvDisposition::Keep,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_HTTP_PORT",
        origin: EnvOrigin::LegacySettings,
        category: "Transport & HTTP",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_HTTP_PATH",
        origin: EnvOrigin::LegacySettings,
        category: "Transport & HTTP",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_HTTP_ALLOWED_ORIGINS",
        origin: EnvOrigin::LegacySettings,
        category: "Transport & HTTP",
        disposition: EnvDisposition::Keep,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_TOOL_CALL_BUDGET",
        origin: EnvOrigin::LegacySettings,
        category: "Transport & HTTP",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_FETCH_STRATEGY",
        origin: EnvOrigin::LegacySettings,
        category: "Browser",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_BROWSER_HEADLESS",
        origin: EnvOrigin::LegacySettings,
        category: "Browser",
        disposition: EnvDisposition::P1Notice,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_BROWSER_CHANNEL",
        origin: EnvOrigin::LegacySettings,
        category: "Browser",
        disposition: EnvDisposition::P1Notice,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_BROWSER_DISK_CACHE_BYTES",
        origin: EnvOrigin::LegacySettings,
        category: "Browser",
        disposition: EnvDisposition::P1Notice,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_BROWSER_POOL_SIZE",
        origin: EnvOrigin::LegacySettings,
        category: "Browser",
        disposition: EnvDisposition::P1Notice,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_BROWSER_LAUNCH_TIMEOUT",
        origin: EnvOrigin::LegacySettings,
        category: "Browser",
        disposition: EnvDisposition::P1Notice,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_BROWSER_LAUNCH_COOLDOWN",
        origin: EnvOrigin::LegacySettings,
        category: "Browser",
        disposition: EnvDisposition::P1Notice,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_ALLOW_PRIVATE_HOSTS",
        origin: EnvOrigin::LegacySettings,
        category: "SSRF, Docs & Downloads",
        disposition: EnvDisposition::Keep,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_SSRF_RESOLVE_ADDRESSES",
        origin: EnvOrigin::LegacySettings,
        category: "SSRF, Docs & Downloads",
        disposition: EnvDisposition::Keep,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_DOCUMENT_ROOT",
        origin: EnvOrigin::LegacySettings,
        category: "SSRF, Docs & Downloads",
        disposition: EnvDisposition::Keep,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_DOWNLOAD_ENABLED",
        origin: EnvOrigin::LegacySettings,
        category: "SSRF, Docs & Downloads",
        disposition: EnvDisposition::Keep,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_DOWNLOAD_DIR",
        origin: EnvOrigin::LegacySettings,
        category: "SSRF, Docs & Downloads",
        disposition: EnvDisposition::Keep,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_DOWNLOAD_TTL_HOURS",
        origin: EnvOrigin::LegacySettings,
        category: "SSRF, Docs & Downloads",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_DOWNLOAD_MAX_MB",
        origin: EnvOrigin::LegacySettings,
        category: "SSRF, Docs & Downloads",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_MAX_RESPONSE_BYTES",
        origin: EnvOrigin::LegacySettings,
        category: "SSRF, Docs & Downloads",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_MAX_PDF_PAGES",
        origin: EnvOrigin::LegacySettings,
        category: "SSRF, Docs & Downloads",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_MAX_DOCUMENT_CHARS",
        origin: EnvOrigin::LegacySettings,
        category: "SSRF, Docs & Downloads",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_TOOLS",
        origin: EnvOrigin::LegacySettings,
        category: "Dropped Legacy Settings",
        disposition: EnvDisposition::Drop,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_AGENT_BACKEND",
        origin: EnvOrigin::LegacySettings,
        category: "Agent & OAuth Engines",
        disposition: EnvDisposition::Keep,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_AGENT_MODEL",
        origin: EnvOrigin::LegacySettings,
        category: "Agent & OAuth Engines",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_AGENT_API_PROTOCOL",
        origin: EnvOrigin::LegacySettings,
        category: "Agent & OAuth Engines",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_AGENT_API_BASE_URL",
        origin: EnvOrigin::LegacySettings,
        category: "Agent & OAuth Engines",
        disposition: EnvDisposition::Keep,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_AGENT_API_KEY",
        origin: EnvOrigin::LegacySettings,
        category: "Agent & OAuth Engines",
        disposition: EnvDisposition::Keep,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_AGENT_COMMAND",
        origin: EnvOrigin::LegacySettings,
        category: "Agent & OAuth Engines",
        disposition: EnvDisposition::Keep,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_AGENT_CLI_ARGS",
        origin: EnvOrigin::LegacySettings,
        category: "Agent & OAuth Engines",
        disposition: EnvDisposition::Keep,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_AGENT_DEPTH",
        origin: EnvOrigin::LegacySettings,
        category: "Agent & OAuth Engines",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_AGENT_READ_SECONDS",
        origin: EnvOrigin::LegacySettings,
        category: "Agent & OAuth Engines",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_AGENT_MAX_STEPS",
        origin: EnvOrigin::LegacySettings,
        category: "Agent & OAuth Engines",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_AGENT_MAX_SOURCE_CHARS",
        origin: EnvOrigin::LegacySettings,
        category: "Agent & OAuth Engines",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_AGENT_TIMEOUT_SECONDS",
        origin: EnvOrigin::LegacySettings,
        category: "Agent & OAuth Engines",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_AGENT_IN_SEARCH",
        origin: EnvOrigin::LegacySettings,
        category: "Agent & OAuth Engines",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_AGENT_SEARCH_TIMEOUT_SECONDS",
        origin: EnvOrigin::LegacySettings,
        category: "Agent & OAuth Engines",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_AGENT_ENGINES",
        origin: EnvOrigin::LegacySettings,
        category: "Agent & OAuth Engines",
        disposition: EnvDisposition::Keep,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_AGENT_LANE_WAIT_SECONDS",
        origin: EnvOrigin::LegacySettings,
        category: "Agent & OAuth Engines",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_AGENT_LANE_GRACE_SECONDS",
        origin: EnvOrigin::LegacySettings,
        category: "Agent & OAuth Engines",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_CODEX_TIMEOUT",
        origin: EnvOrigin::LegacySettings,
        category: "Agent & OAuth Engines",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_CODEX_MODEL",
        origin: EnvOrigin::LegacySettings,
        category: "Agent & OAuth Engines",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_CODEX_REASONING_EFFORT",
        origin: EnvOrigin::LegacySettings,
        category: "Agent & OAuth Engines",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_CODEX_BASE_URL",
        origin: EnvOrigin::LegacySettings,
        category: "Agent & OAuth Engines",
        disposition: EnvDisposition::Keep,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_CODEX_AUTO_SIGNIN",
        origin: EnvOrigin::LegacySettings,
        category: "Agent & OAuth Engines",
        disposition: EnvDisposition::Keep,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_CODEX_SIGNIN_WAIT_SECONDS",
        origin: EnvOrigin::LegacySettings,
        category: "Agent & OAuth Engines",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_ANTIGRAVITY_TIMEOUT",
        origin: EnvOrigin::LegacySettings,
        category: "Agent & OAuth Engines",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_ANTIGRAVITY_MODEL",
        origin: EnvOrigin::LegacySettings,
        category: "Agent & OAuth Engines",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_ANTIGRAVITY_BASE_URLS",
        origin: EnvOrigin::LegacySettings,
        category: "Agent & OAuth Engines",
        disposition: EnvDisposition::Keep,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_ANTIGRAVITY_VERSION",
        origin: EnvOrigin::LegacySettings,
        category: "Agent & OAuth Engines",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_CONFIG_DIR",
        origin: EnvOrigin::LegacyNonSettings,
        category: "Non-Settings Kept",
        disposition: EnvDisposition::Keep,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_PROXY",
        origin: EnvOrigin::LegacyNonSettings,
        category: "Non-Settings Kept",
        disposition: EnvDisposition::Keep,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_PROXY_ENGINES",
        origin: EnvOrigin::LegacyNonSettings,
        category: "Non-Settings Kept",
        disposition: EnvDisposition::Keep,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_BRAVE_API_KEY",
        origin: EnvOrigin::LegacyNonSettings,
        category: "Non-Settings Kept",
        disposition: EnvDisposition::Keep,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_SERPER_API_KEY",
        origin: EnvOrigin::LegacyNonSettings,
        category: "Non-Settings Kept",
        disposition: EnvDisposition::Keep,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_SERPBASE_API_KEY",
        origin: EnvOrigin::LegacyNonSettings,
        category: "Non-Settings Kept",
        disposition: EnvDisposition::Keep,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_TAVILY_API_KEY",
        origin: EnvOrigin::LegacyNonSettings,
        category: "Non-Settings Kept",
        disposition: EnvDisposition::Keep,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_ANYSEARCH_API_KEY",
        origin: EnvOrigin::LegacyNonSettings,
        category: "Non-Settings Kept",
        disposition: EnvDisposition::Keep,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_SEMANTICSCHOLAR_API_KEY",
        origin: EnvOrigin::LegacyNonSettings,
        category: "Non-Settings Kept",
        disposition: EnvDisposition::Keep,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_GITHUB_TOKEN",
        origin: EnvOrigin::LegacyNonSettings,
        category: "Non-Settings Kept",
        disposition: EnvDisposition::Keep,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_STACKEXCHANGE_KEY",
        origin: EnvOrigin::LegacyNonSettings,
        category: "Non-Settings Kept",
        disposition: EnvDisposition::Keep,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_TEST_NETWORK",
        origin: EnvOrigin::LegacyNonSettings,
        category: "Non-Settings Kept",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_REPO_URL",
        origin: EnvOrigin::LegacyNonSettings,
        category: "Non-Settings Kept",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_INSTALL_DIR",
        origin: EnvOrigin::LegacyNonSettings,
        category: "Non-Settings Kept",
        disposition: EnvDisposition::Keep,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_GOOGLE_CSE_API_KEY",
        origin: EnvOrigin::LegacyNonSettings,
        category: "Non-Settings Dropped",
        disposition: EnvDisposition::Drop,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_GOOGLE_CSE_CX",
        origin: EnvOrigin::LegacyNonSettings,
        category: "Non-Settings Dropped",
        disposition: EnvDisposition::Drop,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_ANTIGRAVITY_CLIENT_ID",
        origin: EnvOrigin::LegacyNonSettings,
        category: "Non-Settings Kept",
        disposition: EnvDisposition::Keep,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_ANTIGRAVITY_CLIENT_SECRET",
        origin: EnvOrigin::LegacyNonSettings,
        category: "Non-Settings Kept",
        disposition: EnvDisposition::Keep,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_ADMIN_PORT",
        origin: EnvOrigin::LegacyNonSettings,
        category: "Non-Settings Dropped",
        disposition: EnvDisposition::Drop,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_ADMIN_NO_BROWSER",
        origin: EnvOrigin::LegacyNonSettings,
        category: "Non-Settings Dropped",
        disposition: EnvDisposition::Drop,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_SEARX_INSTANCES",
        origin: EnvOrigin::LegacyNonSettings,
        category: "Non-Settings Dropped",
        disposition: EnvDisposition::Drop,
        security: false,
    },
    EnvVarSpec {
        name: "CODEX_HOME",
        origin: EnvOrigin::LegacyNonSettings,
        category: "Non-Settings Kept",
        disposition: EnvDisposition::Keep,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_NO_PROXY",
        origin: EnvOrigin::New,
        category: "New 1.0 Vars",
        disposition: EnvDisposition::New,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_HTTP_TOKEN",
        origin: EnvOrigin::New,
        category: "New 1.0 Vars",
        disposition: EnvDisposition::New,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_DOWNLOAD_DIR_MAX_MB",
        origin: EnvOrigin::New,
        category: "New 1.0 Vars",
        disposition: EnvDisposition::New,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_EXEC_TIMEOUT_S",
        origin: EnvOrigin::New,
        category: "New 1.0 Vars",
        disposition: EnvDisposition::New,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_EXEC_MAX_HOST_CALLS",
        origin: EnvOrigin::New,
        category: "New 1.0 Vars",
        disposition: EnvDisposition::New,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_EXEC_HEAP_MB",
        origin: EnvOrigin::New,
        category: "New 1.0 Vars",
        disposition: EnvDisposition::New,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_EXEC_RAW_HTTP",
        origin: EnvOrigin::New,
        category: "New 1.0 Vars",
        disposition: EnvDisposition::New,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_EXEC_AUTH_HOSTS",
        origin: EnvOrigin::New,
        category: "New 1.0 Vars",
        disposition: EnvDisposition::New,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_OFFLINE_SYNC",
        origin: EnvOrigin::New,
        category: "New 1.0 Vars",
        disposition: EnvDisposition::New,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_OPENALEX_API_KEY",
        origin: EnvOrigin::New,
        category: "New 1.0 Vars",
        disposition: EnvDisposition::New,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_CORE_API_KEY",
        origin: EnvOrigin::New,
        category: "New 1.0 Vars",
        disposition: EnvDisposition::New,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_NCBI_API_KEY",
        origin: EnvOrigin::New,
        category: "New 1.0 Vars",
        disposition: EnvDisposition::New,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_ADS_API_KEY",
        origin: EnvOrigin::New,
        category: "New 1.0 Vars",
        disposition: EnvDisposition::New,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_OPENAQ_API_KEY",
        origin: EnvOrigin::New,
        category: "New 1.0 Vars",
        disposition: EnvDisposition::New,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_SEARXNG_BASE_URL",
        origin: EnvOrigin::New,
        category: "Optional Instance Endpoints",
        disposition: EnvDisposition::New,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_DEGOOG_BASE_URL",
        origin: EnvOrigin::New,
        category: "Optional Instance Endpoints",
        disposition: EnvDisposition::New,
        security: true,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_SCOPE_ENGINE_LIMIT",
        origin: EnvOrigin::New,
        category: "Pool Renames",
        disposition: EnvDisposition::New,
        security: false,
    },
    EnvVarSpec {
        name: "SEARCH_MCP_HEDGE_ENABLED",
        origin: EnvOrigin::New,
        category: "Pool Renames",
        disposition: EnvDisposition::New,
        security: false,
    },
];

/// The table row for a variable.
pub fn env_spec(name: &str) -> Option<&'static EnvVarSpec> {
    ENV_TABLE.iter().find(|s| s.name == name)
}

/// Credential suffixes: `SEARCH_MCP_*_API_KEY | *_TOKEN | *_KEY | *_SECRET | *_USERNAME | *_CLIENT_ID`
/// (the last three beyond `SPEC.md` §3.4: OAuth2 `env_id`/`env_secret` pairs
/// of A1 §A.2 and account-name credentials such as GeoNames).
pub const KEY_SUFFIXES: [&str; 6] = [
    "_API_KEY",
    "_TOKEN",
    "_KEY",
    "_SECRET",
    "_USERNAME",
    "_CLIENT_ID",
];

/// A credential variable (see [`KEY_SUFFIXES`]).
pub fn is_key_var(name: &str) -> bool {
    name.starts_with(ENV_PREFIX) && KEY_SUFFIXES.iter().any(|s| name.ends_with(s))
}

fn is_dropped(name: &str) -> bool {
    env_spec(name).is_some_and(|s| s.disposition == EnvDisposition::Drop)
}

/// I12: may this variable be set by the CWD `./.env`? (No when true.)
pub fn is_security_sensitive(name: &str) -> bool {
    is_key_var(name) || env_spec(name).is_some_and(|s| s.security)
}

/// One `KEY=value` value read the way dotenv files are (port of
/// `keystore._env_value`): a quoted value is what sits inside the quotes;
/// an unquoted value ends at a `#` preceded by whitespace.
pub fn env_value(raw: &str) -> String {
    let raw = raw.trim();
    if let Some(q) = raw.chars().next().filter(|c| *c == '"' || *c == '\'')
        && let Some(end) = raw[1..].find(q)
    {
        return raw[1..1 + end].to_owned();
    }
    let mut cut = raw.len();
    let bytes = raw.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'#' && i > 0 && bytes[i - 1].is_ascii_whitespace() {
            cut = i;
            break;
        }
    }
    raw[..cut]
        .trim_end()
        .trim_matches('"')
        .trim_matches('\'')
        .to_owned()
}

/// `SEARCH_MCP_*` pairs of a `.env` file, in file order.
pub fn parse_dotenv(contents: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, val)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.starts_with(ENV_PREFIX) {
            out.push((key.to_owned(), env_value(val)));
        }
    }
    out
}

/// MCPB `drop_unfilled`, generalised to every setting: empty values and
/// unresolved `${...}` placeholders mean "unset".
pub fn is_unfilled(value: &str) -> bool {
    value.trim().is_empty() || value.contains("${")
}

/// Raw inputs of [`Config::from_sources`].
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct EnvSources {
    /// The real process environment (only `SEARCH_MCP_*` and `CODEX_HOME` are read).
    pub process: BTreeMap<String, String>,
    /// Contents of `./.env` (CWD).
    pub cwd_dotenv: Option<String>,
    /// Contents of `<config_dir>/.env`.
    pub config_dir_dotenv: Option<String>,
    /// Contents of `<config_dir>/config.json` (`{"secrets": {...}}` or a flat object).
    pub config_json: Option<String>,
    /// Unix permission bits of `config.json`, when known.
    pub config_json_mode: Option<u32>,
    /// Home directory used for defaults and `~` expansion (`None`: unknown).
    pub home: Option<PathBuf>,
}

impl EnvSources {
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self {
            home: Some(home.into()),
            ..Self::default()
        }
    }

    /// Read the real environment, `./.env`, and the config-dir files
    /// (the config dir comes from the process environment only).
    pub fn from_system() -> Self {
        let process: BTreeMap<String, String> = std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .filter(|(k, _)| k.starts_with(ENV_PREFIX) || k == "CODEX_HOME")
            .collect();
        Self::collect(
            process,
            system_home(),
            &|p| std::fs::read_to_string(p).ok(),
            &file_mode,
        )
    }

    /// [`from_system`](Self::from_system) with the home directory and file
    /// access injected. Without a home and without a process-env
    /// `SEARCH_MCP_CONFIG_DIR`, no config-dir file is read.
    fn collect(
        process: BTreeMap<String, String>,
        home: Option<PathBuf>,
        read: &dyn Fn(&Path) -> Option<String>,
        mode: &dyn Fn(&Path) -> Option<u32>,
    ) -> Self {
        let config_dir = match process
            .get("SEARCH_MCP_CONFIG_DIR")
            .filter(|v| !is_unfilled(v))
        {
            Some(v) => expand_home("SEARCH_MCP_CONFIG_DIR", v, home.as_deref()).ok(),
            None => home.as_ref().map(|h| h.join(".config").join("search-mcp")),
        };
        let (config_dir_dotenv, config_json, config_json_mode) = match &config_dir {
            Some(d) => (
                read(&d.join(".env")),
                read(&d.join("config.json")),
                mode(&d.join("config.json")),
            ),
            None => (None, None, None),
        };
        Self {
            cwd_dotenv: read(Path::new(".env")),
            config_dir_dotenv,
            config_json,
            config_json_mode,
            home,
            process,
        }
    }
}

#[cfg(unix)]
fn file_mode(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .ok()
        .map(|m| m.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
fn file_mode(_: &Path) -> Option<u32> {
    None
}

struct Merged {
    vars: BTreeMap<String, String>,
    notes: Vec<String>,
}

fn merge_sources(src: &EnvSources) -> Merged {
    let mut vars = BTreeMap::new();
    let mut notes = Vec::new();
    for (k, v) in &src.process {
        if !is_unfilled(v) {
            vars.insert(k.clone(), v.clone());
        }
    }
    if let Some(text) = &src.cwd_dotenv {
        let mut ignored = Vec::new();
        for (k, v) in parse_dotenv(text) {
            if is_unfilled(&v) || vars.contains_key(&k) {
                continue;
            }
            if is_security_sensitive(&k) {
                ignored.push(k);
            } else {
                vars.insert(k, v);
            }
        }
        if !ignored.is_empty() {
            ignored.sort();
            ignored.dedup();
            notes.push(format!(
                "ignored security-sensitive {} from ./.env; set {} in the process environment or <config_dir>/.env instead",
                ignored.join(", "),
                if ignored.len() == 1 { "it" } else { "them" }
            ));
        }
    }
    if let Some(text) = &src.config_dir_dotenv {
        for (k, v) in parse_dotenv(text) {
            if !is_unfilled(&v) && !vars.contains_key(&k) {
                vars.insert(k, v);
            }
        }
    }
    if let Some(text) = &src.config_json {
        match serde_json::from_str::<serde_json::Value>(text) {
            Ok(serde_json::Value::Object(root)) => {
                let table = match root.get("secrets") {
                    Some(serde_json::Value::Object(s)) => s.clone(),
                    _ => root,
                };
                for (field, value) in table {
                    let value = match value {
                        serde_json::Value::String(s) => s,
                        serde_json::Value::Number(n) => n.to_string(),
                        _ => continue,
                    };
                    let name = format!("{ENV_PREFIX}{}", field.to_ascii_uppercase());
                    let allowed = is_key_var(&name)
                        || name == "SEARCH_MCP_PROXY"
                        || name == "SEARCH_MCP_PROXY_ENGINES"
                        || is_dropped(&name);
                    if allowed && !is_unfilled(&value) && !vars.contains_key(&name) {
                        vars.insert(name, value.trim().to_owned());
                    }
                }
            }
            _ => notes.push("<config_dir>/config.json is not a JSON object; ignored".to_owned()),
        }
        if let Some(mode) = src.config_json_mode
            && mode & 0o077 != 0
        {
            notes.push(format!(
                "<config_dir>/config.json has mode {mode:o}; run chmod 600 on it (it holds API keys)"
            ));
        }
    }
    Merged { vars, notes }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(process: &[(&str, &str)]) -> EnvSources {
        let mut s = EnvSources::new("/home/u");
        s.process = process
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        s
    }

    fn load(s: &EnvSources) -> Config {
        Config::from_sources(s).expect("config loads")
    }

    #[test]
    fn env_value_matches_python_vectors() {
        // tests/test_keystore.py::test_env_values_end_at_an_inline_comment
        for (raw, want) in [
            ("604800   # 7 days", "604800"),
            ("auto        # auto | http | browser", "auto"),
            ("\"quoted # kept\"   # dropped", "quoted # kept"),
            ("'single'", "single"),
            ("http://user:p#ss@proxy:8080", "http://user:p#ss@proxy:8080"),
            ("https://host/path#frag", "https://host/path#frag"),
            ("", ""),
            ("   ", ""),
            ("\"unterminated", "unterminated"),
        ] {
            assert_eq!(env_value(raw), want, "{raw:?}");
        }
    }

    #[test]
    fn dotenv_reads_only_prefixed_keys() {
        let pairs = parse_dotenv(
            "# a comment\nSEARCH_MCP_SERPER_API_KEY=from-dotenv\nSEARCH_MCP_TAVILY_API_KEY=\"quoted-val\"\nNOT_OURS=x\nnot a pair\n",
        );
        assert_eq!(
            pairs,
            vec![
                (
                    "SEARCH_MCP_SERPER_API_KEY".to_owned(),
                    "from-dotenv".to_owned()
                ),
                (
                    "SEARCH_MCP_TAVILY_API_KEY".to_owned(),
                    "quoted-val".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn defaults_follow_the_spec() {
        let c = load(&src(&[]));
        assert_eq!(c.config_dir, PathBuf::from("/home/u/.config/search-mcp"));
        assert_eq!(c.cache_dir, PathBuf::from("/home/u/.cache/search-mcp"));
        assert_eq!(
            c.download_dir,
            PathBuf::from("/home/u/.cache/search-mcp/downloads")
        );
        assert_eq!(
            c.default_engines,
            ["duckduckgo", "bing", "yahoojp", "seznam"]
        );
        assert_eq!(c.reserve_engines, ["yahoo", "youcom", "mwmbl"]);
        assert_eq!(c.locale_engines["zh"], ["so360", "quark", "baidu"]);
        assert_eq!(c.locale_engines["ko"], ["naver", "daum"]);
        assert_eq!(c.fresh_engines, ["googlenews"]);
        assert_eq!((c.search_deadline_s, c.request_timeout_s), (8.0, 5.0));
        assert_eq!(
            (c.exec_timeout_s, c.exec_max_host_calls, c.exec_heap_mb),
            (120, 100, 96)
        );
        assert_eq!(c.exec_raw_http, RawHttpPolicy::GetPost);
        assert_eq!(
            (c.download_dir_max_mb, c.max_response_bytes, c.cache_ttl_s),
            (500, 25_000_000, 604_800)
        );
        assert_eq!(
            (c.http_host.as_str(), c.http_port, c.http_path.as_str()),
            ("127.0.0.1", 8000, "/mcp")
        );
        assert!(c.hedge_enabled && c.offline_sync && c.download_enabled && !c.allow_private_hosts);
        assert!(c.deprecation_warnings.is_empty() && c.api_keys.is_empty());
    }

    #[test]
    fn optional_instance_endpoints_require_trusted_configuration() {
        let defaults = load(&src(&[]));
        assert!(defaults.searxng_base_url.is_none());
        assert!(defaults.degoog_base_url.is_none());
        let mut s = src(&[]);
        s.cwd_dotenv = Some(concat!(
            "SEARCH_MCP_SEARXNG_BASE_URL=https://untrusted.example\n",
            "SEARCH_MCP_DEGOOG_BASE_URL=https://untrusted.example\n",
            "SEARCH_MCP_SEARXNG_TOKEN=untrusted-token\n",
            "SEARCH_MCP_DEGOOG_API_KEY=untrusted-key\n",
        ).into());
        let c = load(&s);
        assert!(c.searxng_base_url.is_none() && c.degoog_base_url.is_none());
        assert!(c.api_key("SEARCH_MCP_SEARXNG_TOKEN").is_none());
        assert!(c.api_key("SEARCH_MCP_DEGOOG_API_KEY").is_none());
        for name in [
            "SEARCH_MCP_SEARXNG_BASE_URL", "SEARCH_MCP_DEGOOG_BASE_URL",
            "SEARCH_MCP_SEARXNG_TOKEN", "SEARCH_MCP_DEGOOG_API_KEY",
        ] {
            assert!(is_security_sensitive(name));
            assert!(c.deprecation_warnings.iter().any(|note| note.contains(name)));
        }
        assert!(!format!("{c:?}").contains("untrusted-token"));
        assert!(!c.deprecation_warnings.join(" ").contains("untrusted-key"));
    }

    #[test]
    fn optional_instance_configuration_uses_process_before_trusted_dotenv() {
        let mut s = src(&[
            ("SEARCH_MCP_SEARXNG_BASE_URL", " https://process.example/search "),
            ("SEARCH_MCP_SEARXNG_TOKEN", "process-token"),
        ]);
        s.config_dir_dotenv = Some(concat!(
            "SEARCH_MCP_SEARXNG_BASE_URL=https://config.example\n",
            "SEARCH_MCP_DEGOOG_BASE_URL=https://config.example/api\n",
            "SEARCH_MCP_SEARXNG_TOKEN=config-token\n",
            "SEARCH_MCP_DEGOOG_API_KEY=config-key\n",
        ).into());
        let c = load(&s);
        assert_eq!(c.searxng_base_url.as_deref(), Some("https://process.example/search"));
        assert_eq!(c.degoog_base_url.as_deref(), Some("https://config.example/api"));
        assert_eq!(c.api_key("SEARCH_MCP_SEARXNG_TOKEN"), Some("process-token"));
        assert_eq!(c.api_key("SEARCH_MCP_DEGOOG_API_KEY"), Some("config-key"));
        let debug = format!("{c:?}");
        assert!(!debug.contains("process-token") && !debug.contains("config-key"));
    }

    #[test]
    fn optional_instance_cwd_values_cannot_override_trusted_endpoints_or_keys() {
        let mut s = src(&[]);
        s.cwd_dotenv = Some(concat!(
            "SEARCH_MCP_SEARXNG_BASE_URL=https://untrusted.example\n",
            "SEARCH_MCP_DEGOOG_BASE_URL=https://untrusted.example\n",
            "SEARCH_MCP_SEARXNG_TOKEN=untrusted-token\n",
            "SEARCH_MCP_DEGOOG_API_KEY=untrusted-key\n",
        ).into());
        s.config_dir_dotenv = Some(concat!(
            "SEARCH_MCP_SEARXNG_BASE_URL=https://trusted.example/searxng\n",
            "SEARCH_MCP_DEGOOG_BASE_URL=https://trusted.example/degoog\n",
            "SEARCH_MCP_SEARXNG_TOKEN=trusted-token\n",
            "SEARCH_MCP_DEGOOG_API_KEY=trusted-key\n",
        ).into());
        let c = load(&s);
        assert_eq!(c.searxng_base_url.as_deref(), Some("https://trusted.example/searxng"));
        assert_eq!(c.degoog_base_url.as_deref(), Some("https://trusted.example/degoog"));
        assert_eq!(c.api_key("SEARCH_MCP_SEARXNG_TOKEN"), Some("trusted-token"));
        assert_eq!(c.api_key("SEARCH_MCP_DEGOOG_API_KEY"), Some("trusted-key"));
    }

    #[test]
    fn optional_instance_blank_endpoints_do_not_enable_engines() {
        let c = load(&src(&[
            ("SEARCH_MCP_SEARXNG_BASE_URL", "   "),
            ("SEARCH_MCP_DEGOOG_BASE_URL", ""),
            ("SEARCH_MCP_SEARXNG_TOKEN", "token-only"),
            ("SEARCH_MCP_DEGOOG_API_KEY", "key-only"),
        ]));
        assert!(c.searxng_base_url.is_none());
        assert!(c.degoog_base_url.is_none());
    }

    #[test]
    fn precedence_process_then_cwd_then_config_dir() {
        let mut s = src(&[("SEARCH_MCP_REGION", "de-de")]);
        s.cwd_dotenv = Some("SEARCH_MCP_REGION=fr-fr\nSEARCH_MCP_ACCEPT_LANGUAGE=fr\n".into());
        s.config_dir_dotenv =
            Some("SEARCH_MCP_ACCEPT_LANGUAGE=ja\nSEARCH_MCP_LOG_LEVEL=debug\n".into());
        let c = load(&s);
        assert_eq!(c.region, "de-de");
        assert_eq!(c.accept_language, "fr");
        assert_eq!(c.log_level, "DEBUG");
    }

    #[test]
    fn cwd_dotenv_cannot_set_security_keys() {
        let mut s = src(&[]);
        s.cwd_dotenv = Some(
            "SEARCH_MCP_ALLOW_PRIVATE_HOSTS=true\nSEARCH_MCP_DOCUMENT_ROOT=/\nSEARCH_MCP_BRAVE_API_KEY=k\nSEARCH_MCP_CONFIG_DIR=/tmp/evil\nSEARCH_MCP_EXEC_RAW_HTTP=get-post\nSEARCH_MCP_PROXY=http://p:1\nSEARCH_MCP_REGION=jp-jp\n".into(),
        );
        let c = load(&s);
        assert!(!c.allow_private_hosts);
        assert!(c.document_root.is_none() && c.proxy.is_none() && c.api_keys.is_empty());
        assert_eq!(c.config_dir, PathBuf::from("/home/u/.config/search-mcp"));
        assert_eq!(c.region, "jp-jp", "non-security keys still load");
        let note = c
            .deprecation_warnings
            .iter()
            .find(|n| n.contains("./.env"))
            .expect("notice");
        for k in [
            "ALLOW_PRIVATE_HOSTS",
            "DOCUMENT_ROOT",
            "BRAVE_API_KEY",
            "CONFIG_DIR",
            "EXEC_RAW_HTTP",
            "PROXY",
        ] {
            assert!(note.contains(k), "{k} missing from {note}");
        }
        assert!(!note.contains("REGION"));
        // The same keys are honoured from the process env and <config_dir>/.env.
        let mut s = src(&[("SEARCH_MCP_ALLOW_PRIVATE_HOSTS", "1")]);
        s.config_dir_dotenv =
            Some("SEARCH_MCP_BRAVE_API_KEY=cfg-key\nSEARCH_MCP_DOCUMENT_ROOT=~/docs\n".into());
        let c = load(&s);
        assert!(c.allow_private_hosts);
        assert_eq!(c.api_key("SEARCH_MCP_BRAVE_API_KEY"), Some("cfg-key"));
        assert_eq!(c.document_root, Some(PathBuf::from("/home/u/docs")));
        assert_eq!(
            c.exec_raw_http,
            RawHttpPolicy::GetOnly,
            "DOCUMENT_ROOT downgrades raw HTTP"
        );
    }

    #[test]
    fn config_json_supplies_secrets_below_env() {
        let mut s = src(&[("SEARCH_MCP_SERPER_API_KEY", "from-env")]);
        s.config_json = Some(
            r#"{"secrets": {"serper_api_key": "from-file", "tavily_api_key": "tvly-x", "proxy": "socks5h://127.0.0.1:7897", "region": "xx", "github_token": ""}}"#.into(),
        );
        s.config_json_mode = Some(0o644);
        let c = load(&s);
        assert_eq!(c.api_key("SEARCH_MCP_SERPER_API_KEY"), Some("from-env"));
        assert_eq!(c.api_key("SEARCH_MCP_TAVILY_API_KEY"), Some("tvly-x"));
        assert_eq!(
            c.api_key("SEARCH_MCP_GITHUB_TOKEN"),
            None,
            "empty string unsets"
        );
        assert_eq!(c.proxy.as_deref(), Some("socks5h://127.0.0.1:7897"));
        assert_eq!(c.region, "us-en", "config.json carries secrets only");
        assert!(
            c.deprecation_warnings
                .iter()
                .any(|n| n.contains("chmod 600"))
        );
        let mut flat = src(&[]);
        flat.config_json = Some(r#"{"brave_api_key": "b"}"#.into());
        assert_eq!(load(&flat).api_key("SEARCH_MCP_BRAVE_API_KEY"), Some("b"));
    }

    #[test]
    fn unfilled_mcpb_placeholders_are_unset() {
        let c = load(&src(&[
            ("SEARCH_MCP_PROXY", "${user_config.proxy}"),
            ("SEARCH_MCP_REGION", ""),
            ("SEARCH_MCP_CACHE_DIR", "   "),
        ]));
        assert!(c.proxy.is_none());
        assert_eq!(c.region, "us-en");
        assert_eq!(c.cache_dir, PathBuf::from("/home/u/.cache/search-mcp"));
        assert!(is_unfilled("${x}") && is_unfilled(" ") && !is_unfilled("v"));
    }

    #[test]
    fn renames_and_drops() {
        let c = load(&src(&[
            ("SEARCH_MCP_CATEGORY_ENGINE_LIMIT", "5"),
            ("SEARCH_MCP_RESCUE_ENGINES", r#"["youcom","baidu"]"#),
            ("SEARCH_MCP_RESCUE_ENABLED", "false"),
            ("SEARCH_MCP_RESCUE_TIMEOUT", "10"),
            ("SEARCH_MCP_TOOLS", "search"),
            ("SEARCH_MCP_AGENT_BACKEND", "api"),
            ("SEARCH_MCP_AGENT_API_KEY", "sk-x"),
            ("SEARCH_MCP_GOOGLE_CSE_API_KEY", "g"),
            ("SEARCH_MCP_BROWSER_HEADLESS", "false"),
            ("CODEX_HOME", "/x"),
        ]));
        assert_eq!(c.scope_engine_limit, 5);
        assert_eq!(c.reserve_engines, ["yahoo", "youcom", "mwmbl", "baidu"]);
        assert!(!c.hedge_enabled);
        // R5: agent and CODEX_HOME are kept and parsed, while AGENT_API_KEY stays off api_keys.
        assert_eq!(c.agent_backend, "api");
        assert_eq!(c.agent_api_key.as_deref(), Some("sk-x"));
        assert_eq!(c.codex_home, Some(PathBuf::from("/x")));
        assert!(
            c.api_keys.is_empty(),
            "dropped and agent keys are not exposed in api_keys: {:?}",
            c.api_keys.keys()
        );
        let w = c.deprecation_warnings.join("\n");
        for frag in [
            "SEARCH_MCP_CATEGORY_ENGINE_LIMIT is deprecated; use SEARCH_MCP_SCOPE_ENGINE_LIMIT",
            "SEARCH_MCP_RESCUE_ENGINES is deprecated",
            "SEARCH_MCP_RESCUE_ENABLED is deprecated; use SEARCH_MCP_HEDGE_ENABLED",
            "SEARCH_MCP_RESCUE_TIMEOUT is no longer used",
            "SEARCH_MCP_TOOLS is no longer used",
            "SEARCH_MCP_GOOGLE_CSE_API_KEY is no longer used",
            "SEARCH_MCP_BROWSER_HEADLESS is accepted but inactive",
        ] {
            assert!(w.contains(frag), "missing notice {frag:?} in\n{w}");
        }
        assert!(
            !w.contains("SEARCH_MCP_AGENT_BACKEND is no longer used"),
            "R5 kept AGENT_BACKEND:\n{w}"
        );
        assert!(
            !w.contains("CODEX_HOME is no longer used"),
            "R5 kept CODEX_HOME:\n{w}"
        );
        // New names win over old ones.
        let c = load(&src(&[
            ("SEARCH_MCP_CATEGORY_ENGINE_LIMIT", "5"),
            ("SEARCH_MCP_SCOPE_ENGINE_LIMIT", "2"),
            ("SEARCH_MCP_RESCUE_ENABLED", "false"),
            ("SEARCH_MCP_HEDGE_ENABLED", "true"),
        ]));
        assert_eq!(c.scope_engine_limit, 2);
        assert!(c.hedge_enabled);
    }

    #[test]
    fn value_parsing_and_errors() {
        let c = load(&src(&[
            ("SEARCH_MCP_DEFAULT_ENGINES", "duckduckgo, bing  seznam"),
            ("SEARCH_MCP_LOCALE_ENGINES", r#"{"zh":["quark"]}"#),
            ("SEARCH_MCP_HEALTH_ENABLED", "off"),
            ("SEARCH_MCP_CLAIM_ENGINE_LIMIT", "-1"),
            ("SEARCH_MCP_SAFESEARCH", "Strict"),
            ("SEARCH_MCP_SSRF_RESOLVE_ADDRESSES", "never"),
            (
                "SEARCH_MCP_HTTP_ALLOWED_ORIGINS",
                "https://a.example, https://b.example",
            ),
            ("SEARCH_MCP_NO_PROXY", "corp.example,.internal"),
            ("SEARCH_MCP_CONTACT_EMAIL", "someone@example.org"),
            ("SEARCH_MCP_TRANSPORT", "streamable-http"),
            ("SEARCH_MCP_HTTP_TOKEN", "t0k"),
            ("SEARCH_MCP_ADS_API_KEY", "ads"),
            ("SEARCH_MCP_EPO_OPS_SECRET", "sec"),
            ("SEARCH_MCP_GEONAMES_USERNAME", "geo"),
            ("SEARCH_MCP_LEGIFRANCE_CLIENT_ID", "cid"),
        ]));
        assert_eq!(c.default_engines, ["duckduckgo", "bing", "seznam"]);
        assert_eq!(c.locale_engines.len(), 1);
        assert!(!c.health_enabled);
        assert_eq!(c.claim_engine_limit, -1);
        assert_eq!(c.safesearch, SafeSearch::Strict);
        assert_eq!(c.ssrf_resolve_addresses, SsrfResolveMode::Never);
        assert_eq!(c.http_allowed_origins.len(), 2);
        assert_eq!(c.no_proxy, ["corp.example", ".internal"]);
        assert_eq!(c.contact_email.as_deref(), Some("someone@example.org"));
        assert_eq!(c.http_token.as_deref(), Some("t0k"));
        assert_eq!(c.api_key("SEARCH_MCP_ADS_API_KEY"), Some("ads"));
        assert_eq!(c.api_key("SEARCH_MCP_EPO_OPS_SECRET"), Some("sec"));
        assert_eq!(c.api_key("SEARCH_MCP_GEONAMES_USERNAME"), Some("geo"));
        assert_eq!(c.api_key("SEARCH_MCP_LEGIFRANCE_CLIENT_ID"), Some("cid"));
        assert!(is_security_sensitive("SEARCH_MCP_GEONAMES_USERNAME"));
        assert!(
            c.api_key("SEARCH_MCP_HTTP_TOKEN").is_none(),
            "HTTP_TOKEN is a server secret, not a source key"
        );

        for (k, v) in [
            ("SEARCH_MCP_HTTP_PORT", "99999"),
            ("SEARCH_MCP_RATE_LIMIT_PER_MINUTE", "0"),
            ("SEARCH_MCP_PREWARM", "maybe"),
            ("SEARCH_MCP_TRANSPORT", "sse"),
            ("SEARCH_MCP_PROXY", "ftp://x"),
            ("SEARCH_MCP_DEFAULT_ENGINES", "[1,2]"),
            ("SEARCH_MCP_REQUEST_TIMEOUT", "-1"),
        ] {
            let err = Config::from_sources(&src(&[(k, v)]))
                .err()
                .unwrap_or_else(|| panic!("{k}={v} accepted"));
            assert!(err.to_string().contains(k), "{err}");
        }
    }

    #[test]
    fn ceilings_clamp_with_a_notice() {
        let c = load(&src(&[
            ("SEARCH_MCP_SEARCH_DEADLINE_SECONDS", "10"),
            ("SEARCH_MCP_EXEC_TIMEOUT_S", "600"),
            ("SEARCH_MCP_EXEC_HEAP_MB", "512"),
            ("SEARCH_MCP_EXEC_MAX_HOST_CALLS", "1000"),
            ("SEARCH_MCP_FETCH_STRATEGY", "browser"),
        ]));
        assert_eq!(
            (
                c.search_deadline_s,
                c.exec_timeout_s,
                c.exec_heap_mb,
                c.exec_max_host_calls
            ),
            (8.0, 120, 128, 300)
        );
        assert_eq!(
            c.deprecation_warnings.len(),
            5,
            "{:?}",
            c.deprecation_warnings
        );
        let c = load(&src(&[
            ("SEARCH_MCP_EXEC_TIMEOUT_S", "45"),
            ("SEARCH_MCP_SEARCH_DEADLINE_SECONDS", "4.5"),
        ]));
        assert_eq!((c.exec_timeout_s, c.search_deadline_s), (45, 4.5));
        let c = load(&src(&[
            ("SEARCH_MCP_DOCUMENT_ROOT", "/data"),
            ("SEARCH_MCP_EXEC_RAW_HTTP", "get-post"),
        ]));
        assert_eq!(
            c.exec_raw_http,
            RawHttpPolicy::GetPost,
            "explicit setting wins over the downgrade"
        );
    }

    #[test]
    fn no_home_never_trusts_the_cwd() {
        use std::cell::RefCell;
        let reads = RefCell::new(Vec::new());
        let read = |p: &Path| {
            reads.borrow_mut().push(p.to_path_buf());
            Some("SEARCH_MCP_REGION=jp-jp\n".to_owned())
        };
        let s = EnvSources::collect(BTreeMap::new(), None, &read, &|_| None);
        assert_eq!(
            *reads.borrow(),
            vec![PathBuf::from(".env")],
            "only the CWD .env is read"
        );
        assert!(s.config_dir_dotenv.is_none() && s.config_json.is_none());
        let c = Config::from_sources(&s).unwrap();
        assert!(
            c.config_dir.starts_with(std::env::temp_dir())
                && c.cache_dir.starts_with(std::env::temp_dir())
        );
        assert!(
            c.deprecation_warnings
                .iter()
                .any(|n| n.contains("no home directory"))
        );
        assert_eq!(c.region, "jp-jp");
        // An explicit process-env CONFIG_DIR is honoured without a home.
        reads.borrow_mut().clear();
        let process: BTreeMap<String, String> =
            [("SEARCH_MCP_CONFIG_DIR".to_owned(), "/etc/fsm".to_owned())].into();
        let s = EnvSources::collect(process, None, &read, &|_| None);
        assert!(reads.borrow().contains(&PathBuf::from("/etc/fsm/.env")));
        assert!(s.config_json.is_some());
        // With a home, the default config dir is read.
        reads.borrow_mut().clear();
        EnvSources::collect(
            BTreeMap::new(),
            Some(PathBuf::from("/home/u")),
            &read,
            &|_| None,
        );
        assert!(
            reads
                .borrow()
                .contains(&PathBuf::from("/home/u/.config/search-mcp/config.json"))
        );
        // `~` cannot be expanded without a home.
        let mut s = EnvSources::default();
        s.process
            .insert("SEARCH_MCP_DOCUMENT_ROOT".into(), "~/docs".into());
        assert!(
            Config::from_sources(&s)
                .unwrap_err()
                .to_string()
                .contains("no home directory")
        );
    }

    #[test]
    fn debug_redacts_secrets() {
        let c = load(&src(&[
            ("SEARCH_MCP_HTTP_TOKEN", "super-secret-token"),
            ("SEARCH_MCP_PROXY", "http://user:pw@proxy:8080"),
            ("SEARCH_MCP_BRAVE_API_KEY", "brave-secret"),
            ("SEARCH_MCP_AGENT_API_KEY", "agent-secret-xyz"),
            ("SEARCH_MCP_ANTIGRAVITY_CLIENT_SECRET", "ag-secret-xyz"),
        ]));
        let d = format!("{c:?}");
        assert!(
            !d.contains("super-secret-token")
                && !d.contains("pw@proxy")
                && !d.contains("brave-secret")
                && !d.contains("agent-secret-xyz")
                && !d.contains("ag-secret-xyz"),
            "{d}"
        );
        assert!(d.contains("SEARCH_MCP_BRAVE_API_KEY"));
    }

    #[test]
    fn r5_agent_and_oauth_defaults_and_parsing() {
        let c = load(&src(&[]));
        assert_eq!(c.agent_backend, "off");
        assert_eq!(c.agent_api_protocol, "openai");
        assert_eq!(c.agent_depth, 3);
        assert_eq!(c.agent_read_seconds, 15.0);
        assert_eq!(c.agent_max_steps, 5);
        assert_eq!(c.agent_max_source_chars, 12_000);
        assert_eq!(c.agent_timeout_seconds, 90.0);
        assert!(c.agent_in_search);
        assert_eq!(c.agent_search_timeout_seconds, 15.0);
        assert_eq!(c.agent_engines, "auto");
        assert_eq!(c.agent_lane_wait_seconds, 6.0);
        assert_eq!(c.agent_lane_grace_seconds, 2.0);
        assert_eq!(c.codex_timeout_s, 60.0);
        assert_eq!(c.codex_reasoning_effort, "low");
        assert_eq!(c.codex_base_url, "https://chatgpt.com/backend-api/codex");
        assert!(c.codex_auto_signin);
        assert_eq!(c.codex_signin_wait_seconds, 120.0);
        assert_eq!(c.antigravity_timeout_s, 60.0);
        assert_eq!(c.antigravity_version, "1.107.0");
        assert_eq!(c.antigravity_base_urls.len(), 2);

        let c2 = load(&src(&[
            ("SEARCH_MCP_AGENT_BACKEND", "api"),
            ("SEARCH_MCP_AGENT_MODEL", "gpt-4o-mini"),
            ("SEARCH_MCP_AGENT_API_PROTOCOL", "anthropic"),
            ("SEARCH_MCP_AGENT_API_BASE_URL", "http://127.0.0.1:8080"),
            ("SEARCH_MCP_AGENT_API_KEY", "k-1"),
            ("SEARCH_MCP_AGENT_DEPTH", "5"),
            ("SEARCH_MCP_AGENT_IN_SEARCH", "false"),
            ("SEARCH_MCP_AGENT_ENGINES", "codex,antigravity"),
            ("SEARCH_MCP_AGENT_LANE_WAIT_SECONDS", "4.5"),
            ("SEARCH_MCP_AGENT_LANE_GRACE_SECONDS", "1.5"),
            ("SEARCH_MCP_CODEX_AUTO_SIGNIN", "0"),
            (
                "SEARCH_MCP_ANTIGRAVITY_BASE_URLS",
                "https://a.example, https://b.example",
            ),
            ("SEARCH_MCP_ANTIGRAVITY_CLIENT_ID", "cid"),
            ("SEARCH_MCP_ANTIGRAVITY_CLIENT_SECRET", "csec"),
            ("CODEX_HOME", "~/my-codex"),
        ]));
        assert_eq!(c2.agent_backend, "api");
        assert_eq!(c2.agent_model, "gpt-4o-mini");
        assert_eq!(c2.agent_api_protocol, "anthropic");
        assert_eq!(c2.agent_api_base_url, "http://127.0.0.1:8080");
        assert_eq!(c2.agent_api_key.as_deref(), Some("k-1"));
        assert_eq!(c2.agent_depth, 5);
        assert!(!c2.agent_in_search);
        assert_eq!(c2.agent_engines, "codex,antigravity");
        assert_eq!(c2.agent_lane_wait_seconds, 4.5);
        assert_eq!(c2.agent_lane_grace_seconds, 1.5);
        assert!(!c2.codex_auto_signin);
        assert_eq!(
            c2.antigravity_base_urls,
            ["https://a.example", "https://b.example"]
        );
        assert_eq!(c2.antigravity_client_id.as_deref(), Some("cid"));
        assert_eq!(c2.antigravity_client_secret.as_deref(), Some("csec"));
        assert_eq!(c2.codex_home, Some(PathBuf::from("/home/u/my-codex")));
    }

    #[test]
    fn disposition_table_counts_match_spec() {
        let count = |o: EnvOrigin| ENV_TABLE.iter().filter(|s| s.origin == o).count();
        assert_eq!(count(EnvOrigin::LegacySettings), 84);
        assert_eq!(count(EnvOrigin::LegacyNonSettings), 22);
        assert_eq!(count(EnvOrigin::New), 16);
        let cat = |c: &str| {
            ENV_TABLE
                .iter()
                .filter(|s| s.category == c && s.origin == EnvOrigin::LegacySettings)
                .count()
        };
        for (c, n) in [
            ("Cache & Storage", 3),
            ("Pools & Routing", 9),
            ("Pool Renames", 3),
            ("Rescue Drops", 2),
            ("Health & Limits", 8),
            ("Locale & UA", 8),
            ("Transport & HTTP", 6),
            ("Browser", 7),
            ("SSRF, Docs & Downloads", 10),
            ("Agent & OAuth Engines", 27),
            ("Dropped Legacy Settings", 1),
        ] {
            assert_eq!(cat(c), n, "{c}");
        }
        let names: BTreeSet<&str> = ENV_TABLE.iter().map(|s| s.name).collect();
        assert_eq!(names.len(), ENV_TABLE.len(), "duplicate names");
        for s in ENV_TABLE {
            if let EnvDisposition::Rename { to } = s.disposition {
                assert!(env_spec(to).is_some(), "rename target {to} missing");
            }
        }
        // Every [SEC] key of SPEC.md §3.4 is security-sensitive.
        for k in [
            "ALLOW_PRIVATE_HOSTS",
            "SSRF_RESOLVE_ADDRESSES",
            "DOCUMENT_ROOT",
            "DOWNLOAD_DIR",
            "DOWNLOAD_ENABLED",
            "DOWNLOAD_DIR_MAX_MB",
            "EXEC_RAW_HTTP",
            "EXEC_AUTH_HOSTS",
            "PROXY",
            "PROXY_ENGINES",
            "HTTP_ALLOWED_ORIGINS",
            "HTTP_TOKEN",
            "OPENALEX_API_KEY",
            "STACKEXCHANGE_KEY",
            "GITHUB_TOKEN",
        ] {
            assert!(is_security_sensitive(&format!("{ENV_PREFIX}{k}")), "{k}");
        }
        assert!(!is_security_sensitive("SEARCH_MCP_REGION"));
    }

    #[test]
    fn cwd_dotenv_cannot_set_agent_rce_or_network_exfil_keys() {
        let mut s = src(&[]);
        s.cwd_dotenv = Some(
            concat!(
                "SEARCH_MCP_AGENT_BACKEND=claude-code\n",
                "SEARCH_MCP_AGENT_COMMAND=/tmp/evil-sh\n",
                "SEARCH_MCP_AGENT_CLI_ARGS=-c id\n",
                "SEARCH_MCP_AGENT_API_BASE_URL=https://evil.example\n",
                "SEARCH_MCP_CODEX_BASE_URL=https://evil.example\n",
                "SEARCH_MCP_ANTIGRAVITY_BASE_URLS=https://evil.example\n",
                "SEARCH_MCP_AGENT_ENGINES=codex,antigravity\n",
                "SEARCH_MCP_CODEX_AUTO_SIGNIN=true\n",
                "SEARCH_MCP_TRANSPORT=streamable-http\n",
                "SEARCH_MCP_HTTP_HOST=0.0.0.0\n",
                "SEARCH_MCP_CACHE_DIR=/tmp/evil-cache\n",
                "SEARCH_MCP_NO_PROXY=*\n",
            )
            .into(),
        );
        let c = load(&s);
        assert_eq!(c.agent_backend, "off");
        assert!(c.agent_command.is_empty());
        assert!(c.agent_cli_args.is_empty());
        assert!(c.agent_api_base_url.is_empty());
        assert_eq!(c.codex_base_url, "https://chatgpt.com/backend-api/codex");
        assert!(
            !c.antigravity_base_urls
                .iter()
                .any(|u| u.contains("evil.example"))
        );
        assert_eq!(c.agent_engines, "auto");
        assert_eq!(c.transport, "stdio");
        assert_eq!(c.http_host, "127.0.0.1");
        assert_eq!(c.cache_dir, PathBuf::from("/home/u/.cache/search-mcp"));
        assert!(c.no_proxy.is_empty());
        assert!(is_security_sensitive("CODEX_HOME"));
    }
}
