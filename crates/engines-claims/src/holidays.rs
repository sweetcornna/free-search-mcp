//! Public holidays by country and year, with China's State Council make-up
//! working days (`holiday-cn`) and global coverage (`Nager.Date`).
//!
//! Port of `src/search_mcp/engines/holidays.py`.

use crate::facts::{
    CountryRow, clip, clip_query, countries_in, query_is_chinese, year_in, zh_country_by_iso2,
};
use crate::http::{
    CallBudget, build_source_decl, get_json_opt, hits_output, is_fatal_fanout_error, make_hit,
    parse_search_params,
};
use fsm_core::{
    Availability, Config, FsmError, Hit, PoolRole, ProbeSpec, Source, SourceDecl, SourceDeps,
    SourceLimits, SrcCx, SrcOutput,
};
use futures::future::join_all;
use regex::Regex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::LazyLock;

static HOLIDAY_ASK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(holidays?|public holidays?|bank holidays?|day off|long weekend)\b|假期|假日|放假|节假日|调休|法定假|公众假期|休假安排|放假安排",
    )
    .expect("holiday_ask regex")
});

pub struct HolidaysEngine {
    decl: SourceDecl,
    deps: SourceDeps,
}

impl HolidaysEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let decl = build_source_decl(
            "holidays",
            "Public Holidays",
            "Public holidays by country and year; China from the State Council notice.",
            &[
                "holiday",
                "holiday-cn",
                "nager",
                "放假安排",
                "节假日",
                "调休",
            ],
            "https://date.nager.at",
            &["reference"],
            PoolRole::DirectClaimant,
            SourceLimits::new(2.0, 4, 2, 5_000, 21_600),
            r#"source.call("holidays.search", {query: "2026年中国放假安排"})"#,
            ProbeSpec::new(json!({"query": "2026年中国放假安排", "limit": 3}), 1),
        );
        Self { decl, deps }
    }

    pub fn targets(query: &str) -> Vec<CountryRow> {
        let mut found = countries_in(query, 2);
        if found.is_empty() && query_is_chinese(query) {
            found.push(("CHN", "CN", "China"));
        }
        found
    }

    pub fn url_for(iso2: &str, year: i32) -> String {
        if iso2 == "CN" {
            format!("https://raw.githubusercontent.com/NateScarlet/holiday-cn/master/{year}.json")
        } else {
            format!("https://nagerholidays.com/api/v4/Holidays/{iso2}/{year}")
        }
    }

    pub fn build_url(query: &str, default_year: i32) -> String {
        let year = year_in(query, default_year);
        let targets = Self::targets(query);
        let iso2 = targets.first().map_or("CN", |t| t.1);
        Self::url_for(iso2, year)
    }

    pub fn map_results(&self, payload: &Value) -> Vec<Hit> {
        if let Some(obj) = payload.as_object()
            && obj.get("days").is_some_and(Value::is_array)
        {
            let year = obj.get("year").and_then(Value::as_i64).unwrap_or(0) as i32;
            return self.map_cn(payload, year, true);
        }
        if let Some(arr) = payload.as_array()
            && let Some(first) = arr.first().and_then(Value::as_object)
        {
            let cc = first
                .get("countryCode")
                .and_then(Value::as_str)
                .unwrap_or("");
            let year = first
                .get("date")
                .and_then(Value::as_str)
                .and_then(|d| d.get(..4))
                .and_then(|y| y.parse::<i32>().ok())
                .unwrap_or(0);
            return self.map_nager(payload, cc, cc, year, false);
        }
        Vec::new()
    }

    pub fn map_cn(&self, payload: &Value, year: i32, zh: bool) -> Vec<Hit> {
        let Some(obj) = payload.as_object() else {
            return Vec::new();
        };
        let Some(days) = obj.get("days").and_then(Value::as_array) else {
            return Vec::new();
        };
        if days.is_empty() {
            return Vec::new();
        }
        let mut groups: HashMap<String, (Vec<String>, Vec<String>)> = HashMap::new();
        let mut order: Vec<String> = Vec::new();
        for row in days {
            let Some(r) = row.as_object() else {
                continue;
            };
            let Some(name) = r.get("name").and_then(Value::as_str) else {
                continue;
            };
            let date_str = r.get("date").and_then(Value::as_str).unwrap_or("");
            let day = date_str.get(5..).unwrap_or("");
            if day.is_empty() {
                continue;
            }
            if !groups.contains_key(name) {
                order.push(name.to_owned());
                groups.insert(name.to_owned(), (Vec::new(), Vec::new()));
            }
            let entry = groups.get_mut(name).expect("inserted");
            if r.get("isOffDay").and_then(Value::as_bool).unwrap_or(false) {
                entry.0.push(day.to_owned());
            } else {
                entry.1.push(day.to_owned());
            }
        }
        let mut parts: Vec<String> = Vec::new();
        for name in &order {
            let (off, work) = &groups[name];
            let span = if off.len() > 1 {
                format!("{} to {}", off[0], off[off.len() - 1])
            } else if let Some(first) = off.first() {
                first.clone()
            } else {
                String::new()
            };
            let mut text = if zh {
                format!("{name} {}", span.replace(" to ", "至"))
            } else {
                format!("{name} {span}").trim().to_owned()
            };
            if !work.is_empty() {
                if zh {
                    text.push_str(&format!("（调休上班 {}）", work.join(", ")));
                } else {
                    text.push_str(&format!(" (make-up working days {})", work.join(", ")));
                }
            }
            parts.push(text);
        }
        let url = obj
            .get("papers")
            .and_then(Value::as_array)
            .and_then(|p| p.first())
            .and_then(Value::as_str)
            .unwrap_or("https://www.gov.cn/zhengce/");
        let title = if zh {
            format!("{year}年中国法定节假日安排（国务院通知）")
        } else {
            format!("China public holidays {year} (State Council notice)")
        };
        let joined = if zh {
            parts.join("；")
        } else {
            parts.join("; ")
        };
        vec![make_hit(
            &self.decl.id,
            title,
            url,
            clip(&joined, 700),
            None,
        )]
    }

    pub fn map_nager(
        &self,
        payload: &Value,
        iso2: &str,
        name: &str,
        year: i32,
        zh: bool,
    ) -> Vec<Hit> {
        let Some(rows) = payload.as_array() else {
            return Vec::new();
        };
        if rows.is_empty() {
            return Vec::new();
        }
        let mut parts: Vec<String> = Vec::new();
        for row in rows {
            let Some(r) = row.as_object() else {
                continue;
            };
            let Some(date_str) = r.get("date").and_then(Value::as_str) else {
                continue;
            };
            let Some(label) = r
                .get("localName")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .or_else(|| r.get("name").and_then(Value::as_str))
            else {
                continue;
            };
            let national = r
                .get("nationalHoliday")
                .and_then(Value::as_bool)
                .or_else(|| r.get("global").and_then(Value::as_bool))
                .unwrap_or(true);
            let md = date_str.get(5..).unwrap_or("");
            let suffix = if national { "" } else { " (regional)" };
            parts.push(format!("{md} {label}{suffix}"));
        }
        if parts.is_empty() {
            return Vec::new();
        }
        let title = if zh {
            let zh_name = zh_country_by_iso2(iso2).unwrap_or(name);
            format!("{year}年{zh_name}公众假期")
        } else {
            format!("{name} public holidays {year}")
        };
        vec![make_hit(
            &self.decl.id,
            title,
            format!("https://date.nager.at/PublicHoliday/Country/{iso2}/{year}"),
            clip(&parts.join("; "), 700),
            None,
        )]
    }

    /// Fetch holidays with an explicit `default_year` (deterministic for tests).
    pub async fn fetch_with_default_year(
        &self,
        query: &str,
        limit: usize,
        default_year: i32,
        cx: &SrcCx,
    ) -> Result<Vec<Hit>, FsmError> {
        let targets = Self::targets(query);
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        let year = year_in(query, default_year);
        let zh = query_is_chinese(query);
        let budget = CallBudget::from_cx(cx, &self.decl);
        let fetches = targets.iter().map(|&(_, iso2, _)| {
            get_json_opt(
                &self.deps,
                &self.decl,
                cx,
                &budget,
                Self::url_for(iso2, year),
                &[],
                None,
            )
        });
        let payloads = join_all(fetches).await;

        let mut results = Vec::new();
        let mut first_err: Option<FsmError> = None;

        for ((_, iso2, name), res) in targets.into_iter().zip(payloads) {
            match res {
                Ok(Some(payload)) => {
                    if iso2 == "CN" {
                        results.extend(self.map_cn(&payload, year, zh));
                    } else {
                        results.extend(self.map_nager(&payload, iso2, name, year, zh));
                    }
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
        Ok(results)
    }
}

#[async_trait::async_trait]
impl Source for HolidaysEngine {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    fn claims(&self, query: &str, _lang: Option<&str>) -> f32 {
        let q = clip_query(query);
        if HOLIDAY_ASK.is_match(q) && !Self::targets(q).is_empty() {
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
        let current_year = i32::from(jiff::ZonedDateTime::now().year());
        let hits = self
            .fetch_with_default_year(&query, limit, current_year, cx)
            .await?;
        Ok(hits_output(hits))
    }
}
