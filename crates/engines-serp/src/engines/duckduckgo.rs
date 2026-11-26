//! DuckDuckGo HTML SERP engine (`html.duckduckgo.com/html/`).
//!
//! Port of `src/search_mcp/engines/duckduckgo.py`.

use crate::common::{
    SerpQuery, augment_query_with_operators, check_wall_if_empty, ddg_safesearch, decode_body,
    extract_date_hint_now, finalize_hits, make_hit, net_cx_for, quote_plus, search_op_decl,
    send_html_get, text_of,
};
use async_trait::async_trait;
use dom_query::Document;
use fsm_core::api::Freshness;
use fsm_core::source::{
    AuthMode, Capability, PoolRole, Source, SourceDecl, SourceLimits, SrcCx, SrcOutput,
    TransportProfile,
};
use fsm_core::types::{Hit, HitKind};
use fsm_core::{FsmError, SourceDeps};
use serde_json::Value;
use std::collections::HashSet;
use url::Url;

const ENDPOINT: &str = "https://html.duckduckgo.com/html/";

pub const GATE_MARKERS: &[(&str, &[&str])] = &[("captcha", &["anomaly-modal", "made by a human"])];

/// Unwrap a DuckDuckGo `/l/?uddg=...` click redirect (decoded EXACTLY once).
pub fn unwrap_ddg_url(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    let url = if raw.starts_with("//") {
        format!("https:{raw}")
    } else {
        raw.to_owned()
    };
    if let Ok(parsed) = Url::parse(&url)
        && parsed
            .host_str()
            .is_some_and(|h| h.contains("duckduckgo.com"))
        && parsed.path().starts_with("/l/")
        && let Some((_, uddg)) = parsed.query_pairs().find(|(k, _)| k == "uddg")
    {
        return uddg.into_owned();
    }
    url
}

fn freshness_token(f: Freshness) -> &'static str {
    match f {
        Freshness::Day => "d",
        Freshness::Week => "w",
        Freshness::Month => "m",
        Freshness::Year => "y",
        _ => "w",
    }
}

/// Build the DuckDuckGo HTML search URL for `q`.
pub fn build_url(q: &SerpQuery) -> String {
    let augmented = augment_query_with_operators(
        &q.query,
        &q.include_domains,
        &q.exclude_domains,
        q.filetype.as_deref(),
    );
    let mut url = format!("{ENDPOINT}?q={}", quote_plus(&augmented));
    if let Some(f) = q.freshness {
        url.push_str("&df=");
        url.push_str(freshness_token(f));
    }
    url.push_str("&kp=");
    url.push_str(ddg_safesearch(q.safesearch));
    if !q.region.is_empty() {
        url.push_str("&kl=");
        url.push_str(&quote_plus(&q.region));
    }
    url
}

/// Parse DuckDuckGo HTML SERP markup into `Hit`s.
pub fn parse(html: &str) -> Vec<Hit> {
    if html.is_empty() {
        return Vec::new();
    }
    let doc = Document::from(html);
    let mut results = Vec::new();
    let mut seen = HashSet::new();

    for div in doc.select("div.result").iter() {
        let classes = div.attr("class").map(|s| s.to_string()).unwrap_or_default();
        if classes.contains("result--ad") || classes.contains("result--sponsored") {
            continue;
        }
        let link = div.select("a.result__a").first();
        if !link.exists() {
            continue;
        }
        let href = link.attr("href").map(|s| s.to_string()).unwrap_or_default();
        let url = unwrap_ddg_url(&href);
        if url.contains("duckduckgo.com/y.js") || url.contains("ad_provider=") {
            continue;
        }
        let title = text_of(&link);
        let snippet = text_of(&div.select(".result__snippet").first());
        if url.is_empty() || title.is_empty() || !seen.insert(url.clone()) {
            continue;
        }
        let date = extract_date_hint_now(&snippet).or_else(|| extract_date_hint_now(&title));
        results.push(make_hit(
            "duckduckgo",
            HitKind::Web,
            title,
            url,
            snippet,
            date,
            Some("snippet"),
        ));
    }
    results
}

pub struct DuckDuckGoEngine {
    decl: SourceDecl,
    deps: SourceDeps,
}

impl DuckDuckGoEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let mut decl = SourceDecl::new(
            "duckduckgo",
            "DuckDuckGo",
            "General web index; the fastest and most dependable of the keyless defaults.",
        )
        .with_scopes(&["web"])
        .with_caps(&[Capability::Search])
        .with_op(search_op_decl(
            "Search the web via DuckDuckGo HTML.",
            "rust async runtime",
            "duckduckgo",
        ));
        decl.aliases = vec!["ddg".into()];
        decl.home_url = Some("https://duckduckgo.com".into());
        decl.tier = 1;
        decl.auth = AuthMode::None;
        decl.transport = TransportProfile::Chrome;
        decl.pool_role = PoolRole::Default;
        decl.limits = SourceLimits::new(1.0, 3, 2, 5_000, 21_600);
        Self { decl, deps }
    }
}

#[async_trait]
impl Source for DuckDuckGoEngine {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    async fn call(&self, op: &str, params: Value, cx: &SrcCx) -> Result<SrcOutput, FsmError> {
        let q = SerpQuery::from_call(&self.decl.id, op, &params, cx, &self.deps.cfg)?;
        let url = build_url(&q);
        let ncx = net_cx_for(cx, &q, &self.decl.id, self.decl.limits.rps);
        let resp = send_html_get(
            &self.deps,
            &self.decl.id,
            self.decl.transport,
            &url,
            &q.accept_language,
            &ncx,
        )
        .await?;
        let html = decode_body(&resp);
        let parsed = parse(&html);
        let checked = check_wall_if_empty(&self.decl.id, resp.status, &html, GATE_MARKERS, parsed)?;
        let hits = finalize_hits(
            checked,
            &q.include_domains,
            &q.exclude_domains,
            q.filetype.as_deref(),
            q.limit,
        );
        Ok(SrcOutput::Hits(hits))
    }
}
