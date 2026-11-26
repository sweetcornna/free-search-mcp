//! Daum Search HTML SERP engine (`search.daum.net/search?w=fusion`).
//!
//! New 1.0 Korean secondary web index (`D/research/general.md` L62, `SPEC.md` §5):
//! - Uses `w=fusion` (통합웹 / unified web search, ~15 organic `<c-card>` results
//!   per page with far less ad/widget clutter than `w=tot`);
//! - Extracts `<c-card>` items carrying `<c-doc-web>` both directly and from
//!   Daum's `<div class="cubic"><template>...</template></div>` container;
//! - Maps freshness to Daum's `period=d|w|m|y` parameter;
//! - Parses Daum's `YYYY.MM.DD` footer dates (`<c-footer-desc>`) and Korean
//!   relative ages (`extract_date_hint`);
//! - Falls back to classic Daum web-document markup if cubic cards are absent.

use crate::common::{
    SerpQuery, augment_query_with_operators, check_wall_if_empty, decode_body,
    extract_date_hint_now, finalize_hits, inline_text, make_hit, net_cx_for, quote_plus,
    search_op_decl, send_html_get, text_of,
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

pub const ENDPOINT: &str = "https://search.daum.net/search";

const DROPPED_HOSTS: &[&str] = &[
    "search.daum.net",
    "sl.search.daum.net",
    "ad.daum.net",
    "ader.naver.com",
    "daumcorp.com",
    "kakaocorp.com",
    "cs.kakao.com",
];

pub const GATE_MARKERS: &[(&str, &[&str])] = &[("captcha", &["자동입력방지", "비정상적인 접근"])];

static FOOTER_DATE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d{4})\.(\d{1,2})\.(\d{1,2})\.?$").expect("regex"));

fn is_dropped(url: &str) -> bool {
    let host = host_of(url);
    host.is_empty() || host_matches(&host, DROPPED_HOSTS)
}

fn parse_footer_date(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if let Some(c) = FOOTER_DATE_RE.captures(trimmed) {
        let y: i16 = c[1].parse().ok()?;
        let m: i8 = c[2].parse().ok()?;
        let d: i8 = c[3].parse().ok()?;
        return Date::new(y, m, d).ok().map(|dt| dt.to_string());
    }
    extract_date_hint_now(trimmed)
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

/// Build the Daum unified-web search URL (`w=fusion`, `q`, optional `period=d|w|m|y`).
pub fn build_url(q: &SerpQuery) -> String {
    let augmented = augment_query_with_operators(
        &q.query,
        &q.include_domains,
        &q.exclude_domains,
        q.filetype.as_deref(),
    );
    let mut url = format!("{ENDPOINT}?w=fusion&q={}", quote_plus(&augmented));
    if let Some(f) = q.freshness {
        url.push_str("&period=");
        url.push_str(freshness_token(f));
    }
    url
}

fn collect_cards_from_doc(doc: &Document, seen: &mut HashSet<String>, out: &mut Vec<Hit>) {
    for card in doc.select("c-card").iter() {
        let web_doc = card.select("c-doc-web").first();
        if !web_doc.exists() {
            continue;
        }
        extract_cubic_card(&card, &web_doc, seen, out);
    }
}

fn extract_cubic_card(
    card: &Selection<'_>,
    web_doc: &Selection<'_>,
    seen: &mut HashSet<String>,
    out: &mut Vec<Hit>,
) {
    let title_el = web_doc.select("c-title").first();
    if !title_el.exists() {
        return;
    }
    let url = title_el
        .attr("data-href")
        .or_else(|| title_el.select("a").first().attr("href"))
        .map(|s| s.trim().to_owned())
        .unwrap_or_default();
    let title = inline_text(&title_el);
    if title.is_empty()
        || !(url.starts_with("http://") || url.starts_with("https://"))
        || is_dropped(&url)
        || !seen.insert(url.clone())
    {
        return;
    }
    let snippet = inline_text(&web_doc.select("c-contents-desc").first());
    let footer = inline_text(&card.select("c-footer-desc").first());
    let date = parse_footer_date(&footer).or_else(|| extract_date_hint_now(&snippet));
    out.push(make_hit(
        "daum",
        HitKind::Web,
        title,
        url,
        snippet,
        date,
        Some("snippet"),
    ));
}

fn unwrap_template_outer_html(outer: &str) -> &str {
    let trimmed = outer.trim();
    let after_open = if trimmed.len() >= 9 && trimmed[..9].eq_ignore_ascii_case("<template") {
        trimmed.find('>').map_or(trimmed, |idx| &trimmed[idx + 1..])
    } else {
        trimmed
    };
    if after_open.len() >= 11
        && after_open[after_open.len() - 11..].eq_ignore_ascii_case("</template>")
    {
        &after_open[..after_open.len() - 11]
    } else {
        after_open
    }
}

/// Parse Daum SERP HTML (`w=fusion` or `w=tot` cubic cards, with classic HTML fallback).
pub fn parse(html: &str) -> Vec<Hit> {
    if html.is_empty() {
        return Vec::new();
    }
    let doc = Document::from(html);
    let mut results = Vec::new();
    let mut seen = HashSet::new();

    // 1. Direct `<c-card>` elements in the document (if already hydrated or flattened).
    collect_cards_from_doc(&doc, &mut seen, &mut results);

    // 2. `<template>` contents (HTML5 `<template>` stores children in an inert
    // `template_contents` fragment; `dom_query`'s `html()` serializes it while
    // `inner_html()` only inspects direct children).
    for tpl in doc.select("template").iter() {
        let outer = tpl.html();
        let inner = unwrap_template_outer_html(&outer);
        if inner.contains("c-card") {
            let frag = Document::fragment(inner);
            collect_cards_from_doc(&frag, &mut seen, &mut results);
        }
    }

    // 3. Fallback for classic Daum web markup if no cubic `<c-card>` was present.
    if results.is_empty() {
        for li in doc.select("ul.c-list-basic li, div.wrap_cont").iter() {
            let anchor = {
                let a = li.select("div.item-title a").first();
                if a.exists() {
                    a
                } else {
                    li.select("a.f_link_b").first()
                }
            };
            if !anchor.exists() {
                continue;
            }
            let url = anchor
                .attr("href")
                .map(|s| s.trim().to_owned())
                .unwrap_or_default();
            let title = text_of(&anchor);
            if title.is_empty()
                || !(url.starts_with("http://") || url.starts_with("https://"))
                || is_dropped(&url)
                || !seen.insert(url.clone())
            {
                continue;
            }
            let snippet = {
                let d = li.select("p.desc, .item-contents").first();
                if d.exists() {
                    text_of(&d)
                } else {
                    String::new()
                }
            };
            let date = extract_date_hint_now(&snippet);
            results.push(make_hit(
                "daum",
                HitKind::Web,
                title,
                url,
                snippet,
                date,
                Some("snippet"),
            ));
        }
    }

    results
}

pub struct DaumEngine {
    decl: SourceDecl,
    deps: SourceDeps,
}

impl DaumEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let mut decl = SourceDecl::new(
            "daum",
            "Daum",
            "Daum: secondary Korean web index complementing Naver for Korean queries.",
        )
        .with_scopes(&["web"])
        .with_caps(&[Capability::Search])
        .with_op(search_op_decl(
            "Search Korean and web pages via Daum.",
            "파이썬 비동기",
            "daum",
        ));
        decl.aliases = vec!["다음".into(), "kakao_daum".into()];
        decl.home_url = Some("https://search.daum.net".into());
        decl.tier = 2;
        decl.auth = AuthMode::None;
        decl.transport = TransportProfile::Chrome;
        decl.pool_role = PoolRole::Locale("ko".into());
        decl.limits = SourceLimits::new(1.0, 3, 2, 5_000, 21_600);
        Self { decl, deps }
    }
}

#[async_trait]
impl Source for DaumEngine {
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
