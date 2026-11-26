//! OAuth 2.0 PKCE sign-in, token store, refresh locking, and provider specs
//! for `codex` (ChatGPT/Codex) and `antigravity` (Google Cloud Code Assist)
//! (port of `src/search_mcp/oauth.py`).
//!
//! Credentials are stored in `<config_dir>/oauth/<provider>.json` with `0600`
//! permissions (`0700` directory) and are NEVER logged or exposed in status output.

use crate::keystore;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use fsm_core::Config;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use rand::RngExt;
use regex::bytes::Regex as BytesRegex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Mutex as AsyncMutex, Notify};

/// Percent-encoding set matching RFC 3986 unreserved (`quote_via=quote`, spaces as `%20`, never `+`).
const RFC3986_UNRESERVED: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Known SHA-256 hashes of Antigravity's installed OAuth client ID and secret.
/// The repository NEVER embeds any raw Google client ID or secret literal.
pub const ANTIGRAVITY_CLIENT_SHA256: (&str, &str) = (
    "bf00c418024ba6bf606ccdc37120976e41bc429dd1d46ecf16a729aa532626ea",
    "1d2f041093fd95aa8995a038c711d50a7960da09a505381c09a745d6ad0ecc60",
);

/// Account-risk notice shown before an Antigravity sign-in.
pub const ANTIGRAVITY_WARNING: &str = "\
Warning: Antigravity uses Google's internal Cloud Code Assist endpoint outside the \
Antigravity app. Google has suspended accounts that do this — use a secondary account \
if you want to try it, and accept the risk of suspension before continuing.";

/// Refresh margin in seconds before expiry.
pub const REFRESH_MARGIN_SECS: f64 = 60.0;

/// Default wait for the browser callback tab to show the token-exchange result.
pub const DEFAULT_REPLY_WAIT_SECS: f64 = 15.0;

/// Structured OAuth error variants matching Python `OAuthError`, `NotSignedIn`,
/// `SignInPending`, `OAuthUnreachable`.
#[derive(Debug, Clone, Error)]
pub enum OAuthError {
    #[error("{0}")]
    NotSignedIn(String),
    #[error("{0}")]
    SignInPending(String),
    #[error("{0}")]
    Unreachable(String),
    #[error("{0}")]
    Failed(String),
}

impl OAuthError {
    pub fn is_not_signed_in(&self) -> bool {
        matches!(self, Self::NotSignedIn(_))
    }

    pub fn is_pending(&self) -> bool {
        matches!(self, Self::SignInPending(_))
    }

    pub fn is_unreachable(&self) -> bool {
        matches!(self, Self::Unreachable(_))
    }
}

/// Static specification of an OAuth 2.0 PKCE provider.
#[derive(Debug, Clone)]
pub struct OAuthProvider {
    pub id: &'static str,
    pub label: &'static str,
    pub engine: &'static str,
    pub authorize_url: String,
    pub token_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub scopes: Vec<String>,
    pub redirect_ports: Vec<u16>,
    pub redirect_path: String,
    pub redirect_host: String,
    pub extra_authorize_params: Vec<(String, String)>,
    pub json_refresh: bool,
}

impl OAuthProvider {
    pub fn codex() -> Self {
        Self {
            id: "codex",
            label: "ChatGPT / Codex",
            engine: "codex",
            authorize_url: "https://auth.openai.com/oauth/authorize".to_owned(),
            token_url: "https://auth.openai.com/oauth/token".to_owned(),
            client_id: "app_EMoamEEZ73f0CkXaXp7hrann".to_owned(),
            client_secret: String::new(),
            scopes: vec![
                "openid".to_owned(),
                "profile".to_owned(),
                "email".to_owned(),
                "offline_access".to_owned(),
            ],
            redirect_ports: vec![1455, 1457],
            redirect_path: "/auth/callback".to_owned(),
            redirect_host: "127.0.0.1".to_owned(),
            extra_authorize_params: vec![
                ("id_token_add_organizations".to_owned(), "true".to_owned()),
                ("codex_cli_simplified_flow".to_owned(), "true".to_owned()),
                ("originator".to_owned(), "codex_cli_rs".to_owned()),
            ],
            json_refresh: true,
        }
    }

    pub fn antigravity() -> Self {
        Self {
            id: "antigravity",
            label: "Google Antigravity",
            engine: "antigravity",
            authorize_url: "https://accounts.google.com/o/oauth2/v2/auth".to_owned(),
            token_url: "https://oauth2.googleapis.com/token".to_owned(),
            client_id: String::new(),
            client_secret: String::new(),
            scopes: vec![
                "https://www.googleapis.com/auth/cloud-platform".to_owned(),
                "https://www.googleapis.com/auth/userinfo.email".to_owned(),
                "https://www.googleapis.com/auth/userinfo.profile".to_owned(),
                "https://www.googleapis.com/auth/cclog".to_owned(),
                "https://www.googleapis.com/auth/experimentsandconfigs".to_owned(),
            ],
            redirect_ports: vec![51121],
            redirect_path: "/oauth-callback".to_owned(),
            redirect_host: "localhost".to_owned(),
            extra_authorize_params: vec![
                ("access_type".to_owned(), "offline".to_owned()),
                ("prompt".to_owned(), "consent".to_owned()),
            ],
            json_refresh: false,
        }
    }

    pub fn by_id(id: &str) -> Result<Self, OAuthError> {
        match id {
            "codex" => Ok(Self::codex()),
            "antigravity" => Ok(Self::antigravity()),
            other => Err(OAuthError::Failed(format!(
                "unknown provider {other:?}; use codex, antigravity"
            ))),
        }
    }

    pub fn redirect_uri(&self, port: u16) -> String {
        format!(
            "http://{}:{}{}",
            self.redirect_host, port, self.redirect_path
        )
    }

    /// Build the authorize link with spaces encoded as `%20` (never `+`).
    pub fn authorize_link(&self, challenge: &str, state: &str, redirect_uri: &str) -> String {
        let mut params: Vec<(&str, String)> = vec![
            ("response_type", "code".to_owned()),
            ("client_id", self.client_id.clone()),
            ("redirect_uri", redirect_uri.to_owned()),
            ("scope", self.scopes.join(" ")),
            ("code_challenge", challenge.to_owned()),
            ("code_challenge_method", "S256".to_owned()),
            ("state", state.to_owned()),
        ];
        for (k, v) in &self.extra_authorize_params {
            params.push((k.as_str(), v.clone()));
        }
        let query = params
            .into_iter()
            .map(|(k, v)| {
                format!(
                    "{}={}",
                    utf8_percent_encode(k, RFC3986_UNRESERVED),
                    utf8_percent_encode(&v, RFC3986_UNRESERVED)
                )
            })
            .collect::<Vec<_>>()
            .join("&");
        format!("{}?{query}", self.authorize_url)
    }
}

/// Stored or linked OAuth credential. `Debug` intentionally redacts secrets.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Credential {
    pub provider: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub access_token: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub refresh_token: String,
    #[serde(default)]
    pub expires_at: f64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub account_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub email: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub plan: String,
    #[serde(default = "default_source")]
    pub source: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub linked_path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub client_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub client_secret: String,
    #[serde(default, skip_serializing)]
    pub id_token: String,
}

fn default_source() -> String {
    "oauth".to_owned()
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("provider", &self.provider)
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .field("account_id", &self.account_id)
            .field("email", &self.email)
            .field("plan", &self.plan)
            .field("source", &self.source)
            .field("linked_path", &self.linked_path)
            .field("client_secret", &"<redacted>")
            .finish()
    }
}

impl Credential {
    pub fn is_fresh(&self, margin_secs: f64) -> bool {
        !self.access_token.is_empty() && self.expires_at > now_unix() + margin_secs
    }
}

/// Public (token-free) sign-in status summary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OAuthStatus {
    pub provider: String,
    pub label: String,
    pub engine: String,
    pub signed_in: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expired: Option<bool>,
}

pub fn now_unix() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Generate a PKCE S256 `(verifier, challenge)` pair.
pub fn pkce_pair() -> (String, String) {
    let mut bytes = [0u8; 64];
    rand::rng().fill(&mut bytes);
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(digest);
    (verifier, challenge)
}

/// Generate a random OAuth `state` token.
pub fn random_state() -> String {
    let mut bytes = [0u8; 24];
    rand::rng().fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Decode the payload segment of a JWT without verifying its signature.
/// Tolerates malformed or non-JWT tokens by returning an empty JSON object.
pub fn jwt_claims(token: &str) -> Value {
    let mut parts = token.split('.');
    let _header = parts.next();
    let Some(payload) = parts.next() else {
        return json!({});
    };
    if parts.next().is_none() || payload.is_empty() {
        return json!({});
    }
    let Ok(decoded) = URL_SAFE_NO_PAD.decode(payload) else {
        return json!({});
    };
    match serde_json::from_slice::<Value>(&decoded) {
        Ok(v @ Value::Object(_)) => v,
        _ => json!({}),
    }
}

/// Populate `email`, `account_id`, `plan`, and `expires_at` from Codex JWT claims.
pub fn apply_codex_claims(cred: &mut Credential, id_token: &str) {
    let id_claims = jwt_claims(id_token);
    let access_claims = jwt_claims(&cred.access_token);

    if let Some(exp) = access_claims.get("exp").and_then(Value::as_f64)
        && exp > 0.0
    {
        cred.expires_at = exp;
    }

    let auth = id_claims
        .get("https://api.openai.com/auth")
        .or_else(|| access_claims.get("https://api.openai.com/auth"));
    let profile = id_claims
        .get("https://api.openai.com/profile")
        .or_else(|| access_claims.get("https://api.openai.com/profile"));

    if let Some(email) = id_claims
        .get("email")
        .and_then(Value::as_str)
        .or_else(|| profile.and_then(|p| p.get("email")).and_then(Value::as_str))
        .or_else(|| access_claims.get("email").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
    {
        cred.email = email.to_owned();
    }

    if let Some(acct) = auth
        .and_then(|a| a.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            id_claims
                .get("organizations")
                .and_then(Value::as_array)
                .and_then(|arr| arr.first())
                .and_then(|o| o.get("id"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
    {
        cred.account_id = acct.to_owned();
    }

    if let Some(plan) = auth
        .and_then(|a| a.get("chatgpt_plan_type"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        cred.plan = plan.to_owned();
    }
}

/// True when `returned` state starts with the non-empty `expected` state
/// (ChatGPT may append an onboarding suffix).
pub fn state_matches(returned: &str, expected: &str) -> bool {
    !expected.is_empty() && returned.starts_with(expected)
}

/// Validate a redirect query parameter map and extract the authorization `code`.
pub fn read_redirect(
    params: &BTreeMap<String, Vec<String>>,
    expected_state: &str,
) -> Result<String, OAuthError> {
    let first = |k: &str| {
        params
            .get(k)
            .and_then(|v| v.first())
            .map(String::as_str)
            .unwrap_or("")
    };
    let returned_state = first("state");
    if !state_matches(returned_state, expected_state) {
        return Err(OAuthError::Failed(
            "OAuth state mismatch; ignoring unexpected callback".to_owned(),
        ));
    }
    let err = first("error");
    if !err.is_empty() {
        let desc = first("error_description");
        if desc.contains("missing_codex_entitlement") {
            return Err(OAuthError::Failed(
                "this ChatGPT account/workspace does not include Codex (missing_codex_entitlement)"
                    .to_owned(),
            ));
        }
        let detail = if desc.is_empty() {
            err.to_owned()
        } else {
            format!("{err}: {desc}")
        };
        return Err(OAuthError::Failed(format!(
            "the sign-in was not completed: {detail}"
        )));
    }
    let code = first("code");
    if code.is_empty() {
        return Err(OAuthError::Failed(
            "the redirect had no authorization code".to_owned(),
        ));
    }
    Ok(code.to_owned())
}

/// Parse a pasted redirect URL or query string (`?code=...&state=...`).
pub fn parse_pasted(raw: &str, expected_state: &str) -> Result<String, OAuthError> {
    let trimmed = raw.trim();
    let query_part = if let Some(q) = trimmed.strip_prefix('?') {
        q.to_owned()
    } else if let Ok(parsed) = url::Url::parse(trimmed) {
        parsed.query().unwrap_or("").to_owned()
    } else if let Some((_, q)) = trimmed.split_once('?') {
        q.to_owned()
    } else {
        String::new()
    };

    if query_part.is_empty() || !query_part.contains("code=") {
        return Err(OAuthError::Failed(
            "paste the full redirect URL containing ?code=...&state=...".to_owned(),
        ));
    }

    let mut params: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (k, v) in url::form_urlencoded::parse(query_part.as_bytes()) {
        params
            .entry(k.into_owned())
            .or_default()
            .push(v.into_owned());
    }
    read_redirect(&params, expected_state)
}

/// Return true iff `provider_id` is a safe single-segment identifier (`^[a-z0-9_-]{1,64}$`).
pub fn is_valid_provider_id(provider_id: &str) -> bool {
    !provider_id.is_empty()
        && provider_id.len() <= 64
        && provider_id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// Path to `<config_dir>/oauth/<provider>.json`.
///
/// Invalid `provider_id` values are sanitized to a single flat filename so callers
/// can never traverse outside `<config_dir>/oauth`.
pub fn credential_path(config_dir: &Path, provider_id: &str) -> PathBuf {
    let safe_stem = if is_valid_provider_id(provider_id) {
        provider_id.to_owned()
    } else {
        let mut s: String = provider_id
            .bytes()
            .take(48)
            .map(|b| {
                if b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-' {
                    b as char
                } else {
                    '_'
                }
            })
            .collect();
        if s.is_empty() {
            s.push_str("empty");
        }
        format!("_invalid_{s}")
    };
    config_dir.join("oauth").join(format!("{safe_stem}.json"))
}

/// Path to Codex CLI's `auth.json` (`$CODEX_HOME/auth.json` or `~/.codex/auth.json`).
pub fn codex_cli_auth_path(codex_home: Option<&Path>) -> PathBuf {
    if let Some(dir) = codex_home {
        return dir.join("auth.json");
    }
    if let Ok(env_home) = std::env::var("CODEX_HOME") {
        let trimmed = env_home.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed).join("auth.json");
        }
    }
    fsm_core::config::system_home()
        .unwrap_or_else(|| std::env::temp_dir().join("codex"))
        .join(".codex")
        .join("auth.json")
}

/// Persist a credential privately (`0700` dir, `0600` file).
///
/// For `"codex-cli"` links, no tokens are stored in our file — only the link metadata.
pub fn save(config_dir: &Path, cred: &Credential) -> std::io::Result<()> {
    if !is_valid_provider_id(&cred.provider) {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            format!("invalid OAuth provider_id {:?}", cred.provider),
        ));
    }
    let path = credential_path(config_dir, &cred.provider);
    let mut to_store = cred.clone();
    to_store.id_token.clear();
    if to_store.source == "codex-cli" {
        to_store.access_token.clear();
        to_store.refresh_token.clear();
    }
    let bytes = serde_json::to_vec_pretty(&to_store)
        .map_err(|e| std::io::Error::new(ErrorKind::InvalidData, e))?;
    keystore::write_private_atomic(&path, &bytes)?;
    clear_signed_in_cache(&path);
    Ok(())
}

/// Read tokens read-only from a Codex CLI `auth.json`.
fn read_codex_cli_auth(path: &Path) -> Result<Credential, OAuthError> {
    let raw = fs::read_to_string(path).map_err(|_| {
        OAuthError::Failed(format!(
            "Codex CLI sign-in not found at {}. Run `codex login` first, or sign in directly with `free-search-mcp login codex`.",
            path.display()
        ))
    })?;
    let val: Value = serde_json::from_str(&raw).map_err(|e| {
        OAuthError::Failed(format!(
            "Codex CLI auth file {} could not be parsed: {e}",
            path.display()
        ))
    })?;
    let auth_mode = val
        .get("auth_mode")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    if auth_mode == "apikey" || auth_mode == "api_key" {
        return Err(OAuthError::Failed(format!(
            "{} uses an API key (auth_mode={auth_mode:?}) and has no ChatGPT sign-in; run `codex login` with ChatGPT or `free-search-mcp login codex`.",
            path.display()
        )));
    }
    let Some(tokens) = val.get("tokens").and_then(Value::as_object) else {
        return Err(OAuthError::Failed(format!(
            "{} has no ChatGPT sign-in tokens; run `codex login` first.",
            path.display()
        )));
    };
    let access_token = tokens
        .get("access_token")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    let refresh_token = tokens
        .get("refresh_token")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    let id_token = tokens
        .get("id_token")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    let account_id = tokens
        .get("account_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();

    if access_token.is_empty() {
        return Err(OAuthError::Failed(format!(
            "{} has no ChatGPT sign-in access_token; run `codex login` first.",
            path.display()
        )));
    }

    let mut cred = Credential {
        provider: "codex".to_owned(),
        access_token,
        refresh_token,
        account_id,
        source: "codex-cli".to_owned(),
        linked_path: path.to_string_lossy().into_owned(),
        ..Credential::default()
    };
    apply_codex_claims(&mut cred, &id_token);
    Ok(cred)
}

/// Link read-only to an existing Codex CLI sign-in (`auth.json`). Never writes to `auth.json`.
pub fn link_codex_cli(
    config_dir: &Path,
    auth_path: Option<&Path>,
    codex_home: Option<&Path>,
) -> Result<Credential, OAuthError> {
    let target = match auth_path {
        Some(p) => p.to_path_buf(),
        None => codex_cli_auth_path(codex_home),
    };
    let cred = read_codex_cli_auth(&target)?;
    save(config_dir, &cred).map_err(|e| OAuthError::Failed(format!("failed to save link: {e}")))?;
    Ok(cred)
}

/// Load a stored (or linked Codex CLI) credential. Corrupt files read as `None`.
pub fn load(config_dir: &Path, provider_id: &str, codex_home: Option<&Path>) -> Option<Credential> {
    if !is_valid_provider_id(provider_id) {
        return None;
    }
    let path = credential_path(config_dir, provider_id);
    let raw = fs::read_to_string(&path).ok()?;
    let mut cred: Credential = serde_json::from_str(&raw).ok()?;
    if cred.provider.is_empty() {
        cred.provider = provider_id.to_owned();
    }
    if cred.source == "codex-cli" {
        let linked = if !cred.linked_path.is_empty() {
            PathBuf::from(&cred.linked_path)
        } else {
            codex_cli_auth_path(codex_home)
        };
        return read_codex_cli_auth(&linked).ok();
    }
    if cred.access_token.is_empty() && cred.refresh_token.is_empty() {
        return None;
    }
    Some(cred)
}

/// Delete the stored credential for `provider_id`. Returns true if a file was removed.
pub fn sign_out(config_dir: &Path, provider_id: &str) -> bool {
    if !is_valid_provider_id(provider_id) {
        return false;
    }
    let path = credential_path(config_dir, provider_id);
    clear_signed_in_cache(&path);
    fs::remove_file(&path).is_ok()
}

type FileStamp = (Option<SystemTime>, u64);

#[derive(Clone)]
struct SignedInMemo {
    cred_stamp: Option<FileStamp>,
    linked_path: Option<PathBuf>,
    linked_stamp: Option<FileStamp>,
    signed_in: bool,
}

static SIGNED_IN_CACHE: LazyLock<Mutex<BTreeMap<PathBuf, SignedInMemo>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

fn stat_stamp(path: &Path) -> Option<FileStamp> {
    let meta = fs::metadata(path).ok()?;
    Some((meta.modified().ok(), meta.len()))
}

fn clear_signed_in_cache(path: &Path) {
    if let Ok(mut guard) = SIGNED_IN_CACHE.lock() {
        guard.remove(path);
    }
}

/// File-stamp memoized check of whether `provider_id` is currently signed in.
/// Tracks changes to both our credential file and any linked Codex CLI `auth.json`.
pub fn is_signed_in(config_dir: &Path, provider_id: &str, codex_home: Option<&Path>) -> bool {
    if !is_valid_provider_id(provider_id) {
        return false;
    }
    let path = credential_path(config_dir, provider_id);
    let cred_stamp = stat_stamp(&path);
    if cred_stamp.is_none() {
        clear_signed_in_cache(&path);
        return false;
    }

    if let Ok(guard) = SIGNED_IN_CACHE.lock()
        && let Some(memo) = guard.get(&path)
        && memo.cred_stamp == cred_stamp
    {
        let linked_ok = match &memo.linked_path {
            Some(lp) => stat_stamp(lp) == memo.linked_stamp,
            None => true,
        };
        if linked_ok {
            return memo.signed_in;
        }
    }

    let raw = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let parsed: Credential = match serde_json::from_str(&raw) {
        Ok(c) => c,
        Err(_) => return false,
    };

    let (linked_path, linked_stamp, signed_in) = if parsed.source == "codex-cli" {
        let lp = if !parsed.linked_path.is_empty() {
            PathBuf::from(&parsed.linked_path)
        } else {
            codex_cli_auth_path(codex_home)
        };
        let ls = stat_stamp(&lp);
        let ok = read_codex_cli_auth(&lp).is_ok();
        (Some(lp), ls, ok)
    } else {
        let ok = !parsed.access_token.is_empty() || !parsed.refresh_token.is_empty();
        (None, None, ok)
    };

    if let Ok(mut guard) = SIGNED_IN_CACHE.lock() {
        guard.insert(
            path,
            SignedInMemo {
                cred_stamp,
                linked_path,
                linked_stamp,
                signed_in,
            },
        );
    }
    signed_in
}

/// Public status for `provider_id` (NEVER includes tokens or client secrets).
pub fn status(
    config_dir: &Path,
    provider_id: &str,
    codex_home: Option<&Path>,
) -> Result<OAuthStatus, OAuthError> {
    let spec = OAuthProvider::by_id(provider_id)?;
    let Some(cred) = load(config_dir, provider_id, codex_home) else {
        return Ok(OAuthStatus {
            provider: spec.id.to_owned(),
            label: spec.label.to_owned(),
            engine: spec.engine.to_owned(),
            signed_in: false,
            email: None,
            plan: None,
            account_id: None,
            source: None,
            expires_at: None,
            expired: None,
        });
    };
    let expired = cred.expires_at > 0.0 && cred.expires_at <= now_unix();
    Ok(OAuthStatus {
        provider: spec.id.to_owned(),
        label: spec.label.to_owned(),
        engine: spec.engine.to_owned(),
        signed_in: true,
        email: (!cred.email.is_empty()).then_some(cred.email),
        plan: (!cred.plan.is_empty()).then_some(cred.plan),
        account_id: (!cred.account_id.is_empty()).then_some(cred.account_id),
        source: Some(cred.source),
        expires_at: (cred.expires_at > 0.0).then_some(cred.expires_at),
        expired: Some(expired),
    })
}

// ---------------------------------------------------------------------------
// Antigravity client resolution & headers
// ---------------------------------------------------------------------------

static GOOGLE_CLIENT_ID_RE: LazyLock<BytesRegex> = LazyLock::new(|| {
    BytesRegex::new(r"[0-9]{10,14}-[a-z0-9]{20,40}\.apps\.googleusercontent\.com")
        .expect("google client id regex")
});

static GOOGLE_CLIENT_SECRET_RE: LazyLock<BytesRegex> = LazyLock::new(|| {
    BytesRegex::new(r"GOCSPX-[A-Za-z0-9_-]{20,40}").expect("google client secret regex")
});

/// Default local install paths where Antigravity's language server binary may live.
pub fn antigravity_install_paths() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let home = fsm_core::config::system_home();

    #[cfg(target_os = "macos")]
    {
        candidates.push(PathBuf::from(
            "/Applications/Antigravity.app/Contents/Resources/app/extensions/antigravity/bin/language_server_macos_arm",
        ));
        candidates.push(PathBuf::from(
            "/Applications/Antigravity.app/Contents/Resources/app/extensions/antigravity/bin/language_server_macos_x64",
        ));
        if let Some(h) = &home {
            candidates.push(h.join(
                "Applications/Antigravity.app/Contents/Resources/app/extensions/antigravity/bin/language_server_macos_arm",
            ));
            candidates.push(h.join(
                "Applications/Antigravity.app/Contents/Resources/app/extensions/antigravity/bin/language_server_macos_x64",
            ));
        }
    }

    #[cfg(target_os = "linux")]
    {
        for base in [
            PathBuf::from("/usr/share/antigravity"),
            PathBuf::from("/opt/antigravity"),
        ] {
            candidates.push(
                base.join("resources/app/extensions/antigravity/bin/language_server_linux_x64"),
            );
            candidates.push(
                base.join("resources/app/extensions/antigravity/bin/language_server_linux_arm"),
            );
        }
        if let Some(h) = &home {
            candidates.push(h.join(".local/share/antigravity/resources/app/extensions/antigravity/bin/language_server_linux_x64"));
            candidates.push(h.join(".local/share/antigravity/resources/app/extensions/antigravity/bin/language_server_linux_arm"));
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            let base = PathBuf::from(local)
                .join("Programs/Antigravity/resources/app/extensions/antigravity/bin");
            candidates.push(base.join("language_server_windows_x64.exe"));
        }
    }

    let _ = home;
    candidates
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Resolve the Antigravity OAuth `(client_id, client_secret)` pair.
///
/// Precedence: env/config/keystore > scanning local Antigravity install binaries by SHA-256.
pub fn antigravity_client(
    cfg: Option<&Config>,
    config_dir: &Path,
    install_paths_override: Option<&[PathBuf]>,
    expected_sha256_override: Option<(&str, &str)>,
) -> Result<(String, String), OAuthError> {
    let cid = cfg
        .and_then(|c| c.antigravity_client_id.clone())
        .or_else(|| keystore::get_secret("antigravity_client_id", config_dir));
    let csec = cfg
        .and_then(|c| c.antigravity_client_secret.clone())
        .or_else(|| keystore::get_secret("antigravity_client_secret", config_dir));

    match (cid, csec) {
        (Some(id), Some(sec)) if !id.is_empty() && !sec.is_empty() => return Ok((id, sec)),
        (Some(_), None) | (None, Some(_)) => {
            return Err(OAuthError::Failed(
                "only one of SEARCH_MCP_ANTIGRAVITY_CLIENT_ID and SEARCH_MCP_ANTIGRAVITY_CLIENT_SECRET is set; set both or neither".to_owned(),
            ));
        }
        _ => {}
    }

    let (want_id_sha, want_sec_sha) = expected_sha256_override.unwrap_or(ANTIGRAVITY_CLIENT_SHA256);
    let paths: Vec<PathBuf> = match install_paths_override {
        Some(p) => p.to_vec(),
        None => antigravity_install_paths(),
    };

    let mut unreadable: Vec<PathBuf> = Vec::new();
    let mut readable_without_match: Vec<PathBuf> = Vec::new();

    for path in paths {
        if !path.exists() {
            continue;
        }
        let Ok(data) = fs::read(&path) else {
            unreadable.push(path);
            continue;
        };

        let mut found_id: Option<String> = None;
        for m in GOOGLE_CLIENT_ID_RE.find_iter(&data) {
            let raw = m.as_bytes();
            // Digits from adjacent binary strings can butt directly against the client ID;
            // test suffix slices from 10..=14 leading digits if full match hash differs.
            if sha256_hex(raw) == want_id_sha {
                found_id = Some(String::from_utf8_lossy(raw).into_owned());
                break;
            }
            if let Some(dash) = raw.iter().position(|&b| b == b'-') {
                for digits in 10..=dash.min(14) {
                    let start = dash - digits;
                    let slice = &raw[start..];
                    if sha256_hex(slice) == want_id_sha {
                        found_id = Some(String::from_utf8_lossy(slice).into_owned());
                        break;
                    }
                }
                if found_id.is_some() {
                    break;
                }
            }
        }

        let mut found_sec: Option<String> = None;
        for m in GOOGLE_CLIENT_SECRET_RE.find_iter(&data) {
            let raw = m.as_bytes();
            if sha256_hex(raw) == want_sec_sha {
                found_sec = Some(String::from_utf8_lossy(raw).into_owned());
                break;
            }
            // Adjacent secrets can be concatenated back to back (e.g. GOCSPX-...GOCSPX-...).
            if raw.len() > 35 {
                let prefix35 = &raw[..35];
                if sha256_hex(prefix35) == want_sec_sha {
                    found_sec = Some(String::from_utf8_lossy(prefix35).into_owned());
                    break;
                }
            }
        }

        if let (Some(id), Some(sec)) = (found_id, found_sec) {
            return Ok((id, sec));
        }
        readable_without_match.push(path);
    }

    if !readable_without_match.is_empty() {
        let listed = readable_without_match
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(OAuthError::Failed(format!(
            "Antigravity install found at {listed}, but it did not contain the expected OAuth client (a newer release may have changed it). Set SEARCH_MCP_ANTIGRAVITY_CLIENT_ID and SEARCH_MCP_ANTIGRAVITY_CLIENT_SECRET."
        )));
    }
    if !unreadable.is_empty() {
        let listed = unreadable
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(OAuthError::Failed(format!(
            "Antigravity install at {listed} could not be read; set SEARCH_MCP_ANTIGRAVITY_CLIENT_ID and SEARCH_MCP_ANTIGRAVITY_CLIENT_SECRET."
        )));
    }
    Err(OAuthError::Failed(
        "no Antigravity install was found on this machine; install Antigravity or set SEARCH_MCP_ANTIGRAVITY_CLIENT_ID and SEARCH_MCP_ANTIGRAVITY_CLIENT_SECRET.".to_owned(),
    ))
}

/// Platform-specific Antigravity `User-Agent`, `Client-Metadata`, and `Authorization` headers.
pub fn antigravity_headers_for(
    token: &str,
    version: &str,
    platform: &str,
    arch: &str,
) -> BTreeMap<String, String> {
    let os_norm = match platform {
        "darwin" | "macos" => "darwin",
        "win32" | "windows" => "windows",
        _ => "linux",
    };
    let arch_lower = arch.to_ascii_lowercase();
    let arch_norm = if arch_lower.contains("arm") || arch_lower.contains("aarch64") {
        "arm64"
    } else {
        "amd64"
    };
    let meta_platform = format!(
        "{}_{}",
        os_norm.to_ascii_uppercase(),
        arch_norm.to_ascii_uppercase()
    );
    let metadata = json!({
        "ideType": "ANTIGRAVITY",
        "platform": meta_platform,
        "pluginType": "GEMINI",
    })
    .to_string();

    let mut headers = BTreeMap::new();
    headers.insert("Authorization".to_owned(), format!("Bearer {token}"));
    headers.insert(
        "User-Agent".to_owned(),
        format!("antigravity/{version} {os_norm}/{arch_norm}"),
    );
    headers.insert("Client-Metadata".to_owned(), metadata);
    headers.insert("Content-Type".to_owned(), "application/json".to_owned());
    headers
}

pub fn antigravity_headers(token: &str, version: &str) -> BTreeMap<String, String> {
    antigravity_headers_for(token, version, std::env::consts::OS, std::env::consts::ARCH)
}

/// Call Cloud Code Assist `:loadCodeAssist` to discover project ID and tier name.
pub async fn load_code_assist(
    http: &reqwest::Client,
    base_urls: &[String],
    token: &str,
    version: &str,
) -> Option<(String, String)> {
    let headers = antigravity_headers(token, version);
    let body = json!({
        "metadata": {
            "ideType": "ANTIGRAVITY",
            "pluginType": "GEMINI",
        }
    });

    for base in base_urls {
        let url = format!("{}/v1internal:loadCodeAssist", base.trim_end_matches('/'));
        let mut req = http.post(&url).json(&body);
        for (k, v) in &headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let Ok(resp) = req.send().await else {
            continue;
        };
        if !resp.status().is_success() {
            continue;
        }
        let Ok(val) = resp.json::<Value>().await else {
            continue;
        };
        let project = val
            .get("cloudaicompanionProject")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        let tier = val
            .get("paidTier")
            .and_then(|t| t.get("name"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                val.get("currentTier")
                    .and_then(|t| t.get("name"))
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or("")
            .to_owned();
        if !project.is_empty() || !tier.is_empty() {
            return Some((project, tier));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Browser detection & detached spawn (never inherits MCP stdio streams!)
// ---------------------------------------------------------------------------

/// True when the current environment appears able to launch a desktop browser.
pub fn can_open_browser_for(
    platform: &str,
    display: Option<&str>,
    wayland_display: Option<&str>,
    has_xdg_open: bool,
) -> bool {
    match platform {
        "darwin" | "macos" | "win32" | "windows" => true,
        _ => {
            let has_gui = display.is_some_and(|s| !s.is_empty())
                || wayland_display.is_some_and(|s| !s.is_empty());
            has_gui && has_xdg_open
        }
    }
}

pub fn can_open_browser() -> bool {
    let display = std::env::var("DISPLAY").ok();
    let wayland = std::env::var("WAYLAND_DISPLAY").ok();
    let has_xdg = find_on_path("xdg-open").is_some();
    can_open_browser_for(
        std::env::consts::OS,
        display.as_deref(),
        wayland.as_deref(),
        has_xdg,
    )
}

/// Search `PATH` for an executable binary.
pub fn find_on_path(bin: &str) -> Option<PathBuf> {
    let p = Path::new(bin);
    if p.components().count() > 1 {
        return p.is_file().then(|| p.to_path_buf());
    }
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Launch the platform browser completely detached from stdin/stdout/stderr
/// (so MCP stdio framing is never corrupted by browser output).
pub fn open_in_browser(url: &str) -> bool {
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(url);
        c
    } else if cfg!(target_os = "windows") {
        let mut c = Command::new("cmd");
        c.args(["/c", "start", "", url]);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn().is_ok()
}

// ---------------------------------------------------------------------------
// OAuthManager: refresh locking, loopback login, first-use browser sign-in
// ---------------------------------------------------------------------------

type BrowserFn = Arc<dyn Fn(&str) -> bool + Send + Sync>;

struct FirstUseState {
    in_flight: Option<Arc<FirstUseJob>>,
    refused_error: Option<String>,
}

struct FirstUseJob {
    done: Notify,
    result: Mutex<Option<Result<Credential, OAuthError>>>,
}

/// Callback invoked with the authorize URL during interactive OAuth login.
pub type OnUrlCallback = Box<dyn Fn(&str) + Send + Sync>;

/// Options for [`OAuthManager::login`].
pub struct LoginOpts {
    pub open_browser: bool,
    pub paste: bool,
    pub wait_seconds: f64,
    pub on_url: Option<OnUrlCallback>,
}

impl Default for LoginOpts {
    fn default() -> Self {
        Self {
            open_browser: true,
            paste: false,
            wait_seconds: 180.0,
            on_url: None,
        }
    }
}

/// Stateful OAuth manager with injectable HTTP client, config dir, and hooks for testing.
pub struct OAuthManager {
    pub config_dir: PathBuf,
    pub codex_home: Option<PathBuf>,
    pub http: reqwest::Client,
    pub antigravity_base_urls: Vec<String>,
    pub antigravity_version: String,
    pub antigravity_client_override: Option<(String, String)>,
    pub antigravity_install_paths: Option<Vec<PathBuf>>,
    pub reply_wait: Duration,
    provider_overrides: Mutex<BTreeMap<String, OAuthProvider>>,
    browser_opener: Mutex<Option<BrowserFn>>,
    refresh_locks: Mutex<BTreeMap<String, Arc<AsyncMutex<()>>>>,
    first_use: Mutex<BTreeMap<String, FirstUseState>>,
}

impl OAuthManager {
    pub fn from_config(cfg: &Config) -> Self {
        let http = build_oauth_http_client(cfg);
        let ag_override = match (&cfg.antigravity_client_id, &cfg.antigravity_client_secret) {
            (Some(id), Some(sec)) => Some((id.clone(), sec.clone())),
            _ => None,
        };
        Self {
            config_dir: cfg.config_dir.clone(),
            codex_home: cfg.codex_home.clone(),
            http,
            antigravity_base_urls: cfg.antigravity_base_urls.clone(),
            antigravity_version: cfg.antigravity_version.clone(),
            antigravity_client_override: ag_override,
            antigravity_install_paths: None,
            reply_wait: Duration::from_secs_f64(DEFAULT_REPLY_WAIT_SECS),
            provider_overrides: Mutex::new(BTreeMap::new()),
            browser_opener: Mutex::new(None),
            refresh_locks: Mutex::new(BTreeMap::new()),
            first_use: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn with_http(mut self, http: reqwest::Client) -> Self {
        self.http = http;
        self
    }

    pub fn override_provider(&self, provider: OAuthProvider) {
        if let Ok(mut guard) = self.provider_overrides.lock() {
            guard.insert(provider.id.to_owned(), provider);
        }
    }

    pub fn set_browser_opener(&self, f: impl Fn(&str) -> bool + Send + Sync + 'static) {
        if let Ok(mut guard) = self.browser_opener.lock() {
            *guard = Some(Arc::new(f));
        }
    }

    pub fn provider(&self, id: &str) -> Result<OAuthProvider, OAuthError> {
        if let Ok(guard) = self.provider_overrides.lock()
            && let Some(p) = guard.get(id)
        {
            return Ok(p.clone());
        }
        OAuthProvider::by_id(id)
    }

    fn resolve_antigravity_client(&self) -> Result<(String, String), OAuthError> {
        if let Some(pair) = &self.antigravity_client_override {
            return Ok(pair.clone());
        }
        antigravity_client(
            None,
            &self.config_dir,
            self.antigravity_install_paths.as_deref(),
            None,
        )
    }

    fn with_client(&self, mut spec: OAuthProvider) -> Result<OAuthProvider, OAuthError> {
        if spec.id == "antigravity" && spec.client_id.is_empty() {
            let (id, sec) = self.resolve_antigravity_client()?;
            spec.client_id = id;
            spec.client_secret = sec;
        }
        Ok(spec)
    }

    fn refresh_mutex(&self, provider_id: &str) -> Arc<AsyncMutex<()>> {
        let mut guard = self.refresh_locks.lock().expect("refresh locks mutex");
        guard
            .entry(provider_id.to_owned())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    pub fn is_signed_in(&self, provider_id: &str) -> bool {
        is_signed_in(&self.config_dir, provider_id, self.codex_home.as_deref())
    }

    pub fn load(&self, provider_id: &str) -> Option<Credential> {
        load(&self.config_dir, provider_id, self.codex_home.as_deref())
    }

    pub fn save(&self, cred: &Credential) -> std::io::Result<()> {
        save(&self.config_dir, cred)
    }

    pub fn sign_out(&self, provider_id: &str) -> bool {
        sign_out(&self.config_dir, provider_id)
    }

    pub fn status(&self, provider_id: &str) -> Result<OAuthStatus, OAuthError> {
        status(&self.config_dir, provider_id, self.codex_home.as_deref())
    }

    /// Return a valid credential, refreshing under in-process + cross-process lock if needed.
    pub async fn credential(
        self: &Arc<Self>,
        provider_id: &str,
        force_refresh: bool,
    ) -> Result<Credential, OAuthError> {
        let cred = self.load(provider_id).ok_or_else(|| {
            OAuthError::NotSignedIn(format!(
                "{provider_id} is not signed in. Run `free-search-mcp login {provider_id}`."
            ))
        })?;

        if !force_refresh && cred.is_fresh(REFRESH_MARGIN_SECS) {
            return Ok(cred);
        }

        if cred.source == "codex-cli" {
            if force_refresh {
                return Err(OAuthError::Failed(
                    "Codex CLI token was rejected (linked read-only). Run `codex` once to refresh it, or run `free-search-mcp login codex`.".to_owned(),
                ));
            }
            return Err(OAuthError::Failed(
                "Codex CLI token has expired (linked read-only). Run `codex` once to refresh it, or run `free-search-mcp login codex`.".to_owned(),
            ));
        }

        // Shield refresh on a spawned task so caller cancellation never abandons a rotating refresh token.
        let this = Arc::clone(self);
        let pid = provider_id.to_owned();
        let handle = tokio::spawn(async move { this.refresh_locked(&pid, force_refresh).await });
        match handle.await {
            Ok(res) => res,
            Err(e) => Err(OAuthError::Failed(format!("refresh task failed: {e}"))),
        }
    }

    async fn refresh_locked(
        &self,
        provider_id: &str,
        force_refresh: bool,
    ) -> Result<Credential, OAuthError> {
        let mutex = self.refresh_mutex(provider_id);
        let _guard = mutex.lock().await;

        // Cross-process file lock in <config_dir>/oauth/<provider>.lock.
        let oauth_dir = self.config_dir.join("oauth");
        let _ = keystore::ensure_private_dir(&oauth_dir);
        let lock_path = oauth_dir.join(format!("{provider_id}.lock"));
        let lock_file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .ok();
        let mut rw_lock = lock_file.map(fd_lock::RwLock::new);
        let _file_guard = rw_lock.as_mut().and_then(|l| l.write().ok());

        // Re-read after acquiring lock: another caller or process may have refreshed already.
        let mut cred = self.load(provider_id).ok_or_else(|| {
            OAuthError::NotSignedIn(format!(
                "{provider_id} is not signed in. Run `free-search-mcp login {provider_id}`."
            ))
        })?;
        if !force_refresh && cred.is_fresh(REFRESH_MARGIN_SECS) {
            return Ok(cred);
        }
        if cred.refresh_token.is_empty() {
            return Err(OAuthError::Failed(format!(
                "{provider_id} has no refresh token; run `free-search-mcp login {provider_id}` to sign in again."
            )));
        }

        let mut spec = self.provider(provider_id)?;
        if spec.id == "antigravity" {
            if !cred.client_id.is_empty() && !cred.client_secret.is_empty() {
                spec.client_id = cred.client_id.clone();
                spec.client_secret = cred.client_secret.clone();
            } else {
                let (id, sec) = self.resolve_antigravity_client()?;
                spec.client_id = id.clone();
                spec.client_secret = sec.clone();
                cred.client_id = id;
                cred.client_secret = sec;
            }
        }

        let reply = if spec.json_refresh {
            let body = json!({
                "client_id": spec.client_id,
                "grant_type": "refresh_token",
                "refresh_token": cred.refresh_token,
            });
            self.token_request_json(&spec, &body).await?
        } else {
            let mut form: Vec<(&str, &str)> = vec![
                ("client_id", spec.client_id.as_str()),
                ("grant_type", "refresh_token"),
                ("refresh_token", cred.refresh_token.as_str()),
            ];
            if !spec.client_secret.is_empty() {
                form.push(("client_secret", spec.client_secret.as_str()));
            }
            self.token_request_form(&spec, &form).await?
        };

        let access = reply
            .get("access_token")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if access.is_empty() {
            return Err(OAuthError::Failed(format!(
                "token refresh for {provider_id} returned no access_token; run `free-search-mcp login {provider_id}`"
            )));
        }
        cred.access_token = access.to_owned();
        if let Some(rt) = reply
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            cred.refresh_token = rt.to_owned();
        }
        if let Some(exp_in) = reply.get("expires_in").and_then(Value::as_f64) {
            cred.expires_at = now_unix() + exp_in;
        }
        let id_token = reply.get("id_token").and_then(Value::as_str).unwrap_or("");
        if spec.id == "codex" {
            apply_codex_claims(&mut cred, id_token);
        } else if !id_token.is_empty()
            && let Some(email) = jwt_claims(id_token)
                .get("email")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        {
            cred.email = email.to_owned();
        }

        self.save(&cred)
            .map_err(|e| OAuthError::Failed(format!("failed to save refreshed token: {e}")))?;
        Ok(cred)
    }

    async fn token_request_json(
        &self,
        spec: &OAuthProvider,
        body: &Value,
    ) -> Result<Value, OAuthError> {
        let resp = self
            .http
            .post(&spec.token_url)
            .header("Content-Type", "application/json")
            .json(body)
            .send()
            .await
            .map_err(|e| map_transport_err(spec, &e))?;
        parse_token_response(spec, resp).await
    }

    async fn token_request_form(
        &self,
        spec: &OAuthProvider,
        form: &[(&str, &str)],
    ) -> Result<Value, OAuthError> {
        let body = {
            let mut ser = url::form_urlencoded::Serializer::new(String::new());
            for &(k, v) in form {
                ser.append_pair(k, v);
            }
            ser.finish()
        };
        let resp = self
            .http
            .post(&spec.token_url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .map_err(|e| map_transport_err(spec, &e))?;
        parse_token_response(spec, resp).await
    }

    /// Run a full OAuth 2.0 PKCE loopback login flow.
    pub async fn login(
        self: &Arc<Self>,
        provider_id: &str,
        opts: LoginOpts,
    ) -> Result<Credential, OAuthError> {
        let base_spec = self.provider(provider_id)?;
        let spec = self.with_client(base_spec)?;

        let (port, listeners) = bind_callback_listeners(&spec).await?;
        let redirect_uri = spec.redirect_uri(port);
        let (verifier, challenge) = pkce_pair();
        let state = random_state();
        let auth_url = spec.authorize_link(&challenge, &state, &redirect_uri);

        if let Some(cb) = &opts.on_url {
            cb(&auth_url);
        }
        if opts.open_browser {
            self.launch_browser(&auth_url);
        }

        let (code_tx, mut code_rx) = tokio::sync::mpsc::channel::<(
            Result<String, OAuthError>,
            Option<tokio::sync::oneshot::Sender<(u16, String)>>,
        )>(4);

        let mut server_handles = Vec::new();
        for listener in listeners {
            let expected_path = spec.redirect_path.clone();
            let expected_state = state.clone();
            let tx = code_tx.clone();
            let reply_wait = self.reply_wait;
            server_handles.push(tokio::spawn(async move {
                serve_callback_listener(listener, expected_path, expected_state, tx, reply_wait)
                    .await;
            }));
        }

        if opts.paste {
            let expected_state = state.clone();
            let tx = code_tx.clone();
            std::thread::spawn(move || {
                let mut line = String::new();
                while std::io::stdin().read_line(&mut line).unwrap_or(0) > 0 {
                    let trimmed = line.trim();
                    if !trimmed.is_empty()
                        && let Ok(code) = parse_pasted(trimmed, &expected_state)
                    {
                        let _ = tx.blocking_send((Ok(code), None));
                        break;
                    }
                    line.clear();
                }
            });
        }

        let wait = Duration::from_secs_f64(opts.wait_seconds.max(0.1));
        let received = tokio::time::timeout(wait, code_rx.recv()).await;
        let result = match received {
            Err(_) => Err(OAuthError::Failed(format!(
                "timed out waiting for {} sign-in callback",
                spec.id
            ))),
            Ok(None) => Err(OAuthError::Failed("callback channel closed".to_owned())),
            Ok(Some((Err(err), reply_tx))) => {
                if let Some(tx) = reply_tx {
                    let _ = tx.send((
                        400,
                        render_callback_html("Sign-in failed", &err.to_string()),
                    ));
                }
                Err(err)
            }
            Ok(Some((Ok(code), reply_tx))) => {
                let exchange = self
                    .finish_code_exchange(&spec, &code, &verifier, &redirect_uri)
                    .await;
                if let Some(tx) = reply_tx {
                    match &exchange {
                        Ok(cred) => {
                            let who = if cred.email.is_empty() {
                                spec.label.to_owned()
                            } else {
                                cred.email.clone()
                            };
                            let _ = tx.send((
                                200,
                                render_callback_html(
                                    "Signed in",
                                    &format!("Signed in ({who}). You can close this tab."),
                                ),
                            ));
                        }
                        Err(err) => {
                            let _ = tx.send((
                                400,
                                render_callback_html("Sign-in failed", &err.to_string()),
                            ));
                        }
                    }
                }
                exchange
            }
        };

        // Brief yield so HTTP reply flushes cleanly to the browser tab before aborting listeners.
        tokio::time::sleep(Duration::from_millis(20)).await;
        for h in server_handles {
            h.abort();
        }
        result
    }

    async fn finish_code_exchange(
        &self,
        spec: &OAuthProvider,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
    ) -> Result<Credential, OAuthError> {
        let mut form: Vec<(&str, &str)> = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", spec.client_id.as_str()),
            ("code_verifier", verifier),
        ];
        if !spec.client_secret.is_empty() {
            form.push(("client_secret", spec.client_secret.as_str()));
        }

        let reply = self.token_request_form(spec, &form).await?;
        let access = reply
            .get("access_token")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if access.is_empty() {
            return Err(OAuthError::Failed(format!(
                "token exchange for {} returned no access_token",
                spec.id
            )));
        }
        let refresh = reply
            .get("refresh_token")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        let expires_at = reply
            .get("expires_in")
            .and_then(Value::as_f64)
            .map(|s| now_unix() + s)
            .unwrap_or(0.0);
        let id_token = reply
            .get("id_token")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();

        let mut cred = Credential {
            provider: spec.id.to_owned(),
            access_token: access.to_owned(),
            refresh_token: refresh,
            expires_at,
            source: "oauth".to_owned(),
            client_id: if spec.id == "antigravity" {
                spec.client_id.clone()
            } else {
                String::new()
            },
            client_secret: if spec.id == "antigravity" {
                spec.client_secret.clone()
            } else {
                String::new()
            },
            ..Credential::default()
        };

        if spec.id == "codex" {
            apply_codex_claims(&mut cred, &id_token);
            if cred.account_id.is_empty() {
                return Err(OAuthError::Failed(
                    "Codex sign-in returned no workspace id (chatgpt_account_id); refusing incomplete sign-in".to_owned(),
                ));
            }
        } else if spec.id == "antigravity" {
            if let Some(email) = jwt_claims(&id_token)
                .get("email")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                cred.email = email.to_owned();
            }
            if let Some((project, tier)) = load_code_assist(
                &self.http,
                &self.antigravity_base_urls,
                &cred.access_token,
                &self.antigravity_version,
            )
            .await
            {
                cred.account_id = project;
                cred.plan = tier;
            }
        }

        self.save(&cred)
            .map_err(|e| OAuthError::Failed(format!("failed to save credential: {e}")))?;
        Ok(cred)
    }

    fn launch_browser(&self, url: &str) -> bool {
        if let Ok(guard) = self.browser_opener.lock()
            && let Some(opener) = guard.as_ref()
        {
            return opener(url);
        }
        open_in_browser(url)
    }

    /// Auto-open the browser once on first explicit use, waiting up to `wait_seconds`.
    /// Never opens a second tab if the first is still pending or was refused/failed.
    pub async fn sign_in_on_first_use(
        self: &Arc<Self>,
        provider_id: &str,
        wait_seconds: f64,
    ) -> Result<Credential, OAuthError> {
        if let Some(cred) = self.load(provider_id)
            && cred.is_fresh(REFRESH_MARGIN_SECS)
        {
            return Ok(cred);
        }

        let job = {
            let mut map = self.first_use.lock().expect("first_use mutex");
            let state = map
                .entry(provider_id.to_owned())
                .or_insert_with(|| FirstUseState {
                    in_flight: None,
                    refused_error: None,
                });
            if let Some(prev_err) = &state.refused_error {
                return Err(OAuthError::Failed(format!(
                    "{prev_err} (browser sign-in is not opened again by itself; run `free-search-mcp login {provider_id}`)"
                )));
            }
            if let Some(existing) = &state.in_flight {
                Arc::clone(existing)
            } else {
                let job = Arc::new(FirstUseJob {
                    done: Notify::new(),
                    result: Mutex::new(None),
                });
                state.in_flight = Some(Arc::clone(&job));

                let this = Arc::clone(self);
                let pid = provider_id.to_owned();
                let job_bg = Arc::clone(&job);
                tokio::spawn(async move {
                    let browser_opened = Arc::new(std::sync::atomic::AtomicBool::new(false));
                    let opened_flag = Arc::clone(&browser_opened);
                    let this_for_url = Arc::clone(&this);
                    let res = this
                        .login(
                            &pid,
                            LoginOpts {
                                open_browser: false,
                                paste: false,
                                wait_seconds: 180.0,
                                on_url: Some(Box::new(move |url| {
                                    let ok = this_for_url.launch_browser(url);
                                    opened_flag.store(ok, std::sync::atomic::Ordering::SeqCst);
                                })),
                            },
                        )
                        .await;
                    let final_res = if !browser_opened.load(std::sync::atomic::Ordering::SeqCst) {
                        Err(OAuthError::Failed(format!(
                            "no browser could be opened for {pid}; run `free-search-mcp login {pid}`"
                        )))
                    } else {
                        res
                    };
                    {
                        let mut r = job_bg.result.lock().expect("job result mutex");
                        *r = Some(final_res.clone());
                    }
                    if let Err(ref err) = final_res
                        && let Ok(mut map) = this.first_use.lock()
                        && let Some(st) = map.get_mut(&pid)
                    {
                        st.in_flight = None;
                        st.refused_error = Some(err.to_string());
                    } else if final_res.is_ok()
                        && let Ok(mut map) = this.first_use.lock()
                        && let Some(st) = map.get_mut(&pid)
                    {
                        st.in_flight = None;
                    }
                    job_bg.done.notify_waiters();
                });

                // Short yield so if `open_browser` fails immediately we don't sit around.
                job
            }
        };

        // Check immediately in case browser failed synchronously or finished fast.
        if let Some(done) = job.result.lock().expect("job result").clone() {
            return done;
        }

        let wait = Duration::from_secs_f64(wait_seconds.max(0.05));
        // Poll notified or check quickly for instant browser failure.
        let notified = job.done.notified();
        // Wait for either completion notification or timeout.
        // Check again after creating notified future to avoid missed wakeups.
        if let Some(done) = job.result.lock().expect("job result").clone() {
            return done;
        }
        match tokio::time::timeout(wait, notified).await {
            Ok(()) => job
                .result
                .lock()
                .expect("job result")
                .clone()
                .unwrap_or_else(|| {
                    Err(OAuthError::Failed(
                        "sign-in finished without result".to_owned(),
                    ))
                }),
            Err(_) => {
                // If browser failed to open right away, surface that instead of SignInPending.
                if let Some(done) = job.result.lock().expect("job result").clone() {
                    return done;
                }
                Err(OAuthError::SignInPending(format!(
                    "{provider_id} sign-in page is open in your browser and not yet approved; finish approving and retry."
                )))
            }
        }
    }
}

async fn bind_callback_listeners(
    spec: &OAuthProvider,
) -> Result<(u16, Vec<TcpListener>), OAuthError> {
    let mut busy_ports = Vec::new();
    for &port in &spec.redirect_ports {
        let v4_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
        match TcpListener::bind(v4_addr).await {
            Ok(v4) => {
                let mut listeners = vec![v4];
                if spec.redirect_host == "localhost" {
                    let v6_addr = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port);
                    if let Ok(v6) = TcpListener::bind(v6_addr).await {
                        listeners.push(v6);
                    }
                }
                return Ok((port, listeners));
            }
            Err(_) => {
                busy_ports.push(port);
            }
        }
    }

    let port_list = busy_ports
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(" and ");
    Err(OAuthError::Failed(format!(
        "could not start local OAuth callback listener on port {port_list} (already in use)"
    )))
}

type CallbackDelivery = (
    Result<String, OAuthError>,
    Option<tokio::sync::oneshot::Sender<(u16, String)>>,
);

async fn serve_callback_listener(
    listener: TcpListener,
    expected_path: String,
    expected_state: String,
    code_tx: tokio::sync::mpsc::Sender<CallbackDelivery>,
    reply_wait: Duration,
) {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            break;
        };
        let mut buf = vec![0u8; 8192];
        let n = match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => n,
            _ => continue,
        };
        let req_text = String::from_utf8_lossy(&buf[..n]);
        let first_line = req_text.lines().next().unwrap_or("");
        let target = first_line.split_whitespace().nth(1).unwrap_or("/");
        let (path, query) = target.split_once('?').unwrap_or((target, ""));

        if path != expected_path {
            let _ = write_http_response(&mut stream, 404, "Not Found", "Not found").await;
            continue;
        }

        let mut params: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
            params
                .entry(k.into_owned())
                .or_default()
                .push(v.into_owned());
        }

        let returned_state = params
            .get("state")
            .and_then(|v| v.first())
            .map(String::as_str)
            .unwrap_or("");
        if !state_matches(returned_state, &expected_state) {
            let body = render_callback_html(
                "Callback ignored",
                "Unexpected or expired OAuth state; this callback was ignored.",
            );
            let _ = write_http_response(&mut stream, 400, "Bad Request", &body).await;
            continue;
        }

        let parsed = read_redirect(&params, &expected_state);
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        if code_tx.send((parsed, Some(reply_tx))).await.is_err() {
            let _ =
                write_http_response(&mut stream, 410, "Gone", "Sign-in already completed").await;
            break;
        }

        match tokio::time::timeout(reply_wait, reply_rx).await {
            Ok(Ok((status, body))) => {
                let reason = match status {
                    200 => "OK",
                    202 => "Accepted",
                    _ => "Bad Request",
                };
                let _ = write_http_response(&mut stream, status, reason, &body).await;
            }
            _ => {
                let body = render_callback_html(
                    "Still finishing",
                    "Still finishing sign-in in the background; check your terminal for the final status.",
                );
                let _ = write_http_response(&mut stream, 202, "Accepted", &body).await;
            }
        }
        break;
    }
}

async fn write_http_response(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    reason: &str,
    body: &str,
) -> std::io::Result<()> {
    let resp = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(resp.as_bytes()).await?;
    stream.shutdown().await
}

fn render_callback_html(title: &str, body: &str) -> String {
    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{t}</title></head><body><h1>{t}</h1><p>{b}</p></body></html>",
        t = esc(title),
        b = esc(body)
    )
}

fn map_transport_err(spec: &OAuthProvider, err: &reqwest::Error) -> OAuthError {
    OAuthError::Unreachable(format!(
        "could not reach {} token endpoint ({err}). Check network access or set SEARCH_MCP_PROXY.",
        spec.id
    ))
}

/// Maximum HTTP response body read from agent/OAuth endpoints (8 MiB).
pub(crate) const MAX_AGENT_HTTP_BODY_BYTES: usize = 8 * 1024 * 1024;
/// Maximum HTTP error body prefix kept for diagnostics (64 KiB).
pub(crate) const MAX_AGENT_ERR_BODY_BYTES: usize = 64 * 1024;

/// Stream a `reqwest::Response` into a UTF-8 string with an explicit byte cap.
///
/// When `truncate_on_cap` is `true`, reading stops once `max_bytes` are buffered and returns
/// the prefix; when `false`, exceeding `max_bytes` returns `Err`.
pub(crate) async fn read_response_text_capped(
    mut resp: reqwest::Response,
    max_bytes: usize,
    truncate_on_cap: bool,
) -> Result<String, String> {
    if !truncate_on_cap
        && let Some(len) = resp.content_length()
        && len > max_bytes as u64
    {
        return Err(format!(
            "response body ({len} bytes) exceeded {max_bytes}-byte cap"
        ));
    }
    let mut buf = Vec::with_capacity(
        resp.content_length()
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(4096)
            .min(max_bytes)
            .min(64 * 1024),
    );
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| format!("failed reading response body: {e}"))?
    {
        let room = max_bytes.saturating_sub(buf.len());
        if chunk.len() > room {
            if truncate_on_cap {
                buf.extend_from_slice(&chunk[..room]);
                break;
            }
            return Err(format!("response body exceeded {max_bytes}-byte cap"));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

async fn parse_token_response(
    spec: &OAuthProvider,
    resp: reqwest::Response,
) -> Result<Value, OAuthError> {
    let status = resp.status();
    let is_ok = status.is_success();
    let text = if is_ok {
        read_response_text_capped(resp, MAX_AGENT_HTTP_BODY_BYTES, false)
            .await
            .map_err(|e| OAuthError::Failed(format!("token endpoint {e}")))?
    } else {
        read_response_text_capped(resp, MAX_AGENT_ERR_BODY_BYTES, true)
            .await
            .unwrap_or_default()
    };
    if !is_ok {
        let detail = extract_oauth_error_detail(&text);
        let hint = if status.as_u16() == 400 || status.as_u16() == 401 {
            format!(
                " — run `free-search-mcp login {}` to sign in again",
                spec.id
            )
        } else {
            String::new()
        };
        return Err(OAuthError::Failed(format!(
            "token endpoint answered HTTP {}: {detail}{hint}",
            status.as_u16()
        )));
    }
    serde_json::from_str(&text)
        .map_err(|e| OAuthError::Failed(format!("invalid JSON from token endpoint: {e}")))
}

fn extract_oauth_error_detail(text: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        if let Some(msg) = v
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
        {
            return msg.to_owned();
        }
        if let Some(desc) = v.get("error_description").and_then(Value::as_str) {
            return desc.to_owned();
        }
        if let Some(err) = v.get("error").and_then(Value::as_str) {
            return err.to_owned();
        }
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "empty response".to_owned();
    }
    let clipped = fsm_core::text::clip_chars(trimmed, 240);
    if clipped.len() < trimmed.len() {
        format!("{clipped}...")
    } else {
        trimmed.to_owned()
    }
}

/// Build a reqwest client for OAuth / agent-engine API traffic honoring `cfg.proxy`
/// when configured for the engine (or globally).
pub fn build_oauth_http_client(cfg: &Config) -> reqwest::Client {
    fsm_net::install_ring_provider();
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy();
    if let Some(proxy_url) = &cfg.proxy
        && let Ok(proxy) = reqwest::Proxy::all(proxy_url)
    {
        builder = builder.proxy(proxy);
    }
    builder.build().unwrap_or_else(|_| reqwest::Client::new())
}
