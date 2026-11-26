//! `fsm-scholar-graph` — citation graph, integrity, Retraction Watch lazy sync,
//! citations, and scholarly entity lookups (owner: lane `W3b`, `SPEC-lanes.md`).
//!
//! Public [`ScholarGraph`] signatures are frozen by `SPEC-contracts.md` §5.2.

pub mod cite;
pub mod entity;
pub mod graph;
pub mod ids;
pub mod integrity;
pub mod rw_sync;

use fsm_core::{
    CallCx, CitationGraph, CiteInput, CiteStyle, EntityKind, FsmError, GraphOpts, IntegrityReport,
    SourceDeps, SourcesApi,
};
use fsm_scholar::ScholarCore;
use serde_json::Value;
use std::sync::Arc;

/// Graph, integrity, Retraction Watch sync, citation, and entity lookups.
pub struct ScholarGraph {
    core: Arc<ScholarCore>,
    sources: Arc<dyn SourcesApi>,
    deps: SourceDeps,
    rw_sync_lock: Arc<tokio::sync::Mutex<()>>,
}

impl ScholarGraph {
    pub fn new(core: Arc<ScholarCore>, sources: Arc<dyn SourcesApi>, deps: SourceDeps) -> Self {
        Self {
            core,
            sources,
            deps,
            rw_sync_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub async fn graph(
        &self,
        id_or_title: &str,
        opts: GraphOpts,
        cx: &CallCx,
    ) -> Result<CitationGraph, FsmError> {
        self.kick_retraction_sync();
        graph::run_graph(&self.core, &self.deps, id_or_title, opts, cx).await
    }

    pub async fn integrity(
        &self,
        doi_or_id: &str,
        cx: &CallCx,
    ) -> Result<IntegrityReport, FsmError> {
        self.kick_retraction_sync();
        integrity::run_integrity(&self.core, &self.deps, &self.sources, doi_or_id, cx).await
    }

    pub async fn cite(
        &self,
        input: CiteInput,
        style: CiteStyle,
        cx: &CallCx,
    ) -> Result<String, FsmError> {
        cite::run_cite(&self.core, &self.deps, input, style, cx).await
    }

    pub async fn entity(
        &self,
        kind: EntityKind,
        query: &str,
        opts: Value,
        cx: &CallCx,
    ) -> Result<Value, FsmError> {
        entity::run_entity(&self.sources, kind, query, opts, cx).await
    }

    /// Spawn a detached background Retraction Watch CSV sync (if `cfg.offline_sync`
    /// is enabled and no sync is already running in this process).
    ///
    /// Uses its own detached deadline ([`rw_sync::RW_SYNC_TIMEOUT`]) and
    /// cancellation token so the ~65 MiB CSV download never starves the caller's
    /// `CallCx`. Called automatically at the start of [`graph`](Self::graph) and
    /// [`integrity`](Self::integrity); sibling lanes (`W3a`/`W6a`) may also call
    /// it on the first `scholar.search` / `scholar.work`.
    pub fn kick_retraction_sync(&self) {
        if !self.deps.cfg.offline_sync {
            return;
        }
        // Acquire the intra-process lock synchronously before spawning so rapid
        // back-to-back calls cannot race into duplicate downloads.
        let Ok(guard) = self.rw_sync_lock.clone().try_lock_owned() else {
            return;
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let deps = self.deps.clone();
            handle.spawn(async move {
                let cx = CallCx::new("", std::time::Instant::now() + rw_sync::RW_SYNC_TIMEOUT);
                let _ = rw_sync::ensure_synced_with_guard(&deps, guard, &cx).await;
            });
        }
    }

    /// Run the lazy Retraction Watch CSV sync synchronously and await its
    /// [`rw_sync::SyncOutcome`]. Useful for tests, CLI maintenance commands, and
    /// explicit warmup.
    pub async fn sync_retractions(&self, cx: &CallCx) -> rw_sync::SyncOutcome {
        rw_sync::ensure_synced(&self.deps, &self.rw_sync_lock, cx).await
    }
}
