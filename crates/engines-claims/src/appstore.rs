//! Apple App Store: current version, release date, and release notes of an iOS app.
//!
//! Port of `src/search_mcp/engines/appstore.py`.

use crate::facts::{clip, clip_query, iso_day, query_is_chinese, version_question};
use crate::http::{
    CallBudget, build_source_decl, encode_query_plus, get_json, hits_output, make_hit,
    parse_search_params,
};
use fsm_core::{
    Availability, Config, FsmError, Hit, PoolRole, ProbeSpec, Source, SourceDecl, SourceDeps,
    SourceLimits, SrcCx, SrcOutput,
};
use regex::Regex;
use serde_json::{Value, json};
use std::sync::LazyLock;

const API_URL: &str = "https://itunes.apple.com/search";

static APP_ASK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(app store|ios app|iphone app|ipad app|app version|ios version of)\b|app\s*store|苹果版|ios 版|iphone 版|应用商店|app 版本|应用版本",
    )
    .expect("appstore app_ask regex")
});

static STRIP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)苹果版|ios ?版|iphone ?版|ipad ?版|应用商店|应用|版本|最新|更新|发布|了什么|的|是|什么|多少|现在|目前|了|吗|\b(app store|ios app|iphone app|ipad app|app version|ios version of|app|ios|iphone|ipad|apple|version|latest|newest|current|release|released|update|updated|what|is|the|of|on)\b",
    )
    .expect("appstore strip regex")
});

static PUNCT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[?？,，。!！]").expect("appstore punct regex"));

/// Strip App Store question words, leaving the app name.
pub fn app_name(query: &str) -> String {
    let step1 = STRIP_RE.replace_all(query, " ");
    let step2 = PUNCT_RE.replace_all(&step1, " ");
    step2.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub struct AppStoreEngine {
    decl: SourceDecl,
    deps: SourceDeps,
}

impl AppStoreEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let decl = build_source_decl(
            "appstore",
            "Apple App Store",
            "Apple App Store: current version, release date and notes of an iOS app.",
            &["ios app", "itunes", "应用商店", "苹果版"],
            "https://apps.apple.com",
            &["code"],
            PoolRole::DirectClaimant,
            SourceLimits::new(0.25, 2, 2, 5_000, 21_600),
            r#"source.call("appstore.search", {query: "Notion iOS app version"})"#,
            ProbeSpec::new(json!({"query": "Notion iOS app version", "limit": 3}), 1),
        );
        Self { decl, deps }
    }

    pub fn build_url(query: &str, max_results: usize) -> String {
        let country = if query_is_chinese(query) { "cn" } else { "us" };
        let n = max_results.clamp(1, 5);
        let extracted = app_name(query);
        let term = if extracted.is_empty() {
            query
        } else {
            extracted.as_str()
        };
        format!(
            "{API_URL}?term={}&country={country}&entity=software&limit={n}",
            encode_query_plus(term)
        )
    }

    pub fn map_results(&self, payload: &Value) -> Vec<Hit> {
        let Some(obj) = payload.as_object() else {
            return Vec::new();
        };
        let mut rows: Vec<&serde_json::Map<String, Value>> = obj
            .get("results")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(Value::as_object).collect())
            .unwrap_or_default();
        let term = obj
            .get("_term")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase();
        if !term.is_empty() {
            rows.sort_by_key(|row| {
                let tn = row
                    .get("trackName")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_lowercase();
                if tn.starts_with(&term) { 0u8 } else { 1u8 }
            });
        }

        let mut results = Vec::new();
        for row in rows {
            let Some(name) = row.get("trackName").and_then(Value::as_str) else {
                continue;
            };
            let Some(version) = row.get("version").and_then(Value::as_str) else {
                continue;
            };
            let raw_url = row
                .get("trackViewUrl")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    let id = row
                        .get("trackId")
                        .map(|v| match v {
                            Value::String(s) => s.clone(),
                            Value::Null => String::new(),
                            other => other.to_string(),
                        })
                        .unwrap_or_default();
                    format!("https://apps.apple.com/app/id{id}")
                });
            let clean_url = raw_url.split('?').next().unwrap_or(&raw_url);
            let day = iso_day(row.get("currentVersionReleaseDate"));
            let mut bits = vec![if !day.is_empty() {
                format!("Version {version}, released {day}")
            } else {
                format!("Version {version}")
            }];
            if let Some(seller) = row.get("sellerName").and_then(Value::as_str) {
                bits.push(format!("by {seller}"));
            }
            if let Some(min_os) = row.get("minimumOsVersion").and_then(Value::as_str) {
                bits.push(format!("requires iOS {min_os}"));
            }
            if let Some(price) = row.get("formattedPrice").and_then(Value::as_str) {
                bits.push(price.to_owned());
            }
            if let Some(rating) = row.get("averageUserRating").and_then(Value::as_f64) {
                bits.push(format!("rating {rating:.1}"));
            }
            if let Some(notes) = row
                .get("releaseNotes")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
            {
                let collapsed: String = notes.split_whitespace().collect::<Vec<_>>().join(" ");
                bits.push(collapsed);
            }
            results.push(make_hit(
                &self.decl.id,
                format!("{name} {version} on the App Store"),
                clean_url,
                clip(&bits.join(" · "), 450),
                (!day.is_empty()).then_some(day),
            ));
        }
        results
    }
}

#[async_trait::async_trait]
impl Source for AppStoreEngine {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    fn claims(&self, query: &str, _lang: Option<&str>) -> f32 {
        let q = clip_query(query);
        if APP_ASK.is_match(q) && version_question(q) && !app_name(q).is_empty() {
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
        let Some(mut payload) =
            get_json(&self.deps, &self.decl, cx, &budget, &url, &[], None).await?
        else {
            return Ok(hits_output(Vec::new()));
        };
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("_term".to_owned(), json!(app_name(&query)));
        }
        let mut hits = self.map_results(&payload);
        hits.truncate(limit);
        Ok(hits_output(hits))
    }
}
