//! `fsm-engines-claims` — 14 native direct-answer / claim engines (owner: lane `W1d`, `SPEC-lanes.md`).
//!
//! Exports [`register`] matching [`fsm_core::RegisterFn`], registering all 14 engines in
//! Python's canonical precedence order so tie-broken direct-fact routing (`endoflife` before
//! `pypi`/`npm`, `osv` before `cisakev`, `frankfurter` before `cfets`) stays deterministic.

pub mod appstore;
pub mod cfets;
pub mod cisakev;
pub mod coingecko;
pub mod crates_io;
pub mod endoflife;
pub mod facts;
pub mod frankfurter;
pub mod fuzzy;
pub mod gleif;
pub mod holidays;
mod http;
pub mod imf;
pub mod npm;
pub mod osv;
pub mod pypi;
pub mod wdi;

use fsm_core::{FsmError, Registry, Source, SourceDeps};
use std::sync::Arc;

/// Canonical registration order of the 14 engines in `fsm-engines-claims`.
pub const ENGINE_IDS: &[&str] = &[
    "imf",
    "endoflife",
    "pypi",
    "npm",
    "crates",
    "appstore",
    "osv",
    "cisakev",
    "wdi",
    "holidays",
    "frankfurter",
    "cfets",
    "gleif",
    "coingecko",
];

/// Register all 14 native direct-answer / claim engines with the registry.
pub fn register(reg: &mut Registry, deps: &SourceDeps) -> Result<(), FsmError> {
    let sources: Vec<Arc<dyn Source>> = vec![
        Arc::new(imf::ImfEngine::new(deps.clone())),
        Arc::new(endoflife::EndOfLifeEngine::new(deps.clone())),
        Arc::new(pypi::PypiEngine::new(deps.clone())),
        Arc::new(npm::NpmEngine::new(deps.clone())),
        Arc::new(crates_io::CratesEngine::new(deps.clone())),
        Arc::new(appstore::AppStoreEngine::new(deps.clone())),
        Arc::new(osv::OsvEngine::new(deps.clone())),
        Arc::new(cisakev::CisaKevEngine::new(deps.clone())),
        Arc::new(wdi::WdiEngine::new(deps.clone())),
        Arc::new(holidays::HolidaysEngine::new(deps.clone())),
        Arc::new(frankfurter::FrankfurterEngine::new(deps.clone())),
        Arc::new(cfets::CfetsEngine::new(deps.clone())),
        Arc::new(gleif::GleifEngine::new(deps.clone())),
        Arc::new(coingecko::CoinGeckoEngine::new(deps.clone())),
    ];
    for src in sources {
        reg.register(src)?;
    }
    Ok(())
}
