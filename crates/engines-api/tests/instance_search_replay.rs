//! Synthetic offline fixtures; no public instance or credentials are used.

use async_trait::async_trait;
use fsm_core::api::Freshness;
use fsm_core::fixture::{Exchange, Fixture, FixtureRequest, FixtureResponse, ReplayNet};
use fsm_core::{
    Config, FsmError, HttpBytes, HttpRequest, NetClient, NetCx, NullStore, PoolRole, Source,
    SourceDeps, SrcCx, SrcOutput, TransportProfile,
};
use fsm_engines_api::{degoog::DegoogEngine, searxng::SearxngEngine};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Capture {
    replay: ReplayNet,
    requests: Mutex<Vec<HttpRequest>>,
}

#[async_trait]
impl NetClient for Capture {
    async fn send(
        &self,
        req: HttpRequest,
        profile: TransportProfile,
        cx: &NetCx,
    ) -> Result<HttpBytes, FsmError> {
        self.requests.lock().unwrap().push(req.clone());
        self.replay.send(req, profile, cx).await
    }

    fn check_url_offline(&self, url: &str) -> Result<(), FsmError> {
        fsm_net::ssrf::check_offline(url, false).map(|_| ())
    }
}

fn endpoint(degoog: bool) -> &'static str {
    if degoog {
        "https://search.example/api/search?q=rust&type=web"
    } else {
        "https://search.example/search?q=rust&format=json"
    }
}

fn setup(
    degoog: bool,
    base: &str,
    key: Option<&str>,
    status: u16,
    body: &str,
) -> (Box<dyn Source>, Arc<Capture>, SrcCx) {
    setup_with_endpoint(degoog, base, key, status, body, endpoint(degoog))
}

fn setup_with_endpoint(
    degoog: bool,
    base: &str,
    key: Option<&str>,
    status: u16,
    body: &str,
    endpoint: &str,
) -> (Box<dyn Source>, Arc<Capture>, SrcCx) {
    let id = if degoog { "degoog" } else { "searxng" };
    let mut fixture = Fixture::new(id, "search", "2026-11-27T00:00:00Z");
    fixture.exchanges.push(Exchange::new(
        FixtureRequest::new("GET", endpoint),
        FixtureResponse::from_bytes(status, vec![], body.as_bytes()),
    ));
    let net = Arc::new(Capture {
        replay: ReplayNet::from_fixtures(vec![fixture]).unwrap(),
        requests: Mutex::new(vec![]),
    });
    let mut cfg = Config::default();
    if let Some(key) = key {
        let env = if degoog {
            "SEARCH_MCP_DEGOOG_API_KEY"
        } else {
            "SEARCH_MCP_SEARXNG_TOKEN"
        };
        cfg.api_keys.insert(env.into(), key.into());
    }
    let cx = SrcCx::from_config(&cfg, Default::default(), Duration::from_secs(5));
    let deps = SourceDeps::new(net.clone(), Arc::new(NullStore::new()), Arc::new(cfg));
    let engine: Box<dyn Source> = if degoog {
        Box::new(DegoogEngine::new(deps, base.into()))
    } else {
        Box::new(SearxngEngine::new(deps, base.into()))
    };
    (engine, net, cx)
}

#[tokio::test]
async fn both_instances_replay_keyless_and_ignore_endpoint_overrides() {
    for degoog in [false, true] {
        let (engine, net, mut cx) = setup(
            degoog,
            "https://search.example/",
            None,
            200,
            r#"{"results":[{"url":"https://rust-lang.org/","title":"<b>Rust</b>","content":"Safe &amp; fast"},{"url":"https://rust-lang.org/"},{"url":"javascript:alert(1)"}]}"#,
        );
        cx.api_key = Some("unrelated-context-key".into());
        assert_eq!(engine.decl().pool_role, PoolRole::Explicit);
        let out = engine
            .call(
                "search",
                json!({"query":"rust", "base_url":"https://evil.example", "api_key":"ignored"}),
                &cx,
            )
            .await
            .unwrap();
        let SrcOutput::Hits(hits) = out else {
            panic!("expected hits")
        };
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Rust");
        assert_eq!(hits[0].snippet, "Safe & fast");
        let requests = net.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(!requests[0].follow_redirects);
        assert!(
            !requests[0]
                .headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("authorization"))
        );
    }
}

#[tokio::test]
async fn credentials_are_config_only_redacted_and_degoog_accepts_api_root() {
    for degoog in [false, true] {
        let base = if degoog {
            "https://search.example/api/"
        } else {
            "https://search.example"
        };
        let (engine, net, cx) = setup(
            degoog, base, Some("synthetic-token"), 200, r#"{"results":[]}"#,
        );
        engine.call("search", json!({"query":"rust"}), &cx).await.unwrap();
        let requests = net.requests.lock().unwrap();
        assert!(requests[0].is_redacted_header("Authorization"));
        assert!(requests[0].headers.iter().any(|(k, v)| {
            k == "Authorization" && v == "Bearer synthetic-token"
        }));
        assert!(!requests[0].follow_redirects);
    }
}

#[tokio::test]
async fn unsafe_endpoints_fail_before_send() {
    for degoog in [false, true] {
        for base in [
            "http://search.example",
            "https://user:secret@search.example",
            "https://search.example/#fragment",
            "https://search.example/?token=secret",
            "https://127.0.0.1",
            "file:///tmp/search",
        ] {
            let (engine, net, cx) = setup(degoog, base, Some("synthetic-token"), 200, "{}");
            assert!(engine.call("search", json!({"query":"rust"}), &cx).await.is_err());
            assert!(net.requests.lock().unwrap().is_empty());
        }
    }
}

#[tokio::test]
async fn upstream_errors_and_malformed_responses_do_not_echo_body() {
    for degoog in [false, true] {
        for (status, body) in [
            (302, "synthetic-token"),
            (401, "synthetic-token"),
            (429, "synthetic-token"),
            (200, "synthetic-token"),
            (200, "{}"),
            (200, r#"{"results":null}"#),
        ] {
            let (engine, net, cx) = setup(
                degoog, "https://search.example", Some("synthetic-token"), status, body,
            );
            let err = engine.call("search", json!({"query":"rust"}), &cx).await.unwrap_err();
            assert!(!err.to_string().contains("synthetic-token"));
            if status == 429 {
                assert!(matches!(err, FsmError::RateLimited { .. }));
            }
            assert_eq!(net.requests.lock().unwrap().len(), 1);
        }
    }
}

#[tokio::test]
async fn cancellation_and_invalid_operations_do_not_send() {
    for degoog in [false, true] {
        let (engine, net, cx) = setup(
            degoog, "https://search.example", None, 200, r#"{"results":[]}"#,
        );
        assert!(engine.call("unknown", json!({"query":"rust"}), &cx).await.is_err());
        assert!(engine.call("search", json!({}), &cx).await.is_err());
        cx.cancel.cancel();
        assert!(matches!(
            engine.call("search", json!({"query":"rust"}), &cx).await,
            Err(FsmError::Timeout(0))
        ));
        assert!(net.requests.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn domain_filters_apply_before_limit_and_context_overrides_params() {
    let body = r#"{"results":[
        {"url":"https://unrelated.example/"},
        {"url":"https://blocked.rust-lang.org/"},
        {"url":"https://rust-lang.org.evil.example/"},
        {"url":"https://doc.rust-lang.org/book/"},
        {"url":"https://rust-lang.org/"}
    ]}"#;
    for degoog in [false, true] {
        for from_context in [false, true] {
            let (engine, net, mut cx) = setup(
                degoog, "https://search.example", None, 200, body,
            );
            let mut params = json!({
                "query": "rust", "limit": 1,
                "include_domains": ["rust-lang.org"],
                "exclude_domains": ["blocked.rust-lang.org"]
            });
            if from_context {
                cx.domains = vec!["rust-lang.org".into(), "-blocked.rust-lang.org".into()];
                params["include_domains"] = json!(["unrelated.example"]);
                params["exclude_domains"] = json!(["doc.rust-lang.org"]);
            }
            let out = engine.call("search", params, &cx).await.unwrap();
            let SrcOutput::Hits(hits) = out else {
                panic!("expected hits")
            };
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].url, "https://doc.rust-lang.org/book/");
            assert_eq!(net.requests.lock().unwrap().len(), 1);
        }
    }
}

#[tokio::test]
async fn freshness_parameters_match_each_instance_and_context_takes_precedence() {
    for degoog in [false, true] {
        for (name, freshness) in [
            ("day", Freshness::Day),
            ("week", Freshness::Week),
            ("month", Freshness::Month),
            ("year", Freshness::Year),
        ] {
            for from_context in [false, true] {
                let parameter = if degoog { "time" } else { "time_range" };
                // SearXNG has no year range; do not send an unsupported value.
                let expected = if degoog || name != "year" { Some(name) } else { None };
                let url = match expected {
                    Some(value) => format!("{}&{parameter}={value}", endpoint(degoog)),
                    None => endpoint(degoog).to_owned(),
                };
                let (engine, net, mut cx) = setup_with_endpoint(
                    degoog, "https://search.example", None, 200, r#"{"results":[]}"#, &url,
                );
                let mut params = json!({"query":"rust", "freshness":name});
                if from_context {
                    cx.freshness = Some(freshness);
                    params["freshness"] = json!(if name == "day" { "month" } else { "day" });
                }
                engine.call("search", params, &cx).await.unwrap();
                let requests = net.requests.lock().unwrap();
                assert_eq!(requests.len(), 1);
                let actual = requests[0].query.iter()
                    .find(|(key, _)| key == parameter)
                    .map(|(_, value)| value.as_str());
                assert_eq!(actual, expected);
                let other = if degoog { "time_range" } else { "time" };
                assert!(!requests[0].query.iter().any(|(key, _)| key == other));
            }
        }
    }
}
