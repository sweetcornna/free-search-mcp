//! Fast-mode `ask` pipeline (no LLM, always available, ruling R5 Deliverable 1).
//!
//! 1. Classify the question and check direct-answer claimants (`Source::claims >= 0.8`, R1).
//!    If a direct claimant answers with structured records/hits, render directly.
//! 2. Otherwise run web search (~8 results, short deadline) and read the top 3 unique
//!    pages in parallel through the existing `FetchApi` pipeline & cache.
//! 3. Select the best passages answering the question via CJK-aware sentence-window
//!    ranking (`bm25` + `rapidfuzz`).
//! 4. Bound the entire call by a hard deadline (~8 s by default) returning partial
//!    results cleanly on timeout.

use crate::passage::{
    CandidateDoc, PassageTokenizer, is_near_duplicate, select_passages,
    unescape_markdown_punctuation,
};
use fsm_core::ext::w7::is_builtin_agent_engine;
use fsm_core::url_canon::{host_of, normalize_url};
use fsm_core::{
    AskOpts, AskPassage, AskResponse, AskSource, Availability, CallCx, Config, FetchApi, FetchOpts,
    FetchOutcome, FsmError, Hit, PageResult, Registry, SearchApi, SearchRequest, SearchResponse,
    Source, SrcCx, SrcOutput,
};
use fsm_extract::research::compute_date_note;
use futures::stream::{FuturesUnordered, StreamExt};
use jiff::Timestamp;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Default hard deadline for fast `ask` (~8 s per ruling R5).
pub const DEFAULT_FAST_DEADLINE: Duration = Duration::from_secs(8);
/// Direct claimant score threshold (RULINGS R1).
pub const DIRECT_CLAIM_THRESHOLD: f32 = 0.8;
/// Target web search result count for fast mode.
pub const FAST_SEARCH_LIMIT: u32 = 8;
/// Number of top search result URLs read in parallel.
pub const FAST_READ_TOP_N: usize = 3;
/// Maximum passages returned in the compact answer block.
pub const FAST_MAX_PASSAGES: usize = 5;

/// Knobs for testing and tuning the fast pipeline.
#[derive(Debug, Clone)]
pub struct FastConfig {
    pub hard_deadline: Duration,
    pub direct_timeout: Duration,
    pub search_timeout: Duration,
    pub read_top_n: usize,
    pub max_passages: usize,
}

impl Default for FastConfig {
    fn default() -> Self {
        Self {
            hard_deadline: DEFAULT_FAST_DEADLINE,
            // Allow 2-hop direct claimants (e.g. openmeteo geocode + forecast ~3.6s)
            // and cold-TLS exchange-rate lookups to succeed in direct mode.
            direct_timeout: Duration::from_millis(4500),
            // Exceed search's 4.0s soft deadline so search always returns fused hits
            // while still leaving >=3.5s for parallel page reading inside the 8s budget.
            search_timeout: Duration::from_millis(4500),
            read_top_n: FAST_READ_TOP_N,
            max_passages: FAST_MAX_PASSAGES,
        }
    }
}

/// Format current UTC wall-clock timestamp in RFC 3339 (`2026-11-25T14:00:00Z`).
pub fn now_utc_rfc3339() -> String {
    let ts = Timestamp::now();
    // Truncate subsecond nanoseconds to clean whole seconds.
    Timestamp::from_second(ts.as_second())
        .map(|t| t.to_string())
        .unwrap_or_else(|_| ts.to_string())
}

/// Evidence bundle gathered by the fast pipeline (reused by agent mode too).
pub struct FastEvidence {
    pub response: AskResponse,
    pub search: Option<SearchResponse>,
    pub pages: Vec<PageResult>,
}

/// Execute fast-mode `ask` (no LLM).
#[allow(clippy::too_many_arguments)]
pub async fn run_fast_ask(
    question: &str,
    opts: &AskOpts,
    search: &Arc<dyn SearchApi>,
    fetch: &Arc<dyn FetchApi>,
    registry: &Arc<Registry>,
    cfg: &Arc<Config>,
    fast_cfg: &FastConfig,
    cx: &CallCx,
) -> Result<FastEvidence, FsmError> {
    let q = question.trim();
    if q.is_empty() {
        return Err(FsmError::invalid("question must not be empty"));
    }

    let started = Instant::now();
    let total_budget = fast_cfg
        .hard_deadline
        .min(cx.remaining().max(Duration::from_millis(50)));
    let deadline = started + total_budget;

    // 1. Direct-answer claimant shortcut (RULINGS R1: score >= 0.8), unless caller constrained domains.
    if opts.domains.is_empty()
        && let Some(direct) = try_direct_claimants(
            q,
            opts.lang.as_deref(),
            registry,
            cfg,
            fast_cfg,
            deadline,
            cx,
        )
        .await
    {
        let mut resp = direct;
        resp.elapsed_ms = started.elapsed().as_millis() as u64;
        return Ok(FastEvidence {
            response: resp,
            search: None,
            pages: Vec::new(),
        });
    }

    // 2. Web search (~8 results, short deadline).
    let remaining_for_search = deadline
        .saturating_duration_since(Instant::now())
        .min(fast_cfg.search_timeout);

    if remaining_for_search.is_zero() || cx.cancel.is_cancelled() {
        let mut resp = AskResponse::new(q, "fast");
        resp.retrieved_at = now_utc_rfc3339();
        resp.elapsed_ms = started.elapsed().as_millis() as u64;
        resp.partial = true;
        resp.notes
            .push("Deadline exceeded before search could complete.".to_owned());
        return Ok(FastEvidence {
            response: resp,
            search: None,
            pages: Vec::new(),
        });
    }

    let mut req = SearchRequest::new(q);
    req.scope = "web".to_owned();
    req.limit = FAST_SEARCH_LIMIT;
    req.freshness = opts.freshness;
    req.lang = opts.lang.clone();
    req.domains = (!opts.domains.is_empty()).then(|| opts.domains.clone());

    let search_cx = sub_call_cx(cx, Instant::now() + remaining_for_search);
    // Give the outer timeout a small slack over `search_cx`'s deadline so the search
    // kernel can finish fusing its partial results cleanly instead of being dropped
    // on the exact same millisecond tick.
    let outer_search_timeout = (remaining_for_search + Duration::from_millis(250))
        .min(deadline.saturating_duration_since(Instant::now()));
    let search_res =
        tokio::time::timeout(outer_search_timeout, search.search(req, &search_cx)).await;

    let mut notes = Vec::new();
    let mut partial = false;
    let search_resp = match search_res {
        Ok(Ok(sr)) => sr,
        Ok(Err(err)) => {
            if matches!(err, FsmError::Timeout(_)) {
                notes.push("Web search timed out; no results arrived in time.".to_owned());
                let mut resp = AskResponse::new(q, "fast");
                resp.retrieved_at = now_utc_rfc3339();
                resp.elapsed_ms = started.elapsed().as_millis() as u64;
                resp.partial = true;
                resp.notes = notes;
                return Ok(FastEvidence {
                    response: resp,
                    search: None,
                    pages: Vec::new(),
                });
            }
            return Err(err);
        }
        Err(_) => {
            let mut resp = AskResponse::new(q, "fast");
            resp.retrieved_at = now_utc_rfc3339();
            resp.elapsed_ms = started.elapsed().as_millis() as u64;
            resp.partial = true;
            resp.notes
                .push("Web search exceeded the fast ask deadline.".to_owned());
            return Ok(FastEvidence {
                response: resp,
                search: None,
                pages: Vec::new(),
            });
        }
    };

    // 3. Pick top N unique HTTP(S) URLs (canonicalized) and fetch them in parallel.
    //    If an authoritative/reference page sits just below the top N (within top 6),
    //    swap it in for the tail slot so official docs/references get full-page reading too.
    let mut targets: Vec<(usize, Hit)> = Vec::new();
    let mut seen_urls = HashSet::new();
    for (idx, hit) in search_resp.results.iter().enumerate() {
        let u = hit.url.trim();
        let canon = normalize_url(u);
        let key = if canon.is_empty() {
            u.to_owned()
        } else {
            canon
        };
        if (u.starts_with("http://") || u.starts_with("https://")) && seen_urls.insert(key) {
            targets.push((idx, hit.clone()));
            if targets.len() >= fast_cfg.read_top_n {
                break;
            }
        }
    }
    if targets.len() >= 2 && fast_cfg.read_top_n >= 2 {
        let is_auth_host = |url: &str| {
            let h = host_of(url);
            h.ends_with(".org")
                || h.ends_with(".gov")
                || h.ends_with(".edu")
                || h.starts_with("docs.")
                || h == "github.com"
        };
        if !targets.iter().any(|(_, h)| is_auth_host(&h.url)) {
            for (idx, hit) in search_resp.results.iter().enumerate().take(6) {
                let u = hit.url.trim();
                let canon = normalize_url(u);
                let key = if canon.is_empty() {
                    u.to_owned()
                } else {
                    canon
                };
                if (u.starts_with("http://") || u.starts_with("https://"))
                    && is_auth_host(u)
                    && !seen_urls.contains(&key)
                {
                    targets.pop();
                    targets.push((idx, hit.clone()));
                    break;
                }
            }
        }
    }

    // Reserve a tiny margin (150ms) for sentence-window ranking before the hard deadline.
    let now = Instant::now();
    let fetch_budget = deadline
        .saturating_duration_since(now)
        .saturating_sub(Duration::from_millis(150));

    let mut fetched_slots: Vec<Option<PageResult>> = vec![None; targets.len()];
    let mut timed_out_fetches = 0usize;

    if !targets.is_empty() && !fetch_budget.is_zero() && !cx.cancel.is_cancelled() {
        let fetch_deadline = Instant::now() + fetch_budget;
        let mut futs = FuturesUnordered::new();
        for (slot_idx, (_, hit)) in targets.iter().enumerate() {
            let fetch = Arc::clone(fetch);
            let url = hit.url.clone();
            let fallback_title = hit.title.clone();
            let sub_cx = sub_call_cx(cx, fetch_deadline);
            futs.push(async move {
                let res = fetch.fetch(&url, FetchOpts::default(), &sub_cx).await;
                let page = match res {
                    Ok(FetchOutcome::Page(mut p)) => {
                        if p.title.trim().is_empty() {
                            p.title = fallback_title;
                        }
                        Some(p)
                    }
                    _ => None,
                };
                (slot_idx, page)
            });
        }

        let mut finished_count = 0usize;
        let total_targets = targets.len();
        loop {
            let rem = fetch_deadline.saturating_duration_since(Instant::now());
            if rem.is_zero() || finished_count >= total_targets {
                break;
            }
            match tokio::time::timeout(rem, futs.next()).await {
                Ok(Some((slot_idx, maybe_page))) => {
                    fetched_slots[slot_idx] = maybe_page;
                    finished_count += 1;
                }
                Ok(None) => break,
                Err(_) => {
                    timed_out_fetches = total_targets.saturating_sub(finished_count);
                    break;
                }
            }
        }
    } else if !targets.is_empty() {
        timed_out_fetches = targets.len();
    }

    if timed_out_fetches > 0 {
        partial = true;
        notes.push(format!(
            "Timed out reading {timed_out_fetches} of {} pages within the {:.1}s budget; returning best passages from completed pages and search snippets.",
            targets.len(),
            total_budget.as_secs_f64()
        ));
    }

    // 4. Build candidate documents from fetched pages + search snippets for CJK-aware passage ranking.
    let mut candidates: Vec<CandidateDoc> = Vec::new();
    let mut fetched_urls = HashSet::new();
    let mut pages_vec: Vec<PageResult> = Vec::new();
    let mut cache_ages: Vec<u64> = Vec::new();

    if let Some(age) = search_resp.cache_age_s {
        cache_ages.push(age);
    }

    for ((rank, hit), maybe_page) in targets.iter().zip(fetched_slots.iter()) {
        if let Some(page) = maybe_page {
            if let Some(age) = page.cache_age_s {
                cache_ages.push(age);
            }
            fetched_urls.insert(canon_or_raw(&hit.url));
            fetched_urls.insert(canon_or_raw(&page.url));
            pages_vec.push(page.clone());
            let date = page
                .published_date
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| hit.date.as_deref().filter(|s| !s.trim().is_empty()))
                .map(str::to_owned);
            candidates.push(CandidateDoc {
                title: if page.title.trim().is_empty() {
                    hit.title.clone()
                } else {
                    page.title.clone()
                },
                url: page.url.clone(),
                date,
                source: (!hit.source.is_empty()).then(|| hit.source.clone()),
                cache_age_s: page.cache_age_s,
                rank: *rank,
                is_full_page: true,
                text: page.content.clone(),
            });
            // Also include the search snippet as a lightweight candidate in case boilerplate hid a key phrase.
            if !hit.snippet.trim().is_empty() {
                candidates.push(CandidateDoc {
                    title: hit.title.clone(),
                    url: hit.url.clone(),
                    date: hit.date.clone(),
                    source: (!hit.source.is_empty()).then(|| hit.source.clone()),
                    cache_age_s: search_resp.cache_age_s,
                    rank: *rank,
                    is_full_page: false,
                    text: hit.snippet.clone(),
                });
            }
        }
    }

    // Add search snippets from unread / remaining search hits so timeouts or fetch failures still yield answers.
    for (rank, hit) in search_resp.results.iter().enumerate() {
        if fetched_urls.contains(&canon_or_raw(&hit.url)) || hit.snippet.trim().is_empty() {
            continue;
        }
        candidates.push(CandidateDoc {
            title: hit.title.clone(),
            url: hit.url.clone(),
            date: hit.date.clone(),
            source: (!hit.source.is_empty()).then(|| hit.source.clone()),
            cache_age_s: search_resp.cache_age_s,
            rank,
            is_full_page: false,
            text: hit.snippet.clone(),
        });
    }

    let passages = select_passages(q, &candidates, fast_cfg.max_passages);

    // Collect deduplicated cited sources (passages first, then top hits).
    let mut sources: Vec<AskSource> = Vec::new();
    let mut seen_src = HashSet::new();
    for p in &passages {
        if !p.url.is_empty() && seen_src.insert(canon_or_raw(&p.url)) {
            let mut s = AskSource::new(&p.title, &p.url);
            s.date = p.date.clone();
            s.source = p.source.clone();
            s.cache_age_s = p.cache_age_s;
            sources.push(s);
        }
    }
    if sources.is_empty() {
        for hit in search_resp.results.iter().take(fast_cfg.read_top_n) {
            if !hit.url.is_empty() && seen_src.insert(canon_or_raw(&hit.url)) {
                let mut s = AskSource::new(&hit.title, &hit.url);
                s.date = hit.date.clone();
                s.source = (!hit.source.is_empty()).then(|| hit.source.clone());
                s.cache_age_s = search_resp.cache_age_s;
                sources.push(s);
            }
        }
    }

    let top_hits_for_dates: Vec<Hit> = targets.iter().map(|(_, h)| h.clone()).collect();
    let slot_refs: Vec<Option<&PageResult>> = fetched_slots.iter().map(Option::as_ref).collect();
    let date_note = compute_date_note(&top_hits_for_dates, &slot_refs);

    let cache_age_s = cache_ages.into_iter().max();

    let mut resp = AskResponse::new(q, "fast");
    resp.passages = passages;
    resp.sources = sources;
    resp.retrieved_at = now_utc_rfc3339();
    resp.cache_age_s = cache_age_s;
    resp.elapsed_ms = started.elapsed().as_millis() as u64;
    resp.partial = partial;
    resp.date_note = date_note;
    resp.notes = notes;

    Ok(FastEvidence {
        response: resp,
        search: Some(search_resp),
        pages: pages_vec,
    })
}

/// Attempt to answer directly from high-confidence (`claims() >= 0.8`) sources (RULINGS R1).
async fn try_direct_claimants(
    question: &str,
    lang: Option<&str>,
    registry: &Arc<Registry>,
    cfg: &Arc<Config>,
    fast_cfg: &FastConfig,
    deadline: Instant,
    cx: &CallCx,
) -> Option<AskResponse> {
    if !cfg.auto_route_enabled {
        return None;
    }

    let mut direct: Vec<(Arc<dyn Source>, f32)> = Vec::new();
    for src in registry.all() {
        let id = src.decl().id.as_str();
        if is_builtin_agent_engine(id) {
            continue;
        }
        let avail = catch_unwind(AssertUnwindSafe(|| src.availability(cfg)))
            .ok()
            .unwrap_or_else(|| Availability::Disabled("panicked".to_owned()));
        if avail != Availability::Available {
            continue;
        }
        let score = catch_unwind(AssertUnwindSafe(|| src.claims(question, lang)))
            .ok()
            .unwrap_or(0.0);
        if score.is_finite() && score >= DIRECT_CLAIM_THRESHOLD {
            direct.push((Arc::clone(src), score));
        }
    }

    if direct.is_empty() {
        return None;
    }
    direct.sort_by(|a, b| {
        b.1.total_cmp(&a.1)
            .then_with(|| a.0.decl().id.cmp(&b.0.decl().id))
    });
    direct.truncate(2);

    // When `pypi` claimed an ambiguous bare package version question (no explicit
    // Python/PyPI cue, e.g. "What is the latest version of tokio?"), also query
    // sibling registries (`crates`, `npm`) in parallel so a stale PyPI package
    // cannot shadow an actively maintained crate or npm package.
    let expanded_ambiguous_pkg =
        direct.iter().any(|(s, _)| s.decl().id == "pypi") && !has_explicit_python_hint(question);
    if expanded_ambiguous_pkg {
        for sibling_id in ["crates", "npm"] {
            if direct.iter().any(|(s, _)| s.decl().id == sibling_id) {
                continue;
            }
            if let Some(src) = registry.get(sibling_id) {
                let avail = catch_unwind(AssertUnwindSafe(|| src.availability(cfg)))
                    .ok()
                    .unwrap_or_else(|| Availability::Disabled("panicked".to_owned()));
                if avail == Availability::Available {
                    direct.push((src, DIRECT_CLAIM_THRESHOLD));
                }
            }
        }
    }

    let timeout = deadline
        .saturating_duration_since(Instant::now())
        .min(fast_cfg.direct_timeout);
    if timeout.is_zero() {
        return None;
    }

    let mut futs = FuturesUnordered::new();
    for (src, score) in direct {
        let q = question.to_owned();
        let cfg = Arc::clone(cfg);
        let cancel = cx.cancel.clone();
        futs.push(async move {
            let id = src.decl().id.clone();
            let op = src
                .decl()
                .ops
                .iter()
                .find(|o| o.name == "search")
                .or_else(|| src.decl().ops.first())
                .map(|o| o.name.clone())
                .unwrap_or_else(|| "search".to_owned());
            let mut scx = SrcCx::from_config(&cfg, cancel, timeout);
            scx.lang = None;
            let params = json!({"query": q, "limit": 5});
            let res = tokio::time::timeout(timeout, src.call(&op, params, &scx)).await;
            (id, score, res)
        });
    }

    let mut hits: Vec<(f32, Hit)> = Vec::new();
    while let Some((src_id, score, res)) = futs.next().await {
        let Ok(Ok(out)) = res else { continue };
        match out {
            SrcOutput::Hits(list) => {
                for mut h in list.into_iter().take(3) {
                    if h.source.is_empty() {
                        h.source = src_id.clone();
                    }
                    hits.push((score, h));
                }
            }
            SrcOutput::Records {
                hits: Some(list), ..
            } if !list.is_empty() => {
                for mut h in list.into_iter().take(3) {
                    if h.source.is_empty() {
                        h.source = src_id.clone();
                    }
                    hits.push((score, h));
                }
            }
            SrcOutput::Records { items, .. } => {
                for item in items.into_iter().take(3) {
                    if let Some(h) = record_to_hit(&src_id, &item) {
                        hits.push((score, h));
                    }
                }
            }
            _ => {}
        }
    }

    if hits.is_empty() {
        return None;
    }

    // Filter direct-claimant results by entity match so exact registry hits (e.g. `tokio`)
    // answer cleanly without unrelated fuzzy search-tail results (e.g. `asupersync`).
    let entity_terms = direct_entity_terms(question);
    let mut scored_hits: Vec<(u8, f32, usize, Hit)> = hits
        .into_iter()
        .enumerate()
        .map(|(orig_idx, (score, hit))| {
            let tier = direct_entity_match_tier(&entity_terms, &hit);
            (tier, score, orig_idx, hit)
        })
        .collect();
    let best_tier = scored_hits.iter().map(|x| x.0).max().unwrap_or(0);
    if best_tier > 0 {
        scored_hits.retain(|x| x.0 == best_tier);
    }
    if expanded_ambiguous_pkg {
        scored_hits.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| {
                    b.3.date
                        .as_deref()
                        .unwrap_or("")
                        .cmp(a.3.date.as_deref().unwrap_or(""))
                })
                .then_with(|| b.1.total_cmp(&a.1))
                .then_with(|| a.2.cmp(&b.2))
        });
        if best_tier >= 2
            && let Some(newest_year) = scored_hits
                .iter()
                .filter_map(|x| parse_iso_year(x.3.date.as_deref()))
                .max()
        {
            scored_hits.retain(|x| match parse_iso_year(x.3.date.as_deref()) {
                Some(y) => newest_year.saturating_sub(y) <= 2,
                None => false,
            });
        }
    } else {
        scored_hits.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| b.1.total_cmp(&a.1))
                .then_with(|| a.2.cmp(&b.2))
        });
    }

    let tok = PassageTokenizer::new();
    let mut passages = Vec::new();
    let mut sources = Vec::new();
    let mut seen_urls = HashSet::new();
    let mut seen_texts: Vec<(String, Vec<char>, HashSet<String>)> = Vec::new();

    for (_, score, _, hit) in scored_hits {
        if passages.len() >= 3 {
            break;
        }
        let raw_body = if !hit.snippet.trim().is_empty() {
            hit.snippet.trim()
        } else {
            hit.title.trim()
        };
        let body = unescape_markdown_punctuation(raw_body);
        if body.is_empty() {
            continue;
        }
        let url_key = canon_or_raw(&hit.url);
        if !url_key.is_empty() && !seen_urls.insert(url_key) {
            continue;
        }
        let lower_body = body.to_lowercase();
        let lower_chars: Vec<char> = lower_body.chars().collect();
        let body_toks: HashSet<String> = tok.tokenize_terms(&body, true).into_iter().collect();
        if seen_texts.iter().any(|(pt, pc, pk)| {
            is_near_duplicate(pt, pc, pk, &lower_body, &lower_chars, &body_toks)
        }) {
            continue;
        }
        seen_texts.push((lower_body, lower_chars, body_toks));

        let mut p = AskPassage::new(body, &hit.title, &hit.url);
        p.date = hit.date.clone();
        p.source = (!hit.source.is_empty()).then(|| hit.source.clone());
        p.score = f64::from(score);
        passages.push(p);

        if !hit.url.is_empty() {
            let mut s = AskSource::new(&hit.title, &hit.url);
            s.date = hit.date.clone();
            s.source = (!hit.source.is_empty()).then(|| hit.source.clone());
            sources.push(s);
        }
    }

    if passages.is_empty() {
        return None;
    }

    let mut resp = AskResponse::new(question, "direct");
    resp.passages = passages;
    resp.sources = sources;
    resp.retrieved_at = now_utc_rfc3339();
    Some(resp)
}

fn has_explicit_python_hint(question: &str) -> bool {
    let lower = question.to_ascii_lowercase();
    if lower.contains("python 包") || lower.contains("python 库") {
        return true;
    }
    lower
        .split(|c: char| !c.is_alphanumeric() && c != '.')
        .any(|tok| matches!(tok, "pypi" | "pypi.org" | "pip" | "python" | "py"))
}

fn parse_iso_year(date: Option<&str>) -> Option<u32> {
    let d = date?.trim();
    if d.len() < 4 || !d.as_bytes()[..4].iter().all(u8::is_ascii_digit) {
        return None;
    }
    d[..4].parse().ok()
}

fn canon_or_raw(url: &str) -> String {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let norm = normalize_url(trimmed);
    if norm.is_empty() {
        trimmed.to_owned()
    } else {
        norm
    }
}

const DIRECT_STOP: &[&str] = &[
    "a",
    "an",
    "the",
    "of",
    "for",
    "in",
    "on",
    "to",
    "is",
    "are",
    "was",
    "be",
    "what",
    "which",
    "when",
    "who",
    "how",
    "latest",
    "newest",
    "current",
    "stable",
    "new",
    "recent",
    "last",
    "next",
    "version",
    "versions",
    "release",
    "released",
    "releases",
    "update",
    "updated",
    "upgrade",
    "changelog",
    "history",
    "date",
    "package",
    "library",
    "lib",
    "module",
    "framework",
    "tool",
    "cli",
    "sdk",
    "api",
    "app",
    "plugin",
    "python",
    "pip",
    "pypi",
    "node",
    "npm",
    "js",
    "javascript",
    "typescript",
    "rust",
    "cargo",
    "crate",
    "crates",
    "go",
    "golang",
    "java",
    "maven",
    "gem",
    "ruby",
    "php",
    "composer",
    "dotnet",
    "nuget",
    "eol",
    "end",
    "life",
    "support",
    "supported",
    "lifecycle",
    "security",
    "vulnerability",
    "vulnerabilities",
    "cve",
    "install",
    "download",
    "still",
    "yet",
    "does",
    "do",
    "did",
    "has",
    "have",
    "with",
    "and",
    "or",
    "not",
    "any",
    "there",
    "it",
    "its",
    "this",
    "that",
    "please",
    "tell",
    "me",
    "find",
    "check",
    "look",
    "up",
    "crates.io",
    "pypi.org",
    "npmjs.com",
];

fn direct_entity_terms(question: &str) -> Vec<String> {
    let mut terms = Vec::new();
    for raw in
        question.split(|c: char| !c.is_alphanumeric() && !matches!(c, '-' | '_' | '@' | '/' | '.'))
    {
        let tok = raw
            .trim_matches(|c: char| matches!(c, '.' | '-' | '_'))
            .to_ascii_lowercase();
        if tok.is_empty()
            || DIRECT_STOP.contains(&tok.as_str())
            || tok.chars().all(|c| c.is_ascii_digit() || c == '.')
        {
            continue;
        }
        if !terms.contains(&tok) {
            terms.push(tok);
        }
    }
    terms
}

/// Tier 2 = exact entity match on title head or URL path segment;
/// Tier 1 = entity token appears inside title or URL;
/// Tier 0 = unrelated search-tail hit.
fn direct_entity_match_tier(entity_terms: &[String], hit: &Hit) -> u8 {
    if entity_terms.is_empty() {
        return 1;
    }
    let title_lower = hit.title.trim().to_ascii_lowercase();
    let title_words: Vec<&str> = title_lower
        .split(|c: char| !c.is_alphanumeric() && !matches!(c, '-' | '_' | '@' | '/'))
        .filter(|w| !w.is_empty())
        .collect();
    let url_segments: Vec<String> = url::Url::parse(&hit.url)
        .ok()
        .and_then(|u| {
            u.path_segments()
                .map(|segs| segs.map(|s| s.to_ascii_lowercase()).collect())
        })
        .unwrap_or_default();

    for term in entity_terms {
        if title_words.first().is_some_and(|w| *w == term.as_str())
            || url_segments.iter().any(|seg| seg == term)
        {
            return 2;
        }
    }
    for term in entity_terms {
        if title_words
            .iter()
            .any(|w| *w == term.as_str() || w.contains(term.as_str()))
            || url_segments.iter().any(|seg| seg.contains(term.as_str()))
        {
            return 1;
        }
    }
    0
}

fn record_to_hit(source_id: &str, item: &Value) -> Option<Hit> {
    let obj = item.as_object()?;
    let title = obj
        .get("title")
        .or_else(|| obj.get("name"))
        .or_else(|| obj.get("id"))
        .and_then(Value::as_str)
        .unwrap_or(source_id)
        .trim();
    let url = obj
        .get("url")
        .or_else(|| obj.get("homepage"))
        .or_else(|| obj.get("link"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let snippet = obj
        .get("snippet")
        .or_else(|| obj.get("summary"))
        .or_else(|| obj.get("description"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| compact_record_fields(obj));
    if snippet.is_empty() {
        return None;
    }
    let date = obj
        .get("date")
        .or_else(|| obj.get("updated_at"))
        .or_else(|| obj.get("published_at"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let mut h = Hit::new(title, url);
    h.snippet = snippet;
    h.date = date;
    h.source = source_id.to_owned();
    h.sources = vec![source_id.to_owned()];
    Some(h)
}

fn compact_record_fields(obj: &serde_json::Map<String, Value>) -> String {
    let mut parts = Vec::new();
    for (k, v) in obj {
        if matches!(k.as_str(), "url" | "link" | "raw") {
            continue;
        }
        let val_s = match v {
            Value::String(s) if !s.trim().is_empty() => s.trim().to_owned(),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            _ => continue,
        };
        parts.push(format!("{k}: {val_s}"));
        if parts.len() >= 8 {
            break;
        }
    }
    parts.join(" · ")
}

fn sub_call_cx(parent: &CallCx, deadline: Instant) -> CallCx {
    let mut sub = CallCx::new(parent.session_id.clone(), deadline.min(parent.deadline));
    sub.nonce = parent.nonce.clone();
    sub.cancel = parent.cancel.clone();
    sub.host_calls = Arc::clone(&parent.host_calls);
    sub.max_host_calls = parent.max_host_calls;
    sub.downloads_used = Arc::clone(&parent.downloads_used);
    sub.local_doc_tainted = Arc::clone(&parent.local_doc_tainted);
    sub
}
