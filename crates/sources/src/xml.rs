//! Deterministic `roxmltree` XML → JSON conversion (A1 §B.6, R3 hardened).
//!
//! Security & normalization rules:
//! - R3: Reject any XML document declaring DTD entities (`<!ENTITY`, case-insensitive)
//!   before parsing; enforce explicit limits on node count, nesting depth, text length,
//!   attribute length, and total materialized output bytes.
//! - Strip XML namespace prefixes AND ASCII-lowercase ALL element & attribute local names.
//! - Root JSON object is `{ "<lower_root_local_name>": <root_node_value> }`.
//! - Leaf element (no attrs, no child elements) -> `Value::String` (concatenated text/CDATA, `""` if empty).
//! - Complex element (attrs and/or child elements) -> `Value::Object` with:
//!   - `"@<lower_attr>"` for every attribute,
//!   - `"#text"` for concatenated direct text/CDATA if non-whitespace,
//!   - child elements grouped by `<lower_local_name>` (1 child = direct value; 2+ = JSON array).
//! - Comments and processing instructions ignored.

use fsm_core::FsmError;
use roxmltree::{Document, Node, ParsingOptions};
use serde_json::{Map, Value};

/// Maximum XML element nesting depth (R3 stack/DoS cap).
pub const MAX_XML_DEPTH: usize = 64;
/// Maximum byte length of direct text content on any single element (R3).
pub const MAX_XML_TEXT_BYTES: usize = 1_048_576;
/// Maximum byte length of any single XML attribute value (R3).
pub const MAX_XML_ATTR_BYTES: usize = 65_536;
/// Maximum total materialized text + attribute bytes across an entire XML document (R3).
pub const MAX_XML_TOTAL_BYTES: usize = 16_777_216;
/// Maximum XML nodes permitted by `roxmltree` (R3).
pub const MAX_XML_NODES: u32 = 50_000;

/// Parse an XML string into the canonical A1 §B.6 JSON representation with R3 caps.
pub fn xml_to_json(xml: &str, source_id: &str) -> Result<Value, FsmError> {
    // R3: Reject any DTD entity declaration before roxmltree ever parses or expands it.
    if contains_entity_decl(xml.as_bytes()) {
        return Err(FsmError::upstream(
            source_id,
            None,
            "XML <!ENTITY declarations are forbidden",
        ));
    }

    let opts = ParsingOptions {
        allow_dtd: true,
        nodes_limit: MAX_XML_NODES,
        ..ParsingOptions::default()
    };
    let doc = Document::parse_with_options(xml, opts)
        .map_err(|e| FsmError::upstream(source_id, None, format!("invalid XML: {e}")))?;
    let root = doc.root_element();
    let root_key = root.tag_name().name().to_ascii_lowercase();
    let mut total_bytes = 0usize;
    let val = node_to_json(root, 0, &mut total_bytes, source_id)?;
    let mut wrapper = Map::with_capacity(1);
    wrapper.insert(root_key, val);
    Ok(Value::Object(wrapper))
}

fn contains_entity_decl(bytes: &[u8]) -> bool {
    bytes
        .windows(8)
        .any(|w| w.eq_ignore_ascii_case(b"<!entity"))
}

fn node_to_json(
    node: Node<'_, '_>,
    depth: usize,
    total_bytes: &mut usize,
    source_id: &str,
) -> Result<Value, FsmError> {
    if depth > MAX_XML_DEPTH {
        return Err(FsmError::upstream(
            source_id,
            None,
            format!("XML nesting depth exceeded limit of {MAX_XML_DEPTH}"),
        ));
    }

    let has_attrs = node.attributes().len() > 0;
    let has_child_elems = node.children().any(|c| c.is_element());

    // Direct text / CDATA concatenation with per-node and document-wide byte caps.
    let mut direct_text = String::new();
    for child in node.children() {
        if child.is_text()
            && let Some(t) = child.text()
        {
            let next_len = direct_text.len().saturating_add(t.len());
            if next_len > MAX_XML_TEXT_BYTES {
                return Err(FsmError::upstream(
                    source_id,
                    None,
                    format!("XML text node exceeded limit of {MAX_XML_TEXT_BYTES} bytes"),
                ));
            }
            *total_bytes = total_bytes.saturating_add(t.len());
            if *total_bytes > MAX_XML_TOTAL_BYTES {
                return Err(FsmError::upstream(
                    source_id,
                    None,
                    format!("XML total content exceeded limit of {MAX_XML_TOTAL_BYTES} bytes"),
                ));
            }
            direct_text.push_str(t);
        }
    }

    if !has_attrs && !has_child_elems {
        return Ok(Value::String(direct_text));
    }

    let mut obj = Map::new();
    for attr in node.attributes() {
        let val = attr.value();
        if val.len() > MAX_XML_ATTR_BYTES {
            return Err(FsmError::upstream(
                source_id,
                None,
                format!("XML attribute value exceeded limit of {MAX_XML_ATTR_BYTES} bytes"),
            ));
        }
        *total_bytes = total_bytes.saturating_add(val.len());
        if *total_bytes > MAX_XML_TOTAL_BYTES {
            return Err(FsmError::upstream(
                source_id,
                None,
                format!("XML total content exceeded limit of {MAX_XML_TOTAL_BYTES} bytes"),
            ));
        }
        let key = format!("@{}", attr.name().to_ascii_lowercase());
        obj.insert(key, Value::String(val.to_owned()));
    }

    if !direct_text.trim().is_empty() {
        obj.insert("#text".to_owned(), Value::String(direct_text));
    }

    for child in node.children().filter(|c| c.is_element()) {
        let key = child.tag_name().name().to_ascii_lowercase();
        let val = node_to_json(child, depth + 1, total_bytes, source_id)?;
        match obj.get_mut(&key) {
            None => {
                obj.insert(key, val);
            }
            Some(Value::Array(arr)) => {
                arr.push(val);
            }
            Some(existing) => {
                let prev = std::mem::replace(existing, Value::Null);
                *existing = Value::Array(vec![prev, val]);
            }
        }
    }

    Ok(Value::Object(obj))
}
