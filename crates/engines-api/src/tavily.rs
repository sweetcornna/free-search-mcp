//! Tavily AI-search REST API engine (keyed: `SEARCH_MCP_TAVILY_API_KEY`).

use crate::common::{
    CallBudget, effective_domains, filter_and_truncate_hits, iso_date, make_hit,
    parse_search_params, resolve_required_key, search_op, send_json_keyed, strip_html_and_unescape,
};
use async_trait::async_trait;
use fsm_core::fixture::canonical_json;
use fsm_core::{
    AuthMode, Availability, Capability, Config, FsmError, Hit, HitKind, HttpRequest, PoolRole,
    Source, SourceDecl, SourceDeps, SourceLimits, SrcCx, SrcOutput, TransportProfile,
};
use serde_json::{Map, Value, json};

const ENDPOINT: &str = "https://api.tavily.com/search";
pub const TAVILY_API_KEY_ENV: &str = "SEARCH_MCP_TAVILY_API_KEY";
const MAX_RESULTS: usize = 20;

/// Tavily AI-search REST API engine (`Authorization: Bearer <key>`, JSON POST).
pub struct TavilyEngine {
    decl: SourceDecl,
    deps: SourceDeps,
}

impl TavilyEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let mut decl = SourceDecl::new(
            "tavily",
            "Tavily",
            "Tavily: an LLM-oriented search API that returns cleaned page content (keyed via SEARCH_MCP_TAVILY_API_KEY).",
        )
        .with_scopes(&["web"])
        .with_caps(&[Capability::Search])
        .with_op(search_op(
            "Run an AI-oriented web search via Tavily with native domain filtering.",
            20,
            r#"source.call("tavily.search", { query: "model context protocol", limit: 10 })"#,
        ));
        decl.aliases = vec!["tavily.com".to_owned()];
        decl.home_url = Some("https://tavily.com".to_owned());
        decl.auth = AuthMode::RequiredKey {
            env: TAVILY_API_KEY_ENV.to_owned(),
        };
        decl.transport = TransportProfile::Plain;
        decl.pool_role = PoolRole::Explicit;
        let mut limits = SourceLimits::new(2.0, 4, 2, 5000, 21_600);
        limits.host_bucket = Some("api.tavily.com".to_owned());
        decl.limits = limits;
        Self { decl, deps }
    }

    /// Map a Tavily JSON payload into hits.
    pub fn map_results(payload: &Value) -> Vec<Hit> {
        let Some(items) = payload.get("results").and_then(Value::as_array) else {
            return Vec::new();
        };
        let mut results = Vec::new();
        for item in items {
            let Some(obj) = item.as_object() else {
                continue;
            };
            let title =
                strip_html_and_unescape(obj.get("title").and_then(Value::as_str).unwrap_or(""));
            let url = obj
                .get("url")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("");
            if title.is_empty() || url.is_empty() {
                continue;
            }
            let snippet =
                strip_html_and_unescape(obj.get("content").and_then(Value::as_str).unwrap_or(""));
            let date = iso_date(obj.get("published_date"));
            results.push(make_hit(
                "tavily",
                title,
                url.to_owned(),
                snippet,
                (!date.is_empty()).then_some(date),
                Some("api"),
                HitKind::Web,
            ));
        }
        results
    }
}

#[async_trait]
impl Source for TavilyEngine {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    fn availability(&self, cfg: &Config) -> Availability {
        if cfg
            .api_key(TAVILY_API_KEY_ENV)
            .is_some_and(|k| !k.trim().is_empty())
        {
            Availability::Available
        } else {
            Availability::NeedsKey {
                env: TAVILY_API_KEY_ENV.to_owned(),
            }
        }
    }

    async fn call(&self, op: &str, params: Value, cx: &SrcCx) -> Result<SrcOutput, FsmError> {
        let (query, limit) = parse_search_params("tavily", op, &params, 10)?;
        let key = resolve_required_key("tavily", TAVILY_API_KEY_ENV, cx, &self.deps.cfg)?;
        if query.trim().is_empty() {
            return Ok(SrcOutput::Hits(Vec::new()));
        }
        let (include_domains, exclude_domains) = effective_domains(&params, cx);

        let mut body = Map::new();
        body.insert("query".to_owned(), json!(query));
        body.insert("max_results".to_owned(), json!(limit.clamp(1, MAX_RESULTS)));
        body.insert("search_depth".to_owned(), json!("basic"));
        body.insert("include_answer".to_owned(), json!(false));
        body.insert("include_raw_content".to_owned(), json!(false));
        if !include_domains.is_empty() {
            body.insert("include_domains".to_owned(), json!(include_domains));
        }
        if !exclude_domains.is_empty() {
            body.insert("exclude_domains".to_owned(), json!(exclude_domains));
        }
        let wire = canonical_json(&Value::Object(body));

        let req = HttpRequest::post(ENDPOINT, wire)
            .header("Authorization", format!("Bearer {key}"))
            .redact_header("Authorization")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .for_source("tavily");

        let budget = CallBudget::from_cx(cx, &self.decl);
        let payload = send_json_keyed(
            self.deps.net.as_ref(),
            req,
            self.decl.transport,
            &self.decl,
            TAVILY_API_KEY_ENV,
            cx,
            &budget,
        )
        .await?;

        let hits = Self::map_results(&payload);
        Ok(SrcOutput::Hits(filter_and_truncate_hits(
            hits,
            &include_domains,
            &exclude_domains,
            limit,
        )))
    }
}
