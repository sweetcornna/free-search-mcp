//! `xtask record` & `xtask probe` live HTTP client (`fsm_net::HttpClient`) + fixture
//! recorder (`RecordingNet`) with full A1 §B.9 & M7 secret redaction.
//!
//! Uses `fsm_net::HttpClient` directly (4-layer SSRF guard across all redirects,
//! zero CLI argument secret exposure, zero temp files on disk).

use anyhow::{Context, Result};
use fsm_core::fixture::{Exchange, Fixture, FixtureRequest, FixtureResponse, canonical_json};
use fsm_core::{Config, FsmError, HttpBytes, HttpRequest, NetClient, NetCx, TransportProfile};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use url::form_urlencoded;

/// Default request headers kept in recorded `headersSubset` (all others omitted,
/// and all `req.redact_headers` are always stripped per A1 §B.9 #2).
pub const RECORDED_HEADER_ALLOWLIST: &[&str] = &["accept", "content-type"];

/// Build the production 4-layer SSRF-guarded live HTTP client (`fsm_net::HttpClient`).
pub fn build_live_http_client(cfg: &Config) -> Result<Arc<fsm_net::HttpClient>> {
    fsm_net::install_ring_provider();
    let client =
        fsm_net::HttpClient::new(cfg).context("failed to initialize fsm_net::HttpClient")?;
    Ok(Arc::new(client))
}

/// Build the production 4-layer SSRF-guarded live HTTP client as `Arc<dyn NetClient>`.
pub fn build_live_net(cfg: &Config) -> Result<Arc<dyn NetClient>> {
    Ok(build_live_http_client(cfg)?)
}

/// Wraps any [`NetClient`] and records every HTTP exchange with secret redaction
/// (A1 §B.9 #2: strips `req.redact_query_keys` and `req.redact_headers`, plus optional
/// `AuthIn::Body` field stripping per M7).
pub struct RecordingNet {
    inner: Arc<dyn NetClient>,
    redact_body_key: Option<String>,
    exchanges: Mutex<Vec<Exchange>>,
}

impl RecordingNet {
    #[allow(dead_code)]
    pub fn new(inner: Arc<dyn NetClient>) -> Self {
        Self::with_body_redaction(inner, None)
    }

    pub fn with_body_redaction(inner: Arc<dyn NetClient>, redact_body_key: Option<String>) -> Self {
        Self {
            inner,
            redact_body_key,
            exchanges: Mutex::new(Vec::new()),
        }
    }

    pub fn take_exchanges(&self) -> Vec<Exchange> {
        let mut guard = self
            .exchanges
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *guard)
    }

    pub fn take_fixture(&self, source: &str, op: &str, recorded_at: &str) -> Fixture {
        let mut f = Fixture::new(source, op, recorded_at);
        f.exchanges = self.take_exchanges();
        f
    }
}

fn redact_body_for_fixture(req: &HttpRequest, key: &str) -> HttpRequest {
    let mut cloned = req.clone();
    let Some(body) = cloned.body.as_deref() else {
        return cloned;
    };
    let ct = cloned
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v.to_ascii_lowercase())
        .unwrap_or_default();

    if ct.contains("json")
        && let Ok(mut val) = serde_json::from_slice::<serde_json::Value>(body)
        && let Some(obj) = val.as_object_mut()
    {
        obj.remove(key);
        cloned.body = Some(canonical_json(&val).into_bytes().into());
    } else if ct.contains("x-www-form-urlencoded") {
        let mut ser = form_urlencoded::Serializer::new(String::new());
        for (k, v) in form_urlencoded::parse(body) {
            if k != key {
                ser.append_pair(&k, &v);
            }
        }
        cloned.body = Some(ser.finish().into_bytes().into());
    }
    cloned
}

impl NetClient for RecordingNet {
    fn send<'life0, 'life1, 'async_trait>(
        &'life0 self,
        req: HttpRequest,
        profile: TransportProfile,
        cx: &'life1 NetCx,
    ) -> Pin<Box<dyn Future<Output = Result<HttpBytes, FsmError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let recorded_req = match self.redact_body_key.as_deref() {
                Some(bk) => {
                    let sanitized = redact_body_for_fixture(&req, bk);
                    FixtureRequest::from_http(&sanitized, RECORDED_HEADER_ALLOWLIST)
                }
                None => FixtureRequest::from_http(&req, RECORDED_HEADER_ALLOWLIST),
            };
            let resp = self.inner.send(req, profile, cx).await?;
            let recorded_resp =
                FixtureResponse::from_bytes(resp.status, resp.headers.clone(), &resp.body);
            let mut guard = self
                .exchanges
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.push(Exchange::new(recorded_req, recorded_resp));
            Ok(resp)
        })
    }

    fn check_url_offline(&self, url: &str) -> Result<(), FsmError> {
        self.inner.check_url_offline(url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    struct EchoNet;

    impl NetClient for EchoNet {
        fn send<'life0, 'life1, 'async_trait>(
            &'life0 self,
            req: HttpRequest,
            _profile: TransportProfile,
            _cx: &'life1 NetCx,
        ) -> Pin<Box<dyn Future<Output = Result<HttpBytes, FsmError>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move {
                Ok(HttpBytes::new(
                    req.url,
                    200,
                    vec![("content-type".into(), "application/json".into())],
                    Bytes::from_static(b"{\"ok\":true}"),
                ))
            })
        }

        fn check_url_offline(&self, _url: &str) -> Result<(), FsmError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn recording_net_redacts_auth_query_keys_headers_and_body_key() {
        let rec = RecordingNet::with_body_redaction(Arc::new(EchoNet), Some("api_key".into()));
        let mut req = HttpRequest::post(
            "https://api.example.org/items",
            Bytes::from_static(br#"{"api_key":"SUPER_SECRET_BODY","q":"rust"}"#),
        );
        req.query.push(("q".into(), "rust".into()));
        req.query
            .push(("api_key".into(), "SUPER_SECRET_QUERY".into()));
        req.headers
            .push(("Accept".into(), "application/json".into()));
        req.headers
            .push(("Content-Type".into(), "application/json".into()));
        req.headers
            .push(("Authorization".into(), "Bearer SUPER_SECRET_HDR".into()));
        req.redact_query_keys.push("api_key".into());
        req.redact_headers.push("Authorization".into());

        let cx = NetCx::default();
        rec.send(req, TransportProfile::Plain, &cx).await.unwrap();

        let fx = rec.take_fixture("demo", "search", "2026-11-25T00:00:00Z");
        let serialized = fx.to_json();
        assert!(
            !serialized.contains("SUPER_SECRET_QUERY"),
            "redacted query secret leaked into fixture: {serialized}"
        );
        assert!(
            !serialized.contains("SUPER_SECRET_HDR"),
            "redacted header secret leaked into fixture: {serialized}"
        );
        assert_eq!(
            fx.exchanges[0].request.url,
            "https://api.example.org/items?q=rust"
        );
        assert_eq!(
            fx.exchanges[0]
                .request
                .headers_subset
                .get("accept")
                .map(String::as_str),
            Some("application/json")
        );
        assert!(
            !fx.exchanges[0]
                .request
                .headers_subset
                .contains_key("authorization")
        );

        // bodySha256 matches the body WITHOUT the `api_key` field (M7).
        let unauth_req = HttpRequest::post(
            "https://api.example.org/items",
            Bytes::from_static(br#"{"q":"rust"}"#),
        );
        let expected_sha =
            FixtureRequest::from_http(&unauth_req, RECORDED_HEADER_ALLOWLIST).body_sha256;
        assert_eq!(fx.exchanges[0].request.body_sha256, expected_sha);
    }

    #[test]
    fn live_net_blocks_private_and_link_local_ips() {
        let net = build_live_net(&Config::default()).expect("build HttpClient");
        for blocked in [
            "http://169.254.169.254/latest/meta-data/",
            "http://10.0.0.1/admin",
            "http://172.16.0.1/",
            "http://192.168.1.1/",
            "http://127.0.0.2/",
            "http://0.0.0.0/",
            "http://[::ffff:127.0.0.1]/",
        ] {
            assert!(
                net.check_url_offline(blocked).is_err(),
                "expected SSRF block for {blocked}"
            );
        }
    }
}
