//! The isolated PDF worker, end to end: real extraction through the worker
//! binary, plus the S3/S11 guarantees (cleared env, piped stdio, RSS kill,
//! wall-clock kill, busy queue, reaping, fallback).

mod common;

use common::*;
use fsm_core::{CallCx, DocApi, FsmError};
use fsm_docs::testing::{ChildSpec, KEPT_ENV, KillReason, WorkerTranscript, run_child};
use fsm_docs::{PdfWorkerConfig, PdftotextMode};
use std::ffi::OsString;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

fn pdf_service(dir: &std::path::Path, worker: PdfWorkerConfig) -> fsm_docs::DocService {
    service_with(config(Some(dir)), Arc::new(FakeNet::default()), worker)
}

fn write_pdf(dir: &std::path::Path, name: &str, bytes: &[u8]) {
    std::fs::write(dir.join(name), bytes).unwrap();
}

/// The child is gone (reaped, not a zombie): signal 0 fails with ESRCH.
#[cfg(unix)]
fn is_reaped(pid: u32) -> bool {
    // SAFETY: kill with signal 0 only checks existence.
    let rc = unsafe { libc::kill(pid as i32, 0) };
    rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

fn spec(args: &[&str], stdin: &[u8], timeout: Duration) -> ChildSpec {
    ChildSpec {
        program: WORKER.into(),
        args: args.iter().map(OsString::from).collect(),
        stdin: Arc::from(stdin.to_vec()),
        timeout,
        rss_limit: 512 << 20,
        growth_limit: 256 << 20,
        growth_window: Duration::from_secs(1),
        poll: Duration::from_millis(200),
        max_stdout: 8 << 20,
        done_marker: None,
        grace: Duration::from_secs(3),
    }
}

#[tokio::test]
async fn worker_extracts_pages_title_and_bookmarks() {
    let dir = sandbox();
    let pdf = simple_pdf(
        &[
            "Alpha page one\nsecond line",
            "Bravo (two) \\ done",
            "Charlie three",
        ],
        Some("Test Title"),
        &[("Chapter A", 0), ("Chapter C", 2)],
    );
    write_pdf(dir.path(), "t.pdf", &pdf);
    let svc = pdf_service(dir.path(), worker_config(&[]));
    let out = svc.read("t.pdf", 0, None, &cx()).await.unwrap();
    assert_eq!(out.format, "pdf");
    assert_eq!(out.pages, Some(3));
    assert_eq!(out.title.as_deref(), Some("Test Title"));
    assert!(
        out.content.starts_with("## Page 1\n\nAlpha page one"),
        "{}",
        out.content
    );
    assert!(
        out.content.contains("## Page 2\n\nBravo (two)"),
        "{}",
        out.content
    );
    assert!(out.content.contains("Charlie three"));
    assert!(!out.truncated);
    let ol = svc.outline("t.pdf", &cx()).await.unwrap();
    let got: Vec<(&str, u8, Option<u32>)> = ol
        .sections
        .iter()
        .map(|s| (s.title.as_str(), s.level, s.page))
        .collect();
    assert_eq!(got, [("Chapter A", 1, Some(1)), ("Chapter C", 1, Some(3))]);
    let at = svc
        .read("t.pdf", ol.sections[1].start_char, Some(30), &cx())
        .await
        .unwrap();
    assert!(at.content.starts_with("## Page 3"), "{}", at.content);
}

#[tokio::test]
async fn test_parse_pdf_caps_pages() {
    let dir = sandbox();
    let texts: Vec<String> = (1..=12).map(|i| format!("page {i} text body")).collect();
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    write_pdf(dir.path(), "many.pdf", &simple_pdf(&refs, None, &[]));
    let mut cfg = config(Some(dir.path()));
    cfg.max_pdf_pages = 10;
    let svc = service_with(cfg, Arc::new(FakeNet::default()), worker_config(&[]));
    let out = svc.read("many.pdf", 0, None, &cx()).await.unwrap();
    assert_eq!(out.pages, Some(12), "reports the true page count");
    assert!(out.truncated);
    assert!(out.content.contains("## Page 10") && !out.content.contains("## Page 11"));
    let ol = svc.outline("many.pdf", &cx()).await.unwrap();
    assert_eq!(
        ol.sections.len(),
        10,
        "one section per extracted page without bookmarks"
    );
}

#[tokio::test]
async fn test_parse_pdf_caps_chars() {
    let dir = sandbox();
    let texts: Vec<String> = (1..=40).map(|i| format!("page {i} text body")).collect();
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    write_pdf(dir.path(), "c.pdf", &simple_pdf(&refs, None, &[]));
    let mut cfg = config(Some(dir.path()));
    cfg.max_document_chars = 50;
    let svc = service_with(cfg, Arc::new(FakeNet::default()), worker_config(&[]));
    let out = svc.read("c.pdf", 0, None, &cx()).await.unwrap();
    assert!(out.truncated);
    assert!(out.total_chars <= 50, "{}", out.total_chars);
}

#[tokio::test]
async fn html_wall_behind_a_pdf_url_never_reaches_the_worker() {
    let dir = sandbox();
    write_pdf(
        dir.path(),
        "wall.pdf",
        b"<html><body><h1>Sign in to download</h1></body></html>",
    );
    // A worker that would hang proves the worker is not used.
    let svc = pdf_service(dir.path(), worker_config(&["--selftest-sleep", "60000"]));
    let started = Instant::now();
    let out = svc.read("wall.pdf", 0, None, &cx()).await.unwrap();
    assert_eq!(out.format, "html");
    assert!(out.content.contains("Sign in to download"));
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[cfg(unix)]
#[tokio::test]
async fn worker_env_is_cleared_and_stdio_is_piped() {
    let run = run_child(
        &spec(&["--selftest-env"], b"", Duration::from_secs(10)),
        &[],
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let t = WorkerTranscript::from_stdout(&run.stdout);
    let report: serde_json::Value =
        serde_json::from_str(&t.error.expect("selftest report").error).unwrap();
    let env: Vec<String> = serde_json::from_value(report["env"].clone()).unwrap();
    assert!(!env.is_empty() || std::env::var_os("PATH").is_none());
    // macOS: CoreFoundation (linked through the network stack) sets
    // __CF_USER_TEXT_ENCODING inside the child at load time even under
    // `env -i`; it is not inherited and carries no secret.
    let self_injected = |n: &str| cfg!(target_os = "macos") && n == "__CF_USER_TEXT_ENCODING";
    for name in &env {
        assert!(
            KEPT_ENV.contains(&name.as_str()) || self_injected(name),
            "leaked env var {name} (got {env:?})"
        );
    }
    // This test process certainly has HOME/CARGO_*; none of them may pass.
    assert!(std::env::var_os("CARGO_MANIFEST_DIR").is_some());
    assert!(
        !env.iter()
            .any(|n| n.starts_with("CARGO") || n == "HOME" || n.starts_with("SEARCH_MCP"))
    );
    assert_eq!(report["stdinPipe"], true);
    assert_eq!(
        report["stdoutPipe"], true,
        "the worker must never inherit the MCP stdout"
    );
    assert_eq!(report["stderrPipe"], true);
    assert!(is_reaped(run.pid.unwrap()));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn memory_bomb_worker_is_killed_by_the_rss_watchdog() {
    let dir = sandbox();
    write_pdf(dir.path(), "m.pdf", &simple_pdf(&["x"], None, &[]));
    let svc = pdf_service(dir.path(), worker_config(&["--selftest-alloc", "3072"]));
    let started = Instant::now();
    let err = svc.read("m.pdf", 0, None, &cx()).await.unwrap_err();
    assert_eq!(err.kind(), "BudgetExceeded", "{err}");
    assert!(err.to_string().contains("pdf worker killed"), "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(12),
        "{:?}",
        started.elapsed()
    );

    // Same through the raw runner, to see the kill reason and the reaping.
    let run = run_child(
        &spec(&["--selftest-alloc", "3072"], b"", Duration::from_secs(20)),
        &[],
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(
        matches!(
            run.killed,
            Some(KillReason::Memory { .. } | KillReason::Growth { .. })
        ),
        "{:?}",
        run.killed
    );
    assert!(run.peak_rss > 64 << 20, "peak {}", run.peak_rss);
    assert!(run.stdout.is_empty(), "nothing leaked to stdout");
    #[cfg(unix)]
    assert!(is_reaped(run.pid.unwrap()));
}

#[tokio::test]
async fn hung_worker_hits_the_wall_clock() {
    let dir = sandbox();
    write_pdf(dir.path(), "h.pdf", &simple_pdf(&["x"], None, &[]));
    let mut w = worker_config(&["--selftest-sleep", "60000"]);
    w.timeout = Duration::from_millis(800);
    let svc = pdf_service(dir.path(), w);
    let started = Instant::now();
    let err = svc.read("h.pdf", 0, None, &cx()).await.unwrap_err();
    assert!(matches!(err, FsmError::Timeout(ms) if ms >= 700), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn exec_deadline_bounds_the_worker_too() {
    let dir = sandbox();
    write_pdf(dir.path(), "d.pdf", &simple_pdf(&["x"], None, &[]));
    let svc = pdf_service(dir.path(), worker_config(&["--selftest-sleep", "60000"]));
    let call = CallCx::new("t", Instant::now() + Duration::from_millis(700));
    let started = Instant::now();
    let err = svc.read("d.pdf", 0, None, &call).await.unwrap_err();
    assert_eq!(err.kind(), "Timeout", "{err}");
    assert!(started.elapsed() < Duration::from_secs(4));
}

#[tokio::test]
async fn queue_wait_over_the_limit_is_pdf_worker_busy() {
    let dir = sandbox();
    write_pdf(dir.path(), "a.pdf", &simple_pdf(&["a"], None, &[]));
    write_pdf(dir.path(), "b.pdf", &simple_pdf(&["b"], None, &[]));
    let mut w = worker_config(&["--selftest-sleep", "4000"]);
    w.max_workers = 1;
    w.queue_wait = Duration::from_millis(300);
    w.timeout = Duration::from_secs(3);
    let svc = Arc::new(pdf_service(dir.path(), w));
    // Whichever call wins the single slot hangs until the wall clock; the
    // other waits 300 ms in the queue and is refused (order-independent).
    let call = |name: &'static str| {
        let svc = Arc::clone(&svc);
        tokio::spawn(async move {
            let started = Instant::now();
            let res = svc.read(name, 0, None, &cx()).await;
            (res, started.elapsed())
        })
    };
    let (a, b) = (call("a.pdf"), call("b.pdf"));
    let (a, b) = (a.await.unwrap(), b.await.unwrap());
    let (busy, held) = if matches!(a.0, Err(FsmError::BudgetExceeded(_))) {
        (a, b)
    } else {
        (b, a)
    };
    assert!(
        matches!(&busy.0, Err(FsmError::BudgetExceeded(m)) if m == "pdf worker busy"),
        "{:?}",
        busy.0
    );
    assert!(
        busy.1 < Duration::from_secs(2),
        "refused after {:?}",
        busy.1
    );
    assert_eq!(held.0.unwrap_err().kind(), "Timeout");
}

#[tokio::test]
async fn cancellation_kills_the_worker() {
    let dir = sandbox();
    write_pdf(dir.path(), "c.pdf", &simple_pdf(&["x"], None, &[]));
    let svc = pdf_service(dir.path(), worker_config(&["--selftest-sleep", "60000"]));
    let call = cx();
    let token = call.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        token.cancel();
    });
    let started = Instant::now();
    let err = svc.read("c.pdf", 0, None, &call).await.unwrap_err();
    assert!(matches!(err, FsmError::Timeout(0)), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(4));
}

#[tokio::test]
async fn worker_without_protocol_output_is_reported_as_unwired() {
    let dir = sandbox();
    write_pdf(dir.path(), "n.pdf", &simple_pdf(&["x"], None, &[]));
    let svc = pdf_service(dir.path(), worker_config(&["--selftest-noise"]));
    let err = svc.read("n.pdf", 0, None, &cx()).await.unwrap_err();
    assert!(
        matches!(
            &err,
            FsmError::Internal {
                kind: "pdf_worker",
                ..
            }
        ),
        "{err:?}"
    );
    assert!(err.to_string().contains("__pdf-worker"), "{err}");
}

#[tokio::test]
async fn unparsable_pdf_is_invalid_input_without_fallback() {
    let dir = sandbox();
    write_pdf(
        dir.path(),
        "junk.pdf",
        b"%PDF-1.4\n this is not a pdf body at all\n%%EOF\n",
    );
    let svc = pdf_service(dir.path(), worker_config(&[]));
    let res = svc.read("junk.pdf", 0, None, &cx()).await;
    match res {
        Err(e) => assert_eq!(e.kind(), "InvalidInput", "{e}"),
        // A tolerant parser may report an empty document instead.
        Ok(out) => assert!(out.content.trim().is_empty(), "{}", out.content),
    }
}

/// The poppler fallback, when the machine has it (skips otherwise).
#[tokio::test]
async fn pdftotext_fallback_rescues_a_broken_worker() {
    let Some(bin) = std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p)
            .map(|d| d.join("pdftotext"))
            .find(|p| p.is_file())
    }) else {
        eprintln!("pdftotext not on PATH; skipping the fallback test");
        return;
    };
    let dir = sandbox();
    write_pdf(
        dir.path(),
        "f.pdf",
        &simple_pdf(&["Fallback works", "Second page"], None, &[]),
    );
    let mut w = worker_config(&["--selftest-noise"]);
    w.pdftotext = PdftotextMode::Path(bin);
    let svc = pdf_service(dir.path(), w);
    let out = svc.read("f.pdf", 0, None, &cx()).await.unwrap();
    assert!(
        out.content.starts_with("## Page 1\n\nFallback works"),
        "{}",
        out.content
    );
    assert!(
        out.content.contains("## Page 2\n\nSecond page"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn real_worker_protocol_streams_pages_then_done() {
    let pdf = simple_pdf(&["one", "", "three"], Some("T"), &[]);
    let mut stdin = br#"{"v":1,"maxPages":200,"maxChars":2000000,"maxBytes":25000000,"outline":true,"rlimitDataMb":1024}"#.to_vec();
    stdin.push(b'\n');
    stdin.extend_from_slice(&pdf);
    let run = run_child(
        &spec(&[], &stdin, Duration::from_secs(15)),
        &[],
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(
        run.status.unwrap().success(),
        "{} {}",
        run.describe_exit(),
        run.stderr_text()
    );
    let t = WorkerTranscript::from_stdout(&run.stdout);
    assert_eq!(t.meta.as_ref().unwrap().total_pages, 3);
    assert_eq!(
        t.pages.iter().map(|p| p.n).collect::<Vec<_>>(),
        [1, 3],
        "blank page skipped"
    );
    assert!(t.done.is_some() && t.error.is_none());
    let bad = run_child(
        &spec(&[], b"not json\n", Duration::from_secs(5)),
        &[],
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(bad.status.unwrap().code(), Some(4));
    assert_eq!(
        WorkerTranscript::from_stdout(&bad.stdout)
            .error
            .unwrap()
            .kind,
        "request"
    );
}

#[tokio::test]
async fn pdf_reading_order_keeps_title_and_columns_contiguous() {
    // Emulate arXiv 1706.03762 page 1 where content stream order interleaves
    // top-line title tokens with lower-row author tokens; `to_plain_text` must
    // assemble visual reading order (title -> authors -> abstract).
    let mut b = PdfBuilder::default();
    let catalog = b.reserve();
    let pages_id = b.reserve();
    let font = b.add("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>");
    let stream = concat!(
        "BT /F1 17 Tf 1 0 0 1 160 720 Tm (Attention) Tj ET\n",
        "BT /F1 10 Tf 1 0 0 1 120 660 Tm (Ashish Vaswani) Tj ET\n",
        "BT /F1 17 Tf 1 0 0 1 240 720 Tm (Is All) Tj ET\n",
        "BT /F1 10 Tf 1 0 0 1 280 660 Tm (Noam Shazeer) Tj ET\n",
        "BT /F1 17 Tf 1 0 0 1 300 720 Tm (You Need) Tj ET\n",
        "BT /F1 12 Tf 1 0 0 1 250 610 Tm (Abstract) Tj ET\n",
        "BT /F1 10 Tf 1 0 0 1 72 580 Tm (The dominant sequence transduction models are based on attention.) Tj ET\n",
    );
    let content = b.add(PdfBuilder::stream("", stream.as_bytes()));
    let page = b.add(format!(
        "<< /Type /Page /Parent {pages_id} 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 {font} 0 R >> >> /Contents {content} 0 R >>"
    ));
    b.set(
        pages_id,
        format!("<< /Type /Pages /Kids [{page} 0 R] /Count 1 >>"),
    );
    b.set(
        catalog,
        format!("<< /Type /Catalog /Pages {pages_id} 0 R >>"),
    );
    let pdf = b.finish(catalog, None);

    let dir = sandbox();
    write_pdf(dir.path(), "reading_order.pdf", &pdf);
    let svc = pdf_service(dir.path(), worker_config(&[]));
    let out = svc.read("reading_order.pdf", 0, None, &cx()).await.unwrap();
    let normalized = out.content.split_whitespace().collect::<Vec<_>>().join(" ");
    let title_pos = normalized
        .find("Attention Is All You Need")
        .unwrap_or_else(|| panic!("title fragmented in reading order: {normalized:?}"));
    let author_pos = normalized
        .find("Ashish Vaswani")
        .unwrap_or_else(|| panic!("missing author: {normalized:?}"));
    let abs_pos = normalized
        .find("Abstract")
        .unwrap_or_else(|| panic!("missing abstract: {normalized:?}"));
    assert!(
        title_pos < author_pos && author_pos < abs_pos,
        "expected title < authors < abstract, got: {normalized:?}"
    );
}
