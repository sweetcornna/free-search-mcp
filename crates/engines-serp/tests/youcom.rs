//! Port of `tests/test_youcom.py`: JSON-RPC body construction, region mapping,
//! include/exclude mutual exclusion, SSE & plain JSON parsing, snippet cap,
//! error handling (tool error, JSON-RPC error, HTTP 429), and malformed replies.

use async_trait::async_trait;
use fsm_core::api::Freshness;
use fsm_core::config::{Config, SafeSearch};
use fsm_core::net::{HttpBytes, HttpRequest, NetClient, NetCx};
use fsm_core::source::{PoolRole, Source, SrcCx, SrcOutput, TransportProfile};
use fsm_core::types::HitKind;
use fsm_core::{FsmError, NullStore, SourceDeps};
use fsm_engines_serp::common::SerpQuery;
use fsm_engines_serp::engines::youcom::{
    ENDPOINT, SNIPPET_CAP, YouComEngine, build_request_body, region_meta,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn sample_search_json() -> Value {
    json!({
        "results": {
            "web": [
                {
                    "url": "https://tokio.rs/",
                    "title": "Tokio - An asynchronous Rust runtime",
                    "description": "Tokio is a runtime for writing reliable asynchronous applications.",
                    "snippets": ["a longer passage that must not win over description"],
                    "page_age": "2021-07-30T00:00:00"
                },
                {
                    "url": "https://github.com/tokio-rs/tokio",
                    "title": "tokio-rs/tokio",
                    "description": "",
                    "snippets": ["A runtime for writing reliable asynchronous applications", "2"]
                }
            ],
            "news": [
                {
                    "url": "https://news.example.com/tokio-1-50",
                    "title": "Tokio 1.50 released",
                    "description": "The release adds ...",
                    "page_age": "2026-10-16T09:00:00"
                }
            ]
        },
        "metadata": {"query": "tokio runtime"}
    })
}

fn rpc_result(payload: Value, is_error: bool) -> Value {
    let text = match payload {
        Value::String(s) => s,
        other => serde_json::to_string(&other).unwrap(),
    };
    let mut res = json!({"content": [{"type": "text", "text": text}]});
    if is_error {
        res["isError"] = json!(true);
    }
    json!({"jsonrpc": "2.0", "id": 1, "result": res})
}

fn sse(messages: &[Value]) -> String {
    messages
        .iter()
        .map(|m| {
            format!(
                "event: message\ndata: {}\n\n",
                serde_json::to_string(m).unwrap()
            )
        })
        .collect()
}

fn notice() -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/message",
        "params": {"level": "info", "data": "Search successful"}
    })
}

struct MockYouNet {
    status: u16,
    body: String,
    seen_body: Mutex<Option<String>>,
}

impl MockYouNet {
    fn new(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
            seen_body: Mutex::new(None),
        }
    }
}

#[async_trait]
impl NetClient for MockYouNet {
    async fn send(
        &self,
        req: HttpRequest,
        _profile: TransportProfile,
        _cx: &NetCx,
    ) -> Result<HttpBytes, FsmError> {
        assert_eq!(req.url, ENDPOINT);
        if let Some(b) = &req.body {
            *self.seen_body.lock().unwrap() = Some(String::from_utf8_lossy(b).into_owned());
        }
        Ok(HttpBytes::new(
            req.url,
            self.status,
            vec![],
            self.body.clone(),
        ))
    }

    fn check_url_offline(&self, _url: &str) -> Result<(), FsmError> {
        Ok(())
    }
}

fn mk_q(
    query: &str,
    limit: u32,
    freshness: Option<Freshness>,
    domains: Vec<String>,
    filetype: Option<&str>,
    region: &str,
    safesearch: SafeSearch,
) -> SerpQuery {
    let mut cx = SrcCx::from_config(
        &Config::default(),
        CancellationToken::new(),
        Duration::from_secs(5),
    );
    cx.freshness = freshness;
    cx.domains = domains;
    cx.region = region.to_owned();
    cx.safesearch = safesearch;
    let mut p = json!({"query": query, "limit": limit});
    if let Some(ft) = filetype {
        p["filetype"] = json!(ft);
    }
    SerpQuery::from_call("youcom", "search", &p, &cx, &Config::default()).unwrap()
}

fn default_cx() -> SrcCx {
    SrcCx::from_config(
        &Config::default(),
        CancellationToken::new(),
        Duration::from_secs(5),
    )
}

#[test]
fn test_registered_reserve_and_keyless() {
    let deps = SourceDeps::new(
        Arc::new(MockYouNet::new(200, "")),
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    );
    let e = YouComEngine::new(deps);
    assert_eq!(e.decl().id, "youcom");
    assert_eq!(e.decl().pool_role, PoolRole::Reserve);
    assert_eq!(e.decl().key_env(), None);
}

#[test]
fn test_request_carries_query_count_safesearch_and_freshness() {
    let body = build_request_body(&mk_q(
        "tokio",
        99,
        Some(Freshness::Week),
        vec![],
        None,
        "us-en",
        SafeSearch::Strict,
    ));
    assert_eq!(body["params"]["name"], "you-search");
    assert_eq!(
        body["params"]["arguments"],
        json!({
            "query": "tokio",
            "count": 50,
            "extraction": "none",
            "safesearch": "strict",
            "freshness": "week",
        })
    );
    let pdf_body = build_request_body(&mk_q(
        "attention",
        5,
        None,
        vec![],
        Some("pdf"),
        "us-en",
        SafeSearch::Moderate,
    ));
    assert_eq!(
        pdf_body["params"]["arguments"]["query"],
        "attention filetype:pdf"
    );
    assert!(pdf_body["params"]["arguments"].get("freshness").is_none());
}

#[test]
fn test_region_becomes_country_and_language() {
    for (region, expected) in [
        ("us-en", json!({"country": "US", "language": "EN"})),
        ("uk-en", json!({"country": "GB", "language": "EN-GB"})),
        ("jp-ja", json!({"country": "JP", "language": "JA"})),
        ("tw-zh", json!({"country": "TW", "language": "ZH-HANT"})),
        ("cn-zh", json!({"country": "CN", "language": "ZH-HANS"})),
        ("br-pt", json!({"country": "BR", "language": "PT-BR"})),
        ("wt-wt", json!({})),
        ("", json!({})),
    ] {
        assert_eq!(
            Value::Object(region_meta(region)),
            expected,
            "region={region}"
        );
    }
}

#[test]
fn test_domain_filters_never_send_include_and_exclude_together() {
    let b1 = build_request_body(&mk_q(
        "q",
        10,
        None,
        vec!["github.com".into()],
        None,
        "us-en",
        SafeSearch::Moderate,
    ));
    assert_eq!(
        b1["params"]["_meta"]["com.you.com/search"]["include_domains"],
        json!(["github.com"])
    );
    assert!(b1["params"]["arguments"].get("exclude_domains").is_none());

    let b2 = build_request_body(&mk_q(
        "q",
        10,
        None,
        vec!["-reddit.com".into()],
        None,
        "us-en",
        SafeSearch::Moderate,
    ));
    assert_eq!(
        b2["params"]["arguments"]["exclude_domains"],
        json!(["reddit.com"])
    );
    assert!(
        b2["params"]["_meta"]["com.you.com/search"]
            .get("include_domains")
            .is_none()
    );

    let b3 = build_request_body(&mk_q(
        "q",
        10,
        None,
        vec!["github.com".into(), "-reddit.com".into()],
        None,
        "us-en",
        SafeSearch::Moderate,
    ));
    assert_eq!(
        b3["params"]["_meta"]["com.you.com/search"]["include_domains"],
        json!(["github.com"])
    );
    assert!(b3["params"]["arguments"].get("exclude_domains").is_none());

    // Inline `site:` counts as an include, so exclude_domains must not join.
    let b4 = build_request_body(&mk_q(
        "tokio site:github.com",
        10,
        None,
        vec!["-x.com".into()],
        None,
        "us-en",
        SafeSearch::Moderate,
    ));
    assert!(b4["params"]["arguments"].get("exclude_domains").is_none());
}

#[tokio::test]
async fn test_maps_web_and_news_from_an_event_stream() {
    let net = Arc::new(MockYouNet::new(
        200,
        sse(&[notice(), rpc_result(sample_search_json(), false)]),
    ));
    let deps = SourceDeps::new(
        net.clone(),
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    );
    let engine = YouComEngine::new(deps);

    let SrcOutput::Hits(out) = engine
        .call(
            "search",
            json!({"query": "tokio runtime", "limit": 10}),
            &default_cx(),
        )
        .await
        .unwrap()
    else {
        panic!("expected hits");
    };
    assert_eq!(
        out.iter().map(|r| r.url.as_str()).collect::<Vec<_>>(),
        vec![
            "https://tokio.rs/",
            "https://github.com/tokio-rs/tokio",
            "https://news.example.com/tokio-1-50",
        ]
    );
    let (web, web2, news) = (&out[0], &out[1], &out[2]);
    assert!(web.snippet.starts_with("Tokio is a runtime"));
    assert_eq!(web.date.as_deref(), Some("2021-07-30"));
    assert_eq!(web.date_source.as_deref(), Some("snippet"));
    assert_eq!(web.kind, HitKind::Web);
    assert_eq!(
        web2.snippet,
        "A runtime for writing reliable asynchronous applications"
    );
    assert_eq!(news.date.as_deref(), Some("2026-10-16"));
    assert_eq!(news.date_source.as_deref(), Some("api"));
    assert_eq!(news.kind, HitKind::News);

    let raw_req = net.seen_body.lock().unwrap().clone().unwrap();
    let req_json: Value = serde_json::from_str(&raw_req).unwrap();
    assert_eq!(req_json["method"], "tools/call");
    assert_eq!(req_json["params"]["arguments"]["query"], "tokio runtime");
}

#[tokio::test]
async fn test_accepts_a_plain_json_reply() {
    let body = serde_json::to_string(&rpc_result(sample_search_json(), false)).unwrap();
    let deps = SourceDeps::new(
        Arc::new(MockYouNet::new(200, body)),
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    );
    let SrcOutput::Hits(out) = YouComEngine::new(deps)
        .call("search", json!({"query": "q", "limit": 10}), &default_cx())
        .await
        .unwrap()
    else {
        panic!("expected hits");
    };
    assert_eq!(out.len(), 3);
}

#[tokio::test]
async fn test_long_passage_snippet_is_capped() {
    let search = json!({"results": {"web": [{"url": "https://e.com/a", "title": "A", "snippets": ["x".repeat(2000)]}]}});
    let deps = SourceDeps::new(
        Arc::new(MockYouNet::new(200, sse(&[rpc_result(search, false)]))),
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    );
    let SrcOutput::Hits(out) = YouComEngine::new(deps)
        .call("search", json!({"query": "q", "limit": 5}), &default_cx())
        .await
        .unwrap()
    else {
        panic!("expected hits");
    };
    assert_eq!(out.len(), 1);
    assert!(out[0].snippet.chars().count() <= SNIPPET_CAP + 2);
    assert!(out[0].snippet.ends_with('…'));
}

#[tokio::test]
async fn test_skips_items_without_title_or_http_url_and_truncates() {
    let web = json!([
        {"url": "https://e.com/ok", "title": "OK"},
        {"url": "", "title": "no url"},
        {"url": "javascript:alert(1)", "title": "not http"},
        {"url": "https://e.com/untitled", "title": "   "},
        "not a dict"
    ]);
    let deps = SourceDeps::new(
        Arc::new(MockYouNet::new(
            200,
            sse(&[rpc_result(json!({"results": {"web": web}}), false)]),
        )),
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    );
    let SrcOutput::Hits(out) = YouComEngine::new(deps)
        .call("search", json!({"query": "q", "limit": 10}), &default_cx())
        .await
        .unwrap()
    else {
        panic!("expected hits");
    };
    assert_eq!(
        out.iter().map(|r| r.url.as_str()).collect::<Vec<_>>(),
        vec!["https://e.com/ok"]
    );
}

#[tokio::test]
async fn test_a_tool_error_and_json_rpc_error_and_429() {
    let tool_err = sse(&[rpc_result(
        json!("Error: daily limit of 100 free searches reached"),
        true,
    )]);
    let deps1 = SourceDeps::new(
        Arc::new(MockYouNet::new(200, tool_err)),
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    );
    let err1 = YouComEngine::new(deps1)
        .call("search", json!({"query": "q", "limit": 5}), &default_cx())
        .await
        .unwrap_err();
    assert!(err1.to_string().contains("daily limit"), "{err1}");

    let rpc_err = sse(&[
        json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32602, "message": "Invalid params"}}),
    ]);
    let deps2 = SourceDeps::new(
        Arc::new(MockYouNet::new(200, rpc_err)),
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    );
    let err2 = YouComEngine::new(deps2)
        .call("search", json!({"query": "q", "limit": 5}), &default_cx())
        .await
        .unwrap_err();
    assert!(err2.to_string().contains("Invalid params"), "{err2}");

    let deps3 = SourceDeps::new(
        Arc::new(MockYouNet::new(429, "Too Many Requests")),
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    );
    let err3 = YouComEngine::new(deps3)
        .call("search", json!({"query": "q", "limit": 5}), &default_cx())
        .await
        .unwrap_err();
    assert!(matches!(err3, FsmError::RateLimited { .. }), "{err3}");

    // M9: string-valued JSON-RPC error renders cleanly without extra JSON quotes.
    let str_err = sse(&[json!({"jsonrpc": "2.0", "id": 1, "error": "quota exhausted"})]);
    let deps4 = SourceDeps::new(
        Arc::new(MockYouNet::new(200, str_err)),
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    );
    let err4 = YouComEngine::new(deps4)
        .call("search", json!({"query": "q", "limit": 5}), &default_cx())
        .await
        .unwrap_err();
    match err4 {
        FsmError::Upstream { message, .. } => {
            assert_eq!(message, "youcom: quota exhausted");
        }
        other => panic!("expected Upstream error, got {other:?}"),
    }
}

struct FailingYouNet;

#[async_trait]
impl NetClient for FailingYouNet {
    async fn send(
        &self,
        _req: HttpRequest,
        _profile: TransportProfile,
        _cx: &NetCx,
    ) -> Result<HttpBytes, FsmError> {
        Err(FsmError::upstream("youcom", None, "connection reset"))
    }

    fn check_url_offline(&self, _url: &str) -> Result<(), FsmError> {
        Ok(())
    }
}

#[tokio::test]
async fn test_refusal_403_500_and_network_error_surface_errors() {
    // 1.0 mapping of `test_youcom.py::test_refusal_is_empty_and_reported` and
    // `test_network_error_is_empty`: refusals and transport faults propagate as
    // typed `FsmError`s so the kernel health tracker can score them.
    for status in [403u16, 500u16] {
        let deps = SourceDeps::new(
            Arc::new(MockYouNet::new(status, "refused")),
            Arc::new(NullStore::new()),
            Arc::new(Config::default()),
        );
        let err = YouComEngine::new(deps)
            .call("search", json!({"query": "q", "limit": 5}), &default_cx())
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                FsmError::Upstream {
                    status: Some(s),
                    ..
                } if s == status
            ),
            "status {status}: {err:?}"
        );
    }

    let net_deps = SourceDeps::new(
        Arc::new(FailingYouNet),
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    );
    let err = YouComEngine::new(net_deps)
        .call("search", json!({"query": "q", "limit": 5}), &default_cx())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("connection reset"), "{err}");
}

#[tokio::test]
async fn test_malformed_reply_is_empty() {
    for body in [
        "".to_owned(),
        "not json".to_owned(),
        sse(&[notice()]),
        sse(&[rpc_result(json!("not the search json"), false)]),
        sse(&[json!({"jsonrpc": "2.0", "id": 1, "result": {"content": "x"}})]),
        sse(&[rpc_result(json!({"results": {"web": "x"}}), false)]),
    ] {
        let deps = SourceDeps::new(
            Arc::new(MockYouNet::new(200, body.clone())),
            Arc::new(NullStore::new()),
            Arc::new(Config::default()),
        );
        let SrcOutput::Hits(out) = YouComEngine::new(deps)
            .call("search", json!({"query": "q", "limit": 5}), &default_cx())
            .await
            .unwrap()
        else {
            panic!("expected hits");
        };
        assert!(out.is_empty(), "body={body}");
    }
}
