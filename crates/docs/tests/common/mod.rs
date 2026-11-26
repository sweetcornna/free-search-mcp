//! Shared helpers for fsm-docs integration tests: in-process document
//! builders (no binary fixtures except the hostile PDFs), a fake
//! `NetClient`, and service constructors.
#![allow(dead_code)]

use fsm_core::{
    CallCx, Config, FsmError, HttpBytes, HttpRequest, NetClient, NetCx, NullStore, SourceDeps,
    TransportProfile,
};
use fsm_docs::{DocService, PdfWorkerConfig, PdftotextMode};
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{Cursor, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const WORKER: &str = env!("CARGO_BIN_EXE_fsm-docs-pdf-worker");

// ------------------------------------------------------------------- PDF

fn pdf_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '(' | ')' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_ascii() => out.push(c),
            _ => out.push('?'),
        }
    }
    out
}

/// Builds small, valid PDFs with a correct xref table.
#[derive(Default)]
pub struct PdfBuilder {
    objs: Vec<Vec<u8>>,
}

impl PdfBuilder {
    /// Reserve an object number (1-based) to fill later.
    pub fn reserve(&mut self) -> usize {
        self.objs.push(Vec::new());
        self.objs.len()
    }

    pub fn set(&mut self, id: usize, body: impl Into<Vec<u8>>) {
        self.objs[id - 1] = body.into();
    }

    pub fn add(&mut self, body: impl Into<Vec<u8>>) -> usize {
        let id = self.reserve();
        self.set(id, body);
        id
    }

    pub fn stream(dict_extra: &str, data: &[u8]) -> Vec<u8> {
        let mut v = format!("<< /Length {}{dict_extra} >>\nstream\n", data.len()).into_bytes();
        v.extend_from_slice(data);
        v.extend_from_slice(b"\nendstream");
        v
    }

    /// Serialize with `root` (and optional `info`) in the trailer.
    pub fn finish(&self, root: usize, info: Option<usize>) -> Vec<u8> {
        let mut out = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in self.objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            out.extend_from_slice(body);
            out.extend_from_slice(b"\nendobj\n");
        }
        let xref = out.len();
        out.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", self.objs.len() + 1).as_bytes(),
        );
        for off in offsets {
            out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        let info = info.map_or_else(String::new, |i| format!(" /Info {i} 0 R"));
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root {root} 0 R{info} >>\nstartxref\n{xref}\n%%EOF\n",
                self.objs.len() + 1
            )
            .as_bytes(),
        );
        out
    }
}

/// Content stream drawing `lines` with Helvetica.
pub fn text_content(lines: &[&str]) -> Vec<u8> {
    let mut s = String::from("BT /F1 12 Tf 14 TL 72 720 Td\n");
    for (i, l) in lines.iter().enumerate() {
        if i > 0 {
            s.push_str("T*\n");
        }
        s.push_str(&format!("({}) Tj\n", pdf_escape(l)));
    }
    s.push_str("ET");
    s.into_bytes()
}

/// A text PDF: one entry of `pages` per page (lines split on \n), an
/// optional Info title and bookmarks `(title, 0-based page)`.
pub fn simple_pdf(pages: &[&str], title: Option<&str>, bookmarks: &[(&str, usize)]) -> Vec<u8> {
    let mut b = PdfBuilder::default();
    let catalog = b.reserve();
    let pages_id = b.reserve();
    let font = b.add("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>");
    let mut kids = Vec::new();
    for text in pages {
        let lines: Vec<&str> = text.split('\n').collect();
        let content = b.add(PdfBuilder::stream("", &text_content(&lines)));
        let page = b.add(format!(
            "<< /Type /Page /Parent {pages_id} 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 {font} 0 R >> >> /Contents {content} 0 R >>"
        ));
        kids.push(page);
    }
    let kid_refs: Vec<String> = kids.iter().map(|k| format!("{k} 0 R")).collect();
    b.set(
        pages_id,
        format!(
            "<< /Type /Pages /Kids [{}] /Count {} >>",
            kid_refs.join(" "),
            kids.len()
        ),
    );
    let mut outlines = String::new();
    if !bookmarks.is_empty() {
        let root = b.reserve();
        let ids: Vec<usize> = bookmarks.iter().map(|_| b.reserve()).collect();
        for (i, (t, page)) in bookmarks.iter().enumerate() {
            let mut d = format!(
                "<< /Title ({}) /Parent {root} 0 R /Dest [{} 0 R /Fit]",
                pdf_escape(t),
                kids[*page]
            );
            if i > 0 {
                d.push_str(&format!(" /Prev {} 0 R", ids[i - 1]));
            }
            if i + 1 < ids.len() {
                d.push_str(&format!(" /Next {} 0 R", ids[i + 1]));
            }
            d.push_str(" >>");
            b.set(ids[i], d);
        }
        b.set(
            root,
            format!(
                "<< /Type /Outlines /First {} 0 R /Last {} 0 R /Count {} >>",
                ids[0],
                ids[ids.len() - 1],
                ids.len()
            ),
        );
        outlines = format!(" /Outlines {root} 0 R");
    }
    b.set(
        catalog,
        format!("<< /Type /Catalog /Pages {pages_id} 0 R{outlines} >>"),
    );
    let info = title.map(|t| {
        b.add(format!(
            "<< /Title ({}) /Producer (fsm-docs tests) >>",
            pdf_escape(t)
        ))
    });
    b.finish(catalog, info)
}

// ------------------------------------------------------------------ ZIPs

pub fn zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, data) in entries {
        w.start_file(*name, opts).unwrap();
        w.write_all(data).unwrap();
    }
    w.finish().unwrap().into_inner()
}

/// Rewrite members of a zip (`_repack`).
pub fn repack(blob: &[u8], replace: &dyn Fn(&str, Vec<u8>) -> Vec<u8>) -> Vec<u8> {
    let mut src = zip::ZipArchive::new(Cursor::new(blob)).unwrap();
    let mut entries = Vec::new();
    for i in 0..src.len() {
        let mut f = src.by_index(i).unwrap();
        let name = f.name().unwrap().into_owned();
        let mut data = Vec::new();
        std::io::Read::read_to_end(&mut f, &mut data).unwrap();
        let data = replace(&name, data);
        entries.push((name, data));
    }
    let refs: Vec<(&str, &[u8])> = entries
        .iter()
        .map(|(n, d)| (n.as_str(), d.as_slice()))
        .collect();
    zip_bytes(&refs)
}

const CT_DOCX: &str = r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#;

pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// DOCX body element: a paragraph with optional style id.
pub fn docx_p(text: &str, style: Option<&str>) -> String {
    let ppr = style.map_or_else(String::new, |s| {
        format!(r#"<w:pPr><w:pStyle w:val="{s}"/></w:pPr>"#)
    });
    format!(
        r#"<w:p>{ppr}<w:r><w:t xml:space="preserve">{}</w:t></w:r></w:p>"#,
        xml_escape(text)
    )
}

pub fn docx_table(rows: &[&[&str]]) -> String {
    let mut s = String::from("<w:tbl>");
    for r in rows {
        s.push_str("<w:tr>");
        for c in *r {
            s.push_str(&format!("<w:tc>{}</w:tc>", docx_p(c, None)));
        }
        s.push_str("</w:tr>");
    }
    s.push_str("</w:tbl>");
    s
}

/// A DOCX from body XML, styles `(id, name)` and a core title.
pub fn docx(body: &str, styles: &[(&str, &str)], title: Option<&str>) -> Vec<u8> {
    let doc = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{body}</w:body></w:document>"#
    );
    let st: String = styles
        .iter()
        .map(|(id, name)| {
            format!(
                r#"<w:style w:type="paragraph" w:styleId="{id}"><w:name w:val="{name}"/></w:style>"#
            )
        })
        .collect();
    let styles_xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">{st}</w:styles>"#
    );
    let core = title.map(|t| {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>{}</dc:title></cp:coreProperties>"#,
            xml_escape(t)
        )
    });
    let mut entries: Vec<(&str, &[u8])> = vec![
        ("[Content_Types].xml", CT_DOCX.as_bytes()),
        ("word/document.xml", doc.as_bytes()),
        ("word/styles.xml", styles_xml.as_bytes()),
    ];
    if let Some(c) = core.as_deref() {
        entries.push(("docProps/core.xml", c.as_bytes()));
    }
    zip_bytes(&entries)
}

/// One worksheet cell: a number, or an inline string.
pub enum Cell<'a> {
    S(&'a str),
    N(f64),
}

fn col_name(mut c: usize) -> String {
    let mut s = Vec::new();
    loop {
        s.push((b'A' + (c % 26) as u8) as char);
        if c < 26 {
            break;
        }
        c = c / 26 - 1;
    }
    s.iter().rev().collect()
}

/// Sheet XML from `(row, col, cell)` triples (0-based), with an optional
/// declared dimension.
pub fn sheet_xml(cells: &[(usize, usize, Cell<'_>)], dimension: Option<&str>) -> String {
    let mut rows: std::collections::BTreeMap<usize, Vec<String>> = Default::default();
    for (r, c, v) in cells {
        let at = format!("{}{}", col_name(*c), r + 1);
        let cell = match v {
            Cell::S(s) => format!(
                r#"<c r="{at}" t="inlineStr"><is><t>{}</t></is></c>"#,
                xml_escape(s)
            ),
            Cell::N(n) => format!(r#"<c r="{at}"><v>{n}</v></c>"#),
        };
        rows.entry(*r).or_default().push(cell);
    }
    let body: String = rows
        .iter()
        .map(|(r, cs)| format!(r#"<row r="{}">{}</row>"#, r + 1, cs.join("")))
        .collect();
    let dim = dimension.map_or_else(String::new, |d| format!(r#"<dimension ref="{d}"/>"#));
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">{dim}<sheetData>{body}</sheetData></worksheet>"#
    )
}

/// An XLSX with named sheets (each given as sheet XML).
pub fn xlsx(sheets: &[(&str, String)]) -> Vec<u8> {
    let mut ct = String::from(
        r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>"#,
    );
    let mut wb_sheets = String::new();
    let mut rels = String::new();
    for (i, (name, _)) in sheets.iter().enumerate() {
        let n = i + 1;
        ct.push_str(&format!(
            r#"<Override PartName="/xl/worksheets/sheet{n}.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>"#
        ));
        wb_sheets.push_str(&format!(
            r#"<sheet name="{}" sheetId="{n}" r:id="rId{n}"/>"#,
            xml_escape(name)
        ));
        rels.push_str(&format!(
            r#"<Relationship Id="rId{n}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet{n}.xml"/>"#
        ));
    }
    ct.push_str("</Types>");
    let root_rels = r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#;
    let workbook = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets>{wb_sheets}</sheets></workbook>"#
    );
    let wb_rels = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{rels}</Relationships>"#
    );
    let names: Vec<String> = (1..=sheets.len())
        .map(|n| format!("xl/worksheets/sheet{n}.xml"))
        .collect();
    let mut entries: Vec<(&str, &[u8])> = vec![
        ("[Content_Types].xml", ct.as_bytes()),
        ("_rels/.rels", root_rels.as_bytes()),
        ("xl/workbook.xml", workbook.as_bytes()),
        ("xl/_rels/workbook.xml.rels", wb_rels.as_bytes()),
    ];
    for (i, (_, xml)) in sheets.iter().enumerate() {
        entries.push((names[i].as_str(), xml.as_bytes()));
    }
    zip_bytes(&entries)
}

fn pptx_sp(text: &str, body_placeholder: bool) -> String {
    let ph = if body_placeholder {
        r#"<p:ph type="body" idx="1"/>"#
    } else {
        ""
    };
    let paras: String = text
        .split('\n')
        .map(|l| format!("<a:p><a:r><a:t>{}</a:t></a:r></a:p>", xml_escape(l)))
        .collect();
    format!(
        r#"<p:sp><p:nvSpPr><p:cNvPr id="2" name="s"/><p:cNvSpPr/><p:nvPr>{ph}</p:nvPr></p:nvSpPr><p:spPr/><p:txBody><a:bodyPr/>{paras}</p:txBody></p:sp>"#
    )
}

const PML: &str = r#"xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main""#;

/// A PPTX: per slide, its shape texts and optional speaker notes.
pub fn pptx(slides: &[(&[&str], Option<&str>)]) -> Vec<u8> {
    let mut ids = String::new();
    let mut rels = String::new();
    let mut parts: Vec<(String, String)> = Vec::new();
    for (i, (texts, notes)) in slides.iter().enumerate() {
        let n = i + 1;
        ids.push_str(&format!(r#"<p:sldId id="{}" r:id="rId{n}"/>"#, 255 + n));
        rels.push_str(&format!(
            r#"<Relationship Id="rId{n}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide{n}.xml"/>"#
        ));
        let shapes: String = texts.iter().map(|t| pptx_sp(t, false)).collect();
        parts.push((
            format!("ppt/slides/slide{n}.xml"),
            format!(r#"<?xml version="1.0" encoding="UTF-8"?><p:sld {PML}><p:cSld><p:spTree>{shapes}</p:spTree></p:cSld></p:sld>"#),
        ));
        if let Some(note) = notes {
            parts.push((
                format!("ppt/slides/_rels/slide{n}.xml.rels"),
                format!(
                    r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId9" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesSlide" Target="../notesSlides/notesSlide{n}.xml"/></Relationships>"#
                ),
            ));
            parts.push((
                format!("ppt/notesSlides/notesSlide{n}.xml"),
                format!(
                    r#"<?xml version="1.0" encoding="UTF-8"?><p:notes {PML}><p:cSld><p:spTree>{}{}</p:spTree></p:cSld></p:notes>"#,
                    pptx_sp("1", false),
                    pptx_sp(note, true)
                ),
            ));
        }
    }
    parts.push((
        "ppt/presentation.xml".into(),
        format!(r#"<?xml version="1.0" encoding="UTF-8"?><p:presentation {PML}><p:sldIdLst>{ids}</p:sldIdLst></p:presentation>"#),
    ));
    parts.push((
        "ppt/_rels/presentation.xml.rels".into(),
        format!(r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{rels}</Relationships>"#),
    ));
    let refs: Vec<(&str, &[u8])> = parts
        .iter()
        .map(|(n, d)| (n.as_str(), d.as_bytes()))
        .collect();
    zip_bytes(&refs)
}

/// An EPUB with a title and XHTML chapters (spine in the given order).
pub fn epub(title: &str, chapters: &[&str]) -> Vec<u8> {
    let container = r#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#;
    let mut manifest = String::new();
    let mut spine = String::new();
    let mut parts: Vec<(String, String)> = Vec::new();
    for (i, body) in chapters.iter().enumerate() {
        let n = i + 1;
        manifest.push_str(&format!(
            r#"<item id="c{n}" href="text/ch%20{n}.xhtml" media-type="application/xhtml+xml"/>"#
        ));
        spine.push_str(&format!(r#"<itemref idref="c{n}"/>"#));
        parts.push((
            format!("OEBPS/text/ch {n}.xhtml"),
            format!(r#"<?xml version="1.0" encoding="utf-8"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>c{n}</title></head><body>{body}</body></html>"#),
        ));
    }
    let opf = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>{}</dc:title></metadata><manifest>{manifest}</manifest><spine>{spine}</spine></package>"#,
        xml_escape(title)
    );
    let mut entries: Vec<(&str, &[u8])> = vec![
        ("mimetype", b"application/epub+zip"),
        ("META-INF/container.xml", container.as_bytes()),
        ("OEBPS/content.opf", opf.as_bytes()),
    ];
    for (n, d) in &parts {
        entries.push((n.as_str(), d.as_bytes()));
    }
    zip_bytes(&entries)
}

// ------------------------------------------------------------- Fake net

/// A canned response: status, content type, body, truncated flag.
pub type Route = (u16, String, Vec<u8>, bool);

/// Canned responses by URL; refuses private hosts in `check_url_offline`.
#[derive(Default)]
pub struct FakeNet {
    pub routes: Mutex<HashMap<String, Route>>,
    pub sends: AtomicUsize,
    pub checks: AtomicUsize,
    pub last_max_bytes: Mutex<Option<u64>>,
}

impl FakeNet {
    pub fn route(&self, url: &str, status: u16, ctype: &str, body: &[u8]) {
        self.routes.lock().unwrap().insert(
            url.to_owned(),
            (status, ctype.to_owned(), body.to_vec(), false),
        );
    }

    pub fn route_truncated(&self, url: &str, body: &[u8]) {
        self.routes.lock().unwrap().insert(
            url.to_owned(),
            (200, "application/pdf".into(), body.to_vec(), true),
        );
    }
}

#[async_trait::async_trait]
impl NetClient for FakeNet {
    async fn send(
        &self,
        req: HttpRequest,
        _p: TransportProfile,
        _cx: &NetCx,
    ) -> Result<HttpBytes, FsmError> {
        self.sends.fetch_add(1, Ordering::SeqCst);
        *self.last_max_bytes.lock().unwrap() = req.max_bytes;
        let routes = self.routes.lock().unwrap();
        let Some((status, ctype, body, truncated)) = routes.get(&req.url) else {
            return Err(FsmError::upstream(
                "fake",
                Some(404),
                format!("no route for {}", req.url),
            ));
        };
        let mut resp = HttpBytes::new(
            req.url.clone(),
            *status,
            vec![("Content-Type".into(), ctype.clone())],
            body.clone(),
        );
        resp.truncated = *truncated;
        Ok(resp)
    }

    fn check_url_offline(&self, url: &str) -> Result<(), FsmError> {
        self.checks.fetch_add(1, Ordering::SeqCst);
        for bad in ["//127.", "//10.", "//169.254.", "//localhost"] {
            if url.contains(bad) {
                return Err(FsmError::UnsafeUrl(format!(
                    "{url} resolves to a private address"
                )));
            }
        }
        Ok(())
    }
}

// -------------------------------------------------------------- Services

pub fn config(root: Option<&Path>) -> Config {
    let mut cfg = Config::default();
    cfg.document_root = root.map(Path::to_path_buf);
    cfg
}

/// Worker settings for tests: the test worker binary, no pdftotext.
pub fn worker_config(args: &[&str]) -> PdfWorkerConfig {
    let mut w = PdfWorkerConfig::with_command(WORKER, args.iter().map(OsString::from).collect());
    w.pdftotext = PdftotextMode::Disabled;
    w.timeout = Duration::from_secs(15);
    w
}

pub fn service_with(cfg: Config, net: Arc<FakeNet>, worker: PdfWorkerConfig) -> DocService {
    let deps = SourceDeps::new(net, Arc::new(NullStore::new()), Arc::new(cfg));
    DocService::new(deps).with_pdf_worker(worker)
}

pub fn service(root: Option<&Path>) -> (DocService, Arc<FakeNet>) {
    let net = Arc::new(FakeNet::default());
    (
        service_with(config(root), Arc::clone(&net), worker_config(&[])),
        net,
    )
}

pub fn cx() -> CallCx {
    CallCx::new("t", std::time::Instant::now() + Duration::from_secs(60))
}

/// A sandbox root inside a temp dir.
pub fn sandbox() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}
