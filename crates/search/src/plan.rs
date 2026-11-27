//! Which sources a search asks: the nominal pool (what the cache key
//! promises), the seats after the breaker, and the reserves hedging may add
//! (ports of `_nominal_pool`, `claimants`, `engines_for_category`,
//! `_active_pool`, `_native_engines`, `_direct_answer_engines`).

use crate::filters::HostClass;
use crate::rank::Weighting;
use fsm_core::scopes::{self, ScopeDef};
use fsm_core::{
    AuthMode, Availability, Config, Freshness, FsmError, PoolRole, Registry, SearchRequest, Source,
    SourceDecl,
};
use fsm_net::EngineHealth;
use std::collections::HashSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::Duration;

/// Per-source timeout of a structured API (`REQUEST_TIMEOUT` is for web engines).
pub const API_TIMEOUT: Duration = Duration::from_millis(4000);
/// `Source::claims()` scale (orchestrator ruling, all lanes): a score at or
/// above this is a DIRECT answer — the source is seated even outside the pool
/// and its first result gets [`crate::rank::DIRECT_WEIGHT`]. Below it (but
/// above 0) the claim is "relevant": the source may be seated (subject to
/// `CLAIM_ENGINE_LIMIT`, strongest first) with native weight, never the
/// direct boost. Pattern/ID matches score ≥ 0.8; bare keyword overlap 0.5.
pub const DIRECT_CLAIM_THRESHOLD: f32 = 0.8;

/// A claim score that makes the source's first result a looked-up record.
pub fn is_direct(score: f32) -> bool {
    score >= DIRECT_CLAIM_THRESHOLD
}

/// Query languages that elevate a default-pool engine to native weight.
pub const LOCALE_ELEVATED: &[(&str, &str)] = &[("ja", "yahoojp")];

/// Why a source sits in this search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The configured default web pool.
    Primary,
    /// A direct-fact claimant (M16).
    Claimant,
    /// Native to the requested scope.
    Native,
    /// `SEARCH_MCP_LOCALE_ENGINES` for the query language.
    Locale,
    /// `SEARCH_MCP_FRESH_ENGINES` (`freshness=day|week`).
    Fresh,
    /// A reserve seated for a benched general engine, or a hedge.
    Reserve,
    /// Named in `sources`.
    Explicit,
}

/// One source of this search.
#[derive(Clone)]
pub struct Seat {
    pub id: String,
    pub src: Arc<dyn Source>,
    pub role: Role,
    /// A general web index: coherence-guarded, counted for hedging/degraded.
    pub guarded: bool,
    /// Native to the requested scope (never host-filtered by it).
    pub native: bool,
    pub timeout: Duration,
}

impl std::fmt::Debug for Seat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Seat")
            .field("id", &self.id)
            .field("role", &self.role)
            .field("guarded", &self.guarded)
            .field("native", &self.native)
            .finish_non_exhaustive()
    }
}

/// A benched source and who stands in for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BenchNote {
    pub id: String,
    pub reason: String,
    /// Seconds until the probe.
    pub retry_in_s: u64,
    pub substitute: Option<String>,
}

/// Everything the kernel decided before asking anyone.
#[derive(Debug)]
pub struct Plan {
    pub scope: &'static ScopeDef,
    /// The pool this search is entitled to, before health (cache key input).
    pub nominal: Vec<String>,
    /// Dispatched at t = 0, in this order.
    pub seats: Vec<Seat>,
    /// Hedge candidates, in `SEARCH_MCP_RESERVE_ENGINES` order.
    pub reserves: Vec<Seat>,
    pub benched: Vec<BenchNote>,
    pub weighting: Weighting,
    /// Allowlist the scope imposes on general engines' results.
    pub host_class: Option<HostClass>,
    /// Hedging may add reserves.
    pub hedge: bool,
    pub explicit: bool,
    /// The general web pool takes part (degraded runs are cached briefly).
    pub auto_pool: bool,
    /// Claimants seated by auto-routing (for the notes).
    pub claimants: Vec<String>,
    pub unknown: Vec<String>,
    pub no_search_op: Vec<String>,
}

/// Every declared scope is `web`: a general web index.
pub fn is_general(decl: &SourceDecl) -> bool {
    !decl.scopes.is_empty() && decl.scopes.iter().all(|s| s == "web")
}

fn can_search(decl: &SourceDecl) -> bool {
    decl.ops.is_empty() || decl.op("search").is_some()
}

fn keys_present(decl: &SourceDecl, cfg: &Config) -> bool {
    let envs = decl.key_envs();
    !envs.is_empty() && envs.iter().all(|e| cfg.api_key(e).is_some())
}

/// `availability()` guarded against a panicking implementation.
fn available(src: &dyn Source, cfg: &Config) -> bool {
    match catch_unwind(AssertUnwindSafe(|| src.availability(cfg))) {
        Ok(Availability::Available) => true,
        Ok(_) => false,
        Err(_) => {
            tracing::warn!(source = %src.decl().id, "availability() panicked");
            false
        }
    }
}

/// Usable for AUTO-selection: searchable, available, and not explicit-only.
/// Never consulted for sources the caller named.
pub fn auto_ok(src: &dyn Source, cfg: &Config) -> bool {
    let decl = src.decl();
    if decl.pool_role == PoolRole::Explicit || !can_search(decl) {
        return false;
    }
    if !available(src, cfg) {
        return false;
    }
    match &decl.auth {
        AuthMode::OptionalKey {
            route_requires_key: true,
            ..
        } => keys_present(decl, cfg),
        AuthMode::RequiredKey { .. } => keys_present(decl, cfg),
        _ => true,
    }
}

/// `claims()` guarded against a panicking implementation (a claim test must
/// never break a search).
pub fn claim(src: &dyn Source, query: &str, lang: Option<&str>) -> f32 {
    match catch_unwind(AssertUnwindSafe(|| src.claims(query, lang))) {
        Ok(c) if c.is_finite() && c > 0.0 => c,
        Ok(_) => 0.0,
        Err(_) => {
            tracing::warn!(source = %src.decl().id, "claims() panicked");
            0.0
        }
    }
}

fn seat(src: &Arc<dyn Source>, role: Role, native: bool, cfg: &Config) -> Seat {
    let decl = src.decl();
    let guarded = is_general(decl);
    let timeout = if decl.limits.timeout_ms > 0 {
        Duration::from_millis(decl.limits.timeout_ms)
    } else if guarded {
        Duration::from_secs_f64(cfg.request_timeout_s.max(0.1))
    } else {
        API_TIMEOUT
    };
    Seat {
        id: decl.id.clone(),
        src: Arc::clone(src),
        role,
        guarded,
        native,
        timeout,
    }
}

/// Look a caller-named source up by id, then by alias.
pub fn lookup(reg: &Registry, name: &str) -> Option<Arc<dyn Source>> {
    let key = name.trim().to_ascii_lowercase();
    reg.get(&key).or_else(|| {
        reg.all()
            .find(|s| {
                s.decl()
                    .aliases
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case(&key))
            })
            .cloned()
    })
}

/// Direct-fact claimants (M16): sources that claim `query` and can run,
/// strongest first (registry order on ties), capped by `CLAIM_ENGINE_LIMIT`.
/// `within`: only sources declaring that scope (or a descendant).
pub fn claimants(
    reg: &Registry,
    query: &str,
    lang: Option<&str>,
    cfg: &Config,
    within: Option<&str>,
) -> Vec<(Arc<dyn Source>, f32)> {
    if !cfg.auto_route_enabled || query.trim().is_empty() {
        return Vec::new();
    }
    let mut picks: Vec<(Arc<dyn Source>, f32)> = reg
        .all()
        .filter(|s| within.is_none_or(|w| s.decl().scopes.iter().any(|d| scopes::is_within(d, w))))
        .filter_map(|s| {
            let c = claim(s.as_ref(), query, lang);
            (c > 0.0 && auto_ok(s.as_ref(), cfg)).then(|| (Arc::clone(s), c))
        })
        .collect();
    picks.sort_by(|a, b| b.1.total_cmp(&a.1));
    if let Ok(limit) = usize::try_from(cfg.claim_engine_limit) {
        picks.truncate(limit);
    }
    picks
}

/// Interleave a root scope's sources across its sub-scopes before the cap
/// (`_round_robin_by_subgroup`); a source is bucketed under its
/// alphabetically first sub-scope.
fn round_robin(root: &str, srcs: Vec<Arc<dyn Source>>) -> Vec<Arc<dyn Source>> {
    let mut buckets: Vec<(String, Vec<Arc<dyn Source>>)> = Vec::new();
    for s in srcs {
        let mut subs: Vec<&str> = s
            .decl()
            .scopes
            .iter()
            .map(String::as_str)
            .filter(|d| *d != root && scopes::is_within(d, root))
            .collect();
        subs.sort_unstable();
        let key = subs.first().copied().unwrap_or("").to_owned();
        match buckets.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => v.push(s),
            None => buckets.push((key, vec![s])),
        }
    }
    let mut out = Vec::new();
    let mut i = 0;
    loop {
        let mut any = false;
        for (_, b) in &buckets {
            if let Some(s) = b.get(i) {
                out.push(Arc::clone(s));
                any = true;
            }
        }
        if !any {
            return out;
        }
        i += 1;
    }
}

/// Sources native to `scope` that auto-routing may seat: claimants first
/// (strongest first), then the rest (spread across sub-scopes for a root
/// scope), capped at `SCOPE_ENGINE_LIMIT` — or at the number of claimants
/// seated, when more claim the query (≤ `CLAIM_ENGINE_LIMIT`).
/// Returns `(source, claim score)` pairs (0 = no claim).
pub fn natives(
    reg: &Registry,
    scope: &ScopeDef,
    query: &str,
    lang: Option<&str>,
    cfg: &Config,
    exclude: &[String],
) -> Vec<(Arc<dyn Source>, f32)> {
    let pool: Vec<Arc<dyn Source>> = reg
        .by_scope(scope.id)
        .into_iter()
        .filter(|s| !exclude.contains(&s.decl().id) && auto_ok(s.as_ref(), cfg))
        .collect();
    let claimed: Vec<(Arc<dyn Source>, f32)> = claimants(reg, query, lang, cfg, Some(scope.id))
        .into_iter()
        .filter(|(s, _)| !exclude.contains(&s.decl().id))
        .collect();
    let claimed_ids: HashSet<String> = claimed.iter().map(|(s, _)| s.decl().id.clone()).collect();
    let (mut scope_native, mut unclaimed_direct): (Vec<_>, Vec<_>) = pool
        .into_iter()
        .filter(|s| !claimed_ids.contains(&s.decl().id))
        .partition(|s| s.decl().pool_role != PoolRole::DirectClaimant);
    // Prefer sources whose primary scope (`scopes[0]`) is the requested scope
    // (e.g. `googlenews`, `gdelt`, `bing_news_rss` for `news`) ahead of
    // cross-listed secondary sources (e.g. `hackernews` with `["forum", "news"]`),
    // without letting obscure single-scope manifests displace multi-scope tier-1
    // leaders (e.g. `datacite` `["dataset", "academic"]` or `github` `["code", ...]`).
    scope_native.sort_by_key(|s| {
        let primary = s
            .decl()
            .scopes
            .first()
            .is_some_and(|first| first == scope.id);
        u8::from(!primary)
    });
    if scope.parent.is_none() {
        scope_native = round_robin(scope.id, scope_native);
        unclaimed_direct = round_robin(scope.id, unclaimed_direct);
    }
    let cap = cfg.scope_engine_limit.max(claimed.len());
    claimed
        .into_iter()
        .chain(
            scope_native
                .into_iter()
                .chain(unclaimed_direct)
                .map(|s| (s, 0.0)),
        )
        .take(cap)
        .collect()
}

fn push_unique(seats: &mut Vec<Seat>, s: Seat) {
    if !seats.iter().any(|x| x.id == s.id) {
        seats.push(s);
    }
}

/// Build the plan. `lang`: the request's `lang`, or the query script's.
/// `health`: the breaker (`None` when health is switched off).
pub fn build(
    req: &SearchRequest,
    query: &str,
    scope: &'static ScopeDef,
    lang: Option<&str>,
    reg: &Registry,
    cfg: &Config,
    health: Option<&EngineHealth>,
) -> Result<Plan, FsmError> {
    let mut plan = Plan {
        scope,
        nominal: Vec::new(),
        seats: Vec::new(),
        reserves: Vec::new(),
        benched: Vec::new(),
        weighting: Weighting::default(),
        host_class: HostClass::for_scope(scope.id),
        hedge: false,
        explicit: false,
        auto_pool: false,
        claimants: Vec::new(),
        unknown: Vec::new(),
        no_search_op: Vec::new(),
    };
    let named: Vec<&String> = req
        .sources
        .iter()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .collect();
    if !named.is_empty() {
        explicit(&mut plan, &named, query, lang, reg, cfg)?;
    } else if scope.exclusive {
        exclusive(&mut plan, query, lang, reg, cfg);
    } else {
        pooled(&mut plan, req, query, lang, reg, cfg);
    }
    plan.nominal = plan.seats.iter().map(|s| s.id.clone()).collect();
    if !plan.explicit
        && cfg.health_enabled
        && let Some(b) = health
    {
        seat_reserves(&mut plan, reg, cfg, b);
    }
    if plan.hedge {
        let mut seen = HashSet::new();
        plan.reserves = cfg
            .reserve_engines
            .iter()
            .filter(|id| seen.insert(id.as_str()))
            .filter(|id| !plan.seats.iter().any(|s| s.id == **id) && !plan.nominal.contains(id))
            .filter_map(|id| reg.get(id))
            .filter(|s| auto_ok(s.as_ref(), cfg))
            .filter(|s| !cfg.health_enabled || health.is_none_or(|b| !b.is_open(&s.decl().id)))
            .map(|s| seat(&s, Role::Reserve, false, cfg))
            .collect();
    }
    Ok(plan)
}

fn explicit(
    plan: &mut Plan,
    named: &[&String],
    query: &str,
    lang: Option<&str>,
    reg: &Registry,
    cfg: &Config,
) -> Result<(), FsmError> {
    plan.explicit = true;
    for name in named {
        let Some(src) = lookup(reg, name) else {
            plan.unknown.push(name.trim().to_owned());
            continue;
        };
        let decl = src.decl();
        if !can_search(decl) {
            plan.no_search_op.push(decl.id.clone());
            continue;
        }
        let score = claim(src.as_ref(), query, lang);
        let native = if plan.scope.id == "web" {
            score > 0.0
        } else {
            decl.scopes
                .iter()
                .any(|d| scopes::is_within(d, plan.scope.id))
        };
        if is_direct(score) {
            plan.weighting.direct.insert(decl.id.clone());
        }
        if native {
            plan.weighting.native.insert(decl.id.clone());
        }
        push_unique(&mut plan.seats, seat(&src, Role::Explicit, native, cfg));
    }
    if plan.seats.is_empty() {
        let mut why = Vec::new();
        if !plan.unknown.is_empty() {
            why.push(format!(
                "unknown source(s) {}; find source ids with catalog.search(...)",
                plan.unknown.join(", ")
            ));
        }
        if !plan.no_search_op.is_empty() {
            why.push(format!(
                "{} cannot search (call its ops directly with exec)",
                plan.no_search_op.join(", ")
            ));
        }
        return Err(FsmError::invalid(format!(
            "sources: nothing to search: {}",
            why.join("; ")
        )));
    }
    Ok(())
}

fn exclusive(plan: &mut Plan, query: &str, lang: Option<&str>, reg: &Registry, cfg: &Config) {
    let picks = natives(reg, plan.scope, query, lang, cfg, &[]);
    if picks.is_empty() {
        // No specialist can run: the general web pool, filtered to the
        // scope's hosts, beats answering with nothing (Python's fallback).
        default_pool(plan, reg, cfg);
        plan.auto_pool = true;
        return;
    }
    plan.host_class = None;
    for (src, score) in picks {
        let id = src.decl().id.clone();
        plan.weighting.native.insert(id.clone());
        if is_direct(score) {
            plan.weighting.direct.insert(id);
        }
        push_unique(&mut plan.seats, seat(&src, Role::Native, true, cfg));
    }
}

fn default_pool(plan: &mut Plan, reg: &Registry, cfg: &Config) {
    for id in &cfg.default_engines {
        if let Some(src) = reg.get(id)
            && can_search(src.decl())
            && available(src.as_ref(), cfg)
        {
            push_unique(&mut plan.seats, seat(&src, Role::Primary, false, cfg));
        }
    }
}

fn pooled(
    plan: &mut Plan,
    req: &SearchRequest,
    query: &str,
    lang: Option<&str>,
    reg: &Registry,
    cfg: &Config,
) {
    plan.auto_pool = true;
    plan.hedge = cfg.hedge_enabled;
    default_pool(plan, reg, cfg);
    let taken: Vec<String> = plan.seats.iter().map(|s| s.id.clone()).collect();
    if plan.scope.id == "web" {
        for (src, score) in claimants(reg, query, lang, cfg, None) {
            let id = src.decl().id.clone();
            if taken.contains(&id) {
                continue;
            }
            plan.weighting.native.insert(id.clone());
            if is_direct(score) {
                plan.weighting.direct.insert(id.clone());
            }
            plan.claimants.push(id);
            push_unique(&mut plan.seats, seat(&src, Role::Claimant, true, cfg));
        }
    } else {
        for (src, score) in natives(reg, plan.scope, query, lang, cfg, &taken) {
            let id = src.decl().id.clone();
            plan.weighting.native.insert(id.clone());
            if is_direct(score) {
                plan.weighting.direct.insert(id.clone());
            }
            if score > 0.0 {
                plan.claimants.push(id);
            }
            push_unique(&mut plan.seats, seat(&src, Role::Native, true, cfg));
        }
    }
    let mut extras: Vec<(&String, Role)> = Vec::new();
    if matches!(req.freshness, Some(Freshness::Day | Freshness::Week)) {
        extras.extend(cfg.fresh_engines.iter().map(|id| (id, Role::Fresh)));
    }
    if let Some(l) = lang
        && let Some(ids) = cfg.locale_engines.get(l)
    {
        extras.extend(ids.iter().map(|id| (id, Role::Locale)));
    }
    for (id, role) in extras {
        if let Some(src) = reg.get(id)
            && auto_ok_or_default(src.as_ref(), cfg)
        {
            let scope_native = plan.scope.id != "web"
                && src
                    .decl()
                    .scopes
                    .iter()
                    .any(|d| scopes::is_within(d, plan.scope.id));
            let native = scope_native || plan.weighting.native.contains(id);
            if native {
                plan.weighting.native.insert(id.clone());
            }
            push_unique(&mut plan.seats, seat(&src, role, native, cfg));
        }
    }
    if let Some(l) = lang {
        for (lg, id) in LOCALE_ELEVATED {
            if *lg == l && plan.seats.iter().any(|s| s.id == *id) {
                plan.weighting.native.insert((*id).to_owned());
            }
        }
    }
}

/// Configured extras (locale/fresh lists) run unless they cannot.
fn auto_ok_or_default(src: &dyn Source, cfg: &Config) -> bool {
    can_search(src.decl()) && available(src, cfg)
}

/// `_active_pool`: drop benched seats; seat reserves while fewer than
/// `MIN_HEALTHY_ENGINES` general engines remain; ignore the breaker when no
/// general engine would be left at all.
fn seat_reserves(plan: &mut Plan, reg: &Registry, cfg: &Config, b: &EngineHealth) {
    let benched: Vec<BenchNote> = plan
        .seats
        .iter()
        .filter_map(|s| {
            b.describe(&s.id).map(|bench| BenchNote {
                id: s.id.clone(),
                reason: bench.reason,
                retry_in_s: bench.retry_in_seconds,
                substitute: None,
            })
        })
        .collect();
    if benched.is_empty() {
        return;
    }
    let mut active: Vec<Seat> = plan
        .seats
        .iter()
        .filter(|s| !benched.iter().any(|x| x.id == s.id))
        .cloned()
        .collect();
    let mut benched = benched;
    let mut healthy = active.iter().filter(|s| s.guarded).count();
    let mut waiting: Vec<usize> = benched
        .iter()
        .enumerate()
        .filter(|(_, n)| plan.seats.iter().any(|s| s.id == n.id && s.guarded))
        .map(|(i, _)| i)
        .collect();
    for id in &cfg.reserve_engines {
        if healthy >= cfg.min_healthy_engines || waiting.is_empty() {
            break;
        }
        if plan.nominal.contains(id) || active.iter().any(|s| s.id == *id) || b.is_open(id) {
            continue;
        }
        let Some(src) = reg.get(id) else { continue };
        if !auto_ok(src.as_ref(), cfg) {
            continue;
        }
        let s = seat(&src, Role::Reserve, false, cfg);
        if s.guarded {
            healthy += 1;
        }
        active.push(s);
        let w = waiting.remove(0);
        benched[w].substitute = Some(id.clone());
    }
    if active.is_empty() || (healthy == 0 && plan.seats.iter().any(|s| s.guarded)) {
        return; // every seat (or every general engine) benched: ask them anyway
    }
    plan.seats = active;
    plan.benched = benched;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decl(scopes: &[&str]) -> SourceDecl {
        SourceDecl::new("x", "x", "fake").with_scopes(scopes)
    }

    #[test]
    fn the_guarded_engines_are_exactly_the_web_indexes() {
        // tests/test_coherence_guard.py (same name), on the 1.0 rule: a source is
        // guarded (and counted as a general engine) iff every scope it declares
        // is "web". The per-engine list is pinned by the engine crates.
        assert!(is_general(&decl(&["web"])));
        assert!(!is_general(&decl(&["web", "news"])));
        assert!(!is_general(&decl(&["academic.preprint"])));
        assert!(!is_general(&decl(&[])));
    }

    #[test]
    fn a_claim_is_direct_from_the_threshold_up() {
        // Orchestrator ruling: >= 0.8 direct, (0, 0.8) relevant, 0 none.
        assert!(is_direct(DIRECT_CLAIM_THRESHOLD));
        assert!(is_direct(1.0));
        assert!(!is_direct(0.79));
        assert!(!is_direct(0.5));
        assert!(!is_direct(0.0));
    }

    #[test]
    fn dedicated_single_scope_news_sources_beat_multi_scope_forum_sources() {
        struct Stub(SourceDecl);
        #[async_trait::async_trait]
        impl Source for Stub {
            fn decl(&self) -> &SourceDecl {
                &self.0
            }
            async fn call(
                &self,
                _: &str,
                _: serde_json::Value,
                _: &fsm_core::SrcCx,
            ) -> Result<fsm_core::SrcOutput, FsmError> {
                Ok(fsm_core::SrcOutput::Hits(Vec::new()))
            }
        }
        let mk = |id: &str, scopes: &[&str]| -> Arc<dyn Source> {
            let mut d = SourceDecl::new(id, id, "stub").with_scopes(scopes);
            d.pool_role = PoolRole::ScopeOnly;
            Arc::new(Stub(d))
        };
        let mut reg = Registry::new();
        reg.register(mk("googlenews", &["news"])).unwrap();
        reg.register(mk("gdelt", &["news"])).unwrap();
        reg.register(mk("hackernews", &["forum", "news"])).unwrap();
        reg.register(mk("bing_news_rss", &["news"])).unwrap();

        let mut cfg = Config::defaults_without_home();
        cfg.scope_engine_limit = 3;
        let scope = scopes::resolve("news").unwrap();
        let picks = natives(&reg, scope, "EU AI Act enforcement", None, &cfg, &[]);
        let ids: Vec<&str> = picks.iter().map(|(s, _)| s.decl().id.as_str()).collect();
        assert_eq!(ids, vec!["googlenews", "gdelt", "bing_news_rss"]);

        // Primary multi-scope sources (e.g. datacite ["dataset", "academic"]) must NOT
        // be displaced by later single-scope manifests (e.g. fao_stat ["dataset"]).
        let mut reg_ds = Registry::new();
        reg_ds
            .register(mk("datacite", &["dataset", "academic"]))
            .unwrap();
        reg_ds
            .register(mk("dataeuropa", &["dataset", "gov"]))
            .unwrap();
        reg_ds.register(mk("re3data", &["dataset"])).unwrap();
        reg_ds.register(mk("fao_stat", &["dataset"])).unwrap();
        let ds_scope = scopes::resolve("dataset").unwrap();
        let ds_picks = natives(&reg_ds, ds_scope, "ImageNet dataset", None, &cfg, &[]);
        let ds_ids: Vec<&str> = ds_picks.iter().map(|(s, _)| s.decl().id.as_str()).collect();
        assert_eq!(ds_ids, vec!["datacite", "dataeuropa", "re3data"]);
    }
}
