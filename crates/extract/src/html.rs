//! HTML preprocessing, main-content extraction, and Markdown conversion.
//!
//! Pipeline (`SPEC.md` §7.3):
//! 1. Pre-DOM safety guards (B1 / I4):
//!    - Byte-size cap (`MAX_DOM_INPUT_BYTES` = 5 MiB) before DOM construction.
//!    - Iterative O(n) byte-level nesting depth scanner (`max_nesting_depth`) capping
//!      depth at `MAX_DOM_NESTING_DEPTH` (512) before any recursive parser (`html5ever`,
//!      `trafilatura`, `dom_smoothie`, `htmd`) runs; over-depth inputs degrade to a
//!      non-recursive flat-text extractor.
//!    - Dedicated 64 MiB-stack worker pool (`run_html_blocking`) with `catch_unwind`
//!      so CPU-heavy DOM jobs never run on Tokio's 2 MiB default blocking threads.
//! 2. Streaming pre-clean via `lol_html` (keeps `<script type="application/ld+json">`,
//!    strips other `<script>`, `<style>`, `<svg>`, `<canvas>`, `<iframe>`, `<template>`,
//!    and navigation boxes like `.navbox` / `.authority-control`).
//! 3. Primary extraction via `trafilatura 0.6` (`FavorPrecision`, `exclude_comments = false`,
//!    links + tables + fallback enabled, matching Python `fetcher.py::_extract`), plus
//!    multi-comment recovery when `trafilatura-rs`'s single-container comment query drops
//!    sibling comments in discussion threads.
//! 4. Fallback extraction via `dom_smoothie 0.20` (Mozilla Readability) → `dom_query`
//!    boilerplate strip → `htmd 0.6` ATX Markdown conversion.

use crate::structured::parse_json_ld_lenient;
use dom_query::Document as DomQueryDoc;
use dom_smoothie::Readability;
use fsm_core::FsmError;
use fsm_core::dates::normalize_date;
use fsm_core::url_canon::host_of;
use htmd::HtmlToMarkdown;
use htmd::options::{
    BulletListMarker, CodeBlockFence, CodeBlockStyle, HeadingStyle, LinkStyle,
    Options as HtmdOptions,
};
use lol_html::{RewriteStrSettings, element, rewrite_str};
use serde_json::Value;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use tokio::sync::oneshot;
use trafilatura::dom::Document as TrafDocument;
use trafilatura::{ExtractionFocus, Options as TrafOptions};
use url::Url;

/// Maximum DOM nesting depth allowed before bypassing recursive parsers (B1 / I4).
pub const MAX_DOM_NESTING_DEPTH: usize = 512;

/// Maximum HTML byte length handed to DOM parsers (I4), independent of the 25 MB transport cap.
pub const MAX_DOM_INPUT_BYTES: usize = 5 * 1024 * 1024;

/// Dedicated large-stack worker pool size and per-thread stack size (64 MiB, B1).
const HTML_POOL_THREADS: usize = 4;
const HTML_WORKER_STACK_BYTES: usize = 64 << 20;

/// Extracted page content and metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtractedPage {
    pub title: String,
    pub markdown: String,
    pub author: Option<String>,
    pub published_date: Option<String>,
    pub sitename: Option<String>,
    /// True when input exceeded `MAX_DOM_INPUT_BYTES` or `MAX_DOM_NESTING_DEPTH`.
    pub truncated: bool,
}

const BOILERPLATE_SELECTORS: [&str; 10] = [
    "script", "style", "noscript", "nav", "header", "footer", "form", "aside", "iframe", "svg",
];

type BoxedJob = Box<dyn FnOnce() + Send + 'static>;

fn html_worker_tx() -> &'static mpsc::Sender<BoxedJob> {
    static POOL: OnceLock<mpsc::Sender<BoxedJob>> = OnceLock::new();
    POOL.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<BoxedJob>();
        let rx = Arc::new(Mutex::new(rx));
        for i in 0..HTML_POOL_THREADS {
            let rx = Arc::clone(&rx);
            let _ = std::thread::Builder::new()
                .name(format!("fsm-html-{i}"))
                .stack_size(HTML_WORKER_STACK_BYTES)
                .spawn(move || {
                    loop {
                        let job = {
                            let Ok(guard) = rx.lock() else { break };
                            match guard.recv() {
                                Ok(j) => j,
                                Err(_) => break,
                            }
                        };
                        let _ = catch_unwind(AssertUnwindSafe(job));
                    }
                });
        }
        tx
    })
}

/// Run a CPU-bound HTML/DOM task on the dedicated 64 MiB-stack worker pool (B1).
pub async fn run_html_blocking<F, T>(task_name: &'static str, f: F) -> Result<T, FsmError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let (done_tx, done_rx) = oneshot::channel();
    let job: BoxedJob = Box::new(move || {
        let res = catch_unwind(AssertUnwindSafe(f));
        let _ = done_tx.send(res);
    });
    html_worker_tx()
        .send(job)
        .map_err(|_| FsmError::internal(task_name, "HTML worker pool unavailable"))?;
    match done_rx.await {
        Ok(Ok(val)) => Ok(val),
        Ok(Err(_panic)) => Err(FsmError::internal(
            task_name,
            "HTML parser panicked on input",
        )),
        Err(_) => Err(FsmError::internal(
            task_name,
            "HTML worker dropped before completing",
        )),
    }
}

/// Truncate `s` to at most `max_bytes` on a UTF-8 character boundary.
pub fn truncate_utf8_bytes(s: &str, max_bytes: usize) -> (&str, bool) {
    if s.len() <= max_bytes {
        return (s, false);
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (&s[..end], true)
}

fn is_void_or_optional_end_tag(tag: &str) -> bool {
    matches!(
        tag,
        // HTML void elements
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
            // Elements with optional end tags (B1)
            | "li"
            | "p"
            | "dt"
            | "dd"
            | "option"
            | "optgroup"
            | "tr"
            | "td"
            | "th"
            | "tbody"
            | "thead"
            | "tfoot"
            | "colgroup"
            | "rb"
            | "rt"
            | "rp"
    )
}

/// Iterative O(n) byte-level HTML nesting depth scanner (B1 / I4).
///
/// Ignores void elements, elements with optional end tags, comments, doctypes,
/// and raw text inside `<script>` / `<style>`. Stops early once depth exceeds
/// `MAX_DOM_NESTING_DEPTH + 1`? No — returns the true scanned depth so callers
/// and tests can inspect exact counts while staying strictly O(n) with zero recursion.
pub fn max_nesting_depth(html: &str) -> usize {
    let bytes = html.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    let mut depth = 0usize;
    let mut max_depth = 0usize;

    while i < len {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        // HTML comment: <!-- ... -->
        if i + 4 <= len && &bytes[i..i + 4] == b"<!--" {
            i += 4;
            while i + 3 <= len && &bytes[i..i + 3] != b"-->" {
                i += 1;
            }
            i = (i + 3).min(len);
            continue;
        }
        // Doctype / declaration / processing instruction: <! ... > or <? ... >
        if i + 1 < len && matches!(bytes[i + 1], b'!' | b'?') {
            i += 2;
            while i < len && bytes[i] != b'>' {
                i += 1;
            }
            if i < len {
                i += 1;
            }
            continue;
        }

        let is_close = i + 1 < len && bytes[i + 1] == b'/';
        let name_start = if is_close { i + 2 } else { i + 1 };
        if name_start >= len || !bytes[name_start].is_ascii_alphabetic() {
            i += 1;
            continue;
        }

        let mut name_end = name_start;
        while name_end < len
            && (bytes[name_end].is_ascii_alphanumeric()
                || matches!(bytes[name_end], b'-' | b'_' | b':'))
        {
            name_end += 1;
        }
        let tag_lower = html[name_start..name_end].to_ascii_lowercase();

        // Advance past tag attributes, respecting single/double quotes.
        let mut j = name_end;
        let mut quote: Option<u8> = None;
        let mut self_closing = false;
        while j < len {
            let b = bytes[j];
            if let Some(q) = quote {
                if b == q {
                    quote = None;
                }
            } else if b == b'"' || b == b'\'' {
                quote = Some(b);
            } else if b == b'>' {
                // Check if the last non-space byte before `>` was `/`.
                let mut k = j;
                while k > name_end && bytes[k - 1].is_ascii_whitespace() {
                    k -= 1;
                }
                if k > name_end && bytes[k - 1] == b'/' {
                    self_closing = true;
                }
                j += 1;
                break;
            }
            j += 1;
        }
        i = j;

        // Raw-text elements: skip until matching </script> or </style> without O(N²) lowercasing.
        if !is_close && !self_closing && matches!(tag_lower.as_str(), "script" | "style") {
            let close_pat: &[u8] = if tag_lower == "script" {
                b"</script"
            } else {
                b"</style"
            };
            if let Some(pos) = find_ascii_ci(&bytes[i..], close_pat) {
                i += pos + close_pat.len();
                while i < len && bytes[i] != b'>' {
                    i += 1;
                }
                if i < len {
                    i += 1;
                }
            } else {
                break;
            }
            continue;
        }

        if self_closing || is_void_or_optional_end_tag(&tag_lower) {
            continue;
        }

        if is_close {
            depth = depth.saturating_sub(1);
        } else {
            depth += 1;
            if depth > max_depth {
                max_depth = depth;
            }
        }
    }

    max_depth
}

fn find_ascii_ci(haystack: &[u8], needle_lower: &[u8]) -> Option<usize> {
    if needle_lower.is_empty() || haystack.len() < needle_lower.len() {
        return None;
    }
    haystack
        .windows(needle_lower.len())
        .position(|w| w.eq_ignore_ascii_case(needle_lower))
}

fn decode_basic_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.char_indices().peekable();
    while let Some((idx, ch)) = chars.next() {
        if ch == '&'
            && let Some(semi_rel) = s[idx + 1..].find(';')
            && semi_rel <= 12
        {
            let ent = &s[idx + 1..idx + 1 + semi_rel];
            let decoded = match ent {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" | "#39" | "#x27" => Some('\''),
                "nbsp" => Some(' '),
                _ if ent.starts_with("#x") || ent.starts_with("#X") => {
                    u32::from_str_radix(&ent[2..], 16)
                        .ok()
                        .and_then(char::from_u32)
                }
                _ if ent.starts_with('#') => ent[1..].parse::<u32>().ok().and_then(char::from_u32),
                _ => None,
            };
            if let Some(c) = decoded {
                out.push(c);
                let target_end = idx + 1 + semi_rel + 1;
                while chars.peek().is_some_and(|&(next_i, _)| next_i < target_end) {
                    chars.next();
                }
                continue;
            }
        }
        out.push(ch);
    }
    out
}

/// Non-recursive O(n) HTML tag stripper and text normalizer used when input exceeds
/// `MAX_DOM_NESTING_DEPTH` (B1 / I4).
pub fn strip_html_tags_flat(html: &str) -> String {
    let bytes = html.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    let mut out = String::with_capacity(html.len().min(64 * 1024));
    let mut title_skip = false;

    while i < len {
        if bytes[i] != b'<' {
            let start = i;
            while i < len && bytes[i] != b'<' {
                i += 1;
            }
            if !title_skip {
                out.push_str(&html[start..i]);
            }
            continue;
        }
        if i + 4 <= len && &bytes[i..i + 4] == b"<!--" {
            i += 4;
            while i + 3 <= len && &bytes[i..i + 3] != b"-->" {
                i += 1;
            }
            i = (i + 3).min(len);
            continue;
        }
        if i + 1 < len && matches!(bytes[i + 1], b'!' | b'?') {
            i += 2;
            while i < len && bytes[i] != b'>' {
                i += 1;
            }
            if i < len {
                i += 1;
            }
            continue;
        }

        let is_close = i + 1 < len && bytes[i + 1] == b'/';
        let name_start = if is_close { i + 2 } else { i + 1 };
        if name_start >= len || !bytes[name_start].is_ascii_alphabetic() {
            if !title_skip {
                out.push('<');
            }
            i += 1;
            continue;
        }
        let mut name_end = name_start;
        while name_end < len
            && (bytes[name_end].is_ascii_alphanumeric()
                || matches!(bytes[name_end], b'-' | b'_' | b':'))
        {
            name_end += 1;
        }
        let tag_lower = html[name_start..name_end].to_ascii_lowercase();

        let mut j = name_end;
        let mut quote: Option<u8> = None;
        while j < len {
            let b = bytes[j];
            if let Some(q) = quote {
                if b == q {
                    quote = None;
                }
            } else if b == b'"' || b == b'\'' {
                quote = Some(b);
            } else if b == b'>' {
                j += 1;
                break;
            }
            j += 1;
        }
        i = j;

        if !is_close && matches!(tag_lower.as_str(), "script" | "style" | "noscript" | "svg") {
            let close_pat: &[u8] = match tag_lower.as_str() {
                "script" => b"</script",
                "style" => b"</style",
                "noscript" => b"</noscript",
                _ => b"</svg",
            };
            if let Some(pos) = find_ascii_ci(&bytes[i..], close_pat) {
                i += pos + close_pat.len();
                while i < len && bytes[i] != b'>' {
                    i += 1;
                }
                if i < len {
                    i += 1;
                }
            } else {
                break;
            }
            continue;
        }
        if tag_lower == "title" {
            title_skip = !is_close;
            continue;
        }

        if matches!(
            tag_lower.as_str(),
            "p" | "div"
                | "br"
                | "li"
                | "h1"
                | "h2"
                | "h3"
                | "h4"
                | "h5"
                | "h6"
                | "tr"
                | "blockquote"
                | "pre"
                | "section"
                | "article"
        ) {
            out.push('\n');
        }
    }

    let decoded = decode_basic_entities(&out);
    normalize_markdown(&decoded)
}

fn extract_title_flat(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let Some(start) = lower.find("<title") else {
        return String::new();
    };
    let Some(gt_rel) = lower[start..].find('>') else {
        return String::new();
    };
    let body_start = start + gt_rel + 1;
    let Some(end_rel) = lower[body_start..].find("</title") else {
        return String::new();
    };
    let raw = &html[body_start..body_start + end_rel];
    decode_basic_entities(raw)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Streaming HTML cleanup using `lol_html`: removes non-JSON-LD scripts, styles,
/// SVGs, canvases, iframes, templates, and navigation/authority boxes before DOM parsing.
pub fn preprocess_html(html: &str) -> String {
    let settings = RewriteStrSettings::new()
        .append_element_content_handler(element!("script", |el| {
            let is_json_ld = el
                .get_attribute("type")
                .map(|t| {
                    t.split(';')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .eq_ignore_ascii_case("application/ld+json")
                })
                .unwrap_or(false);
            if !is_json_ld {
                el.remove();
            }
            Ok(())
        }))
        .append_element_content_handler(element!(
            "style, svg, canvas, iframe, template, .sr-only, .navbox, .authority-control, .catlinks, .mw-editsection",
            |el| {
                el.remove();
                Ok(())
            }
        ));

    rewrite_str(html, settings).unwrap_or_else(|_| html.to_owned())
}

/// Convert an HTML fragment or document into clean ATX Markdown via `htmd`.
///
/// Guards against deeply nested HTML (> `MAX_DOM_NESTING_DEPTH`) and oversized DOM inputs
/// (> `MAX_DOM_INPUT_BYTES`) with a non-recursive text fallback (B1 / I4).
pub fn html_to_markdown(html: &str) -> String {
    if html.trim().is_empty() {
        return String::new();
    }
    let (bounded, was_size_capped) = truncate_utf8_bytes(html, MAX_DOM_INPUT_BYTES);
    let depth = max_nesting_depth(bounded);
    if depth > MAX_DOM_NESTING_DEPTH {
        let mut flat = strip_html_tags_flat(bounded);
        if !flat.is_empty() {
            flat.push_str("\n\n");
        }
        flat.push_str(&format!(
            "[HTML nesting depth ({depth}) exceeded safe limit ({MAX_DOM_NESTING_DEPTH}); \
             extracted via non-recursive text fallback.]"
        ));
        return flat;
    }

    let opts = HtmdOptions {
        heading_style: HeadingStyle::Atx,
        bullet_list_marker: BulletListMarker::Dash,
        ul_bullet_spacing: 1,
        ol_number_spacing: 1,
        code_block_style: CodeBlockStyle::Fenced,
        code_block_fence: CodeBlockFence::Backticks,
        link_style: LinkStyle::Inlined,
        ..HtmdOptions::default()
    };
    let converter = HtmlToMarkdown::builder()
        .options(opts)
        .skip_tags(vec!["script", "style", "noscript", "svg", "iframe"])
        .build();

    let raw = converter.convert(bounded).unwrap_or_default();
    let mut md = normalize_markdown(&unescape_htmd_ampersands(&raw));
    if was_size_capped {
        md.push_str("\n\n[HTML input exceeded 5 MiB DOM parse cap and was truncated.]");
    }
    md
}

/// Remove `htmd`'s superfluous `\&` escapes outside fenced/inline code spans so prose
/// renders cleanly as `&` instead of raw `\&` artifacts.
fn unescape_htmd_ampersands(md: &str) -> String {
    if !md.contains(r"\&") {
        return md.to_owned();
    }
    let mut out = String::with_capacity(md.len());
    let mut in_fence = false;
    for (idx, line) in md.lines().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            out.push_str(line);
            continue;
        }
        if in_fence || !line.contains(r"\&") {
            out.push_str(line);
            continue;
        }
        let mut in_code = false;
        let bytes = line.as_bytes();
        let mut i = 0usize;
        while i < bytes.len() {
            if bytes[i] == b'`' {
                in_code = !in_code;
                out.push('`');
                i += 1;
            } else if !in_code && bytes[i] == b'\\' && i + 1 < bytes.len() && bytes[i + 1] == b'&' {
                out.push('&');
                i += 2;
            } else {
                let ch = line[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    out
}

/// Collapse runs of blank lines (at most one empty line between blocks) and
/// strip trailing spaces per line.
pub fn normalize_markdown(md: &str) -> String {
    let mut out = Vec::new();
    let mut blank = 0usize;
    for line in md.lines() {
        let trimmed_end = line.trim_end();
        if trimmed_end.trim().is_empty() {
            blank += 1;
            if blank <= 1 {
                out.push("");
            }
        } else {
            blank = 0;
            out.push(trimmed_end);
        }
    }
    out.join("\n").trim().to_owned()
}

fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

fn non_empty_opt(s: Option<String>) -> Option<String> {
    s.and_then(|v| non_empty(&v))
}

/// General structural filter for bogus DOM-scraped author candidates (B3):
/// rejects URLs, bracketed posting notices (`[Posted ...`), bare date/month strings,
/// and overly long navbox/table dumps (> 80 chars or > 8 tokens).
fn looks_like_bogus_author(s: &str) -> bool {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") || trimmed.starts_with('[') {
        return true;
    }
    if trimmed.chars().count() > 80 || trimmed.split_whitespace().count() > 8 {
        return true;
    }
    // Reject month-only or month+day date fragments (e.g. "Sept", "Sept 5", "March 29").
    let mut words = lower.split_whitespace();
    if let Some(first) = words.next() {
        let first_clean = first.trim_end_matches('.');
        let is_month = matches!(
            first_clean,
            "jan"
                | "january"
                | "feb"
                | "february"
                | "mar"
                | "march"
                | "apr"
                | "april"
                | "may"
                | "jun"
                | "june"
                | "jul"
                | "july"
                | "aug"
                | "august"
                | "sep"
                | "sept"
                | "september"
                | "oct"
                | "october"
                | "nov"
                | "november"
                | "dec"
                | "december"
        );
        if is_month {
            let rest: Vec<&str> = words.collect();
            if rest.is_empty()
                || rest
                    .iter()
                    .all(|w| w.trim_matches(',').chars().all(|c| c.is_ascii_digit()))
            {
                return true;
            }
        }
    }
    false
}

fn find_json_ld_value<'a>(node: &'a Value, key: &str) -> Option<&'a Value> {
    match node {
        Value::Array(arr) => arr.iter().find_map(|item| find_json_ld_value(item, key)),
        Value::Object(map) => {
            if let Some(v) = map.get(key)
                && !v.is_null()
            {
                return Some(v);
            }
            map.values().find_map(|v| find_json_ld_value(v, key))
        }
        _ => None,
    }
}

fn json_ld_author_name(val: &Value) -> Option<String> {
    match val {
        Value::String(s) => non_empty(s).filter(|v| !looks_like_bogus_author(v)),
        Value::Object(map) => map
            .get("name")
            .and_then(Value::as_str)
            .and_then(non_empty)
            .filter(|v| !looks_like_bogus_author(v)),
        Value::Array(arr) => {
            let names: Vec<String> = arr.iter().filter_map(json_ld_author_name).collect();
            (!names.is_empty()).then(|| names.join(", "))
        }
        _ => None,
    }
}

/// Parse `<script type="application/ld+json">` blocks using the shared lenient parser (I3).
fn parse_json_ld_blocks(doc: &DomQueryDoc) -> Vec<Value> {
    let mut blocks = Vec::new();
    for sel in doc.select("script[type]").iter() {
        let is_ld = sel
            .attr("type")
            .map(|t| {
                t.split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .eq_ignore_ascii_case("application/ld+json")
            })
            .unwrap_or(false);
        if !is_ld {
            continue;
        }
        if let Some(v) = parse_json_ld_lenient(&sel.text()) {
            blocks.push(v);
        }
    }
    blocks
}

/// Refine and supplement the 4 metadata fields from explicit `<meta>` / JSON-LD /
/// `<title>` / `<time>` tags.
fn refine_meta_from_dom(html: &str, url: &str, page: &mut ExtractedPage) {
    let doc = DomQueryDoc::from(html);
    let ld_blocks = parse_json_ld_blocks(&doc);
    let url_host = host_of(url);

    // 1. Title fallback
    if page.title.is_empty() {
        for sel in [
            r#"meta[property="og:title"]"#,
            r#"meta[name="og:title"]"#,
            r#"meta[name="twitter:title"]"#,
            "title",
            "h1",
        ] {
            let el = doc.select_single(sel);
            if el.exists() {
                let candidate = if sel.starts_with("meta") {
                    el.attr("content")
                        .map(|s| s.to_string())
                        .unwrap_or_default()
                } else {
                    el.text().to_string()
                };
                if let Some(t) = non_empty(&candidate) {
                    page.title = t;
                    break;
                }
            }
        }
    }

    // 2. Sitename: prefer explicit og:site_name / application-name / JSON-LD publisher
    //    when trafilatura returned nothing or fell back to the bare hostname.
    let sitename_is_host_fallback = page
        .sitename
        .as_deref()
        .is_none_or(|s| !url_host.is_empty() && s.eq_ignore_ascii_case(&url_host));
    if sitename_is_host_fallback {
        let mut explicit_site: Option<String> = None;
        for sel in [
            r#"meta[property="og:site_name"]"#,
            r#"meta[name="og:site_name"]"#,
            r#"meta[name="application-name"]"#,
            r#"meta[name="publisher"]"#,
        ] {
            let el = doc.select_single(sel);
            if el.exists()
                && let Some(v) = el.attr("content").and_then(|s| non_empty(&s))
            {
                explicit_site = Some(v);
                break;
            }
        }
        if explicit_site.is_none() {
            for b in &ld_blocks {
                if let Some(pub_val) = find_json_ld_value(b, "publisher")
                    && let Some(name) = pub_val
                        .get("name")
                        .and_then(Value::as_str)
                        .and_then(non_empty)
                {
                    explicit_site = Some(name);
                    break;
                }
            }
        }
        if let Some(site) = explicit_site {
            page.sitename = Some(site);
        }
    }

    // 3. Author: discard structurally bogus DOM matches and check explicit meta / JSON-LD.
    if page.author.as_deref().is_some_and(looks_like_bogus_author) {
        page.author = None;
    }
    if page.author.is_none() {
        for sel in [
            r#"meta[name="author"]"#,
            r#"meta[property="article:author"]"#,
            r#"meta[name="dc.creator"]"#,
            r#"meta[name="citation_author"]"#,
        ] {
            let el = doc.select_single(sel);
            if el.exists()
                && let Some(a) = el
                    .attr("content")
                    .and_then(|s| non_empty(&s))
                    .filter(|s| !looks_like_bogus_author(s))
            {
                page.author = Some(a);
                break;
            }
        }
    }
    if page.author.is_none() {
        for b in &ld_blocks {
            if let Some(val) = find_json_ld_value(b, "author")
                && let Some(name) = json_ld_author_name(val)
            {
                page.author = Some(name);
                break;
            }
        }
    }

    // 4. Published date: explicit article:published_time / JSON-LD datePublished
    //    outranks dateModified when present.
    let mut explicit_pub_date: Option<String> = None;
    for sel in [
        r#"meta[property="article:published_time"]"#,
        r#"meta[name="article:published_time"]"#,
        r#"meta[property="og:published_time"]"#,
        r#"meta[name="pubdate"]"#,
        r#"meta[name="publishdate"]"#,
        r#"meta[name="citation_publication_date"]"#,
    ] {
        let el = doc.select_single(sel);
        if el.exists()
            && let Some(raw) = el.attr("content")
            && let Some(norm) = normalize_date(&raw)
        {
            explicit_pub_date = Some(norm);
            break;
        }
    }
    if explicit_pub_date.is_none() {
        for b in &ld_blocks {
            if let Some(raw) = find_json_ld_value(b, "datePublished").and_then(Value::as_str)
                && let Some(norm) = normalize_date(raw)
            {
                explicit_pub_date = Some(norm);
                break;
            }
        }
    }
    if let Some(pub_d) = explicit_pub_date {
        page.published_date = Some(pub_d);
    } else if page.published_date.is_none() {
        for sel in [
            r#"meta[name="date"]"#,
            r#"meta[name="dc.date"]"#,
            "time[datetime]",
        ] {
            let el = doc.select_single(sel);
            if el.exists() {
                let raw = if sel.starts_with("meta") {
                    el.attr("content")
                } else {
                    el.attr("datetime")
                };
                if let Some(r) = raw
                    && let Some(norm) = normalize_date(&r)
                {
                    page.published_date = Some(norm);
                    break;
                }
            }
        }
    }
}

/// Coalesce ad-interleaved multi-part article bodies (e.g. modern CMS templates that split
/// an `<article>` into multiple `.post-content` / `.article-body` / `.entry-content` divs).
///
/// `trafilatura-rs` uses single-match `selector::query` on `content_rule1`, so split chunks
/// only capture the first chunk and trip fallback heuristics. Merging sibling chunks inside
/// the primary `<article>` lets `trafilatura-rs` see the full article body cleanly.
fn coalesce_multipart_article(html: &str) -> Option<String> {
    let doc = DomQueryDoc::from(html);
    let article = doc.select("article").first();
    if !article.exists() {
        return None;
    }
    for sel in [
        ".post-content",
        ".article-content",
        ".article-body",
        ".entry-content",
        ".post-body",
        ".story-body",
    ] {
        let chunks = article.select(sel);
        if chunks.length() < 2 {
            continue;
        }
        let mut combined = String::new();
        for chunk in chunks.iter() {
            combined.push_str(&chunk.inner_html());
            combined.push('\n');
        }
        if combined.trim().len() >= 500 {
            let head = doc.select_single("head").html();
            return Some(format!(
                "<!DOCTYPE html><html>{head}<body><article><div class=\"post-content\">{combined}</div></article></body></html>"
            ));
        }
    }
    None
}

/// Recover multi-post comment threads when `trafilatura-rs`'s single-subtree
/// `extract_comments` only captured the first comment container (B3, e.g. HN / forums).
fn extract_multi_comment_thread(html: &str) -> Option<String> {
    let doc = DomQueryDoc::from(html);
    // Only apply to discussion/forum layouts that have no <article> element
    // and contain >= 4 distinct comment text blocks.
    if doc.select("article").length() > 0 {
        return None;
    }
    for sel in [".commtext", ".comment-body", "div.comment"] {
        let nodes = doc.select(sel);
        if nodes.length() < 4 {
            continue;
        }
        let mut parts = Vec::new();
        for item in nodes.iter() {
            // Strip inline reply links inside comment blocks.
            item.select(".reply").remove();
            let md = html_to_markdown(&item.inner_html());
            if !md.trim().is_empty() {
                parts.push(md);
            }
        }
        let joined = parts.join("\n\n");
        if joined.chars().count() >= 500 {
            return Some(joined);
        }
    }
    None
}

/// Last-resort main-region extraction (port of Python `fetcher._extract_main_html`).
fn fallback_main_html(html: &str) -> (String, String) {
    let doc = DomQueryDoc::from(html);
    let title = {
        let t = doc.select_single("title");
        if t.exists() {
            t.text().trim().to_owned()
        } else {
            String::new()
        }
    };
    for tag in BOILERPLATE_SELECTORS {
        doc.select(tag).remove();
    }
    for sel in [
        "article",
        "main",
        "[role=\"main\"]",
        "#content",
        ".content",
        "body",
    ] {
        let node = doc.select_single(sel);
        if node.exists() {
            let inner = node.inner_html().to_string();
            if !inner.trim().is_empty() {
                return (title, inner);
            }
        }
    }
    (title, html.to_owned())
}

/// Extract main readable content and 4 metadata fields (`title`, `author`,
/// `published_date`, `sitename`) from raw HTML.
pub fn extract_html(html: &str, url: &str) -> ExtractedPage {
    let mut out = ExtractedPage::default();
    if html.trim().is_empty() {
        return out;
    }

    // Pre-DOM safety guards (B1 / I4): cap input bytes and check nesting depth BEFORE html5ever.
    let (bounded, was_size_capped) = truncate_utf8_bytes(html, MAX_DOM_INPUT_BYTES);
    out.truncated = was_size_capped;
    let depth = max_nesting_depth(bounded);
    if depth > MAX_DOM_NESTING_DEPTH {
        out.title = extract_title_flat(bounded);
        let mut flat = strip_html_tags_flat(bounded);
        if !flat.is_empty() {
            flat.push_str("\n\n");
        }
        flat.push_str(&format!(
            "[HTML nesting depth ({depth}) exceeded safe limit ({MAX_DOM_NESTING_DEPTH}); \
             extracted via non-recursive text fallback.]"
        ));
        out.markdown = flat;
        out.truncated = true;
        return out;
    }

    let cleaned = preprocess_html(bounded);
    let parsed_url = Url::parse(url)
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https"));

    // Mirror Python fetcher.py::_extract: FavorPrecision + links + tables + comments + fallback.
    let mut opts = TrafOptions::default()
        .with_links(true)
        .with_fallback(true)
        .with_focus(ExtractionFocus::FavorPrecision);
    opts.exclude_tables = false;
    opts.exclude_comments = false;
    if let Some(u) = parsed_url.clone() {
        opts = opts.with_url(u);
    }

    // Parse once for trafilatura; coalesce multi-part ad-interleaved article chunks if present.
    let traf_input = coalesce_multipart_article(&cleaned);
    let traf_html = traf_input.as_deref().unwrap_or(&cleaned);
    let doc = TrafDocument::parse(traf_html);
    let fallback_meta = trafilatura::metadata::extract_metadata(&doc, &opts);

    match trafilatura::extract_document(doc, &opts) {
        Ok(res) => {
            out.title = non_empty(&res.metadata.title).unwrap_or_default();
            out.author = non_empty(&res.metadata.author);
            out.sitename = non_empty(&res.metadata.sitename);
            out.published_date = res.metadata.date.map(|d| d.to_string());

            let main_md = if !res.content_html.trim().is_empty() {
                html_to_markdown(&res.content_html)
            } else {
                normalize_markdown(&res.content_text)
            };
            let comments_md = if !res.comments_html.trim().is_empty() {
                html_to_markdown(&res.comments_html)
            } else {
                normalize_markdown(&res.comments_text)
            };
            out.markdown = match (main_md.is_empty(), comments_md.is_empty()) {
                (false, true) => main_md,
                (true, false) => comments_md,
                (false, false) => format!("{main_md}\n\n{comments_md}"),
                (true, true) => String::new(),
            };
        }
        Err(_) => {
            out.title = non_empty(&fallback_meta.title).unwrap_or_default();
            out.author = non_empty(&fallback_meta.author);
            out.sitename = non_empty(&fallback_meta.sitename);
            out.published_date = fallback_meta.date.map(|d| d.to_string());
        }
    }

    // Multi-comment discussion rescue (B3): trafilatura-rs's extract_comments only queries
    // the first matching comment container; if a non-article discussion thread has far more
    // comment content than trafilatura returned, extract all comment blocks cleanly.
    if let Some(thread_md) = extract_multi_comment_thread(&cleaned)
        && thread_md.chars().count() > out.markdown.chars().count().saturating_mul(2)
    {
        out.markdown = thread_md;
    }

    // Fallback 1: dom_smoothie (Mozilla Readability) when trafilatura returned empty
    // or very little compared to the page size.
    let cur_chars = out.markdown.chars().count();
    let needs_smoothie = cur_chars == 0 || (cur_chars < 250 && cleaned.len() > 2_000);
    if needs_smoothie {
        let doc_url = parsed_url.as_ref().map(Url::as_str);
        if let Ok(mut reader) = Readability::new(cleaned.as_str(), doc_url, None)
            && let Ok(article) = reader.parse()
        {
            let smoothie_md = html_to_markdown(&article.content);
            if smoothie_md.chars().count() > cur_chars {
                out.markdown = smoothie_md;
            }
            if out.title.is_empty()
                && let Some(t) = non_empty(&article.title)
            {
                out.title = t;
            }
            if out.author.is_none() {
                out.author = non_empty_opt(article.byline);
            }
            if out.sitename.is_none() {
                out.sitename = non_empty_opt(article.site_name);
            }
            if out.published_date.is_none() {
                out.published_date = article.published_time.as_deref().and_then(normalize_date);
            }
        }
    }

    // Fallback 2: boilerplate strip + main-region heuristic + htmd.
    if out.markdown.trim().is_empty() {
        let (fb_title, main_html) = fallback_main_html(&cleaned);
        if out.title.is_empty() {
            out.title = fb_title;
        }
        out.markdown = html_to_markdown(&main_html);
    }

    // Refine & supplement the 4 metadata fields from explicit head/JSON-LD/meta tags.
    refine_meta_from_dom(&cleaned, url, &mut out);

    out
}
