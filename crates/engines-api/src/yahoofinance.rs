//! Yahoo Finance search engine: resolve tickers/instruments and relevant market news.

use crate::common::{
    CallBudget, SNIPPET_CAP, clip, epoch_seconds_to_ymd, make_hit, parse_search_params, quote_plus,
    search_op, send_json_keyless,
};
use async_trait::async_trait;
use fsm_core::{
    AuthMode, Capability, FsmError, Hit, HitKind, HttpRequest, PoolRole, ProbeSpec, Source,
    SourceDecl, SourceDeps, SourceLimits, SrcCx, SrcOutput, TransportProfile,
};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::LazyLock;

const ENDPOINT: &str = "https://query2.finance.yahoo.com/v1/finance/search";

static FILLER: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "earnings",
        "earning",
        "stock",
        "stocks",
        "share",
        "shares",
        "price",
        "prices",
        "news",
        "quote",
        "quotes",
        "forecast",
        "outlook",
        "revenue",
        "guidance",
        "results",
        "result",
        "report",
        "financials",
        "dividend",
        "valuation",
        "analysis",
        "the",
        "for",
        "of",
        "and",
        "fy",
        "q1",
        "q2",
        "q3",
        "q4",
        "股价",
        "财报",
        "业绩",
        "股票",
        "行情",
        "营收",
        "预测",
    ]
    .into_iter()
    .collect()
});

static CORPORATE_SUFFIXES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "inc",
        "inc.",
        "corp",
        "corp.",
        "corporation",
        "ltd",
        "ltd.",
        "plc",
        "holdings",
        "group",
        "company",
        "co",
        "co.",
        "sa",
        "ag",
        "nv",
        "the",
    ]
    .into_iter()
    .collect()
});

/// Drop finance filler words so Yahoo's entity resolver sees the company/ticker name.
pub fn entity_query(query: &str) -> String {
    let kept: Vec<&str> = query
        .split_whitespace()
        .filter(|tok| !FILLER.contains(tok.trim().to_lowercase().as_str()))
        .collect();
    if kept.is_empty() {
        query.to_owned()
    } else {
        kept.join(" ")
    }
}

/// Yahoo Finance instrument and market-news search engine.
pub struct YahooFinanceEngine {
    decl: SourceDecl,
    deps: SourceDeps,
}

impl YahooFinanceEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let mut decl = SourceDecl::new(
            "yahoofinance",
            "Yahoo Finance",
            "Yahoo Finance: resolve tickers and pull recent market news for a company.",
        )
        .with_scopes(&["finance"])
        .with_caps(&[Capability::Search])
        .with_op(
            search_op(
                "Resolve stock tickers and fetch company-relevant market news.",
                20,
                r#"source.call("yahoofinance.search", { query: "nvidia earnings", limit: 6 })"#,
            )
            .with_probe(ProbeSpec::new(json!({"query": "nvidia", "limit": 3}), 1)),
        );
        decl.aliases = vec!["yahoo finance".to_owned(), "ticker".to_owned()];
        decl.home_url = Some("https://finance.yahoo.com".to_owned());
        decl.auth = AuthMode::None;
        decl.transport = TransportProfile::Chrome;
        decl.pool_role = PoolRole::ScopeOnly;
        let mut limits = SourceLimits::new(2.0, 4, 2, 4000, 3600);
        limits.host_bucket = Some("query2.finance.yahoo.com".to_owned());
        decl.limits = limits;
        Self { decl, deps }
    }

    /// Map `quotes[]` entries into hits.
    pub fn map_quotes(quotes: Option<&Value>) -> Vec<Hit> {
        let Some(items) = quotes.and_then(Value::as_array) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for q in items {
            let Some(obj) = q.as_object() else {
                continue;
            };
            let Some(symbol) = obj
                .get("symbol")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let name = obj
                .get("longname")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    obj.get("shortname")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                })
                .unwrap_or(symbol);
            let mut bits = Vec::new();
            for key in ["typeDisp", "exchDisp", "sectorDisp", "industryDisp"] {
                if let Some(v) = obj
                    .get(key)
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    bits.push(v);
                }
            }
            out.push(make_hit(
                "yahoofinance",
                clip(&format!("{symbol} — {name}"), 300),
                format!("https://finance.yahoo.com/quote/{}", quote_plus(symbol)),
                clip(&bits.join(" · "), SNIPPET_CAP),
                None,
                None,
                HitKind::Vertical,
            ));
        }
        out
    }

    /// Derive lower-cased relevance terms from the query and resolved quotes.
    ///
    /// Company words come from the top match only (`quotes[0]`) so ETF/fund names
    /// in secondary rows cannot admit unrelated headlines.
    pub fn relevance_terms(query: &str, quotes: &[Hit]) -> HashSet<String> {
        let mut terms: HashSet<String> = query
            .split_whitespace()
            .filter(|tok| tok.chars().count() > 2 && !FILLER.contains(tok.to_lowercase().as_str()))
            .map(|tok| {
                tok.trim_matches(['.', ',', ';', ':', '!', '?', '(', ')', '[', ']', '"', '\''])
                    .to_lowercase()
            })
            .filter(|t| !t.is_empty())
            .collect();

        for quote in quotes {
            if let Some((symbol, _)) = quote.title.split_once(" — ") {
                let s = symbol.trim().to_lowercase();
                if !s.is_empty() {
                    terms.insert(s);
                }
            }
        }
        if let Some(top) = quotes.first()
            && let Some((_, company)) = top.title.split_once(" — ")
        {
            for word in company.split_whitespace() {
                if word.chars().count() > 2
                    && !CORPORATE_SUFFIXES.contains(word.to_lowercase().as_str())
                {
                    let cleaned = word.trim_matches(['.', ',']).to_lowercase();
                    if !cleaned.is_empty() {
                        terms.insert(cleaned);
                    }
                }
            }
        }
        terms
    }

    /// Map `news[]` entries, keeping only headlines relevant to the query or resolved entity.
    pub fn map_news(news: Option<&Value>, query: &str, quotes: &[Hit]) -> Vec<Hit> {
        let Some(items) = news.and_then(Value::as_array) else {
            return Vec::new();
        };
        let terms = Self::relevance_terms(query, quotes);
        if terms.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for item in items {
            let Some(obj) = item.as_object() else {
                continue;
            };
            let Some(title) = obj
                .get("title")
                .and_then(Value::as_str)
                .filter(|t| !t.trim().is_empty())
            else {
                continue;
            };
            let Some(link) = obj
                .get("link")
                .and_then(Value::as_str)
                .filter(|l| l.starts_with("http"))
            else {
                continue;
            };
            let low = title.to_lowercase();
            if !terms.iter().any(|term| low.contains(term.as_str())) {
                continue;
            }
            let published = Self::published(obj.get("providerPublishTime"));
            let mut bits = Vec::new();
            if let Some(pub_name) = obj
                .get("publisher")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                bits.push(pub_name.to_owned());
            }
            if !published.is_empty() {
                bits.push(published.clone());
            }
            out.push(make_hit(
                "yahoofinance",
                clip(title, 300),
                link.to_owned(),
                clip(&bits.join(" · "), SNIPPET_CAP),
                (!published.is_empty()).then_some(published),
                Some("api"),
                HitKind::News,
            ));
        }
        out
    }

    fn published(value: Option<&Value>) -> String {
        let Some(v) = value else {
            return String::new();
        };
        if v.is_boolean() {
            return String::new();
        }
        if let Some(sec) = v.as_i64() {
            epoch_seconds_to_ymd(sec)
        } else if let Some(f) = v.as_f64().filter(|f| f.is_finite()) {
            epoch_seconds_to_ymd(f.trunc() as i64)
        } else {
            String::new()
        }
    }
}

#[async_trait]
impl Source for YahooFinanceEngine {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    async fn call(&self, op: &str, params: Value, cx: &SrcCx) -> Result<SrcOutput, FsmError> {
        let (query, limit) = parse_search_params("yahoofinance", op, &params, 10)?;
        if query.trim().is_empty() {
            return Ok(SrcOutput::Hits(Vec::new()));
        }
        let n = limit.clamp(1, 20);
        let quotes_count = n.min(6);
        let news_count = (n * 3).min(20);

        let req = HttpRequest::get(ENDPOINT)
            .header("Accept", "application/json")
            .query_param("q", entity_query(query))
            .query_param("quotesCount", quotes_count.to_string())
            .query_param("newsCount", news_count.to_string())
            .for_source("yahoofinance");

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

        let mut quotes = Self::map_quotes(payload.get("quotes"));
        let news = Self::map_news(payload.get("news"), query, &quotes);
        let cap = (limit / 2).max(1);
        let rest_quotes = if quotes.len() > cap {
            quotes.split_off(cap)
        } else {
            Vec::new()
        };
        let mut results = quotes;
        results.extend(news);
        results.extend(rest_quotes);
        results.truncate(limit);
        Ok(SrcOutput::Hits(results))
    }
}
