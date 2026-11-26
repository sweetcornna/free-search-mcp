//! SerpBase (`serpbase.dev`) Google Web Search JSON POST engine (keyed: `SEARCH_MCP_SERPBASE_API_KEY`).
//!
//! SerpBase returns HTTP 200 even for auth/quota failures, encoding the error in
//! the JSON envelope's `status` field (`0` = ok, `1001` = unauthorized, `1003` = forbidden,
//! `1020` = out of credits, `1029` = rate limited; `1000`/`1500`/`1502`/`1503`/`1504` = transient).

use crate::common::{
    CallBudget, augment_query_with_operators, effective_domains, filter_and_truncate_hits,
    make_hit, parse_search_params, resolve_required_key, search_op, send_json_keyed,
};
use async_trait::async_trait;
use fsm_core::fixture::canonical_json;
use fsm_core::{
    AuthMode, Availability, Capability, Config, FsmError, Hit, HitKind, HttpRequest, PoolRole,
    Source, SourceDecl, SourceDeps, SourceLimits, SrcCx, SrcOutput, TransportProfile,
};
use serde_json::{Value, json};

const ENDPOINT: &str = "https://api.serpbase.dev/google/search";
pub const SERPBASE_API_KEY_ENV: &str = "SEARCH_MCP_SERPBASE_API_KEY";
const NUM_MIN: usize = 1;
const NUM_MAX: usize = 100;

const AUTH_ENVELOPE: i64 = 1001;
const FORBIDDEN_ENVELOPE: i64 = 1003;
const QUOTA_ENVELOPE: i64 = 1020;
const RATE_ENVELOPE: i64 = 1029;

fn clean_highlight(val: Option<&Value>) -> String {
    let Some(s) = val.and_then(Value::as_str) else {
        return String::new();
    };
    let mut out = s.trim().to_owned();
    for tag in ["<strong>", "</strong>", "<em>", "</em>", "<b>", "</b>"] {
        out = out.replace(tag, "");
    }
    out.trim().to_owned()
}

/// SerpBase Google Web Search API engine (`X-API-Key` header, JSON POST).
pub struct SerpBaseEngine {
    decl: SourceDecl,
    deps: SourceDeps,
}

impl SerpBaseEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let mut decl = SourceDecl::new(
            "serpbase",
            "SerpBase",
            "SerpBase: Google results through a hosted search-and-data API (keyed via SEARCH_MCP_SERPBASE_API_KEY).",
        )
        .with_scopes(&["web"])
        .with_caps(&[Capability::Search])
        .with_op(search_op(
            "Search Google organic results via SerpBase.",
            100,
            r#"source.call("serpbase.search", { query: "rust sqlx", limit: 10 })"#,
        ));
        decl.aliases = vec!["serpbase.dev".to_owned()];
        decl.home_url = Some("https://serpbase.dev".to_owned());
        decl.auth = AuthMode::RequiredKey {
            env: SERPBASE_API_KEY_ENV.to_owned(),
        };
        decl.transport = TransportProfile::Plain;
        decl.pool_role = PoolRole::Explicit;
        let mut limits = SourceLimits::new(2.0, 4, 2, 5000, 21_600);
        limits.host_bucket = Some("api.serpbase.dev".to_owned());
        decl.limits = limits;
        Self { decl, deps }
    }

    /// Inspect SerpBase's business envelope `status` field.
    ///
    /// Returns `Ok(())` for success (`status == 0` or absent), and `Err` for
    /// auth/plan/credit/rate-limit codes (`1001`, `1003`, `1020`, `1029`) as well
    /// as transient/upstream error codes (`1000`, `1500`, `1502`, `1503`, `1504`, etc.).
    pub fn check_envelope(payload: &Value) -> Result<(), FsmError> {
        let Some(code) = payload.get("status").and_then(Value::as_i64) else {
            return Ok(());
        };
        match code {
            0 => Ok(()),
            AUTH_ENVELOPE => Err(FsmError::upstream(
                "serpbase",
                Some(401),
                format!(
                    "serpbase: the configured credential was rejected (status 1001); check {SERPBASE_API_KEY_ENV}."
                ),
            )),
            FORBIDDEN_ENVELOPE => Err(FsmError::upstream(
                "serpbase",
                Some(403),
                "serpbase: the API refused this request (status 1003) — plan gate or paused product.",
            )),
            QUOTA_ENVELOPE => Err(FsmError::upstream(
                "serpbase",
                Some(402),
                "serpbase: out of credits (status 1020).",
            )),
            RATE_ENVELOPE => Err(FsmError::RateLimited {
                source_id: "serpbase".to_owned(),
                retry_after_s: None,
            }),
            other => Err(FsmError::upstream(
                "serpbase",
                Some(200),
                format!("serpbase: upstream envelope error (status {other})"),
            )),
        }
    }

    /// Map a SerpBase JSON payload into hits.
    pub fn map_results(payload: &Value) -> Vec<Hit> {
        let Some(items) = payload.get("organic").and_then(Value::as_array) else {
            return Vec::new();
        };
        let mut results = Vec::new();
        for item in items {
            let Some(obj) = item.as_object() else {
                continue;
            };
            let title = clean_highlight(obj.get("title"));
            let url = obj
                .get("link")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("");
            if title.is_empty() || url.is_empty() {
                continue;
            }
            let snippet = clean_highlight(obj.get("snippet"));
            results.push(make_hit(
                "serpbase",
                title,
                url.to_owned(),
                snippet,
                None,
                None,
                HitKind::Web,
            ));
        }
        results
    }
}

#[async_trait]
impl Source for SerpBaseEngine {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    fn availability(&self, cfg: &Config) -> Availability {
        if cfg
            .api_key(SERPBASE_API_KEY_ENV)
            .is_some_and(|k| !k.trim().is_empty())
        {
            Availability::Available
        } else {
            Availability::NeedsKey {
                env: SERPBASE_API_KEY_ENV.to_owned(),
            }
        }
    }

    async fn call(&self, op: &str, params: Value, cx: &SrcCx) -> Result<SrcOutput, FsmError> {
        let (query, limit) = parse_search_params("serpbase", op, &params, 10)?;
        let key = resolve_required_key("serpbase", SERPBASE_API_KEY_ENV, cx, &self.deps.cfg)?;
        if query.trim().is_empty() {
            return Ok(SrcOutput::Hits(Vec::new()));
        }
        let (include_domains, exclude_domains) = effective_domains(&params, cx);
        let augmented =
            augment_query_with_operators(query, &include_domains, &exclude_domains, None);

        let body = json!({
            "q": augmented,
            "num": limit.clamp(NUM_MIN, NUM_MAX),
            "gl": "us",
            "hl": "en",
        });
        let wire = canonical_json(&body);

        let req = HttpRequest::post(ENDPOINT, wire)
            .header("X-API-Key", key)
            .redact_header("X-API-Key")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .for_source("serpbase");

        let budget = CallBudget::from_cx(cx, &self.decl);
        let payload = send_json_keyed(
            self.deps.net.as_ref(),
            req,
            self.decl.transport,
            &self.decl,
            SERPBASE_API_KEY_ENV,
            cx,
            &budget,
        )
        .await?;

        Self::check_envelope(&payload)?;

        let hits = Self::map_results(&payload);
        Ok(SrcOutput::Hits(filter_and_truncate_hits(
            hits,
            &include_domains,
            &exclude_domains,
            limit,
        )))
    }
}
