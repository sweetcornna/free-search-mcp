//! Source pool selection and the deadline-bounded fan-out (SPEC.md §6
//! "Keyless Pool & Enrichment Budget").
//!
//! * Default keyless pool: exactly [`DEFAULT_POOL`], by id (never by scope:
//!   CORE declares `academic` too and must not join without a key).
//! * Keyed joiners ([`KEYED_POOL`]): OpenAlex, Semantic Scholar and CORE
//!   search join only when their key is configured.
//! * Sub-scopes add specialists ([`SUBSCOPE_SOURCES`], RRF weight 2.0);
//!   sub-scopes without an entry add up to three registered sources of that
//!   scope that can search.
//! * An explicit `sources` list replaces the pool.
//!
//! Every source is called with op `search` and only the parameters that op
//! declares (`query`, `limit`); `SrcCx.api_key` is filled here because this
//! path bypasses the dispatcher. The fan-out returns at the 2.5 s soft
//! deadline once any source has answered with results, at the 6 s hard
//! deadline (or the caller's deadline) otherwise; stragglers are dropped.

use fsm_core::scopes::{self, ScopeDef};
use fsm_core::{
    AuthMode, Availability, CallCx, Capability, Config, FsmError, PoolRole, Registry, ScholarQuery,
    ScholarlyWork, Source, SrcCx, SrcOutput,
};
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use serde_json::{Map, Value};
use std::sync::Arc;
use std::time::Duration;

/// The keyless default pool (W2a seeds).
pub const DEFAULT_POOL: [&str; 4] = ["arxiv", "crossref", "openaire", "europepmc"];

/// Sources that join the pool only with a key: (source ids tried, key env vars).
pub const KEYED_POOL: [(&[&str], &[&str]); 3] = [
    (&["openalex"], &crate::lookup::OPENALEX_KEY_ENVS),
    (&["semanticscholar", "s2"], &crate::lookup::S2_KEY_ENVS),
    (&["core_ac", "core"], &["SEARCH_MCP_CORE_API_KEY"]),
];

/// Specialists a sub-scope adds (SPEC.md §6).
pub const SUBSCOPE_SOURCES: [(&str, &[&str]); 7] = [
    ("academic.cs", &["dblp", "openreview"]),
    // No biorxiv: its API has no keyword search (lookup / fulltext only);
    // Europe PMC already indexes bioRxiv / medRxiv preprints.
    ("academic.biomed", &["pubmed", "clinicaltrials"]),
    ("academic.trial", &["clinicaltrials", "pubmed"]),
    ("academic.math", &["zbmath"]),
    ("academic.physics", &["inspire"]),
    ("academic.openaccess", &["doaj"]),
    ("dataset", &["datacite", "zenodo"]),
];

/// RRF weight of a sub-scope specialist (the web kernel's native-scope weight).
pub const SUBSCOPE_WEIGHT: f64 = 2.0;

const CORE_IDS: [&str; 2] = ["core_ac", "core"];

/// One source to call.
#[derive(Clone)]
pub(crate) struct PoolMember {
    pub src: Arc<dyn Source>,
    pub weight: f64,
}

impl PoolMember {
    pub(crate) fn id(&self) -> &str {
        &self.src.decl().id
    }
}

fn env_value<'c>(cfg: &'c Config, env: &str) -> Option<&'c str> {
    cfg.api_keys
        .get(env)
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
}

fn has_key(cfg: &Config, envs: &[&str]) -> bool {
    envs.iter().any(|e| env_value(cfg, e).is_some())
}

/// The key a source should be called with: its declared env var(s) (all
/// set, comma-joined for OAuth2 pairs), else the [`KEYED_POOL`] alias for
/// its id (`SEARCH_MCP_S2_API_KEY` for `semanticscholar`).
pub(crate) fn resolve_source_key(cfg: &Config, src: &dyn Source) -> Option<String> {
    let decl = src.decl();
    let envs = decl.key_envs();
    if !envs.is_empty() {
        let vals: Option<Vec<&str>> = envs.iter().map(|e| env_value(cfg, e)).collect();
        if let Some(v) = vals {
            return Some(v.join(","));
        }
    }
    KEYED_POOL
        .iter()
        .find(|(ids, _)| ids.contains(&decl.id.as_str()))
        .and_then(|(_, aliases)| aliases.iter().find_map(|e| env_value(cfg, e)))
        .map(str::to_owned)
}

/// Keyless-capable sources that may only be AUTO-routed with a key
/// (`auth.mode = "optional"` + `route_requires_key`: OpenAlex, S2, CORE,
/// Google Books, ...), and CORE whatever its manifest says.
fn needs_key_to_route(cfg: &Config, src: &dyn Source) -> bool {
    let d = src.decl();
    let declared = matches!(
        d.auth,
        AuthMode::OptionalKey {
            route_requires_key: true,
            ..
        }
    );
    (declared || CORE_IDS.contains(&d.id.as_str())) && resolve_source_key(cfg, src).is_none()
}

/// Resolve a `subscope` ("cs", "academic.cs", "ai", "oa", "data", ...).
pub(crate) fn resolve_subscope(raw: &str) -> Option<&'static ScopeDef> {
    let s = raw.trim().to_ascii_lowercase();
    let ok = |d: &&'static ScopeDef| d.id == "dataset" || scopes::root(d.id) == "academic";
    scopes::resolve(&s)
        .filter(ok)
        .or_else(|| scopes::resolve(&format!("academic.{s}")).filter(ok))
}

/// `None` for no subscope, the scope for a known one, `InvalidInput`
/// (listing the valid ones) otherwise.
pub(crate) fn checked_subscope(raw: Option<&str>) -> Result<Option<&'static ScopeDef>, FsmError> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(r) => resolve_subscope(r).map(Some).ok_or_else(|| {
            FsmError::invalid(format!(
                "unknown subscope '{r}'; use one of: {}",
                valid_subscopes()
            ))
        }),
    }
}

fn valid_subscopes() -> String {
    let mut v: Vec<&str> = scopes::children("academic")
        .map(|d| d.id.trim_start_matches("academic."))
        .collect();
    v.push("data");
    v.join(", ")
}

fn can_search(src: &dyn Source) -> bool {
    let d = src.decl();
    d.ops.is_empty() || d.op("search").is_some()
}

/// Pick the sources for `q`. Returns the members and diagnostics.
pub(crate) fn select(
    reg: &Registry,
    cfg: &Config,
    q: &ScholarQuery,
) -> Result<(Vec<PoolMember>, Vec<String>), FsmError> {
    let mut notes = Vec::new();
    let mut out: Vec<PoolMember> = Vec::new();
    // Returns whether the source was added.
    let push =
        |out: &mut Vec<PoolMember>, notes: &mut Vec<String>, src: Arc<dyn Source>, w: f64| {
            let id = src.decl().id.clone();
            if out.iter().any(|m| m.id() == id) {
                return false;
            }
            match src.availability(cfg) {
                Availability::Available => {}
                Availability::NeedsKey { env } => {
                    notes.push(format!("{id}: skipped, needs {env}"));
                    return false;
                }
                Availability::Disabled(why) => {
                    notes.push(format!("{id}: skipped, disabled ({why})"));
                    return false;
                }
                _ => {
                    notes.push(format!("{id}: skipped, unavailable"));
                    return false;
                }
            }
            if !can_search(src.as_ref()) {
                notes.push(format!("{id}: skipped, it has no search op"));
                return false;
            }
            out.push(PoolMember { src, weight: w });
            true
        };

    if let Some(ids) = q.sources.as_ref().filter(|v| !v.is_empty()) {
        let mut unknown = Vec::new();
        for id in ids {
            let id = id.trim().to_ascii_lowercase();
            match reg.get(&id) {
                Some(src) => {
                    push(&mut out, &mut notes, src, 1.0);
                }
                None => unknown.push(id),
            }
        }
        if !unknown.is_empty() {
            let msg = format!(
                "unknown source(s) {}; find scholarly sources with catalog.search(\"papers\")",
                unknown.join(", ")
            );
            if out.is_empty() {
                return Err(FsmError::invalid(msg));
            }
            notes.push(msg);
        }
        if out.is_empty() {
            return Err(FsmError::invalid(format!(
                "none of the requested sources can search right now ({})",
                notes.join("; ")
            )));
        }
        return Ok((out, notes));
    }

    for id in DEFAULT_POOL {
        match reg.get(id) {
            Some(src) => {
                push(&mut out, &mut notes, src, 1.0);
            }
            None => notes.push(format!("{id}: not registered")),
        }
    }
    for (ids, envs) in KEYED_POOL {
        if has_key(cfg, envs)
            && let Some(src) = ids.iter().find_map(|id| reg.get(id))
        {
            push(&mut out, &mut notes, src, 1.0);
        }
    }
    if let Some(def) = checked_subscope(q.subscope.as_deref())? {
        if let Some((_, ids)) = SUBSCOPE_SOURCES.iter().find(|(s, _)| *s == def.id) {
            for id in *ids {
                if let Some(src) = reg.get(id) {
                    if needs_key_to_route(cfg, src.as_ref()) {
                        notes.push(format!("{id}: skipped, routes only with its key"));
                        continue;
                    }
                    push(&mut out, &mut notes, src, SUBSCOPE_WEIGHT);
                }
            }
        } else if def.id != "academic" {
            // Up to three more sources of that scope that can search and
            // may be auto-routed; only sources actually added count.
            let mut added = 0usize;
            for src in reg.by_scope(def.id) {
                if added >= 3 {
                    break;
                }
                let d = src.decl();
                if d.pool_role == PoolRole::Explicit
                    || needs_key_to_route(cfg, src.as_ref())
                    || !(d.has_cap(Capability::Search) || d.op("search").is_some())
                    || out.iter().any(|m| m.id() == d.id)
                {
                    continue;
                }
                if push(&mut out, &mut notes, src, SUBSCOPE_WEIGHT) {
                    added += 1;
                }
            }
        }
    }
    if out.is_empty() {
        return Err(FsmError::invalid(format!(
            "no scholarly source is available ({})",
            notes.join("; ")
        )));
    }
    Ok((out, notes))
}

/// The parameters a source's `search` op declares, filled from the query.
fn params_for(src: &dyn Source, query: &str, limit: u32) -> Value {
    let mut p = Map::new();
    let op = src.decl().op("search");
    let declares = |name: &str| op.is_none_or(|o| o.params.is_empty() || o.param(name).is_some());
    if declares("query") {
        p.insert("query".into(), Value::from(query));
    }
    if declares("limit") {
        let max = op
            .and_then(|o| o.param("limit"))
            .and_then(|p| p.max)
            .map_or(u32::MAX, |m| m.max(1.0) as u32);
        p.insert("limit".into(), Value::from(limit.min(max).max(1)));
    }
    Value::Object(p)
}

/// The works in a source's output (hits and records are converted).
pub(crate) fn works_of(out: SrcOutput, id: &str) -> Vec<ScholarlyWork> {
    let tag = |mut w: ScholarlyWork| {
        w = crate::merge::prepare(w, Some(id));
        (!w.title.is_empty()).then_some(w)
    };
    match out {
        SrcOutput::Works(ws) => ws.into_iter().filter_map(tag).collect(),
        SrcOutput::Records { items, hits, .. } => {
            let from_items: Vec<ScholarlyWork> = items
                .into_iter()
                .filter_map(|v| serde_json::from_value::<ScholarlyWork>(v).ok())
                .filter_map(tag)
                .collect();
            if !from_items.is_empty() {
                return from_items;
            }
            hits.unwrap_or_default()
                .into_iter()
                .filter_map(|h| tag(hit_work(h)))
                .collect()
        }
        SrcOutput::Hits(hits) => hits.into_iter().filter_map(|h| tag(hit_work(h))).collect(),
        _ => Vec::new(),
    }
}

fn hit_work(h: fsm_core::Hit) -> ScholarlyWork {
    let mut w = ScholarlyWork::new(h.title, h.url);
    w.published_date = h.date;
    w.work_type = "article".into();
    if !h.snippet.trim().is_empty() {
        w.abstract_text = Some(h.snippet);
    }
    w
}

/// What the fan-out produced.
pub(crate) struct FanOut {
    pub lists: Vec<SourceList>,
    pub notes: Vec<String>,
    /// Sources that answered HTTP 429: `(id, upstream_certain)`.
    /// `ManifestSource` maps HTTP 429 to `FsmError::RateLimited`, while
    /// `HttpClient` uses `RateLimited` with `retry_after_s: Some(..)` for a
    /// local GCRA wait exceeding `timeout`. When `retry_after_s` is `None` or
    /// `timeout` comfortably exceeds the bucket's token interval (so a single
    /// GCRA wait could not exceed it), the 429 is treated as upstream.
    pub rate_limited: Vec<(String, bool)>,
    /// Every member answered (no error, no timeout, nobody cut off).
    pub complete: bool,
}

/// One source's answer.
pub(crate) struct SourceList {
    pub id: String,
    pub weight: f64,
    pub works: Vec<ScholarlyWork>,
}

/// Fan-out timing.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Deadlines {
    pub soft: Duration,
    pub hard: Duration,
}

/// Call every member concurrently under the deadlines.
pub(crate) async fn fan_out(
    members: &[PoolMember],
    query: &str,
    per_source: u32,
    cfg: &Config,
    cx: &CallCx,
    d: Deadlines,
) -> Result<FanOut, FsmError> {
    let start = tokio::time::Instant::now();
    let hard = d.hard.min(cx.remaining());
    let soft_at = start + d.soft.min(hard);
    let hard_at = start + hard;
    let cancel = cx.cancel.child_token();
    let mut pending: Vec<String> = members.iter().map(|m| m.id().to_owned()).collect();
    let mut futs = FuturesUnordered::new();
    for m in members {
        let decl = m.src.decl();
        let per_src = Duration::from_millis(decl.limits.timeout_ms)
            .min(hard)
            .max(Duration::from_millis(1));
        let timeout = if decl.limits.timeout_ms == 0 {
            hard
        } else {
            per_src
        };
        let mut scx = SrcCx::from_config(cfg, cancel.child_token(), timeout);
        scx.api_key = resolve_source_key(cfg, m.src.as_ref());
        let params = params_for(m.src.as_ref(), query, per_source);
        let src = Arc::clone(&m.src);
        let (id, weight, rps) = (decl.id.clone(), m.weight, decl.limits.rps);
        futs.push(async move {
            let r = tokio::time::timeout(timeout, src.call("search", params, &scx)).await;
            (id, weight, timeout, rps, r)
        });
    }
    let mut lists: Vec<SourceList> = Vec::new();
    let mut notes = Vec::new();
    let mut rate_limited = Vec::new();
    let mut failed = false;
    let mut soft_passed = false;
    let any_results = |lists: &Vec<SourceList>| lists.iter().any(|l| !l.works.is_empty());
    loop {
        tokio::select! {
            biased;
            () = cx.cancel.cancelled() => {
                cancel.cancel();
                return Err(FsmError::Timeout(0));
            }
            next = futs.next() => {
                let Some((id, weight, timeout, rps, r)) = next else { break };
                pending.retain(|p| *p != id);
                match r {
                    Ok(Ok(out)) => {
                        let works = works_of(out, &id);
                        lists.push(SourceList { id, weight, works });
                    }
                    Ok(Err(e)) => {
                        failed = true;
                        match &e {
                            FsmError::RateLimited { retry_after_s, .. } => {
                                let upstream = retry_after_s.is_none()
                                    || (rps > 0.0 && timeout.as_secs_f64() > 2.0 / rps);
                                rate_limited.push((id.clone(), upstream));
                            }
                            FsmError::Upstream { status: Some(429), .. } => {
                                rate_limited.push((id.clone(), true));
                            }
                            _ => {}
                        }
                        notes.push(format!("{id}: {e}"));
                    }
                    Err(_) => {
                        failed = true;
                        notes.push(format!("{id}: timed out after {} ms", timeout.as_millis()));
                    }
                }
                if soft_passed && any_results(&lists) {
                    break;
                }
            }
            () = tokio::time::sleep_until(soft_at), if !soft_passed => {
                soft_passed = true;
                if any_results(&lists) {
                    break;
                }
            }
            () = tokio::time::sleep_until(hard_at) => break,
        }
    }
    cancel.cancel();
    if !pending.is_empty() {
        let waited = start.elapsed().as_millis();
        notes.push(format!(
            "{}: no answer within {waited} ms; skipped",
            pending.join(", ")
        ));
    }
    let complete = !failed && pending.is_empty();
    Ok(FanOut {
        lists,
        notes,
        rate_limited,
        complete,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscopes_resolve_by_short_name_and_synonym() {
        for (raw, id) in [
            ("cs", "academic.cs"),
            ("academic.cs", "academic.cs"),
            ("AI", "academic.cs"),
            ("biomed", "academic.biomed"),
            ("trial", "academic.trial"),
            ("oa", "academic.openaccess"),
            ("data", "dataset"),
            ("physics", "academic.physics"),
            ("academic", "academic"),
        ] {
            assert_eq!(resolve_subscope(raw).map(|d| d.id), Some(id), "{raw}");
        }
        assert!(resolve_subscope("law").is_none());
        assert!(resolve_subscope("nonsense").is_none());
        assert!(valid_subscopes().contains("cs"));
    }
}
