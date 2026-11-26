//! PyPI: current version, upload date, and Python requirement straight from
//! PyPI's JSON API.
//!
//! Port of `src/search_mcp/engines/pypi.py`. Declares `"code"` in `scopes` (M16).

use crate::facts::{
    SNIPPET_CAP, candidate_names, clip, clip_query, ecosystem_hints, iso_day, version_question,
};
use crate::http::{
    CallBudget, build_source_decl, encode_path_keep_slash, first_truthy_value, get_json_opt,
    hits_output, is_fatal_fanout_error, make_hit, parse_search_params,
};
use fsm_core::{
    Availability, Config, FsmError, Hit, PoolRole, ProbeSpec, Source, SourceDecl, SourceDeps,
    SourceLimits, SrcCx, SrcOutput,
};
use futures::future::join_all;
use serde_json::{Value, json};

const API_URL: &str = "https://pypi.org/pypi";

pub struct PypiEngine {
    decl: SourceDecl,
    deps: SourceDeps,
}

impl PypiEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let decl = build_source_decl(
            "pypi",
            "PyPI",
            "PyPI registry: current version, release date and Python requirement.",
            &["pip", "python package", "python packages"],
            "https://pypi.org",
            &["code"],
            PoolRole::DirectClaimant,
            SourceLimits::new(3.0, 5, 3, 5_000, 21_600),
            r#"source.call("pypi.search", {query: "latest fastapi version"})"#,
            ProbeSpec::new(json!({"query": "latest fastapi version", "limit": 3}), 1),
        );
        Self { decl, deps }
    }

    pub fn build_url(query: &str) -> String {
        let names = candidate_names(query, 1);
        let name = names.first().map(String::as_str).unwrap_or(query.trim());
        format!("{API_URL}/{}/json", encode_path_keep_slash(name))
    }

    pub fn map_results(&self, payload: &Value) -> Vec<Hit> {
        let Some(info) = payload.get("info").and_then(Value::as_object) else {
            return Vec::new();
        };
        let Some(name) = info.get("name").and_then(Value::as_str) else {
            return Vec::new();
        };
        let Some(version) = info
            .get("version")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            return Vec::new();
        };
        let uploaded = payload
            .get("urls")
            .and_then(Value::as_array)
            .and_then(|arr| arr.first())
            .and_then(Value::as_object)
            .map(|f| {
                iso_day(first_truthy_value(
                    f,
                    &["upload_time_iso_8601", "upload_time"],
                ))
            })
            .unwrap_or_default();

        let mut bits = vec![if !uploaded.is_empty() {
            format!("Latest release {version}, uploaded {uploaded}")
        } else {
            format!("Latest release {version}")
        }];
        if let Some(req_py) = info
            .get("requires_python")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            bits.push(format!("requires Python {req_py}"));
        }
        if info.get("yanked").and_then(Value::as_bool) == Some(true) {
            bits.push("this release is yanked".to_owned());
        }
        if let Some(summary) = info
            .get("summary")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            bits.push(summary.to_owned());
        }
        let home = info
            .get("project_urls")
            .and_then(Value::as_object)
            .and_then(|u| u.get("Homepage"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                info.get("home_page")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
            });
        if let Some(h) = home.filter(|s| s.starts_with("http")) {
            bits.push(format!("homepage {h}"));
        }

        vec![make_hit(
            &self.decl.id,
            format!("{name} {version} on PyPI"),
            format!("https://pypi.org/project/{name}/{version}/"),
            clip(&bits.join(" · "), SNIPPET_CAP),
            (!uploaded.is_empty()).then_some(uploaded),
        )]
    }
}

#[async_trait::async_trait]
impl Source for PypiEngine {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    fn claims(&self, query: &str, _lang: Option<&str>) -> f32 {
        let q = clip_query(query);
        let hints = ecosystem_hints(q);
        if version_question(q)
            && (hints.is_empty() || hints.contains("pypi"))
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
        let names = candidate_names(&query, 3);
        if names.is_empty() {
            return Ok(hits_output(Vec::new()));
        }
        let budget = CallBudget::from_cx(cx, &self.decl);
        let fetches = names.iter().map(|name| {
            let url = format!("{API_URL}/{}/json", encode_path_keep_slash(name));
            get_json_opt(&self.deps, &self.decl, cx, &budget, url, &[], None)
        });
        let payloads = join_all(fetches).await;

        let mut results = Vec::new();
        let mut first_err: Option<FsmError> = None;

        for res in payloads {
            match res {
                Ok(Some(payload)) => {
                    results.extend(self.map_results(&payload));
                }
                Ok(None) => {}
                Err(e) if is_fatal_fanout_error(&e) => return Err(e),
                Err(e) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
            }
        }
        if results.is_empty()
            && let Some(e) = first_err
        {
            return Err(e);
        }
        results.truncate(limit);
        Ok(hits_output(results))
    }
}
