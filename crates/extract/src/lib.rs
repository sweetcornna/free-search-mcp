//! `fsm-extract` — web fetch, `gnews` resolution, HTML extraction, structured metadata,
//! raw HTTP request guardrails, research, compare, and ephemeral downloads (lane `W4a`).

pub mod charset;
pub mod compare;
pub mod downloads;
pub mod fetch;
pub mod gnews;
pub mod html;
pub mod request;
pub mod research;
pub mod structured;

use async_trait::async_trait;
use fsm_core::{
    CallCx, CompareItem, DocApi, DownloadResult, FetchApi, FetchOpts, FetchOutcome, FsmError,
    HttpResponse, PageResult, RawRequestOpts, ResearchBrief, ResearchOpts, SearchApi, SourceDeps,
    StructuredMeta,
};
use std::sync::Arc;

/// Default per-host rate limit for `web.request` (`SPEC.md` §3.4 guardrail 4): 2 req/s/host.
const RAW_REQUEST_RPS: f64 = 2.0;
const RAW_REQUEST_BURST: u32 = 4;

/// Fetch / research / extraction service implementing [`FetchApi`].
pub struct FetchService {
    deps: SourceDeps,
    docs: Arc<dyn DocApi>,
    search: Arc<dyn SearchApi>,
    gnews: gnews::GnewsResolver,
    raw_limiter: request::HostRateLimiter,
    fetch_limiter: Option<request::HostRateLimiter>,
}

impl FetchService {
    pub fn new(deps: SourceDeps, docs: Arc<dyn DocApi>, search: Arc<dyn SearchApi>) -> Self {
        let fetch_limiter = if deps.cfg.fetch_rate_limit_rpm > 0 {
            let rpm = deps.cfg.fetch_rate_limit_rpm;
            Some(request::HostRateLimiter::new(
                f64::from(rpm) / 60.0,
                rpm.max(1),
            ))
        } else {
            None
        };
        Self {
            deps,
            docs,
            search,
            gnews: gnews::GnewsResolver::new(),
            raw_limiter: request::HostRateLimiter::new(RAW_REQUEST_RPS, RAW_REQUEST_BURST),
            fetch_limiter,
        }
    }

    /// Access the shared Google News single-flight/memo resolver.
    pub fn gnews(&self) -> &gnews::GnewsResolver {
        &self.gnews
    }
}

#[async_trait]
impl FetchApi for FetchService {
    async fn fetch(
        &self,
        url: &str,
        opts: FetchOpts,
        cx: &CallCx,
    ) -> Result<FetchOutcome, FsmError> {
        fetch::fetch_page(
            url,
            opts,
            &self.deps,
            &self.docs,
            &self.gnews,
            self.fetch_limiter.as_ref(),
            cx,
        )
        .await
    }

    async fn fetch_batch(
        &self,
        urls: Vec<String>,
        opts: FetchOpts,
        max_concurrency: u8,
        cx: &CallCx,
    ) -> Result<Vec<PageResult>, FsmError> {
        fetch::fetch_batch_pages(
            urls,
            opts,
            max_concurrency,
            &self.deps,
            &self.docs,
            &self.gnews,
            self.fetch_limiter.as_ref(),
            cx,
        )
        .await
    }

    async fn request(&self, opts: RawRequestOpts, cx: &CallCx) -> Result<HttpResponse, FsmError> {
        request::execute_raw_request(opts, &self.deps, &self.raw_limiter, cx).await
    }

    async fn structured(&self, url: &str, cx: &CallCx) -> Result<StructuredMeta, FsmError> {
        structured::fetch_structured(url, &self.deps, cx).await
    }

    async fn research(
        &self,
        query: &str,
        opts: ResearchOpts,
        cx: &CallCx,
    ) -> Result<ResearchBrief, FsmError> {
        research::run_research(
            query,
            opts,
            &self.deps,
            &self.docs,
            &self.search,
            &self.gnews,
            self.fetch_limiter.as_ref(),
            cx,
        )
        .await
    }

    async fn compare(
        &self,
        query: &str,
        urls: Vec<String>,
        cx: &CallCx,
    ) -> Result<Vec<CompareItem>, FsmError> {
        compare::compare_urls(
            query,
            urls,
            &self.deps,
            &self.docs,
            &self.gnews,
            self.fetch_limiter.as_ref(),
            cx,
        )
        .await
    }

    async fn download(&self, url: &str, cx: &CallCx) -> Result<DownloadResult, FsmError> {
        downloads::download_url(url, &self.deps, cx).await
    }

    async fn html_to_md(&self, html: &str) -> Result<String, FsmError> {
        let input = html.to_owned();
        html::run_html_blocking("html_to_md_task", move || html::html_to_markdown(&input)).await
    }
}
