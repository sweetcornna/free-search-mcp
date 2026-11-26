//! `fsm-protocols-b` — generic protocol adapters, part 2 (data/web; owner: lane `W2c`).
//!
//! Registers 9 [`fsm_core::ProtocolAdapter`] implementations matching `SPEC-addendum-A1.md` §E Part 2:
//! `ckan`, `socrata`, `opendatasoft`, `discourse`, `wordpress_rest`, `rss_atom`,
//! `activitypub_fediverse`, `shopify_json`, and `sitemap_llms`.

pub mod activitypub_fediverse;
pub mod ckan;
pub mod discourse;
pub mod opendatasoft;
pub mod rss_atom;
pub mod shopify_json;
pub mod sitemap_llms;
pub mod socrata;
mod util;
pub mod wordpress_rest;

use fsm_core::{FsmError, Registry, SourceDeps};
use std::sync::Arc;

/// Register every protocol adapter of this crate with the registry (`reg.register_protocol`).
/// Instance tables are registered by `fsm_sources::register`, which must run after this.
pub fn register(reg: &mut Registry, deps: &SourceDeps) -> Result<(), FsmError> {
    let net = Arc::clone(&deps.net);
    reg.register_protocol(Arc::new(ckan::CkanAdapter::new(Arc::clone(&net))))?;
    reg.register_protocol(Arc::new(socrata::SocrataAdapter::new(Arc::clone(&net))))?;
    reg.register_protocol(Arc::new(opendatasoft::OpenDataSoftAdapter::new(
        Arc::clone(&net),
    )))?;
    reg.register_protocol(Arc::new(discourse::DiscourseAdapter::new(Arc::clone(&net))))?;
    reg.register_protocol(Arc::new(wordpress_rest::WordPressRestAdapter::new(
        Arc::clone(&net),
    )))?;
    reg.register_protocol(Arc::new(rss_atom::RssAtomAdapter::new(Arc::clone(&net))))?;
    reg.register_protocol(Arc::new(
        activitypub_fediverse::ActivityPubFediverseAdapter::new(Arc::clone(&net)),
    ))?;
    reg.register_protocol(Arc::new(shopify_json::ShopifyJsonAdapter::new(Arc::clone(
        &net,
    ))))?;
    reg.register_protocol(Arc::new(sitemap_llms::SitemapLlmsAdapter::new(net)))?;
    Ok(())
}
