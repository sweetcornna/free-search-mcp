//! Extension point of lane `W3b` — Scholar Graph, Integrity, Lazy RW Sync, Cite & Entities (`fsm-scholar-graph`).
//!
//! # Unresolved `CitationGraph` contract (for `W0d` renderers & `W6a` host)
//!
//! `SPEC-contracts.md` defines `CitationGraph.paper` as non-optional
//! (`ScholarlyWork`). When `ScholarGraph::graph` cannot resolve the queried
//! identifier or title to any work, it returns `Ok(CitationGraph)` carrying
//! `paper == ScholarlyWork::default()` (empty `title` and empty `ids`) and
//! human-readable diagnostic explanations (plus up to three near-miss titles)
//! in `graph.notes`. Renderers and callers should check
//! [`is_unresolved_graph`] before rendering paper metadata fields.

use crate::CitationGraph;

/// Returns `true` when `ScholarGraph::graph` could not resolve the queried
/// identifier or title to a scholarly work.
///
/// Downstream renderers (`W0d`) and hosts (`W6a`) should display `graph.notes`
/// as a not-found diagnostic instead of rendering an empty paper header.
#[must_use]
pub fn is_unresolved_graph(graph: &CitationGraph) -> bool {
    graph.paper.title.trim().is_empty() && graph.paper.ids.is_empty()
}
