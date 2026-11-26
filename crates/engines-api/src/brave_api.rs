//! Brave Search official REST API engine (keyed: `SEARCH_MCP_BRAVE_API_KEY`).

use crate::common::{
    CallBudget, augment_query_with_operators, effective_domains, effective_freshness,
    filter_and_truncate_hits, make_hit, parse_search_params, resolve_required_key, search_op,
    send_json_keyed, strip_highlight_tags, today_utc,
};
use async_trait::async_trait;
use fsm_core::api::Freshness;
use fsm_core::dates::extract_date_hint;
use fsm_core::{
    AuthMode, Availability, Capability, Config, FsmError, Hit, HitKind, HttpRequest, PoolRole,
    Source, SourceDecl, SourceDeps, SourceLimits, SrcCx, SrcOutput, TransportProfile,
};
use jiff::civil::Date;
use serde_json::Value;

const ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";
pub const BRAVE_API_KEY_ENV: &str = "SEARCH_MCP_BRAVE_API_KEY";
const MAX_COUNT: usize = 20;

fn brave_freshness(f: Freshness) -> &'static str {
    match f {
        Freshness::Day => "pd",
        Freshness::Week => "pw",
        Freshness::Month => "pm",
        Freshness::Year => "py",
        _ => "pw",
    }
}

fn country_from_region(region: &str) -> String {
    let cc = region
        .split('-')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_uppercase();
    if cc.is_empty() { "US".to_owned() } else { cc }
}

/// Official Brave Search REST API engine (`X-Subscription-Token` header).
pub struct BraveApiEngine {
    decl: SourceDecl,
    deps: SourceDeps,
}

impl BraveApiEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let mut decl = SourceDecl::new(
            "brave_api",
            "Brave Search API",
            "Brave Search official REST API (keyed via SEARCH_MCP_BRAVE_API_KEY).",
        )
        .with_scopes(&["web"])
        .with_caps(&[Capability::Search])
        .with_op(search_op(
            "Run a web search via the official Brave Search REST API.",
            20,
            r#"source.call("brave_api.search", { query: "rust async runtime", limit: 10 })"#,
        ));
        decl.aliases = vec!["brave search api".to_owned()];
        decl.home_url = Some("https://brave.com/search/api/".to_owned());
        decl.auth = AuthMode::RequiredKey {
            env: BRAVE_API_KEY_ENV.to_owned(),
        };
        decl.transport = TransportProfile::Plain;
        decl.pool_role = PoolRole::Explicit;
        let mut limits = SourceLimits::new(1.0, 2, 2, 5000, 21_600);
        limits.host_bucket = Some("api.search.brave.com".to_owned());
        decl.limits = limits;
        Self { decl, deps }
    }

    /// Map a Brave API JSON payload into hits using `today` for date-hint parsing.
    pub fn map_results(payload: &Value, today: Date) -> Vec<Hit> {
        let Some(items) = payload
            .get("web")
            .and_then(|w| w.get("results"))
            .and_then(Value::as_array)
        else {
            return Vec::new();
        };
        let mut results = Vec::new();
        for item in items {
            let Some(obj) = item.as_object() else {
                continue;
            };
            let raw_title = obj.get("title").and_then(Value::as_str).unwrap_or("");
            let title = strip_highlight_tags(raw_title);
            let url = obj
                .get("url")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("");
            if title.is_empty() || url.is_empty() {
                continue;
            }
            let raw_desc = obj.get("description").and_then(Value::as_str).unwrap_or("");
            let snippet = strip_highlight_tags(raw_desc);
            let (date, date_source) = Self::published_age(obj, today);
            results.push(make_hit(
                "brave_api",
                title,
                url.to_owned(),
                snippet,
                date,
                date_source,
                HitKind::Web,
            ));
        }
        results
    }

    fn published_age(
        item: &serde_json::Map<String, Value>,
        today: Date,
    ) -> (Option<String>, Option<&'static str>) {
        if let Some(page_age) = item
            .get("page_age")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let day = page_age.split('T').next().unwrap_or("");
            if !day.is_empty() {
                return (Some(day.to_owned()), Some("api"));
            }
        }
        if let Some(age) = item
            .get("age")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            && let Some(hint) = extract_date_hint(age, today)
        {
            return (Some(hint), Some("api"));
        }
        if let Some(desc) = item
            .get("description")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            && let Some(hint) = extract_date_hint(desc, today)
        {
            return (Some(hint), Some("snippet"));
        }
        (None, None)
    }
}

#[async_trait]
impl Source for BraveApiEngine {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    fn availability(&self, cfg: &Config) -> Availability {
        if cfg
            .api_key(BRAVE_API_KEY_ENV)
            .is_some_and(|k| !k.trim().is_empty())
        {
            Availability::Available
        } else {
            Availability::NeedsKey {
                env: BRAVE_API_KEY_ENV.to_owned(),
            }
        }
    }

    async fn call(&self, op: &str, params: Value, cx: &SrcCx) -> Result<SrcOutput, FsmError> {
        let (query, limit) = parse_search_params("brave_api", op, &params, 10)?;
        let key = resolve_required_key("brave_api", BRAVE_API_KEY_ENV, cx, &self.deps.cfg)?;
        if query.trim().is_empty() {
            return Ok(SrcOutput::Hits(Vec::new()));
        }
        let (include_domains, exclude_domains) = effective_domains(&params, cx);
        let freshness = effective_freshness(&params, cx);
        let augmented =
            augment_query_with_operators(query, &include_domains, &exclude_domains, None);
        let count = limit.clamp(1, MAX_COUNT);
        let region = if cx.region.trim().is_empty() {
            self.deps.cfg.region.as_str()
        } else {
            cx.region.as_str()
        };

        let mut req = HttpRequest::get(ENDPOINT)
            .header("Accept", "application/json")
            .header("X-Subscription-Token", key)
            .redact_header("X-Subscription-Token")
            .query_param("q", augmented)
            .query_param("count", count.to_string())
            .query_param("country", country_from_region(region))
            .query_param("safesearch", cx.safesearch.as_str())
            .for_source("brave_api");
        if let Some(f) = freshness {
            req = req.query_param("freshness", brave_freshness(f));
        }

        let budget = CallBudget::from_cx(cx, &self.decl);
        let payload = send_json_keyed(
            self.deps.net.as_ref(),
            req,
            self.decl.transport,
            &self.decl,
            BRAVE_API_KEY_ENV,
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
