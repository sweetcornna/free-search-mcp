//! 360 Search (so.com) HTML SERP engine (`www.so.com/s`).
//!
//! Port of `src/search_mcp/engines/so360.py`.

use crate::common::{
    SerpQuery, augment_query_with_operators, check_wall_if_empty, decode_body,
    extract_date_hint_now, finalize_hits, make_hit, net_cx_for, quote_plus, search_op_decl,
    send_html_get, text_of,
};
use async_trait::async_trait;
use dom_query::Document;
use fsm_core::api::Freshness;
use fsm_core::config::SafeSearch;
use fsm_core::source::{
    AuthMode, Capability, PoolRole, Source, SourceDecl, SourceLimits, SrcCx, SrcOutput,
    TransportProfile,
};
use fsm_core::types::{Hit, HitKind};
use fsm_core::{FsmError, SourceDeps};
use serde_json::Value;
use std::collections::HashSet;

pub const ENDPOINT: &str = "https://www.so.com/s";

pub const GATE_MARKERS: &[(&str, &[&str])] = &[("captcha", &["请输入验证码", "访问异常页面"])];

fn freshness_token(f: Freshness) -> &'static str {
    match f {
        Freshness::Day => "d",
        Freshness::Week => "w",
        Freshness::Month => "m",
        Freshness::Year => "y",
        _ => "w",
    }
}

/// Build the 360 Search URL (`q`, `rn=10..=50`, optional `adv_t`, optional `secure=1`).
pub fn build_url(q: &SerpQuery) -> String {
    let n = q.limit.clamp(10, 50);
    let augmented = augment_query_with_operators(
        &q.query,
        &q.include_domains,
        &q.exclude_domains,
        q.filetype.as_deref(),
    );
    let mut url = format!("{ENDPOINT}?q={}&rn={n}", quote_plus(&augmented));
    if let Some(f) = q.freshness {
        url.push_str("&adv_t=");
        url.push_str(freshness_token(f));
    }
    if q.safesearch != SafeSearch::Off {
        url.push_str("&secure=1");
    }
    url
}

/// Parse 360 Search HTML (`li.res-list`) into `Hit`s.
pub fn parse(html: &str) -> Vec<Hit> {
    if html.is_empty() {
        return Vec::new();
    }
    let doc = Document::from(html);
    let mut results = Vec::new();
    let mut seen = HashSet::new();

    for item in doc.select("li.res-list").iter() {
        let anchor = item.select("h3 a").first();
        if !anchor.exists() {
            continue;
        }
        let url = anchor
            .attr("data-mdurl")
            .filter(|s| !s.trim().is_empty())
            .or_else(|| anchor.attr("href"))
            .map(|s| s.trim().to_owned())
            .unwrap_or_default();
        let title = text_of(&anchor);
        if title.is_empty()
            || !(url.starts_with("http://") || url.starts_with("https://"))
            || !seen.insert(url.clone())
        {
            continue;
        }
        let body = {
            let d = item.select("p.res-desc").first();
            if d.exists() {
                d
            } else {
                item.select("div.res-comm-con").first()
            }
        };
        let snippet = if body.exists() {
            text_of(&body)
        } else {
            String::new()
        };
        let date = extract_date_hint_now(&snippet);
        results.push(make_hit(
            "so360",
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

pub struct So360Engine {
    decl: SourceDecl,
    deps: SourceDeps,
}

impl So360Engine {
    pub fn new(deps: SourceDeps) -> Self {
        let mut decl = SourceDecl::new(
            "so360",
            "360 搜索",
            "360 搜索: third Chinese web index, with direct target URLs.",
        )
        .with_scopes(&["web"])
        .with_caps(&[Capability::Search])
        .with_op(search_op_decl(
            "Search Chinese web results via 360 Search (so.com).",
            "python 异步编程",
            "so360",
        ));
        decl.aliases = vec!["360".into(), "so".into()];
        decl.home_url = Some("https://www.so.com".into());
        decl.tier = 2;
        decl.auth = AuthMode::None;
        decl.transport = TransportProfile::Chrome;
        decl.pool_role = PoolRole::Locale("zh".into());
        decl.limits = SourceLimits::new(0.5, 2, 2, 5_000, 21_600);
        Self { decl, deps }
    }
}

#[async_trait]
impl Source for So360Engine {
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
