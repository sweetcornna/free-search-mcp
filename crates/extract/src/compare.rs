//! Side-by-side URL comparison (`web.compare`, port of `src/search_mcp/compare.py`).
//!
//! Enforces:
//! - 2–5 URLs (`InvalidInput` outside `[2, 5]`).
//! - 6,000-character excerpt budget per URL (preserving upstream `truncated: true`).
//! - Per-URL error capture into `CompareItem.error` so partial results still return.

use crate::fetch::{fetch_page, outcome_to_page};
use crate::gnews::GnewsResolver;
use crate::request::HostRateLimiter;
use fsm_core::text::smart_truncate;
use fsm_core::{CallCx, CompareItem, DocApi, FetchOpts, FsmError, SourceDeps};
use futures::future::join_all;
use std::sync::Arc;
use tokio::sync::Semaphore;

pub const MIN_COMPARE_URLS: usize = 2;
pub const MAX_COMPARE_URLS: usize = 5;
pub const COMPARE_CHAR_BUDGET: usize = 6_000;

/// Fetch 2–5 URLs concurrently and return per-URL excerpts capped at 6,000 chars each.
pub async fn compare_urls(
    _query: &str,
    urls: Vec<String>,
    deps: &SourceDeps,
    docs: &Arc<dyn DocApi>,
    gnews: &GnewsResolver,
    fetch_limiter: Option<&HostRateLimiter>,
    cx: &CallCx,
) -> Result<Vec<CompareItem>, FsmError> {
    if !(MIN_COMPARE_URLS..=MAX_COMPARE_URLS).contains(&urls.len()) {
        return Err(FsmError::invalid(format!(
            "compare expects {MIN_COMPARE_URLS}-{MAX_COMPARE_URLS} URLs, got {}",
            urls.len()
        )));
    }

    let sem = Arc::new(Semaphore::new(MAX_COMPARE_URLS));
    let futs = urls.into_iter().map(|url| {
        let sem = Arc::clone(&sem);
        async move {
            let Ok(_permit) = sem.acquire_owned().await else {
                let mut item = CompareItem::new(url);
                item.error = Some("compare semaphore closed".to_owned());
                return item;
            };
            match fetch_page(
                &url,
                FetchOpts::default(),
                deps,
                docs,
                gnews,
                fetch_limiter,
                cx,
            )
            .await
            {
                Ok(outcome) => {
                    let page = outcome_to_page(outcome);
                    let (excerpt, soft_trunc) = smart_truncate(&page.content, COMPARE_CHAR_BUDGET);
                    let resolved_url = if page.url.is_empty() { url } else { page.url };
                    let mut item = CompareItem::new(resolved_url);
                    item.title = (!page.title.is_empty()).then_some(page.title);
                    item.published_date = page.published_date;
                    item.excerpt = Some(excerpt);
                    item.truncated = Some(soft_trunc || page.truncated);
                    item
                }
                Err(e) => {
                    let mut item = CompareItem::new(url);
                    item.error = Some(e.to_string());
                    item
                }
            }
        }
    });

    Ok(join_all(futs).await)
}
