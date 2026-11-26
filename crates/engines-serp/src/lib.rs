//! `fsm-engines-serp` — 13 native SERP engines (12 legacy + `daum`, owner: lane `W1b`).
//!
//! Exports [`register`], registering:
//! `duckduckgo`, `bing`, `yahoojp`, `yahoo`, `seznam`, `so360`, `quark`,
//! `naver`, `daum`, `baidu`, `googlenews`, `youcom`, and `bilibili`.

pub mod coherence;
pub mod common;
pub mod cookie_jar;
pub mod engines;
pub mod gnews;

use engines::baidu::BaiduEngine;
use engines::bilibili::BilibiliEngine;
use engines::bing::BingEngine;
use engines::daum::DaumEngine;
use engines::duckduckgo::DuckDuckGoEngine;
use engines::googlenews::GoogleNewsEngine;
use engines::naver::NaverEngine;
use engines::quark::QuarkEngine;
use engines::seznam::SeznamEngine;
use engines::so360::So360Engine;
use engines::yahoo::YahooEngine;
use engines::yahoojp::YahooJapanEngine;
use engines::youcom::YouComEngine;
use fsm_core::{FsmError, Registry, Source, SourceDeps};
use std::sync::Arc;

/// Canonical IDs of all 13 native SERP engines in registration order.
pub const ENGINE_IDS: &[&str] = &[
    "duckduckgo",
    "bing",
    "yahoojp",
    "yahoo",
    "seznam",
    "so360",
    "quark",
    "naver",
    "daum",
    "baidu",
    "googlenews",
    "youcom",
    "bilibili",
];

/// Register all 13 native SERP engines with the registry.
pub fn register(reg: &mut Registry, deps: &SourceDeps) -> Result<(), FsmError> {
    let sources: Vec<Arc<dyn Source>> = vec![
        Arc::new(DuckDuckGoEngine::new(deps.clone())),
        Arc::new(BingEngine::new(deps.clone())),
        Arc::new(YahooJapanEngine::new(deps.clone())),
        Arc::new(YahooEngine::new(deps.clone())),
        Arc::new(SeznamEngine::new(deps.clone())),
        Arc::new(So360Engine::new(deps.clone())),
        Arc::new(QuarkEngine::new(deps.clone())),
        Arc::new(NaverEngine::new(deps.clone())),
        Arc::new(DaumEngine::new(deps.clone())),
        Arc::new(BaiduEngine::new(deps.clone())),
        Arc::new(GoogleNewsEngine::new(deps.clone())),
        Arc::new(YouComEngine::new(deps.clone())),
        Arc::new(BilibiliEngine::new(deps.clone())),
    ];
    for src in sources {
        reg.register(src)?;
    }
    Ok(())
}
