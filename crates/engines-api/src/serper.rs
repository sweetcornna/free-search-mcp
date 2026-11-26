//! Serper (`serper.dev`) Google SERP JSON POST engine (keyed: `SEARCH_MCP_SERPER_API_KEY`).

use crate::common::{
    CallBudget, augment_query_with_operators, effective_domains, effective_freshness,
    filter_and_truncate_hits, make_hit, parse_search_params, resolve_required_key, search_op,
    send_json_keyed, today_utc,
};
use async_trait::async_trait;
use fsm_core::api::Freshness;
use fsm_core::dates::extract_date_hint;
use fsm_core::fixture::canonical_json;
use fsm_core::{
    AuthMode, Availability, Capability, Config, FsmError, Hit, HitKind, HttpRequest, PoolRole,
    Source, SourceDecl, SourceDeps, SourceLimits, SrcCx, SrcOutput, TransportProfile,
};
use jiff::civil::Date;
use serde_json::{Map, Value, json};

const ENDPOINT: &str = "https://google.serper.dev/search";
pub const SERPER_API_KEY_ENV: &str = "SEARCH_MCP_SERPER_API_KEY";
const NUM_MIN: usize = 10;
const NUM_MAX: usize = 20;

fn freshness_qdr(f: Freshness) -> &'static str {
    match f {
        Freshness::Day => "d",
        Freshness::Week => "w",
        Freshness::Month => "m",
        Freshness::Year => "y",
        _ => "w",
    }
}

fn clean_highlight(val: Option<&Value>) -> String {
    let Some(s) = val.and_then(Value::as_str) else {
        return String::new();
    };
    let mut out = s.trim().to_owned();
    for tag in ["<strong>", "</strong>", "<em>", "</em>"] {
        out = out.replace(tag, "");
    }
    out.trim().to_owned()
}

/// Serper Google SERP REST API engine (`X-API-KEY` header, JSON POST).
pub struct SerperEngine {
    decl: SourceDecl,
    deps: SourceDeps,
}

impl SerperEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let mut decl = SourceDecl::new(
            "serper",
            "Serper",
            "Serper: Google results through a hosted API (keyed via SEARCH_MCP_SERPER_API_KEY).",
        )
        .with_scopes(&["web"])
        .with_caps(&[Capability::Search])
        .with_op(search_op(
            "Search Google organic results via Serper.",
            20,
            r#"source.call("serper.search", { query: "rust wasm runtime", limit: 10 })"#,
        ));
        decl.aliases = vec!["serper.dev".to_owned()];
        decl.home_url = Some("https://serper.dev".to_owned());
        decl.auth = AuthMode::RequiredKey {
            env: SERPER_API_KEY_ENV.to_owned(),
        };
        decl.transport = TransportProfile::Plain;
        decl.pool_role = PoolRole::Explicit;
        let mut limits = SourceLimits::new(2.0, 4, 2, 5000, 21_600);
        limits.host_bucket = Some("google.serper.dev".to_owned());
        decl.limits = limits;
        Self { decl, deps }
    }

    /// Map a Serper JSON payload into hits using `today` for date-hint extraction.
    pub fn map_results(payload: &Value, today: Date) -> Vec<Hit> {
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
            let date_raw = clean_highlight(obj.get("date"));
            let date = if date_raw.is_empty() {
                None
            } else {
                extract_date_hint(&date_raw, today)
            };
            results.push(make_hit(
                "serper",
                title,
                url.to_owned(),
                snippet,
                date,
                Some("snippet"),
                HitKind::Web,
            ));
        }
        results
    }
}

#[async_trait]
impl Source for SerperEngine {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    fn availability(&self, cfg: &Config) -> Availability {
        if cfg
            .api_key(SERPER_API_KEY_ENV)
            .is_some_and(|k| !k.trim().is_empty())
        {
            Availability::Available
        } else {
            Availability::NeedsKey {
                env: SERPER_API_KEY_ENV.to_owned(),
            }
        }
    }

    async fn call(&self, op: &str, params: Value, cx: &SrcCx) -> Result<SrcOutput, FsmError> {
        let (query, limit) = parse_search_params("serper", op, &params, 10)?;
        let key = resolve_required_key("serper", SERPER_API_KEY_ENV, cx, &self.deps.cfg)?;
        if query.trim().is_empty() {
            return Ok(SrcOutput::Hits(Vec::new()));
        }
        let (include_domains, exclude_domains) = effective_domains(&params, cx);
        let freshness = effective_freshness(&params, cx);
        let augmented =
            augment_query_with_operators(query, &include_domains, &exclude_domains, None);

        let mut body = Map::new();
        body.insert("q".to_owned(), json!(augmented));
        body.insert("num".to_owned(), json!(limit.clamp(NUM_MIN, NUM_MAX)));
        body.insert("gl".to_owned(), json!("us"));
        body.insert("hl".to_owned(), json!("en"));
        if let Some(f) = freshness {
            body.insert("tbs".to_owned(), json!(format!("qdr:{}", freshness_qdr(f))));
        }
        let wire = canonical_json(&Value::Object(body));

        let req = HttpRequest::post(ENDPOINT, wire)
            .header("X-API-KEY", key)
            .redact_header("X-API-KEY")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .for_source("serper");

        let budget = CallBudget::from_cx(cx, &self.decl);
        let payload = send_json_keyed(
            self.deps.net.as_ref(),
            req,
            self.decl.transport,
            &self.decl,
            SERPER_API_KEY_ENV,
            cx,
            &budget,
        )
        .await?;

        let hits = Self::map_results(&payload, today_utc());
        Ok(SrcOutput::Hits(filter_and_truncate_hits(
            hits,
            &include_domains,
            &exclude_domains,
            limit,
        )))
    }
}
