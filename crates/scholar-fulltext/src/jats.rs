//! JATS XML parser for Europe PMC, PMC OAI-PMH v1, bioRxiv/medRxiv `.source.xml`,
//! PLOS manuscript XML, Springer Nature OpenAccess JATS, and Crossref TDM XML.

use crate::sections::{ParsedDocument, ParsedSection, collapse_ws, normalize_license};
use fsm_core::{FsmError, FulltextSection};
use roxmltree::{Document, Node, ParsingOptions};

/// Default hard ceiling on extracted text characters during JATS traversal (B2).
pub const DEFAULT_MAX_PARSE_CHARS: usize = 2_000_000;
/// Maximum XML element recursion depth during JATS rendering (guards stack against deeply nested trees).
const MAX_JATS_RECURSION_DEPTH: usize = 128;

/// Parse a JATS XML document into structured [`ParsedDocument`] sections using
/// [`DEFAULT_MAX_PARSE_CHARS`].
pub fn parse_jats_xml(xml: &str) -> Result<ParsedDocument, FsmError> {
    parse_jats_xml_with_limit(xml, DEFAULT_MAX_PARSE_CHARS)
}

/// Parse a JATS XML document into structured [`ParsedDocument`] sections,
/// rejecting internal DTD `<!ENTITY` declarations (B2), expanding named HTML/ISO
/// entities without failing on unknown names (I1), and capping extracted chars.
pub fn parse_jats_xml_with_limit(xml: &str, max_chars: usize) -> Result<ParsedDocument, FsmError> {
    let cleaned = preprocess_jats_xml(xml)?;
    let doc = Document::parse_with_options(
        &cleaned,
        ParsingOptions {
            allow_dtd: false,
            nodes_limit: 500_000,
            ..ParsingOptions::default()
        },
    )
    .map_err(|e| FsmError::upstream("jats", None, format!("invalid JATS XML: {e}")))?;

    let article = find_descendant_by_name(doc.root(), "article")
        .ok_or_else(|| FsmError::upstream("jats", None, "JATS XML missing <article> element"))?;

    let mut parsed = ParsedDocument::default();
    let mut budget = CharBudget::new(max_chars.max(1));

    // 1. Front matter: title, license, abstract(s)
    if let Some(front) = find_child_by_name(article, "front")
        && let Some(meta) = find_child_by_name(front, "article-meta")
    {
        if let Some(tg) = find_child_by_name(meta, "title-group")
            && let Some(at) = find_child_by_name(tg, "article-title")
        {
            let title = collapse_ws(&collect_inline_text(at, &mut budget));
            if !title.is_empty() {
                parsed.title = Some(title);
            }
        }
        parsed.license = extract_license(meta);

        let mut abstract_idx = 0usize;
        for child in meta.children().filter(|n| n.is_element()) {
            if budget.exhausted() {
                break;
            }
            if child.tag_name().name() == "abstract" {
                let heading = if abstract_idx == 0 {
                    "Abstract".to_owned()
                } else {
                    child
                        .attribute("abstract-type")
                        .map(prettify_sec_type)
                        .unwrap_or_else(|| "Summary".to_owned())
                };
                let (text, subsections) = render_section_contents(child, 3, &mut budget, 0);
                if !text.is_empty() {
                    parsed.sections.push(ParsedSection {
                        heading,
                        text,
                        subsections,
                    });
                    abstract_idx += 1;
                }
            }
        }
    }

    // 2. Body: top-level <sec> elements (plus orphan top-level blocks / body <ref-list>)
    if !budget.exhausted()
        && let Some(body) = find_child_by_name(article, "body")
    {
        let mut orphan_blocks: Vec<String> = Vec::new();
        for child in body.children().filter(|n| n.is_element()) {
            if budget.exhausted() {
                break;
            }
            match child.tag_name().name() {
                "sec" => {
                    flush_orphan_blocks(&mut orphan_blocks, &mut parsed.sections);
                    if let Some(sec) = parse_top_sec(child, &mut budget) {
                        parsed.sections.push(sec);
                    }
                }
                "ref-list" => {
                    flush_orphan_blocks(&mut orphan_blocks, &mut parsed.sections);
                    let refs = parse_ref_list(child, &mut budget);
                    if !refs.is_empty() {
                        parsed.sections.push(ParsedSection {
                            heading: child_title(child, &mut budget)
                                .unwrap_or_else(|| "References".to_owned()),
                            text: refs,
                            subsections: Vec::new(),
                        });
                    }
                }
                _ => {
                    render_block_node(child, &mut orphan_blocks, &mut budget, 0);
                }
            }
        }
        flush_orphan_blocks(&mut orphan_blocks, &mut parsed.sections);
    }

    // 3. Back matter: ack, app-group, notes, sec, fn-group, ref-list (M1)
    if !budget.exhausted()
        && let Some(back) = find_child_by_name(article, "back")
    {
        for child in back.children().filter(|n| n.is_element()) {
            if budget.exhausted() {
                break;
            }
            match child.tag_name().name() {
                "ack" => {
                    let heading = child_title(child, &mut budget)
                        .unwrap_or_else(|| "Acknowledgments".to_owned());
                    let (text, subsections) = render_section_contents(child, 3, &mut budget, 0);
                    if !text.is_empty() {
                        parsed.sections.push(ParsedSection {
                            heading,
                            text,
                            subsections,
                        });
                    }
                }
                "app-group" => {
                    for app in child
                        .children()
                        .filter(|n| n.is_element() && n.tag_name().name() == "app")
                    {
                        if budget.exhausted() {
                            break;
                        }
                        let heading =
                            child_title(app, &mut budget).unwrap_or_else(|| "Appendix".to_owned());
                        let (text, subsections) = render_section_contents(app, 3, &mut budget, 0);
                        if !text.is_empty() {
                            parsed.sections.push(ParsedSection {
                                heading,
                                text,
                                subsections,
                            });
                        }
                    }
                }
                "sec" => {
                    if let Some(sec) = parse_top_sec(child, &mut budget) {
                        parsed.sections.push(sec);
                    }
                }
                "fn-group" => {
                    let heading =
                        child_title(child, &mut budget).unwrap_or_else(|| "Footnotes".to_owned());
                    let text = render_fn_group(child, &mut budget);
                    if !text.is_empty() {
                        parsed.sections.push(ParsedSection {
                            heading,
                            text,
                            subsections: Vec::new(),
                        });
                    }
                }
                "notes" => {
                    let heading = child_title(child, &mut budget)
                        .or_else(|| child.attribute("notes-type").map(prettify_sec_type))
                        .unwrap_or_else(|| "Notes".to_owned());
                    let (text, subsections) = render_section_contents(child, 3, &mut budget, 0);
                    if !text.is_empty() {
                        parsed.sections.push(ParsedSection {
                            heading,
                            text,
                            subsections,
                        });
                    }
                }
                "ref-list" => {
                    let refs = parse_ref_list(child, &mut budget);
                    if !refs.is_empty() {
                        parsed.sections.push(ParsedSection {
                            heading: child_title(child, &mut budget)
                                .unwrap_or_else(|| "References".to_owned()),
                            text: refs,
                            subsections: Vec::new(),
                        });
                    }
                }
                _ => {}
            }
        }
    }

    // 4. Floats group (M1: standalone <fig> / <table-wrap> elements at end of article)
    if !budget.exhausted()
        && let Some(floats) = find_child_by_name(article, "floats-group")
    {
        let mut float_blocks = Vec::new();
        for child in floats.children().filter(|n| n.is_element()) {
            if budget.exhausted() {
                break;
            }
            render_block_node(child, &mut float_blocks, &mut budget, 0);
        }
        if !float_blocks.is_empty() {
            parsed.sections.push(ParsedSection {
                heading: "Figures and Tables".to_owned(),
                text: float_blocks.join("\n\n"),
                subsections: Vec::new(),
            });
        }
    }

    if parsed.is_empty() {
        return Err(FsmError::upstream(
            "jats",
            None,
            "JATS XML contained no readable sections",
        ));
    }

    Ok(parsed)
}

struct CharBudget {
    remaining: usize,
}

impl CharBudget {
    fn new(limit: usize) -> Self {
        Self { remaining: limit }
    }

    fn exhausted(&self) -> bool {
        self.remaining == 0
    }

    fn take_str(&mut self, s: &str, out: &mut String) {
        if self.remaining == 0 || s.is_empty() {
            return;
        }
        let count = s.chars().count();
        if count <= self.remaining {
            out.push_str(s);
            self.remaining -= count;
        } else {
            for ch in s.chars().take(self.remaining) {
                out.push(ch);
            }
            self.remaining = 0;
        }
    }
}

fn flush_orphan_blocks(blocks: &mut Vec<String>, out: &mut Vec<ParsedSection>) {
    if blocks.is_empty() {
        return;
    }
    let text = blocks.join("\n\n");
    blocks.clear();
    if !text.trim().is_empty() {
        out.push(ParsedSection {
            heading: "Body".to_owned(),
            text,
            subsections: Vec::new(),
        });
    }
}

fn parse_top_sec(sec: Node<'_, '_>, budget: &mut CharBudget) -> Option<ParsedSection> {
    let sec_type = sec.attribute("sec-type");
    let heading = child_title(sec, budget)
        .or_else(|| sec_type.map(prettify_sec_type))
        .unwrap_or_else(|| {
            if find_child_by_name(sec, "ref-list").is_some() {
                "References".to_owned()
            } else {
                "Section".to_owned()
            }
        });
    let (text, subsections) = render_section_contents(sec, 3, budget, 0);
    if text.is_empty() {
        return None;
    }
    Some(ParsedSection {
        heading,
        text,
        subsections,
    })
}

fn render_section_contents(
    container: Node<'_, '_>,
    heading_level: usize,
    budget: &mut CharBudget,
    depth: usize,
) -> (String, Vec<FulltextSection>) {
    if depth >= MAX_JATS_RECURSION_DEPTH || budget.exhausted() {
        return (String::new(), Vec::new());
    }
    let mut blocks: Vec<String> = Vec::new();
    let mut subsections: Vec<FulltextSection> = Vec::new();

    for child in container.children().filter(|n| n.is_element()) {
        if budget.exhausted() {
            break;
        }
        match child.tag_name().name() {
            "title" | "label" => {} // handled by parent heading
            "sec" => {
                let sub_heading = child_title(child, budget)
                    .or_else(|| child.attribute("sec-type").map(prettify_sec_type))
                    .unwrap_or_else(|| "Subsection".to_owned());
                let (sub_text, mut nested) =
                    render_section_contents(child, (heading_level + 1).min(6), budget, depth + 1);
                if !sub_text.is_empty() {
                    let hashes = "#".repeat(heading_level.min(6));
                    blocks.push(format!("{hashes} {sub_heading}\n\n{sub_text}"));
                    subsections.push(FulltextSection::new(sub_heading, sub_text));
                    subsections.append(&mut nested);
                }
            }
            _ => render_block_node(child, &mut blocks, budget, depth + 1),
        }
    }

    (blocks.join("\n\n"), subsections)
}

fn render_block_node(
    node: Node<'_, '_>,
    blocks: &mut Vec<String>,
    budget: &mut CharBudget,
    depth: usize,
) {
    if depth >= MAX_JATS_RECURSION_DEPTH || budget.exhausted() {
        return;
    }
    match node.tag_name().name() {
        "p" => {
            let text = collapse_ws(&collect_inline_text(node, budget));
            if !text.is_empty() {
                blocks.push(text);
            }
        }
        "list" => {
            let mut items = Vec::new();
            for item in node
                .children()
                .filter(|n| n.is_element() && n.tag_name().name() == "list-item")
            {
                if budget.exhausted() {
                    break;
                }
                let t = collapse_ws(&collect_inline_text(item, budget));
                if !t.is_empty() {
                    items.push(format!("- {t}"));
                }
            }
            if !items.is_empty() {
                blocks.push(items.join("\n"));
            }
        }
        "def-list" => {
            let mut items = Vec::new();
            for item in node
                .children()
                .filter(|n| n.is_element() && n.tag_name().name() == "def-item")
            {
                if budget.exhausted() {
                    break;
                }
                let term = find_child_by_name(item, "term")
                    .map(|n| collapse_ws(&collect_inline_text(n, budget)))
                    .unwrap_or_default();
                let def = find_child_by_name(item, "def")
                    .map(|n| collapse_ws(&collect_inline_text(n, budget)))
                    .unwrap_or_default();
                match (term.is_empty(), def.is_empty()) {
                    (false, false) => items.push(format!("- **{term}**: {def}")),
                    (false, true) => items.push(format!("- **{term}**")),
                    (true, false) => items.push(format!("- {def}")),
                    (true, true) => {}
                }
            }
            if !items.is_empty() {
                blocks.push(items.join("\n"));
            }
        }
        "table-wrap" => {
            let rendered = render_table_wrap(node, budget);
            if !rendered.is_empty() {
                blocks.push(rendered);
            }
        }
        "fig" => {
            let label = find_child_by_name(node, "label")
                .map(|n| collapse_ws(&collect_inline_text(n, budget)))
                .unwrap_or_default();
            let caption = find_child_by_name(node, "caption")
                .map(|n| collapse_ws(&collect_inline_text(n, budget)))
                .unwrap_or_default();
            match (label.is_empty(), caption.is_empty()) {
                (false, false) => blocks.push(format!("[{label}: {caption}]")),
                (true, false) => blocks.push(format!("[Figure: {caption}]")),
                _ => {}
            }
        }
        "disp-formula" => {
            let math = extract_formula_text(node, budget);
            if !math.is_empty() {
                blocks.push(format!("$${math}$$"));
            }
        }
        "ref-list" => {
            let refs = parse_ref_list(node, budget);
            if !refs.is_empty() {
                blocks.push(refs);
            }
        }
        "fn-group" => {
            let fns = render_fn_group(node, budget);
            if !fns.is_empty() {
                blocks.push(fns);
            }
        }
        "supplementary-material" => {
            let label = find_child_by_name(node, "label")
                .map(|n| collapse_ws(&collect_inline_text(n, budget)))
                .unwrap_or_default();
            let caption = find_child_by_name(node, "caption")
                .map(|n| collapse_ws(&collect_inline_text(n, budget)))
                .unwrap_or_default();
            let (inner, _) = render_section_contents(node, 4, budget, depth + 1);
            let mut parts = Vec::new();
            match (label.is_empty(), caption.is_empty()) {
                (false, false) => parts.push(format!("[{label}: {caption}]")),
                (true, false) => parts.push(format!("[Supplementary: {caption}]")),
                (false, true) => parts.push(format!("[{label}]")),
                (true, true) => {}
            }
            if !inner.is_empty() {
                parts.push(inner);
            }
            if !parts.is_empty() {
                blocks.push(parts.join("\n"));
            }
        }
        "statement" => {
            let label = find_child_by_name(node, "label")
                .map(|n| collapse_ws(&collect_inline_text(n, budget)))
                .unwrap_or_default();
            let title = child_title(node, budget).unwrap_or_default();
            let (inner, _) = render_section_contents(node, 4, budget, depth + 1);
            if !inner.is_empty() {
                let prefix = match (label.is_empty(), title.is_empty()) {
                    (false, false) => format!("**{label} ({title}).** "),
                    (false, true) => format!("**{label}.** "),
                    (true, false) => format!("**{title}.** "),
                    (true, true) => String::new(),
                };
                blocks.push(format!("{prefix}{inner}"));
            }
        }
        "boxed-text" | "disp-quote" => {
            let (inner, _) = render_section_contents(node, 4, budget, depth + 1);
            if !inner.is_empty() {
                blocks.push(inner);
            }
        }
        _ => {}
    }
}

fn render_fn_group(node: Node<'_, '_>, budget: &mut CharBudget) -> String {
    let mut lines = Vec::new();
    for f in node
        .children()
        .filter(|n| n.is_element() && n.tag_name().name() == "fn")
    {
        if budget.exhausted() {
            break;
        }
        let text = collapse_ws(&collect_inline_text(f, budget));
        if !text.is_empty() {
            lines.push(format!("- {text}"));
        }
    }
    lines.join("\n")
}

fn render_table_wrap(tw: Node<'_, '_>, budget: &mut CharBudget) -> String {
    let mut parts = Vec::new();
    let label = find_child_by_name(tw, "label")
        .map(|n| collapse_ws(&collect_inline_text(n, budget)))
        .unwrap_or_default();
    let caption = find_child_by_name(tw, "caption")
        .map(|n| collapse_ws(&collect_inline_text(n, budget)))
        .unwrap_or_default();
    match (label.is_empty(), caption.is_empty()) {
        (false, false) => parts.push(format!("**{label}:** {caption}")),
        (true, false) => parts.push(format!("**Table:** {caption}")),
        (false, true) => parts.push(format!("**{label}**")),
        (true, true) => {}
    }
    if let Some(table) = find_descendant_by_name(tw, "table") {
        let mut rows: Vec<Vec<String>> = Vec::new();
        for tr in table
            .descendants()
            .filter(|n| n.is_element() && n.tag_name().name() == "tr")
        {
            if budget.exhausted() {
                break;
            }
            let cells: Vec<String> = tr
                .children()
                .filter(|n| n.is_element() && matches!(n.tag_name().name(), "th" | "td"))
                .map(|c| collapse_ws(&collect_inline_text(c, budget)).replace('|', "\\|"))
                .collect();
            if !cells.is_empty() {
                rows.push(cells);
            }
        }
        if !rows.is_empty() {
            let ncols = rows.iter().map(Vec::len).max().unwrap_or(1);
            let mut md = Vec::with_capacity(rows.len() + 1);
            for (i, mut r) in rows.into_iter().enumerate() {
                r.resize(ncols, String::new());
                md.push(format!("| {} |", r.join(" | ")));
                if i == 0 {
                    md.push(format!("| {} |", vec!["---"; ncols].join(" | ")));
                }
            }
            parts.push(md.join("\n"));
        }
    }
    parts.join("\n\n")
}

fn parse_ref_list(ref_list: Node<'_, '_>, budget: &mut CharBudget) -> String {
    let mut lines = Vec::new();
    for (idx, r) in ref_list
        .children()
        .filter(|n| n.is_element() && n.tag_name().name() == "ref")
        .enumerate()
    {
        if budget.exhausted() {
            break;
        }
        let label = find_child_by_name(r, "label")
            .map(|n| collapse_ws(&collect_inline_text(n, budget)))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("{}.", idx + 1));
        let citation = find_child_by_name(r, "mixed-citation")
            .or_else(|| find_child_by_name(r, "element-citation"))
            .or_else(|| find_child_by_name(r, "citation"))
            .map(|n| collapse_ws(&collect_inline_text(n, budget)))
            .unwrap_or_else(|| collapse_ws(&collect_inline_text(r, budget)));
        if !citation.is_empty() {
            lines.push(format!("{label} {citation}"));
        }
    }
    lines.join("\n")
}

fn collect_inline_text(node: Node<'_, '_>, budget: &mut CharBudget) -> String {
    let mut out = String::new();
    collect_inline_into(node, &mut out, budget, 0);
    out
}

fn collect_inline_into(
    node: Node<'_, '_>,
    out: &mut String,
    budget: &mut CharBudget,
    depth: usize,
) {
    if depth >= MAX_JATS_RECURSION_DEPTH || budget.exhausted() {
        return;
    }
    for child in node.children() {
        if budget.exhausted() {
            break;
        }
        if child.is_text() {
            if let Some(t) = child.text() {
                budget.take_str(t, out);
            }
        } else if child.is_element() {
            match child.tag_name().name() {
                "inline-formula" => {
                    let f = extract_formula_text(child, budget);
                    if !f.is_empty() {
                        out.push('$');
                        out.push_str(&f);
                        out.push('$');
                    }
                }
                "disp-formula" => {
                    let f = extract_formula_text(child, budget);
                    if !f.is_empty() {
                        out.push_str(" $$");
                        out.push_str(&f);
                        out.push_str("$$ ");
                    }
                }
                "tex-math" | "alternatives" => {
                    let f = extract_formula_text(child, budget);
                    if !f.is_empty() {
                        out.push_str(&f);
                    }
                }
                // Avoid duplicating nested figures/tables inside <p> text dumps
                "fig" | "table-wrap" | "graphic" | "media" => {}
                _ => {
                    let prev_len = out.len();
                    collect_inline_into(child, out, budget, depth + 1);
                    if out.len() > prev_len
                        && matches!(
                            child.tag_name().name(),
                            "surname" | "given-names" | "article-title" | "source" | "year"
                        )
                    {
                        out.push(' ');
                    }
                }
            }
        }
    }
}

fn extract_formula_text(node: Node<'_, '_>, budget: &mut CharBudget) -> String {
    if let Some(tex) = find_descendant_by_name(node, "tex-math")
        && let Some(raw) = tex.text()
    {
        let stripped = strip_latex_document_wrapper(raw);
        let mut out = String::new();
        budget.take_str(&stripped, &mut out);
        return out;
    }
    if let Some(math) = find_descendant_by_name(node, "math") {
        if let Some(alt) = math.attribute("alttext") {
            let trimmed = collapse_ws(alt);
            if !trimmed.is_empty() {
                let mut out = String::new();
                budget.take_str(&trimmed, &mut out);
                return out;
            }
        }
        if let Some(ann) = find_descendant_by_name(math, "annotation")
            && let Some(t) = ann.text()
        {
            let trimmed = collapse_ws(t);
            if !trimmed.is_empty() {
                let mut out = String::new();
                budget.take_str(&trimmed, &mut out);
                return out;
            }
        }
    }
    let mut buf = String::new();
    for desc in node.descendants() {
        if budget.exhausted() {
            break;
        }
        if desc.is_text()
            && let Some(t) = desc.text()
        {
            budget.take_str(t, &mut buf);
        }
    }
    collapse_ws(&buf)
}

fn strip_latex_document_wrapper(raw: &str) -> String {
    let s = if let Some(after_begin) = raw.split(r"\begin{document}").nth(1) {
        after_begin
            .split(r"\end{document}")
            .next()
            .unwrap_or(after_begin)
    } else {
        raw
    };
    collapse_ws(s.trim().trim_matches('$'))
}

fn extract_license(meta: Node<'_, '_>) -> Option<String> {
    let perms = find_child_by_name(meta, "permissions")?;
    let mut fallback_type: Option<String> = None;
    for lic in perms
        .children()
        .filter(|n| n.is_element() && n.tag_name().name() == "license")
    {
        for attr in lic.attributes() {
            if attr.name() == "href"
                && let Some(norm) = normalize_license(attr.value())
            {
                return Some(norm);
            }
        }
        for desc in lic.descendants().filter(|n| n.is_element()) {
            if desc.tag_name().name() == "ext-link" {
                for attr in desc.attributes() {
                    if attr.name() == "href"
                        && let Some(norm) = normalize_license(attr.value())
                    {
                        return Some(norm);
                    }
                }
            }
        }
        // M1: Inspect <license-p> prose (e.g. Europe PMC "Creative Commons Attribution License")
        let mut lic_p_budget = CharBudget::new(4_096);
        let lic_text = collapse_ws(&collect_inline_text(lic, &mut lic_p_budget));
        if let Some(norm) = extract_license_from_prose(&lic_text) {
            return Some(norm);
        }
        if fallback_type.is_none()
            && let Some(lt) = lic.attribute("license-type")
            && !lt.eq_ignore_ascii_case("open-access")
            && let Some(norm) = normalize_license(lt)
        {
            fallback_type = Some(norm);
        }
    }
    fallback_type
}

fn extract_license_from_prose(text: &str) -> Option<String> {
    if text.is_empty() {
        return None;
    }
    let lower = text.to_ascii_lowercase();
    if lower.contains("creativecommons.org/")
        || lower.contains("creative commons")
        || lower.contains("cc-by")
        || lower.contains("cc by")
        || lower.contains("cc0")
        || lower.contains("public domain dedication")
    {
        return normalize_license(text);
    }
    None
}

fn child_title(node: Node<'_, '_>, budget: &mut CharBudget) -> Option<String> {
    let title = find_child_by_name(node, "title")?;
    let text = collapse_ws(&collect_inline_text(title, budget));
    if text.is_empty() { None } else { Some(text) }
}

fn find_child_by_name<'a, 'input>(node: Node<'a, 'input>, local: &str) -> Option<Node<'a, 'input>> {
    node.children()
        .find(|n| n.is_element() && n.tag_name().name() == local)
}

fn find_descendant_by_name<'a, 'input>(
    node: Node<'a, 'input>,
    local: &str,
) -> Option<Node<'a, 'input>> {
    node.descendants()
        .find(|n| n.is_element() && n.tag_name().name() == local)
}

fn prettify_sec_type(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "intro" | "introduction" => "Introduction".to_owned(),
        "methods" | "materials|methods" | "materials-methods" => "Methods".to_owned(),
        "results" => "Results".to_owned(),
        "discussion" => "Discussion".to_owned(),
        "conclusions" | "conclusion" => "Conclusions".to_owned(),
        "summary" | "executive-summary" => "Summary".to_owned(),
        "cases" | "case" => "Case Report".to_owned(),
        "ref-list" | "references" => "References".to_owned(),
        "supplementary-material" => "Supplementary Material".to_owned(),
        "data-availability" => "Data Availability".to_owned(),
        other => {
            let spaced = other.replace(['_', '-', '|'], " ");
            let mut chars = spaced.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
                None => "Section".to_owned(),
            }
        }
    }
}

/// Strip UTF-8 BOM, reject internal DTD `<!ENTITY` declarations (B2), strip
/// `<!DOCTYPE ...>` declarations so `roxmltree` parses with `allow_dtd: false`,
/// and expand JATS/HTML named entities (escaping any unknown named entity as
/// `&amp;name;` so parsing never fails on an entity name — I1).
fn preprocess_jats_xml(xml: &str) -> Result<String, FsmError> {
    let s = xml.strip_prefix('\u{feff}').unwrap_or(xml);
    let without_doctype = strip_doctype_reject_entities(s)?;
    if !without_doctype.contains('&') {
        return Ok(without_doctype);
    }
    let mut out = String::with_capacity(without_doctype.len());
    let bytes = without_doctype.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'&'
            && let Some(rel_semi) = bytes[i + 1..].iter().take(32).position(|&b| b == b';')
        {
            let name = &without_doctype[i + 1..i + 1 + rel_semi];
            if !name.is_empty() && !name.starts_with('#') {
                if is_xml_builtin_entity(name) {
                    out.push('&');
                    out.push_str(name);
                    out.push(';');
                    i += rel_semi + 2;
                    continue;
                }
                if let Some(repl) = resolve_jats_entity(name) {
                    out.push_str(repl);
                    i += rel_semi + 2;
                    continue;
                }
                if is_entity_name(name) {
                    // I1: preserve unknown named entity as literal text instead of failing XML parse.
                    out.push_str("&amp;");
                    out.push_str(name);
                    out.push(';');
                    i += rel_semi + 2;
                    continue;
                }
            }
        }
        let ch = without_doctype[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    Ok(out)
}

/// Strip `<!DOCTYPE ...>` declarations (including external DTD identifiers) while
/// rejecting any internal DTD subset containing `<!ENTITY` (B2 entity bomb defense).
fn strip_doctype_reject_entities(xml: &str) -> Result<String, FsmError> {
    let bytes = xml.as_bytes();
    let mut out = String::with_capacity(xml.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<'
            && bytes.len().saturating_sub(i) >= 9
            && bytes[i..i + 9].eq_ignore_ascii_case(b"<!DOCTYPE")
        {
            // Advance to closing '>' taking quotes and optional '[ ... ]' internal subset into account.
            let start = i;
            i += 9;
            let mut in_bracket = false;
            let mut quote: Option<u8> = None;
            let mut bracket_start: Option<usize> = None;
            let mut bracket_end: Option<usize> = None;
            while i < bytes.len() {
                let b = bytes[i];
                if let Some(q) = quote {
                    if b == q {
                        quote = None;
                    }
                    i += 1;
                    continue;
                }
                match b {
                    b'"' | b'\'' => {
                        quote = Some(b);
                        i += 1;
                    }
                    b'[' if !in_bracket => {
                        in_bracket = true;
                        bracket_start = Some(i + 1);
                        i += 1;
                    }
                    b']' if in_bracket => {
                        in_bracket = false;
                        bracket_end = Some(i);
                        i += 1;
                    }
                    b'>' if !in_bracket => {
                        i += 1;
                        break;
                    }
                    _ => i += 1,
                }
            }
            if let Some(bs) = bracket_start {
                let be = bracket_end.unwrap_or(i);
                let subset = &xml[bs..be];
                if contains_ascii_ci(subset, "<!entity") {
                    return Err(FsmError::upstream(
                        "jats",
                        None,
                        "JATS XML rejected: internal DTD entity declaration is forbidden",
                    ));
                }
            }
            // Reject unclosed DOCTYPE that still attempted an ENTITY declaration
            if contains_ascii_ci(&xml[start..i], "<!entity") {
                return Err(FsmError::upstream(
                    "jats",
                    None,
                    "JATS XML rejected: DTD entity declaration is forbidden",
                ));
            }
            continue;
        }
        let ch = xml[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    Ok(out)
}

fn contains_ascii_ci(haystack: &str, needle_lower: &str) -> bool {
    let h = haystack.as_bytes();
    let n = needle_lower.as_bytes();
    if n.is_empty() || h.len() < n.len() {
        return false;
    }
    h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

fn is_xml_builtin_entity(name: &str) -> bool {
    matches!(name, "amp" | "lt" | "gt" | "quot" | "apos")
}

fn is_entity_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_alphabetic()
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

fn resolve_jats_entity(name: &str) -> Option<&'static str> {
    match name {
        // Spaces & invisible chars
        "nbsp" | "thinsp" | "thinspace" | "ensp" | "emsp" | "emsp13" | "emsp14" | "hairsp"
        | "numsp" | "puncsp" => Some(" "),
        "shy" | "zwnj" | "zwj" | "lrm" | "rlm" => Some(""),
        // Punctuation & dashes
        "ndash" => Some("\u{2013}"),
        "mdash" | "horbar" => Some("\u{2014}"),
        "minus" => Some("\u{2212}"),
        "lsquo" | "lsquor" => Some("\u{2018}"),
        "rsquo" | "rsquor" => Some("\u{2019}"),
        "ldquo" | "ldquor" => Some("\u{201C}"),
        "rdquo" | "rdquor" => Some("\u{201D}"),
        "laquo" => Some("\u{00AB}"),
        "raquo" => Some("\u{00BB}"),
        "hellip" | "mldr" => Some("\u{2026}"),
        "middot" | "centerdot" => Some("\u{00B7}"),
        "bull" => Some("\u{2022}"),
        "dagger" => Some("\u{2020}"),
        "Dagger" => Some("\u{2021}"),
        "sect" => Some("\u{00A7}"),
        "para" => Some("\u{00B6}"),
        "permil" => Some("\u{2030}"),
        "prime" => Some("\u{2032}"),
        "Prime" => Some("\u{2033}"),
        "lsqb" => Some("["),
        "rsqb" => Some("]"),
        "sol" => Some("/"),
        "bsol" => Some("\\"),
        "verbar" => Some("|"),
        "ast" => Some("*"),
        "lowast" => Some("\u{2217}"),
        "iexcl" => Some("\u{00A1}"),
        "iquest" => Some("\u{00BF}"),
        "copy" => Some("\u{00A9}"),
        "reg" => Some("\u{00AE}"),
        "trade" => Some("\u{2122}"),
        "euro" => Some("\u{20AC}"),
        "pound" => Some("\u{00A3}"),
        "yen" => Some("\u{00A5}"),
        "cent" => Some("\u{00A2}"),
        // Math & arrows
        "times" => Some("\u{00D7}"),
        "divide" => Some("\u{00F7}"),
        "plusmn" => Some("\u{00B1}"),
        "deg" => Some("\u{00B0}"),
        "le" | "leq" => Some("\u{2264}"),
        "ge" | "geq" => Some("\u{2265}"),
        "ne" | "neq" => Some("\u{2260}"),
        "approx" | "asymp" => Some("\u{2248}"),
        "sim" => Some("\u{223C}"),
        "equiv" => Some("\u{2261}"),
        "infin" => Some("\u{221E}"),
        "forall" => Some("\u{2200}"),
        "exist" => Some("\u{2203}"),
        "nabla" => Some("\u{2207}"),
        "part" => Some("\u{2202}"),
        "isin" => Some("\u{2208}"),
        "notin" => Some("\u{2209}"),
        "sum" => Some("\u{2211}"),
        "prod" => Some("\u{220F}"),
        "radic" => Some("\u{221A}"),
        "prop" => Some("\u{221D}"),
        "ang" => Some("\u{2220}"),
        "and" => Some("\u{2227}"),
        "or" => Some("\u{2228}"),
        "cap" => Some("\u{2229}"),
        "cup" => Some("\u{222A}"),
        "int" => Some("\u{222B}"),
        "sub" => Some("\u{2282}"),
        "sup" => Some("\u{2283}"),
        "sube" => Some("\u{2286}"),
        "supe" => Some("\u{2287}"),
        "oplus" => Some("\u{2295}"),
        "otimes" => Some("\u{2297}"),
        "perp" => Some("\u{22A5}"),
        "sdot" => Some("\u{22C5}"),
        "lceil" => Some("\u{2308}"),
        "rceil" => Some("\u{2309}"),
        "lfloor" => Some("\u{230A}"),
        "rfloor" => Some("\u{230B}"),
        "lang" => Some("\u{27E8}"),
        "rang" => Some("\u{27E9}"),
        "larr" => Some("\u{2190}"),
        "uarr" => Some("\u{2191}"),
        "rarr" => Some("\u{2192}"),
        "darr" => Some("\u{2193}"),
        "harr" => Some("\u{2194}"),
        "lArr" => Some("\u{21D0}"),
        "uArr" => Some("\u{21D1}"),
        "rArr" => Some("\u{21D2}"),
        "dArr" => Some("\u{21D3}"),
        "hArr" => Some("\u{21D4}"),
        "frac12" => Some("\u{00BD}"),
        "frac14" => Some("\u{00BC}"),
        "frac34" => Some("\u{00BE}"),
        "sup1" => Some("\u{00B9}"),
        "sup2" => Some("\u{00B2}"),
        "sup3" => Some("\u{00B3}"),
        // Greek letters (lowercase + uppercase)
        "alpha" => Some("\u{03B1}"),
        "beta" => Some("\u{03B2}"),
        "gamma" => Some("\u{03B3}"),
        "delta" => Some("\u{03B4}"),
        "epsilon" | "epsi" => Some("\u{03B5}"),
        "zeta" => Some("\u{03B6}"),
        "eta" => Some("\u{03B7}"),
        "theta" => Some("\u{03B8}"),
        "iota" => Some("\u{03B9}"),
        "kappa" => Some("\u{03BA}"),
        "lambda" => Some("\u{03BB}"),
        "micro" | "mu" => Some("\u{03BC}"),
        "nu" => Some("\u{03BD}"),
        "xi" => Some("\u{03BE}"),
        "omicron" => Some("\u{03BF}"),
        "pi" => Some("\u{03C0}"),
        "rho" => Some("\u{03C1}"),
        "sigmaf" => Some("\u{03C2}"),
        "sigma" => Some("\u{03C3}"),
        "tau" => Some("\u{03C4}"),
        "upsilon" => Some("\u{03C5}"),
        "phi" => Some("\u{03C6}"),
        "chi" => Some("\u{03C7}"),
        "psi" => Some("\u{03C8}"),
        "omega" => Some("\u{03C9}"),
        "Alpha" => Some("\u{0391}"),
        "Beta" => Some("\u{0392}"),
        "Gamma" => Some("\u{0393}"),
        "Delta" => Some("\u{0394}"),
        "Epsilon" => Some("\u{0395}"),
        "Zeta" => Some("\u{0396}"),
        "Eta" => Some("\u{0397}"),
        "Theta" => Some("\u{0398}"),
        "Iota" => Some("\u{0399}"),
        "Kappa" => Some("\u{039A}"),
        "Lambda" => Some("\u{039B}"),
        "Mu" => Some("\u{039C}"),
        "Nu" => Some("\u{039D}"),
        "Xi" => Some("\u{039E}"),
        "Omicron" => Some("\u{039F}"),
        "Pi" => Some("\u{03A0}"),
        "Rho" => Some("\u{03A1}"),
        "Sigma" => Some("\u{03A3}"),
        "Tau" => Some("\u{03A4}"),
        "Upsilon" => Some("\u{03A5}"),
        "Phi" => Some("\u{03A6}"),
        "Chi" => Some("\u{03A7}"),
        "Psi" => Some("\u{03A8}"),
        "Omega" => Some("\u{03A9}"),
        // Common Latin-1 accented letters in author names & prose (I1)
        "Agrave" => Some("\u{00C0}"),
        "Aacute" => Some("\u{00C1}"),
        "Acirc" => Some("\u{00C2}"),
        "Atilde" => Some("\u{00C3}"),
        "Auml" => Some("\u{00C4}"),
        "Aring" => Some("\u{00C5}"),
        "AElig" => Some("\u{00C6}"),
        "Ccedil" => Some("\u{00C7}"),
        "Egrave" => Some("\u{00C8}"),
        "Eacute" => Some("\u{00C9}"),
        "Ecirc" => Some("\u{00CA}"),
        "Euml" => Some("\u{00CB}"),
        "Igrave" => Some("\u{00CC}"),
        "Iacute" => Some("\u{00CD}"),
        "Icirc" => Some("\u{00CE}"),
        "Iuml" => Some("\u{00CF}"),
        "Ntilde" => Some("\u{00D1}"),
        "Ograve" => Some("\u{00D2}"),
        "Oacute" => Some("\u{00D3}"),
        "Ocirc" => Some("\u{00D4}"),
        "Otilde" => Some("\u{00D5}"),
        "Ouml" => Some("\u{00D6}"),
        "Oslash" => Some("\u{00D8}"),
        "Ugrave" => Some("\u{00D9}"),
        "Uacute" => Some("\u{00DA}"),
        "Ucirc" => Some("\u{00DB}"),
        "Uuml" => Some("\u{00DC}"),
        "szlig" => Some("\u{00DF}"),
        "agrave" => Some("\u{00E0}"),
        "aacute" => Some("\u{00E1}"),
        "acirc" => Some("\u{00E2}"),
        "atilde" => Some("\u{00E3}"),
        "auml" => Some("\u{00E4}"),
        "aring" => Some("\u{00E5}"),
        "aelig" => Some("\u{00E6}"),
        "ccedil" => Some("\u{00E7}"),
        "egrave" => Some("\u{00E8}"),
        "eacute" => Some("\u{00E9}"),
        "ecirc" => Some("\u{00EA}"),
        "euml" => Some("\u{00EB}"),
        "igrave" => Some("\u{00EC}"),
        "iacute" => Some("\u{00ED}"),
        "icirc" => Some("\u{00EE}"),
        "iuml" => Some("\u{00EF}"),
        "ntilde" => Some("\u{00F1}"),
        "ograve" => Some("\u{00F2}"),
        "oacute" => Some("\u{00F3}"),
        "ocirc" => Some("\u{00F4}"),
        "otilde" => Some("\u{00F5}"),
        "ouml" => Some("\u{00F6}"),
        "oslash" => Some("\u{00F8}"),
        "ugrave" => Some("\u{00F9}"),
        "uacute" => Some("\u{00FA}"),
        "ucirc" => Some("\u{00FB}"),
        "uuml" => Some("\u{00FC}"),
        "yacute" => Some("\u{00FD}"),
        "yuml" => Some("\u{00FF}"),
        _ => None,
    }
}
