//! Seznam Search engine (`search.seznam.cz/`).
//!
//! Port of `src/search_mcp/engines/seznam.py`: extracts organic hits from the
//! embedded JSON state in `<script id="renderer-state-data" data-state="...">`.

use crate::common::{
    SerpQuery, augment_query_with_operators, check_wall_if_empty, decode_body,
    extract_date_hint_now, finalize_hits, make_hit, net_cx_for, plain_html_fragment, quote_plus,
    search_op_decl, send_html_get, unix_seconds_to_iso_date,
};
use async_trait::async_trait;
use dom_query::Document;
use fsm_core::source::{
    AuthMode, Capability, PoolRole, Source, SourceDecl, SourceLimits, SrcCx, SrcOutput,
    TransportProfile,
};
use fsm_core::types::{Hit, HitKind};
use fsm_core::{FsmError, SourceDeps};
use serde_json::{Map, Value};
use std::collections::HashSet;

pub const ENDPOINT: &str = "https://search.seznam.cz/";

pub const GATE_MARKERS: &[(&str, &[&str])] = &[];

fn collect_organic_entities<'a>(node: &'a Value, out: &mut Vec<&'a Map<String, Value>>) {
    match node {
        Value::Object(map) => {
            if let Some(Value::Object(ident)) = map.get("id")
                && ident.get("service").and_then(Value::as_str) == Some("ftxt-organic")
                && ident.get("type").and_then(Value::as_str) == Some("result")
            {
                out.push(map);
                return;
            }
            for val in map.values() {
                collect_organic_entities(val, out);
            }
        }
        Value::Array(arr) => {
            for val in arr {
                collect_organic_entities(val, out);
            }
        }
        _ => {}
    }
}

fn shown_date(data: &Map<String, Value>) -> Option<String> {
    // Seznam prints publicationDate only on video/forum hits; on other pages it
    // reflects first-crawl time and would mislabel evergreen docs as stale.
    if !data.contains_key("video") && !data.contains_key("forumPosting") {
        return None;
    }
    let attrs = data.get("attributes")?.as_object()?;
    let stamp = attrs.get("publicationDate")?;
    let secs = if let Some(i) = stamp.as_i64() {
        i
    } else if let Some(f) = stamp.as_f64()
        && f.is_finite()
    {
        f as i64
    } else {
        return None;
    };
    unix_seconds_to_iso_date(secs)
}

/// Build the Seznam search URL (`q`, plus `count=20` only when `limit > 10`).
pub fn build_url(q: &SerpQuery) -> String {
    let augmented = augment_query_with_operators(
        &q.query,
        &q.include_domains,
        &q.exclude_domains,
        q.filetype.as_deref(),
    );
    let mut url = format!("{ENDPOINT}?q={}", quote_plus(&augmented));
    if q.limit > 10 {
        url.push_str("&count=20");
    }
    url
}

/// Parse Seznam HTML page by reading `script#renderer-state-data[data-state]`.
pub fn parse(html: &str) -> Vec<Hit> {
    if html.is_empty() {
        return Vec::new();
    }
    let doc = Document::from(html);
    let script = doc.select("script#renderer-state-data").first();
    let Some(raw) = script.attr("data-state") else {
        return Vec::new();
    };
    let Ok(Value::Object(state)) = serde_json::from_str::<Value>(&raw) else {
        return Vec::new();
    };
    let Some(entities) = state
        .get("data")
        .and_then(Value::as_object)
        .and_then(|d| d.get("entities"))
    else {
        return Vec::new();
    };

    let mut organic = Vec::new();
    collect_organic_entities(entities, &mut organic);

    let mut results = Vec::new();
    let mut seen = HashSet::new();

    for entity in organic {
        let Some(data) = entity.get("data").and_then(Value::as_object) else {
            continue;
        };
        let Some(snip) = data.get("snippet").and_then(Value::as_object) else {
            continue;
        };
        let Some(raw_url) = snip.get("url").and_then(Value::as_str) else {
            continue;
        };
        let url = raw_url.trim().to_owned();
        let title = snip
            .get("title")
            .and_then(Value::as_str)
            .map(plain_html_fragment)
            .unwrap_or_default();
        if title.is_empty()
            || !(url.starts_with("http://") || url.starts_with("https://"))
            || !seen.insert(url.clone())
        {
            continue;
        }
        let snippet = snip
            .get("description")
            .and_then(Value::as_str)
            .map(plain_html_fragment)
            .unwrap_or_default();
        let date = shown_date(data).or_else(|| extract_date_hint_now(&snippet));
        results.push(make_hit(
            "seznam",
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

pub struct SeznamEngine {
    decl: SourceDecl,
    deps: SourceDeps,
}

impl SeznamEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let mut decl = SourceDecl::new(
            "seznam",
            "Seznam",
            "Seznam.cz: independent Czech crawler index; strong on Czech, solid on English.",
        )
        .with_scopes(&["web"])
        .with_caps(&[Capability::Search])
        .with_op(search_op_decl(
            "Search the web via Seznam.cz.",
            "python asyncio taskgroup",
            "seznam",
        ));
        decl.home_url = Some("https://search.seznam.cz".into());
        decl.tier = 2;
        decl.auth = AuthMode::None;
        decl.transport = TransportProfile::Chrome;
        decl.pool_role = PoolRole::Default;
        decl.limits = SourceLimits::new(1.0, 3, 2, 5_000, 21_600);
        Self { decl, deps }
    }
}

#[async_trait]
impl Source for SeznamEngine {
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
