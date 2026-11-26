//! OOXML containers: the zip budget, a budgeted member reader, and the
//! DOCX / PPTX (streaming `quick-xml`) and XLSX (`calamine` cell stream)
//! parsers. Ports `_check_zip_budget`, `_parse_docx`, `_parse_pptx`,
//! `_parse_xlsx`.

use crate::csv_doc::{MAX_TABLE_COLS, MAX_TABLE_ROWS};
use crate::markdown::rows_to_markdown;
use crate::parsed::{Limits, TextBuilder};
use fsm_core::SheetInfo;
use quick_xml::XmlVersion;
use quick_xml::events::{BytesRef, BytesStart, Event};
use std::cell::Cell;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Cursor, Read};

/// Most members a document or listed archive may have.
pub(crate) const MAX_ARCHIVE_ENTRIES: usize = 2000;
/// Most worksheets rendered.
pub(crate) const MAX_WORKSHEETS: usize = 100;
/// Maximum byte length of a single XML text or CDATA node (R3).
pub(crate) const MAX_XML_TEXT_BYTES: usize = 1 << 20;
/// Maximum byte length of a single XML attribute value (R3).
pub(crate) const MAX_XML_ATTR_BYTES: usize = 64 * 1024;
/// Maximum byte length of a single XML start/empty/doctype event (R3).
pub(crate) const MAX_XML_TAG_BYTES: usize = 256 * 1024;
/// Maximum byte length of XML metadata parts (container/OPF/core/styles/rels).
pub(crate) const MAX_XML_METADATA_BYTES: usize = 2 << 20;

/// Why a container parser stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DocError {
    /// An input, expansion, or XML length budget was exceeded: never degraded to HTML.
    Budget(String),
    /// An untrusted XML security rule was violated (e.g. `<!ENTITY`): never degraded to HTML.
    Rejected(String),
    /// The bytes are not this format: the caller may fall back to HTML.
    Parse(String),
}

impl DocError {
    pub(crate) fn parse(e: impl std::fmt::Display) -> Self {
        Self::Parse(e.to_string())
    }

    pub(crate) fn is_hard(&self) -> bool {
        matches!(self, Self::Budget(_) | Self::Rejected(_))
    }
}

/// True if `bytes` contain an ASCII case-insensitive `<!ENTITY` declaration.
pub(crate) fn contains_entity_decl(bytes: &[u8]) -> bool {
    bytes
        .windows(8)
        .any(|w| w.eq_ignore_ascii_case(b"<!entity"))
}

/// Reject an XML document declaring `<!ENTITY` (R3).
pub(crate) fn reject_entity_decl(bytes: &[u8]) -> DocResult<()> {
    if contains_entity_decl(bytes) {
        return Err(DocError::Rejected(
            "XML document declares forbidden <!ENTITY".to_owned(),
        ));
    }
    Ok(())
}

pub(crate) type DocResult<T> = Result<T, DocError>;

/// `_check_zip_budget`: refuse containers with more than 2000 members or
/// whose declared expanded size passes `max_bytes`, before any parser loads
/// them. Non-ZIP bodies pass (they get the HTML fallback).
pub(crate) fn check_zip_budget(blob: &[u8], max_bytes: u64) -> DocResult<()> {
    let Ok(mut archive) = zip::ZipArchive::new(Cursor::new(blob)) else {
        return Ok(());
    };
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        return Err(DocError::Budget(format!(
            "Document ZIP entry limit exceeded ({} > {MAX_ARCHIVE_ENTRIES} members)",
            archive.len()
        )));
    }
    let mut total = 0u64;
    for i in 0..archive.len() {
        let size = archive.by_index_raw(i).map_err(DocError::parse)?.size();
        total = total.saturating_add(size);
    }
    if total > max_bytes {
        return Err(DocError::Budget(format!(
            "Document ZIP expanded-byte budget exceeded ({total} > {max_bytes} bytes declared)"
        )));
    }
    Ok(())
}

fn expansion_error(max: u64) -> DocError {
    DocError::Budget(format!(
        "Document ZIP expanded-byte budget exceeded (members inflate past {max} bytes)"
    ))
}

/// A reader that fails once more than `left` bytes came through, flagging
/// `hit` so the caller can tell a budget stop from a parse error.
struct Capped<'a, R> {
    inner: R,
    left: &'a Cell<u64>,
    hit: &'a Cell<bool>,
}

impl<R: Read> Read for Capped<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        let left = self.left.get();
        if n as u64 > left {
            self.hit.set(true);
            return Err(std::io::Error::other("expanded-byte budget exceeded"));
        }
        self.left.set(left - n as u64);
        Ok(n)
    }
}

/// A ZIP container whose members are read under one shared budget of
/// ACTUAL decompressed bytes (declared sizes can lie).
pub(crate) struct ZipDoc<'b> {
    archive: zip::ZipArchive<Cursor<&'b [u8]>>,
    names: Vec<String>,
    left: Cell<u64>,
    max: u64,
}

impl<'b> ZipDoc<'b> {
    pub(crate) fn open(blob: &'b [u8], max_bytes: u64) -> DocResult<Self> {
        let archive = zip::ZipArchive::new(Cursor::new(blob)).map_err(DocError::parse)?;
        if archive.len() > MAX_ARCHIVE_ENTRIES {
            return Err(DocError::Budget(format!(
                "Document ZIP entry limit exceeded ({} > {MAX_ARCHIVE_ENTRIES} members)",
                archive.len()
            )));
        }
        let names = archive
            .file_names()
            .filter_map(|n| n.ok().map(|n| n.into_owned()))
            .collect();
        Ok(Self {
            archive,
            names,
            left: Cell::new(max_bytes),
            max: max_bytes,
        })
    }

    /// The stored name of `name` (exact, else ASCII case-insensitive).
    pub(crate) fn find(&self, name: &str) -> Option<String> {
        let name = name.trim_start_matches('/');
        self.names
            .iter()
            .find(|n| *n == name)
            .or_else(|| self.names.iter().find(|n| n.eq_ignore_ascii_case(name)))
            .cloned()
    }

    /// Run `f` over a streaming reader of member `name`; `None` if absent.
    pub(crate) fn with_member<T>(
        &mut self,
        name: &str,
        f: impl FnOnce(&mut dyn BufRead) -> DocResult<T>,
    ) -> DocResult<Option<T>> {
        let Some(stored) = self.find(name) else {
            return Ok(None);
        };
        let hit = Cell::new(false);
        let file = self.archive.by_name(&stored).map_err(DocError::parse)?;
        let mut reader = BufReader::new(Capped {
            inner: file,
            left: &self.left,
            hit: &hit,
        });
        let out = f(&mut reader);
        if hit.get() {
            return Err(expansion_error(self.max));
        }
        out.map(Some)
    }

    /// Read member `name` fully (under the budget).
    pub(crate) fn read(&mut self, name: &str) -> DocResult<Option<Vec<u8>>> {
        self.with_member(name, |r| {
            let mut v = Vec::new();
            r.read_to_end(&mut v).map_err(DocError::parse)?;
            Ok(v)
        })
    }

    /// Inflate every member under the shared budget (catching lying central
    /// directories) AND screen every XML/rels member for R3 rules (`<!ENTITY`,
    /// text/attribute/tag caps) before passing the archive to `calamine`.
    pub(crate) fn verify_and_screen_xml(&mut self) -> DocResult<()> {
        for name in self.names.clone() {
            let lower = name.to_ascii_lowercase();
            let is_xml = lower.ends_with(".xml") || lower.ends_with(".rels");
            self.with_member(&name, |r| {
                if is_xml {
                    screen_xml_member(r)
                } else {
                    std::io::copy(r, &mut std::io::sink()).map_err(DocError::parse)?;
                    Ok(())
                }
            })?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- XML utils

pub(crate) fn local(name: &str) -> &str {
    name.rfind(':').map_or(name, |i| &name[i + 1..])
}

/// Attribute by local name (any prefix). With `prefixed`, only attributes
/// that carry a prefix match (`r:id` but not `id`).
pub(crate) fn attr(e: &BytesStart<'_>, name: &str, prefixed: bool) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        let key: &str = a.key.as_ref();
        let ok = local(key) == name && (!prefixed || key.contains(':'));
        ok.then(|| {
            a.normalized_value(XmlVersion::Implicit1_0)
                .ok()
                .map(|v| v.into_owned())
        })
        .flatten()
    })
}

/// Text of a general reference (`&amp;`, `&#x41;`, ...).
pub(crate) fn resolve_ref(r: &BytesRef<'_>) -> Option<String> {
    if let Ok(Some(c)) = r.resolve_char_ref() {
        return Some(c.to_string());
    }
    let name = r.xml10_content();
    Some(
        match name.as_ref() {
            "amp" => "&",
            "lt" => "<",
            "gt" => ">",
            "quot" => "\"",
            "apos" => "'",
            _ => return None,
        }
        .to_owned(),
    )
}

fn xml_reader(r: &mut dyn BufRead) -> quick_xml::Reader<&mut dyn BufRead> {
    let mut reader = quick_xml::Reader::from_reader(r);
    reader.config_mut().trim_text(false);
    reader
}

/// Enforce R3 on one XML event (reject `<!ENTITY`, cap text/CDATA/attribute/tag lengths).
fn validate_xml_event(ev: &Event<'_>) -> DocResult<()> {
    match ev {
        Event::DocType(dt) => {
            reject_entity_decl(dt.as_ref().as_bytes())?;
            if dt.len() > MAX_XML_TAG_BYTES {
                return Err(DocError::Budget(format!(
                    "XML DOCTYPE exceeded {MAX_XML_TAG_BYTES} byte limit"
                )));
            }
        }
        Event::Start(e) | Event::Empty(e) => {
            if e.len() > MAX_XML_TAG_BYTES {
                return Err(DocError::Budget(format!(
                    "XML tag exceeded {MAX_XML_TAG_BYTES} byte limit"
                )));
            }
            for a in e.attributes() {
                if let Ok(a) = a
                    && a.value.len() > MAX_XML_ATTR_BYTES
                {
                    return Err(DocError::Budget(format!(
                        "XML attribute exceeded {MAX_XML_ATTR_BYTES} byte limit"
                    )));
                }
            }
        }
        Event::Text(t) => {
            reject_entity_decl(t.as_ref().as_bytes())?;
            if t.len() > MAX_XML_TEXT_BYTES {
                return Err(DocError::Budget(format!(
                    "XML text node exceeded {MAX_XML_TEXT_BYTES} byte limit"
                )));
            }
        }
        Event::CData(c) if c.len() > MAX_XML_TEXT_BYTES => {
            return Err(DocError::Budget(format!(
                "XML CDATA node exceeded {MAX_XML_TEXT_BYTES} byte limit"
            )));
        }
        _ => {}
    }
    Ok(())
}

fn map_xml_err(e: quick_xml::Error, buf: &[u8]) -> DocError {
    if contains_entity_decl(buf) {
        DocError::Rejected("XML document declares forbidden <!ENTITY".to_owned())
    } else {
        DocError::parse(e)
    }
}

/// Zero-copy read of the next XML event with R3 checks on both `Ok` and `Err`.
macro_rules! read_xml_event {
    ($reader:expr, $buf:expr) => {{
        $buf.clear();
        match $reader.read_event_into(&mut $buf) {
            Ok(ev) => match validate_xml_event(&ev) {
                Ok(()) => Ok(ev),
                Err(e) => Err(e),
            },
            Err(e) => Err(map_xml_err(e, &$buf)),
        }
    }};
}

fn looks_like_utf16(head: &[u8]) -> bool {
    head.starts_with(&[0xfe, 0xff])
        || head.starts_with(&[0xff, 0xfe])
        || head.starts_with(&[0x00, b'<'])
        || head.starts_with(&[b'<', 0x00])
}

/// Screen a single XML member for R3 (`<!ENTITY`, text/attr/tag caps) while
/// ensuring the whole member is inflated under the caller's byte budget.
fn screen_xml_member(r: &mut dyn BufRead) -> DocResult<()> {
    let head = r.fill_buf().map_err(DocError::parse)?;
    if looks_like_utf16(head) {
        let mut raw = Vec::new();
        r.read_to_end(&mut raw).map_err(DocError::parse)?;
        let decoded = crate::decode::decode_body(&raw, "application/xml");
        reject_entity_decl(decoded.as_bytes())?;
        let mut cur = Cursor::new(decoded.as_bytes());
        return screen_xml_stream(&mut cur);
    }
    screen_xml_stream(r)
}

fn screen_xml_stream(r: &mut dyn BufRead) -> DocResult<()> {
    let mut reader = xml_reader(r);
    let mut buf = Vec::new();
    loop {
        match read_xml_event!(reader, buf) {
            Ok(Event::Eof) => return Ok(()),
            Ok(_) => {}
            Err(e) if e.is_hard() => return Err(e),
            Err(_) => {
                // Non-XML or malformed member (e.g. a non-XLSX zip that will
                // fall back to HTML): still drain the remaining bytes so the
                // actual-inflation budget is verified, and check for <!ENTITY.
                let mut rest = Vec::new();
                let mut chunk = [0u8; 8192];
                let mut tail = Vec::new();
                while let Ok(n) = r.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    rest.clear();
                    rest.extend_from_slice(&tail);
                    rest.extend_from_slice(&chunk[..n]);
                    reject_entity_decl(&rest)?;
                    let keep = rest.len().min(7);
                    tail.clear();
                    tail.extend_from_slice(&rest[rest.len() - keep..]);
                }
                return Ok(());
            }
        }
    }
}

/// `docProps/core.xml` `dc:title`, if any.
pub(crate) fn core_title(zip: &mut ZipDoc<'_>) -> DocResult<Option<String>> {
    let got = zip.with_member("docProps/core.xml", |r| {
        let mut reader = xml_reader(r);
        let mut buf = Vec::new();
        let mut in_title = false;
        let mut title = String::new();
        loop {
            match read_xml_event!(reader, buf) {
                Ok(Event::Start(e)) if local(e.name().as_ref()) == "title" => in_title = true,
                Ok(Event::End(e)) if local(e.name().as_ref()) == "title" => break,
                Ok(Event::Text(t)) if in_title && title.len() < 4096 => {
                    title.push_str(&t.xml10_content());
                }
                Ok(Event::CData(c)) if in_title && title.len() < 4096 => {
                    title.push_str(c.as_ref());
                }
                Ok(Event::GeneralRef(g)) if in_title && title.len() < 4096 => {
                    title.push_str(&resolve_ref(&g).unwrap_or_default());
                }
                Ok(Event::Eof) => break,
                Err(e) if e.is_hard() => return Err(e),
                Err(_) => break,
                _ => {}
            }
        }
        Ok(title.split_whitespace().collect::<Vec<_>>().join(" "))
    })?;
    Ok(got.filter(|t| !t.is_empty()))
}

/// Resolve a relationship target against the directory of its source part.
pub(crate) fn resolve_part(base_dir: &str, target: &str) -> String {
    if let Some(abs) = target.strip_prefix('/') {
        return abs.to_owned();
    }
    let mut parts: Vec<&str> = base_dir.split('/').filter(|p| !p.is_empty()).collect();
    for seg in target.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// Relationship id → (target part, type) for a `.rels` member.
fn read_rels(
    zip: &mut ZipDoc<'_>,
    rels_path: &str,
    base_dir: &str,
) -> DocResult<HashMap<String, (String, String)>> {
    let got = zip.with_member(rels_path, |r| {
        let mut reader = xml_reader(r);
        let mut buf = Vec::new();
        let mut map = HashMap::new();
        loop {
            match read_xml_event!(reader, buf)? {
                Event::Start(e) | Event::Empty(e) if local(e.name().as_ref()) == "Relationship" => {
                    if let (Some(id), Some(target)) =
                        (attr(&e, "Id", false), attr(&e, "Target", false))
                    {
                        let ty = attr(&e, "Type", false).unwrap_or_default();
                        map.insert(id, (resolve_part(base_dir, &target), ty));
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }
        Ok(map)
    })?;
    Ok(got.unwrap_or_default())
}

// --------------------------------------------------------------------- DOCX

/// Parsed DOCX.
pub(crate) struct OfficeText {
    pub title: Option<String>,
    pub builder: TextBuilder,
    pub truncated: bool,
    pub pages: Option<u32>,
    pub sheets: Option<Vec<SheetInfo>>,
}

fn heading_level(style_name: &str) -> Option<u8> {
    let lower = style_name.to_ascii_lowercase();
    let suffix = lower.strip_prefix("heading")?.trim();
    Some(match suffix {
        "1" | "2" | "3" | "4" | "5" | "6" => suffix.parse().unwrap_or(1),
        _ => 1,
    })
}

fn docx_styles(zip: &mut ZipDoc<'_>) -> DocResult<HashMap<String, String>> {
    let got = zip.with_member("word/styles.xml", |r| {
        let mut reader = xml_reader(r);
        let mut buf = Vec::new();
        let mut map = HashMap::new();
        let mut current: Option<String> = None;
        loop {
            match read_xml_event!(reader, buf) {
                Ok(Event::Start(e)) if local(e.name().as_ref()) == "style" => {
                    current = attr(&e, "styleId", false);
                }
                Ok(Event::Start(e) | Event::Empty(e)) if local(e.name().as_ref()) == "name" => {
                    if let (Some(id), Some(name)) = (current.as_ref(), attr(&e, "val", false)) {
                        map.insert(id.clone(), name);
                    }
                }
                Ok(Event::End(e)) if local(e.name().as_ref()) == "style" => current = None,
                Ok(Event::Eof) => break,
                Err(e) if e.is_hard() => return Err(e),
                Err(_) => break,
                _ => {}
            }
        }
        Ok(map)
    })?;
    Ok(got.unwrap_or_default())
}

#[derive(Default)]
struct Para {
    text: String,
    chars: usize,
    style: Option<String>,
}

impl Para {
    fn push_capped(&mut self, s: &str, cap: usize) -> bool {
        if self.chars >= cap {
            return true;
        }
        let room = cap - self.chars;
        let mut taken = 0usize;
        let mut byte_end = s.len();
        for (i, _) in s.char_indices() {
            if taken == room {
                byte_end = i;
                break;
            }
            taken += 1;
        }
        self.text.push_str(&s[..byte_end]);
        self.chars += taken;
        byte_end < s.len()
    }
}

/// `_parse_docx`: paragraphs (headings as `#` lines) and tables (rows of
/// `cell | cell`), in document order, until `max_document_chars`.
pub(crate) fn parse_docx(blob: &[u8], limits: &Limits) -> DocResult<OfficeText> {
    check_zip_budget(blob, limits.max_response_bytes)?;
    let mut zip = ZipDoc::open(blob, limits.max_response_bytes)?;
    if zip.find("word/document.xml").is_none() {
        return Err(DocError::Parse(
            "not a DOCX: word/document.xml is missing".into(),
        ));
    }
    let styles = docx_styles(&mut zip)?;
    let title = core_title(&mut zip)?;
    let max_chars = limits.max_document_chars;
    let mut b = TextBuilder::new();
    let mut acc = 0usize;
    let mut truncated = false;

    zip.with_member("word/document.xml", |r| {
        let mut reader = xml_reader(r);
        let mut buf = Vec::new();
        let mut paras: Vec<Para> = Vec::new();
        let (mut in_t, mut in_run, mut skip, mut tbl) = (false, 0usize, 0usize, 0usize);
        let mut rows: Vec<String> = Vec::new();
        let mut tbl_chars = 0usize;
        let mut row: Vec<String> = Vec::new();
        let mut cell: Vec<String> = Vec::new();
        let emit = |piece: String,
                    heading: Option<(u8, String)>,
                    acc: &mut usize,
                    b: &mut TextBuilder|
         -> bool {
            if let Some((level, t)) = heading {
                b.section(&t, level, None);
            }
            *acc += piece.chars().count();
            b.push(&piece);
            *acc >= max_chars
        };
        loop {
            let ev = read_xml_event!(reader, buf)?;
            if skip > 0 {
                match ev {
                    Event::Start(_) => skip += 1,
                    Event::End(_) => skip -= 1,
                    Event::Eof => break,
                    _ => {}
                }
                continue;
            }
            let para_cap = max_chars.saturating_sub(acc).saturating_add(16);
            match ev {
                Event::Start(e) => match local(e.name().as_ref()) {
                    "Fallback" => skip = 1,
                    "p" => paras.push(Para::default()),
                    "r" => in_run += 1,
                    "t" => in_t = true,
                    "pStyle" => {
                        if let Some(p) = paras.last_mut() {
                            p.style = attr(&e, "val", false);
                        }
                    }
                    "tbl" => {
                        tbl += 1;
                        if tbl == 1 {
                            rows.clear();
                            tbl_chars = 0;
                        }
                    }
                    "tr" if tbl == 1 => row.clear(),
                    "tc" if tbl == 1 => cell.clear(),
                    _ => {}
                },
                Event::Empty(e) => match local(e.name().as_ref()) {
                    "tab" if in_run > 0 => {
                        if let Some(p) = paras.last_mut()
                            && p.push_capped("\t", para_cap)
                        {
                            truncated = true;
                        }
                    }
                    "br" | "cr" if in_run > 0 => {
                        if let Some(p) = paras.last_mut()
                            && p.push_capped("\n", para_cap)
                        {
                            truncated = true;
                        }
                    }
                    "noBreakHyphen" if in_run > 0 => {
                        if let Some(p) = paras.last_mut()
                            && p.push_capped("-", para_cap)
                        {
                            truncated = true;
                        }
                    }
                    "pStyle" => {
                        if let Some(p) = paras.last_mut() {
                            p.style = attr(&e, "val", false);
                        }
                    }
                    _ => {}
                },
                Event::Text(t) if in_t => {
                    if let Some(p) = paras.last_mut()
                        && p.push_capped(&t.xml10_content(), para_cap)
                    {
                        truncated = true;
                    }
                }
                Event::CData(c) if in_t => {
                    if let Some(p) = paras.last_mut()
                        && p.push_capped(c.as_ref(), para_cap)
                    {
                        truncated = true;
                    }
                }
                Event::GeneralRef(g) if in_t => {
                    if let (Some(p), Some(s)) = (paras.last_mut(), resolve_ref(&g))
                        && p.push_capped(&s, para_cap)
                    {
                        truncated = true;
                    }
                }
                Event::End(e) => match local(e.name().as_ref()) {
                    "t" => in_t = false,
                    "r" => in_run = in_run.saturating_sub(1),
                    "p" => {
                        let Some(p) = paras.pop() else { continue };
                        if tbl > 0 {
                            cell.push(p.text);
                            continue;
                        }
                        let text = p.text.trim();
                        if text.is_empty() {
                            continue;
                        }
                        let style = p.style.as_deref().unwrap_or("");
                        let name = styles.get(style).map_or(style, String::as_str);
                        let (piece, heading) = match heading_level(name) {
                            Some(level) => (
                                format!("{} {text}", "#".repeat(level as usize)),
                                Some((level, text.to_owned())),
                            ),
                            None => (text.to_owned(), None),
                        };
                        if emit(piece, heading, &mut acc, &mut b) {
                            truncated = true;
                            break;
                        }
                    }
                    "tc" if tbl == 1 => row.push(cell.join("\n").trim().to_owned()),
                    "tr" if tbl == 1 => {
                        let line = row.join(" | ");
                        tbl_chars += line.chars().count() + 1;
                        rows.push(line);
                        if acc + tbl_chars >= max_chars {
                            let piece = rows.join("\n");
                            rows.clear();
                            emit(piece, None, &mut acc, &mut b);
                            truncated = true;
                            break;
                        }
                    }
                    "tbl" => {
                        if tbl == 1 && !rows.is_empty() {
                            let piece = rows.join("\n");
                            rows.clear();
                            if emit(piece, None, &mut acc, &mut b) {
                                truncated = true;
                                break;
                            }
                        }
                        tbl = tbl.saturating_sub(1);
                    }
                    _ => {}
                },
                Event::Eof => break,
                _ => {}
            }
        }
        Ok(())
    })?;
    Ok(OfficeText {
        title,
        builder: b,
        truncated,
        pages: None,
        sheets: None,
    })
}

// --------------------------------------------------------------------- PPTX

#[derive(Default)]
struct Shape {
    paras: Vec<String>,
    body_placeholder: bool,
}

/// Every text frame of a slide part: `(text, is_body_placeholder)`.
fn slide_shapes(r: &mut dyn BufRead, char_cap: usize) -> DocResult<Vec<(String, bool)>> {
    let mut reader = xml_reader(r);
    let mut buf = Vec::new();
    let mut out = Vec::new();
    let mut shapes: Vec<Shape> = Vec::new();
    let mut para: Option<Para> = None;
    let mut total_chars = 0usize;
    let (mut in_t, mut skip) = (false, 0usize);
    loop {
        let ev = read_xml_event!(reader, buf)?;
        if skip > 0 {
            match ev {
                Event::Start(_) => skip += 1,
                Event::End(_) => skip -= 1,
                Event::Eof => break,
                _ => {}
            }
            continue;
        }
        let room = char_cap.saturating_sub(total_chars);
        match ev {
            Event::Start(e) => match local(e.name().as_ref()) {
                "Fallback" => skip = 1,
                "sp" => shapes.push(Shape::default()),
                "p" if !shapes.is_empty() => para = Some(Para::default()),
                "t" => in_t = true,
                "ph" => mark_body(&e, &mut shapes),
                _ => {}
            },
            Event::Empty(e) => match local(e.name().as_ref()) {
                "br" => {
                    if let Some(p) = para.as_mut() {
                        p.push_capped("\n", room);
                    }
                }
                "ph" => mark_body(&e, &mut shapes),
                "p" if !shapes.is_empty() => {
                    if let Some(s) = shapes.last_mut() {
                        s.paras.push(String::new());
                    }
                }
                _ => {}
            },
            Event::Text(t) if in_t => {
                if let Some(p) = para.as_mut() {
                    p.push_capped(&t.xml10_content(), room);
                }
            }
            Event::CData(c) if in_t => {
                if let Some(p) = para.as_mut() {
                    p.push_capped(c.as_ref(), room);
                }
            }
            Event::GeneralRef(g) if in_t => {
                if let (Some(p), Some(s)) = (para.as_mut(), resolve_ref(&g)) {
                    p.push_capped(&s, room);
                }
            }
            Event::End(e) => match local(e.name().as_ref()) {
                "t" => in_t = false,
                "p" => {
                    if let (Some(p), Some(s)) = (para.take(), shapes.last_mut()) {
                        total_chars = total_chars.saturating_add(p.chars);
                        s.paras.push(p.text);
                    }
                }
                "sp" => {
                    if let Some(s) = shapes.pop() {
                        out.push((s.paras.join("\n").trim().to_owned(), s.body_placeholder));
                    }
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

fn mark_body(e: &BytesStart<'_>, shapes: &mut [Shape]) {
    if attr(e, "type", false).as_deref() == Some("body")
        && let Some(s) = shapes.last_mut()
    {
        s.body_placeholder = true;
    }
}

fn dir_of(part: &str) -> &str {
    part.rfind('/').map_or("", |i| &part[..i])
}

/// `_parse_pptx`: one `## Slide N` section per slide with text, speaker
/// notes appended; `pages` is the slide count.
pub(crate) fn parse_pptx(blob: &[u8], limits: &Limits) -> DocResult<OfficeText> {
    check_zip_budget(blob, limits.max_response_bytes)?;
    let mut zip = ZipDoc::open(blob, limits.max_response_bytes)?;
    let rels = read_rels(&mut zip, "ppt/_rels/presentation.xml.rels", "ppt")?;
    let order = zip
        .with_member("ppt/presentation.xml", |r| {
            let mut reader = xml_reader(r);
            let mut buf = Vec::new();
            let mut ids = Vec::new();
            loop {
                match read_xml_event!(reader, buf)? {
                    Event::Start(e) | Event::Empty(e) if local(e.name().as_ref()) == "sldId" => {
                        if let Some(id) = attr(&e, "id", true) {
                            ids.push(id);
                        }
                    }
                    Event::Eof => break,
                    _ => {}
                }
            }
            Ok(ids)
        })?
        .ok_or_else(|| DocError::Parse("not a PPTX: ppt/presentation.xml is missing".into()))?;
    let title = core_title(&mut zip)?;
    let slides: Vec<String> = order
        .iter()
        .filter_map(|id| rels.get(id).map(|(p, _)| p.clone()))
        .collect();
    let mut b = TextBuilder::new();
    let mut acc = 0usize;
    let mut truncated = false;
    for (i, part) in slides.iter().enumerate() {
        let n = i + 1;
        let cap = limits
            .max_document_chars
            .saturating_sub(acc)
            .saturating_add(64);
        let shapes = zip
            .with_member(part, |r| slide_shapes(r, cap))?
            .unwrap_or_default();
        let mut lines: Vec<String> = shapes
            .into_iter()
            .map(|(t, _)| t)
            .filter(|t| !t.is_empty())
            .collect();
        let slide_rels_path = format!(
            "{}/_rels/{}.rels",
            dir_of(part),
            part.rsplit('/').next().unwrap_or("")
        );
        let slide_rels = read_rels(&mut zip, &slide_rels_path, dir_of(part))?;
        if let Some((notes_part, _)) = slide_rels
            .values()
            .find(|(_, ty)| ty.ends_with("/notesSlide"))
        {
            let notes = zip
                .with_member(notes_part, |r| slide_shapes(r, cap))?
                .unwrap_or_default();
            if let Some((text, _)) = notes.into_iter().find(|(t, body)| *body && !t.is_empty()) {
                lines.push(format!("_Speaker notes:_ {text}"));
            }
        }
        if lines.is_empty() {
            continue;
        }
        let first = lines[0].lines().next().unwrap_or("").trim();
        let label: String = first.chars().take(80).collect();
        b.section(
            &if label.is_empty() {
                format!("Slide {n}")
            } else {
                format!("Slide {n}: {label}")
            },
            2,
            Some(n as u32),
        );
        let body = lines.join("\n\n");
        acc += body.chars().count();
        b.push(&format!("## Slide {n}\n\n{body}"));
        if acc >= limits.max_document_chars {
            truncated = true;
            break;
        }
    }
    Ok(OfficeText {
        title,
        builder: b,
        truncated,
        pages: Some(slides.len() as u32),
        sheets: None,
    })
}

// --------------------------------------------------------------------- XLSX

fn excel_datetime(dt: &calamine::ExcelDateTime) -> String {
    let v = dt.as_f64();
    if dt.is_duration() {
        let total = (v * 86_400.0).round() as i64;
        let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
        return format!("{h}:{m:02}:{s:02}");
    }
    let (y, mo, d, h, mi, s, _) = dt.to_ymd_hms_milli();
    if v < 1.0 {
        // A time-only cell (openpyxl yields datetime.time).
        return format!("{h:02}:{mi:02}:{s:02}");
    }
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

/// `str(value)` as openpyxl (`data_only=True`) would render it.
fn cell_text(v: &calamine::DataRef<'_>) -> String {
    use calamine::DataRef as D;
    match v {
        D::Int(i) => i.to_string(),
        D::Float(f) => {
            if f.fract() == 0.0 && f.abs() < 1e15 {
                format!("{}", *f as i64)
            } else {
                f.to_string()
            }
        }
        D::String(s) => s.clone(),
        D::SharedString(s) => (*s).to_owned(),
        D::Bool(b) => if *b { "True" } else { "False" }.to_owned(),
        D::DateTime(dt) => excel_datetime(dt),
        D::DateTimeIso(s) => s.replacen('T', " ", 1),
        D::DurationIso(s) => s.clone(),
        D::Error(e) => e.to_string(),
        D::Empty => String::new(),
    }
}

/// `_parse_xlsx`: every sheet (up to 100) as `## <name>` + a Markdown table,
/// 10,000 rows in total, 30 columns, blank rows skipped. Cells are streamed
/// (`worksheet_cells_reader`), never materialised as a dense range, so a
/// sparse sheet with far-apart cells cannot allocate its bounding box.
pub(crate) fn parse_xlsx(blob: &[u8], limits: &Limits) -> DocResult<OfficeText> {
    use calamine::Reader;
    check_zip_budget(blob, limits.max_response_bytes)?;
    let title = {
        let mut zip = ZipDoc::open(blob, limits.max_response_bytes)?;
        zip.verify_and_screen_xml()?;
        core_title(&mut zip)?
    };
    let mut wb = calamine::Xlsx::new(Cursor::new(blob)).map_err(DocError::parse)?;
    let names = wb.sheet_names();
    let max_chars = limits.max_document_chars;
    let mut b = TextBuilder::new();
    let mut sheets = Vec::new();
    let mut acc = 0usize;
    let mut truncated = names.len() > MAX_WORKSHEETS;
    let mut rows_left = MAX_TABLE_ROWS;

    'sheets: for name in names.iter().take(MAX_WORKSHEETS) {
        if rows_left == 0 {
            truncated = true;
            break;
        }
        let mut reader = wb.worksheet_cells_reader(name).map_err(DocError::parse)?;
        let dims = reader.dimensions();
        let mut rows: Vec<Vec<String>> = Vec::new();
        let mut current: Option<(u32, Vec<String>)> = None;
        let (mut max_row, mut max_col, mut any) = (0u32, 0u32, false);
        let mut clipped = false;
        let mut stop = false;
        let flush = |cur: Option<(u32, Vec<String>)>,
                     rows: &mut Vec<Vec<String>>,
                     acc: &mut usize|
         -> bool {
            if let Some((_, cells)) = cur
                && cells.iter().any(|c| !c.trim().is_empty())
            {
                *acc +=
                    cells.iter().map(|c| c.chars().count()).sum::<usize>() + 3 * cells.len() + 4;
                rows.push(cells);
                if *acc >= max_chars {
                    return true;
                }
            }
            false
        };
        loop {
            let cell = match reader.next_cell() {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => return Err(DocError::parse(e)),
            };
            let (r, c) = cell.get_position();
            if !any || r > max_row {
                max_row = r;
            }
            max_col = max_col.max(c);
            any = true;
            if r as usize >= rows_left {
                clipped = true;
                continue;
            }
            if c as usize >= MAX_TABLE_COLS {
                clipped = true;
                continue;
            }
            let text = cell_text(cell.get_value());
            if current.as_ref().is_none_or(|(row, _)| *row != r) {
                if flush(current.take(), &mut rows, &mut acc) {
                    stop = true;
                    break;
                }
                current = Some((r, Vec::new()));
            }
            if let Some((_, cells)) = current.as_mut() {
                if cells.len() <= c as usize {
                    cells.resize(c as usize + 1, String::new());
                }
                cells[c as usize] = text;
            }
        }
        if !stop && flush(current.take(), &mut rows, &mut acc) {
            stop = true;
        }
        let seen_rows = if any { max_row as usize + 1 } else { 0 };
        rows_left = rows_left.saturating_sub(seen_rows.min(rows_left));
        // The declared <dimension> is informational only (it can lie); the
        // reported size is never smaller than what the cells showed.
        let (rows_n, cols_n) = if any {
            (
                (dims.end.0 as usize + 1).max(seen_rows),
                (dims.end.1 as usize + 1).max(max_col as usize + 1),
            )
        } else {
            (0, 0)
        };
        sheets.push(SheetInfo::new(name.clone(), rows_n, cols_n));
        if !rows.is_empty() {
            b.section(name, 2, None);
            b.push(&format!(
                "## {name}\n\n{}",
                rows_to_markdown(&rows, MAX_TABLE_COLS)
            ));
        }
        truncated |= clipped || stop;
        if stop || acc >= max_chars {
            truncated = true;
            break 'sheets;
        }
    }
    Ok(OfficeText {
        title,
        builder: b,
        truncated,
        pages: None,
        sheets: Some(sheets),
    })
}
