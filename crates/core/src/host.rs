//! Host sandbox bridge (`SPEC-contracts.md` §1.5).
//!
//! `crates/surface` owns the `HOST_API` table (prose, examples); core owns
//! only the row type, the render tags, the per-exec call context and the
//! dispatch trait.

use crate::error::FsmError;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// One host function exposed to exec scripts.
///
/// `#[non_exhaustive]`: build rows in `const` context with [`HostFn::new`]
/// and the `const` `with_*` builders:
///
/// ```
/// use fsm_core::HostFn;
/// const FETCH: HostFn = HostFn::new("web.fetch", "fetch(url: string): Promise<PageResult>", "Fetch a page.", "web.fetch(u)")
///     .with_in_description(true)
///     .with_egress(true);
/// assert!(FETCH.egress && !FETCH.is_prelude_sync);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct HostFn {
    pub name: &'static str,
    pub sig: &'static str,
    pub doc: &'static str,
    pub example: &'static str,
    /// Listed in the `exec` tool description (else discoverable via catalog).
    pub in_description: bool,
    /// Implemented in the JS prelude (sync helper), not via `HostServices`.
    pub is_prelude_sync: bool,
    /// M6: true for every network-outbound function; refused centrally once
    /// the exec is tainted by a local document read.
    pub egress: bool,
}

impl HostFn {
    pub const fn new(
        name: &'static str,
        sig: &'static str,
        doc: &'static str,
        example: &'static str,
    ) -> Self {
        Self {
            name,
            sig,
            doc,
            example,
            in_description: false,
            is_prelude_sync: false,
            egress: false,
        }
    }

    pub const fn with_in_description(mut self, yes: bool) -> Self {
        self.in_description = yes;
        self
    }

    pub const fn with_prelude_sync(mut self, yes: bool) -> Self {
        self.is_prelude_sync = yes;
        self
    }

    pub const fn with_egress(mut self, yes: bool) -> Self {
        self.egress = yes;
        self
    }

    /// Namespace half of the name (`"web"` for `"web.fetch"`).
    pub fn namespace(&self) -> &'static str {
        self.name.split_once('.').map_or(self.name, |(ns, _)| ns)
    }
}

/// Markdown renderer tag attached to host results (`SPEC.md` §2.5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum RenderKind {
    #[default]
    PageResult,
    PageList,
    DocSlice,
    FulltextResult,
    SearchResponse,
    WorkResult,
    WorkList,
    ResearchBrief,
    CitationGraph,
    CatalogList,
    InlineImage,
    AskResponse,
}

impl RenderKind {
    /// All 12 tags.
    pub const ALL: [RenderKind; 12] = [
        Self::PageResult,
        Self::PageList,
        Self::DocSlice,
        Self::FulltextResult,
        Self::SearchResponse,
        Self::WorkResult,
        Self::WorkList,
        Self::ResearchBrief,
        Self::CitationGraph,
        Self::CatalogList,
        Self::InlineImage,
        Self::AskResponse,
    ];

    /// Tag string used by the prelude's private `KIND` symbol.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PageResult => "PageResult",
            Self::PageList => "PageList",
            Self::DocSlice => "DocSlice",
            Self::FulltextResult => "FulltextResult",
            Self::SearchResponse => "SearchResponse",
            Self::WorkResult => "WorkResult",
            Self::WorkList => "WorkList",
            Self::ResearchBrief => "ResearchBrief",
            Self::CitationGraph => "CitationGraph",
            Self::CatalogList => "CatalogList",
            Self::InlineImage => "InlineImage",
            Self::AskResponse => "AskResponse",
        }
    }

    pub fn from_tag(tag: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == tag)
    }
}

/// Opaque binary payload (inline images) kept host-side.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct BinaryHandle {
    pub id: String,
    pub media_type: String,
    pub bytes: bytes::Bytes,
}

impl BinaryHandle {
    pub fn new(id: impl Into<String>, media_type: impl Into<String>, bytes: bytes::Bytes) -> Self {
        Self {
            id: id.into(),
            media_type: media_type.into(),
            bytes,
        }
    }
}

/// What a host call hands back to the sandbox.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct HostValue {
    pub json: Value,
    pub render_kind: Option<RenderKind>,
    pub binary: Option<BinaryHandle>,
}

impl HostValue {
    pub fn new(json: Value) -> Self {
        Self {
            json,
            render_kind: None,
            binary: None,
        }
    }

    /// A value the boundary may render as Markdown under `kind`.
    pub fn tagged(json: Value, kind: RenderKind) -> Self {
        Self {
            json,
            render_kind: Some(kind),
            binary: None,
        }
    }

    pub fn with_binary(mut self, binary: BinaryHandle) -> Self {
        self.binary = Some(binary);
        self
    }
}

/// Default per-exec wall clock (`SPEC.md` §3.3).
pub const DEFAULT_EXEC_TIMEOUT: Duration = Duration::from_secs(30);
/// Default per-exec host-call budget (`SPEC.md` §3.3).
pub const DEFAULT_MAX_HOST_CALLS: u32 = 100;

/// Per-exec call context shared by every host call of one script run.
#[derive(Debug)]
#[non_exhaustive]
pub struct CallCx {
    pub session_id: String,
    pub nonce: String,
    pub cancel: CancellationToken,
    pub deadline: Instant,
    pub host_calls: Arc<AtomicU32>,
    pub max_host_calls: u32,
    pub downloads_used: Arc<AtomicU32>,
    pub local_doc_tainted: Arc<AtomicBool>,
    pub fetched_hosts: tokio::sync::Mutex<HashSet<String>>,
    pub progress_tx: Option<tokio::sync::mpsc::Sender<(u64, u64, String)>>,
    pub images: tokio::sync::Mutex<HashMap<String, BinaryHandle>>,
}

impl Default for CallCx {
    /// Session `""`, deadline now + 30 s, 100 host calls.
    fn default() -> Self {
        Self::new(String::new(), Instant::now() + DEFAULT_EXEC_TIMEOUT)
    }
}

impl CallCx {
    pub fn new(session_id: impl Into<String>, deadline: Instant) -> Self {
        Self {
            session_id: session_id.into(),
            nonce: String::new(),
            cancel: CancellationToken::new(),
            deadline,
            host_calls: Arc::new(AtomicU32::new(0)),
            max_host_calls: DEFAULT_MAX_HOST_CALLS,
            downloads_used: Arc::new(AtomicU32::new(0)),
            local_doc_tainted: Arc::new(AtomicBool::new(false)),
            fetched_hosts: tokio::sync::Mutex::new(HashSet::new()),
            progress_tx: None,
            images: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Time left before the deadline (zero once passed).
    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    /// Count one host call; `BudgetExceeded` once `max_host_calls` is used up.
    pub fn charge_host_call(&self) -> Result<u32, FsmError> {
        let n = self.host_calls.fetch_add(1, Ordering::SeqCst) + 1;
        if n > self.max_host_calls {
            return Err(FsmError::BudgetExceeded(format!(
                "host call budget of {} used up; batch work with Promise.all or fetchBatch",
                self.max_host_calls
            )));
        }
        Ok(n)
    }

    /// M6: mark this exec as having read a local document.
    pub fn mark_local_doc_read(&self) {
        self.local_doc_tainted.store(true, Ordering::SeqCst);
    }

    pub fn is_tainted(&self) -> bool {
        self.local_doc_tainted.load(Ordering::SeqCst)
    }
}

/// Dispatches `__host(name, args)` calls from the sandbox.
#[async_trait::async_trait]
pub trait HostServices: Send + Sync {
    async fn call(&self, func: &str, args: Value, cx: &CallCx) -> Result<HostValue, FsmError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROW: HostFn = HostFn::new(
        "util.dedupe",
        "dedupe<T>(items: T[]): T[]",
        "Drop duplicates.",
        "util.dedupe(xs)",
    )
    .with_prelude_sync(true);

    #[test]
    fn const_rows_and_namespace() {
        const { assert!(ROW.is_prelude_sync && !ROW.egress && !ROW.in_description) };
        assert_eq!(ROW.namespace(), "util");
        assert_eq!(HostFn::default().name, "");
    }

    #[test]
    fn render_tags_round_trip() {
        assert_eq!(RenderKind::ALL.len(), 12);
        for k in RenderKind::ALL {
            assert_eq!(RenderKind::from_tag(k.as_str()), Some(k));
        }
        assert_eq!(RenderKind::from_tag("Nope"), None);
    }

    #[test]
    fn host_call_budget_and_taint() {
        let cx = CallCx {
            max_host_calls: 2,
            ..CallCx::default()
        };
        assert_eq!(cx.charge_host_call().unwrap(), 1);
        assert_eq!(cx.charge_host_call().unwrap(), 2);
        assert!(matches!(
            cx.charge_host_call(),
            Err(FsmError::BudgetExceeded(_))
        ));
        assert!(!cx.is_tainted());
        cx.mark_local_doc_read();
        assert!(cx.is_tainted());
        assert!(cx.remaining() <= DEFAULT_EXEC_TIMEOUT);
        let v = HostValue::tagged(serde_json::json!({"url": "u"}), RenderKind::PageResult);
        assert_eq!(v.render_kind, Some(RenderKind::PageResult));
    }
}
