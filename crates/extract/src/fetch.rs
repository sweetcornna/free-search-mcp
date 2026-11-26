//! Web fetch and batch fetch (`web.fetch`, `web.fetchBatch`, port of `src/search_mcp/fetcher.py`).
//!
//! Enforces:
//! - `http(s)://` URLs ONLY.
//! - Offline SSRF layers 1–3 BEFORE every cache hit and BEFORE every network call.
//! - Explicit `render: "browser"` rejected BEFORE Google News resolution.
//! - Google News `batchexecute` URL resolution (single-flight + 2048 memo), skipped when tainted.
//! - Canonical + requested URL cache keys so bare domains and redirects hit cache reliably.
//! - Global fetch rate limiter (`cfg.fetch_rate_limit_rpm`) on network paths only (never cache hits).
//! - 50,000-char default content cap (`cfg.max_content_chars`); full text persisted in cache.
//! - PDF/Office URL & Content-Type delegation to `Arc<dyn DocApi>` for the first slice +
//!   `doc.read` / `doc.outline` hint (truncated slices are not stored as full pages in page cache).
//! - Binary/image asset description (`FetchMethod::Asset`) with opt-in `inline_image: true`
//!   returning `FetchOutcome::InlineImage { meta, binary }`, rejecting truncated bodies and
//!   bounding per-exec inline image count/bytes.
//! - Verbatim return for non-HTML/XML content types (`application/json`, `text/plain`, etc.).
//! - `fetch_batch` cap: ≤ 20 URLs, ≤ 8 concurrent, returning one `PageResult` per input in order.

use crate::charset::decode_body;
use crate::downloads::sha256_hex;
use crate::gnews::{GnewsResolver, is_google_news_url};
use crate::html::{extract_html, run_html_blocking};
use crate::request::{
    HostRateLimiter, check_http_url_offline, check_tainted_host, parse_retry_after,
    send_get_with_redirects,
};
use fsm_core::text::{estimate_tokens, smart_truncate};
use fsm_core::url_canon::host_of;
use fsm_core::{
    BinaryHandle, CallCx, DocApi, FetchMethod, FetchOpts, FetchOutcome, FsmError, HttpBytes,
    InlineImageMeta, PageResult, RenderMode, SourceDeps, TransportProfile,
};
use futures::future::join_all;
use jiff::Timestamp;
use std::sync::Arc;
use tokio::sync::Semaphore;
use url::Url;

pub const MAX_BATCH_URLS: usize = 20;
pub const MAX_BATCH_CONCURRENCY: u8 = 8;

/// Maximum single inline image size (5 MiB); larger images must use `web.download`.
pub const MAX_INLINE_IMAGE_BYTES: u64 = 5 * 1024 * 1024;
/// Maximum number of distinct inline images held in `CallCx.images` per exec.
pub const MAX_INLINE_IMAGES_PER_EXEC: usize = 8;
/// Maximum aggregate bytes across all inline images in `CallCx.images` per exec (20 MiB).
pub const MAX_INLINE_IMAGE_TOTAL_BYTES: u64 = 20 * 1024 * 1024;

const DOC_URL_SUFFIXES: [&str; 6] = [".pdf", ".docx", ".xlsx", ".xlsm", ".pptx", ".epub"];
const DOC_CTYPES: [&str; 6] = [
    "application/pdf",
    "wordprocessingml",
    "application/msword",
    "spreadsheetml",
    "presentationml",
    "application/epub+zip",
];

const ASSET_IMAGE_EXTS: [&str; 11] = [
    ".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp", ".tiff", ".tif", ".ico", ".avif", ".heic",
];
const ASSET_CTYPE_PREFIXES: [&str; 4] = ["image/", "video/", "audio/", "font/"];
const ASSET_CTYPES: [&str; 7] = [
    "application/octet-stream",
    "application/zip",
    "application/x-tar",
    "application/gzip",
    "application/x-7z-compressed",
    "application/x-rar",
    "application/wasm",
];

/// Canonical cache key (`url::Url` serialization with any fragment stripped).
pub fn canonical_cache_key(url: &str) -> String {
    let trimmed = url.trim();
    match Url::parse(trimmed) {
        Ok(mut parsed) => {
            parsed.set_fragment(None);
            parsed.into()
        }
        Err(_) => trimmed.to_owned(),
    }
}

/// Current UTC timestamp formatted as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn now_rfc3339() -> String {
    let now = Timestamp::now();
    Timestamp::from_second(now.as_second())
        .unwrap_or(now)
        .to_string()
}

/// Compute elapsed seconds since an RFC 3339 `retrieved_at` timestamp, if parseable.
pub fn cache_age_from_rfc3339(retrieved_at: &str) -> Option<u64> {
    let ts: Timestamp = retrieved_at.trim().parse().ok()?;
    let diff = Timestamp::now().as_second().saturating_sub(ts.as_second());
    Some(diff.max(0) as u64)
}

/// True for URLs whose path ends in a PDF or Office document extension.
pub fn is_document_url(url: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    let path = parsed.path().to_ascii_lowercase();
    DOC_URL_SUFFIXES.iter().any(|ext| path.ends_with(ext))
}

/// True for PDF or Office document `Content-Type` values.
pub fn is_document_ctype(ctype: &str) -> bool {
    let c = ctype.to_ascii_lowercase();
    DOC_CTYPES.iter().any(|t| c.contains(t))
}

/// True for HTML/XML content types, or an empty header (recoverable).
pub fn ctype_is_markup(ctype: &str) -> bool {
    let c = ctype.to_ascii_lowercase();
    c.is_empty() || c.contains("html") || c.contains("xml")
}

/// True for opaque binary/media content types (excluding SVG, which is readable XML markup).
pub fn is_asset_ctype(ctype: &str) -> bool {
    let c = ctype
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if c.is_empty() || c.starts_with("image/svg") {
        return false;
    }
    ASSET_CTYPE_PREFIXES.iter().any(|p| c.starts_with(p))
        || ASSET_CTYPES.iter().any(|t| c.contains(t))
}

/// True for URLs whose path ends in a raster image extension (excluding `.svg`).
pub fn is_asset_url(url: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    let path = parsed.path().to_ascii_lowercase();
    if path.ends_with(".svg") {
        return false;
    }
    ASSET_IMAGE_EXTS.iter().any(|ext| path.ends_with(ext))
}

/// Read `(width, height)` from an image header without decoding pixels.
/// Returns `(None, None)` for unrecognized or malformed bytes.
pub fn image_dimensions(blob: &[u8]) -> (Option<u32>, Option<u32>) {
    imagesize::blob_size(blob)
        .ok()
        .and_then(|sz| {
            Some((
                u32::try_from(sz.width).ok()?,
                u32::try_from(sz.height).ok()?,
            ))
        })
        .map_or((None, None), |(w, h)| (Some(w), Some(h)))
}

fn human_size(n: u64) -> String {
    let mut value = n as f64;
    for unit in ["B", "KB", "MB", "GB"] {
        if value < 1024.0 || unit == "GB" {
            return if unit == "B" {
                format!("{value:.0} {unit}")
            } else {
                format!("{value:.1} {unit}")
            };
        }
        value /= 1024.0;
    }
    format!("{value:.1} GB")
}

fn format_doc_pagination_hint(url: &str, next_start: usize, full_total: usize) -> String {
    format!(
        "\n\n[Document truncated ({next_start}/{full_total} chars). \
         Use `doc.read({url:?}, {{start: {next_start}}})` to paginate or `doc.outline({url:?})` for structure.]"
    )
}

async fn record_fetched_host(url: &str, cx: &CallCx) {
    if !cx.is_tainted() {
        let host = host_of(url);
        if !host.is_empty() {
            cx.fetched_hosts.lock().await.insert(host);
        }
    }
}

async fn store_page_under_keys(
    base_page: &PageResult,
    req_key: &str,
    canon_key: &str,
    final_key: &str,
    deps: &SourceDeps,
) {
    let mut keys: Vec<&str> = Vec::with_capacity(3);
    keys.push(req_key);
    if canon_key != req_key {
        keys.push(canon_key);
    }
    if final_key != req_key && final_key != canon_key {
        keys.push(final_key);
    }
    for key in keys {
        let mut copy = base_page.clone();
        copy.url = key.to_owned();
        if let Err(e) = deps.store.put_page(&copy, deps.cfg.cache_ttl_s).await {
            tracing::warn!("could not cache page {key}: {e}");
        }
    }
}

async fn build_asset_outcome(
    url: &str,
    resp: HttpBytes,
    inline_image: bool,
    max_response_bytes: u64,
    cx: &CallCx,
) -> Result<FetchOutcome, FsmError> {
    if resp.truncated {
        return Err(FsmError::MaxBytesExceeded(max_response_bytes));
    }
    let media_type = resp
        .media_type()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "application/octet-stream".to_owned());
    let bytes_len = resp.body.len() as u64;
    let hash = sha256_hex(&resp.body);
    let (width, height) = if media_type.starts_with("image/") {
        image_dimensions(&resp.body)
    } else {
        (None, None)
    };

    if inline_image && media_type.starts_with("image/") {
        if bytes_len > MAX_INLINE_IMAGE_BYTES {
            return Err(FsmError::BudgetExceeded(format!(
                "image is {} ({bytes_len} bytes), exceeding the {} inline image cap; use web.download instead",
                human_size(bytes_len),
                human_size(MAX_INLINE_IMAGE_BYTES)
            )));
        }
        let handle_id = format!("img-{}", &hash[..12]);
        let binary = BinaryHandle::new(handle_id.clone(), media_type.clone(), resp.body);
        {
            let mut images = cx.images.lock().await;
            if !images.contains_key(&handle_id) {
                if images.len() >= MAX_INLINE_IMAGES_PER_EXEC {
                    return Err(FsmError::BudgetExceeded(format!(
                        "inline image limit of {MAX_INLINE_IMAGES_PER_EXEC} per exec reached; use web.download instead"
                    )));
                }
                let current_total: u64 = images.values().map(|h| h.bytes.len() as u64).sum();
                if current_total.saturating_add(bytes_len) > MAX_INLINE_IMAGE_TOTAL_BYTES {
                    return Err(FsmError::BudgetExceeded(format!(
                        "inline image total budget of {} per exec exceeded; use web.download instead",
                        human_size(MAX_INLINE_IMAGE_TOTAL_BYTES)
                    )));
                }
                images.insert(handle_id, binary.clone());
            }
        }
        let mut meta = InlineImageMeta::new(url, media_type, bytes_len, hash);
        meta.width = width;
        meta.height = height;
        return Ok(FetchOutcome::InlineImage { meta, binary });
    }

    let name = Url::parse(url)
        .ok()
        .and_then(|u| {
            u.path()
                .rsplit('/')
                .next()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| url.to_owned());

    let mut bits = vec![
        format!("**{name}**"),
        media_type.clone(),
        human_size(bytes_len),
    ];
    if let (Some(w), Some(h)) = (width, height) {
        bits.push(format!("{w}×{h}px"));
    }
    let mut summary = bits.join(" · ");
    if !inline_image && media_type.starts_with("image/") {
        summary.push_str(
            "\n\nBytes not included. Call `web.fetch` again with `{inlineImage: true}` to \
             pass the image itself to a vision-capable model.",
        );
    }

    let mut page = PageResult::new(url, name, summary, FetchMethod::Asset);
    page.media_type = Some(media_type);
    page.bytes = Some(bytes_len);
    page.width = width;
    page.height = height;
    page.sha256 = Some(hash);
    page.retrieved_at = now_rfc3339();
    Ok(FetchOutcome::Page(page))
}

async fn fetch_as_asset(
    url: &str,
    inline_image: bool,
    deps: &SourceDeps,
    cx: &CallCx,
) -> Result<FetchOutcome, FsmError> {
    let (final_url, resp) = send_get_with_redirects(
        url,
        &[],
        TransportProfile::Plain,
        deps.cfg.max_response_bytes,
        "fetch",
        deps,
        cx,
    )
    .await?;

    if resp.status == 429 {
        return Err(FsmError::RateLimited {
            source_id: host_of(&final_url),
            retry_after_s: parse_retry_after(&resp),
        });
    }
    if !resp.is_success() {
        return Err(FsmError::upstream(
            "fetch",
            Some(resp.status),
            format!("HTTP {} fetching asset {final_url}", resp.status),
        ));
    }

    record_fetched_host(&final_url, cx).await;
    build_asset_outcome(
        &final_url,
        resp,
        inline_image,
        deps.cfg.max_response_bytes,
        cx,
    )
    .await
}

async fn fetch_as_document(
    url: &str,
    req_key: &str,
    canon_key: &str,
    deps: &SourceDeps,
    docs: &Arc<dyn DocApi>,
    cx: &CallCx,
) -> Result<FetchOutcome, FsmError> {
    check_tainted_host(url, cx).await?;
    let max_chars = deps.cfg.max_content_chars;
    let slice = docs.read(url, 0, Some(max_chars), cx).await?;

    let title = slice.title.unwrap_or_default();
    let full_total = slice.total_chars.max(slice.content.chars().count());
    let now = now_rfc3339();

    // Only write complete (untruncated) documents into the page cache so `cache.getPage`
    // never serves a 50k first-slice as full text; `DocApi` owns full-document caching.
    if !slice.truncated {
        let mut cached_page = PageResult::new(
            url,
            title.clone(),
            slice.content.clone(),
            FetchMethod::Document,
        );
        cached_page.total_chars = full_total;
        cached_page.truncated = false;
        cached_page.retrieved_at = now.clone();
        let final_key = canonical_cache_key(url);
        store_page_under_keys(&cached_page, req_key, canon_key, &final_key, deps).await;
    }

    let (mut content, soft_trunc) = smart_truncate(&slice.content, max_chars);
    let truncated = slice.truncated || soft_trunc;
    if truncated {
        let next_start = slice.returned_chars.min(full_total);
        content.push_str(&format_doc_pagination_hint(url, next_start, full_total));
    }

    let mut page = PageResult::new(url, title, content, FetchMethod::Document);
    page.total_chars = full_total;
    page.truncated = truncated;
    page.retrieved_at = now;
    record_fetched_host(url, cx).await;
    Ok(FetchOutcome::Page(page))
}

/// Core `web.fetch` implementation.
#[allow(clippy::too_many_arguments)]
pub async fn fetch_page(
    raw_url: &str,
    opts: FetchOpts,
    deps: &SourceDeps,
    docs: &Arc<dyn DocApi>,
    gnews: &GnewsResolver,
    fetch_limiter: Option<&HostRateLimiter>,
    cx: &CallCx,
) -> Result<FetchOutcome, FsmError> {
    // 1. Validate scheme (http(s) ONLY) + offline SSRF layers 1–3 BEFORE gnews or cache.
    check_http_url_offline(raw_url, deps)?;
    let mut url = raw_url.trim().to_owned();

    // 2. Explicit browser mode: 1.0 ships P1 stub only (check BEFORE spending gnews requests).
    if opts.render == RenderMode::Browser {
        return Err(FsmError::BrowserUnavailable(
            "Browser rendering is not available in 1.0 (planned for P1). \
             Retry with render: \"http\" or \"auto\"."
                .into(),
        ));
    }

    // 3. Best-effort Google News batchexecute URL resolution (single-flight + 2048 memo).
    //    Skipped when tainted under M6 so no batchexecute RPC or arbitrary publisher host is hit.
    if !cx.is_tainted()
        && is_google_news_url(&url)
        && let Some(resolved) = gnews.resolve(&url, deps, cx).await
    {
        url = resolved;
        // Re-check offline SSRF layers 1–3 on the resolved publisher URL BEFORE cache lookup!
        check_http_url_offline(&url, deps)?;
    }

    let req_key = url.clone();
    let canon_key = canonical_cache_key(&url);

    // 4. Page cache lookup (skipped when force_refresh, inline_image, or max_age_hours <= 0).
    let max_age_s: Option<u64> = opts.max_age_hours.map(|h| {
        if h <= 0.0 {
            0
        } else {
            (h * 3600.0).round() as u64
        }
    });
    let skip_cache_read = opts.force_refresh || opts.inline_image || max_age_s == Some(0);
    if !skip_cache_read {
        let mut hit = deps.store.get_page(&req_key, max_age_s).await?;
        if hit.is_none() && canon_key != req_key {
            hit = deps.store.get_page(&canon_key, max_age_s).await?;
        }
        if let Some(mut cached) = hit {
            let was_doc = cached.method == FetchMethod::Document;
            let full_chars = cached.total_chars.max(cached.content.chars().count());
            let (mut content, was_trunc) =
                smart_truncate(&cached.content, deps.cfg.max_content_chars);
            let is_trunc = cached.truncated || was_trunc;
            if was_doc && is_trunc && !content.contains("doc.read") {
                let shown = content.chars().count().min(full_chars);
                content.push_str(&format_doc_pagination_hint(&url, shown, full_chars));
            }
            cached.method = FetchMethod::Cache;
            cached.total_chars = full_chars;
            cached.truncated = is_trunc;
            cached.tokens_estimated = estimate_tokens(&content);
            cached.content = content;
            if cached.cache_age_s.is_none() && !cached.retrieved_at.is_empty() {
                cached.cache_age_s = cache_age_from_rfc3339(&cached.retrieved_at);
            }
            record_fetched_host(&url, cx).await;
            return Ok(FetchOutcome::Page(cached));
        }
    }

    // 5. Enforce M6 taint rule BEFORE acquiring fetch limiter or starting any outbound transfer.
    check_tainted_host(&url, cx).await?;

    // 6. Global outbound fetch pacing (never applied to cache hits or rejected URLs).
    if let Some(limiter) = fetch_limiter {
        limiter.acquire("fetch", cx).await?;
    }

    // 7. Route PDF/Office URLs to DocApi before downloading as text.
    if is_document_url(&url) {
        return fetch_as_document(&url, &req_key, &canon_key, deps, docs, cx).await;
    }

    // 8. Route image URLs to asset description / inline image path.
    if is_asset_url(&url) {
        return fetch_as_asset(&url, opts.inline_image, deps, cx).await;
    }

    // 9. Standard HTTP fetch via Chrome transport profile (≤ 5 manual redirects + per-hop SSRF/taint).
    let headers = [
        (
            "Accept".to_owned(),
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8".to_owned(),
        ),
        (
            "Accept-Language".to_owned(),
            deps.cfg.accept_language.clone(),
        ),
    ];
    let (final_url, resp) = send_get_with_redirects(
        &url,
        &headers,
        TransportProfile::Chrome,
        deps.cfg.max_response_bytes,
        "fetch",
        deps,
        cx,
    )
    .await?;

    if resp.status == 429 {
        return Err(FsmError::RateLimited {
            source_id: host_of(&final_url),
            retry_after_s: parse_retry_after(&resp),
        });
    }
    if !resp.is_success() {
        return Err(FsmError::upstream(
            "fetch",
            Some(resp.status),
            format!("HTTP {} fetching {final_url}", resp.status),
        ));
    }

    let raw_ctype = resp.header("content-type").unwrap_or("").to_owned();
    let media = resp.media_type().unwrap_or_default();

    // Extension-less URL serving a PDF/Office document.
    if is_document_ctype(&media) {
        return fetch_as_document(&final_url, &req_key, &canon_key, deps, docs, cx).await;
    }

    // Extension-less URL serving an image or opaque binary asset (reuse already-fetched bytes!).
    if is_asset_ctype(&media) {
        record_fetched_host(&final_url, cx).await;
        return build_asset_outcome(
            &final_url,
            resp,
            opts.inline_image,
            deps.cfg.max_response_bytes,
            cx,
        )
        .await;
    }

    let resp_truncated = resp.truncated;
    let decoded = decode_body(&resp.body, &raw_ctype, Some(&final_url));
    if decoded.is_empty() {
        return Err(FsmError::upstream(
            "fetch",
            Some(resp.status),
            format!("empty response for {final_url}"),
        ));
    }

    // Only HTML/XML payloads go through trafilatura/dom_smoothie extraction.
    // JSON, plain text, CSV, etc. are returned VERBATIM.
    let is_markup = media.contains("html") || media.contains("xml");
    let (title, full_md, author, published_date, sitename, html_truncated) = if is_markup {
        let html_in = decoded;
        let url_in = final_url.clone();
        let extracted =
            run_html_blocking("extract_task", move || extract_html(&html_in, &url_in)).await?;
        (
            extracted.title,
            extracted.markdown,
            extracted.author,
            extracted.published_date,
            extracted.sitename,
            extracted.truncated,
        )
    } else {
        (String::new(), decoded, None, None, None, false)
    };

    let now = now_rfc3339();
    let full_chars = full_md.chars().count();
    let media_opt = (!media.is_empty()).then_some(media);
    let upstream_truncated = resp_truncated || html_truncated;

    // Persist FULL untruncated content in page cache under requested, canonical, and final URLs
    // so repeat fetches and `cache.getPage(<requested url>)` always hit.
    let mut full_page = PageResult::new(&final_url, &title, &full_md, FetchMethod::Http);
    full_page.total_chars = full_chars;
    full_page.truncated = upstream_truncated;
    full_page.author = author.clone();
    full_page.published_date = published_date.clone();
    full_page.sitename = sitename.clone();
    full_page.media_type = media_opt.clone();
    full_page.retrieved_at = now.clone();
    let final_key = canonical_cache_key(&final_url);
    store_page_under_keys(&full_page, &req_key, &canon_key, &final_key, deps).await;

    // Truncate returned content to MAX_CONTENT_CHARS (default 50,000).
    let (content, soft_trunc) = smart_truncate(&full_md, deps.cfg.max_content_chars);
    let mut page = PageResult::new(&final_url, title, content, FetchMethod::Http);
    page.total_chars = full_chars;
    page.truncated = soft_trunc || upstream_truncated;
    page.author = author;
    page.published_date = published_date;
    page.sitename = sitename;
    page.media_type = media_opt;
    page.retrieved_at = now;

    record_fetched_host(&final_url, cx).await;
    Ok(FetchOutcome::Page(page))
}

/// Convert any `FetchOutcome` into a `PageResult` (used by `fetch_batch`).
pub fn outcome_to_page(outcome: FetchOutcome) -> PageResult {
    match outcome {
        FetchOutcome::Page(p) => p,
        FetchOutcome::InlineImage { meta, .. } => {
            let mut summary = format!(
                "**{}** · {} · {}",
                meta.url,
                meta.media_type,
                human_size(meta.bytes)
            );
            if let (Some(w), Some(h)) = (meta.width, meta.height) {
                summary.push_str(&format!(" · {w}×{h}px"));
            }
            let mut p = PageResult::new(&meta.url, &meta.url, summary, FetchMethod::Asset);
            p.media_type = Some(meta.media_type);
            p.bytes = Some(meta.bytes);
            p.width = meta.width;
            p.height = meta.height;
            p.sha256 = Some(meta.sha256);
            p.retrieved_at = now_rfc3339();
            p
        }
        _ => PageResult::default(),
    }
}

/// Concurrent batch fetch (`web.fetchBatch`, ≤ 20 URLs, ≤ 8 concurrent).
///
/// Returns exactly `urls.len()` entries in input order. Per-URL errors (including
/// `UnsafeUrl` or upstream HTTP failures) are recorded in-place as placeholder
/// entries (`[fetch failed: ...]`) so valid siblings are never dropped; only
/// `BudgetExceeded` and exec cancellation abort the batch.
#[allow(clippy::too_many_arguments)]
pub async fn fetch_batch_pages(
    urls: Vec<String>,
    opts: FetchOpts,
    max_concurrency: u8,
    deps: &SourceDeps,
    docs: &Arc<dyn DocApi>,
    gnews: &GnewsResolver,
    fetch_limiter: Option<&HostRateLimiter>,
    cx: &CallCx,
) -> Result<Vec<PageResult>, FsmError> {
    if urls.len() > MAX_BATCH_URLS {
        return Err(FsmError::invalid(format!(
            "fetchBatch accepts at most {MAX_BATCH_URLS} URLs, got {}",
            urls.len()
        )));
    }
    if urls.is_empty() {
        return Ok(Vec::new());
    }
    if cx.cancel.is_cancelled() {
        return Err(FsmError::Timeout(0));
    }

    let conc = if max_concurrency == 0 {
        MAX_BATCH_CONCURRENCY
    } else {
        max_concurrency.min(MAX_BATCH_CONCURRENCY)
    } as usize;
    let sem = Arc::new(Semaphore::new(conc));

    let futs = urls.into_iter().map(|url| {
        let sem = Arc::clone(&sem);
        let opts = opts.clone();
        async move {
            let _permit = sem
                .acquire_owned()
                .await
                .map_err(|_| FsmError::internal("semaphore", "batch semaphore closed"))?;
            if cx.cancel.is_cancelled() {
                return Ok::<(String, Result<FetchOutcome, FsmError>), FsmError>((
                    url,
                    Err(FsmError::Timeout(0)),
                ));
            }
            let res = fetch_page(&url, opts, deps, docs, gnews, fetch_limiter, cx).await;
            Ok((url, res))
        }
    });

    let settled = join_all(futs).await;
    let mut pages = Vec::with_capacity(settled.len());

    for item in settled {
        let (url, res) = item?;
        match res {
            Ok(outcome) => pages.push(outcome_to_page(outcome)),
            Err(e @ FsmError::BudgetExceeded(_)) => return Err(e),
            Err(e @ FsmError::Timeout(0)) if cx.cancel.is_cancelled() => return Err(e),
            Err(e) => {
                tracing::warn!("fetchBatch entry failed for {url}: {e}");
                let mut placeholder =
                    PageResult::new(&url, "", format!("[fetch failed: {e}]"), FetchMethod::Http);
                placeholder.retrieved_at = now_rfc3339();
                pages.push(placeholder);
            }
        }
    }

    Ok(pages)
}
