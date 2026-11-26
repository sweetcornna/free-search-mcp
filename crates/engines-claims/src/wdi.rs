//! World Bank WDI: macroeconomic/demographic indicator series by country.
//!
//! Port of `src/search_mcp/engines/wdi.py`.

use crate::facts::{
    CountryRow, SNIPPET_CAP, clip, clip_query, compact_number, countries_in, fmt_g, iso_day,
    query_is_chinese, zh_country_by_iso3,
};
use crate::http::{
    CallBudget, build_source_decl, get_json, hits_output, is_fatal_fanout_error, make_hit,
    parse_search_params,
};
use fsm_core::{
    Availability, Config, FsmError, Hit, PoolRole, ProbeSpec, Source, SourceDecl, SourceDeps,
    SourceLimits, SrcCx, SrcOutput,
};
use futures::future::join_all;
use regex::Regex;
use serde_json::{Value, json};
use std::sync::LazyLock;

/// `(code, label, unit)`
pub type IndicatorRow = (&'static str, &'static str, &'static str);

enum IndicatorMatcher {
    Regex(Regex),
    /// Exact lookahead-free replacement for `r"\bpopulation\b|人口(?!普查)"`.
    Population(Regex),
}

impl IndicatorMatcher {
    fn is_match(&self, query: &str) -> bool {
        match self {
            Self::Regex(rx) => rx.is_match(query),
            Self::Population(rx) => {
                if rx.is_match(query) {
                    return true;
                }
                let mut pos = 0usize;
                while let Some(rel) = query[pos..].find("人口") {
                    let after = pos + rel + "人口".len();
                    if !query[after..].starts_with("普查") {
                        return true;
                    }
                    pos = after;
                }
                false
            }
        }
    }
}

static INDICATORS: LazyLock<Vec<(IndicatorRow, IndicatorMatcher)>> = LazyLock::new(|| {
    let re = |pat: &str| IndicatorMatcher::Regex(Regex::new(pat).expect("wdi regex"));
    vec![
        (
            ("NY.GDP.PCAP.CD", "GDP per capita", "current US$"),
            re(r"(?i)gdp per capita|per capita gdp|人均 ?gdp|人均国内生产总值|人均产值"),
        ),
        (
            ("NY.GDP.MKTP.KD.ZG", "GDP growth", "annual %"),
            re(r"(?i)gdp growth|growth rate|经济增速|经济增长率|gdp ?增速|gdp ?增长"),
        ),
        (
            ("NY.GDP.MKTP.CD", "GDP", "current US$"),
            re(r"(?i)\bgdp\b|gross domestic product|国内生产总值|经济总量"),
        ),
        (
            ("SP.POP.TOTL", "Population", "people"),
            IndicatorMatcher::Population(Regex::new(r"(?i)\bpopulation\b").expect("pop regex")),
        ),
        (
            ("FP.CPI.TOTL.ZG", "Inflation, consumer prices", "annual %"),
            re(r"(?i)\binflation\b|\bcpi\b|通胀|通货膨胀|物价"),
        ),
        (
            ("SL.UEM.TOTL.ZS", "Unemployment", "% of labour force"),
            re(r"(?i)unemployment|失业率|失业"),
        ),
        (
            ("SP.DYN.LE00.IN", "Life expectancy at birth", "years"),
            re(r"(?i)life expectancy|预期寿命|人均寿命"),
        ),
        (
            ("SP.DYN.TFRT.IN", "Fertility rate", "births per woman"),
            re(r"(?i)fertility|生育率|总和生育率"),
        ),
        (
            ("SP.DYN.CBRT.IN", "Birth rate", "per 1,000 people"),
            re(r"(?i)birth rate|出生率"),
        ),
        (
            (
                "NE.EXP.GNFS.CD",
                "Exports of goods and services",
                "current US$",
            ),
            re(r"(?i)\bexports?\b|出口(额|总额)?"),
        ),
        (
            (
                "NE.IMP.GNFS.CD",
                "Imports of goods and services",
                "current US$",
            ),
            re(r"(?i)\bimports?\b|进口(额|总额)?"),
        ),
        (
            ("GC.DOD.TOTL.GD.ZS", "Central government debt", "% of GDP"),
            re(r"(?i)government debt|public debt|debt to gdp|政府债务|国债.*占比|负债率"),
        ),
        (
            ("FI.RES.TOTL.CD", "Total reserves", "current US$"),
            re(r"(?i)foreign (exchange )?reserves|外汇储备"),
        ),
        (
            (
                "BX.KLT.DINV.CD.WD",
                "Foreign direct investment, net inflows",
                "current US$",
            ),
            re(r"(?i)\bfdi\b|foreign direct investment|外商直接投资|外资流入"),
        ),
        (
            ("IT.NET.USER.ZS", "Internet users", "% of population"),
            re(r"(?i)internet users|internet penetration|互联网普及率|网民"),
        ),
        (
            ("SP.URB.TOTL.IN.ZS", "Urban population", "% of total"),
            re(r"(?i)urbani[sz]ation|urban population|城镇化率|城市化率"),
        ),
        (
            ("AG.LND.TOTL.K2", "Land area", "sq. km"),
            re(r"(?i)land area|国土面积|陆地面积"),
        ),
        (
            ("MS.MIL.XPND.GD.ZS", "Military expenditure", "% of GDP"),
            re(r"(?i)military (spending|expenditure)|defen[cs]e (spending|budget)|军费|国防开支"),
        ),
        (
            ("SE.ADT.LITR.ZS", "Literacy rate, adult", "% of people 15+"),
            re(r"(?i)literacy|识字率"),
        ),
        (
            (
                "SH.XPD.CHEX.GD.ZS",
                "Current health expenditure",
                "% of GDP",
            ),
            re(r"(?i)health (spending|expenditure)|医疗支出|卫生支出"),
        ),
        (
            (
                "EG.USE.ELEC.KH.PC",
                "Electric power consumption",
                "kWh per capita",
            ),
            re(r"(?i)electricity consumption|power consumption|用电量"),
        ),
        (
            ("EN.GHG.CO2.PC.CE.AR5", "CO2 emissions per capita", "t CO2e"),
            re(r"(?i)co2|carbon emissions|碳排放"),
        ),
    ]
});

/// `(code, label, unit)` for each WDI indicator the question names.
pub fn indicators_in(query: &str, limit: usize) -> Vec<IndicatorRow> {
    INDICATORS
        .iter()
        .filter(|(_, matcher)| matcher.is_match(query))
        .map(|(row, _)| *row)
        .take(limit)
        .collect()
}

fn fmt_wdi_value(value: &Value) -> String {
    let number = value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.parse::<f64>().ok()));
    let Some(number) = number.filter(|n| n.is_finite()) else {
        return match value {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
    };
    let abs = number.abs();
    if abs >= 1e12 {
        format!("{} trillion", fmt_g(number / 1e12, 3))
    } else if abs >= 1e9 {
        format!("{} billion", fmt_g(number / 1e9, 3))
    } else if abs >= 1e6 {
        format!("{} million", fmt_g(number / 1e6, 3))
    } else if number.fract() == 0.0 && abs >= 1000.0 {
        compact_number(&(number as i64).to_string())
    } else {
        fmt_g(number, 4)
    }
}

pub struct WdiEngine {
    decl: SourceDecl,
    deps: SourceDeps,
}

impl WdiEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let decl = build_source_decl(
            "wdi",
            "World Bank WDI",
            "World Bank indicators: GDP, population, inflation and more, by country and year.",
            &[
                "world bank indicators",
                "world development indicators",
                "gdp",
                "人口",
            ],
            "https://data.worldbank.org",
            &["reference"],
            PoolRole::DirectClaimant,
            SourceLimits::new(2.0, 4, 2, 5_000, 21_600),
            r#"source.call("wdi.search", {query: "中国 2025 GDP"})"#,
            ProbeSpec::new(json!({"query": "中国 2025 GDP", "limit": 3}), 1),
        );
        Self { decl, deps }
    }

    pub fn indicator_url(iso3: &str, code: &str) -> String {
        format!("https://api.worldbank.org/v2/country/{iso3}/indicator/{code}?format=json&mrv=3")
    }

    pub fn build_url(query: &str) -> String {
        let countries = countries_in(query, 2);
        let indicators = indicators_in(query, 2);
        let iso3 = countries.first().map_or("WLD", |c| c.0);
        let code = indicators.first().map_or("NY.GDP.MKTP.CD", |i| i.0);
        Self::indicator_url(iso3, code)
    }

    pub fn map_results(&self, payload: &Value) -> Vec<Hit> {
        let Some(arr) = payload.as_array() else {
            return Vec::new();
        };
        let Some(rows) = arr.get(1).and_then(Value::as_array) else {
            return Vec::new();
        };
        let Some(first) = rows.first().and_then(Value::as_object) else {
            return Vec::new();
        };
        let code = first
            .get("indicator")
            .and_then(Value::as_object)
            .and_then(|i| i.get("id"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let label = first
            .get("indicator")
            .and_then(Value::as_object)
            .and_then(|i| i.get("value"))
            .and_then(Value::as_str)
            .unwrap_or(code);
        let iso3 = first
            .get("countryiso3code")
            .and_then(Value::as_str)
            .unwrap_or("");
        let name = first
            .get("country")
            .and_then(Value::as_object)
            .and_then(|c| c.get("value"))
            .and_then(Value::as_str)
            .unwrap_or(iso3);
        self.map_pair(payload, (iso3, "", name), (code, label, ""), false)
    }

    fn map_pair(
        &self,
        payload: &Value,
        country: (&str, &str, &str),
        indicator: (&str, &str, &str),
        zh: bool,
    ) -> Vec<Hit> {
        let Some(arr) = payload.as_array() else {
            return Vec::new();
        };
        if arr.len() < 2 {
            return Vec::new();
        }
        let meta = arr.first().and_then(Value::as_object);
        let Some(raw_rows) = arr.get(1).and_then(Value::as_array) else {
            return Vec::new();
        };
        let rows: Vec<&serde_json::Map<String, Value>> = raw_rows
            .iter()
            .filter_map(Value::as_object)
            .filter(|r| r.get("value").is_some_and(|v| !v.is_null()))
            .collect();
        if rows.is_empty() {
            return Vec::new();
        }

        let (iso3, fallback_iso2, name) = country;
        let (code, default_label, _unit) = indicator;
        let label = rows[0]
            .get("indicator")
            .and_then(Value::as_object)
            .and_then(|i| i.get("value"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(default_label);

        let latest = rows[0];
        let values: String = rows
            .iter()
            .take(3)
            .map(|r| {
                let d = r.get("date").and_then(Value::as_str).unwrap_or("");
                let v = r.get("value").map(fmt_wdi_value).unwrap_or_default();
                format!("{d}: {v}")
            })
            .collect::<Vec<_>>()
            .join("; ");
        let updated = meta
            .map(|m| iso_day(m.get("lastupdated")))
            .unwrap_or_default();
        let iso2 = latest
            .get("country")
            .and_then(Value::as_object)
            .and_then(|c| c.get("id"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .or_else(|| (!fallback_iso2.is_empty()).then_some(fallback_iso2))
            .unwrap_or(iso3);

        let first_bit = if zh {
            let zh_name = zh_country_by_iso3(iso3).unwrap_or(name);
            format!("{zh_name}（{label}）：{values}")
        } else {
            format!("{name}, {label}: {values}")
        };
        let second_bit = if !updated.is_empty() {
            format!("World Bank WDI series {code}, data revised {updated}")
        } else {
            format!("World Bank WDI series {code}")
        };

        let latest_date = latest.get("date").and_then(Value::as_str).unwrap_or("");
        let latest_val = latest.get("value").map(fmt_wdi_value).unwrap_or_default();
        vec![make_hit(
            &self.decl.id,
            format!("{name} {label} {latest_date}: {latest_val}"),
            format!("https://data.worldbank.org/indicator/{code}?locations={iso2}"),
            clip(&format!("{first_bit} · {second_bit}"), SNIPPET_CAP),
            (!updated.is_empty()).then_some(updated),
        )]
    }
}

#[async_trait::async_trait]
impl Source for WdiEngine {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    fn claims(&self, query: &str, _lang: Option<&str>) -> f32 {
        let q = clip_query(query);
        if !indicators_in(q, 2).is_empty() && !countries_in(q, 2).is_empty() {
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
        let countries = countries_in(&query, 2);
        let indicators = indicators_in(&query, 2);
        if countries.is_empty() || indicators.is_empty() {
            return Ok(hits_output(Vec::new()));
        }
        let mut pairs: Vec<(CountryRow, IndicatorRow)> = Vec::new();
        for &c in &countries {
            for &i in &indicators {
                pairs.push((c, i));
                if pairs.len() >= 4 {
                    break;
                }
            }
            if pairs.len() >= 4 {
                break;
            }
        }
        let budget = CallBudget::from_cx(cx, &self.decl);
        let fetches = pairs.iter().map(|(c, i)| {
            get_json(
                &self.deps,
                &self.decl,
                cx,
                &budget,
                Self::indicator_url(c.0, i.0),
                &[],
                None,
            )
        });
        let payloads = join_all(fetches).await;

        let zh = query_is_chinese(&query);
        let mut results = Vec::new();
        let mut first_err: Option<FsmError> = None;

        for ((c, i), res) in pairs.into_iter().zip(payloads) {
            match res {
                Ok(Some(payload)) => {
                    results.extend(self.map_pair(&payload, c, i, zh));
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
