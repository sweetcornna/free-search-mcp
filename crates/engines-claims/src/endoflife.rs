//! endoflife.date: release cycles, latest versions, and support end dates.
//!
//! Port of `src/search_mcp/engines/endoflife.py`.

use crate::facts::{clip, clip_query, ecosystem_hints, iso_day, version_question};
use crate::http::{
    CallBudget, build_source_decl, encode_path_segment, get_json, get_json_opt, hits_output,
    is_fatal_fanout_error, make_hit, parse_search_params,
};
use fsm_core::{
    Availability, Config, FsmError, Hit, PoolRole, ProbeSpec, Source, SourceDecl, SourceDeps,
    SourceLimits, SrcCx, SrcOutput,
};
use futures::future::join_all;
use regex::Regex;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::Instant;

const API_URL: &str = "https://endoflife.date/api/v1/products";
const CATALOGUE_TTL: Duration = Duration::from_secs(24 * 3600);

static WORD_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z0-9][A-Za-z0-9.+#-]*").expect("eol word regex"));
static VERSION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b\d+(?:\.\d+)*\b").expect("eol version regex"));

fn normalize_key(text: &str) -> String {
    text.to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '+' || *c == '#')
        .collect()
}

/// Map every spelling (name, label, aliases) of a product to its API name.
pub fn build_index(products: &[Value]) -> HashMap<String, String> {
    let mut index = HashMap::new();
    for product in products {
        let Some(obj) = product.as_object() else {
            continue;
        };
        let Some(name) = obj.get("name").and_then(Value::as_str) else {
            continue;
        };
        let mut spellings: Vec<&str> = vec![name];
        if let Some(label) = obj.get("label").and_then(Value::as_str) {
            spellings.push(label);
        }
        if let Some(aliases) = obj.get("aliases").and_then(Value::as_array) {
            for alias in aliases {
                if let Some(s) = alias.as_str() {
                    spellings.push(s);
                }
            }
        }
        for spelling in spellings {
            if !spelling.is_empty() {
                index
                    .entry(normalize_key(spelling))
                    .or_insert_with(|| name.to_owned());
            }
        }
    }
    index
}

/// Match product names mentioned in `query` against the catalogue index (2-word spans first).
pub fn match_products(query: &str, index: &HashMap<String, String>) -> Vec<String> {
    let words: Vec<&str> = WORD_RE
        .find_iter(query)
        .map(|m| m.as_str())
        .filter(|w| w.len() > 1)
        .collect();
    let mut found: Vec<String> = Vec::new();
    for span in [2usize, 1usize] {
        if words.len() < span {
            continue;
        }
        for i in 0..=(words.len() - span) {
            let combined = words[i..i + span].concat();
            let key = normalize_key(&combined);
            if let Some(name) = index.get(&key)
                && !found.contains(name)
            {
                found.push(name.clone());
                if found.len() >= 3 {
                    return found;
                }
            }
        }
    }
    found
}

#[derive(Default)]
struct CatalogueCache {
    fetched_at: Option<Instant>,
    index: Option<Arc<HashMap<String, String>>>,
}

pub struct EndOfLifeEngine {
    decl: SourceDecl,
    deps: SourceDeps,
    cache: RwLock<CatalogueCache>,
}

impl EndOfLifeEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let decl = build_source_decl(
            "endoflife",
            "endoflife.date",
            "endoflife.date: latest version, release cycles and support end dates.",
            &[
                "eol",
                "end of life",
                "lts",
                "lifecycle",
                "支持周期",
                "生命周期",
            ],
            "https://endoflife.date",
            &["code"],
            PoolRole::DirectClaimant,
            SourceLimits::new(2.0, 4, 2, 5_000, 21_600),
            r#"source.call("endoflife.search", {query: "is python 3.9 still supported"})"#,
            ProbeSpec::new(
                json!({"query": "is python 3.9 still supported", "limit": 3}),
                1,
            ),
        );
        Self {
            decl,
            deps,
            cache: RwLock::new(CatalogueCache::default()),
        }
    }

    async fn catalogue_index(
        &self,
        cx: &SrcCx,
        budget: &CallBudget,
    ) -> Result<Arc<HashMap<String, String>>, FsmError> {
        let stale = if !cx.replay_mode {
            let guard = self.cache.read().await;
            if let (Some(at), Some(index)) = (guard.fetched_at, &guard.index)
                && at.elapsed() <= CATALOGUE_TTL
                && !index.is_empty()
            {
                return Ok(Arc::clone(index));
            }
            guard.index.clone()
        } else {
            None
        };

        let payload = match get_json(&self.deps, &self.decl, cx, budget, API_URL, &[], None).await {
            Ok(p) => p,
            Err(e) if is_fatal_fanout_error(&e) || matches!(e, FsmError::Timeout(_)) => {
                return Err(e);
            }
            Err(e) => {
                if let Some(prev) = stale {
                    return Ok(prev);
                }
                return Err(e);
            }
        };
        let index = payload
            .as_ref()
            .and_then(|v| v.get("result"))
            .and_then(Value::as_array)
            .map(|products| build_index(products))
            .unwrap_or_default();
        if index.is_empty() {
            if let Some(prev) = stale {
                return Ok(prev);
            }
            return Ok(Arc::new(index));
        }
        let arc = Arc::new(index);
        if !cx.replay_mode {
            let mut guard = self.cache.write().await;
            guard.index = Some(Arc::clone(&arc));
            guard.fetched_at = Some(Instant::now());
        }
        Ok(arc)
    }

    pub fn map_results(&self, payload: &Value, query: &str) -> Vec<Hit> {
        let Some(product) = payload.get("result").and_then(Value::as_object) else {
            return Vec::new();
        };
        let Some(name) = product.get("name").and_then(Value::as_str) else {
            return Vec::new();
        };
        let releases: Vec<&serde_json::Map<String, Value>> = product
            .get("releases")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(Value::as_object).collect())
            .unwrap_or_default();
        if releases.is_empty() {
            return Vec::new();
        }
        let label = product.get("label").and_then(Value::as_str).unwrap_or(name);
        let newest = releases[0];
        let latest = newest.get("latest").and_then(Value::as_object);
        let latest_name = latest
            .and_then(|l| l.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let latest_date = latest.map(|l| iso_day(l.get("date"))).unwrap_or_default();
        let newest_cycle = val_to_str(newest.get("name"));

        let mut bits: Vec<String> = Vec::new();
        if !latest_name.is_empty() {
            let on_date = if !latest_date.is_empty() {
                format!(" on {latest_date}")
            } else {
                String::new()
            };
            bits.push(format!(
                "Latest release {latest_name}{on_date} (cycle {newest_cycle})"
            ));
        }
        let eff_query = if !query.is_empty() {
            query
        } else {
            payload.get("_query").and_then(Value::as_str).unwrap_or("")
        };
        let wanted: HashSet<&str> = VERSION_RE
            .find_iter(eff_query)
            .map(|m| m.as_str())
            .collect();
        bits.push(format_cycles(&releases, &wanted));

        vec![make_hit(
            &self.decl.id,
            format!("{label}: release cycles and support dates"),
            format!("https://endoflife.date/{name}"),
            clip(&bits.join(" · "), 600),
            (!latest_date.is_empty()).then_some(latest_date),
        )]
    }
}

fn val_to_str(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => "None".to_owned(),
        Some(other) => other.to_string(),
    }
}

fn format_cycles(releases: &[&serde_json::Map<String, Value>], wanted: &HashSet<&str>) -> String {
    let mut asked = Vec::new();
    let mut rest = Vec::new();
    for &r in releases {
        let name = val_to_str(r.get("name"));
        if wanted.contains(name.as_str()) {
            asked.push(r);
        } else {
            rest.push(r);
        }
    }
    let mut parts = Vec::new();
    for release in asked.into_iter().chain(rest).take(5) {
        let name = val_to_str(release.get("name"));
        let is_eol = release.get("isEol").and_then(Value::as_bool);
        let eol_from = release.get("eolFrom").and_then(Value::as_str);
        let mut state = if is_eol == Some(true) {
            match eol_from {
                Some(d) => format!("end of life since {d}"),
                None => "end of life".to_owned(),
            }
        } else if let Some(d) = eol_from {
            format!("supported until {d}")
        } else if is_eol == Some(false) {
            "supported".to_owned()
        } else {
            "support end not published".to_owned()
        };
        if release.get("isLts").and_then(Value::as_bool) == Some(true) {
            state = format!("LTS, {state}");
        }
        parts.push(format!("{name}: {state}"));
    }
    format!("Cycles: {}", parts.join("; "))
}

#[async_trait::async_trait]
impl Source for EndOfLifeEngine {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    fn claims(&self, query: &str, _lang: Option<&str>) -> f32 {
        let q = clip_query(query);
        if !version_question(q) {
            return 0.0;
        }
        let hints = ecosystem_hints(q);
        let disallowed = hints.iter().any(|h| !matches!(*h, "pypi" | "npm" | "go"));
        if disallowed { 0.0 } else { 1.0 }
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
        let index = self.catalogue_index(cx, &budget).await?;
        let names = match_products(&query, &index);
        if names.is_empty() {
            return Ok(hits_output(Vec::new()));
        }
        let fetches = names.iter().map(|name| {
            let url = format!("{API_URL}/{}", encode_path_segment(name));
            get_json_opt(&self.deps, &self.decl, cx, &budget, url, &[], None)
        });
        let payloads = join_all(fetches).await;

        let mut results = Vec::new();
        let mut first_err: Option<FsmError> = None;

        for res in payloads {
            match res {
                Ok(Some(payload)) => {
                    results.extend(self.map_results(&payload, &query));
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
