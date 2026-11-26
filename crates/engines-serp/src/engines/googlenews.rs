//! Google News RSS engine (`news.google.com/rss/search`).
//!
//! Port of `src/search_mcp/engines/googlenews.py`:
//! - Parses RSS 2.0 XML via `roxmltree` (NEVER HTML5 `dom_query`, which treats
//!   `<link>` as a void element and drops its text!);
//! - Auto-selects the Google News edition (`hl`, `gl`, `ceid`) via query script
//!   detection (`detect_query_region`);
//! - Appends `when:1d|7d|1m|1y` for freshness;
//! - Resolves top-`limit` `news.google.com/rss/articles/...` redirect blobs to
//!   publisher URLs via bounded concurrent `batchexecute` RPC (`gnews.rs`) before
//!   host-based domain filtering.

use crate::common::{
    CallDeadline, SerpQuery, augment_query_with_operators, aux_net_cx, check_http_status,
    decode_body, detect_query_region, finalize_hits, make_hit, net_cx_with_deadline, quote_plus,
    region_to_google_news_ceid, region_to_google_params, search_op_decl, strip_html_tags,
};
use crate::gnews::{GNewsResolver, is_google_news_url};
use async_trait::async_trait;
use fsm_core::api::Freshness;
use fsm_core::net::{HttpRequest, NetClient, NetCx};
use fsm_core::source::{
    AuthMode, Capability, PoolRole, Source, SourceDecl, SourceLimits, SrcCx, SrcOutput,
    TransportProfile,
};
use fsm_core::types::{Hit, HitKind};
use fsm_core::{FsmError, SourceDeps};
use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

pub const ENDPOINT: &str = "https://news.google.com/rss/search";
pub const RESOLVE_CONCURRENCY: usize = 8;
pub const RESOLVE_BUDGET: Duration = Duration::from_millis(2400);
pub const RESOLVE_MAX_FAILURES: usize = 3;
const RESOLVE_DEADLINE_MARGIN: Duration = Duration::from_millis(350);

fn freshness_token(f: Freshness) -> &'static str {
    match f {
        Freshness::Day => "1d",
        Freshness::Week => "7d",
        Freshness::Month => "1m",
        Freshness::Year => "1y",
        _ => "7d",
    }
}

/// Format an RFC 2822 RSS `<pubDate>` into relative (`"N minutes/hours/days ago"`
/// for < 14 days) or `"YYYY-MM-DD"` (exact port of `googlenews._format_pubdate`).
pub fn format_pubdate(raw: Option<&str>, now: Timestamp) -> Option<String> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    let parsed = jiff::fmt::rfc2822::parse(raw).ok()?;
    let dt = parsed.timestamp();
    let secs = now.as_second() - dt.as_second();
    if secs < 0 {
        return Some(dt.to_zoned_datetime(TimeZone::UTC).date().to_string());
    }
    if secs < 3600 {
        let m = (secs / 60).max(1);
        return Some(format!("{m} minute{} ago", if m == 1 { "" } else { "s" }));
    }
    if secs < 86_400 {
        let h = secs / 3600;
        return Some(format!("{h} hour{} ago", if h == 1 { "" } else { "s" }));
    }
    if secs < 86_400 * 14 {
        let d = secs / 86_400;
        return Some(format!("{d} day{} ago", if d == 1 { "" } else { "s" }));
    }
    Some(dt.to_zoned_datetime(TimeZone::UTC).date().to_string())
}

/// Build the Google News RSS search URL (`q`, `hl`, `gl`, `ceid`).
pub fn build_url(q: &SerpQuery) -> String {
    let mut augmented =
        augment_query_with_operators(&q.query, &q.include_domains, &q.exclude_domains, None);
    if let Some(f) = q.freshness {
        augmented.push_str(" when:");
        augmented.push_str(freshness_token(f));
    }
    let region = detect_query_region(&q.query, &q.region);
    let (hl, gl) = region_to_google_params(&region);
    let ceid = region_to_google_news_ceid(&region);
    format!(
        "{ENDPOINT}?q={}&hl={hl}&gl={gl}&ceid={ceid}",
        quote_plus(&augmented)
    )
}

/// Parse a Google News RSS 2.0 XML document using `roxmltree`.
pub fn parse(xml: &str) -> Vec<Hit> {
    parse_with_now(xml, Timestamp::now())
}

/// Parse with an explicit `now` timestamp (for deterministic date assertions).
pub fn parse_with_now(xml: &str, now: Timestamp) -> Vec<Hit> {
    if xml.trim().is_empty() {
        return Vec::new();
    }
    // R3: reject documents declaring XML entities before parsing and enforce a node limit.
    let needle = b"<!entity";
    if xml
        .as_bytes()
        .windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle))
    {
        return Vec::new();
    }
    let opts = roxmltree::ParsingOptions {
        allow_dtd: false,
        nodes_limit: 200_000,
        ..roxmltree::ParsingOptions::default()
    };
    let Ok(doc) = roxmltree::Document::parse_with_options(xml, opts) else {
        return Vec::new();
    };
    let mut results = Vec::new();

    for node in doc.descendants() {
        if !node.is_element() || node.tag_name().name() != "item" {
            continue;
        }
        let child_text = |tag: &str| -> String {
            node.children()
                .find(|c| c.is_element() && c.tag_name().name() == tag)
                .and_then(|c| c.text())
                .map(str::trim)
                .unwrap_or("")
                .to_owned()
        };
        let title = child_text("title");
        let url = child_text("link");
        if title.is_empty() || url.is_empty() {
            continue;
        }
        let source = child_text("source");
        let display_title = if source.is_empty() {
            title
        } else {
            format!("{title} ({source})")
        };
        let raw_desc = child_text("description");
        let snippet = strip_html_tags(&raw_desc);
        let pub_raw = child_text("pubDate");
        let date = format_pubdate((!pub_raw.is_empty()).then_some(pub_raw.as_str()), now);
        results.push(make_hit(
            "googlenews",
            HitKind::News,
            display_title,
            url,
            snippet,
            date,
            Some("rss"),
        ));
    }
    results
}

/// Custom resolver callback type (used to inject fast/scripted resolvers in tests).
pub type ResolveUrlFn = Arc<
    dyn Fn(
            String,
            Arc<dyn NetClient>,
            String,
            NetCx,
        ) -> Pin<Box<dyn Future<Output = Option<String>> + Send>>
        + Send
        + Sync,
>;

/// Configuration for publisher-URL resolution (overridable in unit tests).
#[derive(Clone)]
pub struct ResolveConfig {
    pub concurrency: usize,
    pub budget: Duration,
    pub max_failures: usize,
}

impl Default for ResolveConfig {
    fn default() -> Self {
        Self {
            concurrency: RESOLVE_CONCURRENCY,
            budget: RESOLVE_BUDGET,
            max_failures: RESOLVE_MAX_FAILURES,
        }
    }
}

/// Rewrite `news.google.com` redirect blobs to publisher URLs in place,
/// bounded by concurrency, total budget, and consecutive failures.
pub async fn resolve_publisher_urls(
    hits: &mut [Hit],
    net: Arc<dyn NetClient>,
    accept_language: String,
    ncx: NetCx,
    resolve_fn: ResolveUrlFn,
    cfg: &ResolveConfig,
) {
    let indices: Vec<usize> = hits
        .iter()
        .enumerate()
        .filter(|(_, h)| is_google_news_url(&h.url))
        .map(|(i, _)| i)
        .collect();
    if indices.is_empty() {
        return;
    }

    let sem = Arc::new(Semaphore::new(cfg.concurrency.max(1)));
    let failures = Arc::new(AtomicUsize::new(0));
    let resolved_slots: Arc<tokio::sync::Mutex<Vec<(usize, String)>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::new()));

    // `JoinSet` automatically aborts every spawned task if dropped (e.g. caller
    // straggler cut or budget timeout), avoiding detached background fetches.
    let mut tasks = JoinSet::new();
    for idx in indices {
        let url = hits[idx].url.clone();
        let sem = Arc::clone(&sem);
        let failures = Arc::clone(&failures);
        let resolved_slots = Arc::clone(&resolved_slots);
        let net = Arc::clone(&net);
        let accept_language = accept_language.clone();
        let ncx = ncx.clone();
        let resolve_fn = Arc::clone(&resolve_fn);
        let max_failures = cfg.max_failures;

        tasks.spawn(async move {
            let Ok(_permit) = sem.acquire_owned().await else {
                return;
            };
            if failures.load(Ordering::Acquire) >= max_failures {
                return;
            }
            let out = resolve_fn(url, net, accept_language, ncx).await;
            if let Some(pub_url) = out {
                failures.store(0, Ordering::Release);
                resolved_slots.lock().await.push((idx, pub_url));
            } else {
                failures.fetch_add(1, Ordering::AcqRel);
            }
        });
    }

    let drain = async { while tasks.join_next().await.is_some() {} };
    if tokio::time::timeout(cfg.budget, drain).await.is_err() {
        tasks.abort_all();
    }

    let updates = resolved_slots.lock().await;
    for (idx, pub_url) in updates.iter() {
        hits[*idx].url = pub_url.clone();
    }
}

pub struct GoogleNewsEngine {
    decl: SourceDecl,
    deps: SourceDeps,
    resolve_fn: ResolveUrlFn,
    resolve_cfg: ResolveConfig,
}

impl GoogleNewsEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let resolver = GNewsResolver::new();
        let resolve_fn: ResolveUrlFn = Arc::new(move |url, net, lang, ncx| {
            let r = resolver.clone();
            Box::pin(async move { r.resolve(&url, net.as_ref(), &lang, &ncx).await })
        });
        Self::with_resolver(deps, resolve_fn, ResolveConfig::default())
    }

    /// Construct with custom URL resolver and bounds (for deterministic unit tests).
    pub fn with_resolver(
        deps: SourceDeps,
        resolve_fn: ResolveUrlFn,
        resolve_cfg: ResolveConfig,
    ) -> Self {
        let mut decl = SourceDecl::new(
            "googlenews",
            "Google News RSS",
            "Google News RSS: headlines with exact publish dates, edition-scoped by region.",
        )
        .with_scopes(&["news"])
        .with_caps(&[Capability::Search])
        .with_op(search_op_decl(
            "Search news headlines via Google News RSS.",
            "AI news",
            "googlenews",
        ));
        decl.aliases = vec!["gnews".into(), "google_news".into()];
        decl.home_url = Some("https://news.google.com".into());
        decl.tier = 1;
        decl.auth = AuthMode::None;
        decl.transport = TransportProfile::Plain;
        decl.pool_role = PoolRole::ScopeOnly;
        decl.limits = SourceLimits::new(1.0, 3, 2, 10_000, 21_600);
        Self {
            decl,
            deps,
            resolve_fn,
            resolve_cfg,
        }
    }
}

#[async_trait]
impl Source for GoogleNewsEngine {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    async fn call(&self, op: &str, params: Value, cx: &SrcCx) -> Result<SrcOutput, FsmError> {
        let q = SerpQuery::from_call(&self.decl.id, op, &params, cx, &self.deps.cfg)?;
        let url = build_url(&q);
        let deadline = CallDeadline::from_timeout(q.timeout);
        let primary_ncx = net_cx_with_deadline(
            &cx.cancel,
            deadline,
            &self.decl.id,
            Some(self.decl.limits.rps),
        )?;
        let req = HttpRequest::get(&url)
            .header(
                "Accept",
                "application/rss+xml, application/xml, text/xml, */*;q=0.8",
            )
            .header("Accept-Language", &q.accept_language)
            .for_source(&self.decl.id);
        let resp = self
            .deps
            .net
            .send(req, self.decl.transport, &primary_ncx)
            .await?;
        check_http_status(&self.decl.id, &resp)?;
        let xml = decode_body(&resp);
        let mut results = parse(&xml);

        // Resolve only the head (`..limit`) that can be returned, matching Python.
        // Resolution hops are auxiliary (`rps: None`) bounded by concurrency and
        // `resolve_cfg.budget.min(remaining call deadline)`, using a distinct host
        // bucket so they never queue behind the RSS feed's 1.0 rps limiter.
        let head_len = results.len().min(q.limit);
        let remaining = deadline
            .remaining_or_zero()
            .saturating_sub(RESOLVE_DEADLINE_MARGIN);
        if head_len > 0 && !remaining.is_zero() {
            let mut resolve_ncx = aux_net_cx(&primary_ncx, deadline)?;
            resolve_ncx.host_bucket = Some("googlenews:resolve".to_owned());
            let budget = self.resolve_cfg.budget.min(remaining);
            resolve_ncx.timeout = budget;
            let cfg = ResolveConfig {
                budget,
                ..self.resolve_cfg.clone()
            };
            resolve_publisher_urls(
                &mut results[..head_len],
                Arc::clone(&self.deps.net),
                q.accept_language.clone(),
                resolve_ncx,
                Arc::clone(&self.resolve_fn),
                &cfg,
            )
            .await;
        }

        let hits = finalize_hits(
            results,
            &q.include_domains,
            &q.exclude_domains,
            q.filetype.as_deref(),
            q.limit,
        );
        Ok(SrcOutput::Hits(hits))
    }
}
