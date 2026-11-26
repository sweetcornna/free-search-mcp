//! `fsm-script`: the QuickJS-NG sandbox behind the `exec` tool (lane `W0d`,
//! `SPEC.md` §2.5 and §3, `SPEC-contracts.md` §1.5). Depends on `fsm-core` only.
//!
//! # Shape
//!
//! * [`Sandbox::run`] executes one script on a fresh OS thread with its own
//!   current-thread Tokio runtime, a fresh `AsyncRuntime` and a fresh
//!   `AsyncContext::full` (nothing is shared between runs). Limits: QuickJS heap
//!   ([`Limits::heap_bytes`], `rust-alloc` off so `set_memory_limit` governs the
//!   QuickJS allocator), JS stack, a sticky wall-clock interrupt handler that also
//!   watches the MCP cancel token (`CallCx::cancel`), and an outer
//!   `select!` that cuts a hanging `await`.
//! * Host calls: every async binding is `(...args) => __host(name, args)`. The
//!   bridge charges [`CallCx::charge_host_call`] once per call, queues on a
//!   per-exec semaphore ([`Limits::max_concurrency`]) and runs
//!   `HostServices::call(name, <JSON array of positional args>, cx)` on the
//!   caller's runtime behind an abort-on-drop handle. `util.sleep` (and the
//!   `setTimeout` shim built on it) is served by the sandbox itself, clamped to
//!   the remaining wall clock ([`SANDBOX_NATIVE_FNS`]). **`HostServices`
//!   implementations must not charge the budget again.**
//! * Output: [`ExecOutcome`] carries the raw JSON of the result plus the render
//!   decision ([`ExecValue`]): a value renders as Markdown only if it is (or is a
//!   non-empty array whose every element is) a host-tagged object that still
//!   round-trips losslessly through its `fsm-core` type. [`compose_reply`]
//!   turns an outcome into MCP content with nonced delimiters, token truncation
//!   and the raw-JSON `exec:<id>` overflow handle; Markdown itself comes from a
//!   caller-supplied [`Renderer`] (`fsm-surface`).

mod boundary;
mod engine;
mod normalize;
mod prelude;
mod reply;

pub use boundary::{ExecValue, validate_tagged};
pub use engine::{ExecFailure, ExecOutcome, Sandbox};
pub use normalize::{Candidate, Form, Normalized, normalize_code, strip_fences, strip_types};
pub use prelude::{PRELUDE_SYNC_FNS, SANDBOX_NATIVE_FNS};
pub use reply::{
    ExecReply, JsonRenderer, Renderer, ReplyOptions, ReplyPart, compose_reply, escape_delimiters,
};

use fsm_core::{CallCx, Config};
use std::time::{Duration, Instant};

/// Default `timeout_s` of the `exec` tool (`SPEC.md` §2.4).
pub const DEFAULT_TIMEOUT_S: u64 = 30;
/// Hard ceiling of `timeout_s`, whatever the configuration says.
pub const MAX_TIMEOUT_S: u64 = 120;
/// Default QuickJS heap (`SPEC.md` §3.3).
pub const DEFAULT_HEAP_BYTES: usize = 96 << 20;
/// Hard ceiling for the heap, whatever the configuration says.
pub const MAX_HEAP_BYTES: usize = 128 << 20;
/// Default JS stack (`SPEC.md` §3.3).
pub const DEFAULT_STACK_BYTES: usize = 1 << 20;
/// Hard ceiling for the JS stack.
pub const MAX_STACK_BYTES: usize = 2 << 20;
/// Default per-exec host-call concurrency; extra `Promise.all` calls queue.
pub const DEFAULT_MAX_CONCURRENCY: usize = 12;
/// Hard ceiling for per-exec host-call concurrency.
pub const MAX_CONCURRENCY: usize = 16;
/// `console.*` capture cap (`SPEC.md` §3.3).
pub const CONSOLE_BYTES: usize = 2 << 10;
/// Cap on any JSON payload crossing the bridge (host results, call
/// arguments, the script's result) before conversion (`SPEC.md` §3.3).
pub const PAYLOAD_BYTES: usize = 4 << 20;
/// Default global number of concurrent execs (`SPEC.md` §3.3).
pub const DEFAULT_CONCURRENT_EXECS: usize = 4;
/// Hard ceiling for concurrent execs.
pub const MAX_CONCURRENT_EXECS: usize = 8;

/// Per-exec resource limits (`SPEC.md` §3.3).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Limits {
    /// Wall clock for the whole run, host calls included.
    pub timeout: Duration,
    /// QuickJS heap limit (`set_memory_limit`).
    pub heap_bytes: usize,
    /// QuickJS stack limit (`set_max_stack_size`).
    pub stack_bytes: usize,
    /// Host calls in flight at once; the rest wait their turn.
    pub max_concurrency: usize,
    /// `console.*` bytes kept.
    pub console_bytes: usize,
    /// Largest JSON payload accepted across the bridge.
    pub payload_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_S),
            heap_bytes: DEFAULT_HEAP_BYTES,
            stack_bytes: DEFAULT_STACK_BYTES,
            max_concurrency: DEFAULT_MAX_CONCURRENCY,
            console_bytes: CONSOLE_BYTES,
            payload_bytes: PAYLOAD_BYTES,
        }
    }
}

impl Limits {
    /// Limits for one `exec` call: `timeout_s` (the tool argument, default 30)
    /// is clamped to `1..=cfg.exec_timeout_s`; the heap comes from
    /// `cfg.exec_heap_mb` (clamped to 16..=128 MiB).
    pub fn from_config(cfg: &Config, timeout_s: Option<u64>) -> Self {
        let ceiling = cfg.exec_timeout_s.clamp(1, MAX_TIMEOUT_S);
        let secs = timeout_s.unwrap_or(DEFAULT_TIMEOUT_S).clamp(1, ceiling);
        Self {
            timeout: Duration::from_secs(secs),
            heap_bytes: (cfg.exec_heap_mb.clamp(16, 128)) << 20,
            ..Self::default()
        }
    }

    /// The same limits with another wall clock.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Clamp every field to its hard ceiling (and to sane minimums).
    pub fn clamped(mut self) -> Self {
        self.timeout = self
            .timeout
            .clamp(Duration::from_millis(1), Duration::from_secs(MAX_TIMEOUT_S));
        self.heap_bytes = self.heap_bytes.clamp(1 << 20, MAX_HEAP_BYTES);
        self.stack_bytes = self.stack_bytes.clamp(64 << 10, MAX_STACK_BYTES);
        self.max_concurrency = self.max_concurrency.clamp(1, MAX_CONCURRENCY);
        self.console_bytes = self.console_bytes.clamp(0, 64 << 10);
        self.payload_bytes = self.payload_bytes.clamp(1 << 10, PAYLOAD_BYTES);
        self
    }
}

/// The per-exec [`CallCx`]: deadline `now + limits.timeout`, a fresh
/// delimiter nonce and the configured host-call budget.
pub fn exec_call_cx(session_id: impl Into<String>, cfg: &Config, limits: &Limits) -> CallCx {
    let mut cx = CallCx::new(session_id, Instant::now() + limits.timeout);
    cx.nonce = new_nonce();
    cx.max_host_calls = cfg.exec_max_host_calls;
    cx
}

const BASE36: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

fn base36(len: usize) -> String {
    (0..len)
        .map(|_| char::from(BASE36[rand::random_range(0..BASE36.len())]))
        .collect()
}

/// A 4-char base36 delimiter nonce (I14.3).
pub fn new_nonce() -> String {
    base36(4)
}

/// An 8-char base36 overflow id (`exec:<id>`, `SPEC.md` §2.5.3).
pub fn new_exec_id() -> String {
    base36(8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_follow_config_and_clamp() {
        let mut cfg = Config::default();
        let l = Limits::from_config(&cfg, None);
        assert_eq!(l.timeout, Duration::from_secs(30));
        assert_eq!(l.heap_bytes, 96 << 20);
        assert_eq!(l.stack_bytes, 1 << 20);
        assert_eq!(
            Limits::from_config(&cfg, Some(500)).timeout,
            Duration::from_secs(120)
        );
        assert_eq!(
            Limits::from_config(&cfg, Some(0)).timeout,
            Duration::from_secs(1)
        );
        cfg.exec_timeout_s = 20; // env may only lower the ceiling
        assert_eq!(
            Limits::from_config(&cfg, None).timeout,
            Duration::from_secs(20)
        );
        cfg.exec_heap_mb = 4096;
        assert_eq!(Limits::from_config(&cfg, None).heap_bytes, 128 << 20);
        let wild = Limits {
            max_concurrency: 99,
            stack_bytes: 1 << 30,
            ..Limits::default()
        }
        .clamped();
        assert_eq!((wild.max_concurrency, wild.stack_bytes), (16, 2 << 20));
    }

    /// Review round 1, M4: a configured ceiling above 120 s is still capped,
    /// so `cx.deadline` matches the deadline the sandbox enforces.
    #[test]
    fn r1_m4_from_config_never_exceeds_120_s() {
        let mut cfg = Config::default();
        cfg.exec_timeout_s = 500;
        let l = Limits::from_config(&cfg, Some(500));
        assert_eq!(l.timeout, Duration::from_secs(120));
        let cx = exec_call_cx("s", &cfg, &l);
        assert!(cx.deadline <= Instant::now() + Duration::from_secs(120));
    }

    #[test]
    fn nonce_and_exec_id_are_base36() {
        let n = new_nonce();
        let id = new_exec_id();
        assert_eq!((n.len(), id.len()), (4, 8));
        assert!(n.bytes().chain(id.bytes()).all(|b| BASE36.contains(&b)));
        let cfg = Config::default();
        let cx = exec_call_cx("s1", &cfg, &Limits::default());
        assert_eq!(cx.nonce.len(), 4);
        assert_eq!(cx.max_host_calls, cfg.exec_max_host_calls);
        assert!(cx.remaining() <= Duration::from_secs(30));
    }
}
