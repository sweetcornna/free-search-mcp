//! `fsm-browser` — browser rendering (owner: lane `W4b`, `SPEC-lanes.md`).
//!
//! 1.0 ships the trait and a stub only (`SPEC.md` §7.3, M9: no `chromey` in the
//! active lock). Every render attempt fails with
//! [`FsmError::BrowserUnavailable`] naming what to do instead.
//!
//! P1 contract for a real implementation (CDP via `chromey`), binding on
//! whoever replaces [`BrowserStub`]:
//! - enable `Fetch.enable` and run the SSRF check
//!   ([`fsm_core::NetClient::check_url_offline`] plus DNS pinning) on EVERY
//!   sub-resource, JS-initiated request and redirect (I14.4), failing the
//!   request, not just the navigation;
//! - one browser per process behind a semaphore, with an `fd-lock` lease on
//!   its private `user_data_dir` (never the user's own Chrome profile);
//! - honour [`NetCx::cancel`] and [`NetCx::timeout`] and kill the browser
//!   process tree on either.

use async_trait::async_trait;
use fsm_core::{Config, FsmError, NetCx};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// What a caller asks a renderer for.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct RenderRequest {
    /// Absolute `http(s)` URL to load.
    pub url: String,
    /// CSS selector to wait for before taking the DOM, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_for: Option<String>,
    /// Upper bound on the serialized DOM handed back, in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
}

impl RenderRequest {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Self::default()
        }
    }
}

/// A rendered page: the DOM after scripts ran.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct RenderedPage {
    pub final_url: String,
    pub status: u16,
    pub html: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

impl RenderedPage {
    pub fn new(final_url: impl Into<String>, status: u16, html: impl Into<String>) -> Self {
        Self {
            final_url: final_url.into(),
            status,
            html: html.into(),
            ..Self::default()
        }
    }
}

/// Whether browser rendering can be used, and why not (for `doctor` and
/// for the one-time `FETCH_STRATEGY=browser` notice).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct BrowserStatus {
    pub available: bool,
    pub reason: String,
}

/// A page renderer (P1: Chrome DevTools Protocol).
#[async_trait]
pub trait BrowserRenderer: Send + Sync {
    /// Cheap, side-effect-free availability probe.
    fn status(&self) -> BrowserStatus;

    /// Load `req.url` in a browser and return the rendered DOM.
    async fn render(&self, req: &RenderRequest, cx: &NetCx) -> Result<RenderedPage, FsmError>;
}

/// Why rendering is unavailable in 1.0, with what to do instead.
pub const UNAVAILABLE_REASON: &str = "browser rendering is a P1 feature and is not built into \
free-search-mcp 1.0. Use render: \"http\" (or \"auto\", the default) instead: it covers \
server-rendered pages, and JSON/RSS/sitemap endpoints of a JavaScript-heavy site usually carry \
the same data. When browser rendering ships it will drive a locally installed Google Chrome, \
Chromium or Microsoft Edge over CDP; SEARCH_MCP_FETCH_STRATEGY=browser and the \
SEARCH_MCP_BROWSER_* settings are accepted today but inactive.";

/// The 1.0 renderer: every call fails with [`FsmError::BrowserUnavailable`].
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct BrowserStub;

impl BrowserStub {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl BrowserRenderer for BrowserStub {
    fn status(&self) -> BrowserStatus {
        BrowserStatus {
            available: false,
            reason: UNAVAILABLE_REASON.to_owned(),
        }
    }

    async fn render(&self, req: &RenderRequest, _cx: &NetCx) -> Result<RenderedPage, FsmError> {
        Err(unavailable(&req.url))
    }
}

/// The error every 1.0 render attempt returns.
pub fn unavailable(url: &str) -> FsmError {
    if url.is_empty() {
        FsmError::BrowserUnavailable(UNAVAILABLE_REASON.to_owned())
    } else {
        FsmError::BrowserUnavailable(format!("cannot render {url}: {UNAVAILABLE_REASON}"))
    }
}

/// The renderer the server should install for this configuration. In 1.0
/// this is always the stub, whatever `fetch_strategy` says.
pub fn renderer_for(_cfg: &Config) -> Arc<dyn BrowserRenderer> {
    Arc::new(BrowserStub::new())
}

/// Startup notice when the configuration asks for a browser it cannot have
/// (`SEARCH_MCP_FETCH_STRATEGY=browser`); `None` otherwise.
pub fn config_notice(cfg: &Config) -> Option<String> {
    cfg.fetch_strategy
        .eq_ignore_ascii_case("browser")
        .then(|| format!("SEARCH_MCP_FETCH_STRATEGY=browser is inactive: {UNAVAILABLE_REASON}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stub_render_fails_with_browser_unavailable_naming_options() {
        let r = renderer_for(&Config::default());
        let err = r
            .render(
                &RenderRequest::new("https://example.org/app"),
                &NetCx::default(),
            )
            .await
            .expect_err("stub never renders");
        assert_eq!(err.kind(), "BrowserUnavailable");
        let msg = err.to_string();
        assert!(
            msg.starts_with("Browser unavailable: cannot render https://example.org/app"),
            "{msg}"
        );
        for hint in ["render: \"http\"", "Chrome", "Chromium", "Edge", "P1"] {
            assert!(msg.contains(hint), "missing {hint:?} in {msg}");
        }
        assert!(!err.is_internal());
    }

    #[test]
    fn stub_status_reports_unavailable() {
        let s = BrowserStub::new().status();
        assert!(!s.available);
        assert_eq!(s.reason, UNAVAILABLE_REASON);
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["available"], false);
        assert_eq!(
            unavailable("").to_string(),
            format!("Browser unavailable: {UNAVAILABLE_REASON}")
        );
    }

    #[test]
    fn config_notice_only_for_browser_strategy() {
        let mut cfg = Config::default();
        assert_eq!(config_notice(&cfg), None);
        cfg.fetch_strategy = "Browser".into();
        let n = config_notice(&cfg).expect("notice");
        assert!(
            n.contains("inactive") && n.contains("FETCH_STRATEGY"),
            "{n}"
        );
        cfg.fetch_strategy = "http".into();
        assert_eq!(config_notice(&cfg), None);
    }

    #[test]
    fn request_and_page_wire_shapes_are_camel_case() {
        let mut req = RenderRequest::new("https://x.example/");
        req.wait_for = Some("main".into());
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"url": "https://x.example/", "waitFor": "main"})
        );
        let page = RenderedPage::new("https://x.example/", 200, "<html></html>");
        let v = serde_json::to_value(&page).unwrap();
        assert_eq!(v["finalUrl"], "https://x.example/");
        assert!(v.get("title").is_none());
    }
}
