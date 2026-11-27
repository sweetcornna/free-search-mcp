//! Automatic signed-in agent search lane (`codex` auto, `antigravity` opt-in)
//! running beside the keyless web pool with wait/grace semantics, coherence
//! settlement, lane-specific health strikes, and late background cache merge
//! (port of `src/search_mcp/aggregator.py` agent lane, ruling R5 Deliverable 3).

use crate::coherence::{is_witness, looks_like_decoy};
use crate::filters::{self, Filters, HostClass};
use crate::kernel::{Ctx, Merged, Outcome, Run, to_hits};
use crate::lead;
use crate::plan::{Plan, Role, Seat};
use crate::rank::{Weighting, fuse};
use fsm_core::dates::extract_date_hint;
use fsm_core::ext::w7::{
    is_builtin_agent_engine, is_builtin_agent_lane_auto, is_lasting_agent_failure,
    scope_agent_lane, strip_agent_lasting_tag,
};
use fsm_core::url_canon::{normalize_url, resolve_yahoo_url};
use fsm_core::{
    AgentLaneReport, Availability, CancellationToken, Config, Freshness, FsmError, Hit, Registry,
    SearchResponse, Source, SrcCx, SrcOutput, StoreApi,
};
use fsm_net::EngineHealth;
use futures::FutureExt;
use futures::stream::{FuturesUnordered, StreamExt};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::Instant;

/// Transient failures bench an agent-lane engine after 2 strikes.
pub const LANE_SOFT_THRESHOLD: u32 = 2;
/// Unconfirmed off-topic/doubted lane answers bench after 3 strikes (separate counter).
pub const LANE_UNCONFIRMED_THRESHOLD: u32 = 3;
/// Lasting failures (usage limit, revoked sign-in, pinned model refused, 403) bench on 1 strike.
pub const LANE_REFUSED_THRESHOLD: u32 = 1;

/// Shared state on [`crate::SearchService`] tracking row ownership for late cache merges
/// and any test-registered extra agent engines.
pub struct AgentLaneState {
    next_gen: AtomicU64,
    row_gens: Mutex<HashMap<String, u64>>,
    row_locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    warned_tokens: Mutex<HashSet<String>>,
    /// Extra `(id, auto_default)` agent engines (for tests).
    extra_engines: Mutex<Vec<(String, bool)>>,
}

impl Default for AgentLaneState {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentLaneState {
    pub fn new() -> Self {
        Self {
            next_gen: AtomicU64::new(1),
            row_gens: Mutex::new(HashMap::new()),
            row_locks: Mutex::new(HashMap::new()),
            warned_tokens: Mutex::new(HashSet::new()),
            extra_engines: Mutex::new(Vec::new()),
        }
    }

    pub fn register_extra_engine(&self, id: impl Into<String>, auto_default: bool) {
        if let Ok(mut guard) = self.extra_engines.lock() {
            let id = id.into();
            if !guard.iter().any(|(x, _)| x == &id) {
                guard.push((id, auto_default));
            }
        }
    }

    pub fn is_agent_engine(&self, id: &str) -> bool {
        if is_builtin_agent_engine(id) {
            return true;
        }
        self.extra_engines
            .lock()
            .ok()
            .is_some_and(|g| g.iter().any(|(x, _)| x == id))
    }

    pub fn is_auto_engine(&self, id: &str) -> bool {
        if is_builtin_agent_lane_auto(id) {
            return true;
        }
        self.extra_engines
            .lock()
            .ok()
            .is_some_and(|g| g.iter().any(|(x, auto)| x == id && *auto))
    }

    pub fn all_known_agent_ids(&self, reg: &Registry) -> Vec<String> {
        let mut out = Vec::new();
        for &(id, _) in fsm_core::ext::w7::BUILTIN_AGENT_ENGINES {
            if reg.get(id).is_some() {
                out.push(id.to_owned());
            }
        }
        if let Ok(guard) = self.extra_engines.lock() {
            for (id, _) in guard.iter() {
                if reg.get(id).is_some() && !out.contains(id) {
                    out.push(id.clone());
                }
            }
        }
        out
    }

    /// Claim ownership of cache row `key` for this search generation.
    pub fn claim_row(&self, key: &str) -> u64 {
        let row_gen = self.next_gen.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut guard) = self.row_gens.lock() {
            guard.insert(key.to_owned(), row_gen);
        }
        row_gen
    }

    pub fn owns_row(&self, key: &str, row_gen: u64) -> bool {
        self.row_gens
            .lock()
            .ok()
            .is_some_and(|g| g.get(key).copied() == Some(row_gen))
    }

    pub fn row_lock(&self, key: &str) -> Arc<AsyncMutex<()>> {
        let mut guard = self.row_locks.lock().expect("row_locks mutex");
        guard
            .entry(key.to_owned())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    fn warn_once(&self, token: &str) {
        if let Ok(mut guard) = self.warned_tokens.lock()
            && guard.insert(token.to_owned())
        {
            tracing::warn!(
                token = %token,
                "SEARCH_MCP_AGENT_ENGINES: ignoring unknown or non-agent engine token"
            );
        }
    }
}

/// Parse `SEARCH_MCP_AGENT_ENGINES` into configured agent engine IDs.
pub fn configured_lane(cfg: &Config, reg: &Registry, state: &AgentLaneState) -> Vec<String> {
    let raw = cfg.agent_engines.trim();
    if raw.is_empty() {
        return Vec::new();
    }
    let lower = raw.to_ascii_lowercase();
    if matches!(lower.as_str(), "off" | "none" | "false" | "0") {
        return Vec::new();
    }
    if lower == "auto" {
        return state
            .all_known_agent_ids(reg)
            .into_iter()
            .filter(|id| state.is_auto_engine(id))
            .collect();
    }

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for tok in raw.split(|c: char| c == ',' || c.is_whitespace()) {
        let id = tok.trim().to_ascii_lowercase();
        if id.is_empty() {
            continue;
        }
        if !state.is_agent_engine(&id) || reg.get(&id).is_none() {
            state.warn_once(&id);
            continue;
        }
        if seen.insert(id.clone()) {
            out.push(id);
        }
    }
    out
}

/// Nominal agent lane for a search: signed-in configured agent engines eligible
/// for automatic non-exclusive web searches. Used in the search cache key (`agent:<id>`).
pub fn lane_nominal(
    plan: &Plan,
    cfg: &Config,
    reg: &Registry,
    state: &AgentLaneState,
) -> Vec<String> {
    if !plan.auto_pool || plan.explicit {
        return Vec::new();
    }
    if matches!(plan.scope.id, "media" | "dataset" | "catalog" | "cache") {
        return Vec::new();
    }
    let configured = configured_lane(cfg, reg, state);
    let mut out = Vec::new();
    for id in configured {
        if plan.nominal.contains(&id) {
            continue;
        }
        let Some(src) = reg.get(&id) else {
            continue;
        };
        let avail = catch_unwind(AssertUnwindSafe(|| src.availability(cfg)))
            .ok()
            .unwrap_or_else(|| Availability::Disabled("panicked".to_owned()));
        if avail == Availability::Available {
            out.push(id);
        }
    }
    out
}

/// Seats selected from `nominal` after checking health breaker.
pub struct LanePlan {
    pub nominal: Vec<String>,
    pub seats: Vec<Seat>,
    pub benched: Vec<(String, String)>,
}

pub fn plan_lane(
    plan: &Plan,
    cfg: &Config,
    reg: &Registry,
    breaker: Option<&EngineHealth>,
    state: &AgentLaneState,
) -> LanePlan {
    let nominal = lane_nominal(plan, cfg, reg, state);
    let mut seats = Vec::new();
    let mut benched = Vec::new();
    for id in &nominal {
        if let Some(b) = breaker
            && let Some(info) = b.describe(id)
        {
            benched.push((id.clone(), info.reason));
            continue;
        }
        if let Some(src) = reg.get(id) {
            let timeout = lane_timeout(id, src.as_ref(), cfg);
            seats.push(Seat {
                id: id.clone(),
                src,
                role: Role::Explicit,
                guarded: true,
                native: false,
                timeout,
            });
        }
    }
    LanePlan {
        nominal,
        seats,
        benched,
    }
}

fn lane_timeout(id: &str, src: &dyn Source, cfg: &Config) -> Duration {
    match id {
        "codex" => Duration::from_secs_f64(cfg.codex_timeout_s.clamp(1.0, 300.0)),
        "antigravity" => Duration::from_secs_f64(cfg.antigravity_timeout_s.clamp(1.0, 300.0)),
        _ => {
            let ms = src.decl().limits.timeout_ms;
            if ms > 0 {
                Duration::from_millis(ms)
            } else {
                Duration::from_secs(60)
            }
        }
    }
}

/// Snapshot of a single agent-lane engine call result.
#[derive(Debug)]
pub enum LaneResult {
    Answered {
        hits: Vec<Hit>,
        raw: usize,
        suspect: bool,
    },
    Failed(FsmError),
    TimedOut(Duration),
}

struct InFlightLane {
    seat: Seat,
    cancel: CancellationToken,
    handle: tokio::task::JoinHandle<(String, LaneResult)>,
}

/// Handle returned by [`spawn_lane`] for concurrent execution alongside keyless `kernel::race`.
pub struct SpawnedLane {
    pub started: Instant,
    pub nominal: Vec<String>,
    pub seated: Vec<String>,
    pub benched: Vec<(String, String)>,
    pub max_timeout: Duration,
    tasks: Vec<InFlightLane>,
}

/// Owned query parameters needed by spawned lane tasks (and late background cache merge).
#[derive(Clone)]
pub struct LaneCtx {
    pub query: String,
    pub limit: usize,
    pub per_source: usize,
    pub cfg: Arc<Config>,
    pub lang: Option<String>,
    pub freshness: Option<Freshness>,
    pub domains: Vec<String>,
    pub filters: Filters,
    pub host_class: Option<HostClass>,
    pub weighting: Weighting,
    pub today: jiff::civil::Date,
    pub coherence: bool,
}

impl LaneCtx {
    pub fn from_ctx(ctx: &Ctx<'_>, cfg: Arc<Config>) -> Self {
        let per_source = ctx
            .limit
            .max(cfg.max_results_per_engine as usize)
            .clamp(1, crate::kernel::MAX_LIMIT);
        Self {
            query: ctx.query.to_owned(),
            limit: ctx.limit,
            per_source,
            cfg,
            lang: ctx.lang.clone(),
            freshness: ctx.freshness,
            domains: ctx.domains.clone(),
            filters: ctx.filters.clone(),
            host_class: ctx.host_class,
            weighting: ctx.weighting.clone(),
            today: ctx.today,
            coherence: ctx.coherence,
        }
    }

    fn prepare(&self, seat: &Seat, out: SrcOutput) -> (Vec<Hit>, usize, bool) {
        let mut clean = Vec::new();
        for mut h in to_hits(out, &seat.id) {
            let Some(url) = resolve_yahoo_url(&h.url) else {
                continue;
            };
            h.url = normalize_url(&url);
            if h.url.is_empty() {
                continue;
            }
            if h.source.is_empty() {
                h.source = seat.id.clone();
            }
            if h.sources.is_empty() {
                h.sources = vec![seat.id.clone()];
            }
            if h.date.as_deref().is_none_or(|d| d.trim().is_empty())
                && let Some(hint) = extract_date_hint(&h.snippet, self.today)
                    .or_else(|| extract_date_hint(&h.title, self.today))
            {
                h.date = Some(hint);
                h.date_source = Some("snippet".to_owned());
            }
            clean.push(h);
        }
        let raw = clean.len();
        let (mut kept, _) = filters::apply(clean, &self.filters, self.host_class, self.today);
        kept.truncate(self.per_source);
        let suspect = self.coherence && looks_like_decoy(&self.query, &kept);
        (kept, raw, suspect)
    }
}

/// Dispatch all seated agent-lane engines at `t = 0` inside [`scope_agent_lane`].
pub fn spawn_lane(lane_plan: LanePlan, lctx: &LaneCtx) -> SpawnedLane {
    let started = Instant::now();
    let seated: Vec<String> = lane_plan.seats.iter().map(|s| s.id.clone()).collect();
    let max_timeout = lane_plan
        .seats
        .iter()
        .map(|s| s.timeout)
        .max()
        .unwrap_or(Duration::from_secs(60));

    let mut tasks = Vec::with_capacity(lane_plan.seats.len());
    for seat in lane_plan.seats {
        let cancel = CancellationToken::new();
        let src = Arc::clone(&seat.src);
        let id = seat.id.clone();
        let timeout = seat.timeout;
        let lctx_cloned = lctx.clone();
        let seat_cloned = seat.clone();
        let cancel_cloned = cancel.clone();

        let handle = tokio::spawn(async move {
            let mut params = Map::new();
            params.insert("query".into(), json!(lctx_cloned.query));
            params.insert("limit".into(), json!(lctx_cloned.per_source as i64));
            let mut scx = SrcCx::from_config(&lctx_cloned.cfg, cancel_cloned, timeout);
            scx.lang = lctx_cloned.lang.clone();
            scx.freshness = lctx_cloned.freshness;
            scx.domains = lctx_cloned.domains.clone();

            let fut = scope_agent_lane(async move {
                AssertUnwindSafe(src.call("search", Value::Object(params), &scx))
                    .catch_unwind()
                    .await
            });

            let result = match tokio::time::timeout(timeout, fut).await {
                Ok(Ok(Ok(out))) => {
                    let (hits, raw, suspect) = lctx_cloned.prepare(&seat_cloned, out);
                    LaneResult::Answered { hits, raw, suspect }
                }
                Ok(Ok(Err(err))) => LaneResult::Failed(err),
                Ok(Err(_panic)) => {
                    LaneResult::Failed(FsmError::internal("agent_lane", "agent source panicked"))
                }
                Err(_) => LaneResult::TimedOut(timeout),
            };
            (id, result)
        });

        tasks.push(InFlightLane {
            seat,
            cancel,
            handle,
        });
    }

    SpawnedLane {
        started,
        nominal: lane_plan.nominal,
        seated,
        benched: lane_plan.benched,
        max_timeout,
        tasks,
    }
}

/// A still-running agent-lane engine task handed off for late background cache merge.
pub type LateLaneTask = (
    String,
    CancellationToken,
    tokio::task::JoinHandle<(String, LaneResult)>,
);

/// Output of settling the agent lane after keyless search finishes.
pub struct LaneSettlement {
    pub report: Option<AgentLaneReport>,
    pub merged_results: Vec<Hit>,
    pub engines_added: Vec<String>,
    /// All seated lane engines that settled in time and were not rate-limited
    /// (matches Python `aggregator.py::asked`).
    pub settled_engines: Vec<String>,
    /// True when a transient failure or pending lane engine should cap cache TTL to 1 hour.
    pub lane_missing: bool,
    pub extra_note: Option<String>,
    /// Remaining pending tasks (non-empty only when cacheable and tasks are still running).
    pub late_tasks: Vec<LateLaneTask>,
    pub settled_buckets: Vec<(String, Vec<Hit>)>,
    pub max_timeout: Duration,
}

fn is_passing_bench(reason: &str) -> bool {
    matches!(reason, "error" | "silent" | "timeout" | "http_429")
}

/// Wait on seated agent-lane engines per wait/grace rules, settle suspects & health,
/// and merge with keyless results.
#[allow(clippy::too_many_arguments)]
pub async fn await_and_settle(
    spawned: SpawnedLane,
    keyless_run: &Run,
    keyless_merged: &Merged,
    lctx: &LaneCtx,
    breaker: &Arc<EngineHealth>,
    store: &Arc<dyn StoreApi>,
    cacheable: bool,
    budget_deadline: Instant,
    caller_cancel: &CancellationToken,
) -> LaneSettlement {
    if spawned.nominal.is_empty() {
        return LaneSettlement {
            report: None,
            merged_results: keyless_merged.results.clone(),
            engines_added: Vec::new(),
            settled_engines: Vec::new(),
            lane_missing: false,
            extra_note: None,
            late_tasks: Vec::new(),
            settled_buckets: Vec::new(),
            max_timeout: spawned.max_timeout,
        };
    }

    if spawned.tasks.is_empty() {
        // Every configured signed-in agent engine was benched.
        let lane_missing = spawned
            .benched
            .iter()
            .any(|(_, reason)| is_passing_bench(reason));
        let hint = if !spawned.benched.is_empty() {
            let names: Vec<String> = spawned
                .benched
                .iter()
                .map(|(id, r)| format!("{id} ({r})"))
                .collect();
            Some(format!("Agent lane benched: {}.", names.join(", ")))
        } else {
            None
        };
        let mut report = AgentLaneReport::default();
        report.hint = hint.clone();
        return LaneSettlement {
            report: Some(report),
            merged_results: keyless_merged.results.clone(),
            engines_added: Vec::new(),
            settled_engines: Vec::new(),
            lane_missing,
            extra_note: hint,
            late_tasks: Vec::new(),
            settled_buckets: Vec::new(),
            max_timeout: spawned.max_timeout,
        };
    }

    let cfg = &lctx.cfg;
    let lane_wait = Duration::from_secs_f64(cfg.agent_lane_wait_seconds.max(0.0));
    let lane_grace = Duration::from_secs_f64(cfg.agent_lane_grace_seconds.max(0.0));
    let search_deadline = Duration::from_secs_f64(cfg.search_deadline_s.clamp(0.5, 120.0));

    let keyless_has_results = !keyless_merged.results.is_empty();
    let elapsed_now = spawned.started.elapsed();
    let initial_window = if keyless_has_results {
        lane_wait.min(elapsed_now + lane_grace)
    } else {
        lane_wait.max(search_deadline)
    };
    let mut wait_until = (spawned.started + initial_window).min(budget_deadline);

    // Collect results from tasks that finish before wait_until.
    let mut remaining: Vec<InFlightLane> = spawned.tasks;
    let mut settled: Vec<(String, LaneResult)> = Vec::new();

    // First drain anything that ALREADY completed while keyless was racing.
    let mut idx = 0;
    while idx < remaining.len() {
        if remaining[idx].handle.is_finished() {
            let task = remaining.remove(idx);
            if let Ok(res) = task.handle.await {
                if !keyless_has_results && is_lane_witness(&lctx.query, &res.1) {
                    wait_until = wait_until.min(spawned.started + lane_wait);
                }
                settled.push(res);
            }
        } else {
            idx += 1;
        }
    }

    // Now wait up to `wait_until` for remaining tasks.
    while !remaining.is_empty() && !caller_cancel.is_cancelled() {
        let now = Instant::now();
        if now >= wait_until {
            break;
        }
        // Race the remaining JoinHandles against wait_until & caller_cancel.
        let mut futs = FuturesUnordered::new();
        for (i, item) in remaining.iter_mut().enumerate() {
            let h = &mut item.handle;
            futs.push(async move { (i, h.await) });
        }
        let next = tokio::select! {
            biased;
            () = caller_cancel.cancelled() => None,
            () = tokio::time::sleep_until(wait_until) => None,
            res = futs.next() => res,
        };
        drop(futs);
        let Some((done_idx, join_res)) = next else {
            break;
        };
        remaining.remove(done_idx);
        if let Ok(res) = join_res {
            if !keyless_has_results && is_lane_witness(&lctx.query, &res.1) {
                wait_until = wait_until.min(spawned.started + lane_wait);
            }
            settled.push(res);
        }
    }

    let pending_ids: Vec<String> = remaining.iter().map(|t| t.seat.id.clone()).collect();
    let late_tasks = if cacheable {
        remaining
            .into_iter()
            .map(|t| (t.seat.id, t.cancel, t.handle))
            .collect()
    } else {
        // Non-cacheable searches cancel pending lane engines cleanly on return.
        for t in remaining {
            t.cancel.cancel();
            t.handle.abort();
        }
        Vec::new()
    };

    let keyless_buckets = extract_keyless_buckets(keyless_run, keyless_merged);
    let outcome =
        settle_and_merge_buckets(&lctx.query, &keyless_buckets, settled, lctx, breaker, store)
            .await;

    let lane_missing = outcome.transient_failure
        || !pending_ids.is_empty()
        || spawned
            .benched
            .iter()
            .any(|(_, reason)| is_passing_bench(reason));

    let mut hint_parts = Vec::new();
    if !outcome.answered.is_empty() {
        hint_parts.push(format!("answered: {}", outcome.answered.join(", ")));
    }
    if !pending_ids.is_empty() {
        if cacheable {
            hint_parts.push(format!(
                "still running in background (will merge into cache): {}",
                pending_ids.join(", ")
            ));
        } else {
            hint_parts.push(format!(
                "did not finish in time: {}",
                pending_ids.join(", ")
            ));
        }
    }
    if !outcome.refused.is_empty() {
        hint_parts.push(format!("refused: {}", outcome.refused.join(", ")));
    }
    if !spawned.benched.is_empty() {
        let names: Vec<String> = spawned
            .benched
            .iter()
            .map(|(id, r)| format!("{id} ({r})"))
            .collect();
        hint_parts.push(format!("benched: {}", names.join(", ")));
    }
    let hint = (!hint_parts.is_empty()).then(|| format!("Agent lane: {}.", hint_parts.join("; ")));

    let mut report = AgentLaneReport::default();
    report.seated = spawned.seated;
    report.answered = outcome.answered.clone();
    report.pending = pending_ids;
    report.hint = hint.clone();
    LaneSettlement {
        report: Some(report),
        merged_results: outcome.merged_results,
        engines_added: outcome.answered,
        settled_engines: outcome.settled_engines,
        lane_missing,
        extra_note: hint,
        late_tasks,
        settled_buckets: outcome.kept_lane_buckets,
        max_timeout: spawned.max_timeout,
    }
}

/// Spawn a background task to collect late pending lane engines and merge them into
/// the cached `SearchResponse`, respecting row ownership (`row_gen`) and `late_ceiling`.
#[allow(clippy::too_many_arguments)]
pub fn spawn_late_merge(
    key: String,
    row_gen: u64,
    full_ttl: u64,
    started: Instant,
    base_resp: SearchResponse,
    keyless_buckets: Vec<(String, Vec<Hit>)>,
    already_settled_lane_buckets: Vec<(String, Vec<Hit>)>,
    late_tasks: Vec<LateLaneTask>,
    max_timeout: Duration,
    lctx: LaneCtx,
    breaker: Arc<EngineHealth>,
    store: Arc<dyn StoreApi>,
    state: Arc<AgentLaneState>,
) {
    if late_tasks.is_empty() || full_ttl == 0 {
        return;
    }
    // Oracle formula: _late_ceiling = 4 * max(timeout) + 30s
    let late_ceiling = max_timeout
        .saturating_mul(4)
        .saturating_add(Duration::from_secs(30));

    tokio::spawn(async move {
        let deadline = Instant::now() + late_ceiling;
        let mut collected: Vec<(String, LaneResult)> = Vec::new();
        let mut futs = FuturesUnordered::new();
        let cancels: Vec<CancellationToken> =
            late_tasks.iter().map(|(_, c, _)| c.clone()).collect();

        for (id, _, handle) in late_tasks {
            futs.push(async move {
                match handle.await {
                    Ok(res) => res,
                    Err(_) => (
                        id,
                        LaneResult::Failed(FsmError::internal("agent_lane", "late task aborted")),
                    ),
                }
            });
        }

        loop {
            let rem = deadline.saturating_duration_since(Instant::now());
            if rem.is_zero() {
                for c in &cancels {
                    c.cancel();
                }
                break;
            }
            match tokio::time::timeout(rem, futs.next()).await {
                Ok(Some(res)) => collected.push(res),
                Ok(None) => break,
                Err(_) => {
                    for c in &cancels {
                        c.cancel();
                    }
                    break;
                }
            }
        }

        if collected.is_empty() {
            return;
        }

        let lock = state.row_lock(&key);
        let _guard = lock.lock().await;
        if !state.owns_row(&key, row_gen) {
            // A newer search refreshed this cache key — never overwrite it!
            return;
        }

        let mut combined_base = keyless_buckets;
        combined_base.extend(already_settled_lane_buckets);

        let settled = settle_and_merge_buckets(
            &lctx.query,
            &combined_base,
            collected,
            &lctx,
            &breaker,
            &store,
        )
        .await;

        let mut updated = base_resp;
        if !settled.merged_results.is_empty() {
            updated.results = settled.merged_results;
            updated.lead =
                lead::lead_snippet(&lctx.query, &updated.results, &lctx.weighting.direct);
        }
        for id in &settled.settled_engines {
            if !updated.engines.contains(id) {
                updated.engines.push(id.clone());
            }
        }
        if let Some(ref mut report) = updated.agent_lane {
            for id in &settled.answered {
                if !report.answered.contains(id) {
                    report.answered.push(id.clone());
                }
            }
            report.pending.clear();
            report.hint = if report.answered.is_empty() {
                None
            } else {
                Some(format!(
                    "Agent lane: answered: {}.",
                    report.answered.join(", ")
                ))
            };
        }
        // Refresh Agent lane note line cleanly.
        updated.notes.retain(|n| !n.starts_with("Agent lane:"));
        if let Some(ref h) = updated.agent_lane.as_ref().and_then(|r| r.hint.clone()) {
            updated.notes.push(h.clone());
        }

        // Preserve remaining TTL relative to original start; if no transient failure, lift 1h cap to full TTL.
        let elapsed_s = started.elapsed().as_secs();
        let target_ttl = if settled.transient_failure {
            full_ttl.min(crate::cache::DEGRADED_TTL_S)
        } else {
            full_ttl
        };
        let remaining_ttl = target_ttl.saturating_sub(elapsed_s).max(1);
        if !updated.results.is_empty()
            && let Err(e) = store.search_cache_put(&key, &updated, remaining_ttl).await
        {
            tracing::debug!(error = %e, "late agent-lane cache merge write failed");
        }
    });
}

fn is_lane_witness(query: &str, res: &LaneResult) -> bool {
    matches!(res, LaneResult::Answered { hits, suspect: false, .. } if !hits.is_empty() && is_witness(query, hits))
}

/// Extract usable keyless buckets from a completed keyless `Run` + `Merged`.
pub fn extract_keyless_buckets(run: &Run, merged: &Merged) -> Vec<(String, Vec<Hit>)> {
    let mut out = Vec::new();
    for lane in &run.lanes {
        if merged.off_topic.contains(&lane.seat.id) {
            continue;
        }
        if let Outcome::Answered { hits, .. } = &lane.outcome
            && !hits.is_empty()
        {
            out.push((lane.seat.id.clone(), hits.clone()));
        }
    }
    out
}

struct SettleOutcome {
    merged_results: Vec<Hit>,
    answered: Vec<String>,
    settled_engines: Vec<String>,
    refused: Vec<String>,
    kept_lane_buckets: Vec<(String, Vec<Hit>)>,
    transient_failure: bool,
}

async fn settle_and_merge_buckets(
    query: &str,
    keyless_buckets: &[(String, Vec<Hit>)],
    settled: Vec<(String, LaneResult)>,
    lctx: &LaneCtx,
    breaker: &Arc<EngineHealth>,
    store: &Arc<dyn StoreApi>,
) -> SettleOutcome {
    let mut has_witness = keyless_buckets
        .iter()
        .any(|(_, hits)| is_witness(query, hits));

    for (_, res) in &settled {
        if is_lane_witness(query, res) {
            has_witness = true;
            break;
        }
    }

    let keyless_nonempty = keyless_buckets.iter().any(|(_, h)| !h.is_empty());
    let mut kept_lane_buckets: Vec<(String, Vec<Hit>)> = Vec::new();
    let mut answered = Vec::new();
    let mut settled_engines = Vec::new();
    let mut refused = Vec::new();
    let mut transient_failure = false;

    for (id, res) in settled {
        let is_rate_limited = matches!(res, LaneResult::Failed(FsmError::RateLimited { .. }));
        if !is_rate_limited && !settled_engines.contains(&id) {
            settled_engines.push(id.clone());
        }
        match res {
            LaneResult::Answered { hits, raw, suspect } => {
                if hits.is_empty() {
                    if raw == 0 && lctx.cfg.health_enabled {
                        // Empty lane response does not crash search; record nothing unless persistent.
                    }
                    continue;
                }
                if suspect {
                    // `_settle_lane_suspects`:
                    // Serve a doubted lane bucket ONLY when it is all there is (no witness AND no keyless results).
                    if has_witness || keyless_nonempty {
                        if lctx.cfg.health_enabled
                            && let Some(row) = breaker.record_failure(
                                &id,
                                "lane_unconfirmed",
                                Some(LANE_UNCONFIRMED_THRESHOLD),
                            )
                        {
                            let _ = store.health_save(&row).await;
                        }
                        continue;
                    }
                }
                if lctx.cfg.health_enabled
                    && let Some(row) = breaker.record_success(&id)
                {
                    let _ = store.health_save(&row).await;
                }
                answered.push(id.clone());
                kept_lane_buckets.push((id, hits));
            }
            LaneResult::Failed(err) => {
                if is_lasting_agent_failure(&err) {
                    let msg = strip_agent_lasting_tag(&err.to_string()).to_owned();
                    refused.push(format!("{id} ({msg})"));
                    if lctx.cfg.health_enabled
                        && let Some(row) = breaker.record_failure(
                            &id,
                            "lane_refused",
                            Some(LANE_REFUSED_THRESHOLD),
                        )
                    {
                        let _ = store.health_save(&row).await;
                    }
                    // Lasting refusal does NOT mark `lane_missing` (don't shorten cache TTL).
                } else {
                    transient_failure = true;
                    let reason = match &err {
                        FsmError::RateLimited { .. } => "http_429",
                        FsmError::Timeout(_) => "timeout",
                        _ => "error",
                    };
                    if lctx.cfg.health_enabled
                        && let Some(row) =
                            breaker.record_failure(&id, reason, Some(LANE_SOFT_THRESHOLD))
                    {
                        let _ = store.health_save(&row).await;
                    }
                }
            }
            LaneResult::TimedOut(_) => {
                transient_failure = true;
                if lctx.cfg.health_enabled
                    && let Some(row) =
                        breaker.record_failure(&id, "timeout", Some(LANE_SOFT_THRESHOLD))
                {
                    let _ = store.health_save(&row).await;
                }
            }
        }
    }

    let mut all_buckets: Vec<(String, Vec<Hit>)> = keyless_buckets.to_vec();
    all_buckets.extend(kept_lane_buckets.clone());
    let merged_results = fuse(&all_buckets, &lctx.weighting, lctx.limit);

    SettleOutcome {
        merged_results,
        answered,
        settled_engines,
        refused,
        kept_lane_buckets,
        transient_failure,
    }
}
