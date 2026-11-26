//! `_parse_blob`: dispatch a body to its parser, degrade Office/EPUB/PDF
//! names that actually carry an HTML page (login wall, soft 404) to HTML
//! extraction, and cap the result at `max_document_chars`.

use crate::archive::parse_archive;
use crate::csv_doc::parse_csv;
use crate::decode::decode_body;
use crate::format::{DocFormat, code_lang, ext, source_path, structured_text_lang};
use crate::markdown::{fence_code, heading_sections, html_title, html_to_markdown};
use crate::office::{DocError, OfficeText, parse_docx, parse_pptx, parse_xlsx};
use crate::parsed::{Limits, MAX_SECTIONS, ParsedDoc, TextBuilder, cap_chars};
use crate::pdf::PdfOutcome;
use fsm_core::{DocSection, FsmError, SheetInfo};

/// The error for a format nobody can read.
pub(crate) fn unsupported(source: &str) -> FsmError {
    FsmError::invalid(format!(
        "Unsupported document format for {source:?}. Supported: pdf, docx, xlsx, pptx, epub, csv, \
         archive (zip/tar), code, html, text, markdown."
    ))
}

/// A parser's result before the document-wide character cap.
struct Draft {
    format: DocFormat,
    title: Option<String>,
    text: String,
    pages: Option<u32>,
    truncated: bool,
    sections: Vec<DocSection>,
    sheets: Option<Vec<SheetInfo>>,
}

impl Draft {
    fn new(format: DocFormat, text: String) -> Self {
        Self {
            format,
            title: None,
            text,
            pages: None,
            truncated: false,
            sections: Vec::new(),
            sheets: None,
        }
    }

    /// Apply `max_document_chars`; drop sections the cut left behind.
    fn finish(self, limits: &Limits) -> ParsedDoc {
        let chars = self.text.chars().count();
        let (text, total_chars, cut) = cap_chars(self.text, chars, limits.max_document_chars);
        let mut sections = self.sections;
        sections.retain(|s| s.start_char < total_chars);
        ParsedDoc {
            format: self.format,
            title: self.title.filter(|t| !t.trim().is_empty()),
            text,
            total_chars,
            pages: self.pages,
            truncated: self.truncated || cut,
            sections,
            sheets: self.sheets,
        }
    }
}

fn scanned_sections(text: &str) -> Vec<DocSection> {
    heading_sections(text, MAX_SECTIONS)
        .into_iter()
        .map(|h| DocSection::new(h.title, h.level, h.start_char))
        .collect()
}

fn html_doc(blob: &[u8], ctype: &str, limits: &Limits) -> ParsedDoc {
    let html = decode_body(blob, if ctype.is_empty() { "text/html" } else { ctype });
    let title = html_title(&html);
    let md = html_to_markdown(&html);
    let sections = scanned_sections(&md);
    Draft {
        title,
        sections,
        ..Draft::new(DocFormat::Html, md)
    }
    .finish(limits)
}

fn office_doc(format: DocFormat, o: OfficeText, limits: &Limits) -> ParsedDoc {
    let (text, _, sections, cut) = o.builder.finish(usize::MAX);
    Draft {
        title: o.title,
        pages: o.pages,
        truncated: o.truncated || cut,
        sections,
        sheets: o.sheets,
        ..Draft::new(format, text)
    }
    .finish(limits)
}

/// Parse every format except a real PDF (which needs the worker).
pub(crate) fn parse_sync(
    blob: &[u8],
    ctype: &str,
    source: &str,
    format: DocFormat,
    limits: &Limits,
) -> Result<ParsedDoc, FsmError> {
    let degrade = |format: DocFormat, e: DocError| -> Result<ParsedDoc, FsmError> {
        match e {
            DocError::Budget(m) => Err(FsmError::BudgetExceeded(m)),
            DocError::Rejected(m) => Err(FsmError::invalid(m)),
            DocError::Parse(m) => {
                tracing::info!("{format} parse failed for {source}; falling back to HTML: {m}");
                Ok(html_doc(blob, ctype, limits))
            }
        }
    };
    match format {
        // Only reached for bodies without a %PDF- header: an HTML wall.
        DocFormat::Pdf => Ok(html_doc(blob, ctype, limits)),
        DocFormat::Docx => parse_docx(blob, limits)
            .map(|o| office_doc(format, o, limits))
            .or_else(|e| degrade(format, e)),
        DocFormat::Pptx => parse_pptx(blob, limits)
            .map(|o| office_doc(format, o, limits))
            .or_else(|e| degrade(format, e)),
        DocFormat::Xlsx => parse_xlsx(blob, limits)
            .map(|o| office_doc(format, o, limits))
            .or_else(|e| degrade(format, e)),
        DocFormat::Epub => match crate::epub::parse_epub(blob, limits) {
            Ok(e) => {
                let (text, _, _, cut) = e.builder.finish(usize::MAX);
                let sections = scanned_sections(&text);
                Ok(Draft {
                    title: e.title,
                    truncated: e.truncated || cut,
                    sections,
                    ..Draft::new(format, text)
                }
                .finish(limits))
            }
            Err(e) => degrade(format, e),
        },
        DocFormat::Csv => {
            let (text, truncated, (rows, cols)) = parse_csv(blob, ctype, limits.max_document_chars);
            let name = source_path(source)
                .rsplit(['/', '\\'])
                .next()
                .filter(|n| !n.is_empty())
                .unwrap_or("csv")
                .to_owned();
            let sheets = Some(vec![SheetInfo::new(name, rows, cols)]);
            Ok(Draft {
                truncated,
                sheets,
                ..Draft::new(format, text)
            }
            .finish(limits))
        }
        DocFormat::Archive => {
            let scan_cap = limits.max_response_bytes.saturating_mul(4).max(64 << 20);
            let (text, truncated) = parse_archive(blob, source, scan_cap);
            Ok(Draft {
                truncated,
                ..Draft::new(format, text)
            }
            .finish(limits))
        }
        DocFormat::Code => {
            let lang = code_lang(&ext(source)).unwrap_or_else(|| structured_text_lang(ctype));
            let text = fence_code(&decode_body(blob, ctype), lang);
            Ok(Draft::new(format, text).finish(limits))
        }
        DocFormat::Html => Ok(html_doc(blob, ctype, limits)),
        DocFormat::Markdown => {
            let text = decode_body(blob, ctype);
            let sections = scanned_sections(&text);
            Ok(Draft {
                sections,
                ..Draft::new(format, text)
            }
            .finish(limits))
        }
        DocFormat::Text => Ok(Draft::new(format, decode_body(blob, ctype)).finish(limits)),
        DocFormat::Image => Err(crate::image::image_error(source, blob)),
        DocFormat::Unknown => Err(unsupported(source)),
    }
}

/// Assemble a PDF outcome: `## Page N` pieces joined by blank lines;
/// sections from the bookmarks (anchored at their page) or one per page.
pub(crate) fn pdf_doc(o: PdfOutcome, limits: &Limits) -> ParsedDoc {
    let mut b = TextBuilder::new();
    let mut anchors: Vec<(u32, usize)> = Vec::with_capacity(o.pages.len());
    for p in &o.pages {
        anchors.push((p.n, b.next_offset()));
        b.push(&format!("## Page {}\n\n{}", p.n, p.text));
    }
    let mut added_bookmark = false;
    for item in &o.outline {
        let Some(idx) = item.page else { continue };
        let n = idx.saturating_add(1);
        // The bookmark's page, or the next page that has text.
        if let Some((_, off)) = anchors.iter().find(|(p, _)| *p >= n) {
            b.section_at(&item.title, item.level, *off, Some(n));
            added_bookmark = true;
        }
    }
    if !added_bookmark {
        for (n, off) in &anchors {
            b.section_at(&format!("Page {n}"), 2, *off, Some(*n));
        }
    }
    let (text, _, sections, cut) = b.finish(usize::MAX);
    Draft {
        title: o.title,
        pages: o.total_pages,
        truncated: o.truncated || cut,
        sections,
        ..Draft::new(DocFormat::Pdf, text)
    }
    .finish(limits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdf::protocol::{OutlineItemMsg, PageMsg};

    fn limits() -> Limits {
        Limits {
            max_document_chars: 2_000_000,
            max_response_bytes: 25_000_000,
            max_pdf_pages: 200,
            max_content_chars: 50_000,
        }
    }

    #[test]
    fn test_code_unknown_extension_still_fences_without_a_language() {
        let d = parse_sync(b"data\n", "", "x.unknownext", DocFormat::Code, &limits()).unwrap();
        assert!(d.text.starts_with("```\n"), "{}", d.text);
    }

    #[test]
    fn test_read_doc_code_parser_honours_header_charset() {
        let (gbk, _, _) = encoding_rs::GBK.encode("# 中文注释");
        let d = parse_sync(
            &gbk,
            "text/plain; charset=gbk",
            "a.py",
            DocFormat::Code,
            &limits(),
        )
        .unwrap();
        assert!(d.text.contains("中文注释") && d.text.starts_with("```python"));
    }

    #[test]
    fn test_read_doc_html_parser_honours_header_charset() {
        let (gbk, _, _) = encoding_rs::GBK.encode("<html><body><p>中文正文</p></body></html>");
        let d = parse_sync(
            &gbk,
            "text/html; charset=gbk",
            "a.html",
            DocFormat::Html,
            &limits(),
        )
        .unwrap();
        assert!(d.text.contains("中文正文"));
    }

    #[test]
    fn structured_ctype_fences_api_bodies() {
        let d = parse_sync(
            b"{\"a\":1}",
            "application/ld+json",
            "https://x/api",
            DocFormat::Code,
            &limits(),
        )
        .unwrap();
        assert!(d.text.starts_with("```json\n"));
    }

    #[test]
    fn non_pdf_bytes_behind_a_pdf_name_degrade_to_html() {
        let d = parse_sync(
            b"<html><h1>Please log in</h1></html>",
            "",
            "a.pdf",
            DocFormat::Pdf,
            &limits(),
        )
        .unwrap();
        assert_eq!(d.format, DocFormat::Html);
        assert!(d.text.contains("Please log in"));
    }

    #[test]
    fn documents_are_capped_at_max_document_chars() {
        let mut l = limits();
        l.max_document_chars = 10;
        let d = parse_sync(b"0123456789abcdef", "", "a.txt", DocFormat::Text, &l).unwrap();
        assert_eq!(
            (d.text.as_str(), d.total_chars, d.truncated),
            ("0123456789", 10, true)
        );
    }

    #[test]
    fn pdf_sections_follow_bookmarks_else_pages() {
        let pages = vec![
            PageMsg {
                n: 1,
                text: "one".into(),
            },
            PageMsg {
                n: 3,
                text: "three".into(),
            },
        ];
        let plain = pdf_doc(
            PdfOutcome {
                pages: pages.clone(),
                total_pages: Some(3),
                ..PdfOutcome::default()
            },
            &limits(),
        );
        assert_eq!(plain.text, "## Page 1\n\none\n\n## Page 3\n\nthree");
        let titles: Vec<_> = plain
            .sections
            .iter()
            .map(|s| (s.title.as_str(), s.page))
            .collect();
        assert_eq!(titles, [("Page 1", Some(1)), ("Page 3", Some(3))]);
        let marked = pdf_doc(
            PdfOutcome {
                pages,
                total_pages: Some(3),
                outline: vec![
                    OutlineItemMsg {
                        title: "Intro".into(),
                        level: 1,
                        page: Some(0),
                    },
                    OutlineItemMsg {
                        title: "Blank p2".into(),
                        level: 2,
                        page: Some(1),
                    },
                    OutlineItemMsg {
                        title: "Named".into(),
                        level: 1,
                        page: None,
                    },
                ],
                ..PdfOutcome::default()
            },
            &limits(),
        );
        let got: Vec<_> = marked
            .sections
            .iter()
            .map(|s| (s.title.as_str(), s.level, s.start_char))
            .collect();
        let p3 = marked.text.find("## Page 3").unwrap();
        assert_eq!(
            got,
            [
                ("Intro", 1, 0),
                ("Blank p2", 2, marked.text[..p3].chars().count())
            ]
        );
        assert_eq!(marked.pages, Some(3));

        // When bookmarks exist but none resolve to any extracted page, fall
        // back to per-page sections rather than emitting an empty outline.
        let unresolved = pdf_doc(
            PdfOutcome {
                pages: vec![PageMsg {
                    n: 1,
                    text: "one".into(),
                }],
                total_pages: Some(10),
                outline: vec![
                    OutlineItemMsg {
                        title: "Named".into(),
                        level: 1,
                        page: None,
                    },
                    OutlineItemMsg {
                        title: "Past cap".into(),
                        level: 1,
                        page: Some(5),
                    },
                ],
                ..PdfOutcome::default()
            },
            &limits(),
        );
        assert_eq!(unresolved.sections.len(), 1);
        assert_eq!(unresolved.sections[0].title, "Page 1");
    }
}
