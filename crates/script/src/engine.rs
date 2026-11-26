//! The sandbox runner: one OS thread, one current-thread Tokio runtime, one
//! fresh QuickJS-NG `AsyncRuntime` + `AsyncContext::full` per exec (port of the
//! Spike C sandbox, `D/spikes/core/code/ws/sandbox`).

use crate::boundary::{self, ExecValue, RawValue};
use crate::normalize::{self, Candidate, Form, MODULE_NAME, Normalized};
use crate::prelude::{self, PRELUDE_NAME};
use crate::{DEFAULT_CONCURRENT_EXECS, Limits, MAX_CONCURRENT_EXECS};
use fsm_core::{CallCx, FsmError, HostFn, HostServices, HostValue};
use rquickjs::allocator::{Allocator, RustAllocator};
use rquickjs::context::EvalOptions;
use rquickjs::function::Func;
use rquickjs::module::Declared;
use rquickjs::{
    Array, AsyncContext, AsyncRuntime, CatchResultExt, CaughtError, Ctx, Function, Module, Object,
    Promise, Value,
};
use serde_json::{Value as Json, json};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::Poll;
use std::time::{Duration, Instant};
use tokio::runtime::Handle;
use tokio::sync::{Notify, Semaphore, oneshot};
use tokio::task::{JoinError, JoinHandle};
use tokio_util::sync::CancellationToken;

/// Stack of the sandbox thread. QuickJS stops at [`Limits::stack_bytes`]; the
/// rest is head-room for Rust frames (only touched pages are committed).
const THREAD_STACK: usize = 8 << 20;
/// How long past the deadline or cancel the caller waits for a wedged sandbox thread.
const GRACE: Duration = Duration::from_secs(2);
/// Character cap on `Error.name` in formatted JS errors (B3).
const MAX_ERR_NAME_CHARS: usize = 64;
/// Character cap on `Error.message` in formatted JS errors (B3).
const MAX_ERR_MSG_CHARS: usize = 1_000;
/// Hard ceiling on retained host-error slots per exec so caught error loops stay O(1) memory.
const MAX_ERROR_SLOTS: usize = 512;

/// Why a run failed, with the script line when one is known.
#[derive(Debug)]
#[non_exhaustive]
pub struct ExecFailure {
    /// `Timeout(ms)` for the wall clock (`Timeout(0)` when the caller
    /// cancelled), `BudgetExceeded` for heap / host calls / payload caps,
    /// `JsSyntax`, `JsRuntime`, or the original error of an uncaught host
    /// call failure (`RateLimited`, `PermissionDenied`, ...).
    pub error: FsmError,
    /// 1-based line of the submitted script: exact for `throw` and syntax
    /// errors, best effort for engine errors (`null.x` reports the enclosing
    /// function), the calling line for host errors.
    pub line: Option<u32>,
}

impl ExecFailure {
    pub fn new(error: FsmError) -> Self {
        Self { error, line: None }
    }

    fn at(error: FsmError, line: Option<u32>) -> Self {
        Self { error, line }
    }
}

/// Everything one run produced.
#[derive(Debug)]
#[non_exhaustive]
pub struct ExecOutcome {
    pub result: Result<ExecValue, ExecFailure>,
    /// Captured `console.*` lines (`warn: ` / `error: ` / `debug: ` prefixed).
    pub console: Vec<String>,
    /// `console.*` output went past [`Limits::console_bytes`].
    pub console_truncated: bool,
    /// The script compiled only after TypeScript stripping.
    pub ts_stripped: bool,
    /// Host calls charged to the exec's budget (`util.sleep` included).
    pub host_calls: u32,
    pub elapsed: Duration,
}

impl ExecOutcome {
    fn failed(error: FsmError, started: Instant, cx: &CallCx) -> Self {
        Self {
            result: Err(ExecFailure::new(error)),
            console: Vec::new(),
            console_truncated: false,
            ts_stripped: false,
            host_calls: cx.host_calls.load(Ordering::SeqCst),
            elapsed: started.elapsed(),
        }
    }
}

/// The `exec` sandbox: the prelude built from `HOST_API` plus the global
/// concurrent-exec limit. Cheap to clone; one per server.
#[derive(Clone)]
pub struct Sandbox {
    prelude: Arc<str>,
    slots: Arc<Semaphore>,
    max_concurrent: usize,
}

impl std::fmt::Debug for Sandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sandbox")
            .field("max_concurrent", &self.max_concurrent)
            .field("prelude_bytes", &self.prelude.len())
            .finish()
    }
}

impl Sandbox {
    /// Build the sandbox for a `HOST_API` table: async rows become
    /// `__host` bindings, prelude-sync rows must be one of
    /// [`crate::PRELUDE_SYNC_FNS`]. Rejects malformed or duplicate names.
    pub fn new(host_api: &[HostFn]) -> Result<Self, FsmError> {
        Ok(Self {
            prelude: prelude::build(host_api)?.into(),
            slots: Arc::new(Semaphore::new(DEFAULT_CONCURRENT_EXECS)),
            max_concurrent: DEFAULT_CONCURRENT_EXECS,
        })
    }

    /// Global number of execs running at once (default 4, max 8); later
    /// calls wait for a slot until their deadline.
    pub fn with_max_concurrent_execs(mut self, n: usize) -> Self {
        let n = n.clamp(1, MAX_CONCURRENT_EXECS);
        self.slots = Arc::new(Semaphore::new(n));
        self.max_concurrent = n;
        self
    }

    /// Run one script. `cx` is shared with every host call; its `deadline`
    /// and `limits.timeout` (whichever is sooner) bound the run, and
    /// cancelling `cx.cancel` (or dropping this future) stops it.
    pub async fn run(
        &self,
        code: &str,
        host: Arc<dyn HostServices>,
        cx: Arc<CallCx>,
        limits: &Limits,
    ) -> ExecOutcome {
        let started = Instant::now();
        if cx.cancel.is_cancelled() {
            return ExecOutcome::failed(FsmError::Timeout(0), started, &cx);
        }
        let limits = limits.clone().clamped();
        let deadline = cx.deadline.min(started + limits.timeout);
        let timeout_ms = (deadline.saturating_duration_since(started).as_millis() as u64).max(1);
        if code.trim().is_empty() {
            return ExecOutcome::failed(
                FsmError::invalid(
                    "code is empty: pass `async () => { ... }`, statements with `return`, or an expression",
                ),
                started,
                &cx,
            );
        }
        if code.contains('\0') {
            return ExecOutcome::failed(
                FsmError::invalid("code must not contain NUL bytes"),
                started,
                &cx,
            );
        }
        if code.len() > limits.payload_bytes {
            return ExecOutcome::failed(
                FsmError::BudgetExceeded(format!(
                    "code is {:.1} MiB; the cap is {} MiB",
                    code.len() as f64 / (1 << 20) as f64,
                    limits.payload_bytes >> 20
                )),
                started,
                &cx,
            );
        }
        if started >= deadline {
            return ExecOutcome::failed(FsmError::Timeout(timeout_ms), started, &cx);
        }
        let Ok(main) = Handle::try_current() else {
            return ExecOutcome::failed(
                FsmError::internal(
                    "sandbox",
                    "Sandbox::run must be polled inside a Tokio runtime",
                ),
                started,
                &cx,
            );
        };
        let permit = tokio::select! {
            biased;
            _ = cx.cancel.cancelled() => {
                return ExecOutcome::failed(FsmError::Timeout(0), started, &cx);
            }
            p = self.slots.clone().acquire_owned() => match p {
                Ok(p) => p,
                Err(_) => {
                    return ExecOutcome::failed(
                        FsmError::internal("sandbox", "exec slots closed"),
                        started,
                        &cx,
                    );
                }
            },
            _ = tokio::time::sleep_until(deadline.into()) => {
                return ExecOutcome::failed(
                    FsmError::BudgetExceeded(format!(
                        "{} exec calls were running for this call's whole timeout; retry shortly",
                        self.max_concurrent
                    )),
                    started,
                    &cx,
                );
            }
        };
        // Cancelled by the caller's token, or when this future is dropped.
        let stop = cx.cancel.child_token();
        let _stop_on_drop = stop.clone().drop_guard();
        let shared = Arc::new(Shared::new(limits.console_bytes));
        let job = Job {
            code: code.to_owned(),
            prelude: self.prelude.clone(),
            host,
            cx: cx.clone(),
            inflight: Arc::new(Semaphore::new(limits.max_concurrency)),
            limits,
            deadline,
            timeout_ms,
            stop: stop.clone(),
            main,
            shared: shared.clone(),
        };
        let (tx, mut rx) = oneshot::channel();
        let spawned = std::thread::Builder::new()
            .name("fsm-exec".into())
            .stack_size(THREAD_STACK)
            .spawn(move || {
                let local = match tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .build()
                {
                    Ok(rt) => rt.block_on(run_local(job)),
                    Err(e) => LocalOutcome::internal(format!("sandbox runtime: {e}")),
                };
                let _ = tx.send(local);
            });
        let local = match spawned {
            Err(e) => LocalOutcome::internal(format!("sandbox thread: {e}")),
            Ok(_) => {
                let wait = tokio::select! {
                    biased;
                    res = &mut rx => Some(res),
                    _ = stop.cancelled() => {
                        let cap = (Instant::now() + GRACE).min(deadline + GRACE);
                        tokio::time::timeout_at(cap.into(), rx).await.ok()
                    }
                    _ = tokio::time::sleep_until((deadline + GRACE).into()) => None,
                };
                match wait {
                    Some(Ok(local)) => local,
                    Some(Err(_)) => {
                        LocalOutcome::internal("sandbox thread exited without a result".into())
                    }
                    None => {
                        stop.cancel();
                        tracing::error!(
                            "exec sandbox thread still busy {GRACE:?} after stop/deadline; abandoned"
                        );
                        LocalOutcome::internal("sandbox thread did not stop at its deadline".into())
                    }
                }
            }
        };
        drop(permit);
        let (console, console_truncated) = shared.take_logs();
        let result = match local.result {
            Ok(raw) => boundary::classify(raw, &cx).await,
            Err(f) => Err(f),
        };
        ExecOutcome {
            result,
            console,
            console_truncated,
            ts_stripped: local.ts_stripped,
            host_calls: cx.host_calls.load(Ordering::SeqCst),
            elapsed: started.elapsed(),
        }
    }
}

struct Job {
    code: String,
    prelude: Arc<str>,
    host: Arc<dyn HostServices>,
    cx: Arc<CallCx>,
    inflight: Arc<Semaphore>,
    limits: Limits,
    deadline: Instant,
    timeout_ms: u64,
    stop: CancellationToken,
    main: Handle,
    shared: Arc<Shared>,
}

impl Job {
    /// The run hit its deadline, or the caller cancelled (`Timeout(0)`, the
    /// workspace convention for cancellation).
    fn interrupted(&self) -> ExecFailure {
        if self.stop.is_cancelled() {
            ExecFailure::new(FsmError::Timeout(0))
        } else {
            ExecFailure::new(FsmError::Timeout(self.timeout_ms))
        }
    }

    fn out_of_memory(&self) -> ExecFailure {
        ExecFailure::new(FsmError::BudgetExceeded(format!(
            "the script used up its {} MiB JS heap; process data in smaller pieces or keep less of it",
            self.limits.heap_bytes >> 20
        )))
    }
}

struct LocalOutcome {
    result: Result<RawValue, ExecFailure>,
    ts_stripped: bool,
}

impl LocalOutcome {
    fn internal(message: String) -> Self {
        Self {
            result: Err(ExecFailure::new(FsmError::internal("sandbox", message))),
            ts_stripped: false,
        }
    }
}

#[derive(Default)]
struct ErrorRing {
    next_id: usize,
    slots: Vec<Option<(usize, FsmError)>>,
}

/// State shared between the bridge closures and the runner.
struct Shared {
    logs: Mutex<LogBuf>,
    errors: Mutex<ErrorRing>,
    /// Host calls dispatched and not yet settled. `util.sleep` is not
    /// counted: a timer that never fires (cleared, or a lost `Promise.race`
    /// timeout) is usually harmless.
    pending_calls: AtomicUsize,
    /// Fatal bridge failure (e.g. OOM inside `resolve.call` while delivering a
    /// host result into QuickJS) that leaves the host promise permanently
    /// pending unless `run_local` aborts immediately (B2).
    bridge_fail: Mutex<Option<ExecFailure>>,
    bridge_notify: Notify,
    /// Set by [`CappedAllocator`] when QuickJS hits `heap_bytes`, so QuickJS's
    /// `JS_ThrowError2` fallback to `JS_NULL` (when even `InternalError` cannot
    /// be allocated) is reported as `BudgetExceeded` rather than `throw null`.
    oom_hit: AtomicBool,
}

/// Counts one in-flight host call; decrements when it settles or is dropped
/// with the runtime.
struct PendingGuard(Arc<Shared>);

impl PendingGuard {
    fn new(shared: Arc<Shared>) -> Self {
        shared.pending_calls.fetch_add(1, Ordering::SeqCst);
        Self(shared)
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.0.pending_calls.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct LogBuf {
    lines: Vec<String>,
    bytes: usize,
    cap: usize,
    truncated: bool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    fn new(console_bytes: usize) -> Self {
        Self {
            logs: Mutex::new(LogBuf {
                cap: console_bytes,
                ..LogBuf::default()
            }),
            errors: Mutex::new(ErrorRing::default()),
            pending_calls: AtomicUsize::new(0),
            bridge_fail: Mutex::new(None),
            bridge_notify: Notify::new(),
            oom_hit: AtomicBool::new(false),
        }
    }

    fn set_bridge_fail(&self, fail: ExecFailure) {
        let mut slot = lock(&self.bridge_fail);
        if slot.is_none() {
            *slot = Some(fail);
        }
        drop(slot);
        self.bridge_notify.notify_one();
    }

    fn take_bridge_fail(&self) -> Option<ExecFailure> {
        lock(&self.bridge_fail).take()
    }

    /// A sandbox notice: always kept, whatever the console cap.
    fn push_note(&self, note: String) {
        lock(&self.logs).lines.push(format!("[exec] {note}"));
    }

    /// Note host calls the script started but did not await: they are
    /// cancelled when the script returns.
    fn note_unawaited(&self) {
        let calls = self.pending_calls.load(Ordering::SeqCst);
        if calls > 0 {
            self.push_note(format!(
                "{calls} host call(s) were still pending when the script returned and may not have completed; await them (e.g. with Promise.all)"
            ));
        }
    }

    fn push_log(&self, line: String) {
        let mut buf = lock(&self.logs);
        if buf.truncated {
            return;
        }
        let room = buf.cap.saturating_sub(buf.bytes);
        if line.len() < room {
            buf.bytes += line.len() + 1;
            buf.lines.push(line);
            return;
        }
        let mut cut = room.saturating_sub(1).min(line.len());
        while !line.is_char_boundary(cut) {
            cut -= 1;
        }
        if cut > 0 {
            buf.lines.push(line[..cut].to_owned());
        }
        buf.bytes = buf.cap;
        buf.truncated = true;
    }

    fn take_logs(&self) -> (Vec<String>, bool) {
        let mut buf = lock(&self.logs);
        (std::mem::take(&mut buf.lines), buf.truncated)
    }

    fn push_error(&self, e: FsmError) -> usize {
        let mut ring = lock(&self.errors);
        let id = ring.next_id;
        ring.next_id = if id >= i32::MAX as usize { 0 } else { id + 1 };
        let idx = id % MAX_ERROR_SLOTS;
        if ring.slots.len() <= idx {
            ring.slots.resize_with(idx + 1, || None);
        }
        ring.slots[idx] = Some((id, e));
        id
    }

    fn take_error(&self, slot: usize) -> Option<FsmError> {
        let mut ring = lock(&self.errors);
        let idx = slot % MAX_ERROR_SLOTS;
        match ring.slots.get_mut(idx) {
            Some(entry) if entry.as_ref().is_some_and(|(id, _)| *id == slot) => {
                entry.take().map(|(_, e)| e)
            }
            _ => None,
        }
    }
}

/// QuickJS allocator enforcing `limit` bytes and flagging `shared.oom_hit` when
/// an allocation is refused so `JS_ThrowError2`'s `JS_NULL` OOM fallback is
/// never misreported as `uncaught non-Error value thrown: null`.
struct CappedAllocator {
    used: usize,
    limit: usize,
    shared: Arc<Shared>,
}

// SAFETY: Delegates all raw allocations to `rquickjs::allocator::RustAllocator`,
// checking size/overflow bounds and keeping `used` in sync via `usable_size`.
unsafe impl Allocator for CappedAllocator {
    fn alloc(&mut self, size: usize) -> *mut u8 {
        if size == 0 {
            return std::ptr::null_mut();
        }
        if self.used.saturating_add(size) > self.limit {
            self.shared.oom_hit.store(true, Ordering::Release);
            return std::ptr::null_mut();
        }
        let ptr = RustAllocator.alloc(size);
        if ptr.is_null() {
            self.shared.oom_hit.store(true, Ordering::Release);
            return std::ptr::null_mut();
        }
        let actual = unsafe { RustAllocator::usable_size(ptr) };
        self.used = self.used.saturating_add(actual);
        ptr
    }

    fn calloc(&mut self, count: usize, size: usize) -> *mut u8 {
        if count == 0 || size == 0 {
            return std::ptr::null_mut();
        }
        let Some(total) = count.checked_mul(size) else {
            self.shared.oom_hit.store(true, Ordering::Release);
            return std::ptr::null_mut();
        };
        if self.used.saturating_add(total) > self.limit {
            self.shared.oom_hit.store(true, Ordering::Release);
            return std::ptr::null_mut();
        }
        let ptr = RustAllocator.calloc(count, size);
        if ptr.is_null() {
            self.shared.oom_hit.store(true, Ordering::Release);
            return std::ptr::null_mut();
        }
        let actual = unsafe { RustAllocator::usable_size(ptr) };
        self.used = self.used.saturating_add(actual);
        ptr
    }

    unsafe fn dealloc(&mut self, ptr: *mut u8) {
        if ptr.is_null() {
            return;
        }
        let old = unsafe { RustAllocator::usable_size(ptr) };
        self.used = self.used.saturating_sub(old);
        unsafe { RustAllocator.dealloc(ptr) };
    }

    unsafe fn realloc(&mut self, ptr: *mut u8, new_size: usize) -> *mut u8 {
        if ptr.is_null() {
            return self.alloc(new_size);
        }
        if new_size == 0 {
            unsafe { self.dealloc(ptr) };
            return std::ptr::null_mut();
        }
        let old = unsafe { RustAllocator::usable_size(ptr) };
        let projected = self.used.saturating_sub(old).saturating_add(new_size);
        if projected > self.limit {
            self.shared.oom_hit.store(true, Ordering::Release);
            return std::ptr::null_mut();
        }
        let new_ptr = unsafe { RustAllocator.realloc(ptr, new_size) };
        if new_ptr.is_null() {
            self.shared.oom_hit.store(true, Ordering::Release);
            return std::ptr::null_mut();
        }
        let actual = unsafe { RustAllocator::usable_size(new_ptr) };
        self.used = self.used.saturating_sub(old).saturating_add(actual);
        new_ptr
    }

    unsafe fn usable_size(ptr: *mut u8) -> usize
    where
        Self: Sized,
    {
        if ptr.is_null() {
            0
        } else {
            unsafe { RustAllocator::usable_size(ptr) }
        }
    }
}

async fn run_local(job: Job) -> LocalOutcome {
    let alloc = CappedAllocator {
        used: 0,
        limit: job.limits.heap_bytes,
        shared: job.shared.clone(),
    };
    let rt = match AsyncRuntime::new_with_alloc(alloc) {
        Ok(rt) => rt,
        Err(e) => return LocalOutcome::internal(format!("QuickJS runtime: {e}")),
    };
    rt.set_max_stack_size(job.limits.stack_bytes).await;
    let (stop, deadline) = (job.stop.clone(), job.deadline);
    // Sticky: once the deadline passes or the call is cancelled, every check says stop.
    rt.set_interrupt_handler(Some(Box::new(move || {
        stop.is_cancelled() || Instant::now() >= deadline
    })))
    .await;
    let ctx = match AsyncContext::full(&rt).await {
        Ok(ctx) => ctx,
        Err(e) => return LocalOutcome::internal(format!("QuickJS context: {e}")),
    };
    let mut ts_stripped = false;
    let result = {
        let work = ctx.async_with(async |ctx| run_script(&ctx, &job, &mut ts_stripped).await);
        tokio::select! {
            biased;
            r = work => r,
            _ = job.shared.bridge_notify.notified() => {
                Err(job.shared.take_bridge_fail().unwrap_or_else(|| job.out_of_memory()))
            }
            _ = job.stop.cancelled() => Err(job.interrupted()),
            _ = tokio::time::sleep_until(deadline.into()) => Err(job.interrupted()),
        }
    };
    drop(ctx);
    drop(rt);
    LocalOutcome {
        result,
        ts_stripped,
    }
}

async fn run_script<'js>(
    ctx: &Ctx<'js>,
    job: &Job,
    ts_stripped: &mut bool,
) -> Result<RawValue, ExecFailure> {
    install_natives(ctx, job)
        .map_err(|e| ExecFailure::new(FsmError::internal("sandbox", format!("natives: {e}"))))?;
    let mut opts = EvalOptions::default();
    opts.filename = Some(PRELUDE_NAME.into());
    let internals: Object<'js> = ctx
        .eval_with_options(job.prelude.to_string(), opts)
        .catch(ctx)
        .map_err(|c| match c {
            CaughtError::Exception(e)
                if e.message().as_deref() == Some("interrupted")
                    && (job.stop.is_cancelled() || Instant::now() >= job.deadline) =>
            {
                job.interrupted()
            }
            CaughtError::Error(rquickjs::Error::Allocation) => job.out_of_memory(),
            other => ExecFailure::new(FsmError::internal("prelude", other.to_string())),
        })?;
    let norm = normalize::normalize_code(&job.code);
    let (module, cand) = compile(ctx, &norm, job, ts_stripped)?;
    let fail = |c: CaughtError<'js>| failure(ctx, Some(&internals), false, c, &cand, &norm, job);
    let (module, done) = module.eval().catch(ctx).map_err(fail)?;
    settle(ctx, &internals, done.into_value())
        .await
        .map_err(fail)?;
    let main: Function<'js> = module.get("default").catch(ctx).map_err(fail)?;
    let mut value: Value<'js> = main.call(()).catch(ctx).map_err(fail)?;
    value = settle(ctx, &internals, value).await.map_err(fail)?;
    if cand.form == Form::Expression
        && let Some(f) = value.as_function().cloned()
    {
        value = f.call(()).catch(ctx).map_err(fail)?;
        value = settle(ctx, &internals, value).await.map_err(fail)?;
    }
    let raw = finalize(ctx, &internals, value, &cand, &norm, job)?;
    job.shared.note_unawaited();
    Ok(raw)
}

/// Await `v` if it is a promise, through the prelude's `wrapPromise` so the
/// wait does not depend on the script's `Promise.prototype.then`.
async fn settle<'js>(
    ctx: &Ctx<'js>,
    internals: &Object<'js>,
    v: Value<'js>,
) -> Result<Value<'js>, CaughtError<'js>> {
    if v.as_promise().is_none() {
        return Ok(v);
    }
    let wrap: Function<'js> = internals.get("wrapPromise").catch(ctx)?;
    let wrapped: Promise<'js> = wrap.call((v,)).catch(ctx)?;
    wrapped.into_future::<Value<'js>>().await.catch(ctx)
}

fn is_syntax_error(c: &CaughtError<'_>) -> bool {
    matches!(c, CaughtError::Exception(e) if e.get::<_, Option<String>>("name").ok().flatten().as_deref() == Some("SyntaxError"))
}

fn is_oom_error(name: &str, message: &str) -> bool {
    name == "InternalError" && message == "out of memory"
}

fn clip_with_ellipsis(s: &str, max_chars: usize) -> String {
    let clipped = fsm_core::text::clip_chars(s, max_chars);
    if clipped.len() < s.len() {
        format!("{clipped}…")
    } else {
        clipped.to_owned()
    }
}

/// Strictly compile-only (`JS_EVAL_FLAG_COMPILE_ONLY`, zero JS execution):
/// tries plain JS candidates first, then one TypeScript-stripped retry (S10).
fn compile<'js>(
    ctx: &Ctx<'js>,
    norm: &Normalized,
    job: &Job,
    ts_stripped: &mut bool,
) -> Result<(Module<'js, Declared>, Candidate), ExecFailure> {
    let mut syntax: Option<ExecFailure> = None;
    for cand in &norm.candidates {
        if job.stop.is_cancelled() || Instant::now() >= job.deadline {
            return Err(job.interrupted());
        }
        match Module::declare(ctx.clone(), MODULE_NAME, cand.source.clone()).catch(ctx) {
            Ok(m) => return Ok((m, cand.clone())),
            // Report the statements form's error: it is the general one.
            Err(c) if is_syntax_error(&c) => {
                syntax = Some(failure(ctx, None, true, c, cand, norm, job));
            }
            Err(c) => return Err(failure(ctx, None, true, c, cand, norm, job)),
        }
    }
    // S10: one retry with TypeScript erased; positions are preserved.
    for cand in &norm.candidates {
        if job.stop.is_cancelled() || Instant::now() >= job.deadline {
            return Err(job.interrupted());
        }
        let Ok(stripped) = normalize::strip_types(&cand.source) else {
            continue;
        };
        if stripped == cand.source {
            continue;
        }
        match Module::declare(ctx.clone(), MODULE_NAME, stripped).catch(ctx) {
            Ok(m) => {
                *ts_stripped = true;
                return Ok((m, cand.clone()));
            }
            Err(c) if is_syntax_error(&c) => {}
            Err(c) => return Err(failure(ctx, None, true, c, cand, norm, job)),
        }
    }
    Err(syntax.unwrap_or_else(|| {
        ExecFailure::new(FsmError::internal("sandbox", "no candidate compiled"))
    }))
}

fn finalize<'js>(
    ctx: &Ctx<'js>,
    internals: &Object<'js>,
    value: Value<'js>,
    cand: &Candidate,
    norm: &Normalized,
    job: &Job,
) -> Result<RawValue, ExecFailure> {
    let fail = |c: CaughtError<'js>| failure(ctx, Some(internals), false, c, cand, norm, job);
    let fin: Function<'js> = internals.get("finalize").catch(ctx).map_err(fail)?;
    let parts: Array<'js> = fin.call((value,)).catch(ctx).map_err(fail)?;
    let kind: Option<String> = parts.get(0).catch(ctx).map_err(fail)?;
    let image: Option<String> = parts.get(1).catch(ctx).map_err(fail)?;
    let target: Value<'js> = parts.get(2).catch(ctx).map_err(fail)?;
    let text = ctx.json_stringify(target).catch(ctx).map_err(|c| {
        match failure(ctx, Some(internals), false, c, cand, norm, job) {
            ExecFailure {
                error: FsmError::JsRuntime(msg),
                line,
            } => ExecFailure::at(
                FsmError::JsRuntime(format!(
                    "the result is not JSON-serializable ({msg}); return plain data without cycles or BigInt"
                )),
                line,
            ),
            other => other,
        }
    })?;
    let json = match text {
        None => None,
        Some(s) => Some(
            s.to_string()
                .map_err(|e| ExecFailure::new(FsmError::internal("sandbox", e.to_string())))?,
        ),
    };
    if let Some(j) = &json
        && j.len() > job.limits.payload_bytes
    {
        return Err(ExecFailure::new(FsmError::BudgetExceeded(format!(
            "the result is {:.1} MiB of JSON; the cap is {} MiB. Return less, or cache.put() parts and return their keys",
            j.len() as f64 / (1 << 20) as f64,
            job.limits.payload_bytes >> 20
        ))));
    }
    Ok(RawValue { json, kind, image })
}

/// Map an uncaught JS failure to an [`ExecFailure`].
fn failure<'js>(
    ctx: &Ctx<'js>,
    internals: Option<&Object<'js>>,
    compile_time: bool,
    caught: CaughtError<'js>,
    cand: &Candidate,
    norm: &Normalized,
    job: &Job,
) -> ExecFailure {
    match caught {
        CaughtError::Exception(e) => {
            // Check hostErrorSlot first so a host error whose message happens
            // to be "interrupted" or "out of memory" preserves its FsmError.
            if let Some(internals) = internals {
                let slot = internals
                    .get::<_, Function<'js>>("hostErrorSlot")
                    .and_then(|f| f.call::<_, i32>((e.clone(),)))
                    .catch(ctx)
                    .ok()
                    .unwrap_or(-1);
                if let Ok(slot) = usize::try_from(slot)
                    && let Some(error) = job.shared.take_error(slot)
                {
                    let stack = e.stack().unwrap_or_default();
                    if job.stop.is_cancelled() || Instant::now() >= job.deadline {
                        return job.interrupted();
                    }
                    let line = normalize::locate(&stack, cand, &norm.body).map(|(l, _)| l);
                    return ExecFailure::at(error, line);
                }
            }
            let name_res = e.get::<_, Option<String>>("name");
            let name_alloc_failed = matches!(name_res, Err(rquickjs::Error::Allocation));
            let raw_name = name_res
                .catch(ctx)
                .ok()
                .flatten()
                .unwrap_or_else(|| "Error".into());
            let raw_message = e.message().unwrap_or_default();
            if job.stop.is_cancelled() || Instant::now() >= job.deadline {
                return job.interrupted();
            }
            if name_alloc_failed || is_oom_error(&raw_name, &raw_message) {
                return job.out_of_memory();
            }
            let stack = e.stack().unwrap_or_default();
            if job.stop.is_cancelled() || Instant::now() >= job.deadline {
                return job.interrupted();
            }
            let at = normalize::locate(&stack, cand, &norm.body);
            let line = at.map(|(l, _)| l);
            let message = clip_with_ellipsis(&raw_message, MAX_ERR_MSG_CHARS);
            if compile_time && raw_name == "SyntaxError" {
                let (l, c) = at.unwrap_or((1, 1));
                return ExecFailure::at(
                    FsmError::JsSyntax {
                        line: l,
                        col: c,
                        message,
                        snippet: normalize::snippet(&norm.body, l),
                    },
                    Some(l),
                );
            }
            let name = clip_with_ellipsis(&raw_name, MAX_ERR_NAME_CHARS);
            let place = at
                .map(|(l, c)| format!(" (at line {l}:{c})"))
                .unwrap_or_default();
            ExecFailure::at(
                FsmError::JsRuntime(format!("{name}: {message}{place}")),
                line,
            )
        }
        CaughtError::Value(v) => {
            if v.is_null() && job.shared.oom_hit.load(Ordering::Acquire) {
                return job.out_of_memory();
            }
            let text = match ctx.json_stringify(v).catch(ctx) {
                Ok(Some(s)) => s
                    .to_string()
                    .unwrap_or_else(|_| "a value that is not JSON".into()),
                Ok(None) => "a value that is not JSON".into(),
                Err(CaughtError::Error(rquickjs::Error::Allocation)) => {
                    return job.out_of_memory();
                }
                Err(CaughtError::Exception(e)) => {
                    if job.stop.is_cancelled() || Instant::now() >= job.deadline {
                        return job.interrupted();
                    }
                    let n = e
                        .get::<_, Option<String>>("name")
                        .catch(ctx)
                        .ok()
                        .flatten()
                        .unwrap_or_default();
                    let m = e.message().unwrap_or_default();
                    if is_oom_error(&n, &m) {
                        return job.out_of_memory();
                    }
                    "a value that is not JSON".into()
                }
                Err(_) => "a value that is not JSON".into(),
            };
            if job.stop.is_cancelled() || Instant::now() >= job.deadline {
                return job.interrupted();
            }
            ExecFailure::new(FsmError::JsRuntime(format!(
                "uncaught non-Error value thrown: {}",
                fsm_core::text::clip_chars(&text, 300)
            )))
        }
        CaughtError::Error(rquickjs::Error::Allocation) => job.out_of_memory(),
        CaughtError::Error(e) => ExecFailure::new(FsmError::internal("quickjs", e.to_string())),
    }
}

fn install_natives<'js>(ctx: &Ctx<'js>, job: &Job) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    let shared = job.shared.clone();
    globals.set(
        "__fsm_log",
        Func::from(move |line: String| shared.push_log(line)),
    )?;
    globals.set(
        "__fsm_url_key",
        Func::from(|url: String| fsm_core::url_canon::url_key(&url)),
    )?;
    globals.set("__fsm_payload_bytes", job.limits.payload_bytes as u32)?;
    globals.set("__fsm_console_bytes", job.limits.console_bytes as u32)?;
    let bridge = Bridge {
        host: job.host.clone(),
        cx: job.cx.clone(),
        main: job.main.clone(),
        inflight: job.inflight.clone(),
        shared: job.shared.clone(),
        deadline: job.deadline,
        stop: job.stop.clone(),
        heap_bytes: job.limits.heap_bytes,
        payload_bytes: job.limits.payload_bytes,
    };
    // Callback bridge (not `Func::from(Async(..))` and not returning a Promise):
    // 1. Avoids rquickjs `Promise::wrap_future` printing to stdout on late settle.
    // 2. Delivers replies through a closure-private callback so prototype tampering
    //    on `Promise.prototype.{constructor,then}` cannot forge host envelopes.
    // 3. Validates payload size and charges `max_host_calls` synchronously BEFORE
    //    `ctx.spawn`, returning an immediate error envelope so floods cannot queue
    //    unbounded Rust-heap tasks outside QuickJS's memory limit.
    globals.set(
        "__fsm_host",
        Func::from(
            move |ctx: Ctx<'js>,
                  name: String,
                  args: String,
                  cb: Function<'js>|
                  -> rquickjs::Result<Option<String>> {
                let parsed = match bridge.prepare_call(&name, args) {
                    Ok(v) => v,
                    Err(e) => return Ok(Some(bridge.error_envelope(e))),
                };
                let guard = (!prelude::SANDBOX_NATIVE_FNS.contains(&name.as_str()))
                    .then(|| PendingGuard::new(bridge.shared.clone()));
                let bridge = bridge.clone();
                let ctx2 = ctx.clone();
                ctx.spawn(async move {
                    let envelope = bridge.dispatch_prepared(name, parsed).await;
                    drop(guard);
                    if let Err(err) = cb.call::<_, ()>((envelope,)) {
                        let caught = Err::<(), _>(err).catch(&ctx2).unwrap_err();
                        if !bridge.stop.is_cancelled() && Instant::now() < bridge.deadline {
                            let fail = match caught {
                                CaughtError::Error(rquickjs::Error::Allocation) => {
                                    bridge.out_of_memory()
                                }
                                CaughtError::Exception(e) => {
                                    let name = e
                                        .get::<_, Option<String>>("name")
                                        .catch(&ctx2)
                                        .ok()
                                        .flatten()
                                        .unwrap_or_default();
                                    let msg = e.message().unwrap_or_default();
                                    if is_oom_error(&name, &msg)
                                        || (name.is_empty() && msg.is_empty())
                                    {
                                        bridge.out_of_memory()
                                    } else {
                                        ExecFailure::new(FsmError::internal(
                                            "sandbox",
                                            format!("host callback failed: {name}: {msg}"),
                                        ))
                                    }
                                }
                                other => ExecFailure::new(FsmError::internal(
                                    "sandbox",
                                    format!("host callback failed: {other}"),
                                )),
                            };
                            bridge.shared.set_bridge_fail(fail);
                        }
                    }
                });
                Ok(None)
            },
        ),
    )?;
    Ok(())
}

/// The one native entry point for async host functions.
#[derive(Clone)]
struct Bridge {
    host: Arc<dyn HostServices>,
    cx: Arc<CallCx>,
    main: Handle,
    inflight: Arc<Semaphore>,
    shared: Arc<Shared>,
    deadline: Instant,
    stop: CancellationToken,
    heap_bytes: usize,
    payload_bytes: usize,
}

impl Bridge {
    fn out_of_memory(&self) -> ExecFailure {
        ExecFailure::new(FsmError::BudgetExceeded(format!(
            "the script used up its {} MiB JS heap; process data in smaller pieces or keep less of it",
            self.heap_bytes >> 20
        )))
    }
}

struct Forwarded {
    json: String,
    tag: Option<&'static str>,
    image: Option<String>,
}

/// A spawned host call, aborted when the sandbox drops it (timeout, cancel,
/// or the script finishing without awaiting it).
struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl<T> Future for AbortOnDrop<T> {
    type Output = Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(cx)
    }
}

impl Bridge {
    fn error_envelope(&self, e: FsmError) -> String {
        let (kind, message) = (e.kind(), e.to_string());
        let slot = self.shared.push_error(e);
        json!({"ok": false, "kind": kind, "message": message, "slot": slot}).to_string()
    }

    /// Synchronous validation + budget charging before spawning any async task.
    fn prepare_call(&self, name: &str, args: String) -> Result<Json, FsmError> {
        // The prelude marks arguments JSON.stringify refused (cycles, BigInt).
        if let Some(reason) = args.strip_prefix('\u{1}') {
            return Err(FsmError::invalid(format!(
                "the arguments to {name} are not JSON-serializable ({reason}); pass plain data without cycles or BigInt"
            )));
        }
        // The prelude marks oversized JSON payloads before crossing into Rust.
        if let Some(len_str) = args.strip_prefix('\u{2}') {
            return Err(FsmError::invalid(format!(
                "the arguments to {name} are {len_str} bytes of JSON; the cap is {} MiB",
                self.payload_bytes >> 20
            )));
        }
        if args.len() > self.payload_bytes {
            return Err(FsmError::invalid(format!(
                "the arguments to {name} are {} bytes of JSON; the cap is {} MiB",
                args.len(),
                self.payload_bytes >> 20
            )));
        }
        let parsed: Json = serde_json::from_str(&args).map_err(|e| {
            FsmError::invalid(format!(
                "the arguments to {name} are not plain JSON data ({e})"
            ))
        })?;
        self.cx.charge_host_call()?;
        Ok(parsed)
    }

    /// Envelope JSON for the prelude: `{"ok":true,"tag","image","value"}` or
    /// `{"ok":false,"kind","message","slot"}` (the slot keeps the original
    /// `FsmError` for an uncaught failure).
    async fn dispatch_prepared(&self, name: String, args: Json) -> String {
        match self.forward(&name, args).await {
            Ok(f) => format!(
                r#"{{"ok":true,"tag":{},"image":{},"value":{}}}"#,
                json!(f.tag),
                json!(f.image),
                f.json
            ),
            Err(e) => self.error_envelope(e),
        }
    }

    async fn forward(&self, name: &str, args: Json) -> Result<Forwarded, FsmError> {
        if prelude::SANDBOX_NATIVE_FNS.contains(&name) {
            return Ok(self.sleep(&args).await);
        }
        let (host, cx, inflight, func) = (
            self.host.clone(),
            self.cx.clone(),
            self.inflight.clone(),
            name.to_owned(),
        );
        let task = AbortOnDrop(self.main.spawn(async move {
            let _permit = inflight
                .acquire_owned()
                .await
                .map_err(|_| FsmError::internal("sandbox", "host-call semaphore closed"))?;
            let value: HostValue = host.call(&func, args, &cx).await?;
            if let Some(b) = &value.binary {
                cx.images.lock().await.insert(b.id.clone(), b.clone());
            }
            Ok::<HostValue, FsmError>(value)
        }));
        let value = match task.await {
            Ok(r) => r?,
            Err(e) if e.is_panic() => {
                tracing::error!(func = name, "exec host function panicked");
                return Err(FsmError::internal("host_panic", format!("{name} panicked")));
            }
            Err(_) => {
                return Err(FsmError::internal(
                    "host_cancelled",
                    format!("{name} was cancelled"),
                ));
            }
        };
        let json = serde_json::to_string(&value.json)
            .map_err(|e| FsmError::internal("host_json", e.to_string()))?;
        if json.len() > self.payload_bytes {
            return Err(FsmError::BudgetExceeded(format!(
                "{name} returned {:.1} MiB of JSON; results over {} MiB do not enter the sandbox (ask for fewer items or less text)",
                json.len() as f64 / (1 << 20) as f64,
                self.payload_bytes >> 20
            )));
        }
        Ok(Forwarded {
            json,
            tag: value.render_kind.map(|k| k.as_str()),
            image: value.binary.map(|b| b.id),
        })
    }

    /// `util.sleep(ms)`: a local timer bounded by the wall clock. A delay that
    /// cannot end before the deadline never resolves early: it waits past the
    /// deadline, so the run reports `Timeout` instead of firing the timer.
    async fn sleep(&self, args: &Json) -> Forwarded {
        let ms = args
            .get(0)
            .and_then(Json::as_f64)
            .filter(|v| v.is_finite() && *v > 0.0)
            .unwrap_or(0.0);
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if ms >= remaining.as_millis() as f64 {
            tokio::time::sleep_until((self.deadline + GRACE).into()).await;
        } else {
            tokio::time::sleep(Duration::from_millis(ms as u64)).await;
        }
        Forwarded {
            json: "null".into(),
            tag: None,
            image: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_buffer_caps_and_clips() {
        let s = Shared::new(10);
        s.push_log("abc".into());
        s.push_log("défghij".into());
        s.push_log("never".into());
        let (lines, truncated) = s.take_logs();
        assert!(truncated);
        assert_eq!(lines[0], "abc");
        assert!(
            lines.len() == 2 && "défghij".starts_with(&lines[1]) && lines[1].len() < 8,
            "{lines:?}"
        );
        let s = Shared::new(0);
        s.push_log("x".into());
        assert_eq!(s.take_logs(), (vec![], true));
    }

    #[test]
    fn host_error_slots_are_taken_once() {
        let s = Shared::new(10);
        let slot = s.push_error(FsmError::Timeout(5));
        assert!(matches!(s.take_error(slot), Some(FsmError::Timeout(5))));
        assert!(s.take_error(slot).is_none());
        assert!(s.take_error(99).is_none());
    }

    #[test]
    fn host_error_ring_stays_bounded_and_preserves_latest() {
        let s = Shared::new(10);
        let first = s.push_error(FsmError::Timeout(1));
        let mut last = first;
        for i in 0..(MAX_ERROR_SLOTS + 100) {
            last = s.push_error(FsmError::Timeout(i as u64 + 10));
        }
        assert!(lock(&s.errors).slots.len() <= MAX_ERROR_SLOTS);
        // Overwritten old slot no longer returns a stale error.
        assert!(s.take_error(first).is_none());
        // Most recent error is preserved cleanly.
        assert!(matches!(
            s.take_error(last),
            Some(FsmError::Timeout(t)) if t == (MAX_ERROR_SLOTS + 109) as u64
        ));
    }
}
