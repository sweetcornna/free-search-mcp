//! GLEIF: legal entity records (LEI, addresses, jurisdiction, status).
//!
//! Port of `src/search_mcp/engines/gleif.py`.

use crate::facts::{SNIPPET_CAP, clip, clip_query, iso_day};
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

const API_URL: &str = "https://api.gleif.org/api/v1/lei-records";

static ENTITY_ASK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(lei|legal entity|legal name|registered (?:name|office|address)|incorporated|jurisdiction|parent company|subsidiary|company registration|registration number)\b|法人|法定名称|注册地|注册地址|注册号|母公司|子公司|公司主体|工商登记|营业执照",
    )
    .expect("gleif entity_ask regex")
});

static STRIP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(lei|legal entity|legal name|registered (?:name|office|address)|incorporated|jurisdiction|parent company|subsidiary|company registration|registration number|of|the|what|is|who|where|which|for|company|corporation|corp|inc|ltd|limited)\b|法人|法定名称|注册地址|注册地|注册号|母公司|子公司|公司主体|工商登记|营业执照|的|是|查|哪里|什么|谁|公司",
    )
    .expect("gleif strip regex")
});

static PUNCT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[?？,，。!！]").expect("gleif punct regex"));

static LEI_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[A-Z0-9]{18}[0-9]{2}\b").expect("gleif lei regex"));

static CJK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\u{3040}-\u{30ff}\u{3400}-\u{9fff}]").expect("gleif cjk regex"));

/// Strip legal-entity question scaffolding, leaving the company name.
pub fn entity_name(query: &str) -> String {
    let step1 = STRIP_RE.replace_all(query, " ");
    let step2 = PUNCT_RE.replace_all(&step1, " ");
    step2.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub struct GleifEngine {
    decl: SourceDecl,
    deps: SourceDeps,
}

impl GleifEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let decl = build_source_decl(
            "gleif",
            "GLEIF LEI",
            "GLEIF: legal entity name, addresses, jurisdiction and status by LEI.",
            &["lei", "legal entity", "法人"],
            "https://search.gleif.org",
            &["finance"],
            PoolRole::DirectClaimant,
            SourceLimits::new(0.5, 3, 2, 5_000, 21_600),
            r#"source.call("gleif.search", {query: "alibaba group legal entity LEI"})"#,
            ProbeSpec::new(
                json!({"query": "alibaba group legal entity LEI", "limit": 3}),
                1,
            ),
        );
        Self { decl, deps }
    }

    pub fn build_url(query: &str, max_results: usize) -> String {
        let n = max_results.clamp(1, 5);
        if let Some(m) = LEI_RE.find(query) {
            return format!("{API_URL}?filter%5Blei%5D={}&page%5Bsize%5D=1", m.as_str());
        }
        let extracted = entity_name(query);
        let name = if extracted.is_empty() {
            query
        } else {
            extracted.as_str()
        };
        let field = if CJK_RE.is_match(name) {
            "entity.legalName"
        } else {
            "fulltext"
        };
        format!(
            "{API_URL}?filter%5B{field}%5D={}&page%5Bsize%5D={n}",
            encode_query_plus(name)
        )
    }

    pub fn map_results(&self, payload: &Value) -> Vec<Hit> {
        let Some(obj) = payload.as_object() else {
            return Vec::new();
        };
        let published = obj
            .get("meta")
            .and_then(Value::as_object)
            .and_then(|m| m.get("goldenCopy"))
            .and_then(Value::as_object)
            .map(|gc| iso_day(gc.get("publishDate")))
            .unwrap_or_default();

        let Some(data) = obj.get("data").and_then(Value::as_array) else {
            return Vec::new();
        };

        let mut results = Vec::new();
        for row in data {
            let Some(attrs) = row.get("attributes").and_then(Value::as_object) else {
                continue;
            };
            let Some(entity) = attrs.get("entity").and_then(Value::as_object) else {
                continue;
            };
            let Some(lei) = attrs.get("lei").and_then(Value::as_str) else {
                continue;
            };
            let Some(legal) = entity
                .get("legalName")
                .and_then(Value::as_object)
                .and_then(|ln| ln.get("name"))
                .and_then(Value::as_str)
            else {
                continue;
            };

            let mut bits = vec![format!("LEI {lei}")];
            if let Some(status) = entity.get("status").and_then(Value::as_str) {
                bits.push(status.to_ascii_lowercase());
            }
            if let Some(jurisdiction) = entity.get("jurisdiction").and_then(Value::as_str) {
                bits.push(format!("jurisdiction {jurisdiction}"));
            }
            if let Some(address) = entity.get("legalAddress").and_then(Value::as_object) {
                let place: Vec<&str> = ["city", "region", "country"]
                    .iter()
                    .filter_map(|k| address.get(*k).and_then(Value::as_str))
                    .filter(|s| !s.is_empty())
                    .collect();
                if !place.is_empty() {
                    bits.push(format!("legal address {}", place.join(", ")));
                }
            }
            if let Some(registered) = entity
                .get("registeredAs")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                bits.push(format!("registered as {registered}"));
            }
            let created = iso_day(entity.get("creationDate"));
            if !created.is_empty() {
                bits.push(format!("created {created}"));
            }
            if let Some(others) = entity.get("otherNames").and_then(Value::as_array) {
                let other_names: Vec<&str> = others
                    .iter()
                    .filter_map(|o| o.get("name").and_then(Value::as_str))
                    .take(2)
                    .collect();
                if !other_names.is_empty() {
                    bits.push(format!("also {}", other_names.join("; ")));
                }
            }
            if !published.is_empty() {
                bits.push(format!("GLEIF data of {published}"));
            }
            results.push(make_hit(
                &self.decl.id,
                format!("{legal} ({lei})"),
                format!("https://search.gleif.org/#/record/{lei}"),
                clip(&bits.join(" · "), SNIPPET_CAP),
                (!published.is_empty()).then(|| published.clone()),
            ));
        }
        results
    }
}

#[async_trait::async_trait]
impl Source for GleifEngine {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    fn claims(&self, query: &str, _lang: Option<&str>) -> f32 {
        let q = clip_query(query);
        if LEI_RE.is_match(q) || (ENTITY_ASK.is_match(q) && !entity_name(q).is_empty()) {
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
