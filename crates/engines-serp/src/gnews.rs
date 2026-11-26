//! Local lightweight Google News publisher-URL resolver (port of `gnews.py`).
//!
//! `fsm-engines-serp` cannot import `fsm-extract` (review note 6), so
//! `GoogleNewsEngine` uses this self-contained `batchexecute` resolver with
//! bounded 2048-entry FIFO memoization and single-flight coalescing.

use crate::common::decode_body;
use fsm_core::net::{HttpRequest, NetClient, NetCx};
use fsm_core::source::TransportProfile;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use regex::Regex;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, LazyLock, Mutex};
use tokio::sync::broadcast;
use url::Url;

pub const BATCHEXECUTE_URL: &str = "https://news.google.com/_/DotsSplashUi/data/batchexecute";
const MEMO_MAX: usize = 2048;

static SG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"data-n-a-sg="([^"]+)""#).expect("regex"));
static TS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"data-n-a-ts="([^"]+)""#).expect("regex"));
static ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"data-n-a-id="([^"]+)""#).expect("regex"));
static GARTURL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"garturlres\\?",\\?"\s*(https?://[^"\\]+)"#).expect("regex"));

/// True for `news.google.com` article redirect blobs (`/rss/articles/...`,
/// `/articles/...`, `/read/...`).
pub fn is_google_news_url(url: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    if !matches!(host, "news.google.com" | "www.news.google.com") {
        return false;
    }
    let path = parsed.path();
    path.contains("/articles/") || path.contains("/read/")
}

/// Construct the `f.req=` form payload for one `garturlreq` RPC call.
pub fn build_freq(article_id: &str, ts: &str, sig: &str) -> Option<String> {
    let ts_num: i64 = ts.trim().parse().ok()?;
    let inner = serde_json::to_string(&json!([
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
    ]))
    .ok()?;
    let outer = serde_json::to_string(&json!([[["Fbv4je", inner, null, "generic"]]])).ok()?;
    let encoded = utf8_percent_encode(&outer, NON_ALPHANUMERIC).to_string();
    Some(format!("f.req={encoded}"))
}

/// Extract the publisher URL out of a `batchexecute` response body.
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
                && payload.len() > 1
                && let Some(url) = payload[1].as_str()
                && (url.starts_with("http://") || url.starts_with("https://"))
            {
                return Some(url.to_owned());
            }
        }
    }

    // Fallback for multiline/chunked responses where single-line JSON scan fails.
    GARTURL_RE
        .captures(text)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_owned())
}

#[derive(Default)]
struct ResolverState {
    memo: HashMap<String, String>,
    order: VecDeque<String>,
    inflight: HashMap<String, broadcast::Sender<Option<String>>>,
}

/// Bounded memoized + single-flight Google News URL resolver.
#[derive(Clone, Default)]
pub struct GNewsResolver {
    state: Arc<Mutex<ResolverState>>,
}

impl GNewsResolver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Clear memo & inflight state (for tests).
    pub fn clear(&self) {
        let mut st = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        st.memo.clear();
        st.order.clear();
        st.inflight.clear();
    }

    /// Resolve a `news.google.com` article URL to its publisher URL, best effort.
    ///
    /// Returns `None` on non-Google-News URLs, missing params, or network errors.
    /// Coalesces concurrent callers on the same URL without propagating follower
    /// cancellation to the owner or orphaning followers if the owner cancels.
    pub async fn resolve(
        &self,
        url: &str,
        net: &dyn NetClient,
        accept_language: &str,
        ncx: &NetCx,
    ) -> Option<String> {
        if !is_google_news_url(url) {
            return None;
        }
        enum Action {
            Hit(String),
            Wait(broadcast::Receiver<Option<String>>),
            Own(broadcast::Sender<Option<String>>),
        }

        let action = {
            let mut st = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(hit) = st.memo.get(url) {
                Action::Hit(hit.clone())
            } else if let Some(tx) = st.inflight.get(url) {
                Action::Wait(tx.subscribe())
            } else {
                let (tx, _) = broadcast::channel(1);
                st.inflight.insert(url.to_owned(), tx.clone());
                Action::Own(tx)
            }
        };

        match action {
            Action::Hit(hit) => Some(hit),
            Action::Wait(mut rx) => rx.recv().await.ok().flatten(),
            Action::Own(tx) => {
                // Drop guard guarantees inflight cleanup and waiter wakeup even if
                // this future is dropped/cancelled mid-await!
                struct InflightGuard {
                    state: Arc<Mutex<ResolverState>>,
                    url: String,
                    tx: broadcast::Sender<Option<String>>,
                    settled: bool,
                }
                impl Drop for InflightGuard {
                    fn drop(&mut self) {
                        {
                            let mut st = self
                                .state
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            st.inflight.remove(&self.url);
                        }
                        if !self.settled {
                            let _ = self.tx.send(None);
                        }
                    }
                }

                let mut guard = InflightGuard {
                    state: Arc::clone(&self.state),
                    url: url.to_owned(),
                    tx: tx.clone(),
                    settled: false,
                };

                let resolved = resolve_once(url, net, accept_language, ncx).await;
                if let Some(ref pub_url) = resolved {
                    let mut st = self
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if !st.memo.contains_key(url) {
                        if st.memo.len() >= MEMO_MAX
                            && let Some(oldest) = st.order.pop_front()
                        {
                            st.memo.remove(&oldest);
                        }
                        st.order.push_back(url.to_owned());
                    }
                    st.memo.insert(url.to_owned(), pub_url.clone());
                }
                guard.settled = true;
                let _ = tx.send(resolved.clone());
                drop(guard);
                resolved
            }
        }
    }
}

async fn resolve_once(
    url: &str,
    net: &dyn NetClient,
    accept_language: &str,
    ncx: &NetCx,
) -> Option<String> {
    let page_req = HttpRequest::get(url)
        .header("Accept-Language", accept_language)
        .for_source("googlenews");
    let page = net
        .send(page_req, TransportProfile::Chrome, ncx)
        .await
        .ok()?;
    if !page.final_url.is_empty()
        && (page.final_url.starts_with("http://") || page.final_url.starts_with("https://"))
        && !is_google_news_url(&page.final_url)
    {
        return Some(page.final_url);
    }
    let html = decode_body(&page);
    let sg = SG_RE.captures(&html)?.get(1)?.as_str();
    let ts = TS_RE.captures(&html)?.get(1)?.as_str();
    let aid = ID_RE.captures(&html)?.get(1)?.as_str();
    let body = build_freq(aid, ts, sg)?;

    let rpc_req = HttpRequest::post(BATCHEXECUTE_URL, body)
        .header(
            "Content-Type",
            "application/x-www-form-urlencoded;charset=UTF-8",
        )
        .header("Accept-Language", accept_language)
        .for_source("googlenews");
    let resp = net
        .send(rpc_req, TransportProfile::Chrome, ncx)
        .await
        .ok()?;
    if resp.status != 200 {
        return None;
    }
    let text = decode_body(&resp);
    parse_batchexecute(&text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use percent_encoding::percent_decode_str;

    #[test]
    fn test_is_google_news_url_matches_article_blobs() {
        assert!(is_google_news_url(
            "https://news.google.com/rss/articles/CBMiXXXX"
        ));
        assert!(is_google_news_url(
            "https://news.google.com/articles/CBMiYYYY?oc=5"
        ));
        assert!(is_google_news_url("https://news.google.com/read/CBMiZZZZ"));
    }

    #[test]
    fn test_is_google_news_url_rejects_non_news_and_non_article() {
        assert!(!is_google_news_url("https://www.reuters.com/world/x"));
        assert!(!is_google_news_url("https://news.google.com/topics/abc"));
        assert!(!is_google_news_url("https://example.com/rss/articles/CBM"));
        assert!(!is_google_news_url("not-a-url"));
    }

    #[test]
    fn test_build_freq_encodes_id_ts_sig() {
        let freq = build_freq("ARTICLE_ID", "1700000000", "SIGNATURE").unwrap();
        assert!(freq.starts_with("f.req="));
        let decoded = percent_decode_str(&freq["f.req=".len()..])
            .decode_utf8_lossy()
            .into_owned();
        assert!(decoded.contains("Fbv4je"));
        assert!(decoded.contains("garturlreq"));
        assert!(decoded.contains("ARTICLE_ID"));
        assert!(decoded.contains("SIGNATURE"));
        assert!(decoded.contains("1700000000"));
    }

    #[test]
    fn test_parse_batchexecute_extracts_publisher_url() {
        let inner = serde_json::to_string(&json!([
            "garturlres",
            "https://www.reuters.com/world/story-2026",
            1
        ]))
        .unwrap();
        let body = format!(
            ")]}}'\n\n{}",
            serde_json::to_string(&json!([
                ["wrb.fr", "Fbv4je", inner, null, null, null, "generic"],
                ["di", 13]
            ]))
            .unwrap()
        );
        assert_eq!(
            parse_batchexecute(&body).as_deref(),
            Some("https://www.reuters.com/world/story-2026")
        );
    }

    #[test]
    fn test_parse_batchexecute_returns_none_on_garbage() {
        assert_eq!(parse_batchexecute(""), None);
        assert_eq!(parse_batchexecute(")]}'\n\nnot json at all"), None);
        assert_eq!(
            parse_batchexecute(&format!(")]}}'\n{}", json!([["di", 13]]))),
            None
        );
    }

    #[test]
    fn test_parse_batchexecute_handles_multiline_and_length_prefix() {
        let body = ")]}'\n\n359\n[[\"wrb.fr\",\"Fbv4je\",\n  \"[\\\"garturlres\\\",\\\"https://www.bbc.com/news/articles/abc123\\\",1]\",\n  null,null,null,\"generic\"]]\n";
        assert_eq!(
            parse_batchexecute(body).as_deref(),
            Some("https://www.bbc.com/news/articles/abc123")
        );
    }
}
