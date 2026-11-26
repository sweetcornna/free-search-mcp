//! The child side of the isolated PDF worker: reads a [`WorkerRequest`] and
//! the PDF from stdin, extracts text with `pdf_oxide`, and streams protocol
//! lines to stdout. Runs with a cleared environment (the parent passes only
//! `PATH`, `TMPDIR`, `LANG`) and lowers its own `RLIMIT_DATA`.

use super::protocol::{
    DoneMsg, ErrorMsg, MetaMsg, OutlineItemMsg, OutlineMsg, PageMsg, WorkerMsg, WorkerRequest, exit,
};
use std::io::{BufRead, BufReader, Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Duration;

/// Most outline entries reported, and the deepest level walked.
const MAX_OUTLINE_ITEMS: usize = 2000;
const MAX_OUTLINE_DEPTH: usize = 32;
/// Time the outline walk may take after the text is out.
const OUTLINE_BUDGET: Duration = Duration::from_secs(2);
/// Stack for the extraction thread (deep but legitimate nesting).
const EXTRACT_STACK: usize = 64 << 20;

fn emit(out: &mut impl Write, msg: &WorkerMsg) {
    // A closed stdout means the parent is gone; nothing useful remains to do.
    let _ = writeln!(out, "{}", msg.to_line());
    let _ = out.flush();
}

fn fail(out: &mut impl Write, kind: &str, error: impl Into<String>, code: u8) -> ExitCode {
    emit(
        out,
        &WorkerMsg::Error(ErrorMsg {
            kind: kind.to_owned(),
            error: error.into(),
        }),
    );
    ExitCode::from(code)
}

#[cfg(unix)]
fn limit_data_segment(mb: u64) {
    if mb == 0 {
        return;
    }
    let want = libc::rlim_t::try_from(mb.saturating_mul(1 << 20)).unwrap_or(libc::rlim_t::MAX);
    let mut cur = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit/setrlimit only read/write the rlimit struct we own;
    // failure is ignored (best effort per S3; macOS may not enforce it).
    unsafe {
        if libc::getrlimit(libc::RLIMIT_DATA, &mut cur) == 0 {
            let lim = libc::rlimit {
                rlim_cur: want.min(cur.rlim_max),
                rlim_max: cur.rlim_max,
            };
            libc::setrlimit(libc::RLIMIT_DATA, &lim);
        }
    }
}

#[cfg(not(unix))]
fn limit_data_segment(_mb: u64) {}

/// Entry point (`DocService::run_pdf_worker_cli`).
pub(crate) fn worker_main() -> ExitCode {
    let mut out = std::io::stdout().lock();
    let mut input = BufReader::new(std::io::stdin().lock());
    let mut header = Vec::new();
    if let Err(e) = (&mut input).take(64 * 1024).read_until(b'\n', &mut header) {
        return fail(
            &mut out,
            "request",
            format!("cannot read request: {e}"),
            exit::BAD_REQUEST,
        );
    }
    let req: WorkerRequest = match serde_json::from_slice(&header) {
        Ok(r) => r,
        Err(e) => {
            return fail(
                &mut out,
                "request",
                format!("bad request header: {e}"),
                exit::BAD_REQUEST,
            );
        }
    };
    if req.v != 1 {
        return fail(
            &mut out,
            "request",
            format!("unsupported protocol v{}", req.v),
            exit::BAD_REQUEST,
        );
    }
    limit_data_segment(req.rlimit_data_mb);

    let mut pdf = Vec::new();
    if let Err(e) = (&mut input)
        .take(req.max_bytes.saturating_add(1))
        .read_to_end(&mut pdf)
    {
        return fail(
            &mut out,
            "input",
            format!("cannot read PDF: {e}"),
            exit::FAILED,
        );
    }
    if pdf.len() as u64 > req.max_bytes {
        return fail(
            &mut out,
            "input",
            format!("PDF larger than the {}-byte limit", req.max_bytes),
            exit::FAILED,
        );
    }
    drop(out);

    let handle = std::thread::Builder::new()
        .name("pdf-extract".into())
        .stack_size(EXTRACT_STACK)
        .spawn(move || catch_unwind(AssertUnwindSafe(|| extract(pdf, &req))));
    let code = match handle.map(std::thread::JoinHandle::join) {
        Ok(Ok(Ok(code))) => code,
        Ok(Ok(Err(panic))) | Ok(Err(panic)) => {
            let msg = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_owned());
            let mut out = std::io::stdout().lock();
            fail(
                &mut out,
                "panic",
                format!("PDF parser panicked: {msg}"),
                exit::PANIC,
            )
        }
        Err(e) => {
            let mut out = std::io::stdout().lock();
            fail(
                &mut out,
                "panic",
                format!("cannot start extraction thread: {e}"),
                exit::PANIC,
            )
        }
    };
    // The outline thread may still be stuck in a hostile bookmark cycle:
    // leave without waiting for it.
    let _ = std::io::stdout().lock().flush();
    code
}

/// Decode a PDF text string: UTF-16BE/LE with BOM, UTF-8 with BOM, else
/// PDFDocEncoding (Latin-1 with the 0x18–0x1F / 0x80–0x9F specials).
pub(crate) fn decode_pdf_text(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xfe, 0xff]) {
        let units: Vec<u16> = rest
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_be_bytes(*c))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xff, 0xfe]) {
        let units: Vec<u16> = rest
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    const LOW: [char; 8] = ['˘', 'ˇ', 'ˆ', '˙', '˝', '˛', '˚', '˜'];
    const HIGH: [char; 32] = [
        '•', '†', '‡', '…', '—', '–', 'ƒ', '⁄', '‹', '›', '−', '‰', '„', '“', '”', '‘', '’', '‚',
        '™', 'ﬁ', 'ﬂ', 'Ł', 'Œ', 'Š', 'Ÿ', 'Ž', 'ı', 'ł', 'œ', 'š', 'ž', '\u{fffd}',
    ];
    bytes
        .iter()
        .map(|&b| match b {
            0x18..=0x1f => LOW[(b - 0x18) as usize],
            0x80..=0x9f => HIGH[(b - 0x80) as usize],
            0xa0 => '€',
            _ => char::from(b),
        })
        .collect()
}

fn info_title(doc: &pdf_oxide::PdfDocument) -> Option<String> {
    use pdf_oxide::object::Object;
    let resolve = |o: &Object| -> Option<Object> {
        match o {
            Object::Reference(r) => doc.load_object(*r).ok(),
            other => Some(other.clone()),
        }
    };
    let info = resolve(doc.trailer().as_dict()?.get("Info")?)?;
    let title = resolve(info.as_dict()?.get("Title")?)?;
    let text = decode_pdf_text(title.as_string()?);
    let t: String = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(500)
        .collect();
    (!t.is_empty()).then_some(t)
}

fn flatten_outline(items: &[pdf_oxide::OutlineItem], depth: usize, out: &mut Vec<OutlineItemMsg>) {
    for item in items {
        if out.len() >= MAX_OUTLINE_ITEMS {
            return;
        }
        let title: String = item
            .title
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(300)
            .collect();
        let page = match &item.dest {
            Some(pdf_oxide::Destination::PageIndex(i)) => u32::try_from(*i).ok(),
            _ => None,
        };
        if !title.is_empty() {
            out.push(OutlineItemMsg {
                title,
                level: (depth + 1).min(u8::MAX as usize) as u8,
                page,
            });
        }
        if depth + 1 < MAX_OUTLINE_DEPTH {
            flatten_outline(&item.children, depth + 1, out);
        }
    }
}

fn extract(pdf: Vec<u8>, req: &WorkerRequest) -> ExitCode {
    let mut out = std::io::stdout().lock();
    let doc = match pdf_oxide::PdfDocument::from_bytes(pdf) {
        Ok(d) => d,
        Err(pdf_oxide::Error::EncryptedPdf) => {
            return fail(&mut out, "encrypted", "the PDF is encrypted", exit::FAILED);
        }
        Err(e) => {
            return fail(
                &mut out,
                "parse",
                format!("cannot open PDF: {e}"),
                exit::FAILED,
            );
        }
    };
    let total = match doc.page_count() {
        Ok(n) => n,
        Err(pdf_oxide::Error::EncryptedPdf) => {
            return fail(&mut out, "encrypted", "the PDF is encrypted", exit::FAILED);
        }
        Err(e) => {
            return fail(
                &mut out,
                "parse",
                format!("cannot read the page tree: {e}"),
                exit::FAILED,
            );
        }
    };
    emit(
        &mut out,
        &WorkerMsg::Meta(MetaMsg {
            title: info_title(&doc),
            total_pages: total as u64,
        }),
    );

    // Python parity: acc counts each "## Page N\n\n<text>" piece; stop once
    // it reaches max_chars. Pages are also clipped so the assembled text
    // (pieces joined by "\n\n") never passes max_chars by more than nothing.
    let (mut acc, mut assembled, mut emitted) = (0usize, 0usize, 0usize);
    let mut truncated = false;
    let (mut page_errors, mut first_error) = (0u32, None::<String>);
    let plain_opts = pdf_oxide::converters::ConversionOptions::default();
    for i in 0..total {
        if i >= req.max_pages as usize {
            truncated = true;
            break;
        }
        let text = match doc.to_plain_text(i, &plain_opts) {
            Ok(t) if !t.trim().is_empty() => t,
            _ => match doc.extract_text(i) {
                Ok(t) => t,
                Err(e) => {
                    page_errors += 1;
                    first_error.get_or_insert_with(|| format!("page {}: {e}", i + 1));
                    continue;
                }
            },
        };
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let n = i + 1;
        let header = format!("## Page {n}\n\n").chars().count();
        let sep = if emitted > 0 { 2 } else { 0 };
        if emitted > 0 && assembled + sep + header >= req.max_chars {
            truncated = true;
            break;
        }
        let room = req.max_chars.saturating_sub(assembled + sep + header);
        let chars = text.chars().count();
        let (text, clipped) = if chars > room {
            let b = text.char_indices().nth(room).map_or(text.len(), |(b, _)| b);
            (&text[..b], true)
        } else {
            (text, false)
        };
        let len = text.chars().count();
        emit(
            &mut out,
            &WorkerMsg::Page(PageMsg {
                n: n as u32,
                text: text.to_owned(),
            }),
        );
        emitted += 1;
        assembled += sep + header + len;
        acc += header + chars;
        if clipped || acc >= req.max_chars {
            truncated = true;
            break;
        }
    }
    emit(
        &mut out,
        &WorkerMsg::Done(DoneMsg {
            truncated,
            page_errors,
            first_error,
        }),
    );
    drop(out);

    if req.outline {
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("pdf-outline".into())
            .stack_size(EXTRACT_STACK)
            .spawn(move || {
                let mut items = Vec::new();
                if let Ok(Some(outline)) = doc.get_outline() {
                    flatten_outline(&outline, 0, &mut items);
                }
                let _ = tx.send(items);
            });
        if spawned.is_ok()
            && let Ok(items) = rx.recv_timeout(OUTLINE_BUDGET)
        {
            let mut out = std::io::stdout().lock();
            emit(&mut out, &WorkerMsg::Outline(OutlineMsg { items }));
        }
    }
    ExitCode::from(exit::OK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdf_text_strings_decode_all_forms() {
        assert_eq!(decode_pdf_text(b"\xfe\xff\x00H\x00i"), "Hi");
        assert_eq!(decode_pdf_text(b"\xff\xfeH\x00i\x00"), "Hi");
        assert_eq!(decode_pdf_text(b"\xef\xbb\xbfcaf\xc3\xa9"), "café");
        assert_eq!(
            decode_pdf_text(b"caf\xe9 \x8dq\x8e \x84 \x93"),
            "café “q” — ﬁ"
        );
    }
}
