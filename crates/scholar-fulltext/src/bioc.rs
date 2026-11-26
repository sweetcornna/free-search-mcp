//! PMC BioC JSON parser (`pmcoa.cgi/BioC_json/{pmcid}/unicode`).

use crate::sections::{ParsedDocument, ParsedSection, collapse_ws, normalize_license};
use fsm_core::{FsmError, FulltextSection};
use serde_json::Value;

/// Parse a PMC BioC JSON payload (array of collections, single collection, or single document)
/// into structured [`ParsedDocument`] sections.
pub fn parse_bioc_json(json_text: &str) -> Result<ParsedDocument, FsmError> {
    let root: Value = serde_json::from_str(json_text)
        .map_err(|e| FsmError::upstream("pmc_bioc", None, format!("invalid BioC JSON: {e}")))?;

    let (doc_license, passages) = extract_doc_and_passages(&root).ok_or_else(|| {
        FsmError::upstream(
            "pmc_bioc",
            None,
            "BioC JSON contained no documents/passages",
        )
    })?;

    let mut doc = ParsedDocument {
        license: doc_license,
        ..ParsedDocument::default()
    };
    let mut current_heading: Option<String> = None;
    let mut current_section_type: Option<String> = None;
    let mut current_blocks: Vec<String> = Vec::new();
    let mut current_subs: Vec<FulltextSection> = Vec::new();
    let mut active_sub_heading: Option<String> = None;
    let mut active_sub_blocks: Vec<String> = Vec::new();
    let mut ref_lines: Vec<String> = Vec::new();

    let flush_sub = |sub_heading: &mut Option<String>,
                     sub_blocks: &mut Vec<String>,
                     subs: &mut Vec<FulltextSection>| {
        if let Some(h) = sub_heading.take() {
            let text = sub_blocks.join("\n\n").trim().to_owned();
            if !text.is_empty() {
                subs.push(FulltextSection::new(h, text));
            }
        }
        sub_blocks.clear();
    };

    let flush_section = |heading: &mut Option<String>,
                         blocks: &mut Vec<String>,
                         subs: &mut Vec<FulltextSection>,
                         sub_heading: &mut Option<String>,
                         sub_blocks: &mut Vec<String>,
                         out: &mut Vec<ParsedSection>| {
        flush_sub(sub_heading, sub_blocks, subs);
        if let Some(h) = heading.take() {
            let text = blocks.join("\n\n").trim().to_owned();
            if !text.is_empty() {
                out.push(ParsedSection {
                    heading: h,
                    text,
                    subsections: std::mem::take(subs),
                });
            }
        }
        blocks.clear();
        subs.clear();
    };

    for passage in passages {
        let infons = passage.get("infons").and_then(Value::as_object);
        let section_type = infons
            .and_then(|m| m.get("section_type"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_ascii_uppercase();
        let ptype = infons
            .and_then(|m| m.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let text = passage
            .get("text")
            .and_then(Value::as_str)
            .map(collapse_ws)
            .unwrap_or_default();

        // Pick up license metadata if present in front infons.
        if doc.license.is_none()
            && let Some(lic) = infons
                .and_then(|m| m.get("license"))
                .and_then(Value::as_str)
                .and_then(normalize_license)
        {
            doc.license = Some(lic);
        }

        if text.is_empty() {
            continue;
        }

        // Front / paper title
        if section_type == "TITLE" || ptype == "front" {
            if doc.title.is_none() {
                doc.title = Some(text);
            }
            continue;
        }

        // References collected into one trailing section
        if section_type == "REF" || ptype == "ref" {
            ref_lines.push(text);
            continue;
        }

        // Top-level title starts a new section
        if ptype == "title_1" || (ptype == "title" && section_type != "ABSTRACT") {
            flush_section(
                &mut current_heading,
                &mut current_blocks,
                &mut current_subs,
                &mut active_sub_heading,
                &mut active_sub_blocks,
                &mut doc.sections,
            );
            current_heading = Some(text);
            current_section_type = Some(section_type);
            continue;
        }

        // Sub-level headings stay inside current section and get indexed as subsections
        if matches!(
            ptype.as_str(),
            "title_2" | "title_3" | "title_4" | "abstract_title_1" | "abstract_title_2"
        ) {
            if current_heading.is_none() {
                current_heading = Some(bioc_section_name(&section_type));
                current_section_type = Some(section_type.clone());
            }
            flush_sub(
                &mut active_sub_heading,
                &mut active_sub_blocks,
                &mut current_subs,
            );
            current_blocks.push(format!("### {text}"));
            active_sub_heading = Some(text);
            continue;
        }

        // Figure / table captions attach cleanly to the active section
        if matches!(
            ptype.as_str(),
            "fig_caption" | "fig_title_caption" | "table_caption" | "table_title_caption"
        ) || matches!(section_type.as_str(), "FIG" | "TABLE")
        {
            let tag = if ptype.contains("table") || section_type == "TABLE" {
                "Table"
            } else {
                "Figure"
            };
            let formatted = format!("[{tag}: {text}]");
            if current_heading.is_none() {
                current_heading = Some(bioc_section_name(&section_type));
                current_section_type = Some(section_type.clone());
            }
            current_blocks.push(formatted.clone());
            if active_sub_heading.is_some() {
                active_sub_blocks.push(formatted);
            }
            continue;
        }

        // Section transition when `section_type` changes without an explicit `title_1`
        let changed_section = current_section_type.as_deref().is_some_and(|prev| {
            !prev.is_empty() && !section_type.is_empty() && prev != section_type
        });
        if current_heading.is_none() || changed_section {
            flush_section(
                &mut current_heading,
                &mut current_blocks,
                &mut current_subs,
                &mut active_sub_heading,
                &mut active_sub_blocks,
                &mut doc.sections,
            );
            current_heading = Some(bioc_section_name(&section_type));
            current_section_type = Some(section_type);
        }

        current_blocks.push(text.clone());
        if active_sub_heading.is_some() {
            active_sub_blocks.push(text);
        }
    }

    flush_section(
        &mut current_heading,
        &mut current_blocks,
        &mut current_subs,
        &mut active_sub_heading,
        &mut active_sub_blocks,
        &mut doc.sections,
    );

    if !ref_lines.is_empty() {
        let numbered: Vec<String> = ref_lines
            .into_iter()
            .enumerate()
            .map(|(i, line)| format!("{}. {line}", i + 1))
            .collect();
        doc.sections.push(ParsedSection {
            heading: "References".to_owned(),
            text: numbered.join("\n"),
            subsections: Vec::new(),
        });
    }

    if doc.is_empty() {
        return Err(FsmError::upstream(
            "pmc_bioc",
            None,
            "BioC JSON contained no readable passages",
        ));
    }

    Ok(doc)
}

fn extract_doc_and_passages(root: &Value) -> Option<(Option<String>, &Vec<Value>)> {
    if let Some(arr) = root.as_array() {
        for item in arr {
            if let Some(found) = extract_doc_and_passages(item) {
                return Some(found);
            }
        }
        return None;
    }
    if let Some(docs) = root.get("documents").and_then(Value::as_array) {
        for d in docs {
            if let Some(passages) = d.get("passages").and_then(Value::as_array)
                && !passages.is_empty()
            {
                let lic = d
                    .pointer("/infons/license")
                    .and_then(Value::as_str)
                    .and_then(normalize_license);
                return Some((lic, passages));
            }
        }
    }
    if let Some(passages) = root.get("passages").and_then(Value::as_array)
        && !passages.is_empty()
    {
        let lic = root
            .pointer("/infons/license")
            .and_then(Value::as_str)
            .and_then(normalize_license);
        return Some((lic, passages));
    }
    None
}

fn bioc_section_name(section_type: &str) -> String {
    match section_type {
        "ABSTRACT" => "Abstract".to_owned(),
        "INTRO" => "Introduction".to_owned(),
        "METHODS" => "Methods".to_owned(),
        "RESULTS" => "Results".to_owned(),
        "DISCUSS" => "Discussion".to_owned(),
        "CONCL" => "Conclusion".to_owned(),
        "CASE" => "Case Report".to_owned(),
        "ACK_FUND" => "Acknowledgments".to_owned(),
        "COMP_INT" => "Competing Interests".to_owned(),
        "AUTH_CONT" => "Author Contributions".to_owned(),
        "SUPPL" => "Supplementary Material".to_owned(),
        "ABBR" => "Abbreviations".to_owned(),
        "FIG" => "Figures".to_owned(),
        "TABLE" => "Tables".to_owned(),
        "REF" => "References".to_owned(),
        "" => "Body".to_owned(),
        other => other.to_owned(),
    }
}
