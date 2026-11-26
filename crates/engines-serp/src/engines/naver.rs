//! Naver Search HTML SERP engine (`search.naver.com/search.naver`).
//!
//! Port of `src/search_mcp/engines/naver.py`:
//! - Selects only `div.fds-web-doc-root` web documents (skipping UGC blogs/cafes,
//!   power-link ads, and navigation);
//! - Strips the `"새 창 열림"` accessibility label;
//! - Parses Naver's dotted profile date (`"2026.05.24."`) or Korean relative age
//!   (`"3주 전"`) without false-matching CalVer versions globally;
//! - Maps freshness to `sm=tab_opt&nso=p:1d|1w|1m|1y`.

use crate::common::{
    SerpQuery, augment_query_with_operators, check_wall_if_empty_with_exempt, collapse_ws,
    decode_body, enclosing_anchor, extract_date_hint_now, finalize_hits, inline_text, make_hit,
    net_cx_for, quote_plus, search_op_decl, send_html_get,
};
use async_trait::async_trait;
use dom_query::{Document, Selection};
use fsm_core::api::Freshness;
use fsm_core::source::{
    AuthMode, Capability, PoolRole, Source, SourceDecl, SourceLimits, SrcCx, SrcOutput,
    TransportProfile,
};
use fsm_core::types::{Hit, HitKind};
use fsm_core::url_canon::{host_matches, host_of};
use fsm_core::{FsmError, SourceDeps};
use jiff::civil::Date;
use regex::Regex;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::LazyLock;

pub const ENDPOINT: &str = "https://search.naver.com/search.naver";
const NEW_WINDOW_LABEL: &str = "새 창 열림";
const AD_HOSTS: &[&str] = &["ader.naver.com", "ad.search.naver.com"];

pub const GATE_MARKERS: &[(&str, &[&str])] = &[];

/// Shared gate markers that appear benignly on normal Naver SERPs (e.g.
/// `gw.in.naver.com/.../refresh-captcha` contains the substring `h-captcha`)
/// and must not misclassify a legitimately empty Naver page as a captcha wall.
pub const SHARED_MARKER_EXEMPT: &[&str] = &["h-captcha"];

static PROFILE_DATE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d{4})\.(\d{1,2})\.(\d{1,2})\.").expect("regex"));

fn clean_text(sel: &Selection<'_>) -> String {
    collapse_ws(&inline_text(sel).replace(NEW_WINDOW_LABEL, " "))
}

fn parse_profile_date(text: &str) -> Option<String> {
    if let Some(c) = PROFILE_DATE_RE.captures(text) {
        let y: i16 = c[1].parse().ok()?;
        let m: i8 = c[2].parse().ok()?;
        let d: i8 = c[3].parse().ok()?;
        return Date::new(y, m, d).ok().map(|dt| dt.to_string());
    }
    extract_date_hint_now(text)
}

fn freshness_token(f: Freshness) -> &'static str {
    match f {
        Freshness::Day => "p:1d",
        Freshness::Week => "p:1w",
        Freshness::Month => "p:1m",
        Freshness::Year => "p:1y",
        _ => "p:1w",
    }
}

/// Build the Naver search URL (`where=nexearch`, `query`, optional `sm=tab_opt&nso=...`).
pub fn build_url(q: &SerpQuery) -> String {
    let augmented = augment_query_with_operators(
        &q.query,
        &q.include_domains,
        &q.exclude_domains,
        q.filetype.as_deref(),
    );
    let mut url = format!("{ENDPOINT}?where=nexearch&query={}", quote_plus(&augmented));
    if let Some(f) = q.freshness {
        url.push_str("&sm=tab_opt&nso=");
        url.push_str(&quote_plus(freshness_token(f)));
    }
    url
}

/// Parse Naver SERP HTML (`div.fds-web-doc-root`) into `Hit`s.
pub fn parse(html: &str) -> Vec<Hit> {
    if html.is_empty() {
        return Vec::new();
    }
    let doc = Document::from(html);
    let mut results = Vec::new();
    let mut seen = HashSet::new();

    for item in doc.select("div.fds-web-doc-root").iter() {
        let head = item.select(".sds-comps-text-type-headline1").first();
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
        let title = clean_text(&head);
        if title.is_empty() || !(url.starts_with("http://") || url.starts_with("https://")) {
            continue;
        }
        let host = host_of(&url);
        if host.is_empty() || host_matches(&host, AD_HOSTS) || !seen.insert(url.clone()) {
            continue;
        }
        let snippet = clean_text(&item.select(".sds-comps-text-type-body1").first());
        let profile = clean_text(&item.select(".sds-comps-profile-info-subtext").first());
        let date = parse_profile_date(&profile).or_else(|| extract_date_hint_now(&snippet));
        results.push(make_hit(
            "naver",
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

pub struct NaverEngine {
    decl: SourceDecl,
    deps: SourceDeps,
}

impl NaverEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let mut decl = SourceDecl::new(
            "naver",
            "Naver",
            "Naver: Korea's main web index; reach for Korean-language queries and sites.",
        )
        .with_scopes(&["web"])
        .with_caps(&[Capability::Search])
        .with_op(search_op_decl(
            "Search Korean and web documents via Naver.",
            "파이썬 비동기",
            "naver",
        ));
        decl.aliases = vec!["네이버".into()];
        decl.home_url = Some("https://search.naver.com".into());
        decl.tier = 2;
        decl.auth = AuthMode::None;
        decl.transport = TransportProfile::Chrome;
        decl.pool_role = PoolRole::Locale("ko".into());
        decl.limits = SourceLimits::new(1.0, 3, 2, 5_000, 21_600);
        Self { decl, deps }
    }
}

#[async_trait]
impl Source for NaverEngine {
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
        let checked = check_wall_if_empty_with_exempt(
            &self.decl.id,
            resp.status,
            &html,
            GATE_MARKERS,
            SHARED_MARKER_EXEMPT,
            parsed,
        )?;
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
