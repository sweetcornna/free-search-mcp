//! EPUB → Markdown (port of `_parse_epub`): OPF located through
//! `META-INF/container.xml`, chapters in spine (reading) order, each XHTML
//! document decoded by its own declaration and converted with `htmd`.

use crate::decode::decode_body;
use crate::markdown::html_to_markdown;
use crate::office::{
    DocError, DocResult, MAX_XML_ATTR_BYTES, MAX_XML_METADATA_BYTES, MAX_XML_TEXT_BYTES, ZipDoc,
    check_zip_budget, reject_entity_decl, resolve_part,
};
use crate::parsed::{Limits, TextBuilder};

pub(crate) struct EpubText {
    pub title: Option<String>,
    pub builder: TextBuilder,
    pub truncated: bool,
}

fn parse_xml(bytes: &[u8]) -> DocResult<String> {
    if bytes.len() > MAX_XML_METADATA_BYTES {
        return Err(DocError::Budget(format!(
            "EPUB metadata XML exceeded {MAX_XML_METADATA_BYTES} byte limit"
        )));
    }
    let text = decode_body(bytes, "application/xml");
    reject_entity_decl(text.as_bytes())?;
    Ok(text)
}

fn xml_doc(text: &str) -> DocResult<roxmltree::Document<'_>> {
    reject_entity_decl(text.as_bytes())?;
    // Benign EPUB 2 OPF/container files may carry an external `<!DOCTYPE ...>`
    // without any `<!ENTITY` declarations; rejecting `<!ENTITY` above ensures
    // roxmltree can never define or expand entities (R3).
    let opt = roxmltree::ParsingOptions {
        allow_dtd: true,
        nodes_limit: 200_000,
        ..roxmltree::ParsingOptions::default()
    };
    let doc = roxmltree::Document::parse_with_options(text, opt).map_err(DocError::parse)?;
    for node in doc.descendants() {
        if let Some(t) = node.text()
            && t.len() > MAX_XML_TEXT_BYTES
        {
            return Err(DocError::Budget(format!(
                "EPUB XML text node exceeded {MAX_XML_TEXT_BYTES} byte limit"
            )));
        }
        for attr in node.attributes() {
            if attr.value().len() > MAX_XML_ATTR_BYTES {
                return Err(DocError::Budget(format!(
                    "EPUB XML attribute exceeded {MAX_XML_ATTR_BYTES} byte limit"
                )));
            }
        }
    }
    Ok(doc)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `_parse_epub`.
pub(crate) fn parse_epub(blob: &[u8], limits: &Limits) -> DocResult<EpubText> {
    check_zip_budget(blob, limits.max_response_bytes)?;
    let mut zip = ZipDoc::open(blob, limits.max_response_bytes)?;
    let container = zip
        .read("META-INF/container.xml")?
        .ok_or_else(|| DocError::Parse("not an EPUB: META-INF/container.xml is missing".into()))?;
    let container = parse_xml(&container)?;
    let opf_path = xml_doc(&container)?
        .descendants()
        .find(|n| n.tag_name().name() == "rootfile")
        .and_then(|n| n.attribute("full-path"))
        .map(str::to_owned)
        .ok_or_else(|| DocError::Parse("EPUB container names no rootfile".into()))?;
    let opf_bytes = zip
        .read(&opf_path)?
        .ok_or_else(|| DocError::Parse(format!("EPUB package {opf_path} is missing")))?;
    let opf_text = parse_xml(&opf_bytes)?;
    let opf = xml_doc(&opf_text)?;
    let base = opf_path.rfind('/').map_or("", |i| &opf_path[..i]);

    let title = opf
        .descendants()
        .find(|n| {
            n.tag_name().name() == "title"
                && n.tag_name()
                    .namespace()
                    .is_some_and(|ns| ns.contains("purl.org/dc"))
        })
        .and_then(|n| n.text())
        .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|t| !t.is_empty());

    let mut manifest: Vec<(String, String, String)> = Vec::new(); // id, href, media-type
    for item in opf.descendants().filter(|n| n.tag_name().name() == "item") {
        if let (Some(id), Some(href)) = (item.attribute("id"), item.attribute("href")) {
            manifest.push((
                id.to_owned(),
                href.to_owned(),
                item.attribute("media-type").unwrap_or("").to_owned(),
            ));
        }
    }
    let is_doc = |mt: &str| mt == "application/xhtml+xml" || mt == "text/html";
    let spine: Vec<String> = opf
        .descendants()
        .filter(|n| n.tag_name().name() == "itemref")
        .filter_map(|n| n.attribute("idref").map(str::to_owned))
        .collect();
    let mut docs: Vec<String> = spine
        .iter()
        .filter_map(|id| manifest.iter().find(|(i, _, mt)| i == id && is_doc(mt)))
        .map(|(_, href, _)| href.clone())
        .collect();
    if docs.is_empty() {
        docs = manifest
            .iter()
            .filter(|(_, _, mt)| is_doc(mt))
            .map(|(_, h, _)| h.clone())
            .collect();
    }

    let mut b = TextBuilder::new();
    let mut acc = 0usize;
    let mut truncated = false;
    for href in docs {
        let href = href.split('#').next().unwrap_or("");
        let part = resolve_part(base, &percent_decode(href));
        let Some(bytes) = zip.read(&part)? else {
            continue;
        };
        let html = decode_body(&bytes, "text/html");
        // Reject DTD `<!ENTITY` declarations in chapter prologs (R3).
        let prolog_end = html
            .as_bytes()
            .windows(5)
            .position(|w| w.eq_ignore_ascii_case(b"<body"))
            .unwrap_or_else(|| html.len().min(64 * 1024));
        reject_entity_decl(&html.as_bytes()[..prolog_end])?;
        let md = html_to_markdown(&html);
        if md.is_empty() {
            continue;
        }
        acc += md.chars().count();
        b.push(&md);
        if acc >= limits.max_document_chars {
            truncated = true;
            break;
        }
    }
    Ok(EpubText {
        title,
        builder: b,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_multibyte_utf8_after_percent_does_not_panic() {
        assert_eq!(percent_decode("ch%€1.xhtml"), "ch%€1.xhtml");
        assert_eq!(percent_decode("a%中"), "a%中");
        assert_eq!(percent_decode("chapter%201.xhtml"), "chapter 1.xhtml");
    }
}
