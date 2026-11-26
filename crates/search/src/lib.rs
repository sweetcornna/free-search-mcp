//! `fsm-search` — the search kernel (owner: lane `W1a`, `SPEC.md` §5).
//!
//! [`SearchService`] implements [`SearchApi`] over a [`Registry`]:
//! pool selection with direct-fact claimants on `scope="web"` and
//! `scope="code"` (M16), a quorum/grace/hedging race under soft and hard
//! deadlines ([`kernel`]), weighted RRF with host-canonical fuzzy dedup
//! ([`rank`]), the CJK-fixed coherence guard ([`coherence`]), the extractive
//! lead ([`lead`]), the persisted circuit breaker of `fsm-net`
//! ([`fsm_net::EngineHealth`]) and the `v4`
//! search cache with single-flight ([`cache`]). [`eval_replay`] replays the
//! ranking fixtures offline.

pub mod agent_lane;
pub mod cache;
pub mod coherence;
pub mod eval_replay;
pub mod filters;
pub mod kernel;
pub mod lead;
pub mod locale;
pub mod notes;
pub mod plan;
pub mod rank;

pub use filters::classify_source;

use async_trait::async_trait;
use cache::{KeyParts, cache_key};
use filters::Filters;
use fsm_core::scopes;
use fsm_core::{
    CallCx, CancellationToken, FsmError, Hit, Registry, SearchApi, SearchRequest, SearchResponse,
    SourceDecl, SourceDeps,
};
use fsm_net::EngineHealth;
use fsm_net::health::SILENT_THRESHOLD;
use kernel::{Ctx, Cut, Deadlines, Lane, Merged, Outcome, Run, Verdict, verdict};
use notes::Gate;
use plan::Plan;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

/// The `search` kernel.
pub struct SearchService {
    reg: Arc<Registry>,
    deps: SourceDeps,
    breaker: Arc<EngineHealth>,
    health_loaded: tokio::sync::OnceCell<()>,
    /// Single-flight: concurrent identical searches share one run.
    flights: moka::future::Cache<String, Arc<SearchResponse>>,
    /// Agent-lane coordination & late-merge row ownership (ruling R5).
    agent_lane: Arc<agent_lane::AgentLaneState>,
}

impl SearchService {
    pub fn new(reg: Arc<Registry>, deps: SourceDeps) -> Self {
        // Clocks on `tokio::time`, so a paused test clock drives the cooldowns:
        // monotonic seconds since construction, and the wall clock at
        // construction advanced by the same amount.
        let t0 = Instant::now();
        let wall0 = jiff::Timestamp::now().as_millisecond() as f64 / 1000.0;
        let breaker = Arc::new(EngineHealth::with_clocks(
            deps.cfg.engine_cooldown_s,
            deps.cfg.engine_cooldown_max_s,
            Arc::new(move || t0.elapsed().as_secs_f64()),
            Arc::new(move || wall0 + t0.elapsed().as_secs_f64()),
        ));
        Self {
            reg,
            deps,
            breaker,
            health_loaded: tokio::sync::OnceCell::new(),
            flights: moka::future::Cache::builder()
                .max_capacity(1024)
                .time_to_live(Duration::ZERO)
                .build(),
            agent_lane: Arc::new(agent_lane::AgentLaneState::new()),
        }
    }

    /// Register an extra agent-search engine id (used by tests with fake agent sources).
    pub fn register_agent_engine(&self, id: impl Into<String>, auto_default: bool) {
        self.agent_lane.register_extra_engine(id, auto_default);
    }

    /// Return the currently configured agent-lane engine ids for diagnostics.
    pub fn agent_lane_now(&self) -> Vec<String> {
        agent_lane::configured_lane(&self.deps.cfg, &self.reg, &self.agent_lane)
    }

    /// The circuit breaker (shared with diagnostics).
    pub fn breaker(&self) -> &Arc<EngineHealth> {
        &self.breaker
    }

    /// The bench a source sits on right now, if any.
    pub fn benched(&self, id: &str) -> Option<fsm_net::Benched> {
        self.breaker.describe(id)
    }

    async fn load_health(&self) {
        self.health_loaded
            .get_or_init(|| async {
                match self.deps.store.health_load_all().await {
                    Ok(rows) => self.breaker.adopt(&rows),
                    Err(e) => tracing::debug!(error = %e, "health state not loaded"),
                }
            })
            .await;
    }

    /// `scope="cache"`: full-text search over pages fetched earlier.
    async fn cache_scope(
        &self,
        query: &str,
        limit: u32,
        started: Instant,
    ) -> Result<SearchResponse, FsmError> {
        let rows = self.deps.store.cache_search(query, limit).await?;
        let mut resp = SearchResponse::new(query, "cache");
        resp.results = rows
            .into_iter()
            .enumerate()
            .map(|(i, c)| {
                let mut h = Hit::new(c.title, c.url);
                h.snippet = c.snippet;
                h.date_source = c.date.as_ref().map(|_| "snippet".to_owned());
                h.date = c.date;
                h.source = "cache".to_owned();
                h.sources = vec!["cache".to_owned()];
                h.score = ((1.0 / (rank::RRF_K + (i + 1) as f64)) * 1e5).round() / 1e5;
                let mut extra = serde_json::Map::new();
                if let Some(a) = c.author {
                    extra.insert("author".into(), a.into());
                }
                if let Some(s) = c.sitename {
                    extra.insert("sitename".into(), s.into());
                }
                h.extra = (!extra.is_empty()).then_some(serde_json::Value::Object(extra));
                h
            })
            .collect();
        resp.engines = vec!["cache".to_owned()];
        resp.notes.push(
            "From the local page cache (pages fetched earlier); nothing was searched upstream."
                .to_owned(),
        );
        resp.elapsed_ms = ms(started);
        Ok(resp)
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute(
        &self,
        req: &SearchRequest,
        query: &str,
        token: &str,
        plan: &Plan,
        lang: Option<String>,
        limit: u32,
        key: &str,
        cancel: &CancellationToken,
        budget: Duration,
        started: Instant,
    ) -> Result<SearchResponse, FsmError> {
        let cfg = &*self.deps.cfg;
        let domains = req.domains.clone().unwrap_or_default();
        let ctx = Ctx {
            query,
            limit: limit as usize,
            cfg,
            lang,
            freshness: req.freshness,
            filters: Filters::new(&domains, req.freshness),
            domains,
            host_class: plan.host_class,
            weighting: &plan.weighting,
            today: jiff::ZonedDateTime::now().date(),
            coherence: cfg.coherence_guard_enabled,
        };
        let deadlines = Deadlines::new(cfg, budget.saturating_sub(started.elapsed()));
        let health = cfg.health_enabled.then_some(&*self.breaker);
        let lane_plan = agent_lane::plan_lane(plan, cfg, &self.reg, health, &self.agent_lane);
        let lctx = agent_lane::LaneCtx::from_ctx(&ctx, Arc::clone(&self.deps.cfg));
        let spawned_lane = agent_lane::spawn_lane(lane_plan, &lctx);

        let run = kernel::race(
            &ctx,
            plan.seats.clone(),
            plan.reserves.clone(),
            plan.hedge,
            deadlines,
            cancel,
        )
        .await;
        if run.cut == Some(Cut::Cancelled) {
            return Err(FsmError::Timeout(0));
        }
        let merged = kernel::merge(&ctx, &run);
        self.record_health(&run, &merged).await;

        let decls: Vec<&SourceDecl> = run.lanes.iter().map(|l| l.seat.src.decl()).collect();
        let base_degraded = plan.auto_pool && merged.standing < cfg.min_healthy_engines;
        let base_ttl = cache::ttl(
            token,
            plan.scope.id,
            req.freshness,
            &decls,
            base_degraded,
            cfg,
        );
        let cacheable = base_ttl > 0;
        let keyless_buckets = agent_lane::extract_keyless_buckets(&run, &merged);

        let lane_settlement = agent_lane::await_and_settle(
            spawned_lane,
            &run,
            &merged,
            &lctx,
            &self.breaker,
            &self.deps.store,
            cacheable,
            started + budget,
            cancel,
        )
        .await;

        let mut resp = SearchResponse::new(query, plan.scope.id);
        resp.notes = build_notes(req, plan, &run, &merged, &ctx);
        let final_results = if lane_settlement.report.is_some() {
            lane_settlement.merged_results
        } else {
            merged.results
        };
        resp.lead = lead::lead_snippet(query, &final_results, &plan.weighting.direct);
        if let Some(note) = lane_settlement.extra_note {
            resp.notes.push(note);
        }
        resp.engines = run.lanes.iter().map(|l| l.seat.id.clone()).collect();
        for id in &lane_settlement.settled_engines {
            if !resp.engines.contains(id) {
                resp.engines.push(id.clone());
            }
        }
        resp.agent_lane = lane_settlement.report;
        resp.cache_key = Some(key.to_owned());
        resp.results = final_results;
        resp.elapsed_ms = ms(started);

        let effective_degraded = base_degraded || lane_settlement.lane_missing;
        let ttl = cache::ttl(
            token,
            plan.scope.id,
            req.freshness,
            &decls,
            effective_degraded,
            cfg,
        );
        let row_gen = self.agent_lane.claim_row(key);
        if ttl > 0
            && !resp.results.is_empty()
            && let Err(e) = self.deps.store.search_cache_put(key, &resp, ttl).await
        {
            tracing::warn!(error = %e, "search cache write failed");
        }
        if cacheable && !lane_settlement.late_tasks.is_empty() {
            agent_lane::spawn_late_merge(
                key.to_owned(),
                row_gen,
                base_ttl,
                started,
                resp.clone(),
                keyless_buckets,
                lane_settlement.settled_buckets,
                lane_settlement.late_tasks,
                lane_settlement.max_timeout,
                lctx,
                Arc::clone(&self.breaker),
                Arc::clone(&self.deps.store),
                Arc::clone(&self.agent_lane),
            );
        }
        Ok(resp)
    }

    /// `_record_health`: tell the breaker how each source that ran did.
    async fn record_health(&self, run: &Run, merged: &Merged) {
        if !self.deps.cfg.health_enabled {
            return;
        }
        let found = |l: &Lane| {
            matches!(&l.outcome, Outcome::Answered { hits, .. } if !hits.is_empty())
                && !merged.off_topic.contains(&l.seat.id)
        };
        for lane in &run.lanes {
            let id = lane.seat.id.as_str();
            let b = &self.breaker;
            let row = match &lane.outcome {
                Outcome::Cut(_) => None,
                Outcome::TimedOut(_) => b.record_failure(id, "timeout", None),
                Outcome::Failed(e) => match verdict(e) {
                    Verdict::NoStrike => None,
                    Verdict::Wall(r) => b.record_failure(id, &r, Some(1)),
                    Verdict::Strike(r) => b.record_failure(id, &r, None),
                },
                Outcome::Answered { raw, .. } => {
                    if merged.off_topic.iter().any(|x| x == id) {
                        b.record_failure(id, "off_topic", None)
                    } else if *raw > 0 {
                        b.record_success(id)
                    } else if lane.seat.guarded
                        && run
                            .lanes
                            .iter()
                            .any(|o| o.seat.id != id && o.seat.guarded && found(o))
                    {
                        b.record_failure(id, "silent", Some(SILENT_THRESHOLD))
                    } else {
                        None
                    }
                }
            };
            if let Some(row) = row
                && let Err(e) = self.deps.store.health_save(&row).await
            {
                tracing::debug!(error = %e, source = id, "health state not saved");
            }
        }
    }
}

/// Cancels a flight's child token when the leading caller's future is dropped.
struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

fn ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// An error shared by single-flight joiners, rebuilt (`FsmError` is not `Clone`).
fn rebuild(e: &FsmError) -> FsmError {
    match e {
        FsmError::Timeout(ms) => FsmError::Timeout(*ms),
        FsmError::InvalidInput(m) => FsmError::InvalidInput(m.clone()),
        other => FsmError::internal("search", other.to_string()),
    }
}

fn short(s: &str) -> String {
    let s = s.trim();
    match s.char_indices().nth(120) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_owned(),
    }
}

/// Why a failed source did not answer, for the notes (`None`: not news).
fn failure_note(lane: &Lane) -> Option<String> {
    let id = &lane.seat.id;
    let what = match &lane.outcome {
        Outcome::TimedOut(t) => format!("timed out after {:.1} s", t.as_secs_f64()),
        Outcome::Failed(e) => match e {
            FsmError::Timeout(0) => return None,
            FsmError::Timeout(ms) => format!("timed out after {:.1} s", *ms as f64 / 1000.0),
            FsmError::AuthRequired { env_var, .. } => format!("needs {env_var}"),
            FsmError::RateLimited { retry_after_s, .. } => match retry_after_s {
                Some(s) => format!("rate limited; retry after {s} s"),
                None => "rate limited".to_owned(),
            },
            FsmError::Upstream {
                status: Some(s), ..
            } if *s >= 400 => format!("HTTP {s}"),
            FsmError::BrowserUnavailable(_) => {
                "needs a browser render that is unavailable".to_owned()
            }
            FsmError::InvalidInput(m) => format!("rejected the request: {}", short(m)),
            other => format!("error: {}", short(&other.to_string())),
        },
        _ => return None,
    };
    Some(format!("{id} ({what})"))
}

fn build_notes(
    req: &SearchRequest,
    plan: &Plan,
    run: &Run,
    merged: &Merged,
    ctx: &Ctx<'_>,
) -> Vec<String> {
    let mut notes = Vec::new();
    if !plan.unknown.is_empty() {
        notes.push(format!(
            "Unknown source(s) skipped: {}. Find source ids with catalog.search(...).",
            plan.unknown.join(", ")
        ));
    }
    if !plan.no_search_op.is_empty() {
        notes.push(format!(
            "{} cannot search; call their ops directly with exec.",
            plan.no_search_op.join(", ")
        ));
    }
    if !plan.explicit && !plan.claimants.is_empty() {
        notes.push(format!(
            "Auto-routed to {}: they claim this query.",
            plan.claimants.join(", ")
        ));
    }
    if !plan.benched.is_empty() {
        notes.push(notes::benched_hint(&plan.benched));
    }
    let via = run
        .lanes
        .iter()
        .find(|l| l.hedge.is_some() && l.usable() && !merged.off_topic.contains(&l.seat.id))
        .map(|l| l.seat.id.clone());
    let mut gates: Vec<Gate> = Vec::new();
    for lane in &run.lanes {
        if let Outcome::Failed(e) = &lane.outcome
            && let Verdict::Wall(reason) = verdict(e)
        {
            gates.push(Gate {
                id: lane.seat.id.clone(),
                reason,
                via: via.clone(),
            });
        }
    }
    for id in &merged.off_topic {
        gates.push(Gate {
            id: id.clone(),
            reason: "off_topic".to_owned(),
            via: via.clone(),
        });
    }
    if !gates.is_empty() {
        notes.push(notes::gate_hint(&gates));
    }
    let hedges: Vec<String> = run
        .lanes
        .iter()
        .filter_map(|l| {
            l.hedge
                .as_ref()
                .map(|why| format!("Asked reserve {} because {}.", l.seat.id, why.describe()))
        })
        .collect();
    if !hedges.is_empty() {
        notes.push(hedges.join(" "));
    }
    let failed: Vec<String> = run
        .lanes
        .iter()
        .filter(|l| !gates.iter().any(|g| g.id == l.seat.id))
        .filter_map(failure_note)
        .collect();
    if !failed.is_empty() {
        notes.push(format!("Not answered: {}.", failed.join(", ")));
    }
    let late: Vec<(&str, Cut)> = run
        .lanes
        .iter()
        .filter_map(|l| match l.outcome {
            Outcome::Cut(c @ (Cut::Soft | Cut::Hard)) => Some((l.seat.id.as_str(), c)),
            _ => None,
        })
        .collect();
    if !late.is_empty() {
        let hard = late.iter().any(|(_, c)| *c == Cut::Hard);
        let secs = run
            .lanes
            .iter()
            .map(|l| l.dispatched_at + l.elapsed)
            .max()
            .unwrap_or_default()
            .as_secs_f64();
        let ids: Vec<&str> = late.iter().map(|(id, _)| *id).collect();
        notes.push(format!(
            "{} did not answer within {secs:.1} s ({} deadline) and {} not waited for.",
            ids.join(", "),
            if hard { "hard" } else { "soft" },
            if ids.len() == 1 { "was" } else { "were" }
        ));
    }
    if merged.results.len() <= notes::SPARSE {
        let filtered = !ctx.filters.include.is_empty()
            || !ctx.filters.exclude.is_empty()
            || req.freshness.is_some()
            || plan.host_class.is_some();
        if filtered {
            notes.push(notes::filter_hint(
                &merged.drops,
                merged.raw_total,
                merged.results.len(),
            ));
        }
        let empty: Vec<String> = run
            .lanes
            .iter()
            .filter(|l| matches!(l.outcome, Outcome::Answered { raw: 0, .. }))
            .map(|l| l.seat.id.clone())
            .collect();
        if let Some(h) = notes::empty_hint(&[], &empty) {
            notes.push(h);
        }
    }
    if let Some(n) = notes::freshness_note(&merged.results, req.freshness) {
        notes.push(n);
    }
    notes
}

#[async_trait]
impl SearchApi for SearchService {
    async fn search(&self, req: SearchRequest, cx: &CallCx) -> Result<SearchResponse, FsmError> {
        let started = Instant::now();
        let budget = cx.remaining();
        let query = req.query.trim().to_owned();
        if query.is_empty() {
            return Err(FsmError::invalid("query must not be empty"));
        }
        let limit = req.limit.clamp(1, kernel::MAX_LIMIT as u32);
        let token = match req.scope.trim().to_ascii_lowercase() {
            t if t.is_empty() => "web".to_owned(),
            t => t,
        };
        let scope = scopes::resolve(&token).ok_or_else(|| {
            FsmError::invalid(format!(
                "unknown scope '{}'; valid scopes: {}",
                req.scope.trim(),
                scopes::valid_scope_list()
            ))
        })?;
        match scope.id {
            "catalog" => {
                return Err(FsmError::invalid(
                    "scope=\"catalog\" lists sources, not web results: call catalog.search(...)",
                ));
            }
            "cache" => return self.cache_scope(&query, limit, started).await,
            _ => {}
        }
        self.load_health().await;
        let cfg = &*self.deps.cfg;
        let asked_lang = req
            .lang
            .as_deref()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_ascii_lowercase);
        let lang = asked_lang
            .clone()
            .or_else(|| locale::query_language(&query));
        let health = cfg.health_enabled.then_some(&*self.breaker);
        let plan = plan::build(&req, &query, scope, lang.as_deref(), &self.reg, cfg, health)?;
        let domains = req.domains.clone().unwrap_or_default();
        let lane_nom = agent_lane::lane_nominal(&plan, cfg, &self.reg, &self.agent_lane);
        let mut key_sources = plan.nominal.clone();
        for id in &lane_nom {
            key_sources.push(format!("agent:{id}"));
        }
        let key = cache_key(
            &KeyParts {
                query: &query,
                scope: scope.id,
                sources: &key_sources,
                freshness: req.freshness,
                domains: &domains,
                lang: asked_lang.as_deref(),
                limit,
            },
            cfg,
        );
        let seated: Vec<&SourceDecl> = plan.seats.iter().map(|s| s.src.decl()).collect();
        let ttl = cache::ttl(&token, scope.id, req.freshness, &seated, false, cfg);
        let max_age = req.max_age_hours.map(|h| {
            if h.is_finite() && h > 0.0 {
                (h * 3600.0) as u64
            } else {
                0
            }
        });
        if ttl > 0 && max_age != Some(0) {
            let read_age = max_age.map_or(ttl, |m| m.min(ttl));
            match self.deps.store.search_cache_get(&key, Some(read_age)).await {
                Ok(Some((mut resp, age))) => {
                    resp.cached = true;
                    resp.cache_age_s = Some(age);
                    resp.cache_key = Some(key);
                    resp.elapsed_ms = ms(started);
                    return Ok(resp);
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(error = %e, "search cache read failed"),
            }
        }
        let search_deadline = Duration::from_secs_f64(cfg.search_deadline_s.clamp(0.5, 120.0));
        if ttl == 0 || max_age == Some(0) || budget < search_deadline {
            return self
                .execute(
                    &req, &query, &token, &plan, lang, limit, &key, &cx.cancel, budget, started,
                )
                .await;
        }
        // Single-flight for normal cacheable searches: cancelling one caller
        // drops only its own wait; if it was the leader, CancelOnDrop aborts
        // its engines and moka automatically lets a remaining waiter retry.
        // Zero TTL guarantees no caller arriving after completion sees a stale
        // flight entry (subsequent hits come from StoreApi with max_age checks).
        let flight = tokio::select! {
            biased;
            () = cx.cancel.cancelled() => return Err(FsmError::Timeout(0)),
            res = self.flights.try_get_with(key.clone(), async {
                let flight_cancel = CancellationToken::new();
                let _guard = CancelOnDrop(flight_cancel.clone());
                self.execute(
                    &req,
                    &query,
                    &token,
                    &plan,
                    lang.clone(),
                    limit,
                    &key,
                    &flight_cancel,
                    search_deadline,
                    started,
                )
                .await
                .map(Arc::new)
            }) => res,
        };
        match flight {
            Ok(resp) => {
                let mut resp = (*resp).clone();
                resp.elapsed_ms = ms(started);
                Ok(resp)
            }
            Err(e) => Err(rebuild(&e)),
        }
    }
}
