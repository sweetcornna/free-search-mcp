//! The parsed form of a document, shared by `doc.read` (slicing) and
//! `doc.outline`, and the limits every parser honours.

use crate::format::DocFormat;
use fsm_core::{Config, DocSection, SheetInfo};

/// Caps from `Config` (`MAX_DOCUMENT_CHARS`, `MAX_RESPONSE_BYTES`,
/// `MAX_PDF_PAGES`, `MAX_CONTENT_CHARS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limits {
    pub max_document_chars: usize,
    pub max_response_bytes: u64,
    pub max_pdf_pages: u32,
    pub max_content_chars: usize,
}

impl Limits {
    pub(crate) fn from_config(cfg: &Config) -> Self {
        Self {
            max_document_chars: cfg.max_document_chars.max(1),
            max_response_bytes: cfg.max_response_bytes.max(1),
            max_pdf_pages: cfg.max_pdf_pages.max(1),
            max_content_chars: cfg.max_content_chars.max(1),
        }
    }
}

/// Most outline entries kept per document.
pub(crate) const MAX_SECTIONS: usize = 2000;

/// A fully parsed document (capped at `max_document_chars`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ParsedDoc {
    pub format: DocFormat,
    pub title: Option<String>,
    pub text: String,
    pub total_chars: usize,
    pub pages: Option<u32>,
    /// A bomb cap or a structural limit withheld content.
    pub truncated: bool,
    pub sections: Vec<DocSection>,
    pub sheets: Option<Vec<SheetInfo>>,
}

impl ParsedDoc {
    pub(crate) fn approx_bytes(&self) -> usize {
        self.text.len() + self.sections.len() * 64
    }
}

/// Builds the document text from `"\n\n"`-joined parts while tracking char
/// offsets, so outline sections point at the exact place a heading starts.
#[derive(Debug, Default)]
pub(crate) struct TextBuilder {
    text: String,
    chars: usize,
    parts: usize,
    sections: Vec<DocSection>,
}

impl TextBuilder {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Char offset at which the next part will start.
    pub(crate) fn next_offset(&self) -> usize {
        if self.parts == 0 { 0 } else { self.chars + 2 }
    }

    /// Record a section starting at the next part.
    pub(crate) fn section(&mut self, title: &str, level: u8, page: Option<u32>) {
        if self.sections.len() >= MAX_SECTIONS {
            return;
        }
        let mut s = DocSection::new(title.trim(), level.max(1), self.next_offset());
        s.page = page;
        self.sections.push(s);
    }

    /// Record a section at an explicit char offset.
    pub(crate) fn section_at(
        &mut self,
        title: &str,
        level: u8,
        start_char: usize,
        page: Option<u32>,
    ) {
        if self.sections.len() >= MAX_SECTIONS {
            return;
        }
        let mut s = DocSection::new(title.trim(), level.max(1), start_char);
        s.page = page;
        self.sections.push(s);
    }

    pub(crate) fn push(&mut self, part: &str) {
        if self.parts > 0 {
            self.text.push_str("\n\n");
            self.chars += 2;
        }
        self.text.push_str(part);
        self.chars += part.chars().count();
        self.parts += 1;
    }

    /// The text and its sections, capped at `cap` chars (Python's
    /// `full[:cap]`); the flag says whether the cap cut anything.
    pub(crate) fn finish(self, cap: usize) -> (String, usize, Vec<DocSection>, bool) {
        let (text, chars, cut) = cap_chars(self.text, self.chars, cap);
        let mut sections = self.sections;
        sections.retain(|s| s.start_char < chars);
        (text, chars, sections, cut)
    }
}

/// `text[:cap]` in chars: `(text, chars, was_cut)`.
pub(crate) fn cap_chars(mut text: String, chars: usize, cap: usize) -> (String, usize, bool) {
    if chars <= cap {
        return (text, chars, false);
    }
    let b = text.char_indices().nth(cap).map_or(text.len(), |(b, _)| b);
    text.truncate(b);
    (text, cap, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_offsets_point_at_parts() {
        let mut b = TextBuilder::new();
        b.section("A", 1, None);
        b.push("# A");
        b.push("body é");
        b.section("B", 2, Some(3));
        b.push("## B");
        let (text, chars, sections, cut) = b.finish(1000);
        assert!(!cut);
        assert_eq!(chars, text.chars().count());
        for s in &sections {
            let rest: String = text.chars().skip(s.start_char).collect();
            assert!(rest.starts_with('#'), "{rest:?}");
        }
        assert_eq!(sections[1].page, Some(3));
    }

    #[test]
    fn cap_cuts_text_and_drops_sections_past_it() {
        let mut b = TextBuilder::new();
        b.push("aaaa");
        b.section("late", 1, None);
        b.push("# late");
        let (text, chars, sections, cut) = b.finish(5);
        assert!(cut);
        assert_eq!((text.as_str(), chars), ("aaaa\n", 5));
        assert!(sections.is_empty());
    }
}
