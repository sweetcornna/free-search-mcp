//! Google News `batchexecute` URL resolver (port of `src/search_mcp/gnews.py`).
//!
//! Resolves opaque `https://news.google.com/{rss/articles,articles,read}/CBM...`
//! redirect blobs to their real publisher URLs by fetching the article shell for
//! `data-n-a-sg`, `data-n-a-ts`, `data-n-a-id` and POSTing a `garturlreq` payload
//! to Google News' `batchexecute` RPC.
//!
//! Best-effort by contract: any failure returns `None` so callers fall back to the
//! original URL. Successful resolutions are memoized (bounded to 2048 entries) and
//! concurrent callers for the same URL coalesce via single-flight without letting
//! one cancelled caller hang or cancel its peers.

use crate::charset::decode_body;
use fsm_core::url_canon::host_of;
use fsm_core::{CallCx, HttpRequest, NetCx, SourceDeps, TransportProfile};
use moka::sync::Cache;
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use regex::Regex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use url::Url;

const BATCHEXECUTE_URL: &str = "https://news.google.com/_/DotsSplashUi/data/batchexecute";
pub const MEMO_CAPACITY: u64 = 2048;

/// Matches Python's `urllib.parse.quote(s)` default safe set: RFC 3986 unreserved + `/`.
const PYTHON_QUOTE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'!')
    .add(b'"')
    .add(b'#')
    .add(b'$')
    .add(b'%')
    .add(b'&')
    .add(b'\'')
    .add(b'(')
    .add(b')')
    .add(b'*')
    .add(b'+')
    .add(b',')
    .add(b':')
    .add(b';')
    .add(b'<')
    .add(b'=')
    .add(b'>')
    .add(b'?')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

static SG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"data-n-a-sg="([^"]+)""#).expect("sg regex"));
static TS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"data-n-a-ts="([^"]+)""#).expect("ts regex"));
static ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"data-n-a-id="([^"]+)""#).expect("id regex"));
static GARTURL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"garturlres\\?",\\?"\s*(https?://[^"\\]+)"#).expect("garturl regex")
});

/// True for `news.google.com` article/read redirect blobs we can resolve.
pub fn is_google_news_url(url: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return false;
    }
    let Some(host) = parsed.host_str() else {
        return false;
    };
    if !matches!(host, "news.google.com" | "www.news.google.com") {
        return false;
    }
    let path = parsed.path();
    path.contains("/articles/") || path.contains("/read/")
}

/// Construct the `f.req` form body for a single `garturlreq` call.
pub fn build_freq(article_id: &str, ts: &str, sig: &str) -> Option<String> {
    let ts_num: i64 = ts.trim().parse().ok()?;
    let inner = json!([
        "garturlreq",
        [
            [
                "X",
                "X",
                ["X", "X"],
                null,
                null,
                1,
                1,
                "US:en",
                null,
                1,
                null,
                null,
                null,
                null,
                null,
                0,
                1
            ],
            "X",
            "X",
            1,
            [1, 1, 1],
            1,
            1,
            null,
            0,
            0,
            null,
            0
        ],
        article_id,
        ts_num,
        sig
    ])
    .to_string();
    let outer = json!([[["Fbv4je", inner, null, "generic"]]]).to_string();
    let encoded = utf8_percent_encode(&outer, PYTHON_QUOTE_SET).to_string();
    Some(format!("f.req={encoded}"))
}

/// Extract the publisher URL from a `batchexecute` RPC response body.
pub fn parse_batchexecute(text: &str) -> Option<String> {
    if text.is_empty() {
        return None;
    }
    let body = text.trim_start_matches([')', ']', '}', '\'']).trim();
    let candidate = body
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with('[') && line.contains("Fbv4je"))
        .unwrap_or(body);

    if let Ok(Value::Array(rows)) = serde_json::from_str::<Value>(candidate) {
        for row in rows {
            if let Value::Array(cols) = row
                && cols.len() > 2
                && cols[1].as_str() == Some("Fbv4je")
                && let Some(raw_inner) = cols[2].as_str()
                && let Ok(Value::Array(payload)) = serde_json::from_str::<Value>(raw_inner)
                && let Some(url) = payload.get(1).and_then(Value::as_str)
                && (url.starts_with("http://") || url.starts_with("https://"))
            {
                return Some(url.to_owned());
            }
        }
    }

    // Fallback for chunked/multiline responses where the structured line scan failed.
    GARTURL_RE
        .captures(text)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_owned())
}

/// State sent over the per-URL single-flight watch channel:
/// `None` = in progress; `Some(result)` = settled.
type FlightSlot = watch::Receiver<Option<Option<String>>>;

/// Drop guard that ensures a cancelled owner cleans up its inflight slot and
/// wakes all coalesced followers with `None` instead of leaving them hung.
struct FlightOwnerGuard {
    url: String,
    inflight: Arc<Mutex<HashMap<String, FlightSlot>>>,
    tx: watch::Sender<Option<Option<String>>>,
    settled: bool,
}

impl FlightOwnerGuard {
    fn settle(mut self, result: Option<String>) {
        self.settled = true;
        let _ = self.tx.send(Some(result));
        if let Ok(mut map) = self.inflight.lock() {
            map.remove(&self.url);
        }
    }
}

impl Drop for FlightOwnerGuard {
    fn drop(&mut self) {
        if !self.settled {
            let _ = self.tx.send(Some(None));
            if let Ok(mut map) = self.inflight.lock() {
                map.remove(&self.url);
            }
        }
    }
}

/// Process-lifetime Google News URL resolver with single-flight coalescing and
/// a bounded 2048-entry memo.
#[derive(Clone)]
pub struct GnewsResolver {
    memo: Cache<String, String>,
    inflight: Arc<Mutex<HashMap<String, FlightSlot>>>,
}

impl Default for GnewsResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl GnewsResolver {
    pub fn new() -> Self {
        Self {
            memo: Cache::builder().max_capacity(MEMO_CAPACITY).build(),
            inflight: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Number of currently in-flight resolutions (primarily for testing).
    pub fn inflight_len(&self) -> usize {
        self.inflight.lock().map(|m| m.len()).unwrap_or(0)
    }

    /// Resolve `url` using the real network stack in `deps`.
    pub async fn resolve(&self, url: &str, deps: &SourceDeps, cx: &CallCx) -> Option<String> {
        let deps = deps.clone();
        let cancel = cx.cancel.clone();
        let remaining = cx.remaining();
        self.resolve_with(url, move |u| {
            let deps = deps.clone();
            let cancel = cancel.clone();
            async move { resolve_over_net(&u, &deps, cancel, remaining).await }
        })
        .await
    }

    /// Generic single-flight + memoized resolver with an injectable async worker
    /// (used directly by offline unit tests for cancellation/coalescing invariants).
    pub async fn resolve_with<F, Fut>(&self, url: &str, worker: F) -> Option<String>
    where
        F: FnOnce(String) -> Fut,
        Fut: Future<Output = Option<String>>,
    {
        if !is_google_news_url(url) {
            return None;
        }
        if let Some(hit) = self.memo.get(url) {
            return Some(hit);
        }

        enum Role {
            Owner(FlightOwnerGuard),
            Follower(FlightSlot),
        }

        let role = {
            let mut map = self.inflight.lock().ok()?;
            if let Some(rx) = map.get(url) {
                Role::Follower(rx.clone())
            } else {
                let (tx, rx) = watch::channel(None);
                map.insert(url.to_owned(), rx);
                Role::Owner(FlightOwnerGuard {
                    url: url.to_owned(),
                    inflight: Arc::clone(&self.inflight),
                    tx,
                    settled: false,
                })
            }
        };

        match role {
            Role::Follower(mut rx) => loop {
                if let Some(settled) = rx.borrow().clone() {
                    return settled;
                }
                if rx.changed().await.is_err() {
                    return None;
                }
            },
            Role::Owner(guard) => {
                let resolved = worker(url.to_owned()).await;
                if let Some(ref pub_url) = resolved {
                    self.memo.insert(url.to_owned(), pub_url.clone());
                }
                guard.settle(resolved.clone());
                resolved
            }
        }
    }
}

async fn resolve_over_net(
    url: &str,
    deps: &SourceDeps,
    cancel: fsm_core::CancellationToken,
    remaining: Duration,
) -> Option<String> {
    if remaining.is_zero() || cancel.is_cancelled() {
        return None;
    }
    let per_req = Duration::from_secs_f64(deps.cfg.request_timeout_s.max(0.5)).min(remaining);
    let mut shell_ncx = NetCx::new(cancel.clone(), per_req);
    let shell_host = host_of(url);
    if !shell_host.is_empty() {
        shell_ncx.host_bucket = Some(format!("googlenews:{shell_host}"));
    }

    // 1. Fetch article shell to extract data-n-a-{sg,ts,id}.
    let shell_req = HttpRequest::get(url)
        .header("Accept-Language", &deps.cfg.accept_language)
        .for_source("googlenews")
        .with_max_bytes(deps.cfg.max_response_bytes);
    let shell_resp = match deps
        .net
        .send(shell_req, TransportProfile::Chrome, &shell_ncx)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("google news shell fetch failed for {url}: {e}");
            return None;
        }
    };
    let ctype = shell_resp.header("content-type").unwrap_or("");
    let html = decode_body(&shell_resp.body, ctype, Some(url));

    let sg = SG_RE.captures(&html)?.get(1)?.as_str();
    let ts = TS_RE.captures(&html)?.get(1)?.as_str();
    let aid = ID_RE.captures(&html)?.get(1)?.as_str();
    let freq = build_freq(aid, ts, sg)?;

    // 2. POST to batchexecute RPC.
    let mut rpc_ncx = NetCx::new(cancel, per_req);
    rpc_ncx.host_bucket = Some("googlenews:news.google.com".to_owned());
    let rpc_req = HttpRequest::post(BATCHEXECUTE_URL, freq)
        .header(
            "Content-Type",
            "application/x-www-form-urlencoded;charset=UTF-8",
        )
        .header("Accept-Language", &deps.cfg.accept_language)
        .for_source("googlenews")
        .with_max_bytes(1024 * 1024);

    let rpc_resp = match deps
        .net
        .send(rpc_req, TransportProfile::Chrome, &rpc_ncx)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("google news batchexecute failed for {url}: {e}");
            return None;
        }
    };
    if rpc_resp.status != 200 {
        return None;
    }
    let rpc_ctype = rpc_resp.header("content-type").unwrap_or("");
    let body = decode_body(&rpc_resp.body, rpc_ctype, Some(BATCHEXECUTE_URL));
    parse_batchexecute(&body)
}
