//! Deep edge-case tests covering the riskiest logic in `fsm-extract`:
//! - `web.request` 303/307 redirect method/body handling, query forwarding, GCRA throttling & cancel
//! - `fetch_batch` security fail-fast vs partial upstream failure vs all-fail
//! - `structured` nested microdata, `itemref`, `<base href>`, multi-value `itemprop`, JSON-LD comments
//! - `html` fallback chain (`lol_html` -> `trafilatura` -> `dom_smoothie` -> `dom_query` -> `htmd`)
//! - `downloads` SSRF redirect block, 429 Retry-After, SSRF does not burn download slot
//! - `gnews` fallback resilience when batchexecute returns 500 or shell lacks attributes

mod common;

use common::{MemStore, MockDocs, MockNet, MockSearch, make_service};
use fsm_core::config::Config;
use fsm_core::{
    CallCx, FetchApi, FetchOpts, FetchOutcome, FsmError, HttpBytes, HttpMethod, RawRequestOpts,
};
use fsm_extract::gnews::build_freq;
use fsm_extract::html::extract_html;
use fsm_extract::request::HostRateLimiter;
use fsm_extract::structured::extract_structured_from_html;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tempfile::TempDir;

#[tokio::test]
async fn test_request_303_drops_post_body_and_307_preserves_it() {
    // 303 See Other: POST -> GET, body and Content-Type dropped.
    let net303 = Arc::new(MockNet::with_responses(vec![
        Ok(HttpBytes::new(
            "https://api.example/submit",
            303,
            vec![("location".into(), "/result".into())],
            "",
        )),
        Ok(HttpBytes::new(
            "https://api.example/result",
            200,
            vec![("content-type".into(), "application/json".into())],
            r#"{"done":true}"#,
        )),
    ]));
    let svc303 = make_service(
        net303.clone(),
        Arc::new(MemStore::new()),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );
    let mut opts = RawRequestOpts::new("https://api.example/submit");
    opts.method = Some(HttpMethod::Post);
    opts.body = Some(json!({"x": 1}));
    opts.query = Some(BTreeMap::from([("v".into(), "2".into())]));
    let resp = svc303.request(opts, &CallCx::default()).await.unwrap();
    assert_eq!(resp.json, Some(json!({"done": true})));

    let rec = net303.recorded();
    assert_eq!(rec.len(), 2);
    assert_eq!(rec[0].0.method, HttpMethod::Post);
    assert_eq!(rec[0].0.query, vec![("v".into(), "2".into())]);
    assert!(rec[0].0.body.is_some());
    assert_eq!(rec[1].0.method, HttpMethod::Get);
    assert_eq!(rec[1].0.url, "https://api.example/result");
    assert!(rec[1].0.body.is_none());
    assert!(
        rec[1].0.query.is_empty(),
        "query must not repeat on redirect"
    );
    assert!(
        !rec[1]
            .0
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
    );

    // 307 Temporary Redirect: POST stays POST with identical body.
    let net307 = Arc::new(MockNet::with_responses(vec![
        Ok(HttpBytes::new(
            "https://api.example/old",
            307,
            vec![("location".into(), "https://api.example/new".into())],
            "",
        )),
        Ok(HttpBytes::new(
            "https://api.example/new",
            200,
            vec![("content-type".into(), "text/plain".into())],
            "preserved",
        )),
    ]));
    let svc307 = make_service(
        net307.clone(),
        Arc::new(MemStore::new()),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );
    let mut opts307 = RawRequestOpts::new("https://api.example/old");
    opts307.method = Some(HttpMethod::Post);
    opts307.body = Some(json!({"keep": true}));
    let r307 = svc307.request(opts307, &CallCx::default()).await.unwrap();
    assert_eq!(r307.body, "preserved");
    let rec307 = net307.recorded();
    assert_eq!(rec307[1].0.method, HttpMethod::Post);
    assert_eq!(rec307[1].0.body, rec307[0].0.body);
}

#[tokio::test]
async fn test_host_gcra_limiter_burst_throttle_and_cancellation() {
    // 5 req/s (200 ms interval), burst 2 -> first 2 immediate, 3rd waits ~200 ms.
    let limiter = HostRateLimiter::new(5.0, 2);
    let cx = CallCx::default();
    let t0 = Instant::now();
    limiter.acquire("api.example", &cx).await.unwrap();
    limiter.acquire("api.example", &cx).await.unwrap();
    assert!(t0.elapsed() < Duration::from_millis(100));

    // 3rd request must wait; cancel it to verify cancellation returns Timeout(0) promptly.
    let cx_cancel = CallCx::default();
    let tok = cx_cancel.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        tok.cancel();
    });
    let err = limiter
        .acquire("api.example", &cx_cancel)
        .await
        .unwrap_err();
    assert!(matches!(err, FsmError::Timeout(0)), "{err:?}");

    // Different host has an independent bucket (immediate).
    let t1 = Instant::now();
    limiter.acquire("other.example", &cx).await.unwrap();
    assert!(t1.elapsed() < Duration::from_millis(100));
}

#[tokio::test]
async fn test_fetch_batch_preserves_order_and_marks_per_url_errors() {
    // 1. Mixed safe + unsafe + 503 URLs: every input gets a slot in order;
    //    unsafe URL makes ZERO network requests and does not kill sibling good pages (I2).
    let net = Arc::new(MockNet::with_handler(|req, _, _| {
        if req.url.contains("good") {
            Ok(HttpBytes::new(
                req.url.clone(),
                200,
                vec![("content-type".into(), "text/plain".into())],
                format!("ok:{}", req.url),
            ))
        } else {
            Ok(HttpBytes::new(req.url.clone(), 503, vec![], "unavailable"))
        }
    }));
    let svc = make_service(
        net.clone(),
        Arc::new(MemStore::new()),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );
    let pages = svc
        .fetch_batch(
            vec![
                "https://good.example/1".into(),
                "http://127.0.0.1/secret".into(),
                "https://bad.example/2".into(),
                "https://good.example/3".into(),
            ],
            FetchOpts::default(),
            4,
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert_eq!(pages.len(), 4);
    assert_eq!(pages[0].url, "https://good.example/1");
    assert_eq!(pages[0].content, "ok:https://good.example/1");
    assert_eq!(pages[1].url, "http://127.0.0.1/secret");
    assert!(
        pages[1].content.starts_with("[fetch failed:"),
        "{}",
        pages[1].content
    );
    assert_eq!(pages[2].url, "https://bad.example/2");
    assert!(
        pages[2].content.contains("503"),
        "expected 503 placeholder, got {}",
        pages[2].content
    );
    assert_eq!(pages[3].url, "https://good.example/3");
    assert_eq!(pages[3].content, "ok:https://good.example/3");
    assert!(
        !net.recorded()
            .iter()
            .any(|(r, _)| r.url.contains("127.0.0.1")),
        "private IP must never touch the network"
    );

    // 2. All URLs failing still returns aligned placeholders in input order.
    let all_failed = svc
        .fetch_batch(
            vec![
                "https://bad.example/a".into(),
                "https://bad.example/b".into(),
            ],
            FetchOpts::default(),
            4,
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert_eq!(all_failed.len(), 2);
    assert_eq!(all_failed[0].url, "https://bad.example/a");
    assert!(all_failed[0].content.starts_with("[fetch failed:"));
    assert_eq!(all_failed[1].url, "https://bad.example/b");
    assert!(all_failed[1].content.starts_with("[fetch failed:"));
}

#[test]
fn test_structured_nested_microdata_itemref_base_and_jsonld_comments() {
    let html = r#"<!DOCTYPE html>
<html>
<head>
  <base href="https://cdn.example/docs/">
  <script type="application/ld+json">
  <!--
  [{"@type": "Event", "name": "Summit"}, {"@type": "Organization", "name": "Org"}]
  -->
  </script>
  <script type="application/ld+json">not valid json {</script>
</head>
<body>
  <div id="ext-tag" itemprop="keywords">rust</div>
  <article itemscope itemtype="https://schema.org/Article" itemref="ext-tag">
    <h1 itemprop="headline">Deep Microdata</h1>
    <time itemprop="datePublished" datetime="2026-05-01">May 1, 2026</time>
    <a itemprop="url" href="page.html">Link</a>
    <span itemprop="keywords">wasm</span>
    <div itemprop="author" itemscope itemtype="https://schema.org/Person">
      <span itemprop="name">Grace Hopper</span>
    </div>
  </article>
</body>
</html>"#;

    let out = extract_structured_from_html(html, "https://site.example/orig", 200);
    // Array JSON-LD wrapped in HTML comments parsed into 2 objects; broken block ignored.
    assert_eq!(out.json_ld.len(), 2);
    assert_eq!(out.json_ld[0]["name"], "Summit");
    assert_eq!(out.json_ld[1]["name"], "Org");

    // Microdata resolved nested Person, itemref, <base href>, <time datetime>, and multi-value keywords.
    assert_eq!(out.microdata.len(), 1);
    let art = &out.microdata[0];
    assert_eq!(art["@type"], "Article");
    assert_eq!(art["headline"], "Deep Microdata");
    assert_eq!(art["datePublished"], "2026-05-01");
    assert_eq!(art["url"], "https://cdn.example/docs/page.html");
    assert_eq!(art["author"]["@type"], "Person");
    assert_eq!(art["author"]["name"], "Grace Hopper");
    let kws = art["keywords"]
        .as_array()
        .expect("multi-value itemprop array");
    assert_eq!(kws.len(), 2);
}

#[test]
fn test_html_fallback_recovers_tiny_minimal_page_without_boilerplate() {
    let html = r#"<html><head><title>Short Note</title></head><body>
      <nav>Home | About | Login</nav>
      <main><p>Tiny note.</p></main>
      <footer>Copyright 2026</footer>
    </body></html>"#;
    let res = extract_html(html, "https://example.com/note");
    assert_eq!(res.title, "Short Note");
    assert!(res.markdown.contains("Tiny note."), "{}", res.markdown);
    assert!(!res.markdown.contains("Copyright 2026"), "{}", res.markdown);
}

#[tokio::test]
async fn test_download_ssrf_does_not_burn_slot_and_429_reports_retry_after() {
    let dir = TempDir::new().unwrap();
    let mut cfg = Config::default();
    cfg.download_enabled = true;
    cfg.download_dir = dir.path().join("dl");

    let net = Arc::new(MockNet::with_responses(vec![
        // Redirect to private IP
        Ok(HttpBytes::new(
            "https://x.example/redir",
            302,
            vec![("location".into(), "http://127.0.0.1/secret".into())],
            "",
        )),
        // 429 with Retry-After
        Ok(HttpBytes::new(
            "https://x.example/limited",
            429,
            vec![("Retry-After".into(), "42".into())],
            "slow down",
        )),
    ]));
    let svc = make_service(
        net.clone(),
        Arc::new(MemStore::new()),
        cfg,
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );

    let cx = CallCx::default();
    // Blocked initial URL must NOT consume a download slot.
    assert!(matches!(
        svc.download("http://127.0.0.1/bad", &cx).await,
        Err(FsmError::UnsafeUrl(_))
    ));
    assert_eq!(cx.downloads_used.load(Ordering::SeqCst), 0);

    // Redirect to private IP is blocked on hop.
    assert!(matches!(
        svc.download("https://x.example/redir", &cx).await,
        Err(FsmError::UnsafeUrl(_))
    ));

    // HTTP 429 maps to FsmError::RateLimited with retry_after_s = Some(42).
    let err429 = svc
        .download("https://x.example/limited", &cx)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err429,
            FsmError::RateLimited {
                retry_after_s: Some(42),
                ..
            }
        ),
        "{err429:?}"
    );
}

#[tokio::test]
async fn test_gnews_fallback_resilience_on_bad_ts_and_rpc_500() {
    assert_eq!(build_freq("ID", "not-a-number", "SIG"), None);

    let gn_url = "https://news.google.com/articles/CBMiFailRpc";
    let shell_html = r#"<html><head><title>Shell</title></head><body><c-wiz data-n-a-id="CBMiFailRpc" data-n-a-ts="1700000000" data-n-a-sg="SIG"></c-wiz><article><p>Fallback shell body text.</p></article></body></html>"#;
    let net = Arc::new(MockNet::with_responses(vec![
        // 1. gnews shell GET
        Ok(HttpBytes::new(
            gn_url,
            200,
            vec![("content-type".into(), "text/html".into())],
            shell_html,
        )),
        // 2. gnews batchexecute POST -> 500 error
        Ok(HttpBytes::new(
            "https://news.google.com/_/DotsSplashUi/data/batchexecute",
            500,
            vec![],
            "server error",
        )),
        // 3. Fallback fetch of original gn_url succeeds gracefully
        Ok(HttpBytes::new(
            gn_url,
            200,
            vec![("content-type".into(), "text/html".into())],
            shell_html,
        )),
    ]));
    let svc = make_service(
        net,
        Arc::new(MemStore::new()),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );
    let FetchOutcome::Page(page) = svc
        .fetch(gn_url, FetchOpts::default(), &CallCx::default())
        .await
        .unwrap()
    else {
        panic!("expected Page");
    };
    assert_eq!(page.url, gn_url);
    assert!(page.content.contains("Fallback shell body text."));
}
