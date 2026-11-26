//! Quark / Shenma (Alibaba) HTML SERP engine (`quark.sm.cn/s`).
//!
//! Port of `src/search_mcp/engines/quark.py`:
//! - Never appends `site:` / `-site:` / `filetype:` (Quark returns an empty page
//!   for `site:` queries); domain/PDF filters run post-parse;
//! - Maps freshness to `tl_request=4|3|2|1`;
//! - Keeps only `ss_text`, `ss_pic`, `ss_kv`, `nature_result` cards with falsy
//!   `ads`, skipping own hosts (`sm.cn`, `quark.cn`);
//! - Detects Alibaba's `x5sec` punish burst wall.

use crate::common::{
    SerpQuery, check_wall_if_empty, decode_body, enclosing_anchor, extract_date_hint_now,
    finalize_hits, inline_text, make_hit, net_cx_for, quote_plus, search_op_decl, send_html_get,
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

pub const ENDPOINT: &str = "https://quark.sm.cn/s";

const ORGANIC_CARDS: &[&str] = &["ss_text", "ss_pic", "ss_kv", "nature_result"];
const OWN_HOSTS: &[&str] = &["sm.cn", "quark.cn"];

pub const GATE_MARKERS: &[(&str, &[&str])] = &[(
    "captcha",
    &["_____tmd_____/punish?x5secdata=", "<!--rgv587_flag"],
)];

fn is_own_host(url: &str) -> bool {
    let host = host_of(url);
    host.is_empty() || host_matches(&host, OWN_HOSTS)
}

fn is_truthy_ads(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|x| x != 0.0),
        Some(Value::String(s)) => !s.is_empty() && s != "0",
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn freshness_token(f: Freshness) -> &'static str {
    match f {
        Freshness::Day => "4",
        Freshness::Week => "3",
        Freshness::Month => "2",
        Freshness::Year => "1",
        _ => "3",
    }
}

/// Build the Quark search URL (never adds `site:` / `filetype:` operators).
pub fn build_url(q: &SerpQuery) -> String {
    let mut url = format!("{ENDPOINT}?q={}", quote_plus(&q.query));
    if let Some(f) = q.freshness {
        url.push_str("&tl_request=");
        url.push_str(freshness_token(f));
    }
    url
}

/// Parse Quark SERP HTML (`div.sc[data-sc]`) into `Hit`s.
pub fn parse(html: &str) -> Vec<Hit> {
    if html.is_empty() {
        return Vec::new();
    }
    let doc = Document::from(html);
    let mut results = Vec::new();
    let mut seen = HashSet::new();

    for card in doc.select("div.sc[data-sc]").iter() {
        let Some(raw_sc) = card.attr("data-sc") else {
            continue;
        };
        let Ok(Value::Object(meta)) = serde_json::from_str::<Value>(&raw_sc) else {
            continue;
        };
        let Some(sc_kind) = meta.get("sc").and_then(Value::as_str) else {
            continue;
        };
        if !ORGANIC_CARDS.contains(&sc_kind) || is_truthy_ads(meta.get("ads")) {
            continue;
        }
        let head = card.select(".qk-title-text").first();
        let Some(head_node) = head.nodes().first().copied() else {
            continue;
        };
        let Some(anchor) = enclosing_anchor(head_node) else {
            continue;
        };
        let url = anchor
            .attr("href")
            .map(|s| s.trim().to_owned())
            .unwrap_or_default();
        let title = inline_text(&head);
        if title.is_empty()
            || !(url.starts_with("http://") || url.starts_with("https://"))
            || is_own_host(&url)
            || !seen.insert(url.clone())
        {
            continue;
        }
        let snippet = inline_text(&card.select(".qk-paragraph-text").first());
        let source = card
            .select(".qk-source-item")
            .iter()
            .map(|n| inline_text(&n))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" · ");
        let date = extract_date_hint_now(&source).or_else(|| extract_date_hint_now(&snippet));
        results.push(make_hit(
            "quark",
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

pub struct QuarkEngine {
    decl: SourceDecl,
    deps: SourceDeps,
}

impl QuarkEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let mut decl = SourceDecl::new(
            "quark",
            "Quark / Shenma",
            "Quark/Shenma (Alibaba): Chinese web index; a third opinion beside Baidu and 360.",
        )
        .with_scopes(&["web"])
        .with_caps(&[Capability::Search])
        .with_op(search_op_decl(
            "Search Chinese web results via Quark/Shenma.",
            "python 异步编程",
            "quark",
        ));
        decl.aliases = vec!["shenma".into(), "夸克".into(), "神马".into()];
        decl.home_url = Some("https://quark.sm.cn".into());
        decl.tier = 2;
        decl.auth = AuthMode::None;
        decl.transport = TransportProfile::Chrome;
        decl.pool_role = PoolRole::Locale("zh".into());
        // Strict burst limit to avoid Alibaba's x5sec slider wall (10/min, burst 2).
        decl.limits = SourceLimits::new(10.0 / 60.0, 2, 1, 5_000, 21_600);
        Self { decl, deps }
    }
}

#[async_trait]
impl Source for QuarkEngine {
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
