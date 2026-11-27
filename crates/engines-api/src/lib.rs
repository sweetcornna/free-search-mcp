//! `fsm-engines-api` — 15 native API & keyed engines (`SPEC.md` §9.2, lane `W1c`).
//!
//! Engines registered by [`register`]:
//! - Dev & Registries: `github`, `github_code`, `github_releases`, `registries` (`"code"` scope per M16)
//! - Finance: `sec_edgar`, `yahoofinance`, `cninfo`
//! - Reference & Clock: `worldclock` (local), `openmeteo`, `rdap`, `wikidata`
//! - Keyed Web APIs: `brave_api`, `serper`, `serpbase`, `tavily`

pub mod brave_api;
pub mod cninfo;
pub mod common;
pub mod degoog;
mod instance_search;
pub mod searxng;
pub mod facts;
pub mod github;
pub mod github_releases;
pub mod openmeteo;
pub mod rdap;
pub mod registries;
pub mod sec_edgar;
pub mod serpbase;
pub mod serper;
pub mod tavily;
pub mod wikidata;
pub mod worldclock;
pub mod yahoofinance;

use fsm_core::{FsmError, Registry, SourceDeps};
use std::sync::Arc;

/// Register all 15 native API and keyed engines with `reg`.
pub fn register(reg: &mut Registry, deps: &SourceDeps) -> Result<(), FsmError> {
    reg.register(Arc::new(github::GitHubEngine::new(deps.clone())))?;
    reg.register(Arc::new(github::GitHubCodeEngine::new(deps.clone())))?;
    reg.register(Arc::new(github_releases::GitHubReleasesEngine::new(
        deps.clone(),
    )))?;
    reg.register(Arc::new(registries::RegistriesEngine::new(deps.clone())))?;
    reg.register(Arc::new(sec_edgar::SecEdgarEngine::new(deps.clone())))?;
    reg.register(Arc::new(yahoofinance::YahooFinanceEngine::new(
        deps.clone(),
    )))?;
    reg.register(Arc::new(cninfo::CninfoEngine::new(deps.clone())))?;
    reg.register(Arc::new(worldclock::WorldClockEngine::new()))?;
    reg.register(Arc::new(openmeteo::OpenMeteoEngine::new(deps.clone())))?;
    reg.register(Arc::new(rdap::RdapEngine::new(deps.clone())))?;
    reg.register(Arc::new(wikidata::WikidataEngine::new(deps.clone())))?;
    reg.register(Arc::new(brave_api::BraveApiEngine::new(deps.clone())))?;
    reg.register(Arc::new(serper::SerperEngine::new(deps.clone())))?;
    reg.register(Arc::new(serpbase::SerpBaseEngine::new(deps.clone())))?;
    reg.register(Arc::new(tavily::TavilyEngine::new(deps.clone())))?;
    Ok(())
}
