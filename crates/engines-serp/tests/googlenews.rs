//! Port of `tests/test_googlenews.py` + single-flight cancellation tests from `tests/test_gnews.py`.

use async_trait::async_trait;
use fsm_core::api::Freshness;
use fsm_core::config::Config;
use fsm_core::net::{HttpBytes, HttpRequest, NetClient, NetCx};
use fsm_core::source::{Source, SrcCx, SrcOutput, TransportProfile};
use fsm_core::{FsmError, NullStore, SourceDeps};
use fsm_engines_serp::common::SerpQuery;
use fsm_engines_serp::engines::googlenews::{
    GoogleNewsEngine, RESOLVE_CONCURRENCY, RESOLVE_MAX_FAILURES, ResolveConfig, ResolveUrlFn,
    build_url, parse,
};
use fsm_engines_serp::gnews::GNewsResolver;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

const SAMPLE_RSS: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0">
  <channel>
    <title>"AI" - Google News</title>
    <link>https://news.google.com/search?q=AI</link>
    <description>Google News</description>
    <item>
      <title>OpenAI announces new model</title>
      <link>https://news.google.com/articles/CBMiAbcdef?oc=5</link>
      <guid isPermaLink="false">CBMiAbcdef</guid>
      <pubDate>Tue, 28 Apr 2026 15:30:00 GMT</pubDate>
      <description>&lt;a href="https://news.google.com/x"&gt;OpenAI announces new model&lt;/a&gt;&amp;nbsp;&amp;nbsp;&lt;font color="#6f6f6f"&gt;Reuters&lt;/font&gt;</description>
      <source url="https://www.reuters.com">Reuters</source>
    </item>
    <item>
      <title>Anthropic releases Claude update</title>
      <link>https://news.google.com/articles/CBMiXyz123?oc=5</link>
      <guid isPermaLink="false">CBMiXyz123</guid>
      <pubDate>Tue, 28 Apr 2026 12:00:00 GMT</pubDate>
      <description>Latest Claude model brings improvements.</description>
      <source url="https://www.theverge.com">The Verge</source>
    </item>
  </channel>
</rss>"##;

fn feed(n: usize) -> String {
    let items: String = (0..n)
        .map(|i| {
            format!(
                "<item><title>Story {i}</title><link>https://news.google.com/rss/articles/CBM{i}</link><source>Outlet {i}</source></item>"
            )
        })
        .collect();
    format!("<rss><channel>{items}</channel></rss>")
}

#[test]
fn test_parse_offline_rss() {
    let results = parse(SAMPLE_RSS);
    assert_eq!(results.len(), 2);
    let first = &results[0];
    assert!(first.title.contains("(Reuters)"));
    assert!(first.url.contains("news.google.com"));
    assert!(!first.snippet.contains('<'));
    assert!(!first.snippet.contains("&nbsp;"));
    assert_eq!(first.source, "googlenews");
    assert_eq!(first.date_source.as_deref(), Some("rss"));
}

#[test]
fn test_parse_handles_garbage() {
    assert!(parse("").is_empty());
    assert!(parse("<not><valid xml").is_empty());
}

#[test]
fn test_build_url_includes_freshness_and_locale() {
    let mut cx = SrcCx::from_config(
        &Config::default(),
        CancellationToken::new(),
        Duration::from_secs(5),
    );
    cx.freshness = Some(Freshness::Day);
    let q = SerpQuery::from_call(
        "googlenews",
        "search",
        &json!({"query": "AI", "limit": 5}),
        &cx,
        &Config::default(),
    )
    .unwrap();
    let url = build_url(&q);
    assert!(url.starts_with("https://news.google.com/rss/search?q="));
    assert!(url.contains("when%3A1d"), "{url}");
    assert!(url.contains("ceid=US:en"), "{url}");
    assert!(url.contains("hl=en-US"), "{url}");
}

struct StaticRssNet(String);

#[async_trait]
impl NetClient for StaticRssNet {
    async fn send(
        &self,
        req: HttpRequest,
        _profile: TransportProfile,
        _cx: &NetCx,
    ) -> Result<HttpBytes, FsmError> {
        Ok(HttpBytes::new(
            req.url,
            200,
            vec![(
                "content-type".into(),
                "application/rss+xml; charset=utf-8".into(),
            )],
            self.0.clone(),
        ))
    }

    fn check_url_offline(&self, _url: &str) -> Result<(), FsmError> {
        Ok(())
    }
}

fn default_cx() -> SrcCx {
    SrcCx::from_config(
        &Config::default(),
        CancellationToken::new(),
        Duration::from_secs(5),
    )
}

#[tokio::test]
async fn test_results_carry_publisher_urls_so_host_filters_can_see_them() {
    let deps = SourceDeps::new(
        Arc::new(StaticRssNet(feed(6))),
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    );
    let resolver: ResolveUrlFn = Arc::new(|url, _, _, _| {
        Box::pin(async move {
            let i: usize = url.rsplit_once("CBM")?.1.parse().ok()?;
            let host = if i.is_multiple_of(2) {
                "reuters.com"
            } else {
                "example.net"
            };
            Some(format!("https://{host}/story/{i}"))
        })
    });
    let engine = GoogleNewsEngine::with_resolver(deps, resolver, ResolveConfig::default());

    let SrcOutput::Hits(everything) = engine
        .call("search", json!({"query": "q", "limit": 10}), &default_cx())
        .await
        .unwrap()
    else {
        panic!("expected hits");
    };
    assert!(
        everything
            .iter()
            .all(|r| !r.url.contains("news.google.com"))
    );

    let mut cx = default_cx();
    cx.domains = vec!["reuters.com".into()];
    let SrcOutput::Hits(only_reuters) = engine
        .call("search", json!({"query": "q", "limit": 10}), &cx)
        .await
        .unwrap()
    else {
        panic!("expected hits");
    };
    assert_eq!(
        only_reuters
            .iter()
            .map(|r| r.url.as_str())
            .collect::<Vec<_>>(),
        vec![
            "https://reuters.com/story/0",
            "https://reuters.com/story/2",
            "https://reuters.com/story/4",
        ]
    );
}

#[tokio::test]
async fn test_resolution_is_bounded_in_parallelism() {
    let live = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let l2 = Arc::clone(&live);
    let p2 = Arc::clone(&peak);
    let resolver: ResolveUrlFn = Arc::new(move |url, _, _, _| {
        let live = Arc::clone(&l2);
        let peak = Arc::clone(&p2);
        Box::pin(async move {
            let cur = live.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(cur, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(15)).await;
            live.fetch_sub(1, Ordering::SeqCst);
            Some(url.replace("news.google.com", "publisher.example"))
        })
    });
    let deps = SourceDeps::new(
        Arc::new(StaticRssNet(feed(10))),
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    );
    let engine = GoogleNewsEngine::with_resolver(deps, resolver, ResolveConfig::default());
    engine
        .call("search", json!({"query": "q", "limit": 10}), &default_cx())
        .await
        .unwrap();
    assert_eq!(peak.load(Ordering::SeqCst), RESOLVE_CONCURRENCY);
}

#[tokio::test]
async fn test_only_the_results_that_can_be_returned_are_resolved() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = Arc::clone(&seen);
    let resolver: ResolveUrlFn = Arc::new(move |url, _, _, _| {
        let seen = Arc::clone(&s2);
        Box::pin(async move {
            seen.lock().unwrap().push(url);
            None
        })
    });
    let deps = SourceDeps::new(
        Arc::new(StaticRssNet(feed(10))),
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    );
    let engine = GoogleNewsEngine::with_resolver(
        deps,
        resolver,
        ResolveConfig {
            max_failures: 99,
            ..ResolveConfig::default()
        },
    );
    engine
        .call("search", json!({"query": "q", "limit": 3}), &default_cx())
        .await
        .unwrap();
    assert_eq!(seen.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn test_a_slow_resolver_costs_the_budget_and_keeps_the_redirect_links() {
    let resolver: ResolveUrlFn = Arc::new(|_, _, _, _| {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Some("https://never.example/".to_owned())
        })
    });
    let deps = SourceDeps::new(
        Arc::new(StaticRssNet(feed(5))),
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    );
    let engine = GoogleNewsEngine::with_resolver(
        deps,
        resolver,
        ResolveConfig {
            budget: Duration::from_millis(50),
            ..ResolveConfig::default()
        },
    );
    let started = Instant::now();
    let SrcOutput::Hits(results) = engine
        .call("search", json!({"query": "q", "limit": 10}), &default_cx())
        .await
        .unwrap()
    else {
        panic!("expected hits");
    };
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(results.len(), 5);
    assert!(results.iter().all(|r| r.url.contains("news.google.com")));
}

#[tokio::test]
async fn test_resolution_gives_up_after_consecutive_failures() {
    let calls = Arc::new(AtomicUsize::new(0));
    let c2 = Arc::clone(&calls);
    let resolver: ResolveUrlFn = Arc::new(move |_, _, _, _| {
        let calls = Arc::clone(&c2);
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            None
        })
    });
    let deps = SourceDeps::new(
        Arc::new(StaticRssNet(feed(10))),
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    );
    let engine = GoogleNewsEngine::with_resolver(
        deps,
        resolver,
        ResolveConfig {
            concurrency: 1,
            ..ResolveConfig::default()
        },
    );
    let SrcOutput::Hits(results) = engine
        .call("search", json!({"query": "q", "limit": 10}), &default_cx())
        .await
        .unwrap()
    else {
        panic!("expected hits");
    };
    assert_eq!(calls.load(Ordering::SeqCst), RESOLVE_MAX_FAILURES);
    assert_eq!(results.len(), 10);
}

struct BlockingGNewsNet {
    entered: Arc<Notify>,
    release: Arc<Notify>,
    page_calls: AtomicUsize,
}

#[async_trait]
impl NetClient for BlockingGNewsNet {
    async fn send(
        &self,
        req: HttpRequest,
        _profile: TransportProfile,
        _cx: &NetCx,
    ) -> Result<HttpBytes, FsmError> {
        if req.url.contains("batchexecute") {
            let inner =
                serde_json::to_string(&json!(["garturlres", "https://example.org/story", 1]))
                    .unwrap();
            let body = format!(
                ")]}}'\n\n{}",
                serde_json::to_string(&json!([[
                    "wrb.fr", "Fbv4je", inner, null, null, null, "generic"
                ]]))
                .unwrap()
            );
            return Ok(HttpBytes::new(req.url, 200, vec![], body));
        }
        self.page_calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        self.release.notified().await;
        Ok(HttpBytes::new(
            req.url,
            200,
            vec![],
            r#"<c-wiz data-n-a-id="AID" data-n-a-ts="1700000000" data-n-a-sg="SIG"></c-wiz>"#,
        ))
    }

    fn check_url_offline(&self, _url: &str) -> Result<(), FsmError> {
        Ok(())
    }
}

#[tokio::test]
async fn test_singleflight_cancellation_does_not_cancel_other_waiters() {
    for cancel_owner in [false, true] {
        let resolver = GNewsResolver::new();
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let net = Arc::new(BlockingGNewsNet {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
            page_calls: AtomicUsize::new(0),
        });
        let url = "https://news.google.com/read/CBM-cancellation";
        let ncx = NetCx::new(CancellationToken::new(), Duration::from_secs(5));

        let r0 = resolver.clone();
        let n0 = Arc::clone(&net);
        let ncx0 = ncx.clone();
        let owner = tokio::spawn(async move { r0.resolve(url, n0.as_ref(), "en-US", &ncx0).await });
        entered.notified().await;

        let r1 = resolver.clone();
        let n1 = Arc::clone(&net);
        let ncx1 = ncx.clone();
        let f1 = tokio::spawn(async move { r1.resolve(url, n1.as_ref(), "en-US", &ncx1).await });

        let r2 = resolver.clone();
        let n2 = Arc::clone(&net);
        let ncx2 = ncx.clone();
        let f2 = tokio::spawn(async move { r2.resolve(url, n2.as_ref(), "en-US", &ncx2).await });
        tokio::time::sleep(Duration::from_millis(10)).await;

        if cancel_owner {
            owner.abort();
            let _ = owner.await;
            let res1 = f1.await.unwrap();
            let res2 = f2.await.unwrap();
            assert_eq!(res1, None);
            assert_eq!(res2, None);
        } else {
            f1.abort();
            let _ = f1.await;
            release.notify_waiters();
            let res0 = owner.await.unwrap();
            let res2 = f2.await.unwrap();
            assert_eq!(res0.as_deref(), Some("https://example.org/story"));
            assert_eq!(res2.as_deref(), Some("https://example.org/story"));
            // Memo hit on repeat call: still 1 page call.
            assert_eq!(
                resolver
                    .resolve(url, net.as_ref(), "en-US", &ncx)
                    .await
                    .as_deref(),
                Some("https://example.org/story")
            );
        }
        assert_eq!(net.page_calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn test_resolve_returns_none_for_non_gnews_url() {
    let resolver = GNewsResolver::new();
    let net = StaticRssNet(String::new());
    let ncx = NetCx::new(CancellationToken::new(), Duration::from_secs(5));
    assert_eq!(
        resolver
            .resolve("https://www.bbc.com/news/x", &net, "en-US", &ncx)
            .await,
        None
    );
}

#[tokio::test]
async fn test_dropping_call_aborts_in_flight_resolver_tasks() {
    // M5: resolver tasks live in a JoinSet so dropping call() aborts them
    // promptly instead of detaching background fetches.
    struct ActiveGuard(Arc<AtomicUsize>);
    impl Drop for ActiveGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    let active = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let a2 = Arc::clone(&active);
    let s2 = Arc::clone(&started);
    let resolver: ResolveUrlFn = Arc::new(move |_, _, _, _| {
        let active = Arc::clone(&a2);
        let started = Arc::clone(&s2);
        Box::pin(async move {
            active.fetch_add(1, Ordering::SeqCst);
            let _guard = ActiveGuard(active);
            started.notify_waiters();
            tokio::time::sleep(Duration::from_secs(60)).await;
            Some("https://never.example/".to_owned())
        })
    });
    let deps = SourceDeps::new(
        Arc::new(StaticRssNet(feed(4))),
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    );
    let engine = GoogleNewsEngine::with_resolver(deps, resolver, ResolveConfig::default());

    {
        let cx = default_cx();
        let mut fut = Box::pin(engine.call("search", json!({"query": "q", "limit": 4}), &cx));
        tokio::select! {
            _ = &mut fut => panic!("resolver should have blocked"),
            _ = started.notified() => {}
        }
        assert!(active.load(Ordering::SeqCst) >= 1);
        // `fut` drops here without cancelling `cx.cancel`.
    }

    // Give the runtime a brief yield to run aborted task destructors.
    for _ in 0..20 {
        if active.load(Ordering::SeqCst) == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        active.load(Ordering::SeqCst),
        0,
        "dropped GoogleNewsEngine::call() left detached resolver tasks running"
    );
}

#[tokio::test]
async fn test_resolution_uses_distinct_unthrottled_host_bucket_and_adequate_timeout() {
    let seen_buckets = Arc::new(Mutex::new(Vec::new()));
    let seen_rps = Arc::new(Mutex::new(Vec::new()));
    let b2 = Arc::clone(&seen_buckets);
    let r2 = Arc::clone(&seen_rps);
    let resolver: ResolveUrlFn = Arc::new(move |url, _, _, ncx| {
        let buckets = Arc::clone(&b2);
        let rps = Arc::clone(&r2);
        Box::pin(async move {
            buckets.lock().unwrap().push(ncx.host_bucket.clone());
            rps.lock().unwrap().push(ncx.rps);
            Some(url.replace("news.google.com/rss/articles", "www.reuters.com/world"))
        })
    });
    let deps = SourceDeps::new(
        Arc::new(StaticRssNet(feed(4))),
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    );
    let engine = GoogleNewsEngine::with_resolver(deps, resolver, ResolveConfig::default());
    assert!(
        engine.decl().limits.timeout_ms >= 10_000,
        "timeout_ms must allow RSS + resolution budget, got {}",
        engine.decl().limits.timeout_ms
    );

    let SrcOutput::Hits(hits) = engine
        .call(
            "search",
            json!({"query": "EU AI Act", "limit": 4}),
            &default_cx(),
        )
        .await
        .unwrap()
    else {
        panic!("expected hits");
    };
    assert_eq!(hits.len(), 4);
    assert!(
        hits.iter()
            .all(|h| h.url.starts_with("https://www.reuters.com/"))
    );
    let buckets = seen_buckets.lock().unwrap();
    assert_eq!(buckets.len(), 4);
    assert!(
        buckets
            .iter()
            .all(|b| b.as_deref() != Some("googlenews") && b.is_some()),
        "resolution hops must not reuse the RSS 1.0 rps 'googlenews' bucket: {buckets:?}"
    );
    assert!(seen_rps.lock().unwrap().iter().all(|r| r.is_none()));
}

#[test]
fn test_googlenews_rss_rejects_xml_entity_declarations() {
    let xxe = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE rss [<!ENTITY xxe "expanded">]>
<rss version="2.0"><channel><item>
<title>Headline &xxe;</title>
<link>https://news.google.com/rss/articles/CBMi1</link>
</item></channel></rss>"#;
    assert!(
        fsm_engines_serp::engines::googlenews::parse(xxe).is_empty(),
        "<!ENTITY must be rejected per R3"
    );

    let mixed_case = r#"<?xml version="1.0"?>
<!DOCTYPE rss [<!EnTiTy lol "x">]>
<rss version="2.0"><channel><item><title>T</title><link>https://example.org/a</link></item></channel></rss>"#;
    assert!(fsm_engines_serp::engines::googlenews::parse(mixed_case).is_empty());

    let clean = r#"<?xml version="1.0"?>
<rss version="2.0"><channel><item><title>Safe Item</title><link>https://example.org/safe</link></item></channel></rss>"#;
    let hits = fsm_engines_serp::engines::googlenews::parse(clean);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].title, "Safe Item");
}
