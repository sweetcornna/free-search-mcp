//! PDF extraction through the isolated worker process (`SPEC.md` §7.3):
//! at most `max_workers` children, a 5 s queue wait (S11), a 15 s wall
//! clock, an RSS/growth watchdog (S3) and a `pdftotext` fallback.

pub mod proc;
pub mod protocol;
pub(crate) mod worker;

use crate::parsed::Limits;
use proc::{ChildSpec, KillReason, RawRun, run_child};
use protocol::{OutlineItemMsg, PDF_WORKER_SUBCOMMAND, PageMsg, WorkerRequest, WorkerTranscript};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

/// Whether and where to find `pdftotext` (poppler) for the fallback.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum PdftotextMode {
    /// Look it up on `PATH` once, at service construction.
    #[default]
    Auto,
    /// Never fall back.
    Disabled,
    /// Use this binary.
    Path(PathBuf),
}

/// How PDFs are extracted. `Default` is the production setting.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PdfWorkerConfig {
    /// Worker executable; default: this process's own binary (`current_exe`).
    pub program: PathBuf,
    /// Arguments; default: `["__pdf-worker"]`.
    pub args: Vec<OsString>,
    /// Concurrent workers (`max_pdf_workers`, default 2).
    pub max_workers: usize,
    /// Longest wait for a free worker before `BudgetExceeded("pdf worker busy")`.
    pub queue_wait: Duration,
    /// Wall clock per PDF (also bounded by the exec deadline).
    pub timeout: Duration,
    /// Kill when resident memory passes this.
    pub rss_limit_bytes: u64,
    /// Kill when resident memory grows more than this within `growth_window`.
    pub growth_limit_bytes: u64,
    pub growth_window: Duration,
    /// RSS sampling period.
    pub poll_interval: Duration,
    /// `RLIMIT_DATA` the worker sets on itself (MiB, 0 = none).
    pub rlimit_data_mb: u64,
    /// Time the worker gets for the bookmark walk once its text is out.
    pub outline_grace: Duration,
    pub pdftotext: PdftotextMode,
}

impl Default for PdfWorkerConfig {
    fn default() -> Self {
        Self {
            program: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("free-search-mcp")),
            args: vec![OsString::from(PDF_WORKER_SUBCOMMAND)],
            max_workers: 2,
            queue_wait: Duration::from_secs(5),
            timeout: Duration::from_secs(15),
            rss_limit_bytes: 512 << 20,
            growth_limit_bytes: 256 << 20,
            growth_window: Duration::from_secs(1),
            poll_interval: Duration::from_millis(200),
            rlimit_data_mb: 1024,
            outline_grace: Duration::from_secs(3),
            pdftotext: PdftotextMode::Auto,
        }
    }
}

impl PdfWorkerConfig {
    /// Production defaults with another worker command (tests, or a server
    /// binary that names its subcommand differently).
    pub fn with_command(program: impl Into<PathBuf>, args: Vec<OsString>) -> Self {
        Self {
            program: program.into(),
            args,
            ..Self::default()
        }
    }
}

/// What extraction produced.
#[derive(Debug, Clone, Default)]
pub(crate) struct PdfOutcome {
    pub title: Option<String>,
    pub total_pages: Option<u32>,
    pub pages: Vec<PageMsg>,
    pub truncated: bool,
    pub outline: Vec<OutlineItemMsg>,
}

/// Why extraction failed (mapped to `FsmError` by the caller, who knows the source).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PdfFailure {
    Busy,
    Timeout(Duration),
    Cancelled,
    Memory(String),
    /// The worker could not parse the PDF, crashed, or overflowed its output.
    Parse(String),
    /// No protocol output at all: the worker command is not wired.
    Unavailable(String),
}

pub(crate) struct PdfPool {
    cfg: PdfWorkerConfig,
    permits: Arc<Semaphore>,
    pdftotext: Option<PathBuf>,
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| is_executable(p))
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

fn kill_detail(k: KillReason) -> String {
    match k {
        KillReason::Memory { rss, limit } => {
            format!(
                "memory limit exceeded (RSS {} MiB > {} MiB)",
                rss >> 20,
                limit >> 20
            )
        }
        KillReason::Growth { delta, window } => format!(
            "memory grew {} MiB within {} ms",
            delta >> 20,
            window.as_millis()
        ),
        other => format!("{other:?}"),
    }
}

impl PdfPool {
    pub(crate) fn new(cfg: PdfWorkerConfig) -> Self {
        let bin_name = if cfg!(windows) {
            "pdftotext.exe"
        } else {
            "pdftotext"
        };
        let pdftotext = match &cfg.pdftotext {
            PdftotextMode::Auto => find_on_path(bin_name),
            PdftotextMode::Disabled => None,
            PdftotextMode::Path(p) => Some(p.clone()),
        };
        Self {
            permits: Arc::new(Semaphore::new(cfg.max_workers.max(1))),
            cfg,
            pdftotext,
        }
    }

    fn spec(
        &self,
        program: PathBuf,
        args: Vec<OsString>,
        stdin: Arc<[u8]>,
        timeout: Duration,
        max_stdout: usize,
    ) -> ChildSpec {
        ChildSpec {
            program,
            args,
            stdin,
            timeout,
            rss_limit: self.cfg.rss_limit_bytes,
            growth_limit: self.cfg.growth_limit_bytes,
            growth_window: self.cfg.growth_window,
            poll: self.cfg.poll_interval,
            max_stdout,
            done_marker: None,
            grace: self.cfg.outline_grace,
        }
    }

    /// Extract `pdf` under the queue, time and memory rules.
    pub(crate) async fn extract(
        &self,
        pdf: &[u8],
        limits: &Limits,
        deadline: Instant,
        cancel: &CancellationToken,
    ) -> Result<PdfOutcome, PdfFailure> {
        if cancel.is_cancelled() {
            return Err(PdfFailure::Cancelled);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(PdfFailure::Timeout(Duration::ZERO));
        }
        let wait = self.cfg.queue_wait.min(remaining);
        let permit = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(PdfFailure::Cancelled),
            p = tokio::time::timeout(wait, Arc::clone(&self.permits).acquire_owned()) => match p {
                Ok(Ok(p)) => p,
                Ok(Err(_)) => return Err(PdfFailure::Unavailable("worker pool closed".into())),
                Err(_) if wait >= self.cfg.queue_wait => return Err(PdfFailure::Busy),
                Err(_) => return Err(PdfFailure::Timeout(wait)),
            },
        };
        let result = self.run_worker(pdf, limits, deadline, cancel).await;
        let out = match result {
            Ok(o) => Ok(o),
            Err(
                f @ (PdfFailure::Parse(_) | PdfFailure::Memory(_) | PdfFailure::Unavailable(_)),
            ) => match self.run_pdftotext(pdf, limits, deadline, cancel).await {
                Some(o) => {
                    tracing::info!("pdf_oxide failed ({f:?}); used the pdftotext fallback");
                    Ok(o)
                }
                None => Err(f),
            },
            Err(f) => Err(f),
        };
        drop(permit);
        out
    }

    async fn run_worker(
        &self,
        pdf: &[u8],
        limits: &Limits,
        deadline: Instant,
        cancel: &CancellationToken,
    ) -> Result<PdfOutcome, PdfFailure> {
        if cancel.is_cancelled() {
            return Err(PdfFailure::Cancelled);
        }
        let timeout = self
            .cfg
            .timeout
            .min(deadline.saturating_duration_since(Instant::now()));
        if timeout.is_zero() {
            return Err(PdfFailure::Timeout(Duration::ZERO));
        }
        let req = WorkerRequest {
            v: 1,
            max_pages: limits.max_pdf_pages,
            max_chars: limits.max_document_chars,
            max_bytes: limits.max_response_bytes,
            outline: true,
            rlimit_data_mb: self.cfg.rlimit_data_mb,
        };
        let mut stdin =
            serde_json::to_vec(&req).map_err(|e| PdfFailure::Unavailable(e.to_string()))?;
        stdin.push(b'\n');
        stdin.extend_from_slice(pdf);
        let mut spec = self.spec(
            self.cfg.program.clone(),
            self.cfg.args.clone(),
            Arc::from(stdin),
            timeout,
            limits
                .max_document_chars
                .saturating_mul(6)
                .saturating_add(4 << 20),
        );
        spec.done_marker = Some(b"FSM-PDF/1 done ");
        let run = run_child(&spec, &[], cancel)
            .await
            .map_err(|e| PdfFailure::Unavailable(format!("cannot start the PDF worker: {e}")))?;
        classify(&run, timeout)
    }

    /// `pdftotext -q -enc UTF-8 -l <max_pages + 1> <tmp> -` under the same
    /// isolation; pages split on form feeds.
    async fn run_pdftotext(
        &self,
        pdf: &[u8],
        limits: &Limits,
        deadline: Instant,
        cancel: &CancellationToken,
    ) -> Option<PdfOutcome> {
        let bin = self.pdftotext.clone()?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining < Duration::from_secs(1) || cancel.is_cancelled() {
            return None;
        }
        let owned_pdf = pdf.to_vec();
        let tmp = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let mut f = tempfile::Builder::new()
                .prefix("fsm-pdf-")
                .suffix(".pdf")
                .tempfile()
                .ok()?;
            f.write_all(&owned_pdf).ok()?;
            f.flush().ok()?;
            Some(f)
        })
        .await
        .ok()??;
        let args = vec![
            OsString::from("-q"),
            OsString::from("-enc"),
            OsString::from("UTF-8"),
            OsString::from("-l"),
            OsString::from(limits.max_pdf_pages.saturating_add(1).to_string()),
            tmp.path().as_os_str().to_owned(),
            OsString::from("-"),
        ];
        let max_stdout = limits
            .max_document_chars
            .saturating_mul(4)
            .saturating_add(1 << 20);
        let spec = self.spec(
            bin,
            args,
            Arc::from(Vec::new()),
            self.cfg.timeout.min(remaining),
            max_stdout,
        );
        let run = run_child(&spec, &[], cancel).await.ok();
        let _ = tokio::task::spawn_blocking(move || drop(tmp)).await;
        let run = run?;
        let complete = run.killed.is_none() && run.status.is_some_and(|s| s.success());
        let partial = run.killed == Some(KillReason::OutputTooLarge);
        if !complete && !partial {
            tracing::debug!(
                "pdftotext fallback failed: {} {}",
                run.describe_exit(),
                run.stderr_text()
            );
            return None;
        }
        parse_pdftotext_output(&run.stdout, partial, limits)
    }
}

pub(crate) fn parse_pdftotext_output(
    raw: &[u8],
    partial: bool,
    limits: &Limits,
) -> Option<PdfOutcome> {
    let text = String::from_utf8_lossy(raw);
    // pdftotext terminates every emitted page with a form feed `\x0c`.
    let body = text.strip_suffix('\x0c').unwrap_or(&text);
    let mut out = PdfOutcome {
        truncated: partial,
        ..PdfOutcome::default()
    };
    let mut assembled = 0usize;
    let mut total_seen = 0usize;
    for (i, page) in body.split('\x0c').enumerate() {
        total_seen = i + 1;
        if i >= limits.max_pdf_pages as usize {
            out.truncated = true;
            break;
        }
        let n = i as u32 + 1;
        let page = page.trim();
        if page.is_empty() {
            continue;
        }
        let header = format!("## Page {n}\n\n").chars().count();
        let sep = if out.pages.is_empty() { 0 } else { 2 };
        if !out.pages.is_empty() && assembled + sep + header >= limits.max_document_chars {
            out.truncated = true;
            break;
        }
        let room = limits
            .max_document_chars
            .saturating_sub(assembled + sep + header);
        let chars = page.chars().count();
        let clipped: String = page.chars().take(room).collect();
        assembled += sep + header + clipped.chars().count();
        out.pages.push(PageMsg { n, text: clipped });
        if chars > room || assembled >= limits.max_document_chars {
            out.truncated = true;
            break;
        }
    }
    if out.pages.is_empty() {
        return None;
    }
    out.total_pages = Some(u32::try_from(total_seen).unwrap_or(u32::MAX));
    Some(out)
}

/// Turn a worker run into an outcome. The kill reason and exit status
/// decide; a partial transcript (pages before a hostile one) is kept.
pub(crate) fn classify(run: &RawRun, timeout: Duration) -> Result<PdfOutcome, PdfFailure> {
    let t = WorkerTranscript::from_stdout(&run.stdout);
    if run.killed == Some(KillReason::Cancelled) {
        return Err(PdfFailure::Cancelled);
    }
    let outcome = |truncated: bool| PdfOutcome {
        title: t.meta.as_ref().and_then(|m| m.title.clone()),
        total_pages: t
            .meta
            .as_ref()
            .map(|m| u32::try_from(m.total_pages).unwrap_or(u32::MAX)),
        pages: t.pages.clone(),
        truncated,
        outline: t
            .outline
            .as_ref()
            .map(|o| o.items.clone())
            .unwrap_or_default(),
    };
    if let Some(done) = &t.done {
        if let Some(e) = &done.first_error {
            tracing::debug!("pdf page errors ({}): {e}", done.page_errors);
        }
        if t.pages.is_empty() && done.page_errors > 0 {
            return Err(PdfFailure::Parse(format!(
                "no page could be read ({} errors; first: {})",
                done.page_errors,
                done.first_error.as_deref().unwrap_or("?")
            )));
        }
        return Ok(outcome(done.truncated));
    }
    if let Some(err) = &t.error {
        return Err(PdfFailure::Parse(format!("{} ({})", err.error, err.kind)));
    }
    let partial = !t.pages.is_empty();
    match run.killed {
        Some(KillReason::Timeout) if partial => {
            tracing::warn!(
                "pdf worker timed out after {} pages; returning them",
                t.pages.len()
            );
            Ok(outcome(true))
        }
        Some(KillReason::Timeout) => Err(PdfFailure::Timeout(timeout)),
        Some(k @ (KillReason::Memory { .. } | KillReason::Growth { .. })) if partial => {
            tracing::warn!(
                "pdf worker killed ({}) after {} pages; returning them",
                kill_detail(k),
                t.pages.len()
            );
            Ok(outcome(true))
        }
        Some(k @ (KillReason::Memory { .. } | KillReason::Growth { .. })) => {
            Err(PdfFailure::Memory(kill_detail(k)))
        }
        Some(KillReason::OutputTooLarge) if partial => Ok(outcome(true)),
        Some(KillReason::OutputTooLarge) => {
            Err(PdfFailure::Parse("worker output exceeded its cap".into()))
        }
        _ if partial => {
            tracing::warn!(
                "pdf worker died ({}) after {} pages; returning them",
                run.describe_exit(),
                t.pages.len()
            );
            Ok(outcome(true))
        }
        _ if t.lines == 0 && run.status.is_some_and(|s| s.success()) => {
            Err(PdfFailure::Unavailable(format!(
                "the PDF worker exited without a result; is the `{PDF_WORKER_SUBCOMMAND}` subcommand wired? {}",
                run.stderr_text().chars().take(300).collect::<String>()
            )))
        }
        _ => {
            let stderr = run.stderr_text();
            tracing::debug!("pdf worker failed ({}): {stderr}", run.describe_exit());
            Err(PdfFailure::Parse(format!(
                "the PDF worker failed ({}){}",
                run.describe_exit(),
                crash_summary(&stderr).map_or_else(String::new, |s| format!(": {s}"))
            )))
        }
    }
}

/// One short, caller-safe reason from a crashed worker's stderr: known
/// runtime aborts by name, else its last line (≤ 200 chars). Thread ids and
/// multi-line runtime noise stay in the debug log.
fn crash_summary(stderr: &str) -> Option<String> {
    if stderr.contains("has overflowed its stack") {
        return Some("stack overflow (the document nests too deeply)".into());
    }
    if stderr.contains("memory allocation of") {
        return Some("out of memory".into());
    }
    let last = stderr.lines().rev().find(|l| !l.trim().is_empty())?.trim();
    Some(last.chars().take(200).collect())
}

#[cfg(test)]
mod tests {
    use super::protocol::{DoneMsg, MetaMsg, WorkerMsg};
    use super::*;

    fn run(lines: &[WorkerMsg], killed: Option<KillReason>, stderr: &str) -> RawRun {
        let mut stdout = Vec::new();
        for l in lines {
            stdout.extend_from_slice(l.to_line().as_bytes());
            stdout.push(b'\n');
        }
        RawRun {
            stdout,
            stderr_tail: stderr.as_bytes().to_vec(),
            status: None,
            killed,
            elapsed: Duration::from_millis(5),
            peak_rss: 0,
            pid: None,
        }
    }

    fn page(n: u32) -> WorkerMsg {
        WorkerMsg::Page(PageMsg {
            n,
            text: format!("p{n}"),
        })
    }

    fn meta() -> WorkerMsg {
        WorkerMsg::Meta(MetaMsg {
            title: Some("T".into()),
            total_pages: 9,
        })
    }

    #[test]
    fn pages_streamed_before_a_kill_survive_as_truncated() {
        for k in [
            KillReason::Timeout,
            KillReason::Memory {
                rss: 600 << 20,
                limit: 512 << 20,
            },
            KillReason::Growth {
                delta: 300 << 20,
                window: Duration::from_secs(1),
            },
            KillReason::OutputTooLarge,
        ] {
            let out = classify(
                &run(&[meta(), page(1), page(2)], Some(k), ""),
                Duration::from_secs(15),
            )
            .unwrap();
            assert!(out.truncated, "{k:?}");
            assert_eq!(out.pages.len(), 2);
            assert_eq!(
                (out.title.as_deref(), out.total_pages),
                (Some("T"), Some(9))
            );
        }
    }

    #[test]
    fn kills_without_pages_are_typed_failures() {
        let t = Duration::from_secs(15);
        assert_eq!(
            classify(&run(&[meta()], Some(KillReason::Timeout), ""), t).unwrap_err(),
            PdfFailure::Timeout(t)
        );
        assert!(matches!(
            classify(
                &run(&[], Some(KillReason::Memory { rss: 1, limit: 0 }), ""),
                t
            ),
            Err(PdfFailure::Memory(_))
        ));
        assert_eq!(
            classify(&run(&[page(1)], Some(KillReason::Cancelled), ""), t).unwrap_err(),
            PdfFailure::Cancelled,
            "a cancelled call never returns partial data"
        );
    }

    #[test]
    fn done_without_any_readable_page_is_a_parse_failure() {
        let done = WorkerMsg::Done(DoneMsg {
            truncated: false,
            page_errors: 3,
            first_error: Some("bad font".into()),
        });
        let err = classify(&run(&[meta(), done], None, ""), Duration::from_secs(1)).unwrap_err();
        assert!(
            matches!(&err, PdfFailure::Parse(m) if m.contains("3 errors") && m.contains("bad font")),
            "{err:?}"
        );
        let ok = WorkerMsg::Done(DoneMsg {
            truncated: false,
            page_errors: 0,
            first_error: None,
        });
        assert!(
            classify(&run(&[meta(), ok], None, ""), Duration::from_secs(1))
                .unwrap()
                .pages
                .is_empty()
        );
    }

    #[test]
    fn crash_reasons_are_summarised_without_runtime_noise() {
        let so = "thread 'pdf-extract' (4242) has overflowed its stack\nfatal runtime error: stack overflow, aborting";
        assert_eq!(
            crash_summary(so).unwrap(),
            "stack overflow (the document nests too deeply)"
        );
        assert_eq!(
            crash_summary("memory allocation of 99999 bytes failed").unwrap(),
            "out of memory"
        );
        assert_eq!(crash_summary("a\n\nlast line  \n").unwrap(), "last line");
        assert_eq!(crash_summary(&"x".repeat(500)).unwrap().len(), 200);
        assert_eq!(crash_summary("  \n"), None);
        let err = classify(&run(&[], None, so), Duration::from_secs(1)).unwrap_err();
        assert!(
            matches!(&err, PdfFailure::Parse(m) if m.contains("stack overflow") && !m.contains("4242")),
            "{err:?}"
        );
    }

    #[test]
    fn parse_pdftotext_output_counts_pages_and_truncation_accurately() {
        let mut lim = Limits {
            max_document_chars: 1000,
            max_response_bytes: 25_000_000,
            max_pdf_pages: 2,
            max_content_chars: 50_000,
        };
        // Exactly max_pdf_pages with trailing form feed: NOT truncated, total_pages = 2.
        let exact = parse_pdftotext_output(b"Page one\x0cPage two\x0c", false, &lim).unwrap();
        assert_eq!(exact.pages.len(), 2);
        assert_eq!(exact.total_pages, Some(2));
        assert!(!exact.truncated);

        // One extra page beyond max_pdf_pages (from `-l max+1`): truncated, 2 pages emitted.
        let over =
            parse_pdftotext_output(b"Page one\x0cPage two\x0cPage three\x0c", false, &lim).unwrap();
        assert_eq!(over.pages.len(), 2);
        assert_eq!(over.total_pages, Some(3));
        assert!(over.truncated);

        // Char cap stops cleanly without emitting an empty second page heading.
        lim.max_document_chars = 25;
        let capped = parse_pdftotext_output(b"Page one\x0cPage two\x0c", false, &lim).unwrap();
        assert_eq!(capped.pages.len(), 1);
        assert!(capped.truncated);

        // Partial output (stdout cap) keeps pages and flags truncated.
        lim.max_document_chars = 1000;
        let partial = parse_pdftotext_output(b"Only page", true, &lim).unwrap();
        assert_eq!(partial.pages.len(), 1);
        assert!(partial.truncated);
    }
}
