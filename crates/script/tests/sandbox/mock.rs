//! In-test fake of `HostServices` plus a HOST_API table mirroring the exec
//! `.d.ts` (SPEC.md §2.4) and a few test-only functions.

use async_trait::async_trait;
use fsm_core::{
    BinaryHandle, Bytes, CallCx, CatalogHit, CitationGraph, DocSlice, FetchMethod, FsmError,
    FulltextResult, Hit, HostFn, HostServices, HostValue, PageResult, RenderKind, ResearchBrief,
    ScholarlyWork, SearchMeta, SearchResponse, WorkResult,
};
use fsm_script::{ExecOutcome, Limits, Sandbox};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const fn row(name: &'static str) -> HostFn {
    HostFn::new(name, "", "", "")
}

pub const HOST_API: &[HostFn] = &[
    row("web.search").with_egress(true),
    row("web.fetch").with_egress(true),
    row("web.fetchBatch").with_egress(true),
    row("web.research").with_egress(true),
    row("scholar.search").with_egress(true),
    row("scholar.work").with_egress(true),
    row("doc.read"),
    row("catalog.search"),
    row("cache.put"),
    row("util.sleep"),
    row("util.dedupe").with_prelude_sync(true),
    row("util.grep").with_prelude_sync(true),
    row("util.pick").with_prelude_sync(true),
    row("util.estimateTokens").with_prelude_sync(true),
    row("test.echo"),
    row("test.sleepEcho"),
    row("test.barrier"),
    row("test.hang"),
    row("test.fail"),
    row("test.rateLimited"),
    row("test.image"),
    row("test.big"),
    row("test.fulltext"),
    row("test.graph"),
    row("test.panic"),
    row("test.failWith"),
];

#[derive(Default)]
pub struct Mock {
    pub calls: Mutex<Vec<(String, Value)>>,
    inflight: AtomicUsize,
    pub high_water: AtomicUsize,
    pub started: AtomicUsize,
    pub completed: AtomicUsize,
    pub dropped: AtomicUsize,
}

/// Counts in-flight calls; a call dropped before finishing counts as dropped.
struct Track<'a> {
    mock: &'a Mock,
    done: bool,
}

impl<'a> Track<'a> {
    fn new(mock: &'a Mock) -> Self {
        mock.started.fetch_add(1, Ordering::SeqCst);
        let now = mock.inflight.fetch_add(1, Ordering::SeqCst) + 1;
        mock.high_water.fetch_max(now, Ordering::SeqCst);
        Self { mock, done: false }
    }
}

impl Drop for Track<'_> {
    fn drop(&mut self) {
        self.mock.inflight.fetch_sub(1, Ordering::SeqCst);
        if self.done {
            self.mock.completed.fetch_add(1, Ordering::SeqCst);
        } else {
            self.mock.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }
}

pub fn page(url: &str) -> PageResult {
    let mut p = PageResult::default();
    p.url = url.to_owned();
    p.title = format!("Title of {url}");
    p.content = format!("Content of {url}.\nSecond line mentions Price: 42.");
    p.method = FetchMethod::Http;
    p.tokens_estimated = 12;
    p.total_chars = 40;
    p.retrieved_at = "2026-11-25T00:00:00Z".into();
    p
}

pub fn work(i: usize) -> ScholarlyWork {
    let mut w = ScholarlyWork::default();
    w.title = format!("Work {i}");
    w.work_type = "article".into();
    w.url = format!("https://doi.org/10.1000/{i}");
    w.year = Some(2020);
    w
}

fn search(q: &str) -> SearchResponse {
    let mut r = SearchResponse::default();
    r.query = q.to_owned();
    r.scope = "web".into();
    r.results = (0..3)
        .map(|i| {
            let mut h = Hit::default();
            h.title = format!("Hit {i}");
            h.url = format!("https://example.com/{i}");
            h
        })
        .collect();
    r.engines = vec!["mock".into()];
    r
}

fn arg_str(args: &Value, i: usize) -> String {
    args.get(i)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn to_json<T: serde::Serialize>(v: &T) -> Value {
    serde_json::to_value(v).expect("serialize")
}

impl Mock {
    async fn barrier(&self, target: usize, max_wait: Duration) -> usize {
        let until = Instant::now() + max_wait;
        while self.high_water.load(Ordering::SeqCst) < target && Instant::now() < until {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        self.high_water.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl HostServices for Mock {
    async fn call(&self, func: &str, args: Value, _cx: &CallCx) -> Result<HostValue, FsmError> {
        self.calls
            .lock()
            .unwrap()
            .push((func.to_owned(), args.clone()));
        let mut track = Track::new(self);
        let out = match func {
            "web.fetch" => Ok(HostValue::tagged(
                to_json(&page(&arg_str(&args, 0))),
                RenderKind::PageResult,
            )),
            "web.fetchBatch" => {
                let urls: Vec<String> = serde_json::from_value(args[0].clone()).unwrap_or_default();
                let pages: Vec<PageResult> = urls.iter().map(|u| page(u)).collect();
                Ok(HostValue::tagged(to_json(&pages), RenderKind::PageList))
            }
            "web.search" => Ok(HostValue::tagged(
                to_json(&search(&arg_str(&args, 0))),
                RenderKind::SearchResponse,
            )),
            "web.research" => {
                let mut b = ResearchBrief::default();
                b.query = arg_str(&args, 0);
                b.search = search(&b.query);
                b.pages = vec![page("https://a.example/1"), page("https://b.example/2")];
                Ok(HostValue::tagged(to_json(&b), RenderKind::ResearchBrief))
            }
            "scholar.search" => {
                let mut r = WorkResult::default();
                r.works = (0..3).map(work).collect();
                let mut meta = SearchMeta::default();
                meta.query = arg_str(&args, 0);
                r.meta = meta;
                Ok(HostValue::tagged(to_json(&r), RenderKind::WorkResult))
            }
            "scholar.work" => Ok(HostValue::tagged(to_json(&work(7)), RenderKind::WorkList)),
            "doc.read" => {
                let mut d = DocSlice::default();
                d.source = arg_str(&args, 0);
                d.format = "pdf".into();
                d.content = "doc text".into();
                Ok(HostValue::tagged(to_json(&d), RenderKind::DocSlice))
            }
            "catalog.search" => {
                let mut h = CatalogHit::default();
                h.id = "arxiv".into();
                h.kind = "source".into();
                Ok(HostValue::tagged(
                    to_json(&vec![h.clone(), h]),
                    RenderKind::CatalogList,
                ))
            }
            "cache.put" => Ok(HostValue::new(Value::Null)),
            "test.echo" => Ok(HostValue::new(args)),
            "test.sleepEcho" => {
                let ms = args[0].as_u64().unwrap_or(0);
                tokio::time::sleep(Duration::from_millis(ms)).await;
                Ok(HostValue::new(args.get(1).cloned().unwrap_or(Value::Null)))
            }
            "test.barrier" => {
                let target = args[0].as_u64().unwrap_or(1) as usize;
                let wait = args.get(1).and_then(Value::as_u64).unwrap_or(5000);
                let seen = self.barrier(target, Duration::from_millis(wait)).await;
                Ok(HostValue::new(json!(seen)))
            }
            "test.hang" => {
                std::future::pending::<()>().await;
                unreachable!()
            }
            "test.fail" => Err(FsmError::upstream("mock", Some(503), arg_str(&args, 0))),
            "test.rateLimited" => Err(FsmError::RateLimited {
                source_id: "mock".into(),
                retry_after_s: Some(5),
            }),
            "test.image" => {
                let meta = json!({"url": "https://example.com/a.png", "mediaType": "image/png", "bytes": 4, "sha256": "ab"});
                let handle =
                    BinaryHandle::new("img-1", "image/png", Bytes::from_static(b"\x89PNG"));
                Ok(HostValue::tagged(meta, RenderKind::InlineImage).with_binary(handle))
            }
            "test.big" => {
                let mb = args[0].as_u64().unwrap_or(5) as usize;
                Ok(HostValue::new(json!("x".repeat(mb << 20))))
            }
            "test.fulltext" => {
                let mut f = FulltextResult::new("10.1000/1", true);
                f.format = Some("text".into());
                f.content = Some("Full text body".into());
                Ok(HostValue::tagged(to_json(&f), RenderKind::FulltextResult))
            }
            "test.graph" => {
                let mut g = CitationGraph::new("q", "doi", work(0));
                g.references = (1..4).map(work).collect();
                g.citations = (4..6).map(work).collect();
                Ok(HostValue::tagged(to_json(&g), RenderKind::CitationGraph))
            }
            "test.panic" => panic!("mock host function panicked on purpose"),
            "test.failWith" => Err(FsmError::invalid(arg_str(&args, 0))),
            other => Err(FsmError::invalid(format!("mock has no {other}"))),
        };
        track.done = true;
        out
    }
}

pub struct Run {
    pub out: ExecOutcome,
    pub mock: Arc<Mock>,
    pub cx: Arc<CallCx>,
}

pub fn limits(timeout_ms: u64) -> Limits {
    Limits::default().with_timeout(Duration::from_millis(timeout_ms))
}

pub fn sandbox() -> Sandbox {
    Sandbox::new(HOST_API)
        .expect("sandbox")
        .with_max_concurrent_execs(8)
}

pub fn call_cx(timeout_ms: u64) -> CallCx {
    let mut cx = CallCx::new("test", Instant::now() + Duration::from_millis(timeout_ms));
    cx.nonce = "k7m9".into();
    cx
}

/// Run `code` with a fresh mock, a 5 s default wall clock.
pub async fn exec(code: &str) -> Run {
    exec_with(code, limits(5000), call_cx(5000)).await
}

pub async fn exec_with(code: &str, limits: Limits, cx: CallCx) -> Run {
    let mock = Arc::new(Mock::default());
    let cx = Arc::new(cx);
    let out = sandbox().run(code, mock.clone(), cx.clone(), &limits).await;
    Run { out, mock, cx }
}

impl Run {
    pub fn value(&self) -> &fsm_script::ExecValue {
        match &self.out.result {
            Ok(v) => v,
            Err(f) => panic!(
                "script failed: {:?} (line {:?}); console {:?}",
                f.error, f.line, self.out.console
            ),
        }
    }

    /// The result as JSON (`Text` becomes a JSON string).
    pub fn json(&self) -> Value {
        self.value().raw_json()
    }

    pub fn error(&self) -> &FsmError {
        match &self.out.result {
            Ok(v) => panic!("script succeeded with {v:?}"),
            Err(f) => &f.error,
        }
    }

    pub fn line(&self) -> Option<u32> {
        self.out.result.as_ref().err().and_then(|f| f.line)
    }
}
