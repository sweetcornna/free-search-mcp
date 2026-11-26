//! Network client trait (`SPEC-contracts.md` §1.3). Implemented by
//! `fsm-net` (live) and `fsm_net::replay::ReplayNet` (fixtures).

use crate::error::FsmError;
use crate::source::TransportProfile;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// HTTP method (`"GET" | "POST" | "HEAD"` on the wire and in manifests).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "UPPERCASE")]
#[non_exhaustive]
pub enum HttpMethod {
    #[default]
    Get,
    Post,
    Head,
}

impl HttpMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Head => "HEAD",
        }
    }
}

/// One outbound request.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct HttpRequest {
    pub method: HttpMethod,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub query: Vec<(String, String)>,
    pub body: Option<bytes::Bytes>,
    pub max_bytes: Option<u64>,
    pub follow_redirects: bool,
    pub cookie_jar_id: Option<String>,
    pub source_id: Option<String>,
    /// Query keys carrying credentials (A1 §B.9): stripped by `xtask record`
    /// and ignored by `ReplayNet` matching (case-insensitive).
    pub redact_query_keys: Vec<String>,
    /// Headers carrying credentials (A1 §B.9), same treatment.
    pub redact_headers: Vec<String>,
}

impl HttpRequest {
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: HttpMethod::Get,
            url: url.into(),
            follow_redirects: true,
            ..Self::default()
        }
    }

    pub fn post(url: impl Into<String>, body: impl Into<bytes::Bytes>) -> Self {
        Self {
            method: HttpMethod::Post,
            url: url.into(),
            body: Some(body.into()),
            follow_redirects: true,
            ..Self::default()
        }
    }

    pub fn head(url: impl Into<String>) -> Self {
        Self {
            method: HttpMethod::Head,
            url: url.into(),
            follow_redirects: true,
            ..Self::default()
        }
    }

    /// Builder: append a header.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Builder: append a query parameter.
    pub fn query_param(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.query.push((name.into(), value.into()));
        self
    }

    /// Builder: attribute the request to a source (rate limits, health, proxy scoping).
    pub fn for_source(mut self, source_id: impl Into<String>) -> Self {
        self.source_id = Some(source_id.into());
        self
    }

    /// Builder: mark a query key as a credential (A1 §B.9).
    pub fn redact_query_key(mut self, name: impl Into<String>) -> Self {
        self.redact_query_keys.push(name.into());
        self
    }

    /// Builder: mark a header as a credential (A1 §B.9).
    pub fn redact_header(mut self, name: impl Into<String>) -> Self {
        self.redact_headers.push(name.into());
        self
    }

    /// Whether `name` is a redacted query key (case-insensitive).
    pub fn is_redacted_query_key(&self, name: &str) -> bool {
        self.redact_query_keys
            .iter()
            .any(|k| k.eq_ignore_ascii_case(name))
    }

    /// Whether `name` is a redacted header (case-insensitive).
    pub fn is_redacted_header(&self, name: &str) -> bool {
        self.redact_headers
            .iter()
            .any(|k| k.eq_ignore_ascii_case(name))
    }

    /// Builder: cap the (decompressed) body size.
    pub fn with_max_bytes(mut self, max: u64) -> Self {
        self.max_bytes = Some(max);
        self
    }
}

/// A response with its raw body.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct HttpBytes {
    pub final_url: String,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: bytes::Bytes,
    pub truncated: bool,
}

impl HttpBytes {
    pub fn new(
        final_url: impl Into<String>,
        status: u16,
        headers: Vec<(String, String)>,
        body: impl Into<bytes::Bytes>,
    ) -> Self {
        Self {
            final_url: final_url.into(),
            status,
            headers,
            body: body.into(),
            truncated: false,
        }
    }

    /// First header value with this name (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The `Content-Type` media type, lower-cased, without parameters.
    pub fn media_type(&self) -> Option<String> {
        self.header("content-type").map(|v| {
            v.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// Per-request network context.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct NetCx {
    pub cancel: CancellationToken,
    pub timeout: Duration,
    pub host_bucket: Option<String>,
    pub rps: Option<f64>,
}

impl NetCx {
    pub fn new(cancel: CancellationToken, timeout: Duration) -> Self {
        Self {
            cancel,
            timeout,
            host_bucket: None,
            rps: None,
        }
    }
}

/// The only way any crate reaches the network.
#[async_trait::async_trait]
pub trait NetClient: Send + Sync {
    async fn send(
        &self,
        req: HttpRequest,
        profile: TransportProfile,
        cx: &NetCx,
    ) -> Result<HttpBytes, FsmError>;
    /// SSRF layers 1–3 (no DNS): run before serving anything from cache.
    fn check_url_offline(&self, url: &str) -> Result<(), FsmError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructors_and_helpers() {
        let r = HttpRequest::get("https://x/")
            .header("Accept", "application/json")
            .query_param("q", "a b")
            .for_source("arxiv");
        assert!(r.follow_redirects && r.method == HttpMethod::Get);
        assert_eq!(r.query, vec![("q".to_owned(), "a b".to_owned())]);
        assert_eq!(r.source_id.as_deref(), Some("arxiv"));
        let p = HttpRequest::post("https://x/", bytes::Bytes::from_static(b"{}"));
        assert_eq!(p.method.as_str(), "POST");
        let b = HttpBytes::new(
            "https://x/",
            200,
            vec![(
                "Content-Type".into(),
                "application/JSON; charset=utf-8".into(),
            )],
            bytes::Bytes::new(),
        );
        assert_eq!(b.media_type().as_deref(), Some("application/json"));
        assert!(b.is_success() && !b.truncated);
        assert_eq!(serde_json::to_value(HttpMethod::Head).unwrap(), "HEAD");
    }
}
