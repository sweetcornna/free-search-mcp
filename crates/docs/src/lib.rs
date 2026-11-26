//! `fsm-docs` — document reading for `doc.read` / `doc.outline` (owner: lane
//! `W4b`, `SPEC-lanes.md`; behaviour oracle: `src/search_mcp/documents.py`).
//!
//! - Sources: `http(s)` URLs (through [`fsm_core::NetClient`], SSRF-checked)
//!   or paths inside `SEARCH_MCP_DOCUMENT_ROOT` (off by default). A local
//!   read sets `CallCx::local_doc_tainted` BEFORE the path is resolved, so
//!   even a not-found probe counts (M6).
//! - Formats: PDF (isolated `__pdf-worker` process running `pdf_oxide`,
//!   `pdftotext` fallback), DOCX/PPTX (streaming `zip` + `quick-xml`), XLSX
//!   (`calamine` cell stream), EPUB, CSV/TSV, archive listings, code, HTML,
//!   Markdown, text. Images are refused with a pointer to `web.fetch`.
//! - Pagination: `returned_chars` counts SOURCE characters consumed, so
//!   `start + returned_chars` is always the next unread character.
//!
//! The server wires the hidden subcommand: `free-search-mcp __pdf-worker`
//! must call [`DocService::run_pdf_worker_cli`] and exit with its code.

mod archive;
mod cache;
mod csv_doc;
mod decode;
mod epub;
mod format;
mod image;
mod markdown;
mod office;
mod parse;
mod parsed;
mod pdf;
mod sandbox;
mod slice;

use crate::cache::{CacheKey, ParsedCache};
use crate::format::{DocFormat, detect_format, looks_like_pdf, sniff_unknown};
use crate::parsed::{Limits, ParsedDoc};
use crate::pdf::{PdfFailure, PdfPool};
use crate::sandbox::{FileStamp, SourceKind, classify};
use async_trait::async_trait;
use fsm_core::{
    CallCx, DocApi, DocOutline, DocSlice, FsmError, HttpRequest, NetCx, SourceDeps,
    TransportProfile,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

pub use pdf::protocol::PDF_WORKER_SUBCOMMAND;
pub use pdf::{PdfWorkerConfig, PdftotextMode};

/// Internals exposed for integration tests and the server's `doctor`; not
/// a stable API.
#[doc(hidden)]
pub mod testing {
    pub use crate::pdf::proc::{ChildSpec, KEPT_ENV, KillReason, RawRun, rss_bytes, run_child};
    pub use crate::pdf::protocol::{
        DoneMsg, ErrorMsg, LINE_PREFIX, MetaMsg, OutlineItemMsg, OutlineMsg, PageMsg, WorkerMsg,
        WorkerRequest, WorkerTranscript, exit,
    };
}

/// What `Accept` remote document reads send.
const ACCEPT: &str = "application/pdf,application/vnd.openxmlformats-officedocument.*,\
application/epub+zip,text/csv,text/html;q=0.9,text/*;q=0.8,*/*;q=0.5";

/// Document reader (PDF, DOCX, XLSX, ...).
pub struct DocService {
    deps: SourceDeps,
    pdf: Arc<PdfPool>,
    cpu: Arc<Semaphore>,
    cache: Arc<ParsedCache>,
}

impl DocService {
    pub fn new(deps: SourceDeps) -> Self {
        let cpu = std::thread::available_parallelism().map_or(2, |n| n.get().clamp(1, 4));
        Self {
            deps,
            pdf: Arc::new(PdfPool::new(PdfWorkerConfig::default())),
            cpu: Arc::new(Semaphore::new(cpu)),
            cache: Arc::new(ParsedCache::new(8, 32 << 20, Duration::from_secs(600))),
        }
    }

    /// Replace the PDF worker settings (the worker command, limits,
    /// `pdftotext` fallback). Production uses the defaults.
    pub fn with_pdf_worker(mut self, cfg: PdfWorkerConfig) -> Self {
        self.pdf = Arc::new(PdfPool::new(cfg));
        self
    }

    /// Entry point of the isolated PDF worker process. The server's hidden
    /// `__pdf-worker` subcommand calls this and exits with the result; it
    /// reads the request from stdin and writes protocol lines to stdout.
    pub fn run_pdf_worker_cli() -> std::process::ExitCode {
        pdf::worker::worker_main()
    }

    fn limits(&self) -> Limits {
        Limits::from_config(&self.deps.cfg)
    }

    /// Parse (or fetch from cache) the document behind `source`.
    async fn load(&self, source: &str, cx: &CallCx) -> Result<Arc<ParsedDoc>, FsmError> {
        match classify(source)? {
            SourceKind::Remote => self.load_remote(source, cx).await,
            SourceKind::Local => {
                if self.deps.cfg.document_root.is_some() {
                    // M6: taint before anything is resolved or opened, so a
                    // not-found answer (one bit about the disk) counts too.
                    cx.mark_local_doc_read();
                }
                self.load_local(source, cx).await
            }
        }
    }

    async fn load_remote(&self, url: &str, cx: &CallCx) -> Result<Arc<ParsedDoc>, FsmError> {
        const MAX_DOC_REDIRECTS: usize = 5;

        // SSRF layers 1–3 and M6 taint host check run before any cache hit is served.
        self.deps.net.check_url_offline(url)?;
        check_tainted_doc_host(url, cx).await?;
        let key = CacheKey::Remote(url.to_owned());
        if let Some(doc) = self.cache.get(&key) {
            return Ok(doc);
        }
        let remaining = cx.remaining();
        if remaining.is_zero() || cx.cancel.is_cancelled() {
            return Err(FsmError::Timeout(0));
        }
        let limits = self.limits();
        let timeout =
            Duration::from_secs_f64(self.deps.cfg.fetch_timeout_s.clamp(1.0, 600.0)).min(remaining);

        // Follow redirects manually so every hop re-checks offline SSRF AND M6
        // local-doc taint (`cx.fetched_hosts`), preventing an allowed host from
        // redirecting a tainted `web.fetch(".../x.pdf")` to an arbitrary host.
        let mut current_url = url.to_owned();
        let resp = {
            let mut hops = 0usize;
            loop {
                let mut req = HttpRequest::get(&current_url)
                    .header("Accept", ACCEPT)
                    .with_max_bytes(limits.max_response_bytes);
                req.follow_redirects = false;
                let hop_timeout = timeout.min(cx.remaining());
                if hop_timeout.is_zero() || cx.cancel.is_cancelled() {
                    return Err(FsmError::Timeout(0));
                }
                let ncx = NetCx::new(cx.cancel.child_token(), hop_timeout);
                let resp = self
                    .deps
                    .net
                    .send(req, TransportProfile::Chrome, &ncx)
                    .await?;
                if matches!(resp.status, 301 | 302 | 303 | 307 | 308)
                    && let Some(loc) = resp.header("location")
                {
                    if hops >= MAX_DOC_REDIRECTS {
                        return Err(FsmError::upstream(
                            "doc",
                            Some(resp.status),
                            format!("too many redirects fetching {url}"),
                        ));
                    }
                    let base = url::Url::parse(&current_url).map_err(|e| {
                        FsmError::UnsafeUrl(format!("invalid redirect base URL: {e}"))
                    })?;
                    let next = base
                        .join(loc.trim())
                        .map_err(|e| {
                            FsmError::UnsafeUrl(format!("invalid redirect Location: {e}"))
                        })?
                        .to_string();
                    self.deps.net.check_url_offline(&next)?;
                    check_tainted_doc_host(&next, cx).await?;
                    current_url = next;
                    hops += 1;
                    continue;
                }
                break resp;
            }
        };
        if !resp.is_success() {
            return Err(FsmError::upstream(
                "doc",
                Some(resp.status),
                format!("HTTP {} fetching {current_url}", resp.status),
            ));
        }
        if resp.truncated {
            return Err(FsmError::MaxBytesExceeded(limits.max_response_bytes));
        }
        let ctype = resp.header("content-type").unwrap_or("").to_owned();
        let format = detect_format(&current_url, Some(&ctype));
        let doc = Arc::new(
            self.parse(resp.body.to_vec(), ctype, current_url, format, cx)
                .await?,
        );
        self.cache.put(key, Arc::clone(&doc));
        Ok(doc)
    }

    async fn load_local(&self, source: &str, cx: &CallCx) -> Result<Arc<ParsedDoc>, FsmError> {
        if cx.cancel.is_cancelled() || cx.remaining().is_zero() {
            return Err(FsmError::Timeout(0));
        }
        let root = self.deps.cfg.document_root.clone();
        let home = fsm_core::config::system_home();
        let src = source.to_owned();
        let (path, stamp) = blocking(move || {
            let path = sandbox::resolve_local_path(&src, root.as_deref(), home.as_deref())?;
            let meta = std::fs::metadata(&path)
                .map_err(|e| FsmError::invalid(format!("Cannot read local document: {e}")))?;
            let stamp = FileStamp {
                len: meta.len(),
                modified: meta.modified().ok(),
            };
            Ok((path, stamp))
        })
        .await?;
        if let Some(doc) = self.cache.get(&CacheKey::Local {
            path: path.clone(),
            stamp,
        }) {
            return Ok(doc);
        }
        let cap = self.limits().max_response_bytes;
        let read_path = path.clone();
        let (blob, stamp) = blocking(move || sandbox::read_local_capped(&read_path, cap)).await?;
        let format = detect_format(&path.to_string_lossy(), None);
        let doc = Arc::new(
            self.parse(blob, String::new(), source.to_owned(), format, cx)
                .await?,
        );
        self.cache
            .put(CacheKey::Local { path, stamp }, Arc::clone(&doc));
        Ok(doc)
    }

    /// `_parse_blob` off the async runtime: PDFs through the worker pool,
    /// everything else on a blocking thread under the CPU semaphore.
    async fn parse(
        &self,
        blob: Vec<u8>,
        ctype: String,
        source: String,
        format: DocFormat,
        cx: &CallCx,
    ) -> Result<ParsedDoc, FsmError> {
        let limits = self.limits();
        let format = match format {
            DocFormat::Unknown => sniff_unknown(&blob).unwrap_or(DocFormat::Unknown),
            f => f,
        };
        if format == DocFormat::Pdf && looks_like_pdf(&blob) {
            return match self
                .pdf
                .extract(&blob, &limits, cx.deadline, &cx.cancel)
                .await
            {
                Ok(outcome) => Ok(parse::pdf_doc(outcome, &limits)),
                Err(f) => Err(pdf_error(f, &source)),
            };
        }
        let permit = tokio::select! {
            p = Arc::clone(&self.cpu).acquire_owned() => p.map_err(|_| FsmError::internal("docs", "parser pool closed"))?,
            () = cx.cancel.cancelled() => return Err(FsmError::Timeout(0)),
        };
        blocking(move || {
            let _permit = permit;
            parse::parse_sync(&blob, &ctype, &source, format, &limits)
        })
        .await
    }
}

/// Enforce M6 local-document taint on remote document fetches and redirect hops.
async fn check_tainted_doc_host(raw_url: &str, cx: &CallCx) -> Result<(), FsmError> {
    if !cx.is_tainted() {
        return Ok(());
    }
    let host = url::Url::parse(raw_url.trim())
        .ok()
        .and_then(|u| {
            u.host_str()
                .map(|h| h.trim_end_matches('.').to_ascii_lowercase())
        })
        .unwrap_or_default();
    if !host.is_empty() && cx.fetched_hosts.lock().await.contains(&host) {
        return Ok(());
    }
    Err(FsmError::PermissionDenied(
        "remote document redirect to a new host is blocked after reading a local document in this exec (exfiltration guard)"
            .to_owned(),
    ))
}

fn pdf_error(f: PdfFailure, source: &str) -> FsmError {
    match f {
        PdfFailure::Busy => FsmError::BudgetExceeded("pdf worker busy".to_owned()),
        PdfFailure::Timeout(d) => {
            FsmError::Timeout(u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        }
        PdfFailure::Cancelled => FsmError::Timeout(0),
        PdfFailure::Memory(m) => FsmError::BudgetExceeded(format!("pdf worker killed: {m}")),
        PdfFailure::Parse(m) => FsmError::invalid(format!("could not parse PDF {source:?}: {m}")),
        PdfFailure::Unavailable(m) => FsmError::internal("pdf_worker", m),
    }
}

/// Run blocking work on tokio's blocking pool, mapping a panic to `Internal`.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, FsmError> + Send + 'static,
) -> Result<T, FsmError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| FsmError::internal("docs", format!("document task failed: {e}")))?
}

#[async_trait]
impl DocApi for DocService {
    async fn read(
        &self,
        source: &str,
        start: usize,
        length: Option<usize>,
        cx: &CallCx,
    ) -> Result<DocSlice, FsmError> {
        let doc = self.load(source, cx).await?;
        let limits = self.limits();
        let s = slice::slice(
            &doc.text,
            doc.total_chars,
            start,
            length,
            limits.max_content_chars,
        );
        let mut out = DocSlice::new(source, doc.format.as_str(), s.content);
        out.title = doc.title.clone();
        out.pages = doc.pages;
        out.start = s.start;
        out.returned_chars = s.returned_chars;
        out.total_chars = doc.total_chars;
        out.truncated = s.truncated || doc.truncated;
        Ok(out)
    }

    async fn outline(&self, source: &str, cx: &CallCx) -> Result<DocOutline, FsmError> {
        let doc = self.load(source, cx).await?;
        let mut out = DocOutline::new(source, doc.format.as_str());
        out.title = doc.title.clone();
        out.pages = doc.pages;
        out.sections = doc.sections.clone();
        out.sheets = doc.sheets.clone();
        Ok(out)
    }
}

#[cfg(test)]
mod taint_redirect_tests {
    use super::*;
    use bytes::Bytes;
    use fsm_core::{Config, HttpBytes, NetClient, NullStore};
    use std::sync::atomic::{AtomicBool, Ordering};

    struct RedirectNet {
        attacker_hit: AtomicBool,
    }

    #[async_trait]
    impl NetClient for RedirectNet {
        fn check_url_offline(&self, _url: &str) -> Result<(), FsmError> {
            Ok(())
        }

        async fn send(
            &self,
            req: HttpRequest,
            _profile: TransportProfile,
            _cx: &NetCx,
        ) -> Result<HttpBytes, FsmError> {
            if req.url.contains("attacker.example") {
                self.attacker_hit.store(true, Ordering::SeqCst);
                return Ok(HttpBytes::new(
                    req.url,
                    200,
                    vec![("content-type".into(), "text/plain".into())],
                    Bytes::from_static(b"exfiltrated"),
                ));
            }
            if req.follow_redirects {
                // Simulate HttpClient automatically following 302 to attacker.example.
                self.attacker_hit.store(true, Ordering::SeqCst);
                return Ok(HttpBytes::new(
                    "https://attacker.example/collect",
                    200,
                    vec![("content-type".into(), "text/plain".into())],
                    Bytes::from_static(b"exfiltrated"),
                ));
            }
            Ok(HttpBytes::new(
                req.url,
                302,
                vec![("location".into(), "https://attacker.example/collect".into())],
                Bytes::new(),
            ))
        }
    }

    #[tokio::test]
    async fn load_remote_refuses_tainted_redirect_to_unfetched_host() {
        let net = Arc::new(RedirectNet {
            attacker_hit: AtomicBool::new(false),
        });
        let deps = SourceDeps::new(
            Arc::clone(&net) as Arc<dyn NetClient>,
            Arc::new(NullStore::new()),
            Arc::new(Config::default()),
        );
        let docs = DocService::new(deps);
        let cx = CallCx::new("sess", std::time::Instant::now() + Duration::from_secs(10));
        cx.fetched_hosts
            .lock()
            .await
            .insert("allowed.example".to_owned());
        cx.mark_local_doc_read();

        let err = docs
            .read("https://allowed.example/doc.txt", 0, None, &cx)
            .await
            .expect_err("tainted redirect to attacker.example must be blocked");
        assert!(
            matches!(err, FsmError::PermissionDenied(_)),
            "expected PermissionDenied, got {err:?}"
        );
        assert!(
            !net.attacker_hit.load(Ordering::SeqCst),
            "request must never reach attacker.example"
        );
    }
}
