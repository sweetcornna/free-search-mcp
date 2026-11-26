//! HTML5 + MathML structured section parser for `arxiv.org/html/{id}`,
//! `ar5iv.labs.arxiv.org/html/{id}`, and generic open-access HTML5 articles.

use crate::sections::{ParsedDocument, ParsedSection, collapse_ws, looks_like_paywall_or_stub};
use dom_query::{Document, Selection};
use fsm_core::{FsmError, FulltextSection};

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

fn strip_html_tags_flat(html: &str) -> String {
    let bytes = html.as_bytes();
    let len = bytes.len().min(MAX_HTML_INPUT_BYTES);
    let mut end = len;
    while end > 0 && !html.is_char_boundary(end) {
        end -= 1;
    }
    let slice = &html[..end];
    let bytes = slice.as_bytes();
    let len = bytes.len();
    let mut raw = String::with_capacity(len.min(64 * 1024));
    let mut i = 0usize;

    while i < len {
        if bytes[i] != b'<' {
            let start = i;
            while i < len && bytes[i] != b'<' {
                i += 1;
            }
            raw.push_str(&slice[start..i]);
            continue;
        }
        if bytes[i..].starts_with(b"<!--") {
            match find_ascii_ci(&bytes[i + 4..], b"-->") {
                Some(rel) => i = i + 4 + rel + 3,
                None => break,
            }
            raw.push(' ');
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
            raw.push(' ');
            continue;
        }
        let is_close = first == b'/';
        let name_start = if is_close { i + 2 } else { i + 1 };
        if name_start >= len || !bytes[name_start].is_ascii_alphabetic() {
            raw.push('<');
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

        if !is_close
            && !self_closing
            && matches!(
                tag_slice,
                b"head" | b"script" | b"style" | b"noscript" | b"svg" | b"nav"
            )
        {
            let mut close_pat = [0u8; 18];
            close_pat[0] = b'<';
            close_pat[1] = b'/';
            close_pat[2..2 + tag_len].copy_from_slice(tag_slice);
            let needle = &close_pat[..2 + tag_len];
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
        if matches!(
            tag_slice,
            b"p" | b"div"
                | b"br"
                | b"li"
                | b"h1"
                | b"h2"
                | b"h3"
                | b"h4"
                | b"h5"
                | b"h6"
                | b"section"
                | b"article"
        ) {
            raw.push('\n');
        } else {
            raw.push(' ');
        }
    }
    collapse_ws(&raw)
}

fn exceeds_dom_safety_limits(html: &str) -> bool {
    html.len() > MAX_HTML_INPUT_BYTES || html_nesting_exceeds(html, MAX_HTML_NESTING_DEPTH)
}

/// Parse an arXiv / ar5iv LaTeXML HTML5 document into structured sections with
/// MathML equations normalized to `$...$` / `$$...$$` TeX.
pub fn parse_arxiv_html5(html: &str) -> Result<ParsedDocument, FsmError> {
    let bytes = html.as_bytes();
    if find_ascii_ci(bytes, b"html is not available for the source").is_some()
        || find_ascii_ci(bytes, b"no html is available for this paper").is_some()
        || (find_ascii_ci(bytes, b"ltx_document").is_none()
            && find_ascii_ci(bytes, b"ltx_section").is_none()
            && find_ascii_ci(bytes, b"<article").is_none())
    {
        return Err(FsmError::upstream(
            "arxiv_html5",
            None,
            "response is not a LaTeXML arXiv HTML5 paper",
        ));
    }

    if exceeds_dom_safety_limits(html) {
        let flat = strip_html_tags_flat(html);
        let parsed = crate::sections::extract_text_sections(&flat, None, None);
        if parsed.is_empty() {
            return Err(FsmError::upstream(
                "arxiv_html5",
                None,
                "arXiv HTML5 document yielded no readable text",
            ));
        }
        return Ok(parsed);
    }

    let doc = Document::from(html);
    strip_noise(&doc);
    normalize_mathml(&doc);

    let mut parsed = ParsedDocument::default();

    // 1. Title
    let title_sel = doc.select_single("h1.ltx_title_document, h1.ltx_title, article h1, h1");
    if title_sel.exists() {
        let t = collapse_ws(&title_sel.text());
        if !t.is_empty() {
            parsed.title = Some(t);
        }
    }

    // 2. Abstract
    let abs_sel = doc.select_single(".ltx_abstract");
    if abs_sel.exists() {
        abs_sel.select(".ltx_title_abstract").remove();
        let abs_text = extract_blocks(&abs_sel);
        if !abs_text.is_empty() {
            parsed.sections.push(ParsedSection {
                heading: "Abstract".to_owned(),
                text: abs_text,
                subsections: Vec::new(),
            });
        }
        abs_sel.remove();
    }

    // 3. Walk direct children of `.ltx_document` / `<article>` in document order so
    //    section-less letters and unheaded pre-section paragraphs are preserved (I2).
    let container = {
        let ltx = doc.select_single("article.ltx_document, .ltx_document");
        if ltx.exists() {
            ltx
        } else {
            let art = doc.select_single("article");
            if art.exists() {
                art
            } else {
                doc.select_single("body")
            }
        }
    };

    let has_named_sections = container
        .select("section.ltx_section, section.ltx_appendix")
        .exists();
    let total_container_chars = collapse_ws(&container.text()).chars().count();

    let mut loose_blocks: Vec<String> = Vec::new();
    let mut saw_any_section = false;
    let mut found_ltx_section = false;

    for child in container.children().iter() {
        if child.is("h1.ltx_title_document, h1.ltx_title, .ltx_abstract, .ltx_classification, .ltx_keywords") {
            continue;
        }
        if child.is(
            "section.ltx_section, section.ltx_appendix, section.ltx_acknowledgements, section.ltx_bibliography, div.ltx_acknowledgements",
        ) {
            flush_loose_ltx_blocks(
                &mut loose_blocks,
                !saw_any_section && has_named_sections,
                &mut parsed.sections,
            );
            saw_any_section = true;
            if let Some(ps) = parse_ltx_section(&child) {
                parsed.sections.push(ps);
                found_ltx_section = true;
            }
        } else if child.is("div.ltx_para, p, figure, table, blockquote, ul, ol, div.ltx_logical-block") {
            extract_single_block(&child, &mut loose_blocks);
        } else if child.select("section.ltx_section, section.ltx_appendix, section.ltx_bibliography").exists() {
            // Wrapper div around sections (e.g., `.ltx_page_main`)
            for nested in child.children().iter() {
                if nested.is(
                    "section.ltx_section, section.ltx_appendix, section.ltx_acknowledgements, section.ltx_bibliography, div.ltx_acknowledgements",
                ) {
                    flush_loose_ltx_blocks(
                        &mut loose_blocks,
                        !saw_any_section && has_named_sections,
                        &mut parsed.sections,
                    );
                    saw_any_section = true;
                    if let Some(ps) = parse_ltx_section(&nested) {
                        parsed.sections.push(ps);
                        found_ltx_section = true;
                    }
                } else if nested.is("div.ltx_para, p, figure, table, blockquote, ul, ol") {
                    extract_single_block(&nested, &mut loose_blocks);
                }
            }
        }
    }
    flush_loose_ltx_blocks(
        &mut loose_blocks,
        !saw_any_section && has_named_sections,
        &mut parsed.sections,
    );

    // If `.ltx_document` wrapper didn't contain direct sections, fall back to selector scan.
    if !found_ltx_section {
        let top_sections = doc.select(
            "section.ltx_section, section.ltx_appendix, section.ltx_acknowledgements, section.ltx_bibliography, div.ltx_acknowledgements",
        );
        for sec in top_sections.iter() {
            if sec
                .ancestors(None)
                .is("section.ltx_section, section.ltx_appendix")
            {
                continue;
            }
            if let Some(ps) = parse_ltx_section(&sec)
                && !parsed
                    .sections
                    .iter()
                    .any(|e| e.heading == ps.heading && e.text == ps.text)
            {
                parsed.sections.push(ps);
            }
        }
    }

    // Coverage check (I2): if extracted non-abstract/non-reference body is tiny compared
    // to container text, fall back to generic HTML5 extraction.
    let body_chars: usize = parsed
        .sections
        .iter()
        .filter(|s| {
            let h = s.heading.to_ascii_lowercase();
            h != "abstract" && h != "references" && h != "bibliography"
        })
        .map(|s| s.text.chars().count())
        .sum();
    if (parsed.is_empty() || (body_chars < 120 && total_container_chars > 600))
        && let Ok(generic) = parse_generic_html5_doc(&doc, parsed.title.clone())
    {
        let generic_chars = generic.total_text_chars();
        if generic_chars > body_chars.saturating_add(200) {
            return Ok(generic);
        }
    }

    if parsed.is_empty() {
        return Err(FsmError::upstream(
            "arxiv_html5",
            None,
            "arXiv HTML5 document yielded no readable text",
        ));
    }

    Ok(parsed)
}

fn flush_loose_ltx_blocks(
    blocks: &mut Vec<String>,
    is_intro_before_sections: bool,
    out: &mut Vec<ParsedSection>,
) {
    if blocks.is_empty() {
        return;
    }
    let text = blocks.join("\n\n");
    blocks.clear();
    if text.trim().is_empty() {
        return;
    }
    let heading = if is_intro_before_sections {
        "Introduction".to_owned()
    } else {
        "Body".to_owned()
    };
    out.push(ParsedSection {
        heading,
        text,
        subsections: Vec::new(),
    });
}

/// Parse a generic open-access HTML5 article (e.g. publisher OA HTML page) into sections.
pub fn parse_generic_html5(html: &str) -> Result<ParsedDocument, FsmError> {
    if looks_like_paywall_or_stub(html) {
        return Err(FsmError::upstream(
            "html5",
            None,
            "HTML appears to be a paywall or challenge page",
        ));
    }
    if exceeds_dom_safety_limits(html) {
        let flat = strip_html_tags_flat(html);
        if looks_like_paywall_or_stub(&flat) {
            return Err(FsmError::upstream(
                "html5",
                None,
                "HTML content looks like a paywall stub",
            ));
        }
        let parsed = crate::sections::extract_text_sections(&flat, None, None);
        if parsed.is_empty() {
            return Err(FsmError::upstream(
                "html5",
                None,
                "HTML5 document yielded no readable text",
            ));
        }
        return Ok(parsed);
    }
    let doc = Document::from(html);
    strip_noise(&doc);
    normalize_mathml(&doc);

    let title = {
        let h1 = doc.select_single("article h1, main h1, h1, title");
        if h1.exists() {
            let t = collapse_ws(&h1.text());
            if t.is_empty() { None } else { Some(t) }
        } else {
            None
        }
    };

    parse_generic_html5_doc(&doc, title)
}

fn parse_generic_html5_doc(
    doc: &Document,
    title: Option<String>,
) -> Result<ParsedDocument, FsmError> {
    let mut parsed = ParsedDocument {
        title,
        ..ParsedDocument::default()
    };

    let root = {
        let article = doc.select_single("article");
        if article.exists() {
            article
        } else {
            let main = doc.select_single("main");
            if main.exists() {
                main
            } else {
                doc.select_single("body")
            }
        }
    };
    if !root.exists() {
        return Err(FsmError::upstream(
            "html5",
            None,
            "HTML5 document has no body",
        ));
    }

    // Try direct `<section>` children first.
    let direct_sections = root.select("section");
    let mut found_structured = false;
    for sec in direct_sections.iter() {
        if sec.ancestors(None).is("section") {
            continue;
        }
        let heading_el = sec.select_single("h1, h2, h3, h4");
        let heading = if heading_el.exists() {
            let h = collapse_ws(&heading_el.text());
            heading_el.remove();
            if h.is_empty() {
                "Section".to_owned()
            } else {
                h
            }
        } else {
            "Section".to_owned()
        };
        let text = extract_blocks(&sec);
        if !text.is_empty() {
            parsed.sections.push(ParsedSection {
                heading,
                text,
                subsections: Vec::new(),
            });
            found_structured = true;
        }
    }

    if !found_structured {
        // Convert inner HTML via `htmd` and split on headings, falling back to plain text on hostile nesting.
        let inner = root.inner_html();
        let md = if exceeds_dom_safety_limits(inner.as_ref()) {
            root.text().to_string()
        } else {
            htmd::convert(inner.as_ref()).unwrap_or_else(|_| root.text().to_string())
        };
        if looks_like_paywall_or_stub(&md) {
            return Err(FsmError::upstream(
                "html5",
                None,
                "HTML content looks like a paywall stub",
            ));
        }
        parsed = crate::sections::extract_text_sections(&md, parsed.title, None);
    }

    if parsed.is_empty() {
        return Err(FsmError::upstream(
            "html5",
            None,
            "HTML5 document yielded no readable text",
        ));
    }

    Ok(parsed)
}

fn strip_noise(doc: &Document) {
    doc.select(
        "script, style, noscript, nav, header.ltx_page_header, footer, .ltx_page_footer, .ltx_page_navbar, .ar5iv-footer, .ltx_role_newpage, .ltx_dates, .ltx_authors, .ltx_toc, .sr-only, .visually-hidden",
    )
    .remove();
}

/// Replace every `<math>` element's text with its TeX `alttext` or `<annotation>`
/// so equations stay readable instead of collapsing into mangled MathML tokens.
fn normalize_mathml(doc: &Document) {
    for math in doc.select("math").iter() {
        let tex = math
            .attr("alttext")
            .map(|s| collapse_ws(&s))
            .filter(|s| !s.is_empty())
            .or_else(|| {
                let ann =
                    math.select_single("annotation[encoding='application/x-tex'], annotation");
                if ann.exists() {
                    let t = collapse_ws(&ann.text());
                    if t.is_empty() { None } else { Some(t) }
                } else {
                    None
                }
            });
        if let Some(tex) = tex {
            let is_block = math
                .attr("display")
                .is_some_and(|d| d.eq_ignore_ascii_case("block"))
                || math
                    .ancestors(Some(4))
                    .is(".ltx_equation, .ltx_equationgroup");
            let formatted = if is_block {
                format!(" $${tex}$$ ")
            } else {
                format!(" ${tex}$ ")
            };
            math.set_text(&formatted);
        }
    }
}

fn parse_ltx_section(sec: &Selection<'_>) -> Option<ParsedSection> {
    let heading_el = sec.select_single(
        "h2.ltx_title_section, h2.ltx_title_appendix, h2.ltx_title_bibliography, h2.ltx_title, h3.ltx_title, h6.ltx_title",
    );
    let heading = if heading_el.exists() {
        let h = clean_ltx_heading(&heading_el);
        heading_el.remove();
        if h.is_empty() {
            fallback_ltx_heading(sec)
        } else {
            h
        }
    } else {
        fallback_ltx_heading(sec)
    };

    let mut blocks: Vec<String> = Vec::new();
    let mut subsections: Vec<FulltextSection> = Vec::new();

    // Bibliography items
    if sec.has_class("ltx_bibliography") {
        let mut refs = Vec::new();
        for (idx, item) in sec.select("li.ltx_bibitem").iter().enumerate() {
            let t = collapse_ws(&item.text());
            if !t.is_empty() {
                refs.push(format!("{}. {t}", idx + 1));
            }
        }
        let text = if refs.is_empty() {
            extract_blocks(sec)
        } else {
            refs.join("\n")
        };
        if text.is_empty() {
            return None;
        }
        return Some(ParsedSection {
            heading,
            text,
            subsections,
        });
    }

    // Walk direct children to preserve reading order between paragraphs and subsections.
    for child in sec.children().iter() {
        if child.is("section.ltx_subsection, section.ltx_subsubsection") {
            let sub_h_el = child.select_single(
                "h3.ltx_title_subsection, h4.ltx_title_subsubsection, h3.ltx_title, h4.ltx_title, h5.ltx_title",
            );
            let sub_heading = if sub_h_el.exists() {
                let h = clean_ltx_heading(&sub_h_el);
                sub_h_el.remove();
                if h.is_empty() {
                    "Subsection".to_owned()
                } else {
                    h
                }
            } else {
                "Subsection".to_owned()
            };
            let sub_text = extract_blocks(&child);
            if !sub_text.is_empty() {
                blocks.push(format!("### {sub_heading}\n\n{sub_text}"));
                subsections.push(FulltextSection::new(sub_heading, sub_text));
            }
        } else {
            extract_single_block(&child, &mut blocks);
        }
    }

    let text = blocks.join("\n\n");
    if text.is_empty() {
        return None;
    }
    Some(ParsedSection {
        heading,
        text,
        subsections,
    })
}

fn clean_ltx_heading(el: &Selection<'_>) -> String {
    for tag in el.select(".ltx_tag").iter() {
        let t = collapse_ws(&tag.text());
        if !t.is_empty() {
            tag.set_text(&format!("{t} "));
        }
    }
    collapse_ws(&el.text())
}

fn fallback_ltx_heading(sec: &Selection<'_>) -> String {
    if sec.has_class("ltx_bibliography") {
        "References".to_owned()
    } else if sec.has_class("ltx_acknowledgements") {
        "Acknowledgments".to_owned()
    } else if sec.has_class("ltx_appendix") {
        "Appendix".to_owned()
    } else {
        "Section".to_owned()
    }
}

fn extract_blocks(container: &Selection<'_>) -> String {
    let mut blocks = Vec::new();
    for child in container.children().iter() {
        extract_single_block(&child, &mut blocks);
    }
    if blocks.is_empty() {
        let t = collapse_ws(&container.text());
        if !t.is_empty() {
            blocks.push(t);
        }
    }
    blocks.join("\n\n")
}

fn extract_single_block(el: &Selection<'_>, blocks: &mut Vec<String>) {
    if el.is("div.ltx_para, div.ltx_logical-block") {
        for sub in el.children().iter() {
            extract_single_block(&sub, blocks);
        }
        return;
    }
    if el.is("p, blockquote") {
        let t = collapse_ws(&el.text());
        if !t.is_empty() {
            blocks.push(t);
        }
        return;
    }
    if el.is("ul, ol") {
        let mut items = Vec::new();
        for li in el.select("li").iter() {
            let t = collapse_ws(&li.text());
            if !t.is_empty() {
                items.push(format!("- {t}"));
            }
        }
        if !items.is_empty() {
            blocks.push(items.join("\n"));
        }
        return;
    }
    if el.is("figure.ltx_table, table.ltx_tabular, table") {
        if el.is("table.ltx_equation, table.ltx_equationgroup") {
            let t = collapse_ws(&el.text());
            if !t.is_empty() {
                blocks.push(t);
            }
            return;
        }
        let rendered = render_html_table(el);
        if !rendered.is_empty() {
            blocks.push(rendered);
        }
        return;
    }
    if el.is("figure") {
        let cap = el.select_single("figcaption");
        if cap.exists() {
            let t = collapse_ws(&cap.text());
            if !t.is_empty() {
                blocks.push(format!("[Figure: {t}]"));
            }
        }
        return;
    }
    if el.is("section") {
        let h_el = el.select_single("h3, h4, h5, h6");
        let h = if h_el.exists() {
            let s = clean_ltx_heading(&h_el);
            h_el.remove();
            s
        } else {
            String::new()
        };
        let inner = extract_blocks(el);
        if !inner.is_empty() {
            if !h.is_empty() {
                blocks.push(format!("#### {h}\n\n{inner}"));
            } else {
                blocks.push(inner);
            }
        }
        return;
    }
    let t = collapse_ws(&el.text());
    if !t.is_empty() {
        blocks.push(t);
    }
}

fn render_html_table(el: &Selection<'_>) -> String {
    let mut parts = Vec::new();
    let cap = el.select_single("figcaption, caption");
    if cap.exists() {
        let c = collapse_ws(&cap.text());
        if !c.is_empty() {
            parts.push(format!("**{c}**"));
        }
    }
    let mut rows: Vec<Vec<String>> = Vec::new();
    for tr in el.select("tr").iter() {
        let cells: Vec<String> = tr
            .children()
            .iter()
            .filter(|c| c.is("th, td"))
            .map(|c| collapse_ws(&c.text()).replace('|', "\\|"))
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
    parts.join("\n\n")
}
