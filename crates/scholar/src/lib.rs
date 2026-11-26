//! `fsm-scholar` — the scholar core (lane W3a; SPEC.md §6).
//!
//! * [`ScholarCore::search`]: keyless pool fan-out (arXiv + Crossref +
//!   OpenAIRE + Europe PMC, keyed OpenAlex / S2 / CORE joiners, sub-scope
//!   specialists) under a 2.5 s soft / 6 s hard deadline → two-stage
//!   Union-Find dedup → field-level merge → top-N OpenAlex / S2 enrichment
//!   (concurrency 4, 800 ms, 30-day cache, UTC-midnight bench on 429) →
//!   retraction flags from the local Retraction Watch table → ranking.
//! * [`ScholarCore::work`] / [`ScholarCore::resolve`]: one identifier (DOI,
//!   arXiv, PMID, PMCID, OpenAlex W-id, S2 id) or a title, resolved across
//!   registries, with the preprint → published crosswalk
//!   (`arXiv:1706.03762` resolves to the NeurIPS paper, not to a paper that
//!   merely starts with the same words).
//! * [`ScholarCore::normalize_ids`] / [`ScholarCore::merge_works`]: the pure
//!   building blocks, also used by `fsm-scholar-graph` / `-fulltext`.

pub mod fuzzy;
pub mod ids;
pub mod parse;
pub mod rank;

mod dedup;
mod enrich;
mod lookup;
mod merge;
mod pool;
mod resolve;

pub use dedup::TITLE_DEDUP_MIN;
pub use ids::{Pid, canonical_key, classify};
pub use lookup::{Provider, next_utc_midnight};
pub use pool::{DEFAULT_POOL, KEYED_POOL, SUBSCOPE_SOURCES, SUBSCOPE_WEIGHT};
pub use rank::SortMode;
pub use resolve::Resolution;

use fsm_core::{
    CallCx, FsmError, Registry, ScholarQuery, ScholarlyWork, SearchMeta, SourceDeps, WorkIds,
    WorkResult,
};
use lookup::{Bench, Clock, Http, LookupCx, rfc3339};
use merge::{Member, merge_members};
use pool::{Deadlines, SourceList};
use rank::Ranked;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

/// Merged works are cached this long in `scholar_works`.
pub const WORK_CACHE_TTL_S: u64 = 30 * 86_400;
/// Complete keyword-search answers are cached this long (SPEC.md §5).
pub const SEARCH_CACHE_TTL_S: u64 = 12 * 3_600;
/// Largest `limit` a search accepts.
pub const MAX_LIMIT: u32 = 50;

/// Deadlines and budgets of [`ScholarCore`] (SPEC.md §6, I16).
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct Timing {
    /// The fan-out returns here when any source has answered with results.
    pub soft_deadline: Duration,
    /// The fan-out never waits longer (also bounded by the call's deadline).
    pub hard_deadline: Duration,
    /// Budget of the whole top-N enrichment step.
    pub enrich_deadline: Duration,
    /// Enrichment requests in flight at once.
    pub enrich_concurrency: usize,
    /// How many of the best-ranked works are enriched.
    pub enrich_top_n: usize,
    /// Per-request timeout of identifier lookups (`work` / `resolve`).
    pub lookup_timeout: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            soft_deadline: Duration::from_millis(2_500),
            hard_deadline: Duration::from_secs(6),
            enrich_deadline: Duration::from_millis(800),
            enrich_concurrency: 4,
            enrich_top_n: 5,
            lookup_timeout: Duration::from_secs(6),
        }
    }
}

/// Scholarly search, lookup, id normalisation and merging.
pub struct ScholarCore {
    reg: Arc<Registry>,
    deps: SourceDeps,
    bench: Bench,
    clock: Clock,
    timing: Timing,
}

impl std::fmt::Debug for ScholarCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScholarCore")
            .field("timing", &self.timing)
            .finish_non_exhaustive()
    }
}

/// Calendar year (UTC) of a Unix timestamp.
fn utc_year(secs: i64) -> i32 {
    // Howard Hinnant's civil_from_days.
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let y = yoe + era * 400 + i64::from(mp >= 10);
    i32::try_from(y).unwrap_or(i32::MAX)
}

impl ScholarCore {
    pub fn new(reg: Arc<Registry>, deps: SourceDeps) -> Self {
        Self {
            reg,
            deps,
            bench: Bench::default(),
            clock: lookup::system_clock(),
            timing: Timing::default(),
        }
    }

    /// Replace the Unix-seconds clock (bench expiry, recency). For tests.
    #[must_use]
    pub fn with_clock(mut self, clock: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    /// Replace the deadlines and budgets.
    #[must_use]
    pub fn with_timing(mut self, timing: Timing) -> Self {
        self.timing = timing;
        self
    }

    /// The deadlines and budgets in use.
    pub fn timing(&self) -> Timing {
        self.timing
    }

    /// `Some(unix_secs)` while `provider` is benched after an HTTP 429
    /// (OpenAlex and Semantic Scholar only; the bench survives restarts
    /// through `api_cache`). Sibling crates check this before calling them.
    pub async fn benched_until(&self, provider: Provider) -> Option<i64> {
        self.bench.until(provider, &self.deps, (self.clock)()).await
    }

    /// Record an HTTP 429 a sibling crate got from `provider`: benches
    /// OpenAlex / S2 until the next UTC midnight and returns that time;
    /// `None` for providers that do not bench.
    pub async fn note_rate_limited(&self, provider: Provider) -> Option<i64> {
        if !provider.benches_on_429() {
            return None;
        }
        Some(self.bench.bench(provider, &self.deps, (self.clock)()).await)
    }

    pub(crate) fn http(&self) -> Http<'_> {
        Http {
            reg: &self.reg,
            deps: &self.deps,
            bench: &self.bench,
            clock: &self.clock,
        }
    }

    pub(crate) fn lookup_cx(&self, cx: &CallCx) -> LookupCx {
        LookupCx {
            cancel: cx.cancel.child_token(),
            timeout: self
                .timing
                .lookup_timeout
                .min(cx.remaining())
                .max(Duration::from_millis(1)),
        }
    }

    /// Scholarly search. A query that is itself an identifier (DOI, arXiv,
    /// PMID, PMCID, OpenAlex / S2 id) is resolved instead of searched,
    /// unless explicit `sources` are given.
    pub async fn search(&self, q: ScholarQuery, cx: &CallCx) -> Result<WorkResult, FsmError> {
        let query = q.query.trim().to_owned();
        if query.is_empty() {
            return Err(FsmError::invalid(
                "query is empty; pass keywords, a title, a DOI or an arXiv id",
            ));
        }
        let sort = SortMode::parse(q.sort.as_deref()).ok_or_else(|| {
            FsmError::invalid(format!(
                "unknown sort '{}'; use relevance, citations or recent",
                q.sort.as_deref().unwrap_or_default()
            ))
        })?;
        if let (Some(a), Some(b)) = (q.year_from, q.year_to)
            && a > b
        {
            return Err(FsmError::invalid(format!(
                "yearFrom {a} is after yearTo {b}"
            )));
        }
        let limit = if q.limit == 0 {
            10
        } else {
            q.limit.min(MAX_LIMIT)
        } as usize;
        // Validated up front so an identifier query rejects it too.
        let subscope = pool::checked_subscope(q.subscope.as_deref())?;
        let pid = classify(&query);
        if !pid.is_title() && q.sources.as_ref().is_none_or(Vec::is_empty) {
            return self.search_by_id(&q, pid, subscope.map(|d| d.id), cx).await;
        }
        self.keyword_search(&q, &query, sort, limit, cx).await
    }

    async fn search_by_id(
        &self,
        q: &ScholarQuery,
        pid: Pid,
        subscope: Option<&str>,
        cx: &CallCx,
    ) -> Result<WorkResult, FsmError> {
        let started = tokio::time::Instant::now();
        let r = self.resolve_inner(pid, cx).await?;
        if r.work.is_none() && r.upstream_failed {
            return Err(FsmError::upstream(
                "scholar",
                None,
                format!(
                    "could not look up {} {}: {}",
                    r.pid.kind(),
                    r.pid.value(),
                    r.notes.join("; ")
                ),
            ));
        }
        let mut meta = SearchMeta::new(q.query.trim());
        meta.subscope = subscope.map(str::to_owned);
        meta.notes = r.notes;
        meta.notes.insert(
            0,
            format!(
                "Read the query as {} {}; looked it up instead of searching.",
                r.pid.kind(),
                r.pid.value()
            ),
        );
        meta.cached = r.cached;
        let works: Vec<ScholarlyWork> = r
            .work
            .into_iter()
            .filter(|w| passes_filters(w, q))
            .collect();
        meta.sources = works.first().map(|w| w.sources.clone()).unwrap_or_default();
        meta.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        Ok(WorkResult::new(works, meta))
    }

    /// Keyword search over the pool (no identifier routing).
    pub(crate) async fn keyword_search(
        &self,
        q: &ScholarQuery,
        query: &str,
        sort: SortMode,
        limit: usize,
        cx: &CallCx,
    ) -> Result<WorkResult, FsmError> {
        let started = tokio::time::Instant::now();
        let (members, mut notes) = pool::select(&self.reg, &self.deps.cfg, q)?;
        let cache_key = search_cache_key(q, query, sort, limit, &members);
        if let Some(hit) = self.cached_search(&cache_key, query, started).await {
            return Ok(hit);
        }

        // Benched providers (an HTTP 429 earlier today) are not searched.
        let now = (self.clock)();
        let mut kept = Vec::with_capacity(members.len());
        let mut benched_any = false;
        for m in members {
            if let Some(p) = Provider::for_source_id(m.id()).filter(|p| p.benches_on_429())
                && let Some(until) = self.bench.until(p, &self.deps, now).await
            {
                notes.push(format!(
                    "{}: benched until {} after HTTP 429; skipped",
                    m.id(),
                    rfc3339(until)
                ));
                benched_any = true;
                continue;
            }
            kept.push(m);
        }
        let members = kept;
        if members.is_empty() {
            return Err(FsmError::upstream(
                "scholar",
                Some(429),
                format!("no scholarly source available: {}", notes.join("; ")),
            ));
        }

        let per_source = u32::try_from(limit.clamp(10, 25)).unwrap_or(25);
        let deadlines = Deadlines {
            soft: self.timing.soft_deadline,
            hard: self.timing.hard_deadline,
        };
        let fan = pool::fan_out(&members, query, per_source, &self.deps.cfg, cx, deadlines).await?;
        notes.extend(fan.notes);
        // A 429 from a search: bench for the day when it certainly came from
        // upstream; a local wait only keeps the provider out of this call.
        let mut skip: BTreeSet<Provider> = BTreeSet::new();
        for (id, upstream) in &fan.rate_limited {
            if let Some(p) = Provider::for_source_id(id).filter(|p| p.benches_on_429()) {
                skip.insert(p);
                if *upstream {
                    let until = self.bench.bench(p, &self.deps, (self.clock)()).await;
                    notes.push(format!(
                        "{id}: benched until {} after HTTP 429",
                        rfc3339(until)
                    ));
                }
            }
        }
        let mut lists = fan.lists;
        if lists.is_empty() {
            return Err(FsmError::upstream(
                "scholar",
                None,
                format!("no scholarly source answered: {}", notes.join("; ")),
            ));
        }
        // Pool order, not arrival order: merges are deterministic.
        let order: HashMap<&str, usize> = members
            .iter()
            .enumerate()
            .map(|(i, m)| (m.id(), i))
            .collect();
        lists.sort_by_key(|l| order.get(l.id.as_str()).copied().unwrap_or(usize::MAX));
        let answered: Vec<String> = lists.iter().map(|l| l.id.clone()).collect();
        let rrf_max: f64 = lists.iter().map(|l| l.weight / (rank::RRF_K + 1.0)).sum();

        let mut cands = candidates(lists);
        let before = cands.len();
        cands.retain(|c| passes_years(&c.work, q));
        if cands.len() < before {
            notes.push(format!(
                "{} result(s) outside the year range (or undated) were dropped",
                before - cands.len()
            ));
        }
        let this_year = utc_year((self.clock)());

        // Enrich the most RELEVANT works whatever the requested order: a
        // citations sort must not pass over a work only because its count is
        // still unknown; with oaOnly, works already known closed are skipped.
        rank::score_all(query, &mut cands, rrf_max, this_year);
        rank::sort(&mut cands, SortMode::Relevance);
        let n = self.timing.enrich_top_n.min(limit);
        let picks: Vec<usize> = (0..cands.len())
            .filter(|&i| !(q.oa_only && is_known_closed(&cands[i].work)))
            .take(n)
            .collect();
        let mut used: BTreeSet<String> = BTreeSet::new();
        let enriched_fully = self
            .enrich(&mut cands, &picks, &skip, cx, &mut notes, &mut used)
            .await;

        for c in &mut cands {
            self.apply_retraction(&mut c.work).await;
        }
        if q.oa_only {
            let before = cands.len();
            cands.retain(|c| is_open(&c.work));
            if cands.len() < before {
                notes.push(format!(
                    "{} result(s) without a known open-access copy were dropped (oaOnly)",
                    before - cands.len()
                ));
            }
        }
        rank::score_all(query, &mut cands, rrf_max, this_year);
        rank::sort(&mut cands, sort);
        cands.truncate(limit);

        let mut meta = SearchMeta::new(query);
        meta.subscope = q
            .subscope
            .as_deref()
            .and_then(pool::resolve_subscope)
            .map(|d| d.id.to_owned());
        meta.sources = answered;
        for u in used {
            if !meta.sources.contains(&u) {
                meta.sources.push(u);
            }
        }
        meta.notes = notes;
        meta.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let result = WorkResult::new(cands.into_iter().map(|c| c.work).collect(), meta);
        // Only a complete answer is cached: every pool member answered, none
        // was benched away, and every launched enrichment lookup finished.
        if fan.complete
            && !benched_any
            && skip.is_empty()
            && enriched_fully
            && let Ok(body) = serde_json::to_vec(&result)
        {
            let _ = self
                .deps
                .store
                .api_cache_put(&cache_key, "scholar", &body, None, None, SEARCH_CACHE_TTL_S)
                .await;
        }
        Ok(result)
    }

    /// A cached complete search, with retraction flags refreshed.
    async fn cached_search(
        &self,
        key: &str,
        query: &str,
        started: tokio::time::Instant,
    ) -> Option<WorkResult> {
        let (body, _, _) = self.deps.store.api_cache_get(key).await.ok()??;
        let mut hit: WorkResult = serde_json::from_slice(&body).ok()?;
        for w in &mut hit.works {
            self.apply_retraction(w).await;
        }
        // Retracted works stay listed after the rest (stable).
        hit.works.sort_by_key(|w| w.retracted);
        hit.meta.query = query.to_owned();
        hit.meta.cached = true;
        hit.meta.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        Some(hit)
    }

    /// Flag a work the local Retraction Watch table lists as retracted.
    async fn apply_retraction(&self, w: &mut ScholarlyWork) {
        let Some(doi) = w.ids.doi.clone() else {
            return;
        };
        if let Ok(Some(n)) = self.deps.store.retraction_lookup(&doi).await
            && is_retraction_nature(&n.nature)
        {
            w.retracted = true;
            if w.retraction_reason.is_none() {
                let mut why = n.nature.clone();
                if let Some(r) = n.reason.as_deref().filter(|r| !r.trim().is_empty()) {
                    why.push_str(": ");
                    why.push_str(r.trim());
                }
                if let Some(d) = &n.date {
                    why.push_str(&format!(" ({d})"));
                }
                w.retraction_reason = Some(why);
            }
        }
    }

    /// Resolve an identifier or a title to one merged work, with the
    /// diagnostics a caller needs to explain a miss. `Err` only for an empty
    /// input or a cancelled call; lookup failures are in `notes`.
    pub async fn resolve(&self, id_or_title: &str, cx: &CallCx) -> Result<Resolution, FsmError> {
        let raw = id_or_title.trim();
        if raw.is_empty() {
            return Err(FsmError::invalid(
                "id is empty; pass a DOI, an arXiv id, a PMID/PMCID, an OpenAlex/S2 id or a title",
            ));
        }
        self.resolve_inner(classify(raw), cx).await
    }

    async fn resolve_inner(&self, pid: Pid, cx: &CallCx) -> Result<Resolution, FsmError> {
        if let Some(key) = pid.canonical_key()
            && let Ok(Some(mut w)) = self.deps.store.scholar_get(&key).await
        {
            // The Retraction Watch table may have synced since this was cached.
            self.apply_retraction(&mut w).await;
            let mut r = Resolution::new(pid);
            r.work = Some(w);
            r.cached = true;
            return Ok(r);
        }
        let mut r = self.resolve_pid(&pid, cx).await;
        if cx.cancel.is_cancelled() {
            return Err(FsmError::Timeout(0));
        }
        if let Some(w) = r.work.as_mut() {
            self.apply_retraction(w).await;
            if !pid.is_title()
                && !r.partial
                && let Some(key) = canonical_key(&w.ids)
            {
                let _ = self.deps.store.scholar_put(&key, w, WORK_CACHE_TTL_S).await;
            }
        }
        Ok(r)
    }

    /// One work by identifier or title. `Ok(None)` when the registries that
    /// answered do not know it; `Err(Upstream)` only when none answered.
    pub async fn work(
        &self,
        id_or_title: &str,
        cx: &CallCx,
    ) -> Result<Option<ScholarlyWork>, FsmError> {
        let r = self.resolve(id_or_title, cx).await?;
        if r.work.is_none() && r.upstream_failed {
            return Err(FsmError::upstream(
                "scholar",
                None,
                format!(
                    "could not look up '{}': {}",
                    id_or_title.trim(),
                    r.notes.join("; ")
                ),
            ));
        }
        Ok(r.work)
    }

    /// Classify and normalise one raw identifier (DOI in any URL / `doi:`
    /// form, arXiv id or URL, PMID, PMCID, OpenAlex W-id, S2 id). A title
    /// or unknown input gives empty ids.
    pub fn normalize_ids(raw: &str) -> WorkIds {
        classify(raw).to_ids()
    }

    /// Normalise, deduplicate (exact ids, then title + year + first author)
    /// and field-merge works. Output order follows the first member of each
    /// cluster.
    pub fn merge_works(works: Vec<ScholarlyWork>) -> Vec<ScholarlyWork> {
        let prepared: Vec<ScholarlyWork> =
            works.into_iter().map(|w| merge::prepare(w, None)).collect();
        dedup::clusters(&prepared)
            .into_iter()
            .map(|g| {
                let members: Vec<Member> = g
                    .iter()
                    .map(|&i| Member::primary(prepared[i].clone()))
                    .collect();
                merge_members(&members)
            })
            .collect()
    }
}

/// Retraction Watch natures that take a work out of the record:
/// retraction, withdrawal, removal (not a reinstatement, correction or
/// expression of concern).
fn is_retraction_nature(nature: &str) -> bool {
    let n = nature.to_ascii_lowercase();
    !n.contains("reinstat")
        && (n.contains("retract") || n.contains("withdraw") || n.contains("remov"))
}

/// Versioned cache key of one keyword search: everything that changes the
/// answer (normalised query, canonical subscope, sort, years, oaOnly, limit
/// and the selected source pool, which reflects the configured keys).
fn search_cache_key(
    q: &ScholarQuery,
    query: &str,
    sort: SortMode,
    limit: usize,
    members: &[pool::PoolMember],
) -> String {
    let mut ids: Vec<&str> = members.iter().map(pool::PoolMember::id).collect();
    ids.sort_unstable();
    let norm: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let subscope = q
        .subscope
        .as_deref()
        .and_then(pool::resolve_subscope)
        .map(|d| d.id);
    let shape = serde_json::json!({
        "q": norm.join(" "),
        "subscope": subscope,
        "sort": format!("{sort:?}"),
        "yearFrom": q.year_from,
        "yearTo": q.year_to,
        "oaOnly": q.oa_only,
        "limit": limit,
        "pool": ids,
    });
    let digest = fsm_core::fixture::sha256_hex(shape.to_string().as_bytes());
    format!("scholar:search:v1:{}", &digest[..32])
}

fn is_known_closed(w: &ScholarlyWork) -> bool {
    w.oa.as_ref()
        .is_some_and(|o| o.status.eq_ignore_ascii_case("closed"))
}

/// Dedup the source lists into ranked candidates (RRF ranks per list).
fn candidates(lists: Vec<SourceList>) -> Vec<Ranked> {
    let mut flat: Vec<ScholarlyWork> = Vec::new();
    let mut origin: Vec<(usize, usize)> = Vec::new();
    let weights: Vec<f64> = lists.iter().map(|l| l.weight).collect();
    for (li, l) in lists.into_iter().enumerate() {
        for (pos, w) in l.works.into_iter().enumerate() {
            flat.push(w);
            origin.push((li, pos + 1));
        }
    }
    dedup::clusters(&flat)
        .into_iter()
        .map(|g| {
            let mut best: HashMap<usize, usize> = HashMap::new();
            for &i in &g {
                let (li, rank) = origin[i];
                let e = best.entry(li).or_insert(rank);
                *e = (*e).min(rank);
            }
            let mut ranks: Vec<(usize, usize)> = best.into_iter().collect();
            ranks.sort_unstable();
            let members: Vec<Member> = g
                .iter()
                .map(|&i| Member::primary(flat[i].clone()))
                .collect();
            Ranked {
                work: merge_members(&members),
                members,
                ranks: ranks.into_iter().map(|(li, r)| (weights[li], r)).collect(),
                score: 0.0,
            }
        })
        .collect()
}

fn passes_years(w: &ScholarlyWork, q: &ScholarQuery) -> bool {
    if q.year_from.is_none() && q.year_to.is_none() {
        return true;
    }
    w.year
        .is_some_and(|y| q.year_from.is_none_or(|a| y >= a) && q.year_to.is_none_or(|b| y <= b))
}

fn is_open(w: &ScholarlyWork) -> bool {
    w.oa.as_ref()
        .is_some_and(|o| !o.status.trim().is_empty() && !o.status.eq_ignore_ascii_case("closed"))
}

fn passes_filters(w: &ScholarlyWork, q: &ScholarQuery) -> bool {
    passes_years(w, q) && (!q.oa_only || is_open(w))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_year_of_known_instants() {
        assert_eq!(utc_year(0), 1970);
        assert_eq!(utc_year(1_795_613_400), 2026);
        assert_eq!(utc_year(951_782_400), 2000); // 2000-02-29
        assert_eq!(utc_year(978_307_199), 2000); // 2000-12-31T23:59:59Z
        assert_eq!(utc_year(978_307_200), 2001);
        assert_eq!(utc_year(-1), 1969);
    }

    #[test]
    fn normalize_ids_reads_every_identifier_form() {
        let ids = ScholarCore::normalize_ids("https://arxiv.org/abs/1706.03762v7");
        assert_eq!(ids.arxiv.as_deref(), Some("1706.03762"));
        let ids = ScholarCore::normalize_ids("arXiv:1706.03762");
        assert_eq!(ids.arxiv.as_deref(), Some("1706.03762"));
        let ids = ScholarCore::normalize_ids("doi:10.48550/arXiv.1706.03762");
        assert_eq!(ids.arxiv.as_deref(), Some("1706.03762"));
        let ids = ScholarCore::normalize_ids("https://doi.org/10.1016/S0140-6736(20)31180-6.");
        assert_eq!(ids.doi.as_deref(), Some("10.1016/s0140-6736(20)31180-6"));
        let ids = ScholarCore::normalize_ids("PMID: 32450107");
        assert_eq!(ids.pmid.as_deref(), Some("32450107"));
        assert_eq!(
            ScholarCore::normalize_ids("Attention Is All You Need"),
            WorkIds::default()
        );
    }

    #[test]
    fn merge_works_dedups_and_merges() {
        let mut a = ScholarlyWork::new("Attention Is All You Need", "");
        a.ids.doi = Some("https://doi.org/10.48550/arXiv.1706.03762".into());
        a.sources = vec!["openaire".into()];
        let mut b = ScholarlyWork::new(
            "Attention is all you need",
            "https://arxiv.org/abs/1706.03762",
        );
        b.ids.arxiv = Some("1706.03762v5".into());
        b.year = Some(2017);
        b.sources = vec!["arxiv".into()];
        let c = ScholarlyWork::new("Something else entirely", "");
        let out = ScholarCore::merge_works(vec![a, c, b]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].ids.arxiv.as_deref(), Some("1706.03762"));
        assert_eq!(out[0].year, Some(2017));
        assert_eq!(out[0].sources, ["arxiv", "openaire"]);
        assert_eq!(out[1].title, "Something else entirely");
        assert!(ScholarCore::merge_works(Vec::new()).is_empty());
    }

    #[test]
    fn year_and_oa_filters() {
        let mut q = ScholarQuery::new("x");
        let mut w = ScholarlyWork::new("t", "");
        assert!(passes_filters(&w, &q));
        q.year_from = Some(2020);
        assert!(!passes_filters(&w, &q), "undated works fail a year filter");
        w.year = Some(2021);
        assert!(passes_filters(&w, &q));
        q.year_to = Some(2020);
        assert!(!passes_filters(&w, &q));
        let mut q = ScholarQuery::new("x");
        q.oa_only = true;
        assert!(!passes_filters(&w, &q));
        w.oa = Some(fsm_core::OaLocation::new("closed"));
        assert!(!passes_filters(&w, &q));
        w.oa = Some(fsm_core::OaLocation::new("green"));
        assert!(passes_filters(&w, &q));
    }
}
