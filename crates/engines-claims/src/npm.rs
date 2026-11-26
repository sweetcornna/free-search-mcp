//! npm registry search: JavaScript packages with current version and release date.
//!
//! Port of `src/search_mcp/engines/npm.py`. Declares `"code"` in `scopes` (M16).

use crate::facts::{
    SNIPPET_CAP, candidate_names, clip, clip_query, ecosystem_hints, iso_day, version_question,
};
use crate::http::{
    CallBudget, build_source_decl, encode_query_plus, get_json, hits_output, make_hit,
    parse_search_params,
};
use fsm_core::{
    Availability, Config, FsmError, Hit, PoolRole, ProbeSpec, Source, SourceDecl, SourceDeps,
    SourceLimits, SrcCx, SrcOutput,
};
use serde_json::{Value, json};

const API_URL: &str = "https://registry.npmjs.org/-/v1/search";

pub struct NpmEngine {
    decl: SourceDecl,
    deps: SourceDeps,
}

impl NpmEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let decl = build_source_decl(
            "npm",
            "npm Registry",
            "npm registry: JavaScript packages with their current version and release date.",
            &["npmjs", "node packages", "js packages"],
            "https://www.npmjs.com",
            &["code"],
            PoolRole::DirectClaimant,
            SourceLimits::new(2.0, 4, 2, 5_000, 21_600),
            r#"source.call("npm.search", {query: "npm express latest version"})"#,
            ProbeSpec::new(
                json!({"query": "npm express latest version", "limit": 3}),
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
        format!("{API_URL}?text={}&size={n}", encode_query_plus(&terms))
    }

    pub fn map_results(&self, payload: &Value) -> Vec<Hit> {
        let Some(objects) = payload.get("objects").and_then(Value::as_array) else {
            return Vec::new();
        };
        let mut results = Vec::new();
        for obj in objects {
            let Some(pkg) = obj.get("package").and_then(Value::as_object) else {
                continue;
            };
            let Some(name) = pkg.get("name").and_then(Value::as_str) else {
                continue;
            };
            let Some(version) = pkg.get("version").and_then(Value::as_str) else {
                continue;
            };
            let date = iso_day(pkg.get("date"));
            let mut bits = vec![if !date.is_empty() {
                format!("Version {version}, published {date}")
            } else {
                format!("Version {version}")
            }];
            if let Some(desc) = pkg
                .get("description")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                bits.push(desc.to_owned());
            }
            if let Some(links) = pkg.get("links").and_then(Value::as_object) {
                for key in ["homepage", "repository"] {
                    if let Some(url) = links
                        .get(key)
                        .and_then(Value::as_str)
                        .filter(|s| s.starts_with("http"))
                    {
                        bits.push(format!("{key} {url}"));
                        break;
                    }
                }
            }
            results.push(make_hit(
                &self.decl.id,
                format!("{name} {version} on npm"),
                format!("https://www.npmjs.com/package/{name}/v/{version}"),
                clip(&bits.join(" · "), SNIPPET_CAP),
                (!date.is_empty()).then_some(date),
            ));
        }
        results
    }
}

#[async_trait::async_trait]
impl Source for NpmEngine {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    fn claims(&self, query: &str, _lang: Option<&str>) -> f32 {
        let q = clip_query(query);
        if version_question(q)
            && ecosystem_hints(q).contains("npm")
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
        let Some(payload) = get_json(&self.deps, &self.decl, cx, &budget, &url, &[], None).await?
        else {
            return Ok(hits_output(Vec::new()));
        };
        let mut hits = self.map_results(&payload);
        hits.truncate(limit);
        Ok(hits_output(hits))
    }
}
