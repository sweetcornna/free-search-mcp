//! CSS-selector HTML extraction pipeline (A1 §B.4).

use crate::jaq_rt::collapse_ws_str;
use dom_query::{Document, Matcher, Selection};
use fsm_core::FsmError;
use fsm_core::manifest::HtmlSpec;
use serde_json::{Map, Value};

#[derive(Debug, Clone)]
enum Extractor {
    Text,
    Html,
    Texts,
    Attr(String),
    Attrs(String),
}

struct CompiledField {
    name: String,
    matcher: Option<Matcher>,
    extractor: Extractor,
}

fn parse_field_expr(field: &str, expr: &str) -> Result<(Option<String>, Extractor), FsmError> {
    let (sel_part, ext_part) = match expr.rsplit_once('|') {
        Some((lhs, rhs)) => {
            let lhs = lhs.trim();
            (
                if lhs.is_empty() {
                    None
                } else {
                    Some(lhs.to_owned())
                },
                rhs.trim(),
            )
        }
        None => (None, expr.trim()),
    };

    let extractor = match ext_part {
        "text" => Extractor::Text,
        "html" => Extractor::Html,
        "texts" => Extractor::Texts,
        other => {
            if let Some(inner) = other
                .strip_prefix("attr(")
                .and_then(|s| s.strip_suffix(')'))
            {
                let name = inner.trim();
                if name.is_empty() {
                    return Err(FsmError::invalid(format!(
                        "html.fields.{field}: empty attr() name"
                    )));
                }
                Extractor::Attr(name.to_owned())
            } else if let Some(inner) = other
                .strip_prefix("attrs(")
                .and_then(|s| s.strip_suffix(')'))
            {
                let name = inner.trim();
                if name.is_empty() {
                    return Err(FsmError::invalid(format!(
                        "html.fields.{field}: empty attrs() name"
                    )));
                }
                Extractor::Attrs(name.to_owned())
            } else {
                return Err(FsmError::invalid(format!(
                    "html.fields.{field}: unknown extractor {ext_part:?}"
                )));
            }
        }
    };

    Ok((sel_part, extractor))
}

/// Validate that `items_css` and every CSS selector in `html.fields` parse cleanly.
pub fn validate_html_spec(spec: &HtmlSpec) -> Result<(), FsmError> {
    Matcher::new(&spec.items_css).map_err(|e| {
        FsmError::invalid(format!(
            "html.items_css {:?} invalid CSS selector: {e:?}",
            spec.items_css
        ))
    })?;
    for (field, expr) in spec.fields.iter() {
        let (sel, _) = parse_field_expr(field, expr)?;
        if let Some(css) = sel {
            Matcher::new(&css).map_err(|e| {
                FsmError::invalid(format!(
                    "html.fields.{field} selector {css:?} invalid CSS: {e:?}"
                ))
            })?;
        }
    }
    Ok(())
}

const MAX_HTML_INPUT_BYTES: usize = 5 * 1024 * 1024;
const MAX_HTML_NESTING_DEPTH: usize = 512;

fn find_ascii_ci(haystack: &[u8], needle_lower: &[u8]) -> Option<usize> {
    if needle_lower.is_empty() || haystack.len() < needle_lower.len() {
        return None;
    }
    haystack
        .windows(needle_lower.len())
        .position(|w| w.eq_ignore_ascii_case(needle_lower))
}

fn is_void_html_tag(tag_lower: &[u8]) -> bool {
    matches!(
        tag_lower,
        b"area"
            | b"base"
            | b"br"
            | b"col"
            | b"embed"
            | b"hr"
            | b"img"
            | b"input"
            | b"link"
            | b"meta"
            | b"param"
            | b"source"
            | b"track"
            | b"wbr"
    )
}

fn html_nesting_exceeds(html: &str, max_depth: usize) -> bool {
    let bytes = html.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    let mut depth = 0usize;

    while i < len {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        if bytes[i..].starts_with(b"<!--") {
            match find_ascii_ci(&bytes[i + 4..], b"-->") {
                Some(rel) => i = i + 4 + rel + 3,
                None => break,
            }
            continue;
        }
        let Some(first) = bytes.get(i + 1).copied() else {
            break;
        };
        if first == b'!' || first == b'?' {
            match bytes[i + 2..].iter().position(|&b| b == b'>') {
                Some(rel) => i = i + 2 + rel + 1,
                None => break,
            }
            continue;
        }
        let is_close = first == b'/';
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
        let mut tag_lower = [0u8; 16];
        let tag_len = (name_end - name_start).min(tag_lower.len());
        for (dst, &src) in tag_lower[..tag_len]
            .iter_mut()
            .zip(&bytes[name_start..name_start + tag_len])
        {
            *dst = src.to_ascii_lowercase();
        }
        let tag_slice = &tag_lower[..tag_len];

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
                break;
            }
            j += 1;
        }
        if j >= len {
            break;
        }
        let self_closing = !is_close && j > name_start && bytes[j - 1] == b'/';
        i = j + 1;

        if is_close {
            depth = depth.saturating_sub(1);
            continue;
        }
        if !self_closing && (tag_slice == b"script" || tag_slice == b"style") {
            let needle: &[u8] = if tag_slice == b"script" {
                b"</script"
            } else {
                b"</style"
            };
            match find_ascii_ci(&bytes[i..], needle) {
                Some(rel) => {
                    let after = i + rel + needle.len();
                    match bytes[after..].iter().position(|&b| b == b'>') {
                        Some(gt) => i = after + gt + 1,
                        None => break,
                    }
                }
                None => break,
            }
            continue;
        }
        if !self_closing && !is_void_html_tag(tag_slice) {
            depth += 1;
            if depth > max_depth {
                return true;
            }
        }
    }
    false
}

/// Extract a JSON array of row objects from an HTML document per A1 §B.4.
pub fn extract_html(
    html: &str,
    spec: &HtmlSpec,
    source_id: &str,
    status: u16,
) -> Result<Value, FsmError> {
    // 1. Case-insensitive wall detection before DOM parsing.
    if !spec.wall_markers.is_empty() {
        let bytes = html.as_bytes();
        for marker in &spec.wall_markers {
            if !marker.is_empty()
                && find_ascii_ci(bytes, marker.to_ascii_lowercase().as_bytes()).is_some()
            {
                return Err(FsmError::upstream(
                    source_id,
                    Some(status),
                    format!("HTML wall marker matched: {marker:?}"),
                ));
            }
        }
    }

    if html.len() > MAX_HTML_INPUT_BYTES || html_nesting_exceeds(html, MAX_HTML_NESTING_DEPTH) {
        return Err(FsmError::upstream(
            source_id,
            Some(status),
            "HTML response exceeds safe size or tag nesting depth limit",
        ));
    }

    let items_matcher = Matcher::new(&spec.items_css).map_err(|e| {
        FsmError::invalid(format!(
            "html.items_css {:?} invalid CSS: {e:?}",
            spec.items_css
        ))
    })?;

    let mut compiled = Vec::with_capacity(spec.fields.len());
    for (field, expr) in spec.fields.iter() {
        let (sel, extractor) = parse_field_expr(field, expr)?;
        let matcher = match sel {
            Some(css) => Some(Matcher::new(&css).map_err(|e| {
                FsmError::invalid(format!(
                    "html.fields.{field} selector {css:?} invalid CSS: {e:?}"
                ))
            })?),
            None => None,
        };
        compiled.push(CompiledField {
            name: field.to_owned(),
            matcher,
            extractor,
        });
    }

    let doc = Document::from(html);
    let items_sel = doc.select_matcher(&items_matcher);
    let mut rows = Vec::with_capacity(items_sel.length());

    for item_node in items_sel.nodes() {
        let item = Selection::from(*item_node);
        let mut row = Map::with_capacity(compiled.len());
        for cf in &compiled {
            let target = match &cf.matcher {
                Some(m) => item.select_matcher(m),
                None => item.clone(),
            };
            let value = apply_extractor(&target, &cf.extractor);
            row.insert(cf.name.clone(), value);
        }
        rows.push(Value::Object(row));
    }

    Ok(Value::Array(rows))
}

fn apply_extractor(sel: &Selection<'_>, ext: &Extractor) -> Value {
    match ext {
        Extractor::Text => match sel.nodes().first() {
            Some(n) => Value::String(collapse_ws_str(&n.text())),
            None => Value::Null,
        },
        Extractor::Html => match sel.nodes().first() {
            Some(n) => Value::String(n.inner_html().to_string()),
            None => Value::Null,
        },
        Extractor::Attr(attr) => sel
            .nodes()
            .first()
            .and_then(|n| n.attr(attr))
            .map_or(Value::Null, |v| Value::String(v.to_string())),
        Extractor::Texts => {
            let vals: Vec<Value> = sel
                .nodes()
                .iter()
                .map(|n| collapse_ws_str(&n.text()))
                .filter(|s| !s.is_empty())
                .map(Value::String)
                .collect();
            Value::Array(vals)
        }
        Extractor::Attrs(attr) => {
            let vals: Vec<Value> = sel
                .nodes()
                .iter()
                .filter_map(|n| n.attr(attr))
                .map(|v| Value::String(v.to_string()))
                .collect();
            Value::Array(vals)
        }
    }
}
