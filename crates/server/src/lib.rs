//! `fsm-server` — server assembly, MCP 3-tool handler (`search`, `exec`, `ask`), `HostServices`,
//! Streamable HTTP transport guards (`/mcp` + `/v1/ask`), and CLI (`free-search-mcp`).

pub mod assembly;
pub mod cli;
pub mod host;
pub mod http;
pub mod mcp;

pub use assembly::{
    LiveRuntime, assemble_live, assemble_live_for_transport, assemble_services, build_registry,
    build_registry_for_transport,
};
pub use host::{DISPATCHED_HOST_FNS, DefaultHostServices};
pub use http::{HttpSecurityConfig, build_http_router, build_http_security, serve_http};
pub use mcp::{
    AppServer, AskToolInput, ExecToolInput, SearchToolInput, TransportKind, server_implementation,
};

use async_trait::async_trait;
use fsm_catalog::CatalogService;
use fsm_core::{
    CallCx, CatalogDetail, CatalogHit, CatalogQuery, CitationGraph, CiteInput, CiteStyle,
    EntityKind, FsmError, FulltextOpts, FulltextResult, GraphOpts, IntegrityReport, ProtocolOutput,
    ScholarApi, ScholarQuery, ScholarlyWork, SourceOutput, SourcesApi, WorkResult,
};
use fsm_scholar::ScholarCore;
use fsm_scholar_fulltext::ScholarFulltext;
use fsm_scholar_graph::ScholarGraph;
use fsm_sources::SourceDispatcher;
use serde_json::Value;
use std::sync::Arc;

/// `ScholarApi` assembled from the three scholar crates (`SPEC-contracts.md` §5.2).
pub struct CompositeScholarApi {
    pub core: Arc<ScholarCore>,
    pub graph: Arc<ScholarGraph>,
    pub ft: Arc<ScholarFulltext>,
}

#[async_trait]
impl ScholarApi for CompositeScholarApi {
    async fn search(&self, q: ScholarQuery, cx: &CallCx) -> Result<WorkResult, FsmError> {
        self.graph.kick_retraction_sync();
        self.core.search(q, cx).await
    }

    async fn work(
        &self,
        id_or_title: &str,
        cx: &CallCx,
    ) -> Result<Option<ScholarlyWork>, FsmError> {
        self.graph.kick_retraction_sync();
        self.core.work(id_or_title, cx).await
    }

    async fn graph(
        &self,
        id_or_title: &str,
        opts: GraphOpts,
        cx: &CallCx,
    ) -> Result<CitationGraph, FsmError> {
        self.graph.graph(id_or_title, opts, cx).await
    }

    async fn fulltext(
        &self,
        id_or_doi: &str,
        opts: FulltextOpts,
        cx: &CallCx,
    ) -> Result<FulltextResult, FsmError> {
        self.ft.fulltext(id_or_doi, opts, cx).await
    }

    async fn integrity(&self, doi_or_id: &str, cx: &CallCx) -> Result<IntegrityReport, FsmError> {
        self.graph.integrity(doi_or_id, cx).await
    }

    async fn cite(
        &self,
        input: CiteInput,
        style: CiteStyle,
        cx: &CallCx,
    ) -> Result<String, FsmError> {
        self.graph.cite(input, style, cx).await
    }

    async fn entity(
        &self,
        kind: EntityKind,
        query: &str,
        opts: Value,
        cx: &CallCx,
    ) -> Result<Value, FsmError> {
        self.graph.entity(kind, query, opts, cx).await
    }
}

/// `SourcesApi` assembled from the dispatcher and the catalog (`SPEC-contracts.md` §5.2).
pub struct CompositeSourcesApi {
    pub disp: Arc<SourceDispatcher>,
    pub catalog: Arc<CatalogService>,
}

#[async_trait]
impl SourcesApi for CompositeSourcesApi {
    async fn call(
        &self,
        op_id: &str,
        params: Value,
        cx: &CallCx,
    ) -> Result<SourceOutput, FsmError> {
        self.disp.call(op_id, params, cx).await
    }

    async fn protocol(
        &self,
        kind: &str,
        base_url: &str,
        op: &str,
        params: Value,
        cx: &CallCx,
    ) -> Result<ProtocolOutput, FsmError> {
        self.disp.protocol(kind, base_url, op, params, cx).await
    }

    async fn catalog_search(&self, q: CatalogQuery) -> Result<Vec<CatalogHit>, FsmError> {
        self.catalog.search(q).await
    }

    async fn catalog_describe(&self, id: &str) -> Result<CatalogDetail, FsmError> {
        self.catalog.describe(id).await
    }
}
