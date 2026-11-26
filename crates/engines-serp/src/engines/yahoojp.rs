//! Yahoo! JAPAN Search HTML SERP engine (`search.yahoo.co.jp/search`).
//!
//! Port of `src/search_mcp/engines/yahoojp.py`: Google's organic index served
//! keylessly as HTML (`div.sw-Card.Algo`), with `vd=d|w|m1|y` freshness and
//! EEA/UK region-gate detection.

use crate::common::{
    SerpQuery, augment_query_with_operators, check_wall_if_empty, decode_body,
    extract_date_hint_now, finalize_hits, inline_text, make_hit, net_cx_for, quote_plus,
    search_op_decl, send_html_get,
};
use async_trait::async_trait;
use dom_query::Document;
use fsm_core::api::Freshness;
use fsm_core::source::{
    AuthMode, Capability, PoolRole, Source, SourceDecl, SourceLimits, SrcCx, SrcOutput,
    TransportProfile,
};
use fsm_core::types::{Hit, HitKind};
use fsm_core::url_canon::{host_matches, host_of};
use fsm_core::{FsmError, SourceDeps};
use serde_json::Value;
use std::collections::HashSet;

pub const ENDPOINT: &str = "https://search.yahoo.co.jp/search";

const INTERNAL_HOSTS: &[&str] = &["search.yahoo.co.jp", "rdsig.yahoo.co.jp", "ard.yahoo.co.jp"];

pub const GATE_MARKERS: &[(&str, &[&str])] = &[
    (
        "region",
        &[r#"<meta property="og:url" content="https://privacy.yahoo.co.jp/areacheck"#],
    ),
    (
        "rate_limit",
        &["ご覧になろうとしているページは現在表示できません"],
    ),
];

fn is_internal(url: &str) -> bool {
    let host = host_of(url);
    host.is_empty() || host_matches(&host, INTERNAL_HOSTS)
}

fn freshness_token(f: Freshness) -> &'static str {
    match f {
        Freshness::Day => "d",
        Freshness::Week => "w",
        // Bare `vd=m` is silently ignored by Yahoo! JAPAN; must send `m1`.
        Freshness::Month => "m1",
        Freshness::Year => "y",
        _ => "w",
    }
}

/// Build the Yahoo! JAPAN search URL (`p`, `ei=UTF-8`, optional `vd=d|w|m1|y`).
pub fn build_url(q: &SerpQuery) -> String {
    let augmented = augment_query_with_operators(
        &q.query,
        &q.include_domains,
        &q.exclude_domains,
        q.filetype.as_deref(),
    );
    let mut url = format!("{ENDPOINT}?p={}&ei=UTF-8", quote_plus(&augmented));
    if let Some(f) = q.freshness {
        url.push_str("&vd=");
        url.push_str(freshness_token(f));
    }
    url
}

/// Parse Yahoo! JAPAN SERP HTML into `Hit`s.
pub fn parse(html: &str) -> Vec<Hit> {
    if html.is_empty() {
        return Vec::new();
    }
    let doc = Document::from(html);
    let mut results = Vec::new();
    let mut seen = HashSet::new();

    for card in doc.select("div.sw-Card.Algo").iter() {
        let anchor = card.select("a.sw-Card__titleInner").first();
        if !anchor.exists() {
            continue;
        }
        let url = anchor
            .attr("href")
            .map(|s| s.trim().to_owned())
            .unwrap_or_default();
        let title = inline_text(&anchor.select("h3").first());
        if title.is_empty() || !(url.starts_with("http://") || url.starts_with("https://")) {
            continue;
        }
        if is_internal(&url) || !seen.insert(url.clone()) {
            continue;
        }
        let snippet = inline_text(&card.select("p.sw-Card__summary").first());
        let date = extract_date_hint_now(&snippet);
        results.push(make_hit(
            "yahoojp",
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

pub struct YahooJapanEngine {
    decl: SourceDecl,
    deps: SourceDeps,
}

impl YahooJapanEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let mut decl = SourceDecl::new(
            "yahoojp",
            "Yahoo! JAPAN",
            "Yahoo! JAPAN: Google's web index with no key, in any language; strongest for Japanese.",
        )
        .with_scopes(&["web"])
        .with_caps(&[Capability::Search])
        .with_op(search_op_decl(
            "Search the web via Yahoo! JAPAN.",
            "python asyncio taskgroup",
            "yahoojp",
        ));
        decl.aliases = vec!["yahoo_jp".into(), "yahoo_japan".into()];
        decl.home_url = Some("https://search.yahoo.co.jp".into());
        decl.tier = 2;
        decl.auth = AuthMode::None;
        decl.transport = TransportProfile::Chrome;
        decl.pool_role = PoolRole::Default;
        decl.limits = SourceLimits::new(0.5, 2, 2, 5_000, 21_600);
        Self { decl, deps }
    }
}

#[async_trait]
impl Source for YahooJapanEngine {
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
