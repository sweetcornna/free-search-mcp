//! Budget-bounded Markdown renderers for every [`fsm_core::RenderKind`] and compact JSON
//! water-filling truncation for untagged `exec` results (`SPEC.md` §2.5).

use crate::sanitize::{
    age_phrase, cap_to_tokens, date_crumb, elapsed_crumb, escape_delimiters, invalid_fts_hint,
    retrieved_crumb, sanitize_block, sanitize_inline, sanitize_inline_capped, sanitize_url,
};
use fsm_core::text::{clip_chars, estimate_tokens, shrink_json, smart_truncate};
use fsm_core::{
    AskResponse, AuthorProfile, CacheHit, CatalogDetail, CatalogHit, CitationGraph, CompareItem,
    DocOutline, DocSlice, DownloadResult, FetchMethod, FulltextResult, GrantRecord, GraphNotice,
    Hit, HitKind, InlineImageMeta, IntegrityReport, OrgProfile, PageResult, RenderKind,
    ResearchBrief, RetractionNotice, ScholarlyWork, SearchResponse, StructuredMeta, VenueProfile,
    WorkIds, WorkResult,
};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};

/// Default token budget for [`RenderOpts`].
pub const DEFAULT_MAX_TOKENS: usize = 6000;

/// Maximum snippet character length in `search` hits (`SPEC.md` §2.5.1).
const MAX_HIT_SNIPPET_CHARS: usize = 280;
/// Maximum lead character length in `search` responses.
const MAX_LEAD_CHARS: usize = 320;
/// Maximum JSON nesting depth retained before pruning (prevents stack overflow on pathological values).
const MAX_JSON_DEPTH: usize = 48;

/// Rendering options shared by [`render`] and [`render_json`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderOpts {
    pub max_tokens: usize,
    pub query: Option<String>,
    pub now: Option<String>,
}

impl Default for RenderOpts {
    fn default() -> Self {
        Self {
            max_tokens: DEFAULT_MAX_TOKENS,
            query: None,
            now: None,
        }
    }
}

impl RenderOpts {
    pub fn with_max_tokens(mut self, max_tokens: usize) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    pub fn with_query(mut self, query: impl Into<String>) -> Self {
        self.query = Some(query.into());
        self
    }

    pub fn with_now(mut self, now: impl Into<String>) -> Self {
        self.now = Some(now.into());
        self
    }
}

/// Render any [`RenderKind`] value as budget-bounded, delimiter-safe Markdown.
///
/// Never panics on malformed or unexpected JSON shapes: if `value` does not deserialize
/// cleanly into the expected `fsm-core` type for `kind`, falls back to [`render_json`].
/// Always guarantees `estimate_tokens(&result) <= opts.max_tokens`.
pub fn render(kind: RenderKind, value: &Value, opts: &RenderOpts) -> String {
    if opts.max_tokens == 0 {
        return String::new();
    }
    let rendered = match kind {
        RenderKind::SearchResponse => serde_json::from_value::<SearchResponse>(value.clone())
            .ok()
            .map(|v| render_search_response(&v, opts)),
        RenderKind::PageResult => serde_json::from_value::<PageResult>(value.clone())
            .ok()
            .map(|v| render_page_result(&v, opts)),
        RenderKind::PageList => {
            decode_list::<PageResult>(value, true).map(|v| render_page_list(&v, opts))
        }
        RenderKind::DocSlice => serde_json::from_value::<DocSlice>(value.clone())
            .ok()
            .map(|v| render_doc_slice(&v, opts)),
        RenderKind::FulltextResult => serde_json::from_value::<FulltextResult>(value.clone())
            .ok()
            .map(|v| render_fulltext(&v, opts)),
        RenderKind::WorkResult => serde_json::from_value::<WorkResult>(value.clone())
            .ok()
            .map(|v| render_work_result(&v, opts)),
        RenderKind::WorkList => {
            decode_list::<ScholarlyWork>(value, true).map(|v| render_work_list(&v, opts))
        }
        RenderKind::ResearchBrief => serde_json::from_value::<ResearchBrief>(value.clone())
            .ok()
            .map(|v| render_research_brief(&v, opts)),
        RenderKind::CitationGraph => serde_json::from_value::<CitationGraph>(value.clone())
            .ok()
            .map(|v| render_citation_graph(&v, opts)),
        RenderKind::CatalogList => {
            decode_list::<CatalogHit>(value, true).map(|v| render_catalog_list(&v, opts))
        }
        RenderKind::InlineImage => serde_json::from_value::<InlineImageMeta>(value.clone())
            .ok()
            .map(|v| render_inline_image(&v, opts)),
        RenderKind::AskResponse => serde_json::from_value::<AskResponse>(value.clone())
            .ok()
            .map(|v| render_ask_response(&v, opts)),
        _ => None,
    };

    match rendered {
        Some(md) => cap_to_tokens(&md, opts.max_tokens),
        None => render_json(value, opts),
    }
}

fn decode_list<T: serde::de::DeserializeOwned>(
    value: &Value,
    allow_single: bool,
) -> Option<Vec<T>> {
    match value {
        Value::Array(_) => serde_json::from_value::<Vec<T>>(value.clone()).ok(),
        Value::Object(_) if allow_single => serde_json::from_value::<T>(value.clone())
            .ok()
            .map(|x| vec![x]),
        _ => None,
    }
}

/// Render an untagged `exec` JSON value as compact JSON, shrinking structurally via
/// water-filling when over `opts.max_tokens`. Never panics even on deeply nested or
/// huge values, neutralizes host-delimiter spoofs, and guarantees
/// `estimate_tokens(&result) <= opts.max_tokens`.
pub fn render_json(value: &Value, opts: &RenderOpts) -> String {
    if opts.max_tokens == 0 {
        return String::new();
    }
    let sanitized = sanitize_json_iterative(value, MAX_JSON_DEPTH);
    let shrunk = shrink_json(&sanitized, opts.max_tokens);
    let text = serde_json::to_string(&shrunk.value).unwrap_or_else(|_| "null".to_owned());
    if estimate_tokens(&text) <= opts.max_tokens {
        return text;
    }
    // Pathological long object keys or very small token budget: shrink keys and retry.
    let key_capped = cap_long_keys(&shrunk.value, 24);
    let shrunk2 = shrink_json(&key_capped, opts.max_tokens);
    let text2 = serde_json::to_string(&shrunk2.value).unwrap_or_else(|_| "null".to_owned());
    if estimate_tokens(&text2) <= opts.max_tokens {
        return text2;
    }
    cap_to_tokens(&text2, opts.max_tokens)
}

/// Iteratively sanitize string values (delimiter escaping) and bound nesting depth so
/// `shrink_json` and `serde_json::to_string` can never overflow the stack.
fn sanitize_json_iterative(root: &Value, max_depth: usize) -> Value {
    enum Frame<'a> {
        Enter(&'a Value, usize),
        FinishArray(usize),
        FinishObject(Vec<String>),
    }

    let mut stack: Vec<Frame<'_>> = vec![Frame::Enter(root, 0)];
    let mut results: Vec<Value> = Vec::new();

    while let Some(frame) = stack.pop() {
        match frame {
            Frame::Enter(v, depth) => match v {
                Value::String(s) => results.push(Value::String(escape_delimiters(s))),
                Value::Array(arr) => {
                    if depth >= max_depth {
                        results.push(Value::String(format!("…(array len={})", arr.len())));
                    } else {
                        stack.push(Frame::FinishArray(arr.len()));
                        for item in arr.iter().rev() {
                            stack.push(Frame::Enter(item, depth + 1));
                        }
                    }
                }
                Value::Object(map) => {
                    if depth >= max_depth {
                        results.push(Value::String(format!("…(object keys={})", map.len())));
                    } else {
                        let keys: Vec<String> = map.keys().map(|k| escape_delimiters(k)).collect();
                        stack.push(Frame::FinishObject(keys));
                        for val in map.values().rev() {
                            stack.push(Frame::Enter(val, depth + 1));
                        }
                    }
                }
                primitive => results.push(primitive.clone()),
            },
            Frame::FinishArray(n) => {
                let start = results.len().saturating_sub(n);
                let items: Vec<Value> = results.drain(start..).collect();
                results.push(Value::Array(items));
            }
            Frame::FinishObject(keys) => {
                let n = keys.len();
                let start = results.len().saturating_sub(n);
                let vals: Vec<Value> = results.drain(start..).collect();
                let mut map = Map::with_capacity(n);
                for (k, v) in keys.into_iter().zip(vals) {
                    map.insert(k, v);
                }
                results.push(Value::Object(map));
            }
        }
    }
    results.pop().unwrap_or(Value::Null)
}

fn cap_long_keys(v: &Value, max_key_chars: usize) -> Value {
    match v {
        Value::Array(arr) => Value::Array(
            arr.iter()
                .map(|x| cap_long_keys(x, max_key_chars))
                .collect(),
        ),
        Value::Object(map) => {
            let mut out = Map::with_capacity(map.len());
            for (k, val) in map {
                let short_k = if k.chars().count() > max_key_chars {
                    format!("{}…", clip_chars(k, max_key_chars))
                } else {
                    k.clone()
                };
                out.insert(short_k, cap_long_keys(val, max_key_chars));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// Render local FTS5 cache search hits (`search(scope="cache")` / `cache.search`).
pub fn render_cache_hits(hits: &[CacheHit], opts: &RenderOpts) -> String {
    if opts.max_tokens == 0 {
        return String::new();
    }
    let query = opts
        .query
        .as_deref()
        .map(sanitize_inline)
        .unwrap_or_default();
    if query.is_empty() && hits.is_empty() {
        return cap_to_tokens(
            "_Empty query. Pass FTS5 terms, e.g. `asyncio` or `\"exact phrase\"`._\n",
            opts.max_tokens,
        );
    }
    if hits.is_empty() {
        let msg = if let Some(hint) = invalid_fts_hint(&query) {
            format!("_No results: your search syntax looks invalid. {hint}_\n")
        } else {
            format!(
                "_No cached pages match `{query}`. Use `web.fetch` or `web.research` to populate the cache._\n"
            )
        };
        return cap_to_tokens(&msg, opts.max_tokens);
    }

    let plural = if hits.len() == 1 { "" } else { "s" };
    let header = if query.is_empty() {
        format!("# Cache hits ({} result{plural})\n\n", hits.len())
    } else {
        format!(
            "# Cache hits for `{query}` ({} result{plural})\n\n",
            hits.len()
        )
    };
    let footer = "\nnext: `cache.getPage(url)` to read a full cached page.\n";
    let items: Vec<String> = hits
        .iter()
        .enumerate()
        .map(|(i, h)| format_cache_hit(i + 1, h))
        .collect();

    let out = pack_list_sections(&header, &items, footer, "results", opts.max_tokens);
    cap_to_tokens(&out, opts.max_tokens)
}

fn format_cache_hit(index: usize, h: &CacheHit) -> String {
    let title = non_empty_or(sanitize_inline(&h.title), "(untitled)");
    let url = sanitize_url(&h.url);
    let mut lines = vec![format!("## {index}. {title}")];
    if !url.is_empty() {
        lines.push(format!("<{url}>"));
    }
    let mut meta = Vec::new();
    if let Some(site) = h
        .sitename
        .as_deref()
        .map(sanitize_inline)
        .filter(|s| !s.is_empty())
    {
        meta.push(site);
    }
    if let Some(author) = h
        .author
        .as_deref()
        .map(sanitize_inline)
        .filter(|s| !s.is_empty())
    {
        meta.push(format!("by {author}"));
    }
    if let Some(date) = h
        .date
        .as_deref()
        .map(sanitize_inline)
        .filter(|s| !s.is_empty())
    {
        meta.push(date);
    }
    if !meta.is_empty() {
        lines.push(format!("_{}_", meta.join(" · ")));
    }
    let snippet = sanitize_block(&h.snippet);
    if !snippet.is_empty() {
        let (cut, _) = smart_truncate(&snippet, MAX_HIT_SNIPPET_CHARS);
        lines.push(String::new());
        lines.push(blockquote(&cut));
    }
    lines.join("\n") + "\n\n"
}

// ===========================================================================
// 1. SearchResponse
// ===========================================================================

fn render_search_response(resp: &SearchResponse, opts: &RenderOpts) -> String {
    let query_raw = if resp.query.trim().is_empty() {
        opts.query.as_deref().unwrap_or("")
    } else {
        resp.query.as_str()
    };
    let query = sanitize_inline(query_raw);
    let scope = sanitize_inline(&resp.scope);

    // Special-case empty FTS5 cache response so malformed syntax gets its hint.
    if scope.eq_ignore_ascii_case("cache") && resp.results.is_empty() {
        return render_cache_hits(
            &[],
            &RenderOpts {
                max_tokens: opts.max_tokens,
                query: Some(query_raw.to_owned()),
                now: opts.now.clone(),
            },
        );
    }

    // Engines that actually contributed vs requested (`formatting.py` parity).
    let mut contributed: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for hit in &resp.results {
        if hit.sources.is_empty() {
            let s = sanitize_inline(&hit.source);
            if !s.is_empty() && seen.insert(s.clone()) {
                contributed.push(s);
            }
        } else {
            for s in &hit.sources {
                let clean = sanitize_inline(s);
                if !clean.is_empty() && seen.insert(clean.clone()) {
                    contributed.push(clean);
                }
            }
        }
    }
    let requested: Vec<String> = resp
        .engines
        .iter()
        .map(|e| sanitize_inline(e))
        .filter(|e| !e.is_empty())
        .collect();
    let header_engines = if contributed.is_empty() {
        &requested
    } else {
        &contributed
    };
    let engines_str = if header_engines.is_empty() {
        "none".to_owned()
    } else {
        header_engines.join(", ")
    };

    let n = resp.results.len();
    let elapsed = elapsed_crumb(resp.elapsed_ms);
    let mut headline = format!(
        "# {} — {n} result{} · {engines_str} · {elapsed}",
        non_empty_or(query.clone(), "(empty query)"),
        if n == 1 { "" } else { "s" }
    );
    if let Some(age) = resp.cache_age_s {
        headline.push_str(&format!(" · cached {} ago", age_phrase(age)));
    } else if resp.cached {
        headline.push_str(" · cached");
    }

    let mut header_lines = vec![headline];

    // Sub-header: scope, dated-ness ratio, retrieved timestamp, missing requested engines.
    let mut sub_parts = Vec::new();
    if !scope.is_empty() {
        sub_parts.push(format!("scope: {scope}"));
    }
    if n > 0 {
        let dated = resp
            .results
            .iter()
            .filter(|h| h.date.as_deref().is_some_and(|d| !d.trim().is_empty()))
            .count();
        sub_parts.push(format!("dated: {dated}/{n}"));
    }
    if let Some(now) = opts
        .now
        .as_deref()
        .map(sanitize_inline)
        .filter(|s| !s.is_empty())
    {
        sub_parts.push(format!("retrieved {now}"));
    }
    if !sub_parts.is_empty() {
        header_lines.push(format!("_{}_", sub_parts.join(" · ")));
    }

    if !contributed.is_empty() {
        let missing: Vec<&str> = requested
            .iter()
            .map(String::as_str)
            .filter(|e| !seen.contains(*e))
            .collect();
        if !missing.is_empty() {
            header_lines.push(format!(
                "_(requested but contributed nothing: {})_",
                missing.join(", ")
            ));
        }
    }

    if let Some(lead) = resp
        .lead
        .as_deref()
        .map(sanitize_block)
        .filter(|s| !s.is_empty())
    {
        let (cut, _) = smart_truncate(&lead, MAX_LEAD_CHARS);
        header_lines.push(String::new());
        header_lines.push(format!("> **Lead:** {}", cut.replace('\n', "\n> ")));
    }

    if let Some(agent) = &resp.agent {
        if let Some(ans) = agent
            .answer
            .as_deref()
            .map(sanitize_block)
            .filter(|s| !s.is_empty())
        {
            header_lines.push(String::new());
            header_lines.push("## Answer".to_owned());
            header_lines.push(String::new());
            header_lines.push(ans);
            let mut by = Vec::new();
            if let Some(b) = agent
                .backend
                .as_deref()
                .map(sanitize_inline)
                .filter(|s| !s.is_empty())
            {
                by.push(b);
            }
            if let Some(m) = agent
                .model
                .as_deref()
                .map(sanitize_inline)
                .filter(|s| !s.is_empty())
            {
                by.push(m);
            }
            if agent.elapsed_ms > 0 {
                by.push(elapsed_crumb(agent.elapsed_ms));
            }
            if !by.is_empty() {
                header_lines.push(format!("_answer agent: {}_", by.join(" · ")));
            }
        } else if let Some(err) = agent
            .error
            .as_deref()
            .map(sanitize_inline)
            .filter(|s| !s.is_empty())
        {
            header_lines.push(String::new());
            header_lines.push(format!("_Answer agent: {err}_"));
        }
    }

    header_lines.push(String::new());
    let header = header_lines.join("\n") + "\n";

    let mut footer_lines = Vec::new();
    if !resp.notes.is_empty() {
        let clean_notes: Vec<String> = resp
            .notes
            .iter()
            .map(|n| sanitize_inline(n))
            .filter(|n| !n.is_empty())
            .collect();
        if !clean_notes.is_empty() {
            footer_lines.push(format!("notes: {}", clean_notes.join(" · ")));
        }
    }
    let next_hint = if scope.eq_ignore_ascii_case("cache") {
        "next: `cache.getPage(url)` to read a cached page."
    } else if scope.eq_ignore_ascii_case("catalog") {
        "next: `catalog.describe(id)` or `source.call(\"<id>.search\", {...})`."
    } else {
        "next: `web.fetch(url)` to read full pages before citing exact numbers/dates, or `web.research(q)`."
    };
    footer_lines.push(next_hint.to_owned());
    let footer = format!("\n{}\n", footer_lines.join("\n"));

    if resp.results.is_empty() {
        let empty_body = "**No results.** Try a broader query, another scope, or `search({query, scope:\"catalog\"})` to find specialist sources.\n";
        return format!("{header}{empty_body}{footer}");
    }

    // Try full snippets (<= 280 chars), then progressively tighter snippets (120, 60)
    // to preserve title, url, date, and snippet across all hits before dropping tail hits.
    let render_with_snippet_cap = |snip_cap: usize| -> Vec<String> {
        resp.results
            .iter()
            .enumerate()
            .map(|(i, hit)| format_search_hit(i + 1, hit, snip_cap))
            .collect()
    };

    for snip_cap in [MAX_HIT_SNIPPET_CHARS, 120, 60] {
        let items = render_with_snippet_cap(snip_cap);
        let total = estimate_tokens(&header)
            + items.iter().map(|s| estimate_tokens(s)).sum::<usize>()
            + estimate_tokens(&footer);
        if total <= opts.max_tokens {
            let mut out = header;
            for item in items {
                out.push_str(&item);
            }
            out.push_str(footer.trim_start_matches('\n'));
            return out;
        }
    }

    let compact_items = render_with_snippet_cap(120);
    pack_list_sections(&header, &compact_items, &footer, "results", opts.max_tokens)
}

fn format_search_hit(index: usize, hit: &Hit, snippet_cap: usize) -> String {
    let title = non_empty_or(sanitize_inline_capped(&hit.title, 240), "(untitled)");
    let url = sanitize_url(&hit.url);
    let mut lines = vec![format!("## {index}. {title}")];
    if !url.is_empty() {
        lines.push(format!("<{url}>"));
    }

    let mut meta = Vec::new();
    let src_list: Vec<String> = if hit.sources.is_empty() {
        let s = sanitize_inline(&hit.source);
        if s.is_empty() { vec![] } else { vec![s] }
    } else {
        hit.sources
            .iter()
            .map(|s| sanitize_inline(s))
            .filter(|s| !s.is_empty())
            .collect()
    };
    if !src_list.is_empty() {
        meta.push(src_list.join(", "));
    }
    if hit.kind != HitKind::Web {
        meta.push(hit_kind_label(hit.kind).to_owned());
    }
    let dc = date_crumb(hit.date.as_deref(), hit.date_source.as_deref());
    if !dc.is_empty() {
        meta.push(dc);
    }
    if hit.score > 0.0 && hit.score.is_finite() {
        meta.push(format!("score {:.3}", hit.score));
    }
    if !meta.is_empty() {
        lines.push(format!("_{}_", meta.join(" · ")));
    }

    let snippet = sanitize_block(&hit.snippet);
    if !snippet.is_empty() && snippet_cap > 0 {
        let (cut, _) = smart_truncate(&snippet, snippet_cap);
        lines.push(blockquote(&cut));
    }
    lines.join("\n") + "\n\n"
}

fn hit_kind_label(kind: HitKind) -> &'static str {
    match kind {
        HitKind::Web => "web",
        HitKind::News => "news",
        HitKind::Academic => "academic",
        HitKind::Record => "record",
        HitKind::Vertical => "vertical",
        _ => "web",
    }
}

// ===========================================================================
// 2. PageResult & 3. PageList
// ===========================================================================

fn fetch_method_label(m: FetchMethod) -> &'static str {
    match m {
        FetchMethod::Http => "http",
        FetchMethod::Browser => "browser",
        FetchMethod::Cache => "cache",
        FetchMethod::Document => "document",
        FetchMethod::Asset => "asset",
        _ => "http",
    }
}

fn format_page_header(page: &PageResult, heading_prefix: &str) -> String {
    let title = non_empty_or(sanitize_inline(&page.title), "(untitled)");
    let url = sanitize_url(&page.url);

    let mut header = vec![format!("{heading_prefix} {title}")];
    if !url.is_empty() {
        header.push(format!("<{url}>"));
    }

    let mut byline = Vec::new();
    if let Some(site) = page
        .sitename
        .as_deref()
        .map(sanitize_inline)
        .filter(|s| !s.is_empty())
    {
        byline.push(site);
    }
    if let Some(author) = page
        .author
        .as_deref()
        .map(sanitize_inline)
        .filter(|s| !s.is_empty())
    {
        byline.push(format!("by {author}"));
    }
    if let Some(pub_date) = page
        .published_date
        .as_deref()
        .map(sanitize_inline)
        .filter(|s| !s.is_empty())
    {
        byline.push(format!("published {pub_date}"));
    } else if page
        .media_type
        .as_deref()
        .is_none_or(|m| m.starts_with("text/"))
    {
        byline.push("no publication date found".to_owned());
    }
    if !byline.is_empty() {
        header.push(format!("_{}_", byline.join(" · ")));
    }

    let mut meta = vec![format!("fetched via {}", fetch_method_label(page.method))];
    if page.tokens_estimated > 0 {
        meta.push(format!("~{} tokens", page.tokens_estimated));
    }
    if let Some(mt) = page
        .media_type
        .as_deref()
        .map(sanitize_inline)
        .filter(|s| !s.is_empty())
    {
        meta.push(mt);
    }
    if let Some(bytes) = page.bytes {
        meta.push(format!("{bytes} bytes"));
    }
    if let (Some(w), Some(h)) = (page.width, page.height) {
        meta.push(format!("{w}×{h}"));
    }
    if page.truncated {
        meta.push("truncated".to_owned());
    }
    let crumb = retrieved_crumb(
        (!page.retrieved_at.trim().is_empty()).then_some(page.retrieved_at.as_str()),
        page.cache_age_s,
    );
    if !crumb.is_empty() {
        meta.push(crumb);
    }
    header.push(format!("_{}_", meta.join(" · ")));
    header.join("\n")
}

fn render_page_result(page: &PageResult, opts: &RenderOpts) -> String {
    let header = format_page_header(page, "#");
    let body = sanitize_block(&page.content);
    let header_tokens = estimate_tokens(&header) + 2;
    let body_budget = opts.max_tokens.saturating_sub(header_tokens);
    if body.is_empty() {
        return format!("{header}\n");
    }
    if body_budget == 0 {
        return cap_to_tokens(&header, opts.max_tokens);
    }
    let fitted_body = cap_to_tokens(&body, body_budget);
    format!("{header}\n\n{fitted_body}\n")
}

fn render_page_list(pages: &[PageResult], opts: &RenderOpts) -> String {
    if pages.is_empty() {
        return "_No pages fetched._\n".to_owned();
    }
    if pages.len() == 1 {
        return render_page_result(&pages[0], opts);
    }
    let top_header = format!("# Fetched pages ({})\n\n", pages.len());
    let headers: Vec<String> = pages.iter().map(|p| format_page_header(p, "##")).collect();
    let bodies: Vec<String> = pages.iter().map(|p| sanitize_block(&p.content)).collect();

    // Check if everything fits unmodified.
    let full_sections: Vec<String> = headers
        .iter()
        .zip(&bodies)
        .map(|(h, b)| {
            if b.is_empty() {
                format!("{h}\n\n---\n\n")
            } else {
                format!("{h}\n\n{b}\n\n---\n\n")
            }
        })
        .collect();
    let full_total = estimate_tokens(&top_header)
        + full_sections
            .iter()
            .map(|s| estimate_tokens(s))
            .sum::<usize>();
    if full_total <= opts.max_tokens {
        let mut out = top_header;
        for s in full_sections {
            out.push_str(&s);
        }
        return out.trim_end_matches("\n---\n\n").to_owned() + "\n";
    }

    // Water-fill body budgets across all pages so short pages stay intact and every page
    // retains its title, URL, byline, and opening content.
    let headers_cost: usize = estimate_tokens(&top_header)
        + headers
            .iter()
            .map(|h| estimate_tokens(h) + 6)
            .sum::<usize>();
    if headers_cost + pages.len() * 16 <= opts.max_tokens {
        let avail = opts.max_tokens.saturating_sub(headers_cost);
        let body_costs: Vec<usize> = bodies.iter().map(|b| estimate_tokens(b)).collect();
        let caps = water_fill_budgets(&body_costs, avail);
        let mut out = top_header;
        for (idx, ((h, b), cap)) in headers.iter().zip(&bodies).zip(caps).enumerate() {
            if idx > 0 {
                out.push_str("\n---\n\n");
            }
            out.push_str(h);
            if !b.is_empty() && cap > 0 {
                out.push_str("\n\n");
                out.push_str(&cap_to_tokens(b, cap));
            }
            out.push('\n');
        }
        return out;
    }

    // Even headers alone exceed the budget: keep leading pages with small body slices.
    let compact: Vec<String> = headers
        .iter()
        .zip(&bodies)
        .map(|(h, b)| {
            if b.is_empty() {
                format!("{h}\n\n")
            } else {
                format!("{h}\n\n{}\n\n", cap_to_tokens(b, 60))
            }
        })
        .collect();
    pack_list_sections(&top_header, &compact, "", "pages", opts.max_tokens)
}

// ===========================================================================
// 4. DocSlice
// ===========================================================================

fn render_doc_slice(doc: &DocSlice, opts: &RenderOpts) -> String {
    let source = non_empty_or(sanitize_inline(&doc.source), "(unknown source)");
    let fmt = non_empty_or(sanitize_inline(&doc.format), "document");
    let title = doc
        .title
        .as_deref()
        .map(sanitize_inline)
        .filter(|s| !s.is_empty());

    let mut parts = vec![format!("{fmt}: {source}")];
    if let Some(pages) = doc.pages {
        parts.push(format!("{pages} page{}", if pages == 1 { "" } else { "s" }));
    }
    if doc.tokens_estimated > 0 {
        parts.push(format!("~{} tokens", doc.tokens_estimated));
    }
    let full_read = doc.start == 0 && doc.returned_chars == doc.total_chars && doc.total_chars > 0;
    if (doc.start > 0 || doc.returned_chars > 0) && !full_read {
        let end = doc.start.saturating_add(doc.returned_chars);
        if doc.total_chars > 0 {
            parts.push(format!(
                "slice [{}:{end}] of {} chars",
                doc.start, doc.total_chars
            ));
        } else {
            parts.push(format!("slice [{}:{end}]", doc.start));
        }
    }
    if doc.truncated {
        parts.push("truncated".to_owned());
    }

    let mut header = String::new();
    if let Some(t) = title {
        header.push_str(&format!("# {t}\n\n"));
    }
    header.push_str(&format!("_{}_", parts.join(" · ")));

    let footer = if doc.truncated {
        let next_start = doc.start.saturating_add(doc.returned_chars);
        format!(
            "\n\nnext: `doc.read(\"{source}\", {{start: {next_start}}})` or `doc.outline(\"{source}\")`."
        )
    } else {
        String::new()
    };

    let body = sanitize_block(&doc.content);
    let overhead = estimate_tokens(&header) + estimate_tokens(&footer) + 4;
    let body_budget = opts.max_tokens.saturating_sub(overhead);
    if body.is_empty() {
        return format!("{header}{footer}\n");
    }
    if body_budget == 0 {
        return cap_to_tokens(&header, opts.max_tokens);
    }
    let fitted = cap_to_tokens(&body, body_budget);
    format!("{header}\n\n{fitted}{footer}\n")
}

// ===========================================================================
// 5. FulltextResult
// ===========================================================================

fn render_fulltext(ft: &FulltextResult, opts: &RenderOpts) -> String {
    let id = non_empty_or(sanitize_inline(&ft.id), "(unknown work)");
    let mut lines = vec![format!("# Full text: {id}")];

    if !ft.available {
        let mut meta = vec!["unavailable".to_owned()];
        if let Some(why) = ft
            .why
            .as_deref()
            .map(sanitize_inline)
            .filter(|s| !s.is_empty())
        {
            meta.push(format!("reason: {why}"));
        }
        if let Some(oa) = ft
            .oa_status
            .as_deref()
            .map(sanitize_inline)
            .filter(|s| !s.is_empty())
        {
            meta.push(format!("OA status: {oa}"));
        }
        lines.push(format!("_{}_", meta.join(" · ")));
        if let Some(landing) = ft
            .landing_url
            .as_deref()
            .map(sanitize_url)
            .filter(|s| !s.is_empty())
        {
            lines.push(format!("Landing page: <{landing}>"));
        }
        for note in &ft.notes {
            let n = sanitize_inline(note);
            if !n.is_empty() {
                lines.push(format!("_note: {n}_"));
            }
        }
        lines.push(String::new());
        lines.push(
            "next: use `scholar.work(id)` for metadata/abstract or `scholar.graph(id)` for citations."
                .to_owned(),
        );
        return lines.join("\n") + "\n";
    }

    let mut meta = Vec::new();
    if let Some(fmt) = ft
        .format
        .as_deref()
        .map(sanitize_inline)
        .filter(|s| !s.is_empty())
    {
        meta.push(format!("format: {fmt}"));
    }
    if let Some(src) = ft
        .source
        .as_deref()
        .map(sanitize_inline)
        .filter(|s| !s.is_empty())
    {
        meta.push(format!("source: {src}"));
    }
    if let Some(lic) = ft
        .license
        .as_deref()
        .map(sanitize_inline)
        .filter(|s| !s.is_empty())
    {
        meta.push(format!("license: {lic}"));
    }
    if !meta.is_empty() {
        lines.push(format!("_{}_", meta.join(" · ")));
    }
    if let Some(u) = ft
        .url
        .as_deref()
        .map(sanitize_url)
        .filter(|s| !s.is_empty())
    {
        lines.push(format!("<{u}>"));
    }
    for note in &ft.notes {
        let n = sanitize_inline(note);
        if !n.is_empty() {
            lines.push(format!("_note: {n}_"));
        }
    }
    let header = lines.join("\n") + "\n\n";

    // Only prefer `ft.sections` when at least one section carries body text.
    // `scholar-fulltext` compacts `sections` to a headings-only outline and places
    // the budgeted article Markdown in `content` for large papers (I11).
    if let Some(sections) = ft
        .sections
        .as_ref()
        .filter(|s| s.iter().any(|sec| !sec.text.trim().is_empty()))
    {
        let section_blocks: Vec<String> = sections
            .iter()
            .map(|sec| {
                let heading = non_empty_or(sanitize_inline(&sec.heading), "(section)");
                let text = sanitize_block(&sec.text);
                format!("## {heading}\n\n{text}\n\n")
            })
            .collect();
        let total: usize = estimate_tokens(&header)
            + section_blocks
                .iter()
                .map(|s| estimate_tokens(s))
                .sum::<usize>();
        if total <= opts.max_tokens {
            let mut out = header;
            for b in section_blocks {
                out.push_str(&b);
            }
            return out.trim_end().to_owned() + "\n";
        }
        // Water-fill section bodies so every section heading stays visible when possible.
        let headings: Vec<String> = sections
            .iter()
            .map(|sec| {
                format!(
                    "## {}",
                    non_empty_or(sanitize_inline(&sec.heading), "(section)")
                )
            })
            .collect();
        let bodies: Vec<String> = sections
            .iter()
            .map(|sec| sanitize_block(&sec.text))
            .collect();
        let head_cost = estimate_tokens(&header)
            + headings
                .iter()
                .map(|h| estimate_tokens(h) + 3)
                .sum::<usize>();
        if head_cost + sections.len() * 12 <= opts.max_tokens {
            let avail = opts.max_tokens.saturating_sub(head_cost);
            let costs: Vec<usize> = bodies.iter().map(|b| estimate_tokens(b)).collect();
            let caps = water_fill_budgets(&costs, avail);
            let mut out = header;
            for ((h, b), cap) in headings.iter().zip(&bodies).zip(caps) {
                out.push_str(h);
                out.push_str("\n\n");
                if !b.is_empty() && cap > 0 {
                    out.push_str(&cap_to_tokens(b, cap));
                    out.push_str("\n\n");
                }
            }
            return out.trim_end().to_owned() + "\n";
        }
        return pack_list_sections(&header, &section_blocks, "", "sections", opts.max_tokens);
    }

    let body = ft
        .content
        .as_deref()
        .map(sanitize_block)
        .unwrap_or_default();
    if body.is_empty() {
        return header.trim_end().to_owned() + "\n";
    }
    let avail = opts.max_tokens.saturating_sub(estimate_tokens(&header) + 2);
    if avail == 0 {
        return cap_to_tokens(&header, opts.max_tokens);
    }
    format!("{header}{}\n", cap_to_tokens(&body, avail))
}

// ===========================================================================
// 6. WorkResult & 7. WorkList
// ===========================================================================

fn render_work_result(res: &WorkResult, opts: &RenderOpts) -> String {
    let q_raw = if res.meta.query.trim().is_empty() {
        opts.query.as_deref().unwrap_or("")
    } else {
        res.meta.query.as_str()
    };
    let query = sanitize_inline(q_raw);
    let sources: Vec<String> = res
        .meta
        .sources
        .iter()
        .map(|s| sanitize_inline(s))
        .filter(|s| !s.is_empty())
        .collect();
    let src_str = if sources.is_empty() {
        "scholar".to_owned()
    } else {
        sources.join(", ")
    };
    let n = res.works.len();
    let elapsed = elapsed_crumb(res.meta.elapsed_ms);

    let mut headline = format!(
        "# Academic: {} — {n} work{} · {src_str} · {elapsed}",
        non_empty_or(query, "(query)"),
        if n == 1 { "" } else { "s" }
    );
    if res.meta.cached {
        headline.push_str(" · cached");
    }

    let mut header_lines = vec![headline];
    if let Some(sub) = res
        .meta
        .subscope
        .as_deref()
        .map(sanitize_inline)
        .filter(|s| !s.is_empty())
    {
        header_lines.push(format!("_subscope: {sub}_"));
    }
    header_lines.push(String::new());
    let header = header_lines.join("\n") + "\n";

    let mut footer_lines = Vec::new();
    let notes: Vec<String> = res
        .meta
        .notes
        .iter()
        .map(|n| sanitize_inline(n))
        .filter(|n| !n.is_empty())
        .collect();
    if !notes.is_empty() {
        footer_lines.push(format!("notes: {}", notes.join(" · ")));
    }
    footer_lines.push(
        "next: `scholar.fulltext(id)` · `scholar.graph(id)` · `scholar.integrity(doi)` · `scholar.cite(id)`."
            .to_owned(),
    );
    let footer = format!("\n{}\n", footer_lines.join("\n"));

    if res.works.is_empty() {
        return format!(
            "{header}**No scholarly works found.** Try broader terms, another subscope, or `scholar.work(doiOrArxivId)`.\n{footer}"
        );
    }

    render_work_cards(&header, &res.works, &footer, opts.max_tokens)
}

fn render_work_list(works: &[ScholarlyWork], opts: &RenderOpts) -> String {
    if works.is_empty() {
        return "_No scholarly works._\n".to_owned();
    }
    let footer = "\nnext: `scholar.fulltext(id)` · `scholar.graph(id)` · `scholar.cite(id)`.\n";
    if works.len() == 1 {
        let f_trim = footer.trim_start_matches('\n');
        let card = format_work_card(None, &works[0], 1000);
        if estimate_tokens(&card) + estimate_tokens(f_trim) <= opts.max_tokens {
            return format!("{card}{f_trim}");
        }
        let compact = format_work_card(None, &works[0], 240);
        if estimate_tokens(&compact) + estimate_tokens(f_trim) <= opts.max_tokens {
            return format!("{compact}{f_trim}");
        }
        return format_work_card(None, &works[0], 0);
    }
    let header = format!("# Scholarly works ({})\n\n", works.len());
    render_work_cards(&header, works, footer, opts.max_tokens)
}

/// Render work cards while ALWAYS preserving core bibliographic fields
/// (title, authors, year, venue, ids) before dropping abstracts or tail works.
fn render_work_cards(
    header: &str,
    works: &[ScholarlyWork],
    footer: &str,
    max_tokens: usize,
) -> String {
    let any_abstract = works.iter().any(|w| {
        w.abstract_text
            .as_deref()
            .is_some_and(|a| !a.trim().is_empty())
    });
    for abs_cap in [320usize, 140, 0] {
        let cards: Vec<String> = works
            .iter()
            .enumerate()
            .map(|(i, w)| format_work_card(Some(i + 1), w, abs_cap))
            .collect();
        let abs_note = if abs_cap == 0 && any_abstract {
            "_[…truncated: abstracts omitted to fit budget]_\n"
        } else {
            ""
        };
        let total = estimate_tokens(header)
            + cards.iter().map(|c| estimate_tokens(c)).sum::<usize>()
            + estimate_tokens(abs_note)
            + estimate_tokens(footer);
        if total <= max_tokens {
            let mut out = String::from(header);
            for c in cards {
                out.push_str(&c);
            }
            out.push_str(abs_note);
            out.push_str(footer.trim_start_matches('\n'));
            return out;
        }
    }
    // Even without abstracts all works don't fit: keep core fields of as many leading works as fit.
    let cards: Vec<String> = works
        .iter()
        .enumerate()
        .map(|(i, w)| format_work_card(Some(i + 1), w, 0))
        .collect();
    pack_list_sections(header, &cards, footer, "works", max_tokens)
}

fn format_work_card(index: Option<usize>, w: &ScholarlyWork, abstract_cap: usize) -> String {
    let title = non_empty_or(sanitize_inline_capped(&w.title, 260), "(untitled)");
    let mut badges = Vec::new();
    if w.retracted {
        badges.push("⚠️ **RETRACTED**".to_owned());
    }
    if w.work_type.eq_ignore_ascii_case("preprint") {
        badges.push("[PREPRINT]".to_owned());
    }
    let badge_suffix = if badges.is_empty() {
        String::new()
    } else {
        format!(" {}", badges.join(" "))
    };

    let heading = match index {
        Some(i) => format!("## {i}. {title}{badge_suffix}"),
        None => format!("# {title}{badge_suffix}"),
    };
    let mut lines = vec![heading];

    let url = sanitize_url(&w.url);
    if !url.is_empty() {
        lines.push(format!("<{url}>"));
    }

    // Authors (always preserved)
    if !w.authors.is_empty() {
        let shown: Vec<String> = w
            .authors
            .iter()
            .take(5)
            .map(|a| sanitize_inline(&a.name))
            .filter(|n| !n.is_empty())
            .collect();
        if !shown.is_empty() {
            let mut auth_line = shown.join(", ");
            if w.authors.len() > 5 {
                auth_line.push_str(&format!(" et al. ({} authors)", w.authors.len()));
            }
            lines.push(format!("Authors: {auth_line}"));
        }
    }

    // Year, venue, type, citations, OA badge
    let mut meta = Vec::new();
    if let Some(y) = w.year {
        meta.push(y.to_string());
    } else if let Some(d) = w
        .published_date
        .as_deref()
        .map(sanitize_inline)
        .filter(|s| !s.is_empty())
    {
        meta.push(d);
    }
    if let Some(v) = w
        .venue
        .as_deref()
        .map(sanitize_inline)
        .filter(|s| !s.is_empty())
    {
        meta.push(v);
    }
    if !w.work_type.trim().is_empty() && !w.work_type.eq_ignore_ascii_case("preprint") {
        meta.push(sanitize_inline(&w.work_type));
    }
    if let Some(c) = w.cited_by {
        let src_tag = w
            .cited_by_source
            .as_deref()
            .map(sanitize_inline)
            .filter(|s| !s.is_empty())
            .map(|s| format!(" ({s})"))
            .unwrap_or_default();
        meta.push(format!("cited by {c}{src_tag}"));
    }
    if let Some(oa) = &w.oa {
        let status = non_empty_or(sanitize_inline(&oa.status), "open");
        let fmt = oa
            .structured_format
            .as_deref()
            .map(sanitize_inline)
            .filter(|s| !s.is_empty())
            .map(|f| format!(" [{f}]"))
            .unwrap_or_default();
        meta.push(format!("OA: {status}{fmt}"));
    }
    if !meta.is_empty() {
        lines.push(format!("_{}_", meta.join(" · ")));
    }

    // Persistent IDs (always preserved)
    let id_line = format_work_ids(&w.ids);
    if !id_line.is_empty() {
        lines.push(format!("IDs: {id_line}"));
    }

    if w.retracted
        && let Some(reason) = w
            .retraction_reason
            .as_deref()
            .map(sanitize_inline)
            .filter(|s| !s.is_empty())
    {
        lines.push(format!("> ⚠️ **Retraction:** {reason}"));
    }

    if abstract_cap > 0
        && let Some(abs) = w
            .abstract_text
            .as_deref()
            .map(sanitize_block)
            .filter(|s| !s.is_empty())
    {
        let (cut, _) = smart_truncate(&abs, abstract_cap);
        lines.push(blockquote(&cut));
    }

    if !w.sources.is_empty() {
        let srcs: Vec<String> = w
            .sources
            .iter()
            .map(|s| sanitize_inline(s))
            .filter(|s| !s.is_empty())
            .collect();
        if !srcs.is_empty() {
            lines.push(format!("_sources: {}_", srcs.join(", ")));
        }
    }

    lines.join("\n") + "\n\n"
}

fn format_work_ids(ids: &WorkIds) -> String {
    let mut parts = Vec::new();
    let pairs: [(&str, &Option<String>); 8] = [
        ("doi", &ids.doi),
        ("arXiv", &ids.arxiv),
        ("PMID", &ids.pmid),
        ("PMCID", &ids.pmcid),
        ("OpenAlex", &ids.openalex),
        ("S2", &ids.s2),
        ("DBLP", &ids.dblp),
        ("OpenAIRE", &ids.openaire),
    ];
    for (prefix, val) in pairs {
        if let Some(v) = val
            .as_deref()
            .map(sanitize_inline)
            .filter(|s| !s.is_empty())
        {
            parts.push(format!("{prefix}:{v}"));
        }
    }
    parts.join(" · ")
}

// ===========================================================================
// 8. ResearchBrief
// ===========================================================================

fn render_research_brief(brief: &ResearchBrief, opts: &RenderOpts) -> String {
    let q_raw = if brief.query.trim().is_empty() {
        brief.search.query.as_str()
    } else {
        brief.query.as_str()
    };
    let query = non_empty_or(sanitize_inline(q_raw), "(query)");
    let engines: Vec<String> = brief
        .search
        .engines
        .iter()
        .map(|e| sanitize_inline(e))
        .filter(|e| !e.is_empty())
        .collect();
    let engines_str = if engines.is_empty() {
        "none".to_owned()
    } else {
        engines.join(", ")
    };

    let mut head = vec![format!("# Research brief: {query}"), String::new()];
    let mut meta = vec![
        format!("engines: {engines_str}"),
        format!("sources: {}", brief.search.results.len()),
        format!("pages read: {}", brief.pages.len()),
    ];
    if brief.search.cached {
        meta.push(match brief.search.cache_age_s {
            Some(age) => format!("search cached {} ago", age_phrase(age)),
            None => "search cached".to_owned(),
        });
    }
    if brief.tokens_estimated > 0 {
        meta.push(format!("~{} tokens", brief.tokens_estimated));
    }
    head.push(format!("_{}_", meta.join(" · ")));
    head.push(String::new());

    // Sources section (`formatting.py::render_research` parity)
    head.push("## Sources".to_owned());
    let pages_by_url: HashMap<&str, &PageResult> =
        brief.pages.iter().map(|p| (p.url.as_str(), p)).collect();

    if brief.search.results.is_empty() {
        head.push("_(none: the search returned nothing to read)_".to_owned());
    } else {
        for (i, hit) in brief.search.results.iter().enumerate() {
            let title = non_empty_or(sanitize_inline(&hit.title), "(untitled)");
            let url = sanitize_url(&hit.url);
            head.push(format!("- [{}] **{title}** — <{url}>", i + 1));

            let page = pages_by_url.get(hit.url.as_str()).copied();
            let mut facts = Vec::new();
            if hit.kind != HitKind::Web {
                facts.push(hit_kind_label(hit.kind).to_owned());
            }
            if let Some(pd) = page
                .and_then(|p| p.published_date.as_deref())
                .map(sanitize_inline)
                .filter(|s| !s.is_empty())
            {
                facts.push(format!("published {pd}"));
            } else {
                let dc = date_crumb(hit.date.as_deref(), hit.date_source.as_deref());
                facts.push(if dc.is_empty() {
                    "undated".to_owned()
                } else {
                    dc
                });
            }
            if let Some(age) = page.and_then(|p| p.cache_age_s) {
                facts.push(format!("page cached {} ago", age_phrase(age)));
            }
            head.push(format!("  _{}_", facts.join(" · ")));

            let sn = sanitize_block(&hit.snippet);
            if !sn.is_empty() {
                let (cut, _) = smart_truncate(&sn, 180);
                head.push(format!("  > {}", cut.replace('\n', "\n  > ")));
            }
        }
    }

    if let Some(note) = brief
        .date_note
        .as_deref()
        .map(sanitize_inline)
        .filter(|s| !s.is_empty())
    {
        head.push(String::new());
        head.push(format!("⚠️ **Dates:** {note}"));
    }

    let search_notes: Vec<String> = brief
        .search
        .notes
        .iter()
        .map(|n| sanitize_inline(n))
        .filter(|n| !n.is_empty())
        .collect();
    if !search_notes.is_empty() {
        head.push(String::new());
        head.push(format!("notes: {}", search_notes.join(" · ")));
    }

    let sources_block = head.join("\n") + "\n";
    if brief.pages.is_empty() {
        return sources_block;
    }

    let docs_header = "\n## Documents\n\n";
    let base_cost = estimate_tokens(&sources_block) + estimate_tokens(docs_header);
    let remaining = opts.max_tokens.saturating_sub(base_cost);
    if remaining < 20 {
        return cap_to_tokens(&sources_block, opts.max_tokens);
    }

    let doc_headers: Vec<String> = brief
        .pages
        .iter()
        .map(|p| format_page_header(p, "###"))
        .collect();
    let doc_bodies: Vec<String> = brief
        .pages
        .iter()
        .map(|p| sanitize_block(&p.content))
        .collect();

    let headers_cost: usize = doc_headers.iter().map(|h| estimate_tokens(h) + 6).sum();
    let avail_for_bodies = remaining.saturating_sub(headers_cost);
    let body_costs: Vec<usize> = doc_bodies.iter().map(|b| estimate_tokens(b)).collect();
    let caps = water_fill_budgets(&body_costs, avail_for_bodies);

    let mut out = sources_block;
    out.push_str(docs_header);
    for (idx, ((h, b), cap)) in doc_headers.iter().zip(&doc_bodies).zip(caps).enumerate() {
        if idx > 0 {
            out.push_str("\n---\n\n");
        }
        out.push_str(h);
        if !b.is_empty() && cap > 0 {
            out.push_str("\n\n");
            out.push_str(&cap_to_tokens(b, cap));
        }
        out.push('\n');
    }
    out
}

// ===========================================================================
// 9. CitationGraph
// ===========================================================================

fn render_citation_graph(g: &CitationGraph, opts: &RenderOpts) -> String {
    let mut top = Vec::new();
    if g.paper.retracted {
        top.push("> ⚠️ **RETRACTED PAPER**: do not cite this as a standing result.".to_owned());
        top.push(String::new());
    }

    let title = non_empty_or(sanitize_inline(&g.paper.title), "(untitled)");
    top.push(format!("# {title}"));
    top.push(String::new());
    let url = sanitize_url(&g.paper.url);
    if !url.is_empty() {
        top.push(format!("<{url}>"));
    }

    if !g.paper.authors.is_empty() {
        let names: Vec<String> = g
            .paper
            .authors
            .iter()
            .take(5)
            .map(|a| sanitize_inline(&a.name))
            .filter(|n| !n.is_empty())
            .collect();
        if !names.is_empty() {
            let suffix = if g.paper.authors.len() > 5 {
                " et al."
            } else {
                ""
            };
            top.push(format!("Authors: {}{suffix}", names.join(", ")));
        }
    }

    let mut meta = Vec::new();
    if let Some(y) = g.paper.year {
        meta.push(y.to_string());
    }
    if let Some(v) = g
        .paper
        .venue
        .as_deref()
        .map(sanitize_inline)
        .filter(|s| !s.is_empty())
    {
        meta.push(v);
    }
    if let Some(c) = g.paper.cited_by {
        meta.push(format!("cited by {c}"));
    }
    let resolved = sanitize_inline(&g.resolved_as);
    if !resolved.is_empty() {
        meta.push(format!("resolved via {resolved}"));
    }
    if !meta.is_empty() {
        top.push(format!("_{}_", meta.join(" · ")));
    }
    let id_line = format_work_ids(&g.paper.ids);
    if !id_line.is_empty() {
        top.push(format!("IDs: {id_line}"));
    }

    for notice in &g.notices {
        top.push(String::new());
        top.push(format_graph_notice(notice));
    }

    let header = top.join("\n") + "\n";

    let notes_block = {
        let notes: Vec<String> = g
            .notes
            .iter()
            .map(|n| sanitize_inline(n))
            .filter(|n| !n.is_empty())
            .collect();
        if notes.is_empty() {
            String::new()
        } else {
            format!(
                "\n{}\n",
                notes
                    .iter()
                    .map(|n| format!("_{n}_"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        }
    };

    let ref_items: Vec<String> = g
        .references
        .iter()
        .enumerate()
        .map(|(i, w)| format_graph_node(i + 1, w))
        .collect();
    let cite_items: Vec<String> = g
        .citations
        .iter()
        .enumerate()
        .map(|(i, w)| format_graph_node(i + 1, w))
        .collect();

    // Fast path: everything fits.
    let mut full = header.clone();
    if !ref_items.is_empty() {
        full.push_str("\n## References (what it builds on)\n\n");
        full.push_str(&ref_items.join("\n"));
    }
    if !cite_items.is_empty() {
        full.push_str("\n## Cited by (what built on it)\n\n");
        full.push_str(&cite_items.join("\n"));
    }
    full.push_str(&notes_block);
    if estimate_tokens(&full) <= opts.max_tokens {
        return full;
    }

    // Budgeted path: always keep focal paper + notices, split remaining budget across refs & cites.
    let used = estimate_tokens(&header) + estimate_tokens(&notes_block) + 4;
    let remaining = opts.max_tokens.saturating_sub(used);
    if remaining < 16 {
        return cap_to_tokens(&header, opts.max_tokens);
    }
    let mut out = header;
    match (!ref_items.is_empty(), !cite_items.is_empty()) {
        (true, true) => {
            let half = remaining / 2;
            out.push_str(&pack_list_sections(
                "\n## References (what it builds on)\n\n",
                &ref_items,
                "",
                "references",
                half,
            ));
            out.push_str(&pack_list_sections(
                "\n## Cited by (what built on it)\n\n",
                &cite_items,
                "",
                "citations",
                remaining.saturating_sub(half),
            ));
        }
        (true, false) => {
            out.push_str(&pack_list_sections(
                "\n## References (what it builds on)\n\n",
                &ref_items,
                "",
                "references",
                remaining,
            ));
        }
        (false, true) => {
            out.push_str(&pack_list_sections(
                "\n## Cited by (what built on it)\n\n",
                &cite_items,
                "",
                "citations",
                remaining,
            ));
        }
        (false, false) => {}
    }
    out.push_str(&notes_block);
    out
}

fn format_graph_notice(n: &GraphNotice) -> String {
    let kind = non_empty_or(sanitize_inline(&n.kind).replace('_', " "), "Notice");
    let date = n
        .date
        .as_deref()
        .map(sanitize_inline)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "undated".to_owned());
    let target = n
        .url
        .as_deref()
        .map(sanitize_url)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            n.doi
                .as_deref()
                .map(sanitize_inline)
                .filter(|s| !s.is_empty())
                .map(|d| format!("https://doi.org/{d}"))
        });
    match target {
        Some(t) => format!("⚠️ **{kind}** ({date}) — <{t}>"),
        None => format!("⚠️ **{kind}** ({date})"),
    }
}

fn format_graph_node(index: usize, w: &ScholarlyWork) -> String {
    let title = non_empty_or(sanitize_inline(&w.title), "(untitled)");
    let flag = if w.retracted {
        " ⚠️ **RETRACTED**"
    } else {
        ""
    };
    let mut lines = vec![format!("{index}. **{title}**{flag}")];
    let url = sanitize_url(&w.url);
    if !url.is_empty() {
        lines.push(format!("   <{url}>"));
    }
    let mut bits = Vec::new();
    if let Some(first) = w
        .authors
        .first()
        .map(|a| sanitize_inline(&a.name))
        .filter(|s| !s.is_empty())
    {
        let auth = if w.authors.len() > 1 {
            format!("{first} et al.")
        } else {
            first
        };
        bits.push(auth);
    }
    if let Some(y) = w.year {
        bits.push(y.to_string());
    }
    if let Some(v) = w
        .venue
        .as_deref()
        .map(sanitize_inline)
        .filter(|s| !s.is_empty())
    {
        bits.push(v);
    }
    if let Some(c) = w.cited_by
        && c > 0
    {
        bits.push(format!("cited by {c}"));
    }
    let ids = format_work_ids(&w.ids);
    if !ids.is_empty() {
        bits.push(ids);
    }
    if !bits.is_empty() {
        lines.push(format!("   _{}_", bits.join(" · ")));
    }
    lines.push(String::new());
    lines.join("\n")
}

// ===========================================================================
// 10. CatalogList
// ===========================================================================

fn render_catalog_list(hits: &[CatalogHit], opts: &RenderOpts) -> String {
    if hits.is_empty() {
        return "_No catalog entries matched. Try broader capability terms or omit group/capability filters._\n"
            .to_owned();
    }
    let q = opts
        .query
        .as_deref()
        .map(sanitize_inline)
        .filter(|s| !s.is_empty());
    let header = match q {
        Some(q) => format!(
            "# Catalog: {q} — {} entr{}\n\n",
            hits.len(),
            if hits.len() == 1 { "y" } else { "ies" }
        ),
        None => format!(
            "# Catalog — {} entr{}\n\n",
            hits.len(),
            if hits.len() == 1 { "y" } else { "ies" }
        ),
    };
    let footer = "\nnext: `catalog.describe(\"<id>\")` for full parameters, or paste an example into `exec`.\n";
    let items: Vec<String> = hits
        .iter()
        .enumerate()
        .map(|(i, h)| format_catalog_hit(i + 1, h))
        .collect();
    pack_list_sections(&header, &items, footer, "entries", opts.max_tokens)
}

fn format_catalog_hit(index: usize, h: &CatalogHit) -> String {
    let id = non_empty_or(sanitize_inline(&h.id), "(unknown)");
    let kind = non_empty_or(sanitize_inline(&h.kind), "source");
    let summary = sanitize_inline(&h.summary);
    let auth = non_empty_or(sanitize_inline(&h.auth), "keyless");
    let health = non_empty_or(sanitize_inline(&h.health), "ok");
    let sig = sanitize_inline(&h.signature);
    let ex = sanitize_inline(&h.example);

    let mut lines = vec![format!("## {index}. `{id}` ({kind})")];
    if !summary.is_empty() {
        lines.push(summary);
    }
    lines.push(format!(
        "_auth: {auth} · tier {} · health: {health}_",
        h.tier
    ));
    if !sig.is_empty() {
        lines.push(format!("- sig: `{sig}`"));
    }
    if !ex.is_empty() {
        lines.push(format!("- example: `{ex}`"));
    }
    lines.join("\n") + "\n\n"
}

// ===========================================================================
// 11. InlineImage
// ===========================================================================

fn render_inline_image(img: &InlineImageMeta, _opts: &RenderOpts) -> String {
    let url = sanitize_url(&img.url);
    let mt = non_empty_or(sanitize_inline(&img.media_type), "application/octet-stream");
    let mut parts = vec![mt, format!("{} bytes", img.bytes)];
    if let (Some(w), Some(h)) = (img.width, img.height) {
        parts.push(format!("{w}×{h}"));
    }
    let sha = sanitize_inline(&img.sha256);
    if !sha.is_empty() {
        let short = clip_chars(&sha, 12);
        parts.push(format!("sha256:{short}"));
    }
    if url.is_empty() {
        format!("Image ({})\n", parts.join(" · "))
    } else {
        format!("Image: <{url}> ({})\n", parts.join(" · "))
    }
}

// ===========================================================================
// 12. AskResponse (ruling R5)
// ===========================================================================

fn render_ask_response(resp: &AskResponse, opts: &RenderOpts) -> String {
    let q = non_empty_or(sanitize_inline(&resp.question), "(empty question)");
    let mode = non_empty_or(sanitize_inline(&resp.mode), "fast");
    let elapsed = elapsed_crumb(resp.elapsed_ms);

    let mut meta = vec![format!("mode: {mode}")];
    if let Some(b) = resp
        .backend
        .as_deref()
        .map(sanitize_inline)
        .filter(|s| !s.is_empty())
    {
        if let Some(m) = resp
            .model
            .as_deref()
            .map(sanitize_inline)
            .filter(|s| !s.is_empty())
        {
            meta.push(format!("{b}/{m}"));
        } else {
            meta.push(b);
        }
    }
    if let Some(steps) = resp.steps
        && steps > 0
    {
        meta.push(format!("{steps} step{}", if steps == 1 { "" } else { "s" }));
    }
    meta.push(elapsed);
    let ret = retrieved_crumb(
        (!resp.retrieved_at.trim().is_empty()).then_some(resp.retrieved_at.as_str()),
        resp.cache_age_s,
    );
    if !ret.is_empty() {
        meta.push(ret);
    }
    if resp.partial {
        meta.push("partial (timeout)".to_owned());
    }

    let mut out = format!("# {q}\n_{}_\n\n", meta.join(" · "));

    if let Some(ans) = resp
        .answer
        .as_deref()
        .map(sanitize_block)
        .filter(|s| !s.is_empty())
    {
        out.push_str(&ans);
        out.push_str("\n\n");
        if !resp.sources.is_empty() {
            out.push_str("## Sources\n\n");
            for (i, s) in resp.sources.iter().enumerate() {
                let title = non_empty_or(sanitize_inline(&s.title), "(untitled)");
                let url = sanitize_url(&s.url);
                let date = s
                    .date
                    .as_deref()
                    .map(sanitize_inline)
                    .filter(|d| !d.is_empty())
                    .unwrap_or_else(|| "undated".to_owned());
                let mut tail = vec![date];
                if let Some(src) = s
                    .source
                    .as_deref()
                    .map(sanitize_inline)
                    .filter(|x| !x.is_empty())
                {
                    tail.push(src);
                }
                if let Some(age) = s.cache_age_s {
                    tail.push(format!("cached {} ago", age_phrase(age)));
                }
                if url.is_empty() {
                    out.push_str(&format!(
                        "{}. **{title}** · _{}_\n",
                        i + 1,
                        tail.join(" · ")
                    ));
                } else {
                    out.push_str(&format!(
                        "{}. **{title}** — <{url}> · _{}_\n",
                        i + 1,
                        tail.join(" · ")
                    ));
                }
            }
            out.push('\n');
        }
    } else {
        if let Some(err) = resp
            .agent_error
            .as_deref()
            .map(sanitize_inline)
            .filter(|s| !s.is_empty())
        {
            out.push_str(&format!(
                "_Answer agent failed ({err}); showing retrieved passages._\n\n"
            ));
        }

        if resp.passages.is_empty() {
            out.push_str(
                "_No matching passages found. Try `search` or `exec` for broader research._\n\n",
            );
        } else {
            let items: Vec<String> = resp
                .passages
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    let body = sanitize_block(&p.text);
                    let title = non_empty_or(sanitize_inline(&p.title), "(untitled)");
                    let url = sanitize_url(&p.url);
                    let date = p
                        .date
                        .as_deref()
                        .map(sanitize_inline)
                        .filter(|d| !d.is_empty())
                        .unwrap_or_else(|| "undated".to_owned());
                    let mut byline = vec![date];
                    if let Some(src) = p
                        .source
                        .as_deref()
                        .map(sanitize_inline)
                        .filter(|s| !s.is_empty())
                    {
                        byline.push(src);
                    }
                    if let Some(age) = p.cache_age_s {
                        byline.push(format!("cached {} ago", age_phrase(age)));
                    }
                    let src_line = if url.is_empty() {
                        format!("— **{title}** · _{}_", byline.join(" · "))
                    } else {
                        format!("— **{title}** (<{url}>) · _{}_", byline.join(" · "))
                    };
                    format!("{}. {}\n   {src_line}\n\n", i + 1, body)
                })
                .collect();

            let footer = build_ask_footer(resp);
            return pack_list_sections(&out, &items, &footer, "passages", opts.max_tokens);
        }
    }

    let footer = build_ask_footer(resp);
    if !footer.is_empty() {
        out.push_str(footer.trim_start_matches('\n'));
    }
    cap_to_tokens(&out, opts.max_tokens)
}

fn build_ask_footer(resp: &AskResponse) -> String {
    let mut lines = Vec::new();
    if let Some(dn) = resp
        .date_note
        .as_deref()
        .map(sanitize_inline)
        .filter(|s| !s.is_empty())
    {
        lines.push(format!("dates: {dn}"));
    }
    if !resp.notes.is_empty() {
        let clean: Vec<String> = resp
            .notes
            .iter()
            .map(|n| sanitize_inline(n))
            .filter(|s| !s.is_empty())
            .collect();
        if !clean.is_empty() {
            lines.push(format!("notes: {}", clean.join(" · ")));
        }
    }
    if lines.is_empty() {
        String::new()
    } else {
        format!("\n{}\n", lines.join("\n"))
    }
}

// ===========================================================================
// Shared helpers: list packing & water-filling
// ===========================================================================

/// Pack as many leading list items as fit in `max_tokens` alongside `header` and `footer`,
/// appending an explicit truncation notice when tail items are omitted.
fn pack_list_sections(
    header: &str,
    items: &[String],
    footer: &str,
    noun: &str,
    max_tokens: usize,
) -> String {
    let total_items = items.len();
    let header_cost = estimate_tokens(header);
    let footer_cost = estimate_tokens(footer);
    let all_items_cost: usize = items.iter().map(|s| estimate_tokens(s)).sum();

    if header_cost + all_items_cost + footer_cost <= max_tokens {
        let mut out = String::from(header);
        for item in items {
            out.push_str(item);
        }
        out.push_str(footer.trim_start_matches('\n'));
        return out;
    }

    let trunc_reserve = 20usize;
    let budget = max_tokens.saturating_sub(header_cost + footer_cost + trunc_reserve);

    let mut out = String::from(header);
    let mut used = 0usize;
    let mut kept = 0usize;
    for item in items {
        let cost = estimate_tokens(item);
        if kept > 0 && used + cost > budget {
            break;
        }
        if kept == 0 && cost > budget && budget > 0 {
            out.push_str(&cap_to_tokens(item, budget));
            out.push('\n');
            kept = 1;
            break;
        }
        out.push_str(item);
        used += cost;
        kept += 1;
    }

    if kept < total_items {
        out.push_str(&format!(
            "_[…truncated: showing {kept} of {total_items} {noun}]_\n"
        ));
    }
    out.push_str(footer.trim_start_matches('\n'));
    out
}

/// Water-filling token allocation: short items keep their full size, longer items share
/// the remaining budget equally.
fn water_fill_budgets(costs: &[usize], total_budget: usize) -> Vec<usize> {
    let n = costs.len();
    if n == 0 || total_budget == 0 {
        return vec![0; n];
    }
    let total_need: usize = costs.iter().sum();
    if total_need <= total_budget {
        return costs.to_vec();
    }

    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by_key(|&i| costs[i]);

    let mut alloc = vec![0usize; n];
    let mut remaining = total_budget;
    for (pos, &idx) in order.iter().enumerate() {
        let left = n - pos;
        let share = remaining / left;
        let give = costs[idx].min(share);
        alloc[idx] = give;
        remaining = remaining.saturating_sub(give);
    }
    alloc
}

fn blockquote(text: &str) -> String {
    text.lines()
        .map(|l| {
            if l.is_empty() {
                ">".to_owned()
            } else {
                format!("> {l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn non_empty_or(s: String, fallback: &str) -> String {
    if s.is_empty() { fallback.to_owned() } else { s }
}

// ===========================================================================
// 13. Untagged domain result renderers (IntegrityReport, scholar entities,
//     DocOutline, CompareItem[], DownloadResult, CatalogDetail, StructuredMeta,
//     CacheHit[]) — strict Guardrail-2 keyset matching (no silent field loss).
// ===========================================================================

fn keys_subset_of(obj: &Map<String, Value>, allowed: &[&str]) -> bool {
    obj.keys().all(|k| allowed.contains(&k.as_str()))
}

fn has_all_keys(obj: &Map<String, Value>, required: &[&str]) -> bool {
    required.iter().all(|k| obj.contains_key(*k))
}

fn has_any_key(obj: &Map<String, Value>, any_of: &[&str]) -> bool {
    any_of.iter().any(|k| obj.contains_key(*k))
}

/// Strictly detect and render untagged domain result structs from `exec` as
/// budget-bounded, delimiter-safe Markdown. Returns `None` if `value` does not
/// match a known domain type or contains any extra user-added fields (Guardrail 2),
/// so callers fall back cleanly to JSON without losing data.
pub fn render_untagged(value: &Value, opts: &RenderOpts) -> Option<String> {
    if opts.max_tokens == 0 {
        return Some(String::new());
    }
    match value {
        Value::Object(obj) => {
            if is_integrity_report_shape(obj)
                && let Ok(rep) = serde_json::from_value::<IntegrityReport>(value.clone())
            {
                return Some(cap_to_tokens(
                    &render_integrity_report(&rep, opts),
                    opts.max_tokens,
                ));
            }
            if is_author_profile_shape(obj)
                && let Ok(p) = serde_json::from_value::<AuthorProfile>(value.clone())
            {
                return Some(cap_to_tokens(
                    &render_author_profile(&p, opts),
                    opts.max_tokens,
                ));
            }
            if is_venue_profile_shape(obj)
                && let Ok(p) = serde_json::from_value::<VenueProfile>(value.clone())
            {
                return Some(cap_to_tokens(
                    &render_venue_profile(&p, opts),
                    opts.max_tokens,
                ));
            }
            if is_org_profile_shape(obj)
                && let Ok(p) = serde_json::from_value::<OrgProfile>(value.clone())
            {
                return Some(cap_to_tokens(
                    &render_org_profile(&p, opts),
                    opts.max_tokens,
                ));
            }
            if is_doc_outline_shape(obj)
                && let Ok(o) = serde_json::from_value::<DocOutline>(value.clone())
            {
                return Some(cap_to_tokens(
                    &render_doc_outline(&o, opts),
                    opts.max_tokens,
                ));
            }
            if is_download_result_shape(obj)
                && let Ok(d) = serde_json::from_value::<DownloadResult>(value.clone())
            {
                return Some(cap_to_tokens(
                    &render_download_result(&d, opts),
                    opts.max_tokens,
                ));
            }
            if is_catalog_detail_shape(obj)
                && let Ok(c) = serde_json::from_value::<CatalogDetail>(value.clone())
            {
                return Some(cap_to_tokens(
                    &render_catalog_detail(&c, opts),
                    opts.max_tokens,
                ));
            }
            if is_structured_meta_shape(obj)
                && let Ok(m) = serde_json::from_value::<StructuredMeta>(value.clone())
            {
                return Some(cap_to_tokens(
                    &render_structured_meta(&m, opts),
                    opts.max_tokens,
                ));
            }
            None
        }
        Value::Array(arr) if !arr.is_empty() => {
            if arr
                .iter()
                .all(|v| v.as_object().is_some_and(is_grant_record_shape))
                && let Ok(grants) = serde_json::from_value::<Vec<GrantRecord>>(value.clone())
            {
                return Some(cap_to_tokens(
                    &render_grant_records(&grants, opts),
                    opts.max_tokens,
                ));
            }
            if arr
                .iter()
                .all(|v| v.as_object().is_some_and(is_compare_item_shape))
                && let Ok(items) = serde_json::from_value::<Vec<CompareItem>>(value.clone())
            {
                return Some(cap_to_tokens(
                    &render_compare_items(&items, opts),
                    opts.max_tokens,
                ));
            }
            if arr
                .iter()
                .all(|v| v.as_object().is_some_and(is_cache_hit_shape))
                && let Ok(hits) = serde_json::from_value::<Vec<CacheHit>>(value.clone())
            {
                return Some(render_cache_hits(&hits, opts));
            }
            None
        }
        _ => None,
    }
}

fn is_integrity_report_shape(obj: &Map<String, Value>) -> bool {
    const ALLOWED: &[&str] = &[
        "doi",
        "isRetracted",
        "retractionNotices",
        "scite",
        "doajVetted",
        "openalexFwci",
    ];
    const NOTICE_KEYS: &[&str] = &["nature", "date", "reason", "noticeDoi", "source"];
    const SCITE_KEYS: &[&str] = &["supporting", "contradicting", "mentioning"];
    if !keys_subset_of(obj, ALLOWED) || !has_all_keys(obj, &["isRetracted", "retractionNotices"]) {
        return false;
    }
    if !obj.get("isRetracted").is_some_and(Value::is_boolean) {
        return false;
    }
    let Some(notices) = obj.get("retractionNotices").and_then(Value::as_array) else {
        return false;
    };
    if !notices.iter().all(|n| {
        n.as_object()
            .is_some_and(|m| keys_subset_of(m, NOTICE_KEYS))
    }) {
        return false;
    }
    if let Some(scite) = obj.get("scite")
        && !scite.is_null()
        && !scite
            .as_object()
            .is_some_and(|m| keys_subset_of(m, SCITE_KEYS))
    {
        return false;
    }
    true
}

fn is_author_profile_shape(obj: &Map<String, Value>) -> bool {
    const ALLOWED: &[&str] = &[
        "name",
        "orcid",
        "openalexId",
        "affiliations",
        "worksCount",
        "citedByCount",
        "hIndex",
        "topWorks",
        "sources",
    ];
    keys_subset_of(obj, ALLOWED)
        && has_all_keys(obj, &["name", "affiliations", "topWorks", "sources"])
        && obj.get("affiliations").is_some_and(Value::is_array)
        && obj.get("topWorks").is_some_and(Value::is_array)
}

fn is_org_profile_shape(obj: &Map<String, Value>) -> bool {
    const ALLOWED: &[&str] = &[
        "name",
        "ror",
        "openalexId",
        "country",
        "orgType",
        "homepage",
        "worksCount",
        "citedByCount",
        "sources",
    ];
    keys_subset_of(obj, ALLOWED)
        && has_all_keys(obj, &["name", "sources"])
        && has_any_key(
            obj,
            &[
                "ror",
                "openalexId",
                "country",
                "orgType",
                "homepage",
                "worksCount",
                "citedByCount",
            ],
        )
        && obj.get("sources").is_some_and(Value::is_array)
}

fn is_venue_profile_shape(obj: &Map<String, Value>) -> bool {
    const ALLOWED: &[&str] = &[
        "name",
        "issn",
        "publisher",
        "doajVetted",
        "icoreRank",
        "openalexId",
        "worksCount",
        "sources",
    ];
    keys_subset_of(obj, ALLOWED)
        && has_all_keys(obj, &["name", "issn", "sources"])
        && obj.get("issn").is_some_and(Value::is_array)
}

fn is_grant_record_shape(obj: &Map<String, Value>) -> bool {
    const ALLOWED: &[&str] = &[
        "id",
        "title",
        "funder",
        "pi",
        "org",
        "amount",
        "currency",
        "startDate",
        "endDate",
        "url",
        "source",
    ];
    keys_subset_of(obj, ALLOWED) && has_all_keys(obj, &["id", "title", "funder", "source"])
}

fn is_doc_outline_shape(obj: &Map<String, Value>) -> bool {
    const ALLOWED: &[&str] = &["source", "format", "title", "pages", "sections", "sheets"];
    const SEC_KEYS: &[&str] = &["title", "level", "startChar", "page"];
    const SHEET_KEYS: &[&str] = &["name", "rows", "cols"];
    if !keys_subset_of(obj, ALLOWED) || !has_all_keys(obj, &["source", "format", "sections"]) {
        return false;
    }
    let Some(secs) = obj.get("sections").and_then(Value::as_array) else {
        return false;
    };
    if !secs
        .iter()
        .all(|s| s.as_object().is_some_and(|m| keys_subset_of(m, SEC_KEYS)))
    {
        return false;
    }
    if let Some(sheets) = obj.get("sheets")
        && !sheets.is_null()
        && !sheets.as_array().is_some_and(|arr| {
            arr.iter()
                .all(|s| s.as_object().is_some_and(|m| keys_subset_of(m, SHEET_KEYS)))
        })
    {
        return false;
    }
    true
}

fn is_compare_item_shape(obj: &Map<String, Value>) -> bool {
    const ALLOWED: &[&str] = &[
        "url",
        "title",
        "publishedDate",
        "excerpt",
        "truncated",
        "error",
    ];
    keys_subset_of(obj, ALLOWED)
        && obj.get("url").is_some_and(Value::is_string)
        && has_any_key(obj, &["excerpt", "publishedDate", "truncated", "error"])
}

fn is_download_result_shape(obj: &Map<String, Value>) -> bool {
    const ALLOWED: &[&str] = &[
        "url",
        "savedPath",
        "mediaType",
        "bytes",
        "sha256",
        "expiresInHours",
        "width",
        "height",
    ];
    keys_subset_of(obj, ALLOWED)
        && has_all_keys(
            obj,
            &[
                "url",
                "savedPath",
                "mediaType",
                "bytes",
                "sha256",
                "expiresInHours",
            ],
        )
}

fn is_catalog_detail_shape(obj: &Map<String, Value>) -> bool {
    const ALLOWED: &[&str] = &[
        "id",
        "kind",
        "summary",
        "signature",
        "example",
        "auth",
        "tier",
        "health",
        "params",
        "scopes",
        "caps",
        "homeUrl",
    ];
    const PARAM_KEYS: &[&str] = &["name", "ty", "required", "default", "min", "max", "doc"];
    if !keys_subset_of(obj, ALLOWED)
        || !has_all_keys(
            obj,
            &[
                "id",
                "kind",
                "summary",
                "signature",
                "params",
                "scopes",
                "caps",
            ],
        )
    {
        return false;
    }
    obj.get("params")
        .and_then(Value::as_array)
        .is_some_and(|arr| {
            arr.iter()
                .all(|p| p.as_object().is_some_and(|m| keys_subset_of(m, PARAM_KEYS)))
        })
}

fn is_structured_meta_shape(obj: &Map<String, Value>) -> bool {
    const ALLOWED: &[&str] = &[
        "url",
        "jsonLd",
        "opengraph",
        "microdata",
        "metaFallback",
        "hint",
    ];
    keys_subset_of(obj, ALLOWED)
        && has_all_keys(
            obj,
            &["url", "jsonLd", "opengraph", "microdata", "metaFallback"],
        )
        && obj.get("jsonLd").is_some_and(Value::is_array)
        && obj.get("opengraph").is_some_and(Value::is_object)
}

fn is_cache_hit_shape(obj: &Map<String, Value>) -> bool {
    const ALLOWED: &[&str] = &["url", "title", "snippet", "author", "date", "sitename"];
    keys_subset_of(obj, ALLOWED)
        && obj.get("url").is_some_and(Value::is_string)
        && obj.get("title").is_some_and(Value::is_string)
        && obj.get("snippet").is_some_and(Value::is_string)
}

/// Render [`IntegrityReport`] (`scholar.integrity`) as clean, scannable Markdown.
pub fn render_integrity_report(rep: &IntegrityReport, opts: &RenderOpts) -> String {
    let doi_clean = rep
        .doi
        .as_deref()
        .map(sanitize_inline)
        .filter(|s| !s.is_empty());
    let mut lines = Vec::new();
    match &doi_clean {
        Some(doi) => lines.push(format!("# Integrity report — `{doi}`")),
        None => lines.push("# Integrity report".to_owned()),
    }
    lines.push(String::new());

    if rep.is_retracted {
        let n = rep.retraction_notices.len();
        let detail = if n == 0 {
            "retraction flagged".to_owned()
        } else {
            format!("{n} notice{}", if n == 1 { "" } else { "s" })
        };
        lines.push(format!(
            "> ⚠️ **RETRACTED** ({detail}): do not cite this paper as a standing result."
        ));
    } else if !rep.retraction_notices.is_empty() {
        let n = rep.retraction_notices.len();
        lines.push(format!(
            "> ⚠️ **Notices found** ({n} editorial/correction notice{}; not retracted).",
            if n == 1 { "" } else { "s" }
        ));
    } else {
        lines.push(
            "Status: **Not retracted** (no retraction or correction notices found).".to_owned(),
        );
    }

    let mut signals = Vec::new();
    if let Some(vetted) = rep.doaj_vetted {
        signals.push(format!(
            "DOAJ vetted: {}",
            if vetted { "yes" } else { "no" }
        ));
    }
    if let Some(fwci) = rep.openalex_fwci
        && fwci.is_finite()
    {
        signals.push(format!("OpenAlex FWCI: {fwci:.2}"));
    }
    if let Some(sc) = &rep.scite {
        signals.push(format!(
            "scite.ai: {} supporting · {} contradicting · {} mentioning",
            sc.supporting, sc.contradicting, sc.mentioning
        ));
    }
    if !signals.is_empty() {
        lines.push(String::new());
        for sig in signals {
            lines.push(format!("- {sig}"));
        }
    }

    if !rep.retraction_notices.is_empty() {
        lines.push(String::new());
        lines.push("## Notices".to_owned());
        lines.push(String::new());
        for (i, notice) in rep.retraction_notices.iter().enumerate() {
            lines.push(format_retraction_notice(i + 1, notice));
        }
    }

    lines.push(String::new());
    if let Some(doi) = &doi_clean {
        lines.push(format!(
            "next: `scholar.work({doi:?})` · `scholar.graph({doi:?})` · `scholar.fulltext({doi:?})`."
        ));
    } else {
        lines.push(
            "next: `scholar.work(id)` · `scholar.graph(id)` · `scholar.fulltext(id)`.".to_owned(),
        );
    }

    cap_to_tokens(&(lines.join("\n") + "\n"), opts.max_tokens)
}

fn format_retraction_notice(index: usize, n: &RetractionNotice) -> String {
    let nature = non_empty_or(sanitize_inline(&n.nature), "Notice");
    let source = non_empty_or(sanitize_inline(&n.source), "unknown");
    let date = n
        .date
        .as_deref()
        .map(sanitize_inline)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "undated".to_owned());
    let mut header = format!("{index}. **{nature}** · _{date} · source: {source}_");
    if let Some(ndoi) = n
        .notice_doi
        .as_deref()
        .map(sanitize_inline)
        .filter(|s| !s.is_empty())
    {
        header.push_str(&format!(" — <https://doi.org/{}>", sanitize_url(&ndoi)));
    }
    if let Some(reason) = n
        .reason
        .as_deref()
        .map(sanitize_inline)
        .filter(|s| !s.is_empty())
    {
        format!("{header}\n   Reason: {reason}")
    } else {
        header
    }
}

fn render_author_profile(p: &AuthorProfile, opts: &RenderOpts) -> String {
    let name = non_empty_or(sanitize_inline(&p.name), "(unknown author)");
    let mut lines = vec![format!("# {name}")];
    let mut ids = Vec::new();
    if let Some(orcid) = p
        .orcid
        .as_deref()
        .map(sanitize_inline)
        .filter(|s| !s.is_empty())
    {
        ids.push(format!("ORCID:{orcid}"));
    }
    if let Some(oa) = p
        .openalex_id
        .as_deref()
        .map(sanitize_inline)
        .filter(|s| !s.is_empty())
    {
        ids.push(format!("OpenAlex:{oa}"));
    }
    if !ids.is_empty() {
        lines.push(format!("IDs: {}", ids.join(" · ")));
    }
    let mut stats = Vec::new();
    if let Some(w) = p.works_count {
        stats.push(format!("{w} works"));
    }
    if let Some(c) = p.cited_by_count {
        stats.push(format!("{c} citations"));
    }
    if let Some(h) = p.h_index {
        stats.push(format!("h-index {h}"));
    }
    if !stats.is_empty() {
        lines.push(format!("_{}_", stats.join(" · ")));
    }
    if !p.affiliations.is_empty() {
        let affs: Vec<String> = p
            .affiliations
            .iter()
            .map(|a| sanitize_inline(a))
            .filter(|a| !a.is_empty())
            .collect();
        if !affs.is_empty() {
            lines.push(format!("Affiliations: {}", affs.join("; ")));
        }
    }
    if !p.sources.is_empty() {
        let srcs: Vec<String> = p
            .sources
            .iter()
            .map(|s| sanitize_inline(s))
            .filter(|s| !s.is_empty())
            .collect();
        if !srcs.is_empty() {
            lines.push(format!("_sources: {}_", srcs.join(", ")));
        }
    }
    let header = lines.join("\n") + "\n\n";
    if p.top_works.is_empty() {
        return header;
    }
    let items: Vec<String> = p
        .top_works
        .iter()
        .enumerate()
        .map(|(i, w)| format_graph_node(i + 1, w))
        .collect();
    pack_list_sections(
        &format!("{header}## Top works\n\n"),
        &items,
        "",
        "works",
        opts.max_tokens,
    )
}

fn render_org_profile(p: &OrgProfile, _opts: &RenderOpts) -> String {
    let name = non_empty_or(sanitize_inline(&p.name), "(unknown organization)");
    let mut lines = vec![format!("# {name}")];
    if let Some(hp) = p
        .homepage
        .as_deref()
        .map(sanitize_url)
        .filter(|s| !s.is_empty())
    {
        lines.push(format!("<{hp}>"));
    }
    let mut meta = Vec::new();
    if let Some(t) = p
        .org_type
        .as_deref()
        .map(sanitize_inline)
        .filter(|s| !s.is_empty())
    {
        meta.push(t);
    }
    if let Some(c) = p
        .country
        .as_deref()
        .map(sanitize_inline)
        .filter(|s| !s.is_empty())
    {
        meta.push(c);
    }
    if let Some(w) = p.works_count {
        meta.push(format!("{w} works"));
    }
    if let Some(c) = p.cited_by_count {
        meta.push(format!("{c} citations"));
    }
    if !meta.is_empty() {
        lines.push(format!("_{}_", meta.join(" · ")));
    }
    let mut ids = Vec::new();
    if let Some(ror) = p
        .ror
        .as_deref()
        .map(sanitize_inline)
        .filter(|s| !s.is_empty())
    {
        ids.push(format!("ROR:{ror}"));
    }
    if let Some(oa) = p
        .openalex_id
        .as_deref()
        .map(sanitize_inline)
        .filter(|s| !s.is_empty())
    {
        ids.push(format!("OpenAlex:{oa}"));
    }
    if !ids.is_empty() {
        lines.push(format!("IDs: {}", ids.join(" · ")));
    }
    if !p.sources.is_empty() {
        let srcs: Vec<String> = p
            .sources
            .iter()
            .map(|s| sanitize_inline(s))
            .filter(|s| !s.is_empty())
            .collect();
        if !srcs.is_empty() {
            lines.push(format!("_sources: {}_", srcs.join(", ")));
        }
    }
    lines.join("\n") + "\n"
}

fn render_venue_profile(p: &VenueProfile, _opts: &RenderOpts) -> String {
    let name = non_empty_or(sanitize_inline(&p.name), "(unknown venue)");
    let mut lines = vec![format!("# {name}")];
    let mut meta = Vec::new();
    if let Some(pub_) = p
        .publisher
        .as_deref()
        .map(sanitize_inline)
        .filter(|s| !s.is_empty())
    {
        meta.push(pub_);
    }
    if let Some(rank) = p
        .icore_rank
        .as_deref()
        .map(sanitize_inline)
        .filter(|s| !s.is_empty())
    {
        meta.push(format!("iCORE {rank}"));
    }
    if let Some(vetted) = p.doaj_vetted {
        meta.push(format!(
            "DOAJ vetted: {}",
            if vetted { "yes" } else { "no" }
        ));
    }
    if let Some(w) = p.works_count {
        meta.push(format!("{w} works"));
    }
    if !meta.is_empty() {
        lines.push(format!("_{}_", meta.join(" · ")));
    }
    let mut ids = Vec::new();
    if !p.issn.is_empty() {
        let issns: Vec<String> = p
            .issn
            .iter()
            .map(|s| sanitize_inline(s))
            .filter(|s| !s.is_empty())
            .collect();
        if !issns.is_empty() {
            ids.push(format!("ISSN: {}", issns.join(", ")));
        }
    }
    if let Some(oa) = p
        .openalex_id
        .as_deref()
        .map(sanitize_inline)
        .filter(|s| !s.is_empty())
    {
        ids.push(format!("OpenAlex:{oa}"));
    }
    if !ids.is_empty() {
        lines.push(format!("IDs: {}", ids.join(" · ")));
    }
    if !p.sources.is_empty() {
        let srcs: Vec<String> = p
            .sources
            .iter()
            .map(|s| sanitize_inline(s))
            .filter(|s| !s.is_empty())
            .collect();
        if !srcs.is_empty() {
            lines.push(format!("_sources: {}_", srcs.join(", ")));
        }
    }
    lines.join("\n") + "\n"
}

fn render_grant_records(grants: &[GrantRecord], opts: &RenderOpts) -> String {
    let header = format!(
        "# Grants ({} record{})\n\n",
        grants.len(),
        if grants.len() == 1 { "" } else { "s" }
    );
    let items: Vec<String> = grants
        .iter()
        .enumerate()
        .map(|(i, g)| {
            let title = non_empty_or(sanitize_inline(&g.title), "(untitled grant)");
            let funder = non_empty_or(sanitize_inline(&g.funder), "(unknown funder)");
            let id = sanitize_inline(&g.id);
            let mut lines = vec![format!("## {}. {title}", i + 1)];
            if let Some(url) = g.url.as_deref().map(sanitize_url).filter(|s| !s.is_empty()) {
                lines.push(format!("<{url}>"));
            }
            let mut meta = vec![funder];
            if !id.is_empty() {
                meta.push(format!("ID: {id}"));
            }
            if let Some(pi) =
                g.pi.as_deref()
                    .map(sanitize_inline)
                    .filter(|s| !s.is_empty())
            {
                meta.push(format!("PI: {pi}"));
            }
            if let Some(org) = g
                .org
                .as_deref()
                .map(sanitize_inline)
                .filter(|s| !s.is_empty())
            {
                meta.push(org);
            }
            if let Some(amt) = g.amount
                && amt.is_finite()
            {
                let cur = g
                    .currency
                    .as_deref()
                    .map(sanitize_inline)
                    .filter(|s| !s.is_empty())
                    .unwrap_or_default();
                if cur.is_empty() {
                    meta.push(format!("{amt:.0}"));
                } else {
                    meta.push(format!("{cur} {amt:.0}"));
                }
            }
            match (
                g.start_date
                    .as_deref()
                    .map(sanitize_inline)
                    .filter(|s| !s.is_empty()),
                g.end_date
                    .as_deref()
                    .map(sanitize_inline)
                    .filter(|s| !s.is_empty()),
            ) {
                (Some(s), Some(e)) => meta.push(format!("{s}..{e}")),
                (Some(s), None) => meta.push(format!("from {s}")),
                (None, Some(e)) => meta.push(format!("until {e}")),
                (None, None) => {}
            }
            let src = sanitize_inline(&g.source);
            if !src.is_empty() {
                meta.push(format!("source: {src}"));
            }
            lines.push(format!("_{}_", meta.join(" · ")));
            lines.join("\n") + "\n\n"
        })
        .collect();
    pack_list_sections(&header, &items, "", "grants", opts.max_tokens)
}

fn render_doc_outline(o: &DocOutline, opts: &RenderOpts) -> String {
    let title = o
        .title
        .as_deref()
        .map(sanitize_inline)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| non_empty_or(sanitize_inline(&o.source), "(document)"));
    let fmt = non_empty_or(sanitize_inline(&o.format), "doc");
    let mut meta = vec![fmt];
    if let Some(p) = o.pages {
        meta.push(format!("{p} page{}", if p == 1 { "" } else { "s" }));
    }
    meta.push(format!(
        "{} section{}",
        o.sections.len(),
        if o.sections.len() == 1 { "" } else { "s" }
    ));
    let mut header_lines = vec![
        format!("# Outline: {title}"),
        format!("_{}_", meta.join(" · ")),
    ];
    let src = sanitize_inline(&o.source);
    if !src.is_empty() && src != title {
        header_lines.push(format!("Source: `{src}`"));
    }
    if let Some(sheets) = &o.sheets
        && !sheets.is_empty()
    {
        header_lines.push(String::new());
        header_lines.push("## Sheets".to_owned());
        for s in sheets {
            header_lines.push(format!(
                "- **{}** ({} rows × {} cols)",
                sanitize_inline(&s.name),
                s.rows,
                s.cols
            ));
        }
    }
    header_lines.push(String::new());
    let header = header_lines.join("\n") + "\n";
    let footer = "\nnext: `doc.read(source, {start})` to read a specific section.\n";
    if o.sections.is_empty() {
        return format!("{header}_No headings or outline entries detected._{footer}");
    }
    let items: Vec<String> = o
        .sections
        .iter()
        .map(|s| {
            let indent = "  ".repeat(usize::from(s.level.saturating_sub(1).min(4)));
            let heading = non_empty_or(sanitize_inline(&s.title), "(section)");
            let loc = match s.page {
                Some(p) => format!("p.{p}, char {}", s.start_char),
                None => format!("char {}", s.start_char),
            };
            format!("{indent}- {heading} _({loc})_\n")
        })
        .collect();
    pack_list_sections(&header, &items, footer, "sections", opts.max_tokens)
}

fn render_compare_items(items: &[CompareItem], opts: &RenderOpts) -> String {
    let header = format!(
        "# Comparison ({} source{})\n\n",
        items.len(),
        if items.len() == 1 { "" } else { "s" }
    );
    let rendered: Vec<String> = items
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let title = c
                .title
                .as_deref()
                .map(sanitize_inline)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| non_empty_or(sanitize_url(&c.url), "(untitled)"));
            let url = sanitize_url(&c.url);
            let mut lines = vec![format!("## {}. {title}", i + 1)];
            if !url.is_empty() {
                lines.push(format!("<{url}>"));
            }
            if let Some(d) = c
                .published_date
                .as_deref()
                .map(sanitize_inline)
                .filter(|s| !s.is_empty())
            {
                lines.push(format!("_published {d}_"));
            }
            if let Some(err) = c
                .error
                .as_deref()
                .map(sanitize_inline)
                .filter(|s| !s.is_empty())
            {
                lines.push(format!("_error: {err}_"));
            } else if let Some(ex) = c
                .excerpt
                .as_deref()
                .map(sanitize_block)
                .filter(|s| !s.is_empty())
            {
                lines.push(blockquote(&ex));
            }
            lines.join("\n") + "\n\n"
        })
        .collect();
    pack_list_sections(&header, &rendered, "", "sources", opts.max_tokens)
}

fn render_download_result(d: &DownloadResult, _opts: &RenderOpts) -> String {
    let mut lines = vec!["# Downloaded file".to_owned(), String::new()];
    let url = sanitize_url(&d.url);
    if !url.is_empty() {
        lines.push(format!("- URL: <{url}>"));
    }
    lines.push(format!(
        "- Saved path: `{}`",
        sanitize_inline(&d.saved_path)
    ));
    let mut meta = vec![
        non_empty_or(sanitize_inline(&d.media_type), "application/octet-stream"),
        format!("{} bytes", d.bytes),
    ];
    if let (Some(w), Some(h)) = (d.width, d.height) {
        meta.push(format!("{w}×{h}"));
    }
    lines.push(format!("- Type & size: {}", meta.join(" · ")));
    if !d.sha256.trim().is_empty() {
        lines.push(format!("- SHA-256: `{}`", sanitize_inline(&d.sha256)));
    }
    lines.push(format!("- Expires in: {} h", d.expires_in_hours));
    lines.join("\n") + "\n"
}

fn render_catalog_detail(d: &CatalogDetail, opts: &RenderOpts) -> String {
    let id = non_empty_or(sanitize_inline(&d.hit.id), "(unknown)");
    let kind = non_empty_or(sanitize_inline(&d.hit.kind), "source");
    let mut lines = vec![format!("# `{id}` ({kind})")];
    let summary = sanitize_inline(&d.hit.summary);
    if !summary.is_empty() {
        lines.push(summary);
    }
    let auth = non_empty_or(sanitize_inline(&d.hit.auth), "keyless");
    let health = non_empty_or(sanitize_inline(&d.hit.health), "ok");
    lines.push(format!(
        "_auth: {auth} · tier {} · health: {health}_",
        d.hit.tier
    ));
    if let Some(hp) = d
        .home_url
        .as_deref()
        .map(sanitize_url)
        .filter(|s| !s.is_empty())
    {
        lines.push(format!("Home: <{hp}>"));
    }
    if !d.scopes.is_empty() {
        lines.push(format!(
            "Scopes: {}",
            d.scopes
                .iter()
                .map(|s| sanitize_inline(s))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !d.caps.is_empty() {
        lines.push(format!(
            "Capabilities: {}",
            d.caps
                .iter()
                .map(|s| sanitize_inline(s))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let sig = sanitize_inline(&d.hit.signature);
    if !sig.is_empty() {
        lines.push(format!("- sig: `{sig}`"));
    }
    let ex = sanitize_inline(&d.hit.example);
    if !ex.is_empty() {
        lines.push(format!("- example: `{ex}`"));
    }
    if !d.params.is_empty() {
        lines.push(String::new());
        lines.push("## Parameters".to_owned());
        for p in &d.params {
            let mut flags = vec![sanitize_inline(&p.ty)];
            if p.required {
                flags.push("required".to_owned());
            }
            if let Some(def) = &p.default {
                flags.push(format!("default={def}"));
            }
            if let (Some(mn), Some(mx)) = (p.min, p.max) {
                flags.push(format!("range={mn}..={mx}"));
            }
            let doc = sanitize_inline(&p.doc);
            lines.push(format!(
                "- `{}` ({}): {}",
                sanitize_inline(&p.name),
                flags.join(", "),
                doc
            ));
        }
    }
    cap_to_tokens(&(lines.join("\n") + "\n"), opts.max_tokens)
}

fn render_structured_meta(m: &StructuredMeta, opts: &RenderOpts) -> String {
    let url = sanitize_url(&m.url);
    let mut lines = vec!["# Structured metadata".to_owned()];
    if !url.is_empty() {
        lines.push(format!("<{url}>"));
    }
    if let Some(hint) = m
        .hint
        .as_deref()
        .map(sanitize_inline)
        .filter(|s| !s.is_empty())
    {
        lines.push(format!("_{hint}_"));
    }
    if !m.opengraph.is_empty() {
        lines.push(String::new());
        lines.push("## OpenGraph".to_owned());
        for (k, v) in &m.opengraph {
            lines.push(format!(
                "- **{}**: {}",
                sanitize_inline(k),
                sanitize_inline(v)
            ));
        }
    }
    if !m.json_ld.is_empty() {
        lines.push(String::new());
        lines.push(format!(
            "## JSON-LD ({} block{})",
            m.json_ld.len(),
            if m.json_ld.len() == 1 { "" } else { "s" }
        ));
        for block in m.json_ld.iter().take(5) {
            let compact = render_json(block, &RenderOpts::default().with_max_tokens(300));
            lines.push(format!("```json\n{compact}\n```"));
        }
    }
    if !m.microdata.is_empty() {
        lines.push(String::new());
        lines.push(format!(
            "## Microdata ({} item{})",
            m.microdata.len(),
            if m.microdata.len() == 1 { "" } else { "s" }
        ));
        for item in m.microdata.iter().take(5) {
            let compact = render_json(item, &RenderOpts::default().with_max_tokens(200));
            lines.push(format!("- `{compact}`"));
        }
    }
    if !m.meta_fallback.is_empty() {
        lines.push(String::new());
        lines.push("## Meta tags".to_owned());
        for (k, v) in m.meta_fallback.iter().take(20) {
            lines.push(format!(
                "- **{}**: {}",
                sanitize_inline(k),
                sanitize_inline(v)
            ));
        }
    }
    cap_to_tokens(&(lines.join("\n") + "\n"), opts.max_tokens)
}
