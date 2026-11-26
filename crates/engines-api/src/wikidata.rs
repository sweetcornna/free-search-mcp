//! Wikidata engine: entity search + dated structured facts + Wikipedia sitelink.

use crate::common::{
    CallBudget, clip, collapse_ws, compact_number, encode_path_segment, make_hit,
    parse_search_params, search_op, send_json_keyless,
};
use crate::facts::query_is_chinese;
use async_trait::async_trait;
use fsm_core::{
    AuthMode, Capability, FsmError, Hit, HitKind, HttpRequest, PoolRole, ProbeSpec, Source,
    SourceDecl, SourceDeps, SourceLimits, SrcCx, SrcOutput, TransportProfile,
};
use regex::Regex;
use serde_json::{Value, json};
use std::sync::LazyLock;

const WIKIDATA_API: &str = "https://www.wikidata.org/w/api.php";

const FACTS: &[(&str, &str)] = &[
    ("P571", "inception"),
    ("P1082", "population"),
    ("P2046", "area"),
    ("P2044", "elevation"),
    ("P856", "official website"),
    ("P625", "coordinates"),
    ("P569", "born"),
    ("P570", "died"),
    ("P577", "published"),
    ("P1128", "employees"),
    ("P2139", "revenue"),
    ("P2295", "net profit"),
    ("P1619", "opened"),
    ("P2048", "height"),
];

static FACT_WORDS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(population|area|capital|founded|founder|inception|established|height|elevation|website|coordinates|location|born|birthday|died|death|age|ceo|revenue|employees|headquarters|currency|language|languages|mayor|president|gdp|timezone|what|is|the|of|how|many|much|big|old|tall|when|where|who|was|are|does|do|in|a|an|and|for|about|wikidata|wikipedia)\b|人口|面积|首都|成立|建立|创立|创始人|海拔|官网|官方网站|坐标|出生|生日|逝世|去世|年龄|营收|收入|员工|总部|货币|语言|市长|总统|时区|是什么|是谁|多少|多大|多高|什么时候|哪里|哪一年|几年|的|是|有|在|吗|呢|维基数据|维基百科",
    )
    .expect("regex")
});

static FACT_ASK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(population|area|capital|founded|founder|inception|established|height|elevation|official website|coordinates|born|birthday|died|ceo|revenue|employees|headquarters|currency|mayor|president|gdp|timezone)\b|人口|面积|首都|成立|建立|创立|创始人|海拔|官网|官方网站|坐标|出生|生日|逝世|去世|营收|员工|总部|货币|市长|总统|时区",
    )
    .expect("regex")
});

static PUNCT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[?？,，。!！]").expect("regex"));

fn unit_label(qid: &str) -> Option<&'static str> {
    match qid {
        "Q712226" => Some("km²"),
        "Q25343" => Some("m²"),
        "Q11573" => Some("m"),
        "Q828224" => Some("km"),
        "Q174728" => Some("cm"),
        "Q11570" => Some("kg"),
        "Q4917" => Some("USD"),
        "Q4916" => Some("EUR"),
        "Q39099" => Some("CNY"),
        "Q25224" => Some("GBP"),
        "Q8146" => Some("JPY"),
        "Q577" => Some("years"),
        "Q25235" => Some("h"),
        "Q7727" => Some("min"),
        _ => None,
    }
}

/// Strip fact-question words and punctuation so only the entity name remains.
pub fn entity_terms(query: &str) -> String {
    let stripped = FACT_WORDS_RE.replace_all(query, " ");
    let clean = PUNCT_RE.replace_all(&stripped, " ");
    collapse_ws(&clean)
}

fn lang_text(block: Option<&Value>, lang: &str) -> String {
    block
        .and_then(Value::as_object)
        .and_then(|m| m.get(lang))
        .and_then(Value::as_object)
        .and_then(|e| e.get("value"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

fn format_wikidata_time(val: Option<&Value>) -> String {
    let Some(obj) = val.and_then(Value::as_object) else {
        return String::new();
    };
    let Some(raw) = obj.get("time").and_then(Value::as_str) else {
        return String::new();
    };
    let bce = raw.starts_with('-');
    let stripped = raw.trim_start_matches(['+', '-']);
    let stamp: String = stripped.chars().take(10).collect();
    if !stamp.is_ascii() || stamp.len() < 4 {
        return String::new();
    }
    let Some(year_raw) = stamp.get(..4) else {
        return String::new();
    };
    let year_part = year_raw.trim_start_matches('0');
    let mut year = if year_part.is_empty() {
        "0".to_owned()
    } else {
        year_part.to_owned()
    };
    if bce {
        year.push_str(" BCE");
    }
    let precision = obj.get("precision").and_then(Value::as_i64).unwrap_or(11);
    if precision <= 9 || bce {
        return year;
    }
    if precision == 10
        && let Some(month) = stamp.get(5..7)
    {
        return format!("{year}-{month}");
    }
    if stamp.len() >= 10
        && let Some(rest) = stamp.get(5..)
    {
        format!("{year}-{rest}")
    } else {
        year
    }
}

fn point_in_time(statement: &Value) -> String {
    let val = statement
        .get("qualifiers")
        .and_then(|q| q.get("P585"))
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(|e| e.get("datavalue"))
        .and_then(|dv| dv.get("value"));
    format_wikidata_time(val)
}

fn statement_value(statement: &Value) -> String {
    let Some(dv) = statement
        .get("mainsnak")
        .and_then(|s| s.get("datavalue"))
        .and_then(Value::as_object)
    else {
        return String::new();
    };
    let kind = dv.get("type").and_then(Value::as_str).unwrap_or("");
    let val = dv.get("value");
    match kind {
        "string" => val.and_then(Value::as_str).unwrap_or("").to_owned(),
        "time" => format_wikidata_time(val),
        "quantity" => {
            let Some(qobj) = val.and_then(Value::as_object) else {
                return String::new();
            };
            let raw_amount = qobj
                .get("amount")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim_start_matches('+');
            let amount = compact_number(raw_amount);
            if let Some(unit) = qobj
                .get("unit")
                .and_then(Value::as_str)
                .filter(|u| *u != "1")
            {
                let qid = unit.rsplit('/').next().unwrap_or(unit);
                match unit_label(qid) {
                    Some(lbl) => format!("{amount} {lbl}"),
                    None => format!("{amount} ({qid})"),
                }
            } else {
                amount
            }
        }
        "globecoordinate" => {
            let Some(cobj) = val.and_then(Value::as_object) else {
                return String::new();
            };
            let (Some(lat), Some(lon)) = (
                cobj.get("latitude").and_then(Value::as_f64),
                cobj.get("longitude").and_then(Value::as_f64),
            ) else {
                return String::new();
            };
            format!("{lat:.4}, {lon:.4}")
        }
        _ => String::new(),
    }
}

/// Wikidata structured entity lookup engine.
pub struct WikidataEngine {
    decl: SourceDecl,
    deps: SourceDeps,
}

impl WikidataEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let mut decl = SourceDecl::new(
            "wikidata",
            "Wikidata",
            "Wikidata: structured facts (dates, populations, sites) behind Wikipedia.",
        )
        .with_scopes(&["reference"])
        .with_caps(&[Capability::Search, Capability::Lookup])
        .with_op(
            search_op(
                "Search Wikidata entities and extract dated structured facts and Wikipedia links.",
                7,
                r#"source.call("wikidata.search", { query: "Shanghai population", limit: 3 })"#,
            )
            .with_probe(ProbeSpec::new(
                json!({"query": "Shanghai population", "limit": 3}),
                1,
            )),
        );
        decl.aliases = vec!["wd".to_owned(), "维基数据".to_owned()];
        decl.home_url = Some("https://www.wikidata.org".to_owned());
        decl.auth = AuthMode::None;
        decl.transport = TransportProfile::Plain;
        decl.pool_role = PoolRole::DirectClaimant;
        let mut limits = SourceLimits::new(2.0, 4, 2, 4000, 21_600);
        limits.host_bucket = Some("www.wikidata.org".to_owned());
        decl.limits = limits;
        Self { decl, deps }
    }

    fn extract_facts(claims: Option<&Value>) -> Vec<String> {
        let Some(cmap) = claims.and_then(Value::as_object) else {
            return Vec::new();
        };
        let mut facts = Vec::new();
        for &(prop, label) in FACTS {
            let Some(statements) = cmap
                .get(prop)
                .and_then(Value::as_array)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            // Match Python's `max(statements, key=_point_in_time)`: first max wins on ties.
            let mut best = &statements[0];
            let mut best_when = point_in_time(best);
            for st in &statements[1..] {
                let w = point_in_time(st);
                if w > best_when {
                    best = st;
                    best_when = w;
                }
            }
            let val = statement_value(best);
            if val.is_empty() {
                continue;
            }
            if best_when.is_empty() {
                facts.push(format!("{label} {val}"));
            } else {
                facts.push(format!("{label} {val} (as of {best_when})"));
            }
        }
        facts
    }

    /// Map a single Wikidata entity object into a hit.
    pub fn map_entity(qid: &str, entity: &Value, lang: &str) -> Option<Hit> {
        let obj = entity.as_object()?;
        if obj.contains_key("missing") {
            return None;
        }
        let mut label = lang_text(obj.get("labels"), lang);
        if label.is_empty() {
            label = lang_text(obj.get("labels"), "en");
        }
        if label.is_empty() {
            return None;
        }
        let mut desc = lang_text(obj.get("descriptions"), lang);
        if desc.is_empty() {
            desc = lang_text(obj.get("descriptions"), "en");
        }
        let sitelinks = obj.get("sitelinks").and_then(Value::as_object);
        let lang_wiki = format!("{lang}wiki");
        let url = sitelinks
            .and_then(|sl| sl.get(&lang_wiki).or_else(|| sl.get("enwiki")))
            .and_then(Value::as_object)
            .and_then(|l| l.get("url"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| {
                format!("https://www.wikidata.org/wiki/{}", encode_path_segment(qid))
            });

        let mut bits = Vec::new();
        if !desc.is_empty() {
            bits.push(desc);
        }
        bits.extend(Self::extract_facts(obj.get("claims")));
        bits.push(format!("Wikidata {qid}"));

        Some(make_hit(
            "wikidata",
            label,
            url,
            clip(&bits.join(" · "), 500),
            None,
            None,
            HitKind::Record,
        ))
    }

    /// Map a `wbgetentities` payload directly (defaults to `"en"`).
    pub fn map_entities(payload: &Value, lang: &str) -> Vec<Hit> {
        let Some(entities) = payload.get("entities").and_then(Value::as_object) else {
            return Vec::new();
        };
        entities
            .iter()
            .filter_map(|(qid, entity)| Self::map_entity(qid, entity, lang))
            .collect()
    }
}

#[async_trait]
impl Source for WikidataEngine {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    fn claims(&self, query: &str, _lang: Option<&str>) -> f32 {
        if FACT_ASK_RE.is_match(query) && !entity_terms(query).is_empty() {
            0.9
        } else {
            0.0
        }
    }

    async fn call(&self, op: &str, params: Value, cx: &SrcCx) -> Result<SrcOutput, FsmError> {
        let (query, limit) = parse_search_params("wikidata", op, &params, 5)?;
        if query.trim().is_empty() {
            return Ok(SrcOutput::Hits(Vec::new()));
        }
        let lang = if query_is_chinese(query) { "zh" } else { "en" };
        let n = limit.clamp(1, 7);
        let terms = entity_terms(query);
        let search_term = if terms.is_empty() { query } else { &terms };
        let budget = CallBudget::from_cx(cx, &self.decl);

        let search_req = HttpRequest::get(WIKIDATA_API)
            .header("Accept", "application/json")
            .query_param("action", "wbsearchentities")
            .query_param("search", search_term)
            .query_param("language", lang)
            .query_param("uselang", lang)
            .query_param("format", "json")
            .query_param("limit", n.to_string())
            .for_source("wikidata");

        let Some(found) = send_json_keyless(
            self.deps.net.as_ref(),
            search_req,
            self.decl.transport,
            &self.decl,
            cx,
            &budget,
        )
        .await?
        else {
            return Ok(SrcOutput::Hits(Vec::new()));
        };

        let ids: Vec<String> = found
            .get("search")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|h| h.get("id").and_then(Value::as_str).map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        if ids.is_empty() {
            return Ok(SrcOutput::Hits(Vec::new()));
        }

        let detail_req = HttpRequest::get(WIKIDATA_API)
            .header("Accept", "application/json")
            .query_param("action", "wbgetentities")
            .query_param("ids", ids.join("|"))
            .query_param("props", "labels|descriptions|sitelinks/urls|claims")
            .query_param("languages", format!("{lang}|en"))
            .query_param("sitefilter", format!("{lang}wiki|enwiki"))
            .query_param("format", "json")
            .for_source("wikidata");

        let Some(detail) = send_json_keyless(
            self.deps.net.as_ref(),
            detail_req,
            self.decl.transport,
            &self.decl,
            cx,
            &budget,
        )
        .await?
        else {
            return Ok(SrcOutput::Hits(Vec::new()));
        };

        let Some(entities) = detail.get("entities").and_then(Value::as_object) else {
            return Ok(SrcOutput::Hits(Vec::new()));
        };

        let mut results = Vec::new();
        for qid in &ids {
            if let Some(entity) = entities.get(qid)
                && let Some(hit) = Self::map_entity(qid, entity, lang)
            {
                results.push(hit);
            }
        }
        results.truncate(limit);
        Ok(SrcOutput::Hits(results))
    }
}
