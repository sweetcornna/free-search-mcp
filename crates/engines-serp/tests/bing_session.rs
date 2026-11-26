//! Port of `tests/test_bing_session.py` + Bing `ck/a` tests from `tests/test_engines_extra.py`.

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use fsm_core::api::Freshness;
use fsm_core::config::{Config, SafeSearch};
use fsm_core::net::{HttpBytes, HttpRequest, NetClient, NetCx};
use fsm_core::source::{Source, SrcCx, SrcOutput, TransportProfile};
use fsm_core::{FsmError, NullStore, SourceDeps};
use fsm_engines_serp::common::SerpQuery;
use fsm_engines_serp::engines::bing::{
    BingEngine, ClockFn, EgressFn, JAR_TTL_SECS, WARM_FAIL_TTL_SECS, WARM_URL, build_url, parse,
    resolve_bing_url,
};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const QUERY: &str = "rust ownership borrowing";

fn serp(rows: &[(&str, &str)], host: &str) -> String {
    let mut s = String::from(r#"<html><body><ol id="b_results">"#);
    for (i, (t, sn)) in rows.iter().enumerate() {
        s.push_str(&format!(
            r#"<li class="b_algo"><h2><a href="https://{host}/{i}">{t}</a></h2><div class="b_caption"><p>{sn}</p></div></li>"#
        ));
    }
    s.push_str("</ol></body></html>");
    s
}

fn decoy_html() -> String {
    serp(
        &[("Rust on Steam", "The only aim in Rust is to survive."); 8],
        "decoy.example",
    )
}

fn real_html() -> String {
    serp(
        &[(
            "Understanding Ownership - Rust",
            "Ownership and borrowing in Rust.",
        ); 8],
        "r.example",
    )
}

fn page_two_html() -> String {
    serp(
        &[("References and Borrowing", "Borrowing rules and ownership."); 6],
        "p2.example",
    )
}

type PageResponder =
    Arc<dyn Fn(&str, Option<&str>, usize) -> Result<String, FsmError> + Send + Sync>;

struct ScriptedBingNet {
    warmups: AtomicUsize,
    requests: Mutex<Vec<(String, Option<String>)>>,
    warm_cookies: Vec<Result<Vec<(&'static str, &'static str)>, String>>,
    warm_delay: Duration,
    responder: PageResponder,
}

impl ScriptedBingNet {
    fn new(
        warm_cookies: Vec<Result<Vec<(&'static str, &'static str)>, String>>,
        warm_delay: Duration,
        responder: PageResponder,
    ) -> Self {
        Self {
            warmups: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
            warm_cookies,
            warm_delay,
            responder,
        }
    }

    fn warmup_count(&self) -> usize {
        self.warmups.load(Ordering::SeqCst)
    }

    fn search_requests(&self) -> Vec<(String, Option<String>)> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl NetClient for ScriptedBingNet {
    async fn send(
        &self,
        req: HttpRequest,
        _profile: TransportProfile,
        _cx: &NetCx,
    ) -> Result<HttpBytes, FsmError> {
        if req.url == WARM_URL {
            let idx = self.warmups.fetch_add(1, Ordering::SeqCst);
            if !self.warm_delay.is_zero() {
                tokio::time::sleep(self.warm_delay).await;
            }
            let entry = &self.warm_cookies[idx.min(self.warm_cookies.len() - 1)];
            match entry {
                Ok(pairs) => {
                    let headers: Vec<(String, String)> = pairs
                        .iter()
                        .map(|(k, v)| ("set-cookie".to_owned(), format!("{k}={v}; Path=/")))
                        .collect();
                    return Ok(HttpBytes::new(req.url, 200, headers, "<html></html>"));
                }
                Err(msg) => return Err(FsmError::upstream("bing", None, msg.clone())),
            }
        }

        let cookie = req
            .headers
            .iter()
            .rfind(|(k, _)| k.eq_ignore_ascii_case("cookie"))
            .map(|(_, v)| v.clone());
        let call_num = {
            let mut log = self.requests.lock().unwrap();
            log.push((req.url.clone(), cookie.clone()));
            log.len()
        };
        let html = (self.responder)(&req.url, cookie.as_deref(), call_num)?;
        Ok(HttpBytes::new(
            req.url,
            200,
            vec![("content-type".into(), "text/html; charset=utf-8".into())],
            html,
        ))
    }

    fn check_url_offline(&self, _url: &str) -> Result<(), FsmError> {
        Ok(())
    }
}

fn mk_engine(
    net: Arc<ScriptedBingNet>,
    clock_val: Arc<Mutex<f64>>,
    egress_val: Arc<Mutex<String>>,
) -> BingEngine {
    let deps = SourceDeps::new(net, Arc::new(NullStore::new()), Arc::new(Config::default()));
    let clock: ClockFn = Arc::new(move || *clock_val.lock().unwrap());
    let egress: EgressFn = Arc::new(move |_| egress_val.lock().unwrap().clone());
    BingEngine::with_hooks(deps, clock, egress)
}

fn default_cx() -> SrcCx {
    SrcCx::from_config(
        &Config::default(),
        CancellationToken::new(),
        Duration::from_secs(5),
    )
}

async fn search_hits(engine: &BingEngine, query: &str, limit: u32) -> Vec<fsm_core::Hit> {
    match engine
        .call(
            "search",
            json!({"query": query, "limit": limit}),
            &default_cx(),
        )
        .await
        .expect("bing search ok")
    {
        SrcOutput::Hits(h) => h,
        other => panic!("expected Hits, got {other:?}"),
    }
}

#[tokio::test]
async fn test_the_session_is_minted_once_and_sent_with_every_search() {
    let real = real_html();
    let net = Arc::new(ScriptedBingNet::new(
        vec![Ok(vec![("MUID", "one")])],
        Duration::ZERO,
        Arc::new(move |_, _, _| Ok(real.clone())),
    ));
    let engine = mk_engine(
        net.clone(),
        Arc::new(Mutex::new(1000.0)),
        Arc::new(Mutex::new(String::new())),
    );

    let first = search_hits(&engine, QUERY, 10).await;
    let second = search_hits(&engine, "rust borrowing ownership rules", 10).await;

    assert_eq!(first.len(), 8);
    assert_eq!(second.len(), 8);
    assert_eq!(net.warmup_count(), 1);
    let cookies: Vec<Option<String>> = net.search_requests().into_iter().map(|(_, c)| c).collect();
    assert_eq!(
        cookies,
        vec![Some("MUID=one".to_owned()), Some("MUID=one".to_owned())]
    );
}

#[tokio::test]
async fn test_concurrent_cold_searches_share_one_warm_up() {
    let real = real_html();
    let net = Arc::new(ScriptedBingNet::new(
        vec![Ok(vec![("MUID", "one")])],
        Duration::from_millis(20),
        Arc::new(move |_, _, _| Ok(real.clone())),
    ));
    let engine = Arc::new(mk_engine(
        net.clone(),
        Arc::new(Mutex::new(1000.0)),
        Arc::new(Mutex::new(String::new())),
    ));

    let futs = (0..5).map(|_| {
        let e = Arc::clone(&engine);
        async move { search_hits(&e, QUERY, 10).await }
    });
    let buckets = futures::future::join_all(futs).await;

    assert_eq!(buckets.len(), 5);
    assert_eq!(net.warmup_count(), 1);
    let reqs = net.search_requests();
    assert_eq!(reqs.len(), 5);
    assert!(reqs.iter().all(|(_, c)| c.as_deref() == Some("MUID=one")));
}

#[tokio::test]
async fn test_a_decoy_re_mints_the_session_and_retries_once() {
    let decoy = decoy_html();
    let real = real_html();
    let net = Arc::new(ScriptedBingNet::new(
        vec![Ok(vec![("MUID", "stale")]), Ok(vec![("MUID", "fresh")])],
        Duration::ZERO,
        Arc::new(move |_, _, call_num| {
            if call_num == 1 {
                Ok(decoy.clone())
            } else {
                Ok(real.clone())
            }
        }),
    ));
    let engine = mk_engine(
        net.clone(),
        Arc::new(Mutex::new(1000.0)),
        Arc::new(Mutex::new(String::new())),
    );

    let results = search_hits(&engine, QUERY, 10).await;

    assert_eq!(net.warmup_count(), 2);
    let cookies: Vec<Option<String>> = net.search_requests().into_iter().map(|(_, c)| c).collect();
    assert_eq!(
        cookies,
        vec![Some("MUID=stale".to_owned()), Some("MUID=fresh".to_owned())]
    );
    assert!(results.iter().all(|r| r.url.contains("r.example")));
}

#[tokio::test]
async fn test_a_persistent_decoy_is_returned_for_the_aggregator_to_judge() {
    let decoy = decoy_html();
    let net = Arc::new(ScriptedBingNet::new(
        vec![Ok(vec![("MUID", "a")]), Ok(vec![("MUID", "b")])],
        Duration::ZERO,
        Arc::new(move |_, _, _| Ok(decoy.clone())),
    ));
    let engine = mk_engine(
        net.clone(),
        Arc::new(Mutex::new(1000.0)),
        Arc::new(Mutex::new(String::new())),
    );

    let results = search_hits(&engine, QUERY, 10).await;

    assert_eq!(net.search_requests().len(), 2);
    assert_eq!(net.warmup_count(), 2);
    assert_eq!(results.len(), 8);
    assert!(results.iter().all(|r| r.url.contains("decoy.example")));
}

#[tokio::test]
async fn test_concurrent_decoys_re_mint_once_not_once_each() {
    let decoy = decoy_html();
    let real = real_html();
    let net = Arc::new(ScriptedBingNet::new(
        vec![Ok(vec![("MUID", "stale")]), Ok(vec![("MUID", "fresh")])],
        Duration::from_millis(15),
        Arc::new(move |_, ck, _| {
            if ck == Some("MUID=stale") {
                Ok(decoy.clone())
            } else {
                Ok(real.clone())
            }
        }),
    ));
    let engine = Arc::new(mk_engine(
        net.clone(),
        Arc::new(Mutex::new(1000.0)),
        Arc::new(Mutex::new(String::new())),
    ));

    let futs = (0..4).map(|_| {
        let e = Arc::clone(&engine);
        async move { search_hits(&e, QUERY, 10).await }
    });
    let buckets = futures::future::join_all(futs).await;

    assert_eq!(
        net.warmup_count(),
        2,
        "four decoys from one stale jar must produce one re-mint"
    );
    assert!(
        buckets
            .iter()
            .all(|b| b.iter().all(|r| r.url.contains("r.example")))
    );
}

#[tokio::test]
async fn test_a_failed_warm_up_does_not_fail_the_search() {
    let real = real_html();
    let net = Arc::new(ScriptedBingNet::new(
        vec![Err("no route to host".into())],
        Duration::ZERO,
        Arc::new(move |_, _, _| Ok(real.clone())),
    ));
    let engine = mk_engine(
        net.clone(),
        Arc::new(Mutex::new(1000.0)),
        Arc::new(Mutex::new(String::new())),
    );

    let results = search_hits(&engine, QUERY, 10).await;
    assert_eq!(results.len(), 8);
    assert_eq!(net.search_requests()[0].1, None);
}

#[tokio::test]
async fn test_a_failed_warm_up_is_remembered_briefly_not_retried_per_search() {
    let real = real_html();
    let clock = Arc::new(Mutex::new(1000.0));
    let net = Arc::new(ScriptedBingNet::new(
        vec![Ok(vec![])],
        Duration::ZERO,
        Arc::new(move |_, _, _| Ok(real.clone())),
    ));
    let engine = mk_engine(
        net.clone(),
        clock.clone(),
        Arc::new(Mutex::new(String::new())),
    );

    search_hits(&engine, QUERY, 10).await;
    search_hits(&engine, QUERY, 10).await;
    assert_eq!(net.warmup_count(), 1);

    *clock.lock().unwrap() += WARM_FAIL_TTL_SECS + 1.0;
    search_hits(&engine, QUERY, 10).await;
    assert_eq!(net.warmup_count(), 2);
}

#[tokio::test]
async fn test_the_session_expires() {
    let real = real_html();
    let clock = Arc::new(Mutex::new(1000.0));
    let net = Arc::new(ScriptedBingNet::new(
        vec![Ok(vec![("MUID", "one")])],
        Duration::ZERO,
        Arc::new(move |_, _, _| Ok(real.clone())),
    ));
    let engine = mk_engine(
        net.clone(),
        clock.clone(),
        Arc::new(Mutex::new(String::new())),
    );

    search_hits(&engine, QUERY, 10).await;
    *clock.lock().unwrap() += JAR_TTL_SECS - 1.0;
    search_hits(&engine, QUERY, 10).await;
    assert_eq!(net.warmup_count(), 1);

    *clock.lock().unwrap() += 2.0;
    search_hits(&engine, QUERY, 10).await;
    assert_eq!(net.warmup_count(), 2);
}

#[tokio::test]
async fn test_a_different_proxy_egress_gets_its_own_session() {
    let real = real_html();
    let egress = Arc::new(Mutex::new(String::new()));
    let net = Arc::new(ScriptedBingNet::new(
        vec![Ok(vec![("MUID", "direct")]), Ok(vec![("MUID", "proxied")])],
        Duration::ZERO,
        Arc::new(move |_, _, _| Ok(real.clone())),
    ));
    let engine = mk_engine(net.clone(), Arc::new(Mutex::new(1000.0)), egress.clone());

    search_hits(&engine, QUERY, 10).await;
    *egress.lock().unwrap() = "http://127.0.0.1:7890".to_owned();
    search_hits(&engine, QUERY, 10).await;
    egress.lock().unwrap().clear();
    search_hits(&engine, QUERY, 10).await;

    assert_eq!(net.warmup_count(), 2);
    let cookies: Vec<Option<String>> = net.search_requests().into_iter().map(|(_, c)| c).collect();
    assert_eq!(
        cookies,
        vec![
            Some("MUID=direct".to_owned()),
            Some("MUID=proxied".to_owned()),
            Some("MUID=direct".to_owned()),
        ]
    );
}

#[tokio::test]
async fn test_more_than_ten_results_come_from_a_second_page() {
    let real = real_html();
    let p2 = page_two_html();
    let net = Arc::new(ScriptedBingNet::new(
        vec![Ok(vec![("MUID", "one")])],
        Duration::ZERO,
        Arc::new(move |url, _, _| {
            if url.contains("first=11") {
                Ok(p2.clone())
            } else {
                Ok(real.clone())
            }
        }),
    ));
    let engine = mk_engine(
        net.clone(),
        Arc::new(Mutex::new(1000.0)),
        Arc::new(Mutex::new(String::new())),
    );

    let results = search_hits(&engine, QUERY, 20).await;
    let urls: Vec<String> = net.search_requests().into_iter().map(|(u, _)| u).collect();
    assert_eq!(urls.len(), 2);
    assert!(urls[1].contains("first=11") && !urls[1].contains("count="));
    assert_eq!(results.len(), 14);
}

#[tokio::test]
async fn test_ten_results_or_fewer_is_one_request() {
    let real = real_html();
    let net = Arc::new(ScriptedBingNet::new(
        vec![Ok(vec![("MUID", "one")])],
        Duration::ZERO,
        Arc::new(move |_, _, _| Ok(real.clone())),
    ));
    let engine = mk_engine(
        net.clone(),
        Arc::new(Mutex::new(1000.0)),
        Arc::new(Mutex::new(String::new())),
    );

    search_hits(&engine, QUERY, 10).await;
    assert_eq!(net.search_requests().len(), 1);
}

#[tokio::test]
async fn test_a_failing_second_page_costs_only_the_extra_results() {
    let real = real_html();
    let net = Arc::new(ScriptedBingNet::new(
        vec![Ok(vec![("MUID", "one")])],
        Duration::ZERO,
        Arc::new(move |url, _, _| {
            if url.contains("first=11") {
                Err(FsmError::upstream("bing", Some(502), "page two exploded"))
            } else {
                Ok(real.clone())
            }
        }),
    ));
    let engine = mk_engine(
        net.clone(),
        Arc::new(Mutex::new(1000.0)),
        Arc::new(Mutex::new(String::new())),
    );

    let results = search_hits(&engine, QUERY, 20).await;
    assert_eq!(results.len(), 8);
}

#[tokio::test]
async fn test_a_decoy_is_not_paginated() {
    let decoy = decoy_html();
    let net = Arc::new(ScriptedBingNet::new(
        vec![Ok(vec![("MUID", "one")])],
        Duration::ZERO,
        Arc::new(move |_, _, _| Ok(decoy.clone())),
    ));
    let engine = mk_engine(
        net.clone(),
        Arc::new(Mutex::new(1000.0)),
        Arc::new(Mutex::new(String::new())),
    );

    search_hits(&engine, QUERY, 20).await;
    assert!(
        !net.search_requests()
            .iter()
            .any(|(u, _)| u.contains("first=11"))
    );
}

#[tokio::test]
async fn test_a_query_too_short_to_judge_is_never_retried() {
    for short_q in ["rust", "rust ownership"] {
        let decoy = decoy_html();
        let net = Arc::new(ScriptedBingNet::new(
            vec![Ok(vec![("MUID", "one")])],
            Duration::ZERO,
            Arc::new(move |_, _, _| Ok(decoy.clone())),
        ));
        let engine = mk_engine(
            net.clone(),
            Arc::new(Mutex::new(1000.0)),
            Arc::new(Mutex::new(String::new())),
        );
        search_hits(&engine, short_q, 10).await;
        assert_eq!(net.search_requests().len(), 1, "query={short_q}");
        assert_eq!(net.warmup_count(), 1, "query={short_q}");
    }
}

fn bing_wrapper(target: &str) -> String {
    let payload = URL_SAFE_NO_PAD.encode(target.as_bytes());
    format!("https://www.bing.com/ck/a?!&&p=deadbeef&u=a1{payload}&ntb=1")
}

#[test]
fn test_bing_resolve_url_unwraps_redirect() {
    let target = "https://zod.dev/api?id=zstrictobject";
    assert_eq!(resolve_bing_url(&bing_wrapper(target)), target);
}

#[test]
fn test_bing_resolve_url_passes_through_plain_and_undecodable() {
    let plain = "https://example.com/page";
    assert_eq!(resolve_bing_url(plain), plain);
    let junk = "https://www.bing.com/ck/a?u=a1!!!!notbase64";
    assert_eq!(resolve_bing_url(junk), junk);
}

#[test]
fn test_bing_parse_emits_publisher_urls() {
    let target = "https://zod.dev/api?id=zstrictobject";
    let html = format!(
        r#"<ol id="b_results"><li class="b_algo"><h2><a href="{}">Defining schemas | Zod</a></h2><div class="b_caption"><p>Strict object schemas.</p></div></li></ol>"#,
        bing_wrapper(target)
    );
    let results = parse(&html);
    assert_eq!(
        results.iter().map(|r| r.url.as_str()).collect::<Vec<_>>(),
        vec![target]
    );
}

#[test]
fn test_bing_build_url_never_sends_count_and_emits_safesearch_and_market() {
    for limit in [1, 10, 25, 50] {
        let mut cx = default_cx();
        cx.safesearch = SafeSearch::Strict;
        cx.region = "uk-en".into();
        cx.freshness = Some(Freshness::Week);
        let q = SerpQuery::from_call(
            "bing",
            "search",
            &json!({"query": QUERY, "limit": limit}),
            &cx,
            &Config::default(),
        )
        .unwrap();
        let url = build_url(&q);
        let parsed = url::Url::parse(&url).unwrap();
        let map: std::collections::HashMap<String, String> = parsed
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert!(!map.contains_key("count"));
        assert_eq!(map.get("form").map(String::as_str), Some("QBRE"));
        assert_eq!(map.get("adlt").map(String::as_str), Some("strict"));
        assert_eq!(map.get("mkt").map(String::as_str), Some("en-GB"));
        assert!(map.get("filters").is_some_and(|v| v.contains("ex1")));
    }
}
