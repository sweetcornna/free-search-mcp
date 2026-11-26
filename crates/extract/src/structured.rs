//! Structured metadata extraction (`web.structured`, port of `src/search_mcp/structured.py`).
//!
//! Uses `dom_query 0.30` to extract:
//! - JSON-LD (`<script type="application/ld+json">`) with lenient parsing for CMS output
//!   (raw control chars in strings, trailing commas, `//` and `/* */` comments — I3).
//! - OpenGraph (`<meta property="og:...">` / `name="og:..."`).
//! - HTML5 Microdata (`[itemscope]`, `[itemtype]`, `[itemprop]`, safe `[itemref]` lookup
//!   without CSS selector injection — I5).
//! - Bare `<meta>` fallback (SEO + Twitter cards + article tags) + diagnostic `hint`.
//! - Pre-DOM depth & size guards (B1 / I4) and `catch_unwind` degradation (I5).

use crate::charset::decode_body;
use crate::html::{
    MAX_DOM_INPUT_BYTES, MAX_DOM_NESTING_DEPTH, max_nesting_depth, run_html_blocking,
    truncate_utf8_bytes,
};
use crate::request::send_get_with_redirects;
use dom_query::{Document, NodeId, NodeRef};
use fsm_core::text::clip_chars;
use fsm_core::{CallCx, FsmError, SourceDeps, StructuredMeta, TransportProfile};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use url::Url;

pub const LONG_TEXT_CAP: usize = 500;

const LONG_TEXT_KEYS: [&str; 7] = [
    "articleBody",
    "text",
    "description",
    "reviewBody",
    "transcript",
    "abstract",
    "content",
];

const META_TARGETS: [&str; 18] = [
    "description",
    "keywords",
    "author",
    "robots",
    "viewport",
    "theme-color",
    "twitter:card",
    "twitter:title",
    "twitter:description",
    "twitter:image",
    "twitter:site",
    "twitter:creator",
    "article:published_time",
    "article:modified_time",
    "article:author",
    "article:section",
    "article:tag",
    "generator",
];

fn local_name(key: &str) -> &str {
    key.rsplit('/')
        .next()
        .unwrap_or(key)
        .rsplit('#')
        .next()
        .unwrap_or(key)
        .rsplit(':')
        .next()
        .unwrap_or(key)
}

fn clip_str_if_needed(key: &str, value: &str, clipped: &mut Vec<usize>) -> String {
    let char_len = value.chars().count();
    if char_len > LONG_TEXT_CAP && LONG_TEXT_KEYS.contains(&local_name(key)) {
        clipped.push(char_len);
        format!(
            "{} … [clipped: {char_len} characters in the page]",
            clip_chars(value, LONG_TEXT_CAP).trim_end()
        )
    } else {
        value.to_owned()
    }
}

fn clip_long_text(node: Value, clipped: &mut Vec<usize>) -> Value {
    match node {
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| clip_long_text(item, clipped))
                .collect(),
        ),
        Value::Object(map) => {
            let mut out = Map::with_capacity(map.len());
            for (k, v) in map {
                let next = match v {
                    Value::String(s) => Value::String(clip_str_if_needed(&k, &s, clipped)),
                    other => clip_long_text(other, clipped),
                };
                out.insert(k, next);
            }
            Value::Object(out)
        }
        other => other,
    }
}

fn resolve_base_url(doc: &Document, page_url: &str) -> Option<Url> {
    let base = Url::parse(page_url).ok()?;
    let base_el = doc.select_single("base[href]");
    if base_el.exists()
        && let Some(href) = base_el.attr("href")
        && let Ok(joined) = base.join(href.trim())
    {
        return Some(joined);
    }
    Some(base)
}

fn resolve_attr_url(raw: &str, base: Option<&Url>) -> String {
    let trimmed = raw.trim();
    if let Some(b) = base
        && let Ok(u) = b.join(trimmed)
    {
        return u.into();
    }
    trimmed.to_owned()
}

fn clean_json_ld_wrappers(raw: &str) -> &str {
    let mut s = raw.trim();
    if let Some(rest) = s.strip_prefix("<!--") {
        s = rest.trim_start();
    }
    if let Some(rest) = s.strip_suffix("-->") {
        s = rest.trim_end();
    }
    if let Some(rest) = s.strip_prefix("//<![CDATA[") {
        s = rest.trim_start();
    } else if let Some(rest) = s.strip_prefix("<![CDATA[") {
        s = rest.trim_start();
    }
    if let Some(rest) = s.strip_suffix("//]]>") {
        s = rest.trim_end();
    } else if let Some(rest) = s.strip_suffix("]]>") {
        s = rest.trim_end();
    }
    s
}

/// String-aware lenient JSON sanitizer (I3):
/// - Escapes raw control bytes (< 0x20) inside JSON string literals (`\n`, `\r`, `\t`, etc.).
/// - Strips `//...` line comments and `/*...*/` block comments outside strings.
/// - Strips trailing commas before `}` or `]` outside strings.
fn sanitize_lenient_json(input: &str) -> String {
    // Pass 1: escape control chars in strings and strip comments outside strings.
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut pass1 = String::with_capacity(len);
    let mut i = 0usize;
    let mut in_str = false;
    let mut escaped = false;

    while i < len {
        let ch = input[i..].chars().next().unwrap();
        let ch_len = ch.len_utf8();

        if in_str {
            if escaped {
                pass1.push(ch);
                escaped = false;
            } else if ch == '\\' {
                pass1.push(ch);
                escaped = true;
            } else if ch == '"' {
                pass1.push(ch);
                in_str = false;
            } else if (ch as u32) < 0x20 {
                match ch {
                    '\n' => pass1.push_str("\\n"),
                    '\r' => pass1.push_str("\\r"),
                    '\t' => pass1.push_str("\\t"),
                    '\x08' => pass1.push_str("\\b"),
                    '\x0c' => pass1.push_str("\\f"),
                    other => pass1.push_str(&format!("\\u{:04x}", other as u32)),
                }
            } else {
                pass1.push(ch);
            }
            i += ch_len;
            continue;
        }

        if ch == '"' {
            in_str = true;
            pass1.push(ch);
            i += 1;
        } else if ch == '/' && i + 1 < len && bytes[i + 1] == b'/' {
            // Line comment outside string.
            i += 2;
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
        } else if ch == '/' && i + 1 < len && bytes[i + 1] == b'*' {
            // Block comment outside string.
            i += 2;
            while i + 1 < len && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(len);
        } else {
            pass1.push(ch);
            i += ch_len;
        }
    }

    // Pass 2: remove trailing commas before `}` or `]` outside strings.
    let pbytes = pass1.as_bytes();
    let plen = pbytes.len();
    let mut out = String::with_capacity(plen);
    let mut j = 0usize;
    in_str = false;
    escaped = false;

    while j < plen {
        let ch = pass1[j..].chars().next().unwrap();
        let ch_len = ch.len_utf8();
        if in_str {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_str = false;
            }
            j += ch_len;
            continue;
        }
        if ch == '"' {
            in_str = true;
            out.push(ch);
            j += 1;
        } else if ch == ',' {
            let mut k = j + 1;
            while k < plen && pbytes[k].is_ascii_whitespace() {
                k += 1;
            }
            if k < plen && matches!(pbytes[k], b'}' | b']') {
                // Drop trailing comma.
                j += 1;
            } else {
                out.push(ch);
                j += 1;
            }
        } else {
            out.push(ch);
            j += ch_len;
        }
    }

    out
}

/// Parse a JSON-LD block: strict first, then lenient sanitizer fallback (I3).
pub fn parse_json_ld_lenient(raw: &str) -> Option<Value> {
    let cleaned = clean_json_ld_wrappers(raw);
    if cleaned.is_empty() {
        return None;
    }
    if let Ok(v) = serde_json::from_str::<Value>(cleaned) {
        return Some(v);
    }
    let sanitized = sanitize_lenient_json(cleaned);
    serde_json::from_str::<Value>(&sanitized).ok()
}

fn extract_json_ld(doc: &Document, clipped: &mut Vec<usize>) -> Vec<Value> {
    let mut out = Vec::new();
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
        let Some(parsed) = parse_json_ld_lenient(&sel.text()) else {
            continue;
        };
        match parsed {
            Value::Array(arr) => {
                for item in arr {
                    if item.is_object() {
                        out.push(clip_long_text(item, clipped));
                    }
                }
            }
            Value::Object(_) => out.push(clip_long_text(parsed, clipped)),
            _ => {}
        }
    }
    out
}

fn extract_opengraph(doc: &Document, clipped: &mut Vec<usize>) -> BTreeMap<String, String> {
    let mut og = BTreeMap::new();
    for meta in doc.select("meta").iter() {
        let key = meta
            .attr("property")
            .or_else(|| meta.attr("name"))
            .map(|s| s.trim().to_ascii_lowercase());
        let Some(key) = key else { continue };
        if !key.starts_with("og:") {
            continue;
        }
        let Some(content) = meta.attr("content") else {
            continue;
        };
        let val = content.trim();
        if val.is_empty() {
            continue;
        }
        og.entry(key.clone())
            .or_insert_with(|| clip_str_if_needed(&key, val, clipped));
    }
    og
}

fn microdata_prop_value(node: &NodeRef<'_>, base: Option<&Url>) -> String {
    let tag = node
        .node_name()
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    match tag.as_str() {
        "meta" => node
            .attr("content")
            .map(|s| s.trim().to_owned())
            .unwrap_or_default(),
        "audio" | "embed" | "iframe" | "img" | "source" | "track" | "video" => node
            .attr("src")
            .map(|s| resolve_attr_url(&s, base))
            .unwrap_or_default(),
        "a" | "area" | "link" => node
            .attr("href")
            .map(|s| resolve_attr_url(&s, base))
            .unwrap_or_default(),
        "object" => node
            .attr("data")
            .map(|s| resolve_attr_url(&s, base))
            .unwrap_or_default(),
        "data" | "meter" => node
            .attr("value")
            .map(|s| s.trim().to_owned())
            .unwrap_or_default(),
        "time" => node
            .attr("datetime")
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| node.text().trim().to_owned()),
        _ => node.text().trim().to_owned(),
    }
}

fn insert_prop(obj: &mut Map<String, Value>, name: &str, val: Value) {
    match obj.get_mut(name) {
        None => {
            obj.insert(name.to_owned(), val);
        }
        Some(Value::Array(arr)) => arr.push(val),
        Some(existing) => {
            let prev = std::mem::replace(existing, Value::Null);
            *existing = Value::Array(vec![prev, val]);
        }
    }
}

fn collect_item_props(
    doc: &Document,
    id_nodes: &[NodeRef<'_>],
    node: &NodeRef<'_>,
    base: Option<&Url>,
    obj: &mut Map<String, Value>,
    visited: &mut HashSet<NodeId>,
    depth: usize,
) {
    if depth > 16 {
        return;
    }
    for child in node.element_children() {
        let is_scope = child.has_attr("itemscope");
        if let Some(prop_attr) = child.attr("itemprop") {
            let val = if is_scope {
                extract_itemscope(doc, id_nodes, &child, base, visited, depth + 1)
            } else {
                Value::String(microdata_prop_value(&child, base))
            };
            for prop_name in prop_attr.split_ascii_whitespace() {
                if !prop_name.is_empty() {
                    insert_prop(obj, prop_name, val.clone());
                }
            }
        }
        if !is_scope {
            collect_item_props(doc, id_nodes, &child, base, obj, visited, depth + 1);
        }
    }
}

fn extract_itemscope(
    doc: &Document,
    id_nodes: &[NodeRef<'_>],
    root: &NodeRef<'_>,
    base: Option<&Url>,
    visited: &mut HashSet<NodeId>,
    depth: usize,
) -> Value {
    let mut obj = Map::new();
    let node_key = root.id;
    if !visited.insert(node_key) || depth > 16 {
        return Value::Object(obj);
    }

    if let Some(itemtype) = root.attr("itemtype") {
        let t = itemtype.trim();
        if !t.is_empty() {
            if let Some(short) = t
                .strip_prefix("https://schema.org/")
                .or_else(|| t.strip_prefix("http://schema.org/"))
            {
                obj.insert(
                    "@context".to_owned(),
                    Value::String("https://schema.org".to_owned()),
                );
                obj.insert("@type".to_owned(), Value::String(short.to_owned()));
            } else {
                obj.insert("@type".to_owned(), Value::String(t.to_owned()));
            }
        }
    }
    if let Some(itemid) = root.attr("itemid") {
        let id = itemid.trim();
        if !id.is_empty() {
            obj.insert("@id".to_owned(), Value::String(resolve_attr_url(id, base)));
        }
    }

    collect_item_props(doc, id_nodes, root, base, &mut obj, visited, depth);

    // Safe `itemref` lookup: match `id` attribute directly without building a CSS selector
    // from untrusted page content (fixes I5 selector-injection panic).
    if let Some(itemref) = root.attr("itemref") {
        for ref_id in itemref.split_ascii_whitespace() {
            if ref_id.is_empty() {
                continue;
            }
            if let Some(target) = id_nodes
                .iter()
                .find(|n| n.attr("id").as_deref() == Some(ref_id))
            {
                if let Some(prop_attr) = target.attr("itemprop") {
                    let val = if target.has_attr("itemscope") {
                        extract_itemscope(doc, id_nodes, target, base, visited, depth + 1)
                    } else {
                        Value::String(microdata_prop_value(target, base))
                    };
                    for prop_name in prop_attr.split_ascii_whitespace() {
                        if !prop_name.is_empty() {
                            insert_prop(&mut obj, prop_name, val.clone());
                        }
                    }
                }
                if !target.has_attr("itemscope") {
                    collect_item_props(doc, id_nodes, target, base, &mut obj, visited, depth + 1);
                }
            }
        }
    }

    visited.remove(&node_key);
    Value::Object(obj)
}

fn extract_microdata(doc: &Document, base: Option<&Url>, clipped: &mut Vec<usize>) -> Vec<Value> {
    let mut out = Vec::new();
    let scopes = doc.select("[itemscope]");
    if scopes.is_empty() {
        return out;
    }
    let id_sel = doc.select("[id]");
    let id_nodes = id_sel.nodes();
    for node in scopes.nodes() {
        if node.has_attr("itemprop") {
            continue;
        }
        let mut visited = HashSet::new();
        let item = extract_itemscope(doc, id_nodes, node, base, &mut visited, 0);
        if item.as_object().is_some_and(|m| !m.is_empty()) {
            out.push(clip_long_text(item, clipped));
        }
    }
    out
}

fn extract_meta_fallback(doc: &Document) -> BTreeMap<String, String> {
    let mut found: BTreeMap<String, String> = BTreeMap::new();
    for meta in doc.select("meta").iter() {
        let key = meta
            .attr("name")
            .or_else(|| meta.attr("property"))
            .map(|s| s.trim().to_ascii_lowercase());
        let Some(key) = key else { continue };
        if !META_TARGETS.contains(&key.as_str()) {
            continue;
        }
        let Some(content) = meta.attr("content") else {
            continue;
        };
        let val = content.trim();
        if val.is_empty() {
            continue;
        }
        found.entry(key).or_insert_with(|| val.to_owned());
    }
    found
}

fn extract_structured_inner(html: &str, url: &str, status: u16) -> StructuredMeta {
    let mut out = StructuredMeta::new(url);
    if html.trim().is_empty() {
        out.hint = Some(build_empty_hint(status, true));
        return out;
    }

    // Pre-DOM safety guards (B1 / I4): cap input bytes & nesting depth BEFORE html5ever.
    let (bounded, _capped) = truncate_utf8_bytes(html, MAX_DOM_INPUT_BYTES);
    let depth = max_nesting_depth(bounded);
    if depth > MAX_DOM_NESTING_DEPTH {
        out.hint = Some(format!(
            "HTML nesting depth ({depth}) exceeded safe limit ({MAX_DOM_NESTING_DEPTH}); \
             skipped DOM structured-data parsing."
        ));
        return out;
    }

    let doc = Document::from(bounded);
    let base = resolve_base_url(&doc, url);
    let mut clipped: Vec<usize> = Vec::new();

    out.json_ld = extract_json_ld(&doc, &mut clipped);
    out.opengraph = extract_opengraph(&doc, &mut clipped);
    out.microdata = extract_microdata(&doc, base.as_ref(), &mut clipped);

    let has_primary =
        !out.json_ld.is_empty() || !out.opengraph.is_empty() || !out.microdata.is_empty();

    if !has_primary {
        out.meta_fallback = extract_meta_fallback(&doc);
        out.hint = Some(build_empty_hint(status, out.meta_fallback.is_empty()));
    } else if !clipped.is_empty() {
        let total_chars: usize = clipped.iter().sum();
        out.hint = Some(format!(
            "{} long text field(s) clipped to {LONG_TEXT_CAP} characters ({total_chars} in the page), so use `web.fetch` for the full text.",
            clipped.len()
        ));
    }

    out
}

/// Pure HTML -> `StructuredMeta` extractor (usable offline in unit tests).
/// Never panics on pathological HTML: degrades cleanly to empty lists + `hint` (I5).
pub fn extract_structured_from_html(html: &str, url: &str, status: u16) -> StructuredMeta {
    catch_unwind(AssertUnwindSafe(|| {
        extract_structured_inner(html, url, status)
    }))
    .unwrap_or_else(|_| {
        let mut out = StructuredMeta::new(url);
        out.hint = Some(build_empty_hint(status, true));
        out
    })
}

fn build_empty_hint(status: u16, fallback_empty: bool) -> String {
    let mut hint = String::from(
        "No JSON-LD / OpenGraph / microdata found in the initial HTML. Possible causes: \
         (1) page genuinely has no structured metadata, (2) data is loaded by JavaScript \
         after the initial HTML, (3) site blocks bots and served an empty shell. \
         Bare <meta> tags surfaced as `metaFallback` if any.",
    );
    if status >= 400 {
        hint.push_str(&format!(
            " The page returned HTTP {status}, so this is very likely a bot-block/error shell rather than the real content."
        ));
    }
    if fallback_empty {
        hint.push_str(
            " No fallback meta tags either, so the response was likely a bot-block shell.",
        );
    }
    hint
}

/// Fetch `url` over `TransportProfile::Plain` (keeping 4xx/5xx shells for
/// `meta_fallback` + `hint`) and extract structured metadata.
pub async fn fetch_structured(
    url: &str,
    deps: &SourceDeps,
    cx: &CallCx,
) -> Result<StructuredMeta, FsmError> {
    let headers = [(
        "Accept".to_owned(),
        "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8".to_owned(),
    )];
    let (final_url, resp) = send_get_with_redirects(
        url,
        &headers,
        TransportProfile::Plain,
        deps.cfg.max_response_bytes,
        "structured",
        deps,
        cx,
    )
    .await?;

    let raw_ctype = resp.header("content-type").unwrap_or("").to_owned();
    let html = decode_body(&resp.body, &raw_ctype, Some(&final_url));
    let status = resp.status;
    let url_owned = final_url;

    run_html_blocking("structured_task", move || {
        extract_structured_from_html(&html, &url_owned, status)
    })
    .await
}
