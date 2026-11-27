//! Service composition root and registry assembly (`SPEC-contracts.md` §5.2, `SPEC.md` §4–§8).
//!
//! Registration order is fixed by contract:
//! 1. Native engines (`fsm-engines-serp`, `fsm-engines-api`, `fsm-engines-claims`)
//! 2. Generic protocol adapters (`fsm-protocols-a`, `fsm-protocols-b`)
//! 3. Declarative manifests + user `sources.d/*.toml` + protocol instances (`fsm-sources`)

use crate::{CompositeScholarApi, CompositeSourcesApi};
use fsm_agent::{AskService, register_agent_engines};
use fsm_catalog::CatalogService;
use fsm_core::{
    AskApi, Config, DocApi, FetchApi, FsmError, NetClient, Registry, ScholarApi, SearchApi,
    Services, SourceDeps, SourcesApi, StoreApi, TransportProfile,
};
use fsm_docs::DocService;
use fsm_extract::FetchService;
use fsm_net::HttpClient;
use fsm_scholar::ScholarCore;
use fsm_scholar_fulltext::ScholarFulltext;
use fsm_scholar_graph::ScholarGraph;
use fsm_search::SearchService;
use fsm_sources::SourceDispatcher;
use fsm_store::SqliteStore;
use std::sync::Arc;

/// Register every native engine, protocol adapter, embedded manifest, user override,
/// protocol instance, and OAuth-backed agent engine (`codex`, `antigravity`) into a fresh [`Registry`].
pub fn build_registry(deps: &SourceDeps) -> Result<Registry, FsmError> {
    let is_http = deps
        .cfg
        .transport
        .trim()
        .eq_ignore_ascii_case("streamable-http");
    build_registry_for_transport(deps, is_http)
}

/// Same as [`build_registry`], with explicit transport flag so `codex` never
/// auto-opens a browser sign-in window on Streamable HTTP.
pub fn build_registry_for_transport(
    deps: &SourceDeps,
    is_http_transport: bool,
) -> Result<Registry, FsmError> {
    let mut reg = Registry::new();

    // 1. Native engines (42 total: 13 SERP + 15 API + 14 direct-claim).
    fsm_engines_serp::register(&mut reg, deps)?;
    fsm_engines_api::register(&mut reg, deps)?;
    fsm_engines_claims::register(&mut reg, deps)?;

    // Instance endpoints are opt-in and cannot be supplied by a CWD dotenv.
    if let Some(base_url) = &deps.cfg.searxng_base_url {
        reg.register(Arc::new(fsm_engines_api::searxng::SearxngEngine::new(
            deps.clone(),
            base_url.clone(),
        )))?;
    }
    if let Some(base_url) = &deps.cfg.degoog_base_url {
        reg.register(Arc::new(fsm_engines_api::degoog::DegoogEngine::new(
            deps.clone(),
            base_url.clone(),
        )))?;
    }

    // 1b. Agent-search engines (codex, antigravity; ruling R5).
    register_agent_engines(&mut reg, deps, is_http_transport)?;

    // 2. Generic protocol adapters (18 total: 9 academic/repo + 9 data/web).
    // Must precede `fsm_sources::register` so instance tables find their adapters.
    fsm_protocols_a::register(&mut reg, deps)?;
    fsm_protocols_b::register(&mut reg, deps)?;

    // 3. Embedded declarative manifests, user `sources.d/*.toml`, and protocol instances.
    fsm_sources::register(&mut reg, deps)?;

    Ok(reg)
}

/// Apply per-source GCRA rate limits from the registry onto an [`HttpClient`].
pub fn configure_rate_limits(http: &HttpClient, reg: &Registry, cfg: &Config) {
    let limits = http.rate_limits();
    for src in reg.all() {
        let decl = src.decl();
        if decl.transport == TransportProfile::Local {
            continue;
        }
        let key = decl
            .limits
            .host_bucket
            .as_deref()
            .filter(|k| !k.trim().is_empty())
            .unwrap_or(decl.id.as_str());
        if decl.limits.rps.is_finite() && decl.limits.rps > 0.0 {
            limits.configure(key, decl.limits.rps, decl.limits.burst.max(1));
        } else if cfg.rate_limit_rpm > 0 {
            limits.configure_per_minute(key, f64::from(cfg.rate_limit_rpm), None);
        }
    }
}

/// Assemble the full [`Services`] bundle from injected `cfg`, `net`, and `store`.
///
/// Works identically for live execution (`HttpClient` + `SqliteStore`) and offline
/// replay/unit tests (`ReplayNet` + `NullStore` or temp `SqliteStore`).
pub fn assemble_services(
    cfg: Arc<Config>,
    net: Arc<dyn NetClient>,
    store: Arc<dyn StoreApi>,
) -> Result<Services, FsmError> {
    let deps = SourceDeps::new(Arc::clone(&net), Arc::clone(&store), Arc::clone(&cfg));
    let registry = Arc::new(build_registry(&deps)?);
    assemble_services_with_registry(deps, registry)
}

/// Assemble [`Services`] from an already-constructed [`Registry`] and [`SourceDeps`].
pub fn assemble_services_with_registry(
    deps: SourceDeps,
    registry: Arc<Registry>,
) -> Result<Services, FsmError> {
    let search_svc = Arc::new(SearchService::new(Arc::clone(&registry), deps.clone()));
    let search: Arc<dyn SearchApi> = search_svc;

    let doc_svc = Arc::new(DocService::new(deps.clone()));
    let doc: Arc<dyn DocApi> = doc_svc;

    let fetch_svc = Arc::new(FetchService::new(
        deps.clone(),
        Arc::clone(&doc),
        Arc::clone(&search),
    ));
    let fetch: Arc<dyn FetchApi> = fetch_svc;

    let disp = Arc::new(SourceDispatcher::new(Arc::clone(&registry), deps.clone()));
    let catalog = Arc::new(CatalogService::new(
        Arc::clone(&registry),
        fsm_surface::HOST_API,
        Arc::clone(&deps.store),
    ));
    let sources: Arc<dyn SourcesApi> = Arc::new(CompositeSourcesApi { disp, catalog });

    let scholar_core = Arc::new(ScholarCore::new(Arc::clone(&registry), deps.clone()));
    let scholar_graph = Arc::new(ScholarGraph::new(
        Arc::clone(&scholar_core),
        Arc::clone(&sources),
        deps.clone(),
    ));
    let scholar_ft = Arc::new(ScholarFulltext::new(
        Arc::clone(&scholar_core),
        Arc::clone(&doc),
        Arc::clone(&fetch),
        deps.clone(),
    ));
    let scholar: Arc<dyn ScholarApi> = Arc::new(CompositeScholarApi {
        core: scholar_core,
        graph: scholar_graph,
        ft: scholar_ft,
    });

    let ask: Arc<dyn AskApi> = Arc::new(AskService::new(
        Arc::clone(&search),
        Arc::clone(&fetch),
        Arc::clone(&doc),
        Arc::clone(&registry),
        Arc::clone(&deps.cfg),
    ));

    Ok(Services::new(
        search, scholar, fetch, doc, sources, deps.store, registry, deps.cfg,
    )
    .with_ask(ask))
}

/// Live runtime bundle returned by [`assemble_live`].
pub struct LiveRuntime {
    pub services: Services,
    pub http: Arc<HttpClient>,
    pub store: Arc<SqliteStore>,
}

/// Assemble a live server runtime backed by [`HttpClient`] and [`SqliteStore`].
pub fn assemble_live(cfg: Arc<Config>) -> Result<LiveRuntime, FsmError> {
    let is_http = cfg.transport.trim().eq_ignore_ascii_case("streamable-http");
    assemble_live_for_transport(cfg, is_http)
}

/// Assemble a live server runtime with explicit HTTP transport flag.
pub fn assemble_live_for_transport(
    cfg: Arc<Config>,
    is_http_transport: bool,
) -> Result<LiveRuntime, FsmError> {
    let store = Arc::new(SqliteStore::open(&cfg)?);
    let http = Arc::new(HttpClient::new(&cfg)?);
    let net: Arc<dyn NetClient> = Arc::clone(&http) as Arc<dyn NetClient>;
    let store_api: Arc<dyn StoreApi> = Arc::clone(&store) as Arc<dyn StoreApi>;
    let deps = SourceDeps::new(net, store_api, Arc::clone(&cfg));
    let registry = Arc::new(build_registry_for_transport(&deps, is_http_transport)?);
    configure_rate_limits(&http, &registry, &cfg);
    let services = assemble_services_with_registry(deps, registry)?;
    Ok(LiveRuntime {
        services,
        http,
        store,
    })
}

/// Default warm-up targets for `SEARCH_MCP_PREWARM=true` (fast, non-blocking).
pub const PREWARM_TARGETS: &[(&str, TransportProfile)] = &[
    ("https://html.duckduckgo.com/", TransportProfile::Chrome),
    ("https://www.bing.com/", TransportProfile::Chrome),
];
