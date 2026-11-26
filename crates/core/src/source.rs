//! Source / ProtocolAdapter traits and their declarations
//! (`SPEC-contracts.md` §1.2).

use crate::api::Freshness;
use crate::config::{Config, SafeSearch};
use crate::error::FsmError;
use crate::types::{Hit, ProtocolOutput, ScholarlyWork};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Whether a source can run under the current configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Availability {
    #[default]
    Available,
    /// A required key is missing; `env` names the variable to set.
    NeedsKey { env: String },
    /// Disabled, with the reason.
    Disabled(String),
}

/// Which HTTP client a source's traffic goes through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TransportProfile {
    /// Tier 0: reqwest + rustls(ring), honest UA, no cookies.
    #[default]
    Plain,
    /// Tier 1: wreq Chrome emulation.
    Chrome,
    /// Tier 1 with an Edge-style preflight/cookie warm-up.
    EdgePreflight,
    /// No network (local computation, e.g. `worldclock`).
    Local,
}

/// How a source joins pools.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PoolRole {
    /// In the default web pool.
    Default,
    /// A hedging reserve.
    Reserve,
    /// Joins when the query language matches (`{ locale = "zh" }` in TOML).
    Locale(String),
    /// Joins only when its scope is requested.
    #[default]
    ScopeOnly,
    /// Only when named in `sources`.
    Explicit,
    /// Auto-routed through `Source::claims()`.
    DirectClaimant,
}

/// Key requirements of a source.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AuthMode {
    #[default]
    None,
    /// Works keyless; `route_requires_key` = auto-routing only with a key.
    /// `env` is one variable or a comma-separated list that must all be set
    /// (OAuth2: `"<env_id>,<env_secret>"`, A1 §B.1 #3); see `SourceDecl::key_envs`.
    OptionalKey {
        env: String,
        route_requires_key: bool,
    },
    RequiredKey {
        env: String,
    },
}

/// Parameter type of an op.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ParamType {
    #[default]
    String,
    Int,
    Float,
    Bool,
    StringArray,
    /// Any JSON value (A1 §A.3); only `in = "none"` or `in = "body"`.
    Json,
}

impl ParamType {
    /// TypeScript spelling, for signatures in the catalog.
    pub fn ts_name(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Int | Self::Float => "number",
            Self::Bool => "boolean",
            Self::StringArray => "string[]",
            Self::Json => "any",
        }
    }

    /// Whether a JSON value is acceptable for this type.
    pub fn accepts(self, v: &Value) -> bool {
        match self {
            Self::String => v.is_string(),
            Self::Int => v.as_i64().is_some() || v.as_u64().is_some(),
            Self::Float => v.is_number(),
            Self::Bool => v.is_boolean(),
            Self::StringArray => v.as_array().is_some_and(|a| a.iter().all(Value::is_string)),
            Self::Json => true,
        }
    }
}

/// What an op returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ReturnKind {
    Hits,
    Works,
    #[default]
    Records,
    Text,
    Raw,
}

/// What a source can do (catalog facets; entity caps map to structs per M13:
/// `grant` → `GrantRecord`, `author` → `AuthorProfile`, `org` →
/// `OrgProfile`, `venue` → `VenueProfile`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Capability {
    #[default]
    Search,
    Lookup,
    Harvest,
    Graph,
    Fulltext,
    Author,
    Org,
    Venue,
    Dataset,
    Patent,
    Grant,
    Thesis,
    Book,
    Integrity,
}

impl Capability {
    /// Wire name (`"search"`, `"grant"`, ...).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::Lookup => "lookup",
            Self::Harvest => "harvest",
            Self::Graph => "graph",
            Self::Fulltext => "fulltext",
            Self::Author => "author",
            Self::Org => "org",
            Self::Venue => "venue",
            Self::Dataset => "dataset",
            Self::Patent => "patent",
            Self::Grant => "grant",
            Self::Thesis => "thesis",
            Self::Book => "book",
            Self::Integrity => "integrity",
        }
    }
}

/// Rate/time limits of a source (also the `limits = {..}` table of a manifest).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct SourceLimits {
    pub rps: f64,
    pub burst: u32,
    pub concurrency: u32,
    pub timeout_ms: u64,
    pub cache_ttl_s: u64,
    /// Shared GCRA bucket (S8); defaults to the source id.
    #[serde(default)]
    pub host_bucket: Option<String>,
}

impl SourceLimits {
    pub fn new(rps: f64, burst: u32, concurrency: u32, timeout_ms: u64, cache_ttl_s: u64) -> Self {
        Self {
            rps,
            burst,
            concurrency,
            timeout_ms,
            cache_ttl_s,
            host_bucket: None,
        }
    }
}

/// One parameter of an op.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Param {
    pub name: String,
    pub ty: ParamType,
    pub required: bool,
    #[serde(default, with = "crate::serde_compat::json_compat")]
    pub default: Option<Value>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub enum_values: Option<Vec<String>>,
    pub doc: String,
}

impl Param {
    pub fn new(name: impl Into<String>, ty: ParamType, doc: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ty,
            doc: doc.into(),
            ..Self::default()
        }
    }

    /// Builder: mark required.
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    /// Builder: set the default value.
    pub fn with_default(mut self, value: Value) -> Self {
        self.default = Some(value);
        self
    }

    /// Builder: numeric bounds.
    pub fn with_range(mut self, min: f64, max: f64) -> Self {
        self.min = Some(min);
        self.max = Some(max);
        self
    }

    /// Builder: allowed string values.
    pub fn with_enum(mut self, values: &[&str]) -> Self {
        self.enum_values = Some(values.iter().map(|s| (*s).to_owned()).collect());
        self
    }
}

/// Per-op auth override (A1 §A.1): `inherit` the source's `[auth]`, `none`
/// (a keyless op of a keyed source) or `required` (needs a top-level `[auth]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OpAuthOverride {
    #[default]
    Inherit,
    None,
    Required,
}

/// Live-probe declaration of an op.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProbeSpec {
    pub op: Option<String>,
    #[serde(with = "crate::serde_compat::json_compat")]
    pub params: Value,
    pub min_results: usize,
    /// `fixtures/<wave>/<id>/<op>/<case>.json`; `None` = the canonical
    /// `.../default.json` (A1 §D.2). Always serialized (postcard-safe).
    #[serde(default)]
    pub fixture: Option<String>,
}

impl ProbeSpec {
    pub fn new(params: Value, min_results: usize) -> Self {
        Self {
            op: None,
            params,
            min_results,
            fixture: None,
        }
    }

    /// Builder: explicit fixture path.
    pub fn with_fixture(mut self, fixture: impl Into<String>) -> Self {
        self.fixture = Some(fixture.into());
        self
    }
}

/// One callable op of a source.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OpDecl {
    pub name: String,
    pub summary: String,
    pub params: Vec<Param>,
    pub returns: ReturnKind,
    pub examples: Vec<String>,
    pub probe: Option<ProbeSpec>,
    /// A1 §A.1 per-op auth override.
    #[serde(default)]
    pub auth: OpAuthOverride,
}

impl OpDecl {
    pub fn new(name: impl Into<String>, summary: impl Into<String>, returns: ReturnKind) -> Self {
        Self {
            name: name.into(),
            summary: summary.into(),
            returns,
            ..Self::default()
        }
    }

    /// Builder: append a parameter.
    pub fn with_param(mut self, param: Param) -> Self {
        self.params.push(param);
        self
    }

    /// Builder: append a copy-ready example.
    pub fn with_example(mut self, example: impl Into<String>) -> Self {
        self.examples.push(example.into());
        self
    }

    /// Builder: per-op auth override.
    pub fn with_auth(mut self, auth: OpAuthOverride) -> Self {
        self.auth = auth;
        self
    }

    /// Builder: set the live probe.
    pub fn with_probe(mut self, probe: ProbeSpec) -> Self {
        self.probe = Some(probe);
        self
    }

    pub fn param(&self, name: &str) -> Option<&Param> {
        self.params.iter().find(|p| p.name == name)
    }
}

/// Static declaration of a source (native engine, manifest or protocol instance).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SourceDecl {
    pub id: String,
    pub name: String,
    pub summary: String,
    pub aliases: Vec<String>,
    pub home_url: Option<String>,
    pub tier: u8,
    pub scopes: Vec<String>,
    pub caps: Vec<Capability>,
    pub auth: AuthMode,
    pub transport: TransportProfile,
    pub pool_role: PoolRole,
    pub limits: SourceLimits,
    pub ops: Vec<OpDecl>,
}

impl SourceDecl {
    pub fn new(id: impl Into<String>, name: impl Into<String>, summary: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            summary: summary.into(),
            tier: 1,
            ..Self::default()
        }
    }

    /// Builder: scopes (canonical `core::SCOPES` ids).
    pub fn with_scopes(mut self, scopes: &[&str]) -> Self {
        self.scopes = scopes.iter().map(|s| (*s).to_owned()).collect();
        self
    }

    /// Builder: capabilities.
    pub fn with_caps(mut self, caps: &[Capability]) -> Self {
        self.caps = caps.to_vec();
        self
    }

    /// Builder: append an op.
    pub fn with_op(mut self, op: OpDecl) -> Self {
        self.ops.push(op);
        self
    }

    pub fn op(&self, name: &str) -> Option<&OpDecl> {
        self.ops.iter().find(|o| o.name == name)
    }

    pub fn has_cap(&self, cap: Capability) -> bool {
        self.caps.contains(&cap)
    }

    /// Every env var the key needs (`key_env` split on commas; OAuth2
    /// sources need both `env_id` and `env_secret`).
    pub fn key_envs(&self) -> Vec<&str> {
        self.key_env()
            .map(|e| {
                e.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The env var(s) holding this source's key, as declared (may be a comma list).
    pub fn key_env(&self) -> Option<&str> {
        match &self.auth {
            AuthMode::OptionalKey { env, .. } | AuthMode::RequiredKey { env } => Some(env),
            AuthMode::None => None,
        }
    }
}

/// What a `Source::call` produced.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum SrcOutput {
    Hits(Vec<Hit>),
    Works(Vec<ScholarlyWork>),
    Records {
        items: Vec<Value>,
        hits: Option<Vec<Hit>>,
        next_cursor: Option<String>,
    },
    Text(String),
    Raw(Value),
}

impl Default for SrcOutput {
    fn default() -> Self {
        Self::Hits(Vec::new())
    }
}

/// Per-call context handed to sources and protocol adapters.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct SrcCx {
    pub cancel: CancellationToken,
    pub timeout: Duration,
    pub api_key: Option<String>,
    pub contact_email: Option<String>,
    pub lang: Option<String>,
    pub region: String,
    pub safesearch: SafeSearch,
    pub accept_language: String,
    /// Requested freshness, exposed to `jaq` as `$cfg.freshness` (A1 §B.10).
    pub freshness: Option<Freshness>,
    /// The calling protocol instance's `dialect` table (A1 §D.2); empty otherwise.
    pub dialect: BTreeMap<String, Value>,
    /// Offline replay (`manifest_replay`): skip OAuth2 token fetches and
    /// in-memory discovery caches (A1 §B.9).
    pub replay_mode: bool,
    /// Domain filters of the request: `"example.com"` includes, `"-example.com"`
    /// excludes. Engines that support them push them upstream (`site:`, JSON
    /// fields); the search kernel filters the merged results either way.
    pub domains: Vec<String>,
}

impl SrcCx {
    pub fn new(cancel: CancellationToken, timeout: Duration) -> Self {
        Self {
            cancel,
            timeout,
            ..Self::default()
        }
    }

    /// A context carrying the locale/contact settings of `cfg`.
    pub fn from_config(cfg: &Config, cancel: CancellationToken, timeout: Duration) -> Self {
        Self {
            cancel,
            timeout,
            api_key: None,
            contact_email: cfg.contact_email.clone(),
            lang: None,
            region: cfg.region.clone(),
            safesearch: cfg.safesearch,
            accept_language: cfg.accept_language.clone(),
            freshness: None,
            dialect: BTreeMap::new(),
            replay_mode: false,
            domains: Vec::new(),
        }
    }
}

/// A searchable/callable source (native engine, manifest, or protocol instance).
#[async_trait::async_trait]
pub trait Source: Send + Sync {
    fn decl(&self) -> &SourceDecl;
    /// How strongly this source claims a query for direct-fact routing (0 = no claim).
    fn claims(&self, _query: &str, _lang: Option<&str>) -> f32 {
        0.0
    }
    fn availability(&self, _cfg: &Config) -> Availability {
        Availability::Available
    }
    async fn call(&self, op: &str, params: Value, cx: &SrcCx) -> Result<SrcOutput, FsmError>;
}

/// A generic protocol (OAI-PMH, CKAN, ...) callable against any base URL.
#[async_trait::async_trait]
pub trait ProtocolAdapter: Send + Sync {
    fn kind(&self) -> &'static str;
    fn ops(&self) -> &[OpDecl];
    /// Check an instance's `dialect` table (A1 §D.2); called by `xtask validate`
    /// and at registration. Default: the adapter accepts no dialect keys.
    fn validate_dialect(&self, dialect: &BTreeMap<String, Value>) -> Result<(), FsmError> {
        if dialect.is_empty() {
            Ok(())
        } else {
            let keys: Vec<&str> = dialect.keys().map(String::as_str).collect();
            Err(FsmError::invalid(format!(
                "{} does not accept dialect keys ({})",
                self.kind(),
                keys.join(", ")
            )))
        }
    }
    async fn call(
        &self,
        base_url: &str,
        op: &str,
        params: Value,
        cx: &SrcCx,
    ) -> Result<ProtocolOutput, FsmError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn enum_wire_names() {
        assert_eq!(
            serde_json::to_value(PoolRole::Locale("zh".into())).unwrap(),
            json!({"locale": "zh"})
        );
        assert_eq!(
            serde_json::to_value(PoolRole::DirectClaimant).unwrap(),
            json!("direct_claimant")
        );
        assert_eq!(
            serde_json::to_value(TransportProfile::EdgePreflight).unwrap(),
            json!("edge_preflight")
        );
        assert_eq!(
            serde_json::to_value(ParamType::StringArray).unwrap(),
            json!("string_array")
        );
        assert_eq!(
            serde_json::to_value(Capability::Fulltext).unwrap(),
            json!("fulltext")
        );
        assert_eq!(Capability::Grant.as_str(), "grant");
        assert_eq!(
            serde_json::to_value(AuthMode::OptionalKey {
                env: "E".into(),
                route_requires_key: true
            })
            .unwrap(),
            json!({"optional_key": {"env": "E", "route_requires_key": true}})
        );
    }

    #[test]
    fn defaults_match_m1() {
        assert_eq!(AuthMode::default(), AuthMode::None);
        assert_eq!(TransportProfile::default(), TransportProfile::Plain);
        assert_eq!(PoolRole::default(), PoolRole::ScopeOnly);
        assert_eq!(ReturnKind::default(), ReturnKind::Records);
        assert_eq!(SourceDecl::new("x", "X", "s").tier, 1);
        assert_eq!(SrcOutput::default(), SrcOutput::Hits(vec![]));
    }

    #[test]
    fn param_type_accepts() {
        assert!(ParamType::Int.accepts(&json!(3)));
        assert!(!ParamType::Int.accepts(&json!(3.5)));
        assert!(ParamType::Float.accepts(&json!(3)));
        assert!(ParamType::StringArray.accepts(&json!(["a", "b"])));
        assert!(!ParamType::StringArray.accepts(&json!(["a", 1])));
        assert_eq!(ParamType::Int.ts_name(), "number");
    }

    #[test]
    fn builders_and_lookups() {
        let decl = SourceDecl::new("crates", "crates.io", "Rust crates")
            .with_scopes(&["code"])
            .with_caps(&[Capability::Search, Capability::Lookup])
            .with_op(
                OpDecl::new("search", "Search crates", ReturnKind::Hits)
                    .with_param(Param::new("query", ParamType::String, "Terms").required())
                    .with_param(
                        Param::new("limit", ParamType::Int, "Rows")
                            .with_default(json!(10))
                            .with_range(1.0, 50.0),
                    ),
            );
        assert!(decl.has_cap(Capability::Lookup));
        assert!(decl.op("search").unwrap().param("query").unwrap().required);
        assert_eq!(
            decl.op("search").unwrap().param("limit").unwrap().max,
            Some(50.0)
        );
        assert_eq!(decl.key_env(), None);
    }
}
