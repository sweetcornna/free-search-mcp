//! IMF DataMapper: macroeconomic indicator series by country (with WEO forecasts).
//!
//! Port of `src/search_mcp/engines/imf.py`.

use crate::facts::{SNIPPET_CAP, clip, clip_query};
use crate::fuzzy::{
    contains_word_bounded, strip_word_bounded_ci, token_set_ratio, token_sort_ratio,
};
use crate::http::{
    CallBudget, build_source_decl, encode_path_segment, get_json, hits_output,
    is_fatal_fanout_error, make_hit, parse_search_params,
};
use fsm_core::{
    Availability, Config, FsmError, Hit, PoolRole, ProbeSpec, Source, SourceDecl, SourceDeps,
    SourceLimits, SrcCx, SrcOutput, TransportProfile,
};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::RwLock;

const API_URL: &str = "https://www.imf.org/external/datamapper/api/v1";
const YEARS: usize = 8;
const MIN_SCORE: f64 = 60.0;
const MAX_INDICATORS: usize = 3;
const MAX_COUNTRIES: usize = 3;

/// Country catalogue preserving API insertion order for equal-length alias tie-breaks.
#[derive(Default)]
struct ImfCountries {
    entries: Vec<(String, String)>,
    by_code: HashMap<String, String>,
}

#[derive(Default)]
struct ImfCache {
    indicators: Option<Arc<Map<String, Value>>>,
    countries: Option<Arc<ImfCountries>>,
}

pub struct ImfEngine {
    decl: SourceDecl,
    deps: SourceDeps,
    cache: RwLock<ImfCache>,
}

impl ImfEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let mut decl = build_source_decl(
            "imf",
            "IMF DataMapper",
            "IMF DataMapper: GDP, inflation, debt and jobs series, with WEO forecasts.",
            &["imf weo", "world economic outlook", "macro"],
            "https://www.imf.org/external/datamapper/",
            &["finance"],
            PoolRole::ScopeOnly,
            SourceLimits::new(2.0, 4, 2, 8_000, 21_600),
            r#"source.call("imf.search", {query: "vietnam real gdp growth"})"#,
            ProbeSpec::new(json!({"query": "vietnam real gdp growth", "limit": 3}), 1),
        );
        // Oracle parity: `imf.org` sits behind Akamai and Python's `ImfEngine`
        // keeps Chrome TLS/header impersonation enabled.
        decl.transport = TransportProfile::Chrome;
        Self {
            decl,
            deps,
            cache: RwLock::new(ImfCache::default()),
        }
    }

    /// Case- and punctuation-insensitive fuzzy ratio (`token_set_ratio` or `token_sort_ratio`).
    pub fn ratio(query: &str, text: &str, sort: bool) -> f64 {
        if sort {
            token_sort_ratio(query, text)
        } else {
            token_set_ratio(query, text)
        }
    }

    /// Names a person would type for this country (full label + short form before first comma).
    pub fn aliases(label: &str) -> Vec<String> {
        let full = label.trim().to_lowercase();
        let short = full.split(',').next().unwrap_or("").trim().to_owned();
        let mut out = Vec::new();
        for cand in [full, short] {
            if cand.chars().count() >= 4 && !out.contains(&cand) {
                out.push(cand);
            }
        }
        out
    }

    /// ISO-3 codes named in `query`, by country name or by ISO-3 code itself,
    /// preserving the caller's slice order (API order) on equal-length alias ties.
    pub fn match_countries_ordered(query: &str, catalogue: &[(String, String)]) -> Vec<String> {
        let q = clip_query(query);
        let low = q.to_lowercase();
        let tokens: HashSet<String> = q
            .split_whitespace()
            .map(|t| t.trim_matches(|c| c == '.' || c == ',').to_uppercase())
            .collect();
        let mut hits: Vec<(usize, String)> = Vec::new();
        for (code, label) in catalogue {
            if tokens.contains(code) {
                hits.push((99, code.clone()));
                continue;
            }
            for alias in Self::aliases(label) {
                if contains_word_bounded(&low, &alias) {
                    hits.push((alias.chars().count(), code.clone()));
                    break;
                }
            }
        }
        hits.sort_by_key(|a| std::cmp::Reverse(a.0));
        hits.into_iter()
            .take(MAX_COUNTRIES)
            .map(|(_, code)| code)
            .collect()
    }

    /// ISO-3 codes named in `query`, by country name or by ISO-3 code itself.
    pub fn match_countries(query: &str, catalogue: &BTreeMap<String, String>) -> Vec<String> {
        let entries: Vec<(String, String)> = catalogue
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        Self::match_countries_ordered(query, &entries)
    }

    /// Remove matched country words so only the metric is scored.
    pub fn strip_countries(
        query: &str,
        codes: &[String],
        catalogue: &BTreeMap<String, String>,
    ) -> String {
        Self::strip_countries_lookup(query, codes, |code| catalogue.get(code).map(String::as_str))
    }

    fn strip_countries_lookup<'a>(
        query: &str,
        codes: &[String],
        lookup: impl Fn(&str) -> Option<&'a str>,
    ) -> String {
        let q = clip_query(query);
        let mut out = q.to_owned();
        for code in codes {
            if let Some(label) = lookup(code) {
                for alias in Self::aliases(label) {
                    out = strip_word_bounded_ci(&out, &alias);
                }
            }
            out = strip_word_bounded_ci(&out, code);
        }
        let collapsed: String = out.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.is_empty() {
            q.to_owned()
        } else {
            collapsed
        }
    }

    /// Best indicator codes for `query` (with countries stripped), strongest first.
    pub fn match_indicators(query: &str, catalogue: &Map<String, Value>) -> Vec<String> {
        let query = clip_query(query);
        let typed: HashSet<&str> = query
            .split_whitespace()
            .filter(|t| {
                let mut has_cased = false;
                let mut all_upper = true;
                for c in t.chars() {
                    if c.is_lowercase() {
                        all_upper = false;
                        break;
                    }
                    if c.is_uppercase() {
                        has_cased = true;
                    }
                }
                has_cased && all_upper
            })
            .map(|t| t.trim_matches(|c| c == '.' || c == ','))
            .collect();

        let mut scored: Vec<(f64, f64, f64, String)> = Vec::new();
        for (code, raw_entry) in catalogue {
            let Some(entry) = raw_entry.as_object() else {
                continue;
            };
            let label = entry
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            let description = entry
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("");
            let mut score = Self::ratio(query, label, false);
            if !description.is_empty() {
                score = score.max(Self::ratio(query, description, false) * 0.6);
            }
            let exact = if typed.contains(code.as_str()) {
                1.0
            } else {
                0.0
            };
            if score < MIN_SCORE && exact == 0.0 {
                continue;
            }
            let tight = Self::ratio(query, label, true);
            scored.push((exact, score, tight, code.clone()));
        }
        scored.sort_by(|a, b| {
            b.0.total_cmp(&a.0)
                .then_with(|| b.1.total_cmp(&a.1))
                .then_with(|| b.2.total_cmp(&a.2))
                .then_with(|| a.3.cmp(&b.3))
        });
        scored
            .into_iter()
            .take(MAX_INDICATORS)
            .map(|(_, _, _, code)| code)
            .collect()
    }

    /// The last `YEARS` observations, oldest first, forecasts included.
    pub fn recent(values: &Map<String, Value>) -> Vec<(String, Value)> {
        let mut years: Vec<&String> = values
            .keys()
            .filter(|y| !y.is_empty() && y.bytes().all(|b| b.is_ascii_digit()))
            .collect();
        years.sort();
        let start = years.len().saturating_sub(YEARS);
        years[start..]
            .iter()
            .map(|y| ((*y).clone(), values[*y].clone()))
            .collect()
    }

    async fn load_indicators(
        &self,
        cx: &SrcCx,
        budget: &CallBudget,
    ) -> Result<Arc<Map<String, Value>>, FsmError> {
        if !cx.replay_mode
            && let Some(cached) = &self.cache.read().await.indicators
        {
            return Ok(Arc::clone(cached));
        }
        let url = format!("{API_URL}/indicators");
        let payload = get_json(&self.deps, &self.decl, cx, budget, &url, &[], None).await?;
        let block = payload
            .as_ref()
            .and_then(|v| v.get("indicators"))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let arc = Arc::new(block);
        if !cx.replay_mode && !arc.is_empty() {
            self.cache.write().await.indicators = Some(Arc::clone(&arc));
        }
        Ok(arc)
    }

    async fn load_countries(
        &self,
        cx: &SrcCx,
        budget: &CallBudget,
    ) -> Result<(Arc<ImfCountries>, Option<FsmError>), FsmError> {
        if !cx.replay_mode
            && let Some(cached) = &self.cache.read().await.countries
        {
            return Ok((Arc::clone(cached), None));
        }
        let url = format!("{API_URL}/countries");
        let payload = match get_json(&self.deps, &self.decl, cx, budget, &url, &[], None).await {
            Ok(p) => p,
            Err(e) if is_fatal_fanout_error(&e) || matches!(e, FsmError::Timeout(_)) => {
                return Err(e);
            }
            Err(e) => return Ok((Arc::new(ImfCountries::default()), Some(e))),
        };
        let mut entries = Vec::new();
        let mut by_code = HashMap::new();
        if let Some(block) = payload
            .as_ref()
            .and_then(|v| v.get("countries"))
            .and_then(Value::as_object)
        {
            for (code, entry) in block {
                if let Some(label) = entry
                    .get("label")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    entries.push((code.clone(), label.to_owned()));
                    by_code.insert(code.clone(), label.to_owned());
                }
            }
        }
        let arc = Arc::new(ImfCountries { entries, by_code });
        if !cx.replay_mode && !arc.entries.is_empty() {
            self.cache.write().await.countries = Some(Arc::clone(&arc));
        }
        Ok((arc, None))
    }

    async fn series(
        &self,
        code: &str,
        cx: &SrcCx,
        budget: &CallBudget,
    ) -> Result<Map<String, Value>, FsmError> {
        let url = format!("{API_URL}/{}", encode_path_segment(code));
        let payload = get_json(&self.deps, &self.decl, cx, budget, &url, &[], None).await?;
        Ok(payload
            .as_ref()
            .and_then(|v| v.get("values"))
            .and_then(Value::as_object)
            .and_then(|vals| vals.get(code))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default())
    }

    fn indicator_result(
        &self,
        code: &str,
        entry: &Map<String, Value>,
        iso3: Option<&str>,
        values: &Map<String, Value>,
        countries: &ImfCountries,
    ) -> Hit {
        let raw_label = entry
            .get("label")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(code);
        let label: String = raw_label.split_whitespace().collect::<Vec<_>>().join(" ");
        let unit = entry
            .get("unit")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let dataset = entry
            .get("dataset")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("WEO");
        let country_name = iso3
            .and_then(|c| countries.by_code.get(c))
            .map(String::as_str)
            .unwrap_or("");

        let mut title = if !country_name.is_empty() {
            format!("{label} — {country_name}")
        } else {
            label
        };
        if !unit.is_empty() {
            title = format!("{title} ({unit})");
        }

        let mut bits: Vec<String> = Vec::new();
        let recent = Self::recent(values);
        if !recent.is_empty() {
            let formatted: Vec<String> = recent
                .into_iter()
                .map(|(y, v)| format!("{y}: {}", fmt_imf_val(&v)))
                .collect();
            bits.push(formatted.join(" · "));
        }
        if let Some(source) = entry
            .get("source")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            bits.push(source.to_owned());
        }
        if let Some(desc) = entry
            .get("description")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            bits.push(desc.to_owned());
        }

        let raw_url = format!(
            "https://www.imf.org/external/datamapper/{}@{}/{}",
            encode_path_segment(code),
            encode_path_segment(dataset),
            encode_path_segment(iso3.unwrap_or(""))
        );
        let url = raw_url.trim_end_matches('/').to_owned();

        make_hit(
            &self.decl.id,
            clip(&title, 300),
            url,
            clip(&bits.join(" — "), SNIPPET_CAP),
            None,
        )
    }
}

fn fmt_imf_val(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[async_trait::async_trait]
impl Source for ImfEngine {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    fn claims(&self, _query: &str, _lang: Option<&str>) -> f32 {
        0.0
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
        let catalogue = self.load_indicators(cx, &budget).await?;
        if catalogue.is_empty() {
            return Ok(hits_output(Vec::new()));
        }
        let (country_catalogue, countries_err) = self.load_countries(cx, &budget).await?;
        let countries = Self::match_countries_ordered(&query, &country_catalogue.entries);
        let metric = Self::strip_countries_lookup(&query, &countries, |code| {
            country_catalogue.by_code.get(code).map(String::as_str)
        });
        let codes = Self::match_indicators(&metric, &catalogue);
        if codes.is_empty() {
            if let Some(e) = countries_err {
                return Err(e);
            }
            return Ok(hits_output(Vec::new()));
        }

        let mut results = Vec::new();
        let mut first_err: Option<FsmError> = countries_err;

        for code in &codes {
            let entry = catalogue
                .get(code)
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            if countries.is_empty() {
                results.push(self.indicator_result(
                    code,
                    &entry,
                    None,
                    &Map::new(),
                    &country_catalogue,
                ));
                continue;
            }
            match self.series(code, cx, &budget).await {
                Ok(series) => {
                    for iso3 in &countries {
                        if let Some(vals) = series.get(iso3).and_then(Value::as_object)
                            && !vals.is_empty()
                        {
                            results.push(self.indicator_result(
                                code,
                                &entry,
                                Some(iso3),
                                vals,
                                &country_catalogue,
                            ));
                        }
                    }
                }
                Err(e) if is_fatal_fanout_error(&e) => return Err(e),
                Err(e) => {
                    let timed_out = matches!(e, FsmError::Timeout(_));
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                    if timed_out {
                        break;
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
