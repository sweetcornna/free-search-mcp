//! End-to-end `DocApi` tests ported from the Python oracle
//! (`tests/test_document_formats.py`, `test_documents_pagination.py`,
//! `test_fetch_safety.py`, `test_backend_network_limits.py`,
//! `test_network_resource_followups.py`, `test_smoke.py`), named after the
//! originals, plus the taint and cache invariants of this crate.

mod common;

use common::*;
use fsm_core::{DocApi, FsmError};
use std::sync::Arc;
use std::sync::atomic::Ordering;

async fn read(svc: &fsm_docs::DocService, src: &str) -> Result<fsm_core::DocSlice, FsmError> {
    svc.read(src, 0, None, &cx()).await
}

fn path_str(p: &std::path::Path) -> String {
    p.to_string_lossy().into_owned()
}

// ----------------------------------------------------- sandbox & taint

#[tokio::test]
async fn test_local_read_disabled_by_default() {
    let (svc, _) = service(None);
    let call = cx();
    let err = svc.read("/etc/passwd", 0, None, &call).await.unwrap_err();
    assert_eq!(err.kind(), "PermissionDenied");
    assert!(err.to_string().contains("SEARCH_MCP_DOCUMENT_ROOT"));
    assert!(
        !call.is_tainted(),
        "nothing local can be read without a root"
    );
}

#[tokio::test]
async fn test_read_doc_tool_local_disabled_by_default_raises() {
    let dir = sandbox();
    std::fs::write(dir.path().join("doc.txt"), "data").unwrap();
    let (svc, _) = service(None);
    let err = read(&svc, &path_str(&dir.path().join("doc.txt")))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), "PermissionDenied");
}

/// `test_mcp_boundary.py::test_local_read_refusals_keep_their_message`, at
/// the `DocApi` level (the MCP framing is the host's). The `length: -1`
/// case cannot reach this `usize` API: the host's argument parsing owns it.
#[tokio::test]
async fn test_local_read_refusals_keep_their_message() {
    let dir = sandbox();
    let outside = dir.path().join("outside.txt");
    std::fs::write(&outside, "secret").unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("pic.png"), b"\x89PNG\r\n\x1a\n").unwrap();
    std::fs::write(root.join("blob.xyz"), b"\x00\x01").unwrap();
    std::fs::write(root.join("notes.txt"), "some notes\n").unwrap();
    let outside_s = path_str(&outside);

    let (disabled_svc, _) = service(None);
    let disabled = read(&disabled_svc, &outside_s).await.unwrap_err();
    let (svc, _) = service(Some(&root));
    let cases = [
        (disabled, "Local file reads are disabled"),
        (
            read(&svc, &outside_s).await.unwrap_err(),
            "resolves outside the document_root sandbox",
        ),
        (
            read(&svc, &format!("file://{outside_s}"))
                .await
                .unwrap_err(),
            "file:// URLs are not allowed",
        ),
        (
            read(&svc, "nope.txt").await.unwrap_err(),
            "No such file under SEARCH_MCP_DOCUMENT_ROOT",
        ),
        // 1.0 renames the tool: `fetch` → `web.fetch`.
        (read(&svc, "pic.png").await.unwrap_err(), "is an image"),
        (
            read(&svc, "blob.xyz").await.unwrap_err(),
            "Unsupported document format",
        ),
    ];
    for (err, phrase) in &cases {
        assert!(err.to_string().contains(phrase), "{phrase:?} not in {err}");
    }
    assert!(cases[4].0.to_string().contains("Use `web.fetch"));
}

#[tokio::test]
async fn test_local_read_file_scheme_rejected() {
    let dir = sandbox();
    let (svc, _) = service(Some(dir.path()));
    let err = read(&svc, "file:///etc/passwd").await.unwrap_err();
    assert_eq!(err.kind(), "InvalidInput");
    assert!(err.to_string().contains("file://"));
}

#[tokio::test]
async fn test_local_read_traversal_blocked() {
    let dir = sandbox();
    let root = dir.path().join("sandbox");
    std::fs::create_dir(&root).unwrap();
    let (svc, _) = service(Some(&root));
    for src in ["../../etc/passwd", "/etc/passwd"] {
        let err = read(&svc, src).await.unwrap_err();
        assert_eq!(err.kind(), "PermissionDenied", "{src}: {err}");
    }
}

#[tokio::test]
async fn test_local_read_inside_root_succeeds() {
    let dir = sandbox();
    let root = dir.path().join("sandbox");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("note.txt"), "hello sandbox").unwrap();
    let (svc, _) = service(Some(&root));
    let out = read(&svc, &path_str(&root.join("note.txt"))).await.unwrap();
    assert_eq!(out.content, "hello sandbox");
    // Relative paths resolve against the root, not the CWD.
    let out2 = read(&svc, "note.txt").await.unwrap();
    assert_eq!(out2.content, "hello sandbox");
    assert_eq!(out2.format, "text");
}

#[tokio::test]
async fn local_reads_taint_before_resolution_even_when_missing() {
    let dir = sandbox();
    std::fs::write(dir.path().join("a.txt"), "x").unwrap();
    let (svc, _) = service(Some(dir.path()));
    let ok = cx();
    svc.read("a.txt", 0, None, &ok).await.unwrap();
    assert!(ok.is_tainted(), "a successful local read taints");
    let missing = cx();
    let err = svc
        .read("missing.txt", 0, None, &missing)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), "InvalidInput");
    assert!(
        missing.is_tainted(),
        "a not-found probe still taints (one bit leaked)"
    );
    let outside = cx();
    assert_eq!(
        svc.read("/etc/hosts", 0, None, &outside)
            .await
            .unwrap_err()
            .kind(),
        "PermissionDenied"
    );
    assert!(outside.is_tainted());
    let outline = cx();
    svc.outline("a.txt", &outline).await.unwrap();
    assert!(outline.is_tainted(), "doc.outline taints like doc.read");
    // A cache hit must still taint.
    let again = cx();
    svc.read("a.txt", 0, None, &again).await.unwrap();
    assert!(again.is_tainted());
    // file:// never reaches the disk: no taint.
    let scheme = cx();
    let _ = svc.read("file:///etc/passwd", 0, None, &scheme).await;
    assert!(!scheme.is_tainted());
}

#[tokio::test]
async fn remote_reads_never_taint() {
    let dir = sandbox();
    let (svc, net) = service(Some(dir.path()));
    net.route(
        "https://docs.example/a.txt",
        200,
        "text/plain",
        b"remote body",
    );
    let call = cx();
    let out = svc
        .read("https://docs.example/a.txt", 0, None, &call)
        .await
        .unwrap();
    assert_eq!(out.content, "remote body");
    assert!(!call.is_tainted());
}

#[tokio::test]
async fn test_oversized_local_file_refused_before_read() {
    let dir = sandbox();
    std::fs::write(dir.path().join("big.txt"), vec![b'x'; 100]).unwrap();
    let mut cfg = config(Some(dir.path()));
    cfg.max_response_bytes = 50;
    let svc = service_with(cfg, Arc::new(FakeNet::default()), worker_config(&[]));
    let err = read(&svc, "big.txt").await.unwrap_err();
    assert_eq!(err.kind(), "BudgetExceeded");
    assert!(err.to_string().to_lowercase().contains("limit"));
}

// ------------------------------------------------------------- remote

#[tokio::test]
async fn test_read_remote_blocks_ssrf_before_connect() {
    let (svc, net) = service(None);
    for url in [
        "http://169.254.169.254/",
        "http://127.0.0.1/",
        "http://10.0.0.1/",
    ] {
        let err = read(&svc, url).await.unwrap_err();
        assert_eq!(err.kind(), "UnsafeUrl", "{url}");
    }
    assert_eq!(
        net.sends.load(Ordering::SeqCst),
        0,
        "no request may be sent"
    );
}

#[tokio::test]
async fn remote_cache_hit_still_runs_the_offline_ssrf_check() {
    let (svc, net) = service(None);
    let url = "https://docs.example/paper.txt";
    net.route(
        url,
        200,
        "text/plain; charset=utf-8",
        "page one. ".repeat(10).as_bytes(),
    );
    let a = svc.read(url, 0, Some(10), &cx()).await.unwrap();
    let b = svc
        .read(url, a.returned_chars, Some(10), &cx())
        .await
        .unwrap();
    assert_eq!(
        net.sends.load(Ordering::SeqCst),
        1,
        "pagination is served from the parsed cache"
    );
    assert_eq!(
        net.checks.load(Ordering::SeqCst),
        2,
        "check_url_offline runs before every hit"
    );
    assert_eq!(b.start, 10);
    assert_eq!(*net.last_max_bytes.lock().unwrap(), Some(25_000_000));
}

#[tokio::test]
async fn remote_errors_map_to_upstream_and_byte_cap() {
    let (svc, net) = service(None);
    net.route(
        "https://docs.example/403.pdf",
        403,
        "text/html",
        b"<h1>Forbidden</h1>",
    );
    let err = read(&svc, "https://docs.example/403.pdf")
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            FsmError::Upstream {
                status: Some(403),
                ..
            }
        ),
        "{err:?}"
    );
    net.route_truncated("https://docs.example/huge.pdf", b"%PDF-1.4 ...");
    let err = read(&svc, "https://docs.example/huge.pdf")
        .await
        .unwrap_err();
    assert_eq!(err.kind(), "MaxBytesExceeded");
    let err = read(&svc, "ftp://docs.example/a.pdf").await.unwrap_err();
    assert_eq!(err.kind(), "InvalidInput");
}

#[tokio::test]
async fn remote_content_type_decides_when_the_path_cannot() {
    let (svc, net) = service(None);
    net.route(
        "https://api.example/pkg/json",
        200,
        "application/json",
        br#"{"name":"uv"}"#,
    );
    let out = read(&svc, "https://api.example/pkg/json").await.unwrap();
    assert_eq!(out.format, "code");
    assert!(out.content.starts_with("```json\n"));
    net.route(
        "https://dl.example/get?id=7",
        200,
        "application/octet-stream",
        b"plain words only\n",
    );
    assert_eq!(
        read(&svc, "https://dl.example/get?id=7")
            .await
            .unwrap()
            .format,
        "text"
    );
}

// --------------------------------------------------------- pagination

#[tokio::test]
async fn test_pagination_slices_offset() {
    let dir = sandbox();
    std::fs::write(
        dir.path().join("long.txt"),
        format!("{}{}", "A".repeat(1000), "B".repeat(1000)),
    )
    .unwrap();
    let (svc, _) = service(Some(dir.path()));
    let head = svc.read("long.txt", 0, Some(500), &cx()).await.unwrap();
    assert_eq!(
        (head.returned_chars, head.start, head.total_chars),
        (500, 0, 2000)
    );
    assert!(head.truncated);
    assert_eq!(&head.content[..5], "AAAAA");
    let middle = svc.read("long.txt", 900, Some(200), &cx()).await.unwrap();
    assert_eq!((middle.start, middle.returned_chars), (900, 200));
    assert!(middle.content.starts_with('A') && middle.content.contains('B'));
}

#[tokio::test]
async fn test_pagination_zero_length_returns_to_end() {
    let dir = sandbox();
    std::fs::write(dir.path().join("small.txt"), "hi there").unwrap();
    let (svc, _) = service(Some(dir.path()));
    let out = read(&svc, "small.txt").await.unwrap();
    assert_eq!(
        (
            out.start,
            out.total_chars,
            out.returned_chars,
            out.truncated
        ),
        (0, 8, 8, false)
    );
}

#[tokio::test]
async fn test_tokens_estimated_present() {
    let dir = sandbox();
    std::fs::write(
        dir.path().join("x.txt"),
        "the quick brown fox jumps over the lazy dog",
    )
    .unwrap();
    let (svc, _) = service(Some(dir.path()));
    assert!(read(&svc, "x.txt").await.unwrap().tokens_estimated > 0);
}

#[tokio::test]
async fn test_slice_returned_chars_is_source_consumed() {
    let dir = sandbox();
    let body = format!(
        "{}\n\n{}",
        "alpha beta gamma delta. ".repeat(4),
        "more text here. ".repeat(40)
    );
    std::fs::write(dir.path().join("doc.txt"), &body).unwrap();
    let mut cfg = config(Some(dir.path()));
    cfg.max_content_chars = 120;
    let svc = service_with(cfg, Arc::new(FakeNet::default()), worker_config(&[]));
    let first = read(&svc, "doc.txt").await.unwrap();
    assert!(first.truncated);
    let second = svc
        .read("doc.txt", first.returned_chars, None, &cx())
        .await
        .unwrap();
    let mut displayed = first.content.as_str();
    for marker in ["\n\n[…truncated]", " …"] {
        if let Some(d) = displayed.strip_suffix(marker) {
            displayed = d;
            break;
        }
    }
    let src: Vec<char> = body.chars().collect();
    let upto: String = src[..first.returned_chars].iter().collect();
    assert_eq!(displayed, upto, "marker chars leaked into returned_chars");
    let next: String = src[first.returned_chars..first.returned_chars + 20]
        .iter()
        .collect();
    assert!(second.content.starts_with(&next));
}

#[tokio::test]
async fn test_slice_start_past_eof_reports_clamped_start() {
    let dir = sandbox();
    std::fs::write(dir.path().join("short.txt"), "0123456789").unwrap();
    let (svc, _) = service(Some(dir.path()));
    let out = svc.read("short.txt", 999_999, None, &cx()).await.unwrap();
    assert_eq!(
        (
            out.start,
            out.returned_chars,
            out.content.as_str(),
            out.truncated
        ),
        (10, 0, "", false)
    );
}

#[tokio::test]
async fn test_read_doc_tool_start_past_eof_clamps() {
    let dir = sandbox();
    std::fs::write(dir.path().join("short.txt"), "hello").unwrap();
    let (svc, _) = service(Some(dir.path()));
    let out = svc.read("short.txt", 9999, None, &cx()).await.unwrap();
    assert_eq!(out.returned_chars, 0);
    assert_eq!(out.start, out.total_chars);
    assert!(!out.truncated);
}

#[tokio::test]
async fn test_read_local_text() {
    let dir = sandbox();
    std::fs::write(dir.path().join("hello.txt"), "hello world\n").unwrap();
    let (svc, _) = service(Some(dir.path()));
    let out = read(&svc, &path_str(&dir.path().join("hello.txt")))
        .await
        .unwrap();
    assert!(out.content.contains("hello world"));
    assert_eq!(out.format, "text");
}

#[tokio::test]
async fn local_cache_key_includes_size_and_mtime() {
    let dir = sandbox();
    let p = dir.path().join("live.txt");
    std::fs::write(&p, "version one").unwrap();
    let (svc, _) = service(Some(dir.path()));
    assert_eq!(read(&svc, "live.txt").await.unwrap().content, "version one");
    std::fs::write(&p, "version two, longer").unwrap();
    assert_eq!(
        read(&svc, "live.txt").await.unwrap().content,
        "version two, longer"
    );
}

// ------------------------------------------------------------ formats

#[tokio::test]
async fn test_xlsx_renders_one_table_per_sheet() {
    let dir = sandbox();
    let book = xlsx(&[
        (
            "Alpha",
            sheet_xml(
                &[
                    (0, 0, Cell::S("h1")),
                    (0, 1, Cell::S("h2")),
                    (1, 0, Cell::S("a")),
                    (1, 1, Cell::N(1.0)),
                ],
                Some("A1:B2"),
            ),
        ),
        ("Beta", sheet_xml(&[(0, 0, Cell::S("only"))], None)),
    ]);
    std::fs::write(dir.path().join("b.xlsx"), book).unwrap();
    let (svc, _) = service(Some(dir.path()));
    let out = read(&svc, "b.xlsx").await.unwrap();
    assert_eq!(out.format, "xlsx");
    assert!(
        out.content.contains("## Alpha") && out.content.contains("## Beta"),
        "{}",
        out.content
    );
    assert!(out.content.contains("| h1 | h2 |"));
    assert!(out.content.contains("| a | 1 |"), "{}", out.content);
    let ol = svc.outline("b.xlsx", &cx()).await.unwrap();
    let sheets = ol.sheets.unwrap();
    assert_eq!(
        (sheets[0].name.as_str(), sheets[0].rows, sheets[0].cols),
        ("Alpha", 2, 2)
    );
    assert_eq!(
        ol.sections
            .iter()
            .map(|s| s.title.as_str())
            .collect::<Vec<_>>(),
        ["Alpha", "Beta"]
    );
}

#[tokio::test]
async fn test_xlsx_skips_the_empty_padding_rows_spreadsheets_carry() {
    let dir = sandbox();
    let sheet = sheet_xml(
        &[
            (0, 0, Cell::S("h")),
            (1, 0, Cell::S("  ")),
            (2, 0, Cell::S("v")),
        ],
        None,
    );
    std::fs::write(dir.path().join("pad.xlsx"), xlsx(&[("S", sheet)])).unwrap();
    let (svc, _) = service(Some(dir.path()));
    let out = read(&svc, "pad.xlsx").await.unwrap();
    let rows: Vec<&str> = out.content.lines().filter(|l| l.starts_with('|')).collect();
    assert_eq!(rows, ["| h |", "| --- |", "| v |"]);
}

#[tokio::test]
async fn test_sparse_xlsx_does_not_expand_unbounded_grid() {
    let dir = sandbox();
    // Two cells at opposite corners of the sheet: a dense range would be
    // 16384 × 1048576 cells.
    let sheet = sheet_xml(
        &[(0, 0, Cell::S("one")), (1_048_575, 16_383, Cell::S("far"))],
        Some("A1:XFD1048576"),
    );
    std::fs::write(dir.path().join("sparse.xlsx"), xlsx(&[("S", sheet)])).unwrap();
    let (svc, _) = service(Some(dir.path()));
    let out = read(&svc, "sparse.xlsx").await.unwrap();
    assert!(out.truncated);
    assert!(out.content.contains("| one |") && !out.content.contains("far"));
}

#[tokio::test]
async fn test_clipped_xlsx_columns_do_not_hide_later_sheets() {
    let dir = sandbox();
    let book = xlsx(&[
        (
            "Wide",
            sheet_xml(&[(0, 30, Cell::S("outside visible columns"))], None),
        ),
        (
            "Second",
            sheet_xml(&[(0, 0, Cell::S("second marker"))], None),
        ),
        ("Third", sheet_xml(&[(0, 0, Cell::S("third marker"))], None)),
    ]);
    std::fs::write(dir.path().join("w.xlsx"), book).unwrap();
    let (svc, _) = service(Some(dir.path()));
    let out = read(&svc, "w.xlsx").await.unwrap();
    assert!(out.truncated);
    assert!(out.content.contains("second marker") && out.content.contains("third marker"));
    assert!(!out.content.contains("outside visible"));
}

#[tokio::test]
async fn test_pptx_reports_slide_count_and_notes() {
    let dir = sandbox();
    std::fs::write(
        dir.path().join("d.pptx"),
        pptx(&[
            (&["Title here", "Body text"], Some("remember this")),
            (&[], None),
        ]),
    )
    .unwrap();
    let (svc, _) = service(Some(dir.path()));
    let out = read(&svc, "d.pptx").await.unwrap();
    assert_eq!(out.format, "pptx");
    assert_eq!(out.pages, Some(2));
    assert!(out.content.contains("## Slide 1"));
    assert!(out.content.contains("Title here") && out.content.contains("Body text"));
    assert!(
        out.content.contains("_Speaker notes:_ remember this"),
        "{}",
        out.content
    );
    assert!(
        !out.content.contains("## Slide 2"),
        "empty slides are skipped"
    );
    let ol = svc.outline("d.pptx", &cx()).await.unwrap();
    assert_eq!(ol.sections[0].title, "Slide 1: Title here");
}

#[tokio::test]
async fn docx_paragraphs_headings_tables_and_outline() {
    let dir = sandbox();
    let body = format!(
        "{}{}{}{}",
        docx_p("Intro", Some("Heading1")),
        docx_p("First paragraph & more.", None),
        docx_table(&[&["k", "v"], &["a", "1"]]),
        docx_p("Detail", Some("Heading2")),
    );
    let blob = docx(
        &body,
        &[("Heading1", "heading 1"), ("Heading2", "heading 2")],
        Some("Doc Title"),
    );
    std::fs::write(dir.path().join("r.docx"), blob).unwrap();
    let (svc, _) = service(Some(dir.path()));
    let out = read(&svc, "r.docx").await.unwrap();
    assert_eq!(
        out.content,
        "# Intro\n\nFirst paragraph & more.\n\nk | v\na | 1\n\n## Detail"
    );
    assert_eq!(out.title.as_deref(), Some("Doc Title"));
    let ol = svc.outline("r.docx", &cx()).await.unwrap();
    let got: Vec<(&str, u8)> = ol
        .sections
        .iter()
        .map(|s| (s.title.as_str(), s.level))
        .collect();
    assert_eq!(got, [("Intro", 1), ("Detail", 2)]);
    let at = svc
        .read("r.docx", ol.sections[1].start_char, None, &cx())
        .await
        .unwrap();
    assert!(at.content.starts_with("## Detail"));
}

#[tokio::test]
async fn test_docx_heading_multiplier_is_bounded() {
    let dir = sandbox();
    let blob = docx(
        &docx_p("small", Some("H100000")),
        &[("H100000", "Heading 100000")],
        None,
    );
    std::fs::write(dir.path().join("h.docx"), blob).unwrap();
    let mut cfg = config(Some(dir.path()));
    cfg.max_document_chars = 100;
    let svc = service_with(cfg, Arc::new(FakeNet::default()), worker_config(&[]));
    let out = read(&svc, "h.docx").await.unwrap();
    assert_eq!(out.content, "# small");
}

#[tokio::test]
async fn test_parse_docx_caps_chars() {
    let dir = sandbox();
    let body: String = (0..1000)
        .map(|_| docx_p(&"word ".repeat(50), None))
        .collect();
    std::fs::write(dir.path().join("big.docx"), docx(&body, &[], None)).unwrap();
    let mut cfg = config(Some(dir.path()));
    cfg.max_document_chars = 200;
    let svc = service_with(cfg, Arc::new(FakeNet::default()), worker_config(&[]));
    let out = read(&svc, "big.docx").await.unwrap();
    assert!(out.truncated);
    assert!(out.total_chars <= 200);
}

#[tokio::test]
async fn test_parse_docx_small_not_truncated() {
    let dir = sandbox();
    std::fs::write(
        dir.path().join("s.docx"),
        docx(
            &format!("{}{}", docx_p("hi", None), docx_p("there", None)),
            &[],
            None,
        ),
    )
    .unwrap();
    let (svc, _) = service(Some(dir.path()));
    let out = read(&svc, "s.docx").await.unwrap();
    assert!(!out.truncated);
    assert!(out.content.contains("hi") && out.content.contains("there"));
}

#[tokio::test]
async fn test_docx_expansion_is_refused_before_parser() {
    let dir = sandbox();
    let small = docx(&docx_p("small", None), &[], None);
    let expanded = repack(&small, &|name, data| {
        if name == "word/document.xml" {
            vec![b'x'; 2_000_000]
        } else {
            data
        }
    });
    std::fs::write(dir.path().join("a.docx"), expanded).unwrap();
    let mut cfg = config(Some(dir.path()));
    cfg.max_response_bytes = 1_000_000;
    let svc = service_with(cfg, Arc::new(FakeNet::default()), worker_config(&[]));
    let err = read(&svc, "a.docx").await.unwrap_err();
    assert_eq!(err.kind(), "BudgetExceeded");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("budget") || msg.contains("limit") || msg.contains("expanded"),
        "{msg}"
    );
}

#[tokio::test]
async fn test_zip_expansion_budget_precedes_loader_and_html_fallback() {
    for fmt in ["docx", "xlsx", "pptx", "epub"] {
        for sizes in [&[2048usize][..], &[600, 600]] {
            let dir = sandbox();
            let names: Vec<String> = (0..sizes.len()).map(|i| format!("member{i}.xml")).collect();
            let datas: Vec<Vec<u8>> = sizes.iter().map(|s| vec![b'x'; *s]).collect();
            let entries: Vec<(&str, &[u8])> = names
                .iter()
                .map(String::as_str)
                .zip(datas.iter().map(Vec::as_slice))
                .collect();
            let blob = zip_bytes(&entries);
            assert!(blob.len() < 1024);
            std::fs::write(dir.path().join(format!("test.{fmt}")), blob).unwrap();
            let mut cfg = config(Some(dir.path()));
            cfg.max_response_bytes = 1024;
            let svc = service_with(cfg, Arc::new(FakeNet::default()), worker_config(&[]));
            let err = read(&svc, &format!("test.{fmt}")).await.unwrap_err();
            assert_eq!(err.kind(), "BudgetExceeded", "{fmt} {sizes:?}: {err}");
            assert!(err.to_string().contains("expanded"), "{err}");
        }
    }
}

#[tokio::test]
async fn test_zip_exact_expansion_budget_is_accepted() {
    let dir = sandbox();
    let a = vec![b'x'; 512];
    std::fs::write(
        dir.path().join("t.docx"),
        zip_bytes(&[("member0.xml", &a), ("member1.xml", &a)]),
    )
    .unwrap();
    let mut cfg = config(Some(dir.path()));
    cfg.max_response_bytes = 1024;
    let svc = service_with(cfg, Arc::new(FakeNet::default()), worker_config(&[]));
    // Within budget: no refusal; not a DOCX, so the HTML fallback answers.
    let out = read(&svc, "t.docx").await.unwrap();
    assert_eq!(out.format, "html");
}

#[tokio::test]
async fn lying_central_directory_is_caught_by_actual_inflation() {
    // Declared sizes are small but the real member inflates past the cap.
    let dir = sandbox();
    let book = xlsx(&[(
        "S",
        sheet_xml(&[(0, 0, Cell::S(&"y".repeat(50_000)))], None),
    )]);
    let mut lying = book.clone();
    // Patch every central-directory uncompressed size of the sheet to 10 bytes.
    let sig = [0x50u8, 0x4b, 0x01, 0x02];
    let mut i = 0;
    while let Some(pos) = lying[i..].windows(4).position(|w| w == sig) {
        let at = i + pos;
        lying[at + 24..at + 28].copy_from_slice(&10u32.to_le_bytes());
        i = at + 4;
    }
    std::fs::write(dir.path().join("lie.xlsx"), lying).unwrap();
    let mut cfg = config(Some(dir.path()));
    cfg.max_response_bytes = 20_000;
    let svc = service_with(cfg, Arc::new(FakeNet::default()), worker_config(&[]));
    let res = read(&svc, "lie.xlsx").await;
    match res {
        Err(e) => assert!(matches!(e.kind(), "BudgetExceeded" | "InvalidInput"), "{e}"),
        Ok(out) => assert!(
            !out.content.contains(&"y".repeat(20_001)),
            "inflated past the cap"
        ),
    }
}

#[tokio::test]
async fn epub_chapters_in_spine_order_with_title() {
    let dir = sandbox();
    let book = epub(
        "My Book",
        &[
            "<h1>One</h1><p>first chapter</p>",
            "<h2>Two</h2><p>second <b>bold</b></p>",
        ],
    );
    std::fs::write(dir.path().join("b.epub"), book).unwrap();
    let (svc, _) = service(Some(dir.path()));
    let out = read(&svc, "b.epub").await.unwrap();
    assert_eq!(out.format, "epub");
    assert_eq!(out.title.as_deref(), Some("My Book"));
    let one = out.content.find("# One").unwrap();
    let two = out.content.find("## Two").unwrap();
    assert!(one < two, "{}", out.content);
    assert!(out.content.contains("**bold**"));
    let ol = svc.outline("b.epub", &cx()).await.unwrap();
    assert_eq!(
        ol.sections
            .iter()
            .map(|s| s.title.as_str())
            .collect::<Vec<_>>(),
        ["One", "Two"]
    );
}

#[tokio::test]
async fn test_code_file_round_trips() {
    let dir = sandbox();
    std::fs::write(dir.path().join("m.py"), "def hi():\n    return 1\n").unwrap();
    let (svc, _) = service(Some(dir.path()));
    let out = read(&svc, "m.py").await.unwrap();
    assert_eq!(out.format, "code");
    assert!(out.content.contains("```python"));
}

#[tokio::test]
async fn test_csv_file_round_trips() {
    let dir = sandbox();
    std::fs::write(dir.path().join("t.csv"), "name,qty\nwidget,3\n").unwrap();
    let (svc, _) = service(Some(dir.path()));
    let out = read(&svc, "t.csv").await.unwrap();
    assert_eq!(out.format, "csv");
    assert!(out.content.contains("| name | qty |"));
    let ol = svc.outline("t.csv", &cx()).await.unwrap();
    let s = &ol.sheets.unwrap()[0];
    assert_eq!((s.name.as_str(), s.rows, s.cols), ("t.csv", 2, 2));
}

#[tokio::test]
async fn test_zip_file_round_trips() {
    let dir = sandbox();
    std::fs::write(dir.path().join("a.zip"), zip_bytes(&[("x.txt", b"hello")])).unwrap();
    let (svc, _) = service(Some(dir.path()));
    let out = read(&svc, "a.zip").await.unwrap();
    assert_eq!(out.format, "archive");
    assert!(out.content.contains("x.txt"));
}

#[tokio::test]
async fn test_images_are_redirected_to_fetch() {
    let dir = sandbox();
    let mut png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
    png.extend(4u32.to_be_bytes());
    png.extend(3u32.to_be_bytes());
    png.extend([0u8; 32]);
    std::fs::write(dir.path().join("p.png"), png).unwrap();
    let (svc, _) = service(Some(dir.path()));
    let err = read(&svc, "p.png").await.unwrap_err();
    assert_eq!(err.kind(), "InvalidInput");
    assert!(err.to_string().contains("Use `web.fetch"), "{err}");
    assert!(err.to_string().contains("4×3"), "{err}");
}

#[tokio::test]
async fn test_unsupported_format_lists_what_is_supported() {
    let dir = sandbox();
    std::fs::write(dir.path().join("x.bin"), b"\x00\x01\x02").unwrap();
    let (svc, _) = service(Some(dir.path()));
    let err = read(&svc, "x.bin").await.unwrap_err();
    assert_eq!(err.kind(), "InvalidInput");
    assert!(err.to_string().contains("xlsx"));
}

#[tokio::test]
async fn test_office_container_serving_html_falls_back() {
    let dir = sandbox();
    std::fs::write(
        dir.path().join("wall.xlsx"),
        b"<html><body><h1>Please log in</h1></body></html>",
    )
    .unwrap();
    let (svc, _) = service(Some(dir.path()));
    let out = read(&svc, "wall.xlsx").await.unwrap();
    assert_eq!(out.format, "html");
    assert!(out.content.contains("Please log in"));
}

#[tokio::test]
async fn html_and_markdown_outline_from_headings() {
    let dir = sandbox();
    std::fs::write(dir.path().join("p.html"), "<html><head><title>Page T</title></head><body><h1>Top</h1><p>x</p><h2>Sub</h2></body></html>").unwrap();
    std::fs::write(
        dir.path().join("n.md"),
        "# A\n\ntext\n\n```\n# not\n```\n\n## B\n",
    )
    .unwrap();
    let (svc, _) = service(Some(dir.path()));
    let ol = svc.outline("p.html", &cx()).await.unwrap();
    assert_eq!(ol.title.as_deref(), Some("Page T"));
    assert_eq!(
        ol.sections
            .iter()
            .map(|s| (s.title.as_str(), s.level))
            .collect::<Vec<_>>(),
        [("Top", 1), ("Sub", 2)]
    );
    let md = svc.outline("n.md", &cx()).await.unwrap();
    assert_eq!(md.format, "markdown");
    assert_eq!(
        md.sections
            .iter()
            .map(|s| s.title.as_str())
            .collect::<Vec<_>>(),
        ["A", "B"]
    );
}

// --------------------------------------------------- R3 untrusted XML guards

#[tokio::test]
async fn untrusted_xml_entity_declarations_rejected_across_formats() {
    let dir = sandbox();
    let (svc, _) = service(Some(dir.path()));

    // 1. DOCX with <!ENTITY> in word/document.xml, word/styles.xml, docProps/core.xml.
    let entity_dtd = r#"<!DOCTYPE d [ <!ENTITY a "xxxxxxxxxx"> <!ENTITY b "&a;&a;&a;&a;"> ]>"#;
    let doc_entity = zip_bytes(&[(
        "word/document.xml",
        format!(
            r#"<?xml version="1.0"?>{entity_dtd}<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>&b;</w:t></w:r></w:p></w:body></w:document>"#
        )
        .as_bytes(),
    )]);
    std::fs::write(dir.path().join("ent_doc.docx"), doc_entity).unwrap();
    let err = read(&svc, "ent_doc.docx").await.unwrap_err();
    assert_eq!(err.kind(), "InvalidInput", "{err}");
    assert!(err.to_string().contains("ENTITY"), "{err}");

    let styles_entity = zip_bytes(&[
        (
            "word/document.xml",
            br#"<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>ok</w:t></w:r></w:p></w:body></w:document>"#,
        ),
        (
            "word/styles.xml",
            format!(
                r#"<?xml version="1.0"?>{entity_dtd}<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"/>"#
            )
            .as_bytes(),
        ),
    ]);
    std::fs::write(dir.path().join("ent_styles.docx"), styles_entity).unwrap();
    assert_eq!(
        read(&svc, "ent_styles.docx").await.unwrap_err().kind(),
        "InvalidInput"
    );

    let core_entity = zip_bytes(&[
        (
            "word/document.xml",
            br#"<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>ok</w:t></w:r></w:p></w:body></w:document>"#,
        ),
        (
            "docProps/core.xml",
            format!(
                r#"<?xml version="1.0"?>{entity_dtd}<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>&b;</dc:title></cp:coreProperties>"#
            )
            .as_bytes(),
        ),
    ]);
    std::fs::write(dir.path().join("ent_core.docx"), core_entity).unwrap();
    assert_eq!(
        read(&svc, "ent_core.docx").await.unwrap_err().kind(),
        "InvalidInput"
    );

    // 2. PPTX with <!ENTITY> in slide XML and rels.
    let base_pptx = pptx(&[(&["Slide one"], None)]);
    let ent_pptx = repack(&base_pptx, &|name, data| {
        if name == "ppt/slides/slide1.xml" {
            let s = String::from_utf8(data).unwrap();
            s.replacen(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
                &format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>{entity_dtd}"),
                1,
            )
            .into_bytes()
        } else {
            data
        }
    });
    std::fs::write(dir.path().join("ent.pptx"), ent_pptx).unwrap();
    assert_eq!(
        read(&svc, "ent.pptx").await.unwrap_err().kind(),
        "InvalidInput"
    );

    // 3. XLSX with <!ENTITY> in worksheet XML (UTF-8 and UTF-16LE).
    let base_xlsx = xlsx(&[("S", sheet_xml(&[(0, 0, Cell::S("ok"))], None))]);
    let ent_xlsx = repack(&base_xlsx, &|name, data| {
        if name == "xl/worksheets/sheet1.xml" {
            let s = String::from_utf8(data).unwrap();
            s.replacen(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
                &format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>{entity_dtd}"),
                1,
            )
            .into_bytes()
        } else {
            data
        }
    });
    std::fs::write(dir.path().join("ent.xlsx"), ent_xlsx).unwrap();
    assert_eq!(
        read(&svc, "ent.xlsx").await.unwrap_err().kind(),
        "InvalidInput"
    );

    let ent_xlsx_u16 = repack(&base_xlsx, &|name, data| {
        if name == "xl/worksheets/sheet1.xml" {
            let s = String::from_utf8(data).unwrap().replacen(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
                &format!("<?xml version=\"1.0\" encoding=\"UTF-16\"?>{entity_dtd}"),
                1,
            );
            let mut out = vec![0xffu8, 0xfe];
            for u in s.encode_utf16() {
                out.extend_from_slice(&u.to_le_bytes());
            }
            out
        } else {
            data
        }
    });
    std::fs::write(dir.path().join("ent_u16.xlsx"), ent_xlsx_u16).unwrap();
    assert_eq!(
        read(&svc, "ent_u16.xlsx").await.unwrap_err().kind(),
        "InvalidInput"
    );

    // 4. EPUB with quadratic/billion-laughs <!ENTITY> in container, OPF, and chapter prolog.
    let base_epub = epub("Book", &["<h1>Hi</h1><p>body</p>"]);
    for target in [
        "META-INF/container.xml",
        "OEBPS/content.opf",
        "OEBPS/text/ch 1.xhtml",
    ] {
        let poisoned = repack(&base_epub, &|name, data| {
            if name == target {
                let s = String::from_utf8(data).unwrap();
                let idx = s.find("?>").map_or(0, |i| i + 2);
                format!("{}{entity_dtd}{}", &s[..idx], &s[idx..]).into_bytes()
            } else {
                data
            }
        });
        let fname = format!("ent_{}.epub", target.replace(['/', ' ', '.'], "_"));
        std::fs::write(dir.path().join(&fname), poisoned).unwrap();
        let err = read(&svc, &fname).await.unwrap_err();
        assert_eq!(err.kind(), "InvalidInput", "{target}: {err}");
    }

    // 5. Benign EPUB 2 external <!DOCTYPE> without <!ENTITY> still succeeds.
    let benign_dtd = r#"<!DOCTYPE package PUBLIC "+//ISBN 0-9673008-1-9//DTD OEB 1.2 Package//EN" "http://openebook.org/dtds/oeb-1.2/oebpkg12.dtd">"#;
    let benign_epub = repack(&base_epub, &|name, data| {
        if name == "OEBPS/content.opf" {
            let s = String::from_utf8(data).unwrap();
            let idx = s.find("?>").map_or(0, |i| i + 2);
            format!("{}{benign_dtd}{}", &s[..idx], &s[idx..]).into_bytes()
        } else {
            data
        }
    });
    std::fs::write(dir.path().join("benign_dtd.epub"), benign_epub).unwrap();
    let ok = read(&svc, "benign_dtd.epub").await.unwrap();
    assert_eq!(ok.format, "epub");
    assert!(ok.content.contains("body"));
}

#[tokio::test]
async fn untrusted_xml_oversized_text_and_attributes_rejected() {
    let dir = sandbox();
    let (svc, _) = service(Some(dir.path()));

    // DOCX with attribute > 64 KiB.
    let huge_attr = "a".repeat(70 * 1024);
    let doc_attr = docx(
        &format!(r#"<w:p w:evil="{huge_attr}"><w:r><w:t>hi</w:t></w:r></w:p>"#),
        &[],
        None,
    );
    std::fs::write(dir.path().join("big_attr.docx"), doc_attr).unwrap();
    assert_eq!(
        read(&svc, "big_attr.docx").await.unwrap_err().kind(),
        "BudgetExceeded"
    );

    // DOCX with a single text node > 1 MiB.
    let huge_text = "x".repeat((1 << 20) + 1024);
    let doc_text = docx(&docx_p(&huge_text, None), &[], None);
    std::fs::write(dir.path().join("big_text.docx"), doc_text).unwrap();
    assert_eq!(
        read(&svc, "big_text.docx").await.unwrap_err().kind(),
        "BudgetExceeded"
    );

    // XLSX with an attribute > 64 KiB.
    let base_xlsx = xlsx(&[("S", sheet_xml(&[(0, 0, Cell::S("ok"))], None))]);
    let big_attr_xlsx = repack(&base_xlsx, &|name, data| {
        if name == "xl/worksheets/sheet1.xml" {
            String::from_utf8(data)
                .unwrap()
                .replacen(
                    "<worksheet ",
                    &format!(r#"<worksheet evil="{huge_attr}" "#),
                    1,
                )
                .into_bytes()
        } else {
            data
        }
    });
    std::fs::write(dir.path().join("big_attr.xlsx"), big_attr_xlsx).unwrap();
    assert_eq!(
        read(&svc, "big_attr.xlsx").await.unwrap_err().kind(),
        "BudgetExceeded"
    );

    // EPUB OPF with an attribute > 64 KiB.
    let base_epub = epub("Book", &["<p>ok</p>"]);
    let big_attr_epub = repack(&base_epub, &|name, data| {
        if name == "OEBPS/content.opf" {
            String::from_utf8(data)
                .unwrap()
                .replacen("<package ", &format!(r#"<package evil="{huge_attr}" "#), 1)
                .into_bytes()
        } else {
            data
        }
    });
    std::fs::write(dir.path().join("big_attr.epub"), big_attr_epub).unwrap();
    assert_eq!(
        read(&svc, "big_attr.epub").await.unwrap_err().kind(),
        "BudgetExceeded"
    );
}

#[tokio::test]
async fn docx_cdata_and_giant_table_are_handled_and_bounded() {
    let dir = sandbox();
    let body = format!(
        r#"<w:p><w:r><w:t><![CDATA[hello <cdata>]]></w:t></w:r></w:p>{}"#,
        docx_table(&vec![&["col1", "col2"][..]; 200])
    );
    std::fs::write(dir.path().join("t.docx"), docx(&body, &[], None)).unwrap();
    let mut cfg = config(Some(dir.path()));
    cfg.max_document_chars = 120;
    let svc = service_with(cfg, Arc::new(FakeNet::default()), worker_config(&[]));
    let out = read(&svc, "t.docx").await.unwrap();
    assert!(out.content.contains("hello <cdata>"), "{}", out.content);
    assert!(out.truncated);
    assert!(out.total_chars <= 120);
}
