//! Hostile PDFs (`SPEC.md` §8: giant pages, nested streams, flate bomb,
//! malformed xref, memory bomb; plus deep nesting, an outline cycle and an
//! xref /Prev loop). The files are checked in under
//! `tests/fixtures/hostile/`; regenerate them with
//! `FSM_BLESS_HOSTILE=1 cargo test -p fsm-docs --test hostile_pdfs -- --ignored`.
//!
//! Each must be contained: the call returns within the worker timeout plus
//! slack, never as an `Internal` error, with bounded content, and the
//! worker process is reaped.

mod common;

use common::*;
use fsm_core::DocApi;
use fsm_docs::testing::{ChildSpec, WorkerTranscript, run_child};
use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

const TIMEOUT: Duration = Duration::from_secs(10);

/// What each fixture must produce (observed behaviour, pinned).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Expect {
    /// `Ok`, and the content contains this marker text.
    Text(&'static str),
    /// `Ok` (possibly empty or partial) or one of the contained error kinds.
    Contained,
}

const FIXTURES: &[(&str, Expect)] = &[
    ("giant_pages.pdf", Expect::Text("far away corner")),
    ("nested_streams.pdf", Expect::Text("nested text survives")),
    ("flate_bomb.pdf", Expect::Contained),
    (
        "malformed_xref.pdf",
        Expect::Text("Recovered by xref reconstruction"),
    ),
    ("memory_bomb.pdf", Expect::Contained),
    ("deep_nesting.pdf", Expect::Contained),
    ("outline_cycle.pdf", Expect::Text("cycle outline body")),
    ("xref_prev_loop.pdf", Expect::Text("prev loop text")),
];

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hostile")
}

#[test]
fn hostile_fixtures_are_checked_in() {
    let mut present = 0;
    for (name, _) in FIXTURES {
        let p = dir().join(name);
        let bytes =
            std::fs::read(&p).unwrap_or_else(|e| panic!("missing fixture {}: {e}", p.display()));
        assert!(bytes.starts_with(b"%PDF-"), "{name} is not a PDF");
        assert!(
            bytes.len() < 512 * 1024,
            "{name} is {} bytes; keep fixtures small",
            bytes.len()
        );
        present += 1;
    }
    assert!(present >= 5);
}

#[tokio::test]
async fn hostile_pdfs_are_contained_through_doc_read() {
    let root = dir();
    let mut report = Vec::new();
    for (name, expect) in FIXTURES {
        let mut w = worker_config(&[]);
        w.timeout = TIMEOUT;
        let svc = service_with(config(Some(&root)), Arc::new(FakeNet::default()), w);
        let started = Instant::now();
        let res = svc.read(name, 0, None, &cx()).await;
        let took = started.elapsed();
        report.push(format!(
            "{name}: {took:?} -> {}",
            match &res {
                Ok(s) => format!(
                    "Ok(chars={}, pages={:?}, truncated={})",
                    s.total_chars, s.pages, s.truncated
                ),
                Err(e) => format!(
                    "Err({}: {})",
                    e.kind(),
                    e.to_string().chars().take(160).collect::<String>()
                ),
            }
        ));
        assert!(
            took < TIMEOUT + Duration::from_secs(5),
            "{name} took {took:?}"
        );
        match (&res, expect) {
            (Ok(s), Expect::Text(marker)) => {
                assert!(s.content.contains(marker), "{name}: {}", s.content)
            }
            (Err(e), Expect::Text(_)) => panic!("{name}: expected text, got {e}"),
            (Ok(s), Expect::Contained) => assert!(s.total_chars <= 2_000_000, "{name}"),
            (Err(e), Expect::Contained) => {
                assert!(
                    matches!(e.kind(), "InvalidInput" | "BudgetExceeded" | "Timeout"),
                    "{name}: unexpected {} {e}",
                    e.kind()
                );
                assert!(
                    !e.to_string().contains("thread '"),
                    "{name}: raw runtime noise leaked: {e}"
                );
            }
        }
    }
    eprintln!("{}", report.join("\n"));
}

#[cfg(unix)]
#[tokio::test]
async fn hostile_pdf_workers_are_reaped_and_stdout_stays_protocol_only() {
    for (name, _) in FIXTURES {
        let pdf = std::fs::read(dir().join(name)).unwrap();
        let mut stdin = br#"{"v":1,"maxPages":200,"maxChars":2000000,"maxBytes":25000000,"outline":true,"rlimitDataMb":1024}"#.to_vec();
        stdin.push(b'\n');
        stdin.extend_from_slice(&pdf);
        let spec = ChildSpec {
            program: WORKER.into(),
            args: Vec::<OsString>::new(),
            stdin: Arc::from(stdin),
            timeout: TIMEOUT,
            rss_limit: 512 << 20,
            growth_limit: 256 << 20,
            growth_window: Duration::from_secs(1),
            poll: Duration::from_millis(200),
            max_stdout: 16 << 20,
            done_marker: Some(b"FSM-PDF/1 done "),
            grace: Duration::from_secs(3),
        };
        let run = run_child(&spec, &[], &CancellationToken::new())
            .await
            .unwrap();
        let pid = run.pid.unwrap();
        // SAFETY: signal 0 only checks existence.
        let gone = unsafe { libc::kill(pid as i32, 0) } == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        assert!(gone, "{name}: worker {pid} not reaped");
        let t = WorkerTranscript::from_stdout(&run.stdout);
        let noise = run
            .stdout
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty() && !l.starts_with(b"FSM-PDF/1 "))
            .count();
        eprintln!(
            "{name}: {} in {:?}, peak RSS {} MiB, {} pages, done={}, error={:?}, noise lines={noise}",
            run.describe_exit(),
            run.elapsed,
            run.peak_rss >> 20,
            t.pages.len(),
            t.done.is_some(),
            t.error.as_ref().map(|e| &e.kind)
        );
        assert!(run.elapsed < TIMEOUT + Duration::from_secs(5), "{name}");
    }
}

// ------------------------------------------------------------ generator

fn flate(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

/// zlib(zlib(repeat(unit) up to `total` bytes, after `head`)), streamed.
fn double_flate_repeat(head: &[u8], unit: &[u8], total: usize) -> Vec<u8> {
    let outer = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    let mut inner = flate2::write::ZlibEncoder::new(outer, flate2::Compression::best());
    inner.write_all(head).unwrap();
    let chunk: Vec<u8> = unit.iter().copied().cycle().take(1 << 20).collect();
    let mut written = head.len();
    while written < total {
        let n = chunk.len().min(total - written);
        inner.write_all(&chunk[..n]).unwrap();
        written += n;
    }
    inner.finish().unwrap().finish().unwrap()
}

fn page_pdf(
    content: Vec<u8>,
    extra_resources: &str,
    pages_count: Option<&str>,
    media_box: &str,
) -> Vec<u8> {
    let mut b = PdfBuilder::default();
    let catalog = b.reserve();
    let pages = b.reserve();
    let font = b.add("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>");
    let c = b.add(content);
    let page = b.add(format!(
        "<< /Type /Page /Parent {pages} 0 R /MediaBox {media_box} /Resources << /Font << /F1 {font} 0 R >>{extra_resources} >> /Contents {c} 0 R >>"
    ));
    b.set(
        pages,
        format!(
            "<< /Type /Pages /Kids [{page} 0 R] /Count {} >>",
            pages_count.unwrap_or("1")
        ),
    );
    b.set(catalog, format!("<< /Type /Catalog /Pages {pages} 0 R >>"));
    b.finish(catalog, None)
}

fn generate() -> Vec<(&'static str, Vec<u8>)> {
    let mut out = Vec::new();

    // 1. Giant pages: a page tree that claims 2^31-1 pages and a 1e7-unit page.
    out.push((
        "giant_pages.pdf",
        page_pdf(
            PdfBuilder::stream(
                "",
                b"BT /F1 12 Tf 72 700 Td (far away corner) Tj 9000000 9000000 Td (edge) Tj ET",
            ),
            "",
            Some("2147483647"),
            "[0 0 10000000 10000000]",
        ),
    ));

    // 2. Nested streams: a triple-FlateDecode content stream plus a Form
    //    XObject that draws itself (unbounded recursion if unguarded).
    {
        let mut b = PdfBuilder::default();
        let catalog = b.reserve();
        let pages = b.reserve();
        let font = b.add("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>");
        let form = b.reserve();
        let form_data = b"/X1 Do BT /F1 9 Tf 10 10 Td (loop) Tj ET".to_vec();
        b.set(
            form,
            PdfBuilder::stream(
                &format!(" /Type /XObject /Subtype /Form /BBox [0 0 612 792] /Resources << /XObject << /X1 {form} 0 R >> /Font << /F1 {font} 0 R >> >>"),
                &form_data,
            ),
        );
        let text = b"BT /F1 12 Tf 72 700 Td (nested text survives) Tj ET q /X1 Do Q";
        let packed = flate(&flate(&flate(text)));
        let c = b.add(PdfBuilder::stream(
            " /Filter [/FlateDecode /FlateDecode /FlateDecode]",
            &packed,
        ));
        let page = b.add(format!(
            "<< /Type /Page /Parent {pages} 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 {font} 0 R >> /XObject << /X1 {form} 0 R >> >> /Contents {c} 0 R >>"
        ));
        b.set(
            pages,
            format!("<< /Type /Pages /Kids [{page} 0 R] /Count 1 >>"),
        );
        b.set(catalog, format!("<< /Type /Catalog /Pages {pages} 0 R >>"));
        out.push(("nested_streams.pdf", b.finish(catalog, None)));
    }

    // 3. Flate bomb: 512 MiB of whitespace behind two FlateDecode layers.
    out.push((
        "flate_bomb.pdf",
        page_pdf(
            PdfBuilder::stream(
                " /Filter [/FlateDecode /FlateDecode]",
                &double_flate_repeat(
                    b"BT /F1 12 Tf 72 700 Td (before the bomb) Tj ET\n",
                    b" ",
                    512 << 20,
                ),
            ),
            "",
            None,
            "[0 0 612 792]",
        ),
    ));

    // 4. Malformed xref: every offset and startxref point at garbage.
    {
        let good = simple_pdf(
            &["Recovered by xref reconstruction"],
            Some("Broken xref"),
            &[],
        );
        let text = String::from_utf8_lossy(&good).into_owned();
        let mut bad = String::new();
        let mut in_xref = false;
        for line in text.split_inclusive('\n') {
            if line.starts_with("xref") {
                in_xref = true;
            } else if line.starts_with("trailer") {
                in_xref = false;
            }
            if in_xref && line.ends_with(" 00000 n \n") {
                bad.push_str("0000000007 00000 n \n");
            } else {
                bad.push_str(line);
            }
        }
        let bad = match bad.rfind("startxref\n") {
            Some(i) => format!("{}startxref\n999999999\n%%EOF\n", &bad[..i]),
            None => bad,
        };
        let mut bytes = good[..15].to_vec();
        bytes.extend_from_slice(&bad.as_bytes()[15..]);
        out.push(("malformed_xref.pdf", bytes));
    }

    // 5. Memory bomb: ~200 MB of text-show operators (under pdf_oxide's
    //    256 MB stream cap) behind two FlateDecode layers.
    out.push((
        "memory_bomb.pdf",
        page_pdf(
            PdfBuilder::stream(
                " /Filter [/FlateDecode /FlateDecode]",
                &double_flate_repeat(b"BT /F1 1 Tf 0 0 Td\n", b"(AAAAAAAA) Tj\n", 200 << 20),
            ),
            "",
            None,
            "[0 0 612 792]",
        ),
    ));

    // 6. Deep nesting: a resource value nested 100,000 arrays deep.
    {
        let depth = 100_000;
        let nested = format!(" /Bomb {}{}", "[".repeat(depth), "]".repeat(depth));
        out.push((
            "deep_nesting.pdf",
            page_pdf(
                PdfBuilder::stream("", b"BT /F1 12 Tf 72 700 Td (deep nesting text) Tj ET"),
                &nested,
                None,
                "[0 0 612 792]",
            ),
        ));
    }

    // 7. Outline cycle: a bookmark whose /Next is itself.
    {
        let mut b = PdfBuilder::default();
        let catalog = b.reserve();
        let pages = b.reserve();
        let font = b.add("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>");
        let c = b.add(PdfBuilder::stream(
            "",
            b"BT /F1 12 Tf 72 700 Td (cycle outline body) Tj ET",
        ));
        let page = b.add(format!(
            "<< /Type /Page /Parent {pages} 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 {font} 0 R >> >> /Contents {c} 0 R >>"
        ));
        let outlines = b.reserve();
        let item = b.reserve();
        b.set(item, format!("<< /Title (Loop) /Parent {outlines} 0 R /Dest [{page} 0 R /Fit] /Next {item} 0 R /First {item} 0 R /Count 1 >>"));
        b.set(
            outlines,
            format!("<< /Type /Outlines /First {item} 0 R /Last {item} 0 R /Count 1 >>"),
        );
        b.set(
            pages,
            format!("<< /Type /Pages /Kids [{page} 0 R] /Count 1 >>"),
        );
        b.set(
            catalog,
            format!("<< /Type /Catalog /Pages {pages} 0 R /Outlines {outlines} 0 R >>"),
        );
        out.push(("outline_cycle.pdf", b.finish(catalog, None)));
    }

    // 8. xref /Prev loop: the trailer's previous section is itself.
    {
        let good = simple_pdf(&["prev loop text"], None, &[]);
        let text = String::from_utf8_lossy(&good).into_owned();
        let xref_at = text.rfind("\nxref\n").unwrap() + 1;
        let patched = text.replacen("/Size", &format!("/Prev {xref_at} /Size"), 1);
        let mut bytes = good[..15].to_vec();
        bytes.extend_from_slice(&patched.as_bytes()[15..]);
        out.push(("xref_prev_loop.pdf", bytes));
    }
    out
}

#[test]
#[ignore = "generator: FSM_BLESS_HOSTILE=1 cargo test -p fsm-docs --test hostile_pdfs -- --ignored"]
fn regenerate_hostile_fixtures() {
    if std::env::var_os("FSM_BLESS_HOSTILE").is_none() {
        eprintln!("set FSM_BLESS_HOSTILE=1 to rewrite tests/fixtures/hostile/");
        return;
    }
    std::fs::create_dir_all(dir()).unwrap();
    let files = generate();
    assert_eq!(files.len(), FIXTURES.len());
    for (name, bytes) in files {
        assert!(bytes.starts_with(b"%PDF-"));
        std::fs::write(dir().join(name), &bytes).unwrap();
        eprintln!("wrote {name}: {} bytes", bytes.len());
    }
}
