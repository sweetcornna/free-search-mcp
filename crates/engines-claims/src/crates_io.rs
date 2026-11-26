//! crates.io: Rust crates with their current stable version and last update.
//!
//! Port of `src/search_mcp/engines/crates.py`. Declares `"code"` in `scopes` (M16).

use crate::facts::{
    SNIPPET_CAP, candidate_names, clip, clip_query, ecosystem_hints, iso_day, version_question,
};
use crate::http::{
    CallBudget, build_source_decl, encode_query_plus, get_json, hits_output, make_hit,
    parse_search_params, user_agent,
};
use fsm_core::{
    Availability, Config, FsmError, Hit, PoolRole, ProbeSpec, Source, SourceDecl, SourceDeps,
    SourceLimits, SrcCx, SrcOutput,
};
use serde_json::{Value, json};

const API_URL: &str = "https://crates.io/api/v1/crates";

pub struct CratesEngine {
    decl: SourceDecl,
    deps: SourceDeps,
}

impl CratesEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let decl = build_source_decl(
            "crates",
            "crates.io",
            "crates.io registry: Rust crates, their current stable version and last update.",
            &["crates.io", "cargo", "rust crates"],
            "https://crates.io",
            &["code"],
            PoolRole::DirectClaimant,
            SourceLimits::new(1.0, 2, 1, 5_000, 21_600),
            r#"source.call("crates.search", {query: "tokio crate latest stable version"})"#,
            ProbeSpec::new(
                json!({"query": "tokio crate latest stable version", "limit": 3}),
                1,
            ),
        );
        Self { decl, deps }
    }

    pub fn build_url(query: &str, max_results: usize) -> String {
        let n = max_results.clamp(1, 20);
        let names = candidate_names(query, 2);
        let terms = if names.is_empty() {
            query.to_owned()
        } else {
            names.join(" ")
        };
        format!("{API_URL}?q={}&per_page={n}", encode_query_plus(&terms))
    }

    pub fn map_results(&self, payload: &Value) -> Vec<Hit> {
        let Some(crates) = payload.get("crates").and_then(Value::as_array) else {
            return Vec::new();
        };
        let mut results = Vec::new();
        for item in crates {
            let Some(crate_obj) = item.as_object() else {
                continue;
            };
            let Some(name) = crate_obj.get("name").and_then(Value::as_str) else {
                continue;
            };
            let Some(version) = crate_obj
                .get("max_stable_version")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    crate_obj
                        .get("newest_version")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                })
            else {
                continue;
            };
            let updated = iso_day(crate_obj.get("updated_at"));
            let mut bits = vec![if !updated.is_empty() {
                format!("Current stable version {version}, updated {updated}")
            } else {
                format!("Current stable version {version}")
            }];
            if let Some(desc) = crate_obj
                .get("description")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                bits.push(desc.to_owned());
            }
            for key in ["documentation", "repository"] {
                if let Some(url) = crate_obj
                    .get(key)
                    .and_then(Value::as_str)
                    .filter(|s| s.starts_with("http"))
                {
                    bits.push(format!("{key} {url}"));
                    break;
                }
            }
            results.push(make_hit(
                &self.decl.id,
                format!("{name} {version} on crates.io"),
                format!("https://crates.io/crates/{name}/{version}"),
                clip(&bits.join(" · "), SNIPPET_CAP),
                (!updated.is_empty()).then_some(updated),
            ));
        }
        results
    }
}

#[async_trait::async_trait]
impl Source for CratesEngine {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    fn claims(&self, query: &str, _lang: Option<&str>) -> f32 {
        let q = clip_query(query);
        if version_question(q)
            && ecosystem_hints(q).contains("cargo")
            && !candidate_names(q, 3).is_empty()
        {
            1.0
        } else {
            0.0
        }
    }

    fn availability(&self, _cfg: &Config) -> Availability {
        Availability::Available
    }

    async fn call(&self, op: &str, params: Value, cx: &SrcCx) -> Result<SrcOutput, FsmError> {
        let (query, limit) = parse_search_params(&self.decl.id, op, &params, 10, 50)?;
        if query.trim().is_empty() {
            return Ok(hits_output(Vec::new()));
        }
        let budget = CallBudget::from_cx(cx, &self.decl);
        let url = Self::build_url(&query, limit);
        let ua = user_agent(cx);
        let headers = [("User-Agent", ua.as_str())];
        let Some(payload) =
            get_json(&self.deps, &self.decl, cx, &budget, &url, &headers, None).await?
        else {
            return Ok(hits_output(Vec::new()));
        };
        let mut hits = self.map_results(&payload);
        hits.truncate(limit);
        Ok(hits_output(hits))
    }
}
