//! Composed `web.research` workflow (port of `src/search_mcp/research.py`).
//!
//! Searches via `Arc<dyn SearchApi>`, fetches the top `depth` (`1..=8`, default `3`)
//! pages concurrently within `cx.remaining()`, computes the `date_note` (flagging undated
//! sources and ISO date spans > 365 days), and returns a `ResearchBrief`.

use crate::fetch::{MAX_BATCH_CONCURRENCY, fetch_page, outcome_to_page};
use crate::gnews::GnewsResolver;
use crate::request::HostRateLimiter;
use fsm_core::text::estimate_tokens;
use fsm_core::{
    CallCx, DocApi, FetchOpts, FsmError, Hit, PageResult, ResearchBrief, ResearchOpts, SearchApi,
    SearchRequest, SourceDeps,
};
use futures::future::join_all;
use jiff::civil::Date;
use regex::Regex;
use std::sync::{Arc, LazyLock};
use tokio::sync::Semaphore;

pub const MIN_DEPTH: u8 = 1;
pub const MAX_DEPTH: u8 = 8;
pub const DATE_SPREAD_WARN_DAYS: i32 = 365;

static ISO_DATE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|[^0-9])(\d{4})-(\d{2})-(\d{2})(?:$|[^0-9])").expect("iso date regex")
});

/// Build the human/LLM date provenance warning for a research brief.
///
/// Mirrors Python `research._date_note`:
/// - A fetched page's own `published_date` outranks the search hit's `date`.
/// - Reports count of undated sources when any lack a date.
/// - Reports a conflict-resolution warning whenever ISO dates across sources
///   span strictly more than 365 days.
pub fn compute_date_note(hits: &[Hit], pages_by_slot: &[Option<&PageResult>]) -> Option<String> {
    if hits.is_empty() {
        return None;
    }
    let mut stamps: Vec<Date> = Vec::new();
    let mut undated = 0usize;

    for (idx, hit) in hits.iter().enumerate() {
        let page_date = pages_by_slot
            .get(idx)
            .and_then(|opt| *opt)
            .and_then(|p| p.published_date.as_deref())
            .filter(|s| !s.trim().is_empty());
        let hit_date = hit.date.as_deref().filter(|s| !s.trim().is_empty());
        let value = page_date.or(hit_date);
        let Some(val) = value else {
            undated += 1;
            continue;
        };
        if let Some(c) = ISO_DATE_RE.captures(val)
            && let (Ok(y), Ok(m), Ok(d)) =
                (c[1].parse::<i16>(), c[2].parse::<i8>(), c[3].parse::<i8>())
            && let Ok(parsed) = Date::new(y, m, d)
        {
            stamps.push(parsed);
        }
    }

    let mut parts: Vec<String> = Vec::new();
    if undated > 0 {
        parts.push(format!(
            "{undated} of {} sources carry no publish date, so their age is unknown. \
             Do not assume they are recent.",
            hits.len()
        ));
    }
    if stamps.len() >= 2
        && let (Some(&min_d), Some(&max_d)) = (stamps.iter().min(), stamps.iter().max())
        && let Ok(span) = min_d.until(max_d)
        && span.get_days() > DATE_SPREAD_WARN_DAYS
    {
        parts.push(format!(
            "Dated sources span {min_d} to {max_d}. \
             Where they disagree, prefer the newer one and say which you used."
        ));
    }

    (!parts.is_empty()).then(|| parts.join(" "))
}

/// Run `web.research`: search -> fetch top `depth` pages -> compute `date_note` & tokens.
#[allow(clippy::too_many_arguments)]
pub async fn run_research(
    query: &str,
    opts: ResearchOpts,
    deps: &SourceDeps,
    docs: &Arc<dyn DocApi>,
    search: &Arc<dyn SearchApi>,
    gnews: &GnewsResolver,
    fetch_limiter: Option<&HostRateLimiter>,
    cx: &CallCx,
) -> Result<ResearchBrief, FsmError> {
    if query.trim().is_empty() {
        return Err(FsmError::invalid("research query must not be empty"));
    }

    let depth = opts.depth.clamp(MIN_DEPTH, MAX_DEPTH);

    let mut req = SearchRequest::new(query);
    if let Some(scope) = opts.scope {
        req.scope = scope;
    }
    req.freshness = opts.freshness;
    req.domains = opts.domains;
    req.lang = opts.lang;
    req.limit = (u32::from(depth) * 2).max(u32::from(depth) + 3);

    let search_resp = search.search(req, cx).await?;
    let top_hits: Vec<Hit> = search_resp
        .results
        .iter()
        .take(usize::from(depth))
        .cloned()
        .collect();

    let sem = Arc::new(Semaphore::new(usize::from(MAX_BATCH_CONCURRENCY)));
    let fetch_futs = top_hits.iter().map(|hit| {
        let sem = Arc::clone(&sem);
        let url = hit.url.clone();
        let fallback_title = hit.title.clone();
        async move {
            let Ok(_permit) = sem.acquire_owned().await else {
                return None;
            };
            let rem = cx.remaining();
            if rem.is_zero() || cx.cancel.is_cancelled() {
                return None;
            }
            let fut = fetch_page(
                &url,
                FetchOpts::default(),
                deps,
                docs,
                gnews,
                fetch_limiter,
                cx,
            );
            match tokio::time::timeout(rem, fut).await {
                Ok(Ok(outcome)) => {
                    let mut page = outcome_to_page(outcome);
                    if page.title.is_empty() {
                        page.title = fallback_title;
                    }
                    Some(page)
                }
                Ok(Err(e)) => {
                    tracing::debug!("research skipped {url}: {e}");
                    None
                }
                Err(_) => {
                    tracing::debug!("research timed out reading {url}");
                    None
                }
            }
        }
    });

    let slot_results: Vec<Option<PageResult>> = join_all(fetch_futs).await;
    let slot_refs: Vec<Option<&PageResult>> = slot_results.iter().map(Option::as_ref).collect();
    let date_note = compute_date_note(&top_hits, &slot_refs);

    let pages: Vec<PageResult> = slot_results.into_iter().flatten().collect();
    let page_tokens: usize = pages.iter().map(|p| p.tokens_estimated).sum();
    let tokens_estimated = if page_tokens > 0 {
        page_tokens
    } else {
        let snippets = top_hits
            .iter()
            .map(|h| h.snippet.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        estimate_tokens(&snippets)
    };

    let mut brief = ResearchBrief::new(query, search_resp);
    brief.pages = pages;
    brief.date_note = date_note;
    brief.tokens_estimated = tokens_estimated;
    Ok(brief)
}
