//! Declarative source manifests (`SPEC-contracts.md` §2 + `SPEC-addendum-A1.md` §A)
//! and protocol instance tables (§3 + A1 §D).
//!
//! Every struct is `#[serde(deny_unknown_fields)]` and derives `JsonSchema`
//! (`crates/manifests/manifest.schema.json` is generated from [`ManifestDecl`]).
//! The types are also postcard-safe (no `skip_serializing_if`, no untagged
//! enums at the top level; free-form TOML values travel through
//! [`crate::serde_compat::json_compat`]) so `fsm-manifests/build.rs` can embed them.
//!
//! Interpreter semantics live in A1 §B (W2a): responses are parsed by the
//! declared `response` (§B.1); XML→JSON strips namespace prefixes and
//! ASCII-lowercases every element/attribute local name (§B.6); `map.items` is a
//! `jaq` stream with no silent array unwrapping (§B.5); the 13 native prelude
//! functions are listed in §C; `jaq` sees `$p` and `$cfg = {contact_email, lang,
//! region, freshness}`.

use crate::deps::is_valid_id;
use crate::error::FsmError;
use crate::net::HttpMethod;
use crate::scopes;
pub use crate::source::OpAuthOverride;
use crate::source::{
    AuthMode, Capability, OpDecl, Param, ParamType, PoolRole, ProbeSpec, ReturnKind, SourceDecl,
    SourceLimits, TransportProfile,
};
use regex::Regex;
use schemars::JsonSchema;
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::marker::PhantomData;
use std::sync::LazyLock;

// ---------------------------------------------------------------------------
// Ordered table
// ---------------------------------------------------------------------------

/// A table that keeps document order (ops and params are listed in catalog
/// signatures in the order the manifest writes them). Duplicate keys are
/// rejected on deserialization.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderedMap<V>(pub Vec<(String, V)>);

impl<V> Default for OrderedMap<V> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<V> OrderedMap<V> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, key: &str) -> Option<&V> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut V> {
        self.0.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// Insert or replace (a new key goes last).
    pub fn insert(&mut self, key: impl Into<String>, value: V) {
        let key = key.into();
        match self.get_mut(&key) {
            Some(slot) => *slot = value,
            None => self.0.push((key, value)),
        }
    }

    pub fn remove(&mut self, key: &str) -> Option<V> {
        let i = self.0.iter().position(|(k, _)| k == key)?;
        Some(self.0.remove(i).1)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &V)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(k, _)| k.as_str())
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<V: Serialize> Serialize for OrderedMap<V> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (k, v) in &self.0 {
            map.serialize_entry(k, v)?;
        }
        map.end()
    }
}

impl<'de, V: Deserialize<'de>> Deserialize<'de> for OrderedMap<V> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V2<V>(PhantomData<V>);
        impl<'de, V: Deserialize<'de>> Visitor<'de> for V2<V> {
            type Value = OrderedMap<V>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a table")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
                let mut out: Vec<(String, V)> = Vec::with_capacity(access.size_hint().unwrap_or(0));
                while let Some((k, v)) = access.next_entry::<String, V>()? {
                    if out.iter().any(|(e, _)| *e == k) {
                        return Err(serde::de::Error::custom(format!("duplicate key {k:?}")));
                    }
                    out.push((k, v));
                }
                Ok(OrderedMap(out))
            }
        }
        deserializer.deserialize_map(V2(PhantomData))
    }
}

impl<V: JsonSchema> JsonSchema for OrderedMap<V> {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        <BTreeMap<String, V>>::schema_name()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        <BTreeMap<String, V>>::json_schema(generator)
    }
}

// ---------------------------------------------------------------------------
// Constant query values (A1 §A.5)
// ---------------------------------------------------------------------------

/// A `const_query` / `pre.params` value: one string, or an array emitting
/// repeated `k=v` pairs. Numbers and booleans are accepted and stringified.
/// Values undergo `{var}` template expansion.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConstQueryVal {
    One(String),
    Many(Vec<String>),
}

impl Default for ConstQueryVal {
    fn default() -> Self {
        Self::One(String::new())
    }
}

impl ConstQueryVal {
    pub fn one(v: impl Into<String>) -> Self {
        Self::One(v.into())
    }

    pub fn many<I: IntoIterator<Item = S>, S: Into<String>>(vs: I) -> Self {
        Self::Many(vs.into_iter().map(Into::into).collect())
    }

    /// The values in emission order.
    pub fn values(&self) -> Vec<&str> {
        match self {
            Self::One(v) => vec![v.as_str()],
            Self::Many(vs) => vs.iter().map(String::as_str).collect(),
        }
    }
}

impl Serialize for ConstQueryVal {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::One(v) => serializer.serialize_str(v),
            Self::Many(vs) => vs.serialize(serializer),
        }
    }
}

struct Scalar(String);

impl<'de> Deserialize<'de> for Scalar {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = Scalar;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a string, number or boolean")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Scalar, E> {
                Ok(Scalar(v.to_owned()))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Scalar, E> {
                Ok(Scalar(v.to_string()))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Scalar, E> {
                Ok(Scalar(v.to_string()))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Scalar, E> {
                Ok(Scalar(v.to_string()))
            }
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Scalar, E> {
                Ok(Scalar(v.to_string()))
            }
        }
        d.deserialize_any(V)
    }
}

impl<'de> Deserialize<'de> for ConstQueryVal {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = ConstQueryVal;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a string, number, boolean or an array of them")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<ConstQueryVal, E> {
                Ok(ConstQueryVal::One(v.to_owned()))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<ConstQueryVal, E> {
                Ok(ConstQueryVal::One(v.to_string()))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<ConstQueryVal, E> {
                Ok(ConstQueryVal::One(v.to_string()))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<ConstQueryVal, E> {
                Ok(ConstQueryVal::One(v.to_string()))
            }
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<ConstQueryVal, E> {
                Ok(ConstQueryVal::One(v.to_string()))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<ConstQueryVal, A::Error> {
                let mut out = Vec::new();
                while let Some(Scalar(s)) = seq.next_element()? {
                    out.push(s);
                }
                Ok(ConstQueryVal::Many(out))
            }
        }
        d.deserialize_any(V)
    }
}

impl JsonSchema for ConstQueryVal {
    fn schema_name() -> Cow<'static, str> {
        "ConstQueryVal".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "description": "A string (numbers/booleans are stringified) or an array of them (repeated k=v pairs).",
            "anyOf": [
                { "type": ["string", "number", "boolean"] },
                { "type": "array", "items": { "type": ["string", "number", "boolean"] } }
            ]
        })
    }
}

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// `transport` of a manifest (`local` is reserved for in-process sources).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ManifestTransport {
    #[default]
    Plain,
    Chrome,
    EdgePreflight,
}

impl ManifestTransport {
    pub fn profile(self) -> TransportProfile {
        match self {
            Self::Plain => TransportProfile::Plain,
            Self::Chrome => TransportProfile::Chrome,
            Self::EdgePreflight => TransportProfile::EdgePreflight,
        }
    }
}

/// `response` format of an op; the body is parsed by this, never by
/// `Content-Type` (A1 §B.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ResponseFormat {
    #[default]
    Json,
    Xml,
    Html,
    Csv,
    Ndjson,
    Text,
}

impl ResponseFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Xml => "xml",
            Self::Html => "html",
            Self::Csv => "csv",
            Self::Ndjson => "ndjson",
            Self::Text => "text",
        }
    }
}

/// Request body encoding (wire contract: A1 §B.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum BodyKind {
    #[default]
    Json,
    Form,
    Graphql,
    Sparql,
}

impl BodyKind {
    /// Default `Content-Type` (A1 §B.7).
    pub fn content_type(self) -> &'static str {
        match self {
            Self::Json | Self::Graphql => "application/json",
            Self::Form => "application/x-www-form-urlencoded",
            Self::Sparql => "application/sparql-query",
        }
    }
}

/// Where a parameter goes; `none` = never transmitted directly (usable in
/// templates, `pre`, `jaq`, `query_jaq`; A1 §A.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ParamLocation {
    #[default]
    Query,
    Path,
    Header,
    Body,
    None,
}

/// Where the credential goes (`[auth] in`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AuthIn {
    #[default]
    Header,
    Query,
    Body,
}

/// `[auth] kind` (A1 §A.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AuthKind {
    /// A key read from `env`.
    #[default]
    Static,
    /// RFC 6749 §4.4 token exchange (runtime rules: A1 §B.8).
    Oauth2ClientCredentials,
}

/// How OAuth2 client credentials are sent to `token_url`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OAuthStyle {
    /// HTTP Basic header.
    #[default]
    Basic,
    /// Form-urlencoded `client_id` / `client_secret`.
    Body,
}

/// `auth.mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AuthModeManifest {
    #[default]
    Optional,
    Required,
}

/// `paginate.style`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PaginateStyle {
    #[default]
    Page,
    Offset,
    Cursor,
    LinkHeader,
    Token,
}

/// `string_array` query serialization (A1 §A.3): `k=a&k=b`, `k=a,b`, `k[]=a&k[]=b`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ArrayStyle {
    #[default]
    Repeat,
    Csv,
    Brackets,
}

/// Path parameter encoding (A1 §A.3, §B.3): `segment` percent-encodes `/`,
/// `raw` keeps `/` (traversal guardrails still apply).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PathEncode {
    #[default]
    Segment,
    Raw,
}

/// Whether a pre-flight step injects the op's effective auth (A1 §A.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PreAuth {
    #[default]
    Inherit,
    None,
}

/// What happens when `pre.bind` yields `null`, `""`, `[]` or nothing (A1 §A.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OnEmpty {
    /// Return an empty result without calling the main request.
    #[default]
    Empty,
    /// Fail with an upstream error.
    Error,
    /// Call the main request with an empty `{pre}`.
    Continue,
}

// ---------------------------------------------------------------------------
// Manifest structs
// ---------------------------------------------------------------------------

/// One `crates/manifests/sources/<wave>/<id>.toml` file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ManifestDecl {
    /// `^[a-z][a-z0-9_]{1,39}$`.
    pub id: String,
    pub name: String,
    pub summary: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub home_url: Option<String>,
    /// 1 = core, 2 = specialised, 3 = long tail.
    pub tier: u8,
    /// Canonical `core::SCOPES` ids.
    pub scopes: Vec<String>,
    pub caps: Vec<Capability>,
    pub transport: ManifestTransport,
    pub pool_role: PoolRole,
    pub limits: SourceLimits,
    #[serde(default)]
    pub auth: Option<AuthDecl>,
    #[serde(default)]
    pub claims: Option<ClaimsManifest>,
    /// `[op.<name>]` tables in document order.
    pub op: OrderedMap<OpSpec>,
}

/// `[auth]`: a static key (`env`) or OAuth2 client credentials (A1 §A.2).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct AuthDecl {
    #[serde(default)]
    pub kind: AuthKind,
    pub mode: AuthModeManifest,
    /// Static only: full variable name, e.g. `SEARCH_MCP_ADS_API_KEY`.
    #[serde(default)]
    pub env: Option<String>,
    /// Required for `static`; OAuth2 default `header`.
    #[serde(default, rename = "in")]
    pub location: Option<AuthIn>,
    /// Header / query / body field name. Required for `static`; OAuth2 default `Authorization`.
    #[serde(default)]
    pub name: Option<String>,
    /// Value template containing `{key}`. Default `{key}` (static) / `Bearer {key}` (OAuth2).
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub route_requires_key: bool,
    /// OAuth2: `https://` token endpoint.
    #[serde(default)]
    pub token_url: Option<String>,
    /// OAuth2: variable holding `client_id`.
    #[serde(default)]
    pub env_id: Option<String>,
    /// OAuth2: variable holding `client_secret`.
    #[serde(default)]
    pub env_secret: Option<String>,
    /// OAuth2: default `basic`.
    #[serde(default)]
    pub style: Option<OAuthStyle>,
    /// OAuth2: optional space-delimited scope.
    #[serde(default)]
    pub scope: Option<String>,
}

impl AuthDecl {
    /// A static key in `in`/`name`.
    pub fn new_static(env: impl Into<String>, location: AuthIn, name: impl Into<String>) -> Self {
        Self {
            env: Some(env.into()),
            location: Some(location),
            name: Some(name.into()),
            ..Self::default()
        }
    }

    /// OAuth2 client credentials (Basic style, Bearer header).
    pub fn new_oauth2(
        token_url: impl Into<String>,
        env_id: impl Into<String>,
        env_secret: impl Into<String>,
    ) -> Self {
        Self {
            kind: AuthKind::Oauth2ClientCredentials,
            mode: AuthModeManifest::Required,
            token_url: Some(token_url.into()),
            env_id: Some(env_id.into()),
            env_secret: Some(env_secret.into()),
            ..Self::default()
        }
    }

    pub fn is_oauth2(&self) -> bool {
        self.kind == AuthKind::Oauth2ClientCredentials
    }

    /// Where the key/token is injected.
    pub fn effective_in(&self) -> AuthIn {
        self.location.unwrap_or(AuthIn::Header)
    }

    /// Field/header name carrying the key/token.
    pub fn effective_name(&self) -> &str {
        match (&self.name, self.is_oauth2()) {
            (Some(n), _) => n,
            (None, true) => "Authorization",
            (None, false) => "",
        }
    }

    /// Value template (contains `{key}`).
    pub fn effective_format(&self) -> &str {
        match (&self.format, self.is_oauth2()) {
            (Some(f), _) => f,
            (None, true) => "Bearer {key}",
            (None, false) => "{key}",
        }
    }

    pub fn effective_style(&self) -> OAuthStyle {
        self.style.unwrap_or_default()
    }

    /// Every credential variable this auth reads (all must be set).
    pub fn credential_envs(&self) -> Vec<&str> {
        [&self.env, &self.env_id, &self.env_secret]
            .into_iter()
            .filter_map(|e| e.as_deref())
            .collect()
    }

    /// The variable that represents the source's key in `SourceDecl::auth`
    /// (`env`, or `env_id` for OAuth2).
    /// The `env` shown in `AuthMode` / `Availability::NeedsKey` (A1 §B.1 #3):
    /// `"<env_id>,<env_secret>"` for OAuth2, else the static `env`.
    pub fn env_display(&self) -> String {
        match (&self.env_id, &self.env_secret) {
            (Some(id), Some(secret)) if self.is_oauth2() => format!("{id},{secret}"),
            _ => self.primary_env().to_owned(),
        }
    }

    pub fn primary_env(&self) -> &str {
        self.env.as_deref().or(self.env_id.as_deref()).unwrap_or("")
    }
}

/// `[claims]` (direct-answer claim routing).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ClaimsManifest {
    /// Rust `regex` syntax (no look-around).
    #[serde(default)]
    pub patterns: Vec<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default = "default_max_per_search")]
    pub max_per_search: u32,
}

impl Default for ClaimsManifest {
    fn default() -> Self {
        Self {
            patterns: Vec::new(),
            keywords: Vec::new(),
            max_per_search: default_max_per_search(),
        }
    }
}

fn default_max_per_search() -> u32 {
    3
}

/// `[op.<name>]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct OpSpec {
    pub summary: String,
    pub method: HttpMethod,
    /// May contain `{query}`, `{limit}`, `{cursor}`, `{lang}`, `{contact_email}`, `{key}`, `{pre}`, `{<param>}`.
    pub url: String,
    /// Mirrors tried in order when `url` fails (≤ 3, same template variables; A1 §A.6).
    #[serde(default)]
    pub fallback_urls: Vec<String>,
    /// Per-op auth override (A1 §A.1).
    #[serde(default)]
    pub auth: OpAuthOverride,
    /// Replace default headers case-insensitively (A1 §B.1).
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub response: ResponseFormat,
    #[serde(default)]
    pub csv: Option<CsvOpts>,
    #[serde(default)]
    pub body: Option<BodySpec>,
    /// Constant query pairs; arrays emit repeated keys (A1 §A.5).
    #[serde(default, with = "crate::serde_compat::json_compat")]
    #[schemars(with = "BTreeMap<String, ConstQueryVal>")]
    pub const_query: BTreeMap<String, ConstQueryVal>,
    /// `jaq` filter returning `[[key, value], ...]` appended to the query (A1 §A.4).
    #[serde(default)]
    pub query_jaq: Option<String>,
    pub returns: ReturnKind,
    #[serde(default)]
    pub params: OrderedMap<ParamDecl>,
    /// At most one pre-request step.
    #[serde(default)]
    pub pre: Vec<PreStepDecl>,
    #[serde(default)]
    pub map: Option<MapSpec>,
    #[serde(default)]
    pub html: Option<HtmlSpec>,
    #[serde(default)]
    pub paginate: Option<PaginateSpec>,
    #[serde(default)]
    pub probe: Option<ProbeManifest>,
    /// Copy-ready call examples for the catalog (feeds `OpDecl::examples`).
    #[serde(default)]
    pub examples: Vec<String>,
}

/// `[op.<name>.params.<p>]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ParamDecl {
    #[serde(default, rename = "type")]
    pub ty: ParamType,
    #[serde(default)]
    pub required: bool,
    #[serde(default, with = "crate::serde_compat::json_compat")]
    #[schemars(with = "Option<Value>")]
    pub default: Option<Value>,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    #[serde(default, rename = "enum")]
    pub enum_values: Option<Vec<String>>,
    #[serde(default, rename = "in")]
    pub location: ParamLocation,
    /// Literal outgoing key (default: the param name). Nested POST JSON paths
    /// live in `body.jaq`, never here. Forbidden with `in = "none"`.
    #[serde(default)]
    pub to: Option<String>,
    /// Value template, e.g. `"all:{query}"`.
    #[serde(default)]
    pub template: Option<String>,
    #[serde(default)]
    pub doc: String,
    /// `string_array` + `in = "query"` only; default `repeat`.
    #[serde(default)]
    pub array: Option<ArrayStyle>,
    /// `in = "path"` only; default `segment`.
    #[serde(default)]
    pub encode: Option<PathEncode>,
}

/// `[[op.<name>.pre]]`: one request whose `bind` result is exposed as `{pre}`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct PreStepDecl {
    #[serde(default)]
    pub method: HttpMethod,
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default, with = "crate::serde_compat::json_compat")]
    #[schemars(with = "BTreeMap<String, ConstQueryVal>")]
    pub const_query: BTreeMap<String, ConstQueryVal>,
    /// Templated query pairs, e.g. `{ term = "{query}" }`.
    #[serde(default, with = "crate::serde_compat::json_compat")]
    #[schemars(with = "BTreeMap<String, ConstQueryVal>")]
    pub params: BTreeMap<String, ConstQueryVal>,
    #[serde(default)]
    pub response: ResponseFormat,
    /// `jaq` expression producing the `{pre}` value.
    pub bind: String,
    /// Body for `POST` pre-flight steps (A1 §A.7).
    #[serde(default)]
    pub body: Option<BodySpec>,
    #[serde(default)]
    pub auth: PreAuth,
    #[serde(default)]
    pub on_empty: OnEmpty,
}

/// `[op.<name>.map]` (`jaq` expressions, or `"@ref:op.<other>.map.<field>"`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct MapSpec {
    /// A `jaq` stream of item values (A1 §B.5: `"."`, not `"[.]"`).
    pub items: String,
    #[serde(default)]
    pub next_cursor: Option<String>,
    #[serde(default)]
    pub total: Option<String>,
    /// Non-null result ⇒ `FsmError::Upstream`.
    #[serde(default)]
    pub error: Option<String>,
    /// → `ScholarlyWork` (camelCase keys).
    #[serde(default)]
    pub work: Option<String>,
    /// → record (entity caps: `GrantRecord`/`AuthorProfile`/`OrgProfile`/`VenueProfile`, M13).
    #[serde(default)]
    pub record: Option<String>,
    /// → `Hit` (M3 defaults `source`/`sources`/`score`/`kind`).
    #[serde(default)]
    pub hit: Option<String>,
}

/// Names of the [`MapSpec`] fields (targets of `@ref`).
pub const MAP_FIELDS: [&str; 7] = [
    "items",
    "next_cursor",
    "total",
    "error",
    "work",
    "record",
    "hit",
];

impl MapSpec {
    /// A field by name.
    pub fn field(&self, name: &str) -> Option<&str> {
        match name {
            "items" => Some(self.items.as_str()),
            "next_cursor" => self.next_cursor.as_deref(),
            "total" => self.total.as_deref(),
            "error" => self.error.as_deref(),
            "work" => self.work.as_deref(),
            "record" => self.record.as_deref(),
            "hit" => self.hit.as_deref(),
            _ => None,
        }
    }

    fn fields(&self) -> impl Iterator<Item = (&'static str, &str)> {
        MAP_FIELDS
            .into_iter()
            .filter_map(|f| self.field(f).map(|v| (f, v)))
    }
}

/// `[op.<name>.html]`: CSS-selector pipeline for HTML responses (grammar:
/// A1 §B.4); its JSON array of records may feed `[op.<name>.map]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct HtmlSpec {
    pub items_css: String,
    /// Output key → `"<css> | <filter>"`, in document order.
    pub fields: OrderedMap<String>,
    /// Page text that marks a block/consent wall (⇒ upstream error).
    #[serde(default)]
    pub wall_markers: Vec<String>,
}

/// `[op.<name>.paginate]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct PaginateSpec {
    pub style: PaginateStyle,
    /// Outgoing parameter carrying the page/offset/cursor/token (not used by `link_header`).
    #[serde(default)]
    pub param: Option<String>,
    #[serde(default)]
    pub start: Option<i64>,
    #[serde(default)]
    pub max_pages: Option<u32>,
}

/// `probe = {..}` of an op, and of a protocol instance (which also names the
/// adapter `op`). The TOML/postcard-safe form of [`ProbeSpec`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ProbeManifest {
    /// Instances only.
    #[serde(default)]
    pub op: Option<String>,
    #[serde(default, with = "crate::serde_compat::json_compat")]
    #[schemars(with = "BTreeMap<String, Value>")]
    pub params: BTreeMap<String, Value>,
    #[serde(default = "default_min_results")]
    pub min_results: usize,
    /// `fixtures/<wave>/<id>/<op>/<case>.json` (M5); `None` = `.../default.json`.
    #[serde(default)]
    pub fixture: Option<String>,
}

impl Default for ProbeManifest {
    fn default() -> Self {
        Self {
            op: None,
            params: BTreeMap::new(),
            min_results: default_min_results(),
            fixture: None,
        }
    }
}

fn default_min_results() -> usize {
    1
}

impl ProbeManifest {
    pub fn to_spec(&self) -> ProbeSpec {
        let mut spec = ProbeSpec::new(
            Value::Object(self.params.clone().into_iter().collect()),
            self.min_results,
        );
        spec.op = self.op.clone();
        spec.fixture = self.fixture.clone();
        spec
    }

    /// The fixture path: explicit, or `fixtures/<wave>/<id>/<op>/default.json`.
    pub fn fixture_path(&self, wave: &str, id: &str, op: &str) -> String {
        self.fixture
            .clone()
            .unwrap_or_else(|| format!("fixtures/{wave}/{id}/{op}/default.json"))
    }
}

/// `csv = { header, delimiter }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct CsvOpts {
    #[serde(default = "default_true")]
    pub header: bool,
    #[serde(default = "default_delimiter")]
    pub delimiter: String,
}

impl Default for CsvOpts {
    fn default() -> Self {
        Self {
            header: true,
            delimiter: default_delimiter(),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_delimiter() -> String {
    ",".to_owned()
}

/// `body = {..}` (wire contract: A1 §B.7). `graphql` uses `query` (+ optional
/// `variables_jaq`, `operation_name`); every other kind uses `jaq` over `$p`/`$cfg`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct BodySpec {
    pub kind: BodyKind,
    #[serde(default)]
    pub jaq: Option<String>,
    /// GraphQL document.
    #[serde(default)]
    pub query: Option<String>,
    /// GraphQL: `jaq` producing the `variables` object (default `{}`).
    #[serde(default)]
    pub variables_jaq: Option<String>,
    /// GraphQL `operationName`.
    #[serde(default)]
    pub operation_name: Option<String>,
}

impl BodySpec {
    /// A `json`/`form`/`sparql` body built by `jaq`.
    pub fn jaq(kind: BodyKind, jaq: impl Into<String>) -> Self {
        Self {
            kind,
            jaq: Some(jaq.into()),
            ..Self::default()
        }
    }

    /// A GraphQL body.
    pub fn graphql(query: impl Into<String>, variables_jaq: Option<String>) -> Self {
        Self {
            kind: BodyKind::Graphql,
            query: Some(query.into()),
            variables_jaq,
            ..Self::default()
        }
    }

    fn problems(&self, at: &str, p: &mut Vec<String>) {
        let empty = |o: &Option<String>| o.as_deref().is_none_or(|s| s.trim().is_empty());
        if self.kind == BodyKind::Graphql {
            if empty(&self.query) {
                p.push(format!(
                    "{at}: body.kind = \"graphql\" needs a non-empty query"
                ));
            }
            if self.jaq.is_some() {
                p.push(format!(
                    "{at}: body.kind = \"graphql\" uses query/variables_jaq, not jaq"
                ));
            }
        } else {
            if empty(&self.jaq) {
                p.push(format!(
                    "{at}: body.kind = {:?} needs a non-empty jaq",
                    self.kind
                ));
            }
            if self.query.is_some() || self.variables_jaq.is_some() || self.operation_name.is_some()
            {
                p.push(format!(
                    "{at}: query/variables_jaq/operation_name are only for body.kind = \"graphql\""
                ));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Protocol instance tables (§3, A1 §D)
// ---------------------------------------------------------------------------

/// One `crates/manifests/instances/<wave>/*.toml` file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct InstanceFileDecl {
    /// Must match a registered `ProtocolAdapter::kind()`.
    pub protocol: String,
    #[serde(default)]
    pub instance: Vec<InstanceDecl>,
}

/// `[[instance]]` (A1 §D.1).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct InstanceDecl {
    pub id: String,
    pub name: String,
    pub summary: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub base_url: String,
    pub scopes: Vec<String>,
    pub caps: Vec<Capability>,
    pub tier: u8,
    /// Default `plain` (`local` is rejected).
    #[serde(default)]
    pub transport: TransportProfile,
    /// Default: `scope_only` with the `search` cap, else `explicit`.
    #[serde(default)]
    pub pool_role: Option<PoolRole>,
    /// Default: [`default_instance_limits`].
    #[serde(default)]
    pub limits: Option<SourceLimits>,
    /// Shallow-merged under caller params (caller wins); every key must be a
    /// param of some adapter op (checked at registration).
    #[serde(default, with = "crate::serde_compat::json_compat")]
    #[schemars(with = "BTreeMap<String, Value>")]
    pub default_params: BTreeMap<String, Value>,
    /// Adapter-specific settings, checked by `ProtocolAdapter::validate_dialect`
    /// and copied into `SrcCx::dialect`.
    #[serde(default, with = "crate::serde_compat::json_compat")]
    #[schemars(with = "BTreeMap<String, Value>")]
    pub dialect: BTreeMap<String, Value>,
    /// `op` is required; `fixture` optional.
    pub probe: ProbeManifest,
    #[serde(default)]
    pub home_url: Option<String>,
}

/// Limits of an instance without `limits` (A1 §D.2): 2 rps, burst 4,
/// concurrency 2, 8 s, 6 h cache, bucket = the `base_url` host.
pub fn default_instance_limits(base_url: &str) -> SourceLimits {
    let mut l = SourceLimits::new(2.0, 4, 2, 8000, 21_600);
    let host = crate::url_canon::host_of(base_url);
    l.host_bucket = (!host.is_empty()).then_some(host);
    l
}

// ---------------------------------------------------------------------------
// Parsing, validation, conversion
// ---------------------------------------------------------------------------

/// Template variables every op may use without declaring them. A declared
/// param of the same name shadows the builtin (`{key}` is then the param;
/// declaring `key` in a manifest with `[auth]` is rejected as ambiguous).
pub const BUILTIN_TEMPLATE_VARS: [&str; 7] = [
    "query",
    "limit",
    "cursor",
    "lang",
    "contact_email",
    "key",
    "pre",
];

/// Maximum number of `fallback_urls`.
pub const MAX_FALLBACK_URLS: usize = 3;

const ENTITY_CAPS: [Capability; 4] = [
    Capability::Grant,
    Capability::Author,
    Capability::Org,
    Capability::Venue,
];

static PLACEHOLDER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{([A-Za-z_][A-Za-z0-9_]*)\}").expect("regex"));
/// An A1 §B.4 `html.fields` extractor (the part after the last `|`).
static HTML_EXTRACTOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:text|html|texts|(?:attr|attrs)\(\s*[A-Za-z_][A-Za-z0-9_:.-]*\s*\))$")
        .expect("regex")
});
static OP_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z][a-z0-9_]{0,39}$").expect("regex"));
static KEY_ENV: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^SEARCH_MCP_[A-Z0-9_]+(_API_KEY|_TOKEN|_KEY|_SECRET|_USERNAME|_CLIENT_ID)$")
        .expect("regex")
});
static FIXTURE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^fixtures/([a-z0-9_]+)/([a-z][a-z0-9_]{1,39})/([a-z][a-z0-9_]{0,39})/([a-z0-9_-]+)\.json$")
        .expect("regex")
});
static PARAM_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_.\[\]-]{0,63}$").expect("regex"));
static REF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^@ref:op\.([a-z][a-z0-9_]{0,39})\.map\.([a-z_]+)$").expect("regex")
});

/// Template variables (`{name}`) used in a string.
pub fn placeholders(template: &str) -> Vec<&str> {
    PLACEHOLDER
        .captures_iter(template)
        .filter_map(|c| c.get(1))
        .map(|m| m.as_str())
        .collect()
}

/// Check a probe fixture path against M5 (`fixtures/<wave>/<id>/<op>/<case>.json`);
/// returns `(wave, case)`.
pub fn check_fixture_path<'a>(
    path: &'a str,
    id: &str,
    op: &str,
) -> Result<(&'a str, &'a str), FsmError> {
    let c = FIXTURE.captures(path).ok_or_else(|| {
        FsmError::invalid(format!(
            "fixture path {path:?} must look like fixtures/<wave>/{id}/{op}/<case>.json"
        ))
    })?;
    if &c[2] != id || &c[3] != op {
        return Err(FsmError::invalid(format!(
            "fixture path {path:?} names {}/{} but belongs to {id}/{op}",
            &c[2], &c[3]
        )));
    }
    let wave = c.get(1).map_or("", |m| m.as_str());
    let case = c.get(4).map_or("", |m| m.as_str());
    Ok((wave, case))
}

/// A1 §B.5: an `items` expression that wraps everything in one array
/// (`"[.]"`, `"[.data]"`) emits a single array value, not a stream of objects.
pub fn items_wraps_single_array(items: &str) -> bool {
    let t = items.trim();
    t.starts_with('[') && t.ends_with(']') && !t.ends_with("[]")
}

fn is_http_url(s: &str) -> bool {
    let l = s.trim().to_ascii_lowercase();
    (l.starts_with("https://") || l.starts_with("http://")) && l.len() > "https://".len()
}

fn check_common(
    p: &mut Vec<String>,
    id: &str,
    name: &str,
    summary: &str,
    tier: u8,
    scopes_: &[String],
    home_url: Option<&str>,
) {
    if !is_valid_id(id) {
        p.push(format!("id {id:?} must match ^[a-z][a-z0-9_]{{1,39}}$"));
    }
    if name.trim().is_empty() {
        p.push("name is empty".into());
    }
    if summary.trim().is_empty() {
        p.push("summary is empty".into());
    }
    if !(1..=3).contains(&tier) {
        p.push(format!("tier {tier} must be 1, 2 or 3"));
    }
    if scopes_.is_empty() {
        p.push("scopes is empty".into());
    }
    for s in scopes_ {
        if scopes::get(s).is_none() {
            let hint = scopes::resolve(s)
                .map(|d| format!(" (did you mean {:?}?)", d.id))
                .unwrap_or_default();
            p.push(format!(
                "scope {s:?} is not a canonical scope id{hint}; valid: {}",
                scopes::valid_scope_list()
            ));
        }
    }
    if let Some(u) = home_url
        && !is_http_url(u)
    {
        p.push(format!("home_url {u:?} must be an http(s) URL"));
    }
}

fn check_limits(p: &mut Vec<String>, what: &str, l: &SourceLimits) {
    if !(l.rps > 0.0 && l.rps.is_finite()) {
        p.push(format!("{what}.rps must be > 0"));
    }
    if l.burst == 0 {
        p.push(format!("{what}.burst must be >= 1"));
    }
    if l.concurrency == 0 {
        p.push(format!("{what}.concurrency must be >= 1"));
    }
    if l.timeout_ms == 0 {
        p.push(format!("{what}.timeout_ms must be > 0"));
    }
}

fn check_auth(p: &mut Vec<String>, a: &AuthDecl) {
    let bad_env = |e: &str| !KEY_ENV.is_match(e);
    let env_rule = "SEARCH_MCP_<NAME>_API_KEY|_TOKEN|_KEY|_SECRET|_USERNAME|_CLIENT_ID";
    match a.kind {
        AuthKind::Static => {
            match a.env.as_deref() {
                None => p.push("auth.env is required for kind = \"static\"".into()),
                Some(e) if bad_env(e) => p.push(format!("auth.env {e:?} must match {env_rule}")),
                Some(_) => {}
            }
            if a.location.is_none() {
                p.push("auth.in is required for kind = \"static\"".into());
            }
            if a.name.as_deref().is_none_or(|n| n.trim().is_empty()) {
                p.push("auth.name is required for kind = \"static\"".into());
            }
            for (field, set) in [
                ("token_url", a.token_url.is_some()),
                ("env_id", a.env_id.is_some()),
                ("env_secret", a.env_secret.is_some()),
                ("style", a.style.is_some()),
                ("scope", a.scope.is_some()),
            ] {
                if set {
                    p.push(format!(
                        "auth.{field} is only for kind = \"oauth2_client_credentials\""
                    ));
                }
            }
        }
        AuthKind::Oauth2ClientCredentials => {
            if a.env.is_some() {
                p.push("auth.env must not be set for kind = \"oauth2_client_credentials\" (use env_id/env_secret)".into());
            }
            match a.token_url.as_deref() {
                Some(u) if u.trim().to_ascii_lowercase().starts_with("https://") => {}
                Some(u) => p.push(format!("auth.token_url {u:?} must start with https://")),
                None => p.push(
                    "auth.token_url is required for kind = \"oauth2_client_credentials\"".into(),
                ),
            }
            for (field, v) in [("env_id", &a.env_id), ("env_secret", &a.env_secret)] {
                match v.as_deref() {
                    None => p.push(format!(
                        "auth.{field} is required for kind = \"oauth2_client_credentials\""
                    )),
                    Some(e) if bad_env(e) => {
                        p.push(format!("auth.{field} {e:?} must match {env_rule}"))
                    }
                    Some(_) => {}
                }
            }
            if a.name.as_deref().is_some_and(|n| n.trim().is_empty()) {
                p.push("auth.name is empty".into());
            }
        }
    }
    if !a.effective_format().contains("{key}") {
        p.push(format!(
            "auth.format {:?} must contain {{key}}",
            a.effective_format()
        ));
    }
}

impl ManifestDecl {
    /// Parse a manifest from TOML (unknown fields are errors).
    pub fn from_toml(text: &str) -> Result<Self, FsmError> {
        toml::from_str(text).map_err(|e| FsmError::invalid(format!("manifest TOML: {e}")))
    }

    /// Parse and [`validate`](Self::validate).
    pub fn parse_validated(text: &str) -> Result<Self, FsmError> {
        let m = Self::from_toml(text)?;
        m.validate()?;
        Ok(m)
    }

    /// All problems found, in a stable order (empty = valid).
    ///
    /// Spec rules (§2, A1 §A): id regex; scopes ∈ `SCOPES`; cap `search` ⇒
    /// `op.search`; `op.search` returning records ⇒ `map.hit` (M3); ≤ 1 `pre`;
    /// probe fixture path (M5); auth kinds (A1 §A.2); `op.auth = "required"` ⇒
    /// `[auth]`; `in = "none"` ⇒ no `to`; `type = "json"` ⇒ `in = none|body`;
    /// `array` only on query `string_array`; `encode` only on path params;
    /// ≤ 3 `fallback_urls` with the URL's template variables; body kind fields
    /// (A1 §A.8); single-array `map.items` rejected (A1 §B.5). Structural rules
    /// (a manifest breaking them cannot run): tier 1–3, ≥ 1 op, valid names,
    /// positive limits, http(s) URLs, declared template variables, path params
    /// present in the URL, `{key}` only with usable auth and `{pre}` only with a
    /// pre step, output mapping for the declared `returns`, `html`/`csv` matching
    /// `response`, bodies only on POST, `@ref` targets that exist, pagination
    /// parameter, consistent defaults/enums/ranges, probes passing exactly the
    /// declared required params, credential-suffixed env names, compiling claim
    /// patterns, entity caps (M13) backed by a records op. `jaq` programs
    /// (incl. `query_jaq`) are compiled by W2a, not here.
    pub fn problems(&self) -> Vec<String> {
        let mut p = Vec::new();
        check_common(
            &mut p,
            &self.id,
            &self.name,
            &self.summary,
            self.tier,
            &self.scopes,
            self.home_url.as_deref(),
        );
        check_limits(&mut p, "limits", &self.limits);
        if self.op.is_empty() {
            p.push("at least one [op.<name>] is required".into());
        }
        if self.caps.contains(&Capability::Search) && !self.op.contains_key("search") {
            p.push("caps contains \"search\" but there is no [op.search]".into());
        }
        if self.caps.iter().any(|c| ENTITY_CAPS.contains(c))
            && !self
                .op
                .iter()
                .any(|(_, o)| o.returns == ReturnKind::Records)
        {
            p.push(
                "entity caps (grant/author/org/venue) need an op returning records (M13)".into(),
            );
        }
        if let Some(a) = &self.auth {
            check_auth(&mut p, a);
        }
        if let Some(c) = &self.claims {
            for pat in &c.patterns {
                if let Err(e) = Regex::new(pat) {
                    p.push(format!("claims pattern {pat:?} does not compile: {e}"));
                }
            }
        }
        for (name, op) in self.op.iter() {
            self.check_op(&mut p, name, op);
        }
        p
    }

    fn check_op(&self, p: &mut Vec<String>, name: &str, op: &OpSpec) {
        let at = format!("op.{name}");
        if !OP_NAME.is_match(name) {
            p.push(format!("{at}: op name must match ^[a-z][a-z0-9_]{{0,39}}$"));
        }
        if op.summary.trim().is_empty() {
            p.push(format!("{at}.summary is empty"));
        }
        if !is_http_url(&op.url) {
            p.push(format!("{at}.url {:?} must be an http(s) URL", op.url));
        }
        if op.auth == OpAuthOverride::Required && self.auth.is_none() {
            p.push(format!("{at}.auth = \"required\" needs a top-level [auth]"));
        }
        let key_usable = self.auth.is_some() && op.auth != OpAuthOverride::None;
        // Template variables.
        let declared: BTreeSet<&str> = op.params.keys().collect();
        let check_vars = |p: &mut Vec<String>,
                          what: &str,
                          text: &str,
                          allow_pre: bool,
                          key_ok: bool| {
            for v in placeholders(text) {
                if declared.contains(v) {
                    // A declared param shadows the builtin of the same name
                    // (e.g. an SDMX path param `key`), unless that is ambiguous.
                    if v == "key" && self.auth.is_some() {
                        p.push(format!("{at}.{what}: {{key}} is ambiguous (a declared param and the [auth] key)"));
                    }
                    continue;
                }
                if v == "key" && !key_ok {
                    p.push(format!(
                        "{at}.{what} uses {{key}} but no [auth] key is injected there (no [auth], op.auth = \"none\" or pre.auth = \"none\")"
                    ));
                } else if v == "pre" && (!allow_pre || op.pre.is_empty()) {
                    p.push(format!(
                        "{at}.{what} uses {{pre}} without a usable pre step"
                    ));
                } else if !BUILTIN_TEMPLATE_VARS.contains(&v) {
                    p.push(format!(
                        "{at}.{what} uses undeclared template variable {{{v}}}"
                    ));
                }
            }
        };
        check_vars(p, "url", &op.url, true, key_usable);
        for (i, u) in op.fallback_urls.iter().enumerate() {
            check_vars(p, &format!("fallback_urls[{i}]"), u, true, key_usable);
        }
        for (h, v) in &op.headers {
            check_vars(p, &format!("headers.{h}"), v, true, key_usable);
        }
        for (k, v) in &op.const_query {
            for s in v.values() {
                check_vars(p, &format!("const_query.{k}"), s, true, key_usable);
            }
        }
        for (pn, param) in op.params.iter() {
            if let Some(t) = &param.template {
                check_vars(p, &format!("params.{pn}.template"), t, true, key_usable);
            }
        }
        // Fallback mirrors.
        if op.fallback_urls.len() > MAX_FALLBACK_URLS {
            p.push(format!(
                "{at}.fallback_urls has {} entries (max {MAX_FALLBACK_URLS})",
                op.fallback_urls.len()
            ));
        }
        let url_vars: BTreeSet<&str> = placeholders(&op.url).into_iter().collect();
        for (i, u) in op.fallback_urls.iter().enumerate() {
            if !is_http_url(u) {
                p.push(format!(
                    "{at}.fallback_urls[{i}] {u:?} must be an http(s) URL"
                ));
            }
            let vars: BTreeSet<&str> = placeholders(u).into_iter().collect();
            if vars != url_vars {
                p.push(format!(
                    "{at}.fallback_urls[{i}] must use the same template variables as url"
                ));
            }
        }
        if op.query_jaq.as_deref().is_some_and(|q| q.trim().is_empty()) {
            p.push(format!("{at}.query_jaq is empty"));
        }
        // Pre-flight.
        if op.pre.len() > 1 {
            p.push(format!(
                "{at}: at most one [[{at}.pre]] step is allowed, found {}",
                op.pre.len()
            ));
        }
        for (i, pre) in op.pre.iter().enumerate() {
            let pa = format!("{at}.pre[{i}]");
            if !is_http_url(&pre.url) {
                p.push(format!("{pa}.url {:?} must be an http(s) URL", pre.url));
            }
            // A1 §B.2: url, headers, const_query and params are templated;
            // {key} only when the step inherits the op's auth.
            let pre_key = key_usable && pre.auth == PreAuth::Inherit;
            check_vars(p, &format!("pre[{i}].url"), &pre.url, false, pre_key);
            for (h, v) in &pre.headers {
                check_vars(p, &format!("pre[{i}].headers.{h}"), v, false, pre_key);
            }
            for (k, v) in pre.params.iter().chain(&pre.const_query) {
                for s in v.values() {
                    check_vars(p, &format!("pre[{i}].{k}"), s, false, pre_key);
                }
            }
            if pre.bind.trim().is_empty() {
                p.push(format!("{pa}.bind is empty"));
            }
            if let Some(b) = &pre.body {
                b.problems(&pa, p);
                if pre.method != HttpMethod::Post {
                    p.push(format!("{pa}.body requires method = \"POST\""));
                }
            }
        }
        // Params.
        for (pn, param) in op.params.iter() {
            let pa = format!("{at}.params.{pn}");
            if !PARAM_NAME.is_match(pn) {
                p.push(format!("{pa}: invalid parameter name"));
            }
            match param.location {
                ParamLocation::Path => {
                    if !placeholders(&op.url).contains(&pn) {
                        p.push(format!(
                            "{pa} is in = \"path\" but {{{pn}}} is not in the url"
                        ));
                    }
                    if !param.required && param.default.is_none() {
                        p.push(format!(
                            "{pa} is a path param: it must be required or have a default"
                        ));
                    }
                }
                ParamLocation::Body if op.body.is_none() => {
                    p.push(format!("{pa} is in = \"body\" but the op has no body"));
                }
                ParamLocation::None if param.to.is_some() => {
                    p.push(format!(
                        "{pa} is in = \"none\" (never transmitted), so it cannot have to"
                    ));
                }
                _ => {}
            }
            if param.ty == ParamType::Json
                && !matches!(param.location, ParamLocation::None | ParamLocation::Body)
            {
                p.push(format!(
                    "{pa}: type = \"json\" needs in = \"none\" or in = \"body\""
                ));
            }
            if param.array.is_some()
                && !(param.ty == ParamType::StringArray && param.location == ParamLocation::Query)
            {
                p.push(format!(
                    "{pa}.array is only for type = \"string_array\" with in = \"query\""
                ));
            }
            if param.encode.is_some() && param.location != ParamLocation::Path {
                p.push(format!("{pa}.encode is only for in = \"path\""));
            }
            if param.to.as_deref().is_some_and(|t| t.trim().is_empty()) {
                p.push(format!("{pa}.to is empty"));
            }
            if let Some(d) = &param.default
                && !param.ty.accepts(d)
            {
                p.push(format!("{pa}.default {d} is not a {}", param.ty.ts_name()));
            }
            if let (Some(lo), Some(hi)) = (param.min, param.max)
                && lo > hi
            {
                p.push(format!("{pa}: min {lo} > max {hi}"));
            }
            if let (Some(n), Some(lo)) = (param.default.as_ref().and_then(Value::as_f64), param.min)
                && n < lo
            {
                p.push(format!("{pa}.default {n} < min {lo}"));
            }
            if let (Some(n), Some(hi)) = (param.default.as_ref().and_then(Value::as_f64), param.max)
                && n > hi
            {
                p.push(format!("{pa}.default {n} > max {hi}"));
            }
            if let Some(e) = &param.enum_values {
                if e.is_empty() {
                    p.push(format!("{pa}.enum is empty"));
                }
                if let Some(d) = param.default.as_ref().and_then(Value::as_str)
                    && !e.iter().any(|x| x == d)
                {
                    p.push(format!("{pa}.default {d:?} is not one of enum {e:?}"));
                }
            }
        }
        // Body / response shape.
        if let Some(b) = &op.body {
            b.problems(&at, p);
            if op.method != HttpMethod::Post {
                p.push(format!("{at}.body requires method = \"POST\""));
            }
        }
        if op.html.is_some() && op.response != ResponseFormat::Html {
            p.push(format!("{at}.html requires response = \"html\""));
        }
        if op.response == ResponseFormat::Html && op.html.is_none() {
            p.push(format!(
                "{at} has response = \"html\" but no [op.{name}.html] (A1 §B.4)"
            ));
        }
        if op
            .csv
            .as_ref()
            .is_some_and(|_| op.response != ResponseFormat::Csv)
        {
            p.push(format!("{at}.csv requires response = \"csv\""));
        }
        if let Some(c) = &op.csv
            && c.delimiter.chars().count() != 1
        {
            p.push(format!("{at}.csv.delimiter must be one character"));
        }
        if let Some(h) = &op.html {
            if h.items_css.trim().is_empty() || h.fields.is_empty() {
                p.push(format!("{at}.html needs items_css and at least one field"));
            }
            // A1 §B.4: "[<css_selector> ]| <extractor>"; no "|" = extractor on the item node.
            for (field, expr) in h.fields.iter() {
                let ext = expr
                    .rsplit_once('|')
                    .map_or(expr.as_str(), |(_, e)| e)
                    .trim();
                if !HTML_EXTRACTOR.is_match(ext) {
                    p.push(format!(
                        "{at}.html.fields.{field}: unknown extractor {ext:?} (A1 §B.4: text, html, texts, attr(<name>) or attrs(<name>))"
                    ));
                }
            }
        }
        // Output mapping for the declared return kind.
        let map = op.map.as_ref();
        let has = |f: &str| map.and_then(|m| m.field(f)).is_some();
        match op.returns {
            ReturnKind::Works if !has("work") => {
                p.push(format!("{at} returns works but has no map.work"))
            }
            ReturnKind::Hits if !has("hit") => {
                p.push(format!("{at} returns hits but has no map.hit"))
            }
            ReturnKind::Records if map.is_none() && op.html.is_none() => {
                p.push(format!("{at} returns records but has neither map nor html"))
            }
            _ => {}
        }
        // A1 §B.4: html rows without a map are records of a non-search op only
        // (hits/works need map.hit/map.work; search needs map.hit, M3).
        if op.html.is_some()
            && map.is_none()
            && matches!(op.returns, ReturnKind::Text | ReturnKind::Raw)
        {
            p.push(format!(
                "{at}: html without [op.{name}.map] is allowed only for returns = \"records\" (A1 §B.4)"
            ));
        }
        if name == "search" && op.returns == ReturnKind::Records && !has("hit") {
            p.push(format!("{at} returns records, so map.hit is required (M3)"));
        }
        if let Some(m) = map {
            if items_wraps_single_array(&m.items) {
                p.push(format!(
                    "{at}.map.items {:?} emits one array, not a stream of items (A1 §B.5): use \".\" or \".data\" instead of \"[.]\" or \"[.data]\"",
                    m.items
                ));
            }
            for (field, expr) in m.fields() {
                if expr.trim().is_empty() {
                    p.push(format!("{at}.map.{field} is empty"));
                }
                if expr.starts_with("@ref:")
                    && let Err(e) = self.resolve_ref(expr)
                {
                    p.push(format!("{at}.map.{field}: {e}"));
                }
            }
        }
        if let Some(pg) = &op.paginate {
            if pg.style != PaginateStyle::LinkHeader
                && pg.param.as_deref().is_none_or(|s| s.trim().is_empty())
            {
                p.push(format!(
                    "{at}.paginate.param is required for style {:?}",
                    pg.style
                ));
            }
            if pg.max_pages == Some(0) {
                p.push(format!("{at}.paginate.max_pages must be >= 1"));
            }
        }
        if let Some(pr) = &op.probe {
            if pr.op.is_some() {
                p.push(format!(
                    "{at}.probe.op is only allowed in protocol instances"
                ));
            }
            for k in pr.params.keys() {
                if !declared.contains(k.as_str()) {
                    p.push(format!("{at}.probe passes undeclared param {k:?}"));
                }
            }
            for (pn, param) in op.params.iter() {
                if param.required && param.default.is_none() && !pr.params.contains_key(pn) {
                    p.push(format!("{at}.probe lacks required param {pn:?}"));
                }
            }
            if let Some(f) = &pr.fixture
                && let Err(e) = check_fixture_path(f, &self.id, name)
            {
                p.push(format!("{at}.probe: {e}"));
            }
        }
    }

    /// Enforce [`problems`](Self::problems): `Err(InvalidInput)` listing every problem.
    pub fn validate(&self) -> Result<(), FsmError> {
        let p = self.problems();
        if p.is_empty() {
            Ok(())
        } else {
            Err(FsmError::invalid(format!(
                "manifest {:?}: {}",
                self.id,
                p.join("; ")
            )))
        }
    }

    /// Resolve a map expression: `"@ref:op.<other>.map.<field>"` → the referenced
    /// expression (one level; a reference to a reference is an error); any
    /// other string is returned as is.
    pub fn resolve_ref<'a>(&'a self, expr: &'a str) -> Result<&'a str, FsmError> {
        if !expr.starts_with("@ref:") {
            return Ok(expr);
        }
        let c = REF.captures(expr).ok_or_else(|| {
            FsmError::invalid(format!("{expr:?} must look like @ref:op.<op>.map.<field>"))
        })?;
        let (op, field) = (
            c.get(1).map_or("", |m| m.as_str()),
            c.get(2).map_or("", |m| m.as_str()),
        );
        let target = self
            .op
            .get(op)
            .ok_or_else(|| FsmError::invalid(format!("{expr:?} refers to missing op {op:?}")))?
            .map
            .as_ref()
            .and_then(|m| m.field(field))
            .ok_or_else(|| {
                FsmError::invalid(format!("{expr:?} refers to missing op.{op}.map.{field}"))
            })?;
        if target.starts_with("@ref:") {
            return Err(FsmError::invalid(format!(
                "{expr:?} refers to another @ref"
            )));
        }
        Ok(target)
    }

    /// The registry declaration this manifest describes. OAuth2 sources are
    /// represented by their `env_id` (both `env_id` and `env_secret` must be set).
    pub fn to_source_decl(&self) -> SourceDecl {
        let mut d = SourceDecl::new(&self.id, &self.name, &self.summary);
        d.aliases = self.aliases.clone();
        d.home_url = self.home_url.clone();
        d.tier = self.tier;
        d.scopes = self.scopes.clone();
        d.caps = self.caps.clone();
        d.auth = match &self.auth {
            None => AuthMode::None,
            Some(a) if a.mode == AuthModeManifest::Required => AuthMode::RequiredKey {
                env: a.env_display(),
            },
            Some(a) => AuthMode::OptionalKey {
                env: a.env_display(),
                route_requires_key: a.route_requires_key,
            },
        };
        d.transport = self.transport.profile();
        d.pool_role = self.pool_role.clone();
        d.limits = self.limits.clone();
        d.ops = self
            .op
            .iter()
            .map(|(name, op)| op.to_op_decl(name))
            .collect();
        d
    }
}

impl OpSpec {
    /// The catalog/registry view of this op.
    pub fn to_op_decl(&self, name: &str) -> OpDecl {
        let mut o = OpDecl::new(name, &self.summary, self.returns);
        o.params = self.params.iter().map(|(pn, p)| p.to_param(pn)).collect();
        o.examples = self.examples.clone();
        o.probe = self.probe.as_ref().map(ProbeManifest::to_spec);
        o.auth = self.auth;
        o
    }
}

impl ParamDecl {
    pub fn new(ty: ParamType, doc: impl Into<String>) -> Self {
        Self {
            ty,
            doc: doc.into(),
            ..Self::default()
        }
    }

    pub fn to_param(&self, name: &str) -> Param {
        Param {
            name: name.to_owned(),
            ty: self.ty,
            required: self.required,
            default: self.default.clone(),
            min: self.min,
            max: self.max,
            enum_values: self.enum_values.clone(),
            doc: self.doc.clone(),
        }
    }

    /// Outgoing key (`to`, else the param name).
    pub fn wire_name<'a>(&'a self, name: &'a str) -> &'a str {
        self.to.as_deref().unwrap_or(name)
    }

    pub fn array_style(&self) -> ArrayStyle {
        self.array.unwrap_or_default()
    }

    pub fn path_encode(&self) -> PathEncode {
        self.encode.unwrap_or_default()
    }
}

impl InstanceFileDecl {
    /// Parse an instance table from TOML (unknown fields are errors).
    pub fn from_toml(text: &str) -> Result<Self, FsmError> {
        toml::from_str(text).map_err(|e| FsmError::invalid(format!("instance TOML: {e}")))
    }

    /// Structural problems. Registration additionally checks that `protocol` is a
    /// registered adapter, that `probe.op` and the `default_params` keys exist on
    /// it, and runs `ProtocolAdapter::validate_dialect`.
    pub fn problems(&self) -> Vec<String> {
        let mut p = Vec::new();
        if !OP_NAME.is_match(&self.protocol) {
            p.push(format!(
                "protocol {:?} must match ^[a-z][a-z0-9_]*$",
                self.protocol
            ));
        }
        if self.instance.is_empty() {
            p.push("at least one [[instance]] is required".into());
        }
        let mut seen = BTreeSet::new();
        for (i, inst) in self.instance.iter().enumerate() {
            let mut q = Vec::new();
            check_common(
                &mut q,
                &inst.id,
                &inst.name,
                &inst.summary,
                inst.tier,
                &inst.scopes,
                inst.home_url.as_deref(),
            );
            if !is_http_url(&inst.base_url) {
                q.push(format!(
                    "base_url {:?} must be an http(s) URL",
                    inst.base_url
                ));
            }
            if inst.transport == TransportProfile::Local {
                q.push("transport = \"local\" is not allowed for a remote instance".into());
            }
            if let Some(l) = &inst.limits {
                check_limits(&mut q, "limits", l);
            }
            match inst.probe.op.as_deref() {
                Some(op) if OP_NAME.is_match(op) => {
                    if let Some(f) = &inst.probe.fixture
                        && let Err(e) = check_fixture_path(f, &inst.id, op)
                    {
                        q.push(format!("probe: {e}"));
                    }
                }
                _ => q.push("probe.op must name an adapter op".into()),
            }
            if inst
                .dialect
                .keys()
                .chain(inst.default_params.keys())
                .any(|k| k.trim().is_empty())
            {
                q.push("dialect/default_params keys must be non-empty".into());
            }
            if !seen.insert(inst.id.as_str()) {
                q.push("duplicate id".into());
            }
            p.extend(
                q.into_iter()
                    .map(|m| format!("instance[{i}] {:?}: {m}", inst.id)),
            );
        }
        p
    }

    pub fn validate(&self) -> Result<(), FsmError> {
        let p = self.problems();
        if p.is_empty() {
            Ok(())
        } else {
            Err(FsmError::invalid(format!(
                "instances of {:?}: {}",
                self.protocol,
                p.join("; ")
            )))
        }
    }
}

impl InstanceDecl {
    /// `pool_role`, or the A1 §D.2 default (`scope_only` with `search`, else `explicit`).
    pub fn effective_pool_role(&self) -> PoolRole {
        self.pool_role
            .clone()
            .unwrap_or(if self.caps.contains(&Capability::Search) {
                PoolRole::ScopeOnly
            } else {
                PoolRole::Explicit
            })
    }

    /// `limits`, or [`default_instance_limits`].
    pub fn effective_limits(&self) -> SourceLimits {
        self.limits
            .clone()
            .unwrap_or_else(|| default_instance_limits(&self.base_url))
    }

    /// The registry declaration of this instance over the adapter's ops (the
    /// probe is attached to the op it names).
    pub fn to_source_decl(&self, adapter_ops: &[OpDecl]) -> SourceDecl {
        let mut d = SourceDecl::new(&self.id, &self.name, &self.summary);
        d.aliases = self.aliases.clone();
        d.home_url = self.home_url.clone();
        d.tier = self.tier;
        d.scopes = self.scopes.clone();
        d.caps = self.caps.clone();
        d.transport = self.transport;
        d.pool_role = self.effective_pool_role();
        d.limits = self.effective_limits();
        d.ops = adapter_ops.to_vec();
        if let Some(op) = self.probe.op.as_deref()
            && let Some(o) = d.ops.iter_mut().find(|o| o.name == op)
        {
            o.probe = Some(self.probe.to_spec());
        }
        d
    }
}

/// JSON Schema of a source manifest (the content of
/// `crates/manifests/manifest.schema.json`).
pub fn json_schema() -> Value {
    serde_json::to_value(schemars::schema_for!(ManifestDecl)).unwrap_or(Value::Null)
}

/// JSON Schema of a protocol instance table.
pub fn instance_json_schema() -> Value {
    serde_json::to_value(schemars::schema_for!(InstanceFileDecl)).unwrap_or(Value::Null)
}

/// Pretty-printed [`json_schema`] with a trailing newline (the committed file).
pub fn json_schema_text() -> String {
    let mut s = serde_json::to_string_pretty(&json_schema()).unwrap_or_default();
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_and_fixture_paths() {
        assert_eq!(
            placeholders("https://x/{nct}/a?{q}&{lang}"),
            ["nct", "q", "lang"]
        );
        assert!(placeholders("https://x/a").is_empty());
        assert_eq!(
            check_fixture_path("fixtures/seed/arxiv/work/default.json", "arxiv", "work").unwrap(),
            ("seed", "default")
        );
        assert!(
            check_fixture_path("fixtures/seed/arxiv/search/default.json", "arxiv", "work").is_err()
        );
        assert!(check_fixture_path("fixtures/arxiv/work/default.json", "arxiv", "work").is_err());
        assert!(
            check_fixture_path("fixtures/seed/arxiv/work/default.toml", "arxiv", "work").is_err()
        );
        assert!(
            check_fixture_path(
                "/abs/fixtures/seed/arxiv/work/default.json",
                "arxiv",
                "work"
            )
            .is_err()
        );
    }

    #[test]
    fn ordered_map_keeps_order_and_rejects_duplicates() {
        let m: OrderedMap<u32> = serde_json::from_str(r#"{"z":1,"a":2,"m":3}"#).unwrap();
        assert_eq!(m.keys().collect::<Vec<_>>(), ["z", "a", "m"]);
        assert_eq!(serde_json::to_string(&m).unwrap(), r#"{"z":1,"a":2,"m":3}"#);
        assert!(serde_json::from_str::<OrderedMap<u32>>(r#"{"a":1,"a":2}"#).is_err());
        let mut m = m;
        m.insert("a", 9);
        m.insert("b", 4);
        assert_eq!(m.get("a"), Some(&9));
        assert_eq!(m.remove("z"), Some(1));
        assert_eq!(m.keys().collect::<Vec<_>>(), ["a", "m", "b"]);
    }

    #[test]
    fn schema_is_strict() {
        let s = json_schema();
        assert_eq!(s["additionalProperties"], Value::Bool(false));
        let req: Vec<&str> = s["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        for k in [
            "id",
            "name",
            "summary",
            "tier",
            "scopes",
            "caps",
            "transport",
            "pool_role",
            "limits",
            "op",
        ] {
            assert!(req.contains(&k), "{k} not required in schema");
        }
        assert_eq!(
            instance_json_schema()["additionalProperties"],
            Value::Bool(false)
        );
    }

    #[test]
    fn const_query_values_accept_scalars_and_arrays() {
        let m: BTreeMap<String, ConstQueryVal> =
            toml::from_str("a = \"x\"\nb = 3\nc = true\n\"fields[]\" = [\"t\", 2]\n").unwrap();
        assert_eq!(m["a"], ConstQueryVal::one("x"));
        assert_eq!(m["b"], ConstQueryVal::one("3"));
        assert_eq!(m["c"], ConstQueryVal::one("true"));
        assert_eq!(m["fields[]"].values(), ["t", "2"]);
        assert!(toml::from_str::<BTreeMap<String, ConstQueryVal>>("a = { b = 1 }").is_err());
        assert_eq!(
            serde_json::to_string(&m["fields[]"]).unwrap(),
            r#"["t","2"]"#
        );
    }

    #[test]
    fn single_array_items_are_detected() {
        for bad in ["[.]", " [.data] ", "[.results[] | .x]"] {
            assert!(items_wraps_single_array(bad), "{bad}");
        }
        for ok in [
            ".",
            ".data",
            ".results[]",
            ".feed.entry | arr | .[]",
            "[.a, .b][]",
        ] {
            assert!(!items_wraps_single_array(ok), "{ok}");
        }
    }

    #[test]
    fn instance_defaults_follow_a1() {
        let mut i = InstanceDecl {
            base_url: "https://cir.nii.ac.jp/opensearch".into(),
            caps: vec![Capability::Search],
            ..InstanceDecl::default()
        };
        assert_eq!(i.effective_pool_role(), PoolRole::ScopeOnly);
        i.caps = vec![Capability::Lookup];
        assert_eq!(i.effective_pool_role(), PoolRole::Explicit);
        let l = i.effective_limits();
        assert_eq!(
            (l.rps, l.burst, l.concurrency, l.timeout_ms, l.cache_ttl_s),
            (2.0, 4, 2, 8000, 21_600)
        );
        assert_eq!(l.host_bucket.as_deref(), Some("cir.nii.ac.jp"));
    }
}
