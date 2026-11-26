//! CFETS: PBOC daily RMB central parity rate against 24 currencies.
//!
//! Port of `src/search_mcp/engines/cfets.py`.

use crate::facts::{SNIPPET_CAP, clip, clip_query, fmt_comma_fixed, fmt_g, query_is_chinese};
use crate::frankfurter::parse_pair;
use crate::http::{
    CallBudget, build_source_decl, get_json, hits_output, make_hit, parse_search_params,
};
use fsm_core::{
    Availability, Config, FsmError, Hit, PoolRole, ProbeSpec, Source, SourceDecl, SourceDeps,
    SourceLimits, SrcCx, SrcOutput,
};
use jiff::civil::Date;
use regex::Regex;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::LazyLock;

const API_URL: &str = "https://www.chinamoney.com.cn/r/cms/www/chinamoney/data/fx/ccpr.json";
const PAGE_URL: &str = "https://www.chinamoney.com.cn/chinese/bkccpr/";

static PARITY_ASK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)中间价|central parity|中国外汇交易中心|cfets|人民币汇率|人民币兑|兑人民币")
        .expect("cfets parity regex")
});

pub struct CfetsEngine {
    decl: SourceDecl,
    deps: SourceDeps,
}

impl CfetsEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let decl = build_source_decl(
            "cfets",
            "CFETS RMB Central Parity",
            "CFETS: the PBOC's daily RMB central parity against 24 currencies.",
            &["pboc", "chinamoney", "人民币中间价", "外汇交易中心"],
            PAGE_URL,
            &["finance"],
            PoolRole::DirectClaimant,
            SourceLimits::new(0.33, 2, 2, 5_000, 3_600),
            r#"source.call("cfets.search", {query: "人民币中间价"})"#,
            ProbeSpec::new(json!({"query": "人民币中间价", "limit": 3}), 1),
        );
        Self { decl, deps }
    }

    pub fn map_results(&self, payload: &Value) -> Vec<Hit> {
        let Some(obj) = payload.as_object() else {
            return Vec::new();
        };
        let Some(records) = obj.get("records").and_then(Value::as_array) else {
            return Vec::new();
        };
        let wanted: HashSet<String> = obj
            .get("_wanted")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .filter(|s: &HashSet<String>| !s.is_empty())
            .unwrap_or_else(|| HashSet::from(["USD".to_owned()]));
        let amount = obj.get("_amount").and_then(Value::as_f64).unwrap_or(1.0);
        let base = obj.get("_base").and_then(Value::as_str).unwrap_or("USD");
        let zh = obj.get("_zh").and_then(Value::as_bool).unwrap_or(false);
        let stamp = obj
            .get("data")
            .and_then(Value::as_object)
            .and_then(|d| d.get("lastDate"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let stamp10: String = stamp.chars().take(10).collect();
        let day = stamp10
            .parse::<Date>()
            .ok()
            .map(|d| d.to_string())
            .unwrap_or_default();

        let mut results = Vec::new();
        for row in records {
            let Some(r) = row.as_object() else {
                continue;
            };
            let Some(code) = r.get("foreignCName").and_then(Value::as_str) else {
                continue;
            };
            if !wanted.contains(code) {
                continue;
            }
            let Some(pair_name) = r.get("vrtEName").and_then(Value::as_str) else {
                continue;
            };
            let rate = match r.get("price") {
                Some(Value::Number(n)) => n.as_f64(),
                Some(Value::String(s)) => s.replace(',', "").trim().parse::<f64>().ok(),
                _ => None,
            }
            .filter(|r| r.is_finite());
            let Some(rate) = rate else {
                continue;
            };
            let per = if pair_name.starts_with("100") {
                100.0
            } else {
                1.0
            };
            let foreign_first = pair_name.replace("100", "").starts_with(code);
            let unit_rate = if foreign_first {
                rate / per
            } else if rate != 0.0 {
                1.0 / rate
            } else {
                0.0
            };
            let converted = if base == "CNY" && unit_rate != 0.0 {
                format!(
                    "{} CNY = {} {code}",
                    fmt_g(amount, 6),
                    fmt_comma_fixed(amount / unit_rate, 4)
                )
            } else {
                format!(
                    "{} {code} = {} CNY",
                    fmt_g(amount, 6),
                    fmt_comma_fixed(amount * unit_rate, 4)
                )
            };
            let quote_line = if zh {
                format!("{pair_name} {}（{stamp}）", fmt_g(rate, 6))
            } else {
                format!("{pair_name} {} ({stamp})", fmt_g(rate, 6))
            };
            let first_bit = if amount != 1.0 {
                converted
            } else {
                format!("1 {code} = {} CNY", fmt_g(unit_rate, 6))
            };
            let second_bit = if zh {
                format!(
                    "人民币中间价 {quote_line}，中国外汇交易中心每个交易日 9:15 公布，不是实时汇率"
                )
            } else {
                format!(
                    "RMB central parity {quote_line}, published by CFETS at 9:15 each trading day; not a live market quote"
                )
            };
            let shown_day = if !day.is_empty() {
                day.as_str()
            } else {
                stamp10.as_str()
            };
            let title = if zh {
                format!("{code}/CNY 中间价 {}（{shown_day}）", fmt_g(unit_rate, 6))
            } else {
                format!(
                    "{code} to CNY central parity: {} (CFETS, {shown_day})",
                    fmt_g(unit_rate, 6)
                )
            };
            results.push(make_hit(
                &self.decl.id,
                title,
                PAGE_URL,
                clip(&format!("{first_bit} · {second_bit}"), SNIPPET_CAP),
                (!day.is_empty()).then(|| day.clone()),
            ));
        }
        results
    }
}

#[async_trait::async_trait]
impl Source for CfetsEngine {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    fn claims(&self, query: &str, _lang: Option<&str>) -> f32 {
        let q = clip_query(query);
        if PARITY_ASK.is_match(q) {
            return 1.0;
        }
        if let Some((base, quote, _)) = parse_pair(q)
            && (base == "CNY" || quote == "CNY")
            && query_is_chinese(q)
        {
            return 1.0;
        }
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
        let Some(mut payload) =
            get_json(&self.deps, &self.decl, cx, &budget, API_URL, &[], None).await?
        else {
            return Ok(hits_output(Vec::new()));
        };
        let pair = parse_pair(&query);
        let mut wanted: Vec<String> = match &pair {
            Some((b, q, _)) => [b.as_str(), q.as_str()]
                .into_iter()
                .filter(|c| *c != "CNY")
                .map(str::to_owned)
                .collect(),
            None => Vec::new(),
        };
        wanted.sort();
        wanted.dedup();
        if wanted.is_empty() {
            wanted.push("USD".to_owned());
        }
        let amount = pair.as_ref().map_or(1.0, |p| p.2);
        let base = pair.as_ref().map_or("USD", |p| p.0.as_str());
        let zh = query_is_chinese(&query);
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("_wanted".to_owned(), json!(wanted));
            obj.insert("_amount".to_owned(), json!(amount));
            obj.insert("_base".to_owned(), json!(base));
            obj.insert("_zh".to_owned(), json!(zh));
        }
        let mut hits = self.map_results(&payload);
        hits.truncate(limit);
        Ok(hits_output(hits))
    }
}
