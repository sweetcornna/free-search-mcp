//! SEC EDGAR full-text search over US regulatory filings (keyless JSON API).

use crate::common::{
    CallBudget, SNIPPET_CAP, clip, collapse_ws, effective_freshness, make_hit, parse_search_params,
    quote_plus, search_op, send_json_keyless, today_utc,
};
use async_trait::async_trait;
use fsm_core::api::Freshness;
use fsm_core::{
    AuthMode, Capability, FsmError, Hit, HitKind, HttpRequest, PoolRole, ProbeSpec, Source,
    SourceDecl, SourceDeps, SourceLimits, SrcCx, SrcOutput, TransportProfile,
};
use jiff::ToSpan;
use jiff::civil::Date;
use regex::Regex;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::LazyLock;

const ENDPOINT: &str = "https://efts.sec.gov/LATEST/search-index";

static FORM_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(10-K(?:/A)?|10-Q(?:/A)?|8-K(?:/A)?|20-F|40-F|6-K|S-1|S-3|S-4|F-1|11-K|N-CSR|424B[0-9]|13F(?:-HR)?|DEF\s?14A|DEFA\s?14A|SC\s?13[DG])\b",
    )
    .expect("regex")
});

static TICKER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Z]{1,5}(?:[.-][A-Z]{1,2})?$").expect("regex"));

static NOT_TICKERS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "A", "I", "AND", "OR", "THE", "IN", "OF", "TO", "AI", "ML", "US", "USA", "UK", "EU", "CEO",
        "CFO", "CTO", "COO", "IPO", "ESG", "GDP", "SEC", "IRS", "FDA", "FTC", "DOJ", "EPS", "ETF",
        "REIT", "SPAC", "PDF", "API", "LLC", "LLP", "PLC", "INC", "LTD", "NYSE", "IT", "ALL",
        "NEW", "ON", "SO", "GO", "NOW", "BY", "FOR", "ARE", "CAN", "HAS", "ITS", "OUT",
    ]
    .into_iter()
    .collect()
});

/// SEC-acceptable User-Agent: declarative name plus optional contact email, with NO URL.
///
/// `efts.sec.gov` answers 403 to User-Agents containing `https://...`.
pub fn sec_user_agent(contact_email: Option<&str>) -> String {
    match contact_email.map(str::trim).filter(|c| !c.is_empty()) {
        Some(contact) => format!("free-search-mcp {contact}"),
        None => "free-search-mcp".to_owned(),
    }
}

/// Pull SEC filing form codes (e.g. `10-K`, `8-K`, `SC 13D`) out of a free-text query.
///
/// Keeps the original query text if removing forms would leave it empty.
pub fn split_forms(query: &str) -> (String, Vec<String>) {
    let mut forms = Vec::new();
    for cap in FORM_RE.captures_iter(query) {
        let form = collapse_ws(&cap[1]).to_ascii_uppercase();
        if !forms.contains(&form) {
            forms.push(form);
        }
    }
    if forms.is_empty() {
        return (query.to_owned(), Vec::new());
    }
    let stripped = collapse_ws(&FORM_RE.replace_all(query, " "));
    let remaining = if stripped.is_empty() {
        query.to_owned()
    } else {
        stripped
    };
    (remaining, forms)
}

/// Detect an upper-case ticker symbol in a free-text query, leaving the query unchanged.
pub fn split_ticker(query: &str) -> (String, String) {
    for raw in query.split_whitespace() {
        let token = raw.trim_matches(['.', ',', ';', ':', '!', '?', '(', ')', '[', ']', '"', '\'']);
        if token.is_empty()
            || token != token.to_uppercase()
            || NOT_TICKERS.contains(token)
            || !TICKER_RE.is_match(token)
        {
            continue;
        }
        return (query.to_owned(), token.to_owned());
    }
    (query.to_owned(), String::new())
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

/// SEC EDGAR filing full-text search engine.
pub struct SecEdgarEngine {
    decl: SourceDecl,
    deps: SourceDeps,
}

impl SecEdgarEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let mut decl = SourceDecl::new(
            "sec_edgar",
            "SEC EDGAR",
            "SEC EDGAR full-text search over US filings: 10-K, 10-Q, 8-K, proxies.",
        )
        .with_scopes(&["finance"])
        .with_caps(&[Capability::Search])
        .with_op(
            search_op(
                "Full-text search across US SEC EDGAR filings.",
                50,
                r#"source.call("sec_edgar.search", { query: "NVDA 10-K risk factors", limit: 5 })"#,
            )
            .with_probe(ProbeSpec::new(
                json!({"query": "NVDA 10-K risk factors", "limit": 3}),
                1,
            )),
        );
        decl.aliases = vec![
            "edgar".to_owned(),
            "sec".to_owned(),
            "10-k".to_owned(),
            "us filings".to_owned(),
        ];
        decl.home_url = Some("https://www.sec.gov/edgar/search/".to_owned());
        decl.auth = AuthMode::None;
        decl.transport = TransportProfile::Plain;
        decl.pool_role = PoolRole::ScopeOnly;
        let mut limits = SourceLimits::new(5.0, 5, 2, 4000, 21_600);
        limits.host_bucket = Some("efts.sec.gov".to_owned());
        decl.limits = limits;
        Self { decl, deps }
    }

    /// Build the EDGAR query parameters for a search, optionally scoped by `entityName`.
    pub fn build_query_params(
        query: &str,
        freshness: Option<Freshness>,
        scoped: bool,
        today: Date,
    ) -> Vec<(String, String)> {
        let (text, forms) = split_forms(query);
        let (text, entity) = split_ticker(&text);
        let mut params = vec![("q".to_owned(), text)];
        if scoped && !entity.is_empty() {
            params.push(("entityName".to_owned(), entity));
        }
        if !forms.is_empty() {
            params.push(("forms".to_owned(), forms.join(",")));
        }
        if let Some(f) = freshness {
            let days = freshness_days(f);
            if let Ok(start) = today.checked_sub(days.days()) {
                params.push(("dateRange".to_owned(), "custom".to_owned()));
                params.push(("startdt".to_owned(), start.to_string()));
                params.push(("enddt".to_owned(), today.to_string()));
            }
        }
        params
    }

    /// Render the full EDGAR request URL (useful for diagnostics and tests).
    pub fn build_url(
        query: &str,
        freshness: Option<Freshness>,
        scoped: bool,
        today: Date,
    ) -> String {
        let params = Self::build_query_params(query, freshness, scoped, today);
        let qs = params
            .into_iter()
            .map(|(k, v)| format!("{k}={}", quote_plus(&v)))
            .collect::<Vec<_>>()
            .join("&");
        format!("{ENDPOINT}?{qs}")
    }

    fn build_request(
        &self,
        query: &str,
        freshness: Option<Freshness>,
        scoped: bool,
        cx: &SrcCx,
    ) -> HttpRequest {
        let contact = cx
            .contact_email
            .as_deref()
            .or(self.deps.cfg.contact_email.as_deref());
        let mut req = HttpRequest::get(ENDPOINT)
            .header("User-Agent", sec_user_agent(contact))
            .header("Accept", "application/json")
            .for_source("sec_edgar");
        for (k, v) in Self::build_query_params(query, freshness, scoped, today_utc()) {
            req = req.query_param(k, v);
        }
        req
    }

    /// Map an EDGAR EFTS JSON payload to hits.
    pub fn map_results(payload: &Value) -> Vec<Hit> {
        let Some(items) = payload
            .get("hits")
            .and_then(|h| h.get("hits"))
            .and_then(Value::as_array)
        else {
            return Vec::new();
        };
        let mut results = Vec::new();
        for item in items {
            let Some(obj) = item.as_object() else {
                continue;
            };
            let Some(src) = obj.get("_source").and_then(Value::as_object) else {
                continue;
            };
            let Some(url) = Self::document_url(obj.get("_id"), src) else {
                continue;
            };
            let title = Self::title(src);
            if title.is_empty() {
                continue;
            }
            let date = src
                .get("file_date")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            results.push(make_hit(
                "sec_edgar",
                clip(&title, 300),
                url,
                Self::snippet(src),
                (!date.is_empty()).then_some(date),
                Some("api"),
                HitKind::Vertical,
            ));
        }
        results
    }

    fn document_url(
        doc_id: Option<&Value>,
        src: &serde_json::Map<String, Value>,
    ) -> Option<String> {
        let raw_id = doc_id.and_then(Value::as_str)?;
        let (accession, document) = raw_id.split_once(':')?;
        if accession.is_empty() || document.is_empty() {
            return None;
        }
        let cik_raw = src
            .get("ciks")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(Value::as_str)?;
        let trimmed = cik_raw.trim_matches('0');
        if trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let cik_num: u64 = cik_raw.parse().ok()?;
        let acc_clean = accession.replace('-', "");
        Some(format!(
            "https://www.sec.gov/Archives/edgar/data/{cik_num}/{acc_clean}/{document}"
        ))
    }

    fn title(src: &serde_json::Map<String, Value>) -> String {
        let mut company = String::new();
        if let Some(first_name) = src
            .get("display_names")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(Value::as_str)
        {
            let before_cik = first_name.split("(CIK").next().unwrap_or("").trim();
            company = collapse_ws(before_cik);
            if let Some((head, tickers)) = company.split_once('(')
                && tickers.contains(',')
            {
                let first_ticker = tickers.split(',').next().unwrap_or("").trim();
                company = format!("{} ({first_ticker})", head.trim());
            }
        }
        let form = src.get("form").and_then(Value::as_str).unwrap_or("");
        if !form.is_empty() && !company.is_empty() {
            format!("{form} — {company}")
        } else if !form.is_empty() {
            form.to_owned()
        } else {
            company
        }
    }

    fn snippet(src: &serde_json::Map<String, Value>) -> String {
        let mut bits = Vec::new();
        if let Some(date) = src
            .get("file_date")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            bits.push(format!("filed {date}"));
        }
        if let Some(period) = src
            .get("period_ending")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            bits.push(format!("period ending {period}"));
        }
        if let Some(desc) = src
            .get("file_description")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            bits.push(desc.to_owned());
        }
        if let Some(loc) = src
            .get("biz_locations")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            bits.push(loc.to_owned());
        }
        clip(&bits.join(" · "), SNIPPET_CAP)
    }
}

#[async_trait]
impl Source for SecEdgarEngine {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    async fn call(&self, op: &str, params: Value, cx: &SrcCx) -> Result<SrcOutput, FsmError> {
        let (query, limit) = parse_search_params("sec_edgar", op, &params, 10)?;
        if query.trim().is_empty() {
            return Ok(SrcOutput::Hits(Vec::new()));
        }
        let freshness = effective_freshness(&params, cx);
        let budget = CallBudget::from_cx(cx, &self.decl);

        let req = self.build_request(query, freshness, true, cx);
        let mut results = send_json_keyless(
            self.deps.net.as_ref(),
            req,
            self.decl.transport,
            &self.decl,
            cx,
            &budget,
        )
        .await?
        .map(|p| Self::map_results(&p))
        .unwrap_or_default();

        if results.is_empty() {
            let (without_forms, _) = split_forms(query);
            let (_, entity) = split_ticker(&without_forms);
            if !entity.is_empty() {
                let retry_req = self.build_request(query, freshness, false, cx);
                results = send_json_keyless(
                    self.deps.net.as_ref(),
                    retry_req,
                    self.decl.transport,
                    &self.decl,
                    cx,
                    &budget,
                )
                .await?
                .map(|p| Self::map_results(&p))
                .unwrap_or_default();
            }
        }

        results.truncate(limit);
        Ok(SrcOutput::Hits(results))
    }
}
