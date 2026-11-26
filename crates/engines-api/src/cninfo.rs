//! 巨潮资讯 (`cninfo.com.cn`) official A-share / STAR / ChiNext / HK announcements.

use crate::common::{
    CallBudget, SNIPPET_CAP, clip, effective_freshness, make_hit, parse_search_params, search_op,
    send_json_keyless, strip_tags_tight,
};
use async_trait::async_trait;
use fsm_core::api::Freshness;
use fsm_core::{
    AuthMode, Capability, FsmError, Hit, HitKind, HttpRequest, PoolRole, ProbeSpec, Source,
    SourceDecl, SourceDeps, SourceLimits, SrcCx, SrcOutput, TransportProfile,
};
use jiff::civil::Date;
use jiff::tz::{Offset, TimeZone};
use jiff::{Timestamp, ToSpan};
use serde_json::{Value, json};

const ENDPOINT: &str = "https://www.cninfo.com.cn/new/hisAnnouncement/query";
const STATIC_BASE: &str = "https://static.cninfo.com.cn/";

fn beijing_tz() -> TimeZone {
    TimeZone::fixed(Offset::constant(8))
}

fn freshness_days(f: Freshness) -> i64 {
    match f {
        Freshness::Day => 1,
        Freshness::Week => 7,
        Freshness::Month => 31,
        Freshness::Year => 366,
        _ => 31,
    }
}

/// 巨潮资讯 A-share / HK filing search engine (form POST returning JSON).
pub struct CninfoEngine {
    decl: SourceDecl,
    deps: SourceDeps,
}

impl CninfoEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let mut decl = SourceDecl::new(
            "cninfo",
            "巨潮资讯 (cninfo)",
            "巨潮资讯: official A-share, STAR, ChiNext and HK filings and announcements.",
        )
        .with_scopes(&["finance"])
        .with_caps(&[Capability::Search])
        .with_op(
            search_op(
                "Search Chinese listed company disclosures and announcements on cninfo.",
                30,
                r#"source.call("cninfo.search", { query: "人工智能", limit: 5 })"#,
            )
            .with_probe(ProbeSpec::new(json!({"query": "人工智能", "limit": 3}), 1)),
        );
        decl.aliases = vec![
            "巨潮资讯".to_owned(),
            "巨潮".to_owned(),
            "a-share filings".to_owned(),
        ];
        decl.home_url = Some("https://www.cninfo.com.cn".to_owned());
        decl.auth = AuthMode::None;
        decl.transport = TransportProfile::Chrome;
        decl.pool_role = PoolRole::ScopeOnly;
        let mut limits = SourceLimits::new(2.0, 4, 2, 4000, 21_600);
        limits.host_bucket = Some("www.cninfo.com.cn".to_owned());
        decl.limits = limits;
        Self { decl, deps }
    }

    /// Build form-POST pairs in deterministic order.
    pub fn build_form(
        query: &str,
        limit: usize,
        freshness: Option<Freshness>,
        today_cst: Date,
    ) -> Vec<(&'static str, String)> {
        let n = limit.clamp(1, 30);
        let mut form = vec![
            ("pageNum", "1".to_owned()),
            ("pageSize", n.to_string()),
            ("column", "szse".to_owned()),
            ("tabName", "fulltext".to_owned()),
            ("searchkey", query.to_owned()),
            ("sortName", String::new()),
            ("sortType", String::new()),
            ("isHLtitle", "true".to_owned()),
        ];
        if let Some(f) = freshness {
            let days = freshness_days(f);
            if let Ok(start) = today_cst.checked_sub(days.days()) {
                form.push(("seDate", format!("{start}~{today_cst}")));
            }
        }
        form
    }

    /// Encode form pairs as `application/x-www-form-urlencoded`.
    pub fn encode_form(pairs: &[(&str, String)]) -> String {
        let mut ser = url::form_urlencoded::Serializer::new(String::new());
        for (k, v) in pairs {
            ser.append_pair(k, v);
        }
        ser.finish()
    }

    /// Map a cninfo JSON payload into hits.
    pub fn map_results(payload: &Value) -> Vec<Hit> {
        let Some(items) = payload.get("announcements").and_then(Value::as_array) else {
            return Vec::new();
        };
        let mut results = Vec::new();
        for item in items {
            let Some(obj) = item.as_object() else {
                continue;
            };
            let Some(raw_title) = obj
                .get("announcementTitle")
                .and_then(Value::as_str)
                .filter(|t| !t.trim().is_empty())
            else {
                continue;
            };
            let Some(adjunct) = obj
                .get("adjunctUrl")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|a| !a.is_empty())
            else {
                continue;
            };
            let url = format!("{STATIC_BASE}{}", adjunct.trim_start_matches('/'));
            let mut title = strip_tags_tight(raw_title);
            let code = obj.get("secCode").and_then(Value::as_str).unwrap_or("");
            let sec_name = obj.get("secName").and_then(Value::as_str).unwrap_or("");
            if !code.is_empty() && !sec_name.is_empty() {
                title = format!("{sec_name} ({code}) {title}");
            }
            let date = Self::announced_cst(obj.get("announcementTime"));
            let snippet = Self::snippet(obj, &date);
            results.push(make_hit(
                "cninfo",
                clip(&title, 300),
                url,
                snippet,
                (!date.is_empty()).then_some(date),
                Some("api"),
                HitKind::Vertical,
            ));
        }
        results
    }

    /// Convert epoch milliseconds into Beijing time (`UTC+8`) `YYYY-MM-DD`.
    pub fn announced_cst(value: Option<&Value>) -> String {
        let Some(v) = value else {
            return String::new();
        };
        if v.is_boolean() {
            return String::new();
        }
        let ms = if let Some(n) = v.as_i64() {
            n
        } else if let Some(f) = v.as_f64().filter(|f| f.is_finite()) {
            f.trunc() as i64
        } else {
            return String::new();
        };
        let Ok(ts) = Timestamp::from_millisecond(ms) else {
            return String::new();
        };
        ts.to_zoned_datetime(beijing_tz()).date().to_string()
    }

    fn snippet(obj: &serde_json::Map<String, Value>, date: &str) -> String {
        let mut bits = Vec::new();
        if !date.is_empty() {
            bits.push(format!("披露 {date}"));
        }
        if let Some(board) = obj
            .get("pageColumn")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            bits.push(board.to_owned());
        }
        if let Some(kind) = obj
            .get("adjunctType")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            if let Some(size) = obj.get("adjunctSize").and_then(Value::as_i64) {
                bits.push(format!("{kind} {size}KB"));
            } else {
                bits.push(kind.to_owned());
            }
        }
        clip(&bits.join(" · "), SNIPPET_CAP)
    }
}

#[async_trait]
impl Source for CninfoEngine {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    async fn call(&self, op: &str, params: Value, cx: &SrcCx) -> Result<SrcOutput, FsmError> {
        let (query, limit) = parse_search_params("cninfo", op, &params, 10)?;
        if query.trim().is_empty() {
            return Ok(SrcOutput::Hits(Vec::new()));
        }
        let freshness = effective_freshness(&params, cx);
        let today_cst = Timestamp::now().to_zoned_datetime(beijing_tz()).date();
        let pairs = Self::build_form(query, limit, freshness, today_cst);
        let body = Self::encode_form(&pairs);

        let req = HttpRequest::post(ENDPOINT, body)
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .for_source("cninfo");

        let budget = CallBudget::from_cx(cx, &self.decl);
        let Some(payload) = send_json_keyless(
            self.deps.net.as_ref(),
            req,
            self.decl.transport,
            &self.decl,
            cx,
            &budget,
        )
        .await?
        else {
            return Ok(SrcOutput::Hits(Vec::new()));
        };
        let mut results = Self::map_results(&payload);
        results.truncate(limit);
        Ok(SrcOutput::Hits(results))
    }
}
