//! `fsm-protocols-a` — generic protocol adapters, part 1 (+ `mediawiki`) (owner: lane `W2b`, `SPEC-lanes.md`).
//!
//! Implements the 9 academic, repository, and wiki [`ProtocolAdapter`](fsm_core::ProtocolAdapter)s
//! specified in `SPEC-addendum-A1.md` §E Part 1:
//! `oai_pmh`, `sru`, `opensearch`, `sparql`, `dspace7`, `inveniordm`, `dataverse`, `eprints`, `mediawiki`.

mod common;
mod dataverse;
mod dspace7;
mod eprints;
mod inveniordm;
mod mediawiki;
mod oai_pmh;
mod opensearch;
mod sparql;
mod sru;

use fsm_core::{FsmError, Registry, SourceDeps};
use std::sync::Arc;

/// Register every protocol adapter of this crate with the registry (`reg.register_protocol`).
/// Instance tables are registered by `fsm_sources::register`, which must run after this.
pub fn register(reg: &mut Registry, deps: &SourceDeps) -> Result<(), FsmError> {
    reg.register_protocol(Arc::new(oai_pmh::OaiPmhAdapter::new(deps.clone())))?;
    reg.register_protocol(Arc::new(sru::SruAdapter::new(deps.clone())))?;
    reg.register_protocol(Arc::new(opensearch::OpenSearchAdapter::new(deps.clone())))?;
    reg.register_protocol(Arc::new(sparql::SparqlAdapter::new(deps.clone())))?;
    reg.register_protocol(Arc::new(dspace7::Dspace7Adapter::new(deps.clone())))?;
    reg.register_protocol(Arc::new(inveniordm::InvenioRdmAdapter::new(deps.clone())))?;
    reg.register_protocol(Arc::new(dataverse::DataverseAdapter::new(deps.clone())))?;
    reg.register_protocol(Arc::new(eprints::EprintsAdapter::new(deps.clone())))?;
    reg.register_protocol(Arc::new(mediawiki::MediaWikiAdapter::new(deps.clone())))?;
    Ok(())
}
