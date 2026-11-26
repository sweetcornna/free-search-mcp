//! Review Round 1 regression tests covering EVERY finding (`B1–B2`, `I1–I7`, `m1–m7`)
//! and all missing test cases listed in `D/handoff/w4a-review.md` (`B3` is in `test_corpus_golden.rs`).

mod common;

use async_trait::async_trait;
use common::{MemStore, MockDocs, MockNet, MockSearch, make_png, make_service, offline_ssrf_check};
use fsm_core::config::Config;
use fsm_core::text::estimate_tokens;
use fsm_core::{
    CallCx, DocSlice, FetchApi, FetchMethod, FetchOpts, FetchOutcome, FsmError, HttpBytes,
    HttpMethod, HttpRequest, NetClient, NetCx, PageResult, RawRequestOpts, RenderMode, StoreApi,
    TransportProfile,
};
use fsm_extract::fetch::{MAX_INLINE_IMAGE_BYTES, MAX_INLINE_IMAGES_PER_EXEC};
use fsm_extract::html::{MAX_DOM_INPUT_BYTES, extract_html, html_to_markdown};
use fsm_extract::request::HostRateLimiter;
use fsm_extract::structured::extract_structured_from_html;
use fsm_net::HttpClient;
use futures::future::join_all;
use serde_json::json;
use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::Url;

// ---------------------------------------------------------------------------
// B1 + I4: Deep nesting must not stack-overflow (2 MiB thread) or stall CPU
// ---------------------------------------------------------------------------

#[test]
fn test_b1_i4_deep_nesting_does_not_stack_overflow_or_stall() {
    let handle = std::thread::Builder::new()
        .name("b1-small-stack".into())
        .stack_size(2 << 20)
        .spawn(|| {
            for tag in ["ul", "blockquote", "div"] {
                let html = format!(
                    "<html><head><title>Deep {tag}</title></head><body>{}hello world{}</body></html>",
                    format!("<{tag}>").repeat(5_000),
                    format!("</{tag}>").repeat(5_000),
                );

                let t0 = Instant::now();
                let md = html_to_markdown(&html);
                assert!(
                    t0.elapsed() < Duration::from_secs(1),
                    "html_to_markdown({tag}x5000) took {:?}",
                    t0.elapsed()
                );
                assert!(md.contains("hello world"), "lost inner text for {tag}: {md}");
                assert!(md.contains("nesting depth"), "missing fallback note for {tag}: {md}");

                let t1 = Instant::now();
                let ex = extract_html(&html, "https://example.com/deep");
                assert!(
                    t1.elapsed() < Duration::from_secs(1),
                    "extract_html({tag}x5000) took {:?}",
                    t1.elapsed()
                );
                assert!(ex.truncated, "expected truncated flag for deep {tag}");
                assert!(
                    ex.markdown.contains("hello world"),
                    "lost inner text for {tag}: {}",
                    ex.markdown
                );
                assert!(
                    ex.markdown.contains("nesting depth"),
                    "missing fallback note for {tag}: {}",
                    ex.markdown
                );

                let t2 = Instant::now();
                let st = extract_structured_from_html(&html, "https://example.com/deep", 200);
                assert!(
                    t2.elapsed() < Duration::from_secs(1),
                    "extract_structured_from_html({tag}x5000) took {:?}",
                    t2.elapsed()
                );
                assert!(st.hint.is_some(), "expected hint for deep {tag}");
            }
        })
        .expect("spawn 2 MiB stack thread");

    handle
        .join()
        .expect("2 MiB stack thread must not panic or abort");
}

#[tokio::test]
async fn test_b1_i4_async_html_to_md_and_input_byte_cap() {
    let deep = format!(
        "<html><body>{}needle{}</body></html>",
        "<div>".repeat(5_000),
        "</div>".repeat(5_000)
    );
    let oversized = format!(
        "<html><head><title>Huge</title></head><body><article><p>Lead paragraph.</p><p>{}</p></article></body></html>",
        "word ".repeat(MAX_DOM_INPUT_BYTES / 5 + 50_000)
    );
    let net = Arc::new(MockNet::with_responses(vec![Ok(HttpBytes::new(
        "https://example.com/huge",
        200,
        vec![("content-type".into(), "text/html".into())],
        oversized,
    ))]));
    let svc = make_service(
        net,
        Arc::new(MemStore::new()),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );

    let md = svc.html_to_md(&deep).await.unwrap();
    assert!(md.contains("needle"), "{md}");

    let FetchOutcome::Page(page) = svc
        .fetch(
            "https://example.com/huge",
            FetchOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap()
    else {
        panic!("expected Page");
    };
    assert!(page.truncated, "DOM byte cap must surface page.truncated");
    assert!(page.content.contains("Lead paragraph."));
}

// ---------------------------------------------------------------------------
// B2: Per-host rate-limit / Retry-After penalty isolation (real fsm-net)
// ---------------------------------------------------------------------------

async fn spawn_test_http_server(bind_addr: &str) -> Option<(String, tokio::task::JoinHandle<()>)> {
    let listener = TcpListener::bind(bind_addr).await.ok()?;
    let addr = listener.local_addr().ok()?;
    let base = match addr {
        std::net::SocketAddr::V4(v4) => format!("http://{}:{}", v4.ip(), v4.port()),
        std::net::SocketAddr::V6(v6) => format!("http://[{}]:{}", v6.ip(), v6.port()),
    };
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/");
                let resp = if path.starts_with("/busy") {
                    "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 120\r\nContent-Type: text/plain\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbusy".to_owned()
                } else if path.starts_with("/meta") {
                    let body = r#"<!doctype html><html><head><script type="application/ld+json">{"@type":"Article","headline":"Host B Meta"}</script></head><body>ok</body></html>"#;
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                } else if path.starts_with("/api") {
                    let body = r#"{"host":"B","ok":true}"#;
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                } else if path.starts_with("/file.txt") {
                    let body = "downloaded-from-host-B";
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                } else {
                    let body = "<!doctype html><html><head><title>Host B</title></head><body><article><h1>Host B Article</h1><p>This is a full article paragraph from host B so trafilatura extracts it cleanly.</p><p>Second paragraph confirms host B was never blocked by host A.</p></article></body></html>";
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                let _ = stream.write_all(resp.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    Some((base, handle))
}

#[tokio::test]
async fn test_b2_rate_limit_penalty_isolated_per_host_with_real_fsm_net() {
    let (host_a, task_a) = spawn_test_http_server("127.0.0.1:0")
        .await
        .expect("bind 127.0.0.1:0");
    let (host_b, task_b) = match spawn_test_http_server("[::1]:0").await {
        Some(pair) => pair,
        None => {
            // Fallback for environments without IPv6 loopback: use 127.0.0.2.
            spawn_test_http_server("127.0.0.2:0")
                .await
                .expect("bind [::1]:0 or 127.0.0.2:0")
        }
    };

    let dir = TempDir::new().unwrap();
    let mut cfg = Config::default();
    cfg.allow_private_hosts = true;
    cfg.download_enabled = true;
    cfg.download_dir = dir.path().join("dl");

    let client = Arc::new(HttpClient::new(&cfg).expect("build real fsm_net::HttpClient"));
    let svc = make_service(
        client,
        Arc::new(MemStore::new()),
        cfg,
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );
    let cx = CallCx::default();

    // 1. web.fetch: host A returns 503 + Retry-After: 120 -> host A is penalized, host B succeeds!
    let err_fetch_a = svc
        .fetch(&format!("{host_a}/busy"), FetchOpts::default(), &cx)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err_fetch_a,
            FsmError::Upstream {
                status: Some(503),
                ..
            }
        ),
        "expected 503 from host A, got {err_fetch_a:?}"
    );
    // Confirm host A itself IS now penalized (proving fsm-net penalty fired).
    let err_fetch_a2 = svc
        .fetch(&format!("{host_a}/ok"), FetchOpts::default(), &cx)
        .await
        .unwrap_err();
    assert!(
        matches!(err_fetch_a2, FsmError::RateLimited { .. }),
        "expected host A to be penalized, got {err_fetch_a2:?}"
    );
    // Host B MUST succeed unaffected.
    let FetchOutcome::Page(page_b) = svc
        .fetch(&format!("{host_b}/page"), FetchOpts::default(), &cx)
        .await
        .expect("host B web.fetch must not be blocked by host A's Retry-After")
    else {
        panic!("expected Page");
    };
    assert!(
        page_b.content.contains("never blocked by host A"),
        "{}",
        page_b.content
    );

    // 2. web.structured: host A 503 + Retry-After -> host B succeeds!
    let _ = svc.structured(&format!("{host_a}/busy"), &cx).await;
    let meta_b = svc
        .structured(&format!("{host_b}/meta"), &cx)
        .await
        .expect("host B web.structured must not be blocked by host A");
    assert_eq!(meta_b.json_ld.len(), 1);
    assert_eq!(meta_b.json_ld[0]["headline"], "Host B Meta");

    // 3. web.download: host A 503 + Retry-After -> host B succeeds!
    let _ = svc.download(&format!("{host_a}/busy"), &cx).await;
    let dl_b = svc
        .download(&format!("{host_b}/file.txt"), &cx)
        .await
        .expect("host B web.download must not be blocked by host A");
    assert_eq!(
        fs::read_to_string(&dl_b.saved_path).unwrap(),
        "downloaded-from-host-B"
    );

    // 4. web.request: host A 503 + Retry-After -> host B succeeds!
    let _ = svc
        .request(RawRequestOpts::new(format!("{host_a}/busy")), &cx)
        .await;
    let req_b = svc
        .request(RawRequestOpts::new(format!("{host_b}/api")), &cx)
        .await
        .expect("host B web.request must not be blocked by host A");
    assert_eq!(req_b.status, 200);
    assert_eq!(req_b.json, Some(json!({"host": "B", "ok": true})));

    task_a.abort();
    task_b.abort();
}

// ---------------------------------------------------------------------------
// I1: Cache round-trip with URL-normalizing & redirecting mock
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_i1_cache_round_trip_url_normalizing_and_redirecting_mock() {
    let net = Arc::new(MockNet::with_handler(|req, _, _| {
        let normalized = Url::parse(&req.url).unwrap().to_string();
        if req.url == "http://x.example/a" {
            return Ok(HttpBytes::new(
                normalized,
                301,
                vec![("location".into(), "https://x.example/a".into())],
                "",
            ));
        }
        Ok(HttpBytes::new(
            normalized,
            200,
            vec![("content-type".into(), "text/plain".into())],
            format!("body for {}", req.url),
        ))
    }));
    let store = Arc::new(MemStore::new());
    let svc = make_service(
        net.clone(),
        store.clone(),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );
    let cx = CallCx::default();

    // 1. Bare domain without trailing slash: mock normalizes final_url to "https://x.example/".
    let FetchOutcome::Page(p1) = svc
        .fetch("https://x.example", FetchOpts::default(), &cx)
        .await
        .unwrap()
    else {
        panic!("expected Page");
    };
    assert_eq!(p1.method, FetchMethod::Http);
    assert_eq!(net.recorded().len(), 1);

    let FetchOutcome::Page(p2) = svc
        .fetch("https://x.example", FetchOpts::default(), &cx)
        .await
        .unwrap()
    else {
        panic!("expected Page");
    };
    assert_eq!(p2.method, FetchMethod::Cache, "2nd fetch must hit cache");
    assert_eq!(
        net.recorded().len(),
        1,
        "2nd fetch must make 0 net requests"
    );

    // Trailing-slash variant also hits cache with 0 net requests.
    let FetchOutcome::Page(p3) = svc
        .fetch("https://x.example/", FetchOpts::default(), &cx)
        .await
        .unwrap()
    else {
        panic!("expected Page");
    };
    assert_eq!(p3.method, FetchMethod::Cache);
    assert_eq!(net.recorded().len(), 1);

    // Direct store.get_page(<requested url>) must hit!
    assert!(
        store
            .get_page("https://x.example", None)
            .await
            .unwrap()
            .is_some(),
        "store.get_page on requested bare URL must hit"
    );

    // 2. Redirecting URL: http://x.example/a -> 301 -> https://x.example/a.
    let before = net.recorded().len();
    let FetchOutcome::Page(r1) = svc
        .fetch("http://x.example/a", FetchOpts::default(), &cx)
        .await
        .unwrap()
    else {
        panic!("expected Page");
    };
    assert_eq!(r1.method, FetchMethod::Http);
    assert_eq!(r1.url, "https://x.example/a");
    assert_eq!(net.recorded().len(), before + 2);

    let FetchOutcome::Page(r2) = svc
        .fetch("http://x.example/a", FetchOpts::default(), &cx)
        .await
        .unwrap()
    else {
        panic!("expected Page");
    };
    assert_eq!(
        r2.method,
        FetchMethod::Cache,
        "2nd fetch of redirected URL must hit cache"
    );
    assert_eq!(
        net.recorded().len(),
        before + 2,
        "0 extra requests on cache hit"
    );

    assert!(
        store
            .get_page("http://x.example/a", None)
            .await
            .unwrap()
            .is_some(),
        "store.get_page(<requested redirected url>) must hit"
    );
    assert!(
        store
            .get_page("https://x.example/a", None)
            .await
            .unwrap()
            .is_some(),
        "store.get_page(<final url>) must also hit"
    );
}

// ---------------------------------------------------------------------------
// I2: fetchBatch alignment & measured concurrency cap <= 8
// ---------------------------------------------------------------------------

struct ConcurrencyMeasuringNet {
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
    delay: Duration,
}

#[async_trait]
impl NetClient for ConcurrencyMeasuringNet {
    async fn send(
        &self,
        req: HttpRequest,
        _profile: TransportProfile,
        _cx: &NetCx,
    ) -> Result<HttpBytes, FsmError> {
        offline_ssrf_check(&req.url, false)?;
        let cur = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(cur, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);

        if req.url.contains("/missing") {
            return Ok(HttpBytes::new(req.url, 404, vec![], "not found"));
        }
        Ok(HttpBytes::new(
            req.url.clone(),
            200,
            vec![("content-type".into(), "text/plain".into())],
            format!("ok:{}", req.url),
        ))
    }

    fn check_url_offline(&self, url: &str) -> Result<(), FsmError> {
        offline_ssrf_check(url, false)
    }
}

#[tokio::test]
async fn test_i2_fetch_batch_mixed_alignment_and_concurrency_cap() {
    let net = Arc::new(ConcurrencyMeasuringNet {
        in_flight: AtomicUsize::new(0),
        max_in_flight: AtomicUsize::new(0),
        delay: Duration::from_millis(30),
    });
    let svc = make_service(
        net.clone(),
        Arc::new(MemStore::new()),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );

    // 16 URLs requested with max_concurrency = 16 -> must actually run concurrently AND cap at <= 8.
    let urls: Vec<String> = (0..16)
        .map(|i| format!("https://batch.example/{i}"))
        .collect();
    let pages = svc
        .fetch_batch(urls.clone(), FetchOpts::default(), 16, &CallCx::default())
        .await
        .unwrap();
    assert_eq!(pages.len(), 16);
    for (i, p) in pages.iter().enumerate() {
        assert_eq!(p.url, urls[i]);
    }
    let peak = net.max_in_flight.load(Ordering::SeqCst);
    assert!(
        (2..=8).contains(&peak),
        "expected measured max_in_flight in 2..=8, got {peak}"
    );

    // Mixed success / 404 / unsafe URL preserves exact input length and order.
    let mixed = vec![
        "https://batch.example/one".to_owned(),
        "https://batch.example/missing".to_owned(),
        "http://127.0.0.1/private".to_owned(),
        "https://batch.example/four".to_owned(),
    ];
    let res = svc
        .fetch_batch(mixed.clone(), FetchOpts::default(), 4, &CallCx::default())
        .await
        .unwrap();
    assert_eq!(res.len(), 4);
    assert_eq!(res[0].content, "ok:https://batch.example/one");
    assert_eq!(res[1].url, "https://batch.example/missing");
    assert!(res[1].content.starts_with("[fetch failed:") && res[1].content.contains("404"));
    assert_eq!(res[2].url, "http://127.0.0.1/private");
    assert!(res[2].content.starts_with("[fetch failed:"));
    assert_eq!(res[3].content, "ok:https://batch.example/four");
}

// ---------------------------------------------------------------------------
// I3: Lenient JSON-LD (raw newline, trailing comma, // and /* */ comments)
// ---------------------------------------------------------------------------

#[test]
fn test_i3_lenient_jsonld_raw_newline_trailing_comma_and_comments() {
    let html = concat!(
        r#"<!DOCTYPE html><html><head><title>Lenient Test</title>"#,
        "<script type=\"application/ld+json\">{\n",
        "  // CMS comment\n",
        "  \"@context\": \"https://schema.org\",\n",
        "  \"@type\": \"NewsArticle\",\n",
        "  \"headline\": \"Hello\",\n",
        "  \"description\": \"line one\nline two\",\n",
        "  \"datePublished\": \"2024-03-29T10:00:00Z\",\n",
        "  /* block comment */\n",
        "  \"author\": {\"@type\": \"Person\", \"name\": \"Jane Doe\",},\n",
        "}</script>",
        r#"<script type="application/ld+json">{"@type":"Event","name":"Conf","startDate":"2025-01-01",}</script>"#,
        r#"</head><body><article><h1>Hello</h1><p>This is a full news article body paragraph for trafilatura extraction testing.</p><p>A second body paragraph confirms clean extraction.</p></article></body></html>"#
    );

    let st = extract_structured_from_html(html, "https://news.example/story", 200);
    assert_eq!(
        st.json_ld.len(),
        2,
        "both lenient JSON-LD blocks must parse: {st:?}"
    );
    assert_eq!(st.json_ld[0]["author"]["name"], "Jane Doe");
    assert_eq!(st.json_ld[0]["description"], "line one\nline two");
    assert_eq!(st.json_ld[1]["startDate"], "2025-01-01");
    assert!(
        st.hint.is_none(),
        "valid JSON-LD must not emit bot-block hint: {:?}",
        st.hint
    );

    let ex = extract_html(html, "https://news.example/story");
    assert_eq!(ex.author.as_deref(), Some("Jane Doe"));
    assert_eq!(ex.published_date.as_deref(), Some("2024-03-29"));
}

// ---------------------------------------------------------------------------
// I5: Hostile itemref & malformed HTML/base never panic or return Internal
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_i5_hostile_itemref_and_malformed_inputs_never_panic_or_internal_error() {
    // Reviewer's exact failing repro plus bracket/backslash/quote injection.
    let hostile_html = r#"<!DOCTYPE html>
<html>
<head><base href="http://[::invalid-base-url"><meta name="description" content="still works"></head>
<body>
  <div itemscope itemtype="https://schema.org/Thing" itemref='a\" b]c[d'>
    <span itemprop="name">n</span>
  </div>
  <p id='a\"' itemprop="description">d</p>
  <p id='b]c[d' itemprop="alternateName">alt</p>
</body>
</html>"#;

    let st = extract_structured_from_html(hostile_html, "not a valid url", 200);
    assert_eq!(st.microdata.len(), 1, "{st:?}");
    assert_eq!(st.microdata[0]["name"], "n");
    assert_eq!(st.microdata[0]["description"], "d");
    assert_eq!(st.microdata[0]["alternateName"], "alt");

    // Also verify end-to-end through FetchService::structured (must not return Internal error).
    let net = Arc::new(MockNet::with_responses(vec![Ok(HttpBytes::new(
        "https://site.example/hostile",
        200,
        vec![("content-type".into(), "text/html".into())],
        hostile_html,
    ))]));
    let svc = make_service(
        net,
        Arc::new(MemStore::new()),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );
    let out = svc
        .structured("https://site.example/hostile", &CallCx::default())
        .await
        .expect("hostile itemref must not produce Internal error");
    assert_eq!(out.microdata[0]["name"], "n");
    assert_eq!(out.microdata[0]["description"], "d");
}

// ---------------------------------------------------------------------------
// I6: M6 taint blocks redirects to unfetched hosts, gnews, and remote docs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_i6_tainted_exec_blocks_unfetched_redirects_gnews_and_remote_docs() {
    let net = Arc::new(MockNet::with_handler(|req, _, _| {
        if req.url == "https://allowed.example/init" {
            return Ok(HttpBytes::new(
                req.url.clone(),
                200,
                vec![("content-type".into(), "text/plain".into())],
                "initial public page",
            ));
        }
        if req.url == "https://allowed.example/redir" {
            return Ok(HttpBytes::new(
                req.url.clone(),
                302,
                vec![(
                    "location".into(),
                    "https://evil.example/exfil?s=secret".into(),
                )],
                "",
            ));
        }
        Ok(HttpBytes::new(
            req.url.clone(),
            200,
            vec![("content-type".into(), "text/plain".into())],
            "should never be reached",
        ))
    }));
    let docs = Arc::new(MockDocs::default());
    let dir = TempDir::new().unwrap();
    let mut cfg = Config::default();
    cfg.download_enabled = true;
    cfg.download_dir = dir.path().join("dl");

    let svc = make_service(
        net.clone(),
        Arc::new(MemStore::new()),
        cfg,
        docs.clone(),
        Arc::new(MockSearch::default()),
    );

    let cx = CallCx::default();
    // Pre-taint fetch populates fetched_hosts = {"allowed.example"}.
    svc.fetch("https://allowed.example/init", FetchOpts::default(), &cx)
        .await
        .unwrap();
    // Local doc read taints the exec.
    cx.mark_local_doc_read();

    // 1. web.fetch redirect allowed.example -> evil.example MUST be refused.
    let err = svc
        .fetch("https://allowed.example/redir", FetchOpts::default(), &cx)
        .await
        .unwrap_err();
    assert!(matches!(err, FsmError::PermissionDenied(_)), "{err:?}");
    assert!(
        !net.recorded()
            .iter()
            .any(|(r, _)| r.url.contains("evil.example")),
        "tainted redirect must never contact evil.example"
    );

    // 2. web.request redirect allowed.example -> evil.example MUST be refused.
    let err_req = svc
        .request(RawRequestOpts::new("https://allowed.example/redir"), &cx)
        .await
        .unwrap_err();
    assert!(
        matches!(err_req, FsmError::PermissionDenied(_)),
        "{err_req:?}"
    );
    assert!(
        !net.recorded()
            .iter()
            .any(|(r, _)| r.url.contains("evil.example"))
    );

    // 3. Google News URL while tainted MUST NOT run gnews resolution (0 batchexecute calls).
    let reqs_before_gn = net.recorded().len();
    let err_gn = svc
        .fetch(
            "https://news.google.com/articles/CBMiSecretToken",
            FetchOpts::default(),
            &cx,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err_gn, FsmError::PermissionDenied(_)),
        "{err_gn:?}"
    );
    assert_eq!(
        net.recorded().len(),
        reqs_before_gn,
        "gnews must make 0 requests when tainted"
    );

    // Even if news.google.com was previously fetched, gnews resolution is still skipped when tainted!
    cx.fetched_hosts
        .lock()
        .await
        .insert("news.google.com".to_owned());
    let _ = svc
        .fetch(
            "https://news.google.com/articles/CBMiSecretToken2",
            FetchOpts::default(),
            &cx,
        )
        .await;
    assert!(
        !net.recorded()
            .iter()
            .any(|(r, _)| r.url.contains("batchexecute")),
        "tainted exec must never call gnews batchexecute"
    );

    // 4. Remote PDF on unfetched host MUST be refused before DocApi::read.
    let err_pdf = svc
        .fetch("https://evil.example/secret.pdf", FetchOpts::default(), &cx)
        .await
        .unwrap_err();
    assert!(
        matches!(err_pdf, FsmError::PermissionDenied(_)),
        "{err_pdf:?}"
    );
    assert!(docs.calls.lock().unwrap().is_empty());

    // 5. web.download on unfetched host MUST be refused without consuming a slot.
    let err_dl = svc
        .download("https://evil.example/x.bin", &cx)
        .await
        .unwrap_err();
    assert!(
        matches!(err_dl, FsmError::PermissionDenied(_)),
        "{err_dl:?}"
    );
    assert_eq!(cx.downloads_used.load(Ordering::SeqCst), 0);
}

// ---------------------------------------------------------------------------
// I7: Global fetch_rate_limit_rpm paces network fetches only (not cache hits)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_i7_fetch_rate_limit_rpm_paces_network_only_not_cache() {
    let mut cfg = Config::default();
    cfg.fetch_rate_limit_rpm = 1; // 1 req/min (burst 1, interval 60s)

    let net = Arc::new(MockNet::with_handler(|req, _, _| {
        Ok(HttpBytes::new(
            req.url.clone(),
            200,
            vec![("content-type".into(), "text/plain".into())],
            format!("body {}", req.url),
        ))
    }));
    let svc = make_service(
        net.clone(),
        Arc::new(MemStore::new()),
        cfg,
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );

    // 1st network fetch uses the 1 burst token immediately.
    let FetchOutcome::Page(p1) = svc
        .fetch(
            "https://paced.example/one",
            FetchOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap()
    else {
        panic!("expected Page");
    };
    assert_eq!(p1.method, FetchMethod::Http);
    assert_eq!(net.recorded().len(), 1);

    // Repeat fetch hits cache and MUST NOT wait on the fetch limiter.
    let t0 = Instant::now();
    let FetchOutcome::Page(p2) = svc
        .fetch(
            "https://paced.example/one",
            FetchOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap()
    else {
        panic!("expected Page");
    };
    assert_eq!(p2.method, FetchMethod::Cache);
    assert!(t0.elapsed() < Duration::from_millis(100));

    // 2nd distinct network fetch with 50 ms budget must be paced -> Timeout before socket.
    let short_cx = CallCx::new("paced", Instant::now() + Duration::from_millis(50));
    let err = svc
        .fetch("https://paced.example/two", FetchOpts::default(), &short_cx)
        .await
        .unwrap_err();
    assert!(matches!(err, FsmError::Timeout(_)), "{err:?}");
    assert_eq!(net.recorded().len(), 1, "paced fetch must not hit network");
}

// ---------------------------------------------------------------------------
// m1: HEAD with 303 redirect stays HEAD and returns empty body
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_m1_head_303_redirect_stays_head_with_empty_body() {
    let net = Arc::new(MockNet::with_responses(vec![
        Ok(HttpBytes::new(
            "https://api.example/start",
            303,
            vec![("location".into(), "https://api.example/target".into())],
            "",
        )),
        Ok(HttpBytes::new(
            "https://api.example/target",
            200,
            vec![("content-type".into(), "application/pdf".into())],
            b"%PDF-1.7-should-not-trigger-body-or-binary-err".as_slice(),
        )),
    ]));
    let svc = make_service(
        net.clone(),
        Arc::new(MemStore::new()),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );
    let mut opts = RawRequestOpts::new("https://api.example/start");
    opts.method = Some(HttpMethod::Head);
    let resp = svc.request(opts, &CallCx::default()).await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, "");
    assert!(resp.json.is_none());
    let rec = net.recorded();
    assert_eq!(rec.len(), 2);
    assert_eq!(rec[1].0.method, HttpMethod::Head);
}

// ---------------------------------------------------------------------------
// m2: Truncated asset response rejected and never inlined
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_m2_truncated_asset_rejected_and_never_inlined() {
    let make_trunc = || {
        let mut resp = HttpBytes::new(
            "https://cdn.example/huge.png",
            200,
            vec![("content-type".into(), "image/png".into())],
            make_png(100, 100),
        );
        resp.truncated = true;
        Ok(resp)
    };
    let net = Arc::new(MockNet::with_responses(vec![make_trunc(), make_trunc()]));
    let svc = make_service(
        net,
        Arc::new(MemStore::new()),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );

    let err_desc = svc
        .fetch(
            "https://cdn.example/huge.png",
            FetchOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err_desc, FsmError::MaxBytesExceeded(_)),
        "{err_desc:?}"
    );

    let cx = CallCx::default();
    let mut inline_opts = FetchOpts::default();
    inline_opts.inline_image = true;
    let err_inline = svc
        .fetch("https://cdn.example/huge.png", inline_opts, &cx)
        .await
        .unwrap_err();
    assert!(
        matches!(err_inline, FsmError::MaxBytesExceeded(_)),
        "{err_inline:?}"
    );
    assert!(
        cx.images.lock().await.is_empty(),
        "truncated image must never be inlined"
    );
}

// ---------------------------------------------------------------------------
// m3: render: "browser" errors BEFORE spending gnews network calls
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_m3_render_browser_rejects_before_gnews_resolution() {
    let net = Arc::new(MockNet::new());
    let svc = make_service(
        net.clone(),
        Arc::new(MemStore::new()),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );
    let mut opts = FetchOpts::default();
    opts.render = RenderMode::Browser;
    let err = svc
        .fetch(
            "https://news.google.com/articles/CBMiTestBrowser",
            opts,
            &CallCx::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, FsmError::BrowserUnavailable(_)), "{err:?}");
    assert!(
        net.recorded().is_empty(),
        "must make 0 gnews requests for render:browser"
    );
}

// ---------------------------------------------------------------------------
// m4: GCRA rejected / cancelled calls must not advance TAT for later callers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_m4_gcra_rejected_and_cancelled_calls_do_not_penalize_later_callers() {
    // 5 req/s (200 ms interval), burst 1.
    let limiter = HostRateLimiter::new(5.0, 1);
    limiter
        .acquire("h.example", &CallCx::default())
        .await
        .unwrap();

    // 5 rejected calls (budget 5 ms < 200 ms) MUST NOT push TAT out by 1000 ms.
    for _ in 0..5 {
        let short_cx = CallCx::new("short", Instant::now() + Duration::from_millis(5));
        let err = limiter.acquire("h.example", &short_cx).await.unwrap_err();
        assert!(matches!(err, FsmError::Timeout(_)), "{err:?}");
    }

    // 1 cancelled waiter mid-sleep MUST roll back its slot too.
    let cancel_cx = CallCx::default();
    let tok = cancel_cx.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        tok.cancel();
    });
    let err_cancel = limiter.acquire("h.example", &cancel_cx).await.unwrap_err();
    assert!(matches!(err_cancel, FsmError::Timeout(0)), "{err_cancel:?}");

    // Wait just past the single 200 ms interval: next caller must succeed immediately!
    tokio::time::sleep(Duration::from_millis(210)).await;
    let t0 = Instant::now();
    limiter
        .acquire("h.example", &CallCx::default())
        .await
        .unwrap();
    assert!(
        t0.elapsed() < Duration::from_millis(80),
        "rejected/cancelled calls leaked TAT delay: {:?}",
        t0.elapsed()
    );
}

// ---------------------------------------------------------------------------
// m5: Truncated doc not cached as full page; cached truncated doc keeps hint
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_m5_truncated_document_not_cached_as_full_and_cached_doc_keeps_hint() {
    let mut trunc_slice = DocSlice::new("https://docs.example/big.pdf", "pdf", "First 50k chars");
    trunc_slice.title = Some("Big PDF".into());
    trunc_slice.returned_chars = 15;
    trunc_slice.total_chars = 200_000;
    trunc_slice.truncated = true;

    let store = Arc::new(MemStore::new());
    let svc = make_service(
        Arc::new(MockNet::new()),
        store.clone(),
        Config::default(),
        Arc::new(MockDocs::new(trunc_slice)),
        Arc::new(MockSearch::default()),
    );
    let FetchOutcome::Page(page) = svc
        .fetch(
            "https://docs.example/big.pdf",
            FetchOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap()
    else {
        panic!("expected Page");
    };
    assert!(page.truncated);
    assert!(page.content.contains("doc.read"));
    assert!(
        store.stored_page("https://docs.example/big.pdf").is_none(),
        "truncated first-slice PDF must not be persisted as full page in page cache"
    );

    // If a truncated Document page IS pre-seeded in cache (without hint), cache hit re-appends hint.
    let mut seeded = PageResult::new(
        "https://docs.example/seeded.pdf",
        "Seeded PDF",
        "Cached prefix without hint",
        FetchMethod::Document,
    );
    seeded.total_chars = 120_000;
    seeded.truncated = true;
    store.seed_page(seeded, 5);

    let FetchOutcome::Page(hit) = svc
        .fetch(
            "https://docs.example/seeded.pdf",
            FetchOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap()
    else {
        panic!("expected Page");
    };
    assert_eq!(hit.method, FetchMethod::Cache);
    assert!(hit.truncated);
    assert!(hit.content.contains("doc.read"), "{}", hit.content);
    assert!(hit.content.contains("doc.outline"), "{}", hit.content);
}

// ---------------------------------------------------------------------------
// m6: Concurrent downloads serialized against download_dir_max_mb (no TOCTOU)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_m6_concurrent_downloads_respect_dir_cap_without_toctou() {
    let dir = TempDir::new().unwrap();
    let mut cfg = Config::default();
    cfg.download_enabled = true;
    cfg.download_dir = dir.path().join("dl");
    cfg.download_max_mb = 10;
    cfg.download_dir_max_mb = 1; // 1 MiB total dir cap

    let net = Arc::new(MockNet::with_handler(|req, _, _| {
        let tag = req.url.bytes().last().unwrap_or(b'0');
        Ok(HttpBytes::new(
            req.url.clone(),
            200,
            vec![("content-type".into(), "application/octet-stream".into())],
            vec![tag; 400 * 1024], // 400 KiB distinct payload per URL
        ))
    }));
    let svc = Arc::new(make_service(
        net,
        Arc::new(MemStore::new()),
        cfg.clone(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    ));
    let cx = Arc::new(CallCx::default());

    let futs = (0..5).map(|i| {
        let svc = Arc::clone(&svc);
        let cx = Arc::clone(&cx);
        async move {
            svc.download(&format!("https://files.example/blob-{i}.bin"), &cx)
                .await
        }
    });
    let results = join_all(futs).await;
    let ok_count = results.iter().filter(|r| r.is_ok()).count();
    let budget_err_count = results
        .iter()
        .filter(|r| matches!(r, Err(FsmError::BudgetExceeded(_))))
        .count();

    assert_eq!(
        ok_count, 2,
        "only two 400 KiB files fit under 1 MiB cap: {results:?}"
    );
    assert_eq!(
        budget_err_count, 3,
        "other 3 must fail with BudgetExceeded: {results:?}"
    );

    let total_disk_bytes: u64 = fs::read_dir(&cfg.download_dir)
        .unwrap()
        .flatten()
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum();
    assert!(
        total_disk_bytes <= 1024 * 1024,
        "disk total {total_disk_bytes} exceeded 1 MiB cap"
    );
}

// ---------------------------------------------------------------------------
// m7: Per-image (5 MiB) and per-exec (8 images / 20 MiB) inline image caps
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_m7_inline_images_capped_per_image_and_per_exec() {
    // 1. Single image > 5 MiB refused with BudgetExceeded pointing to web.download.
    let mut big_png = make_png(64, 64);
    big_png.resize(MAX_INLINE_IMAGE_BYTES as usize + 1024, 0);
    let net_big = Arc::new(MockNet::with_responses(vec![Ok(HttpBytes::new(
        "https://img.example/huge.png",
        200,
        vec![("content-type".into(), "image/png".into())],
        big_png,
    ))]));
    let svc_big = make_service(
        net_big,
        Arc::new(MemStore::new()),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );
    let cx_big = CallCx::default();
    let mut inline_opts = FetchOpts::default();
    inline_opts.inline_image = true;
    let err_big = svc_big
        .fetch("https://img.example/huge.png", inline_opts.clone(), &cx_big)
        .await
        .unwrap_err();
    assert!(
        matches!(err_big, FsmError::BudgetExceeded(_)),
        "{err_big:?}"
    );
    assert!(err_big.to_string().contains("web.download"), "{err_big}");
    assert!(cx_big.images.lock().await.is_empty());

    // 2. Count cap: 8 distinct inline images allowed; duplicate allowed; 9th rejected.
    let net_cnt = Arc::new(MockNet::with_handler(|req, _, _| {
        let idx: u32 = req
            .url
            .rsplit('/')
            .next()
            .and_then(|s| s.strip_suffix(".png"))
            .and_then(|s| s.parse().ok())
            .unwrap_or(1);
        Ok(HttpBytes::new(
            req.url.clone(),
            200,
            vec![("content-type".into(), "image/png".into())],
            make_png(idx + 1, idx + 1),
        ))
    }));
    let svc_cnt = make_service(
        net_cnt,
        Arc::new(MemStore::new()),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );
    let cx_cnt = CallCx::default();
    for i in 0..MAX_INLINE_IMAGES_PER_EXEC {
        svc_cnt
            .fetch(
                &format!("https://img.example/{i}.png"),
                inline_opts.clone(),
                &cx_cnt,
            )
            .await
            .unwrap();
    }
    assert_eq!(cx_cnt.images.lock().await.len(), MAX_INLINE_IMAGES_PER_EXEC);

    // Re-inlining identical image #0 deduplicates and succeeds.
    svc_cnt
        .fetch("https://img.example/0.png", inline_opts.clone(), &cx_cnt)
        .await
        .unwrap();

    // 9th distinct image exceeds per-exec count cap.
    let err_9th = svc_cnt
        .fetch("https://img.example/99.png", inline_opts.clone(), &cx_cnt)
        .await
        .unwrap_err();
    assert!(
        matches!(err_9th, FsmError::BudgetExceeded(_)),
        "{err_9th:?}"
    );
    assert!(err_9th.to_string().contains("web.download"), "{err_9th}");

    // 3. Aggregate 20 MiB cap: four 4.5 MiB images fit (18 MiB), 5th (22.5 MiB) rejected.
    let net_tot = Arc::new(MockNet::with_handler(|req, _, _| {
        let tag = req
            .url
            .rsplit('/')
            .next()
            .and_then(|s| s.as_bytes().get(1))
            .copied()
            .unwrap_or(1);
        let mut blob = make_png(u32::from(tag), 10);
        blob.resize(4_500_000, tag);
        Ok(HttpBytes::new(
            req.url.clone(),
            200,
            vec![("content-type".into(), "image/png".into())],
            blob,
        ))
    }));
    let svc_tot = make_service(
        net_tot,
        Arc::new(MemStore::new()),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );
    let cx_tot = CallCx::default();
    for i in 0..4 {
        svc_tot
            .fetch(
                &format!("https://img.example/m{i}.png"),
                inline_opts.clone(),
                &cx_tot,
            )
            .await
            .unwrap();
    }
    let err_tot = svc_tot
        .fetch("https://img.example/m4.png", inline_opts, &cx_tot)
        .await
        .unwrap_err();
    assert!(
        matches!(err_tot, FsmError::BudgetExceeded(_)),
        "{err_tot:?}"
    );
}

// ---------------------------------------------------------------------------
// Missing #10: Remaining unported oracle tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_oversized_content_length_rejected_up_front() {
    let cfg = Config::default();
    let cap = cfg.max_response_bytes;
    let net = Arc::new(MockNet::with_responses(vec![Err(
        FsmError::MaxBytesExceeded(cap),
    )]));
    let svc = make_service(
        net,
        Arc::new(MemStore::new()),
        cfg,
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );
    let err = svc
        .fetch(
            "https://example.com/giant",
            FetchOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, FsmError::MaxBytesExceeded(n) if n == cap),
        "{err:?}"
    );
}

#[tokio::test]
async fn test_streaming_aborts_when_body_exceeds_cap() {
    let mut resp = HttpBytes::new(
        "https://example.com/stream",
        200,
        vec![("content-type".into(), "text/plain".into())],
        "partial streamed text",
    );
    resp.truncated = true;
    let net = Arc::new(MockNet::with_responses(vec![Ok(resp)]));
    let svc = make_service(
        net,
        Arc::new(MemStore::new()),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );
    let cx = CallCx::default();
    let FetchOutcome::Page(p1) = svc
        .fetch("https://example.com/stream", FetchOpts::default(), &cx)
        .await
        .unwrap()
    else {
        panic!("expected Page");
    };
    assert!(
        p1.truncated,
        "streaming truncation must surface on PageResult"
    );
    assert_eq!(p1.content, "partial streamed text");

    // Truncated flag must survive on subsequent cache hit as well.
    let FetchOutcome::Page(p2) = svc
        .fetch("https://example.com/stream", FetchOpts::default(), &cx)
        .await
        .unwrap()
    else {
        panic!("expected Page");
    };
    assert_eq!(p2.method, FetchMethod::Cache);
    assert!(
        p2.truncated,
        "truncated flag must be preserved on cache hit"
    );
}

#[test]
fn test_extract_output_matches_string_path() {
    let html = r#"<!DOCTYPE html><html><head><title>Determinism</title></head><body>
      <article><h1>Determinism</h1>
        <p>First paragraph of body text ensuring trafilatura extracts cleanly every time.</p>
        <p>Second paragraph verifies identical output from decoded UTF-8 bytes and &str.</p>
      </article></body></html>"#;
    let decoded = fsm_extract::charset::decode_body(
        html.as_bytes(),
        "text/html; charset=utf-8",
        Some("https://example.com/det"),
    );
    let from_str = extract_html(html, "https://example.com/det");
    let from_bytes = extract_html(&decoded, "https://example.com/det");
    assert_eq!(from_str.title, from_bytes.title);
    assert_eq!(from_str.markdown, from_bytes.markdown);
    assert_eq!(from_str.author, from_bytes.author);
    assert_eq!(from_str.published_date, from_bytes.published_date);
    assert_eq!(from_str.sitename, from_bytes.sitename);
}

#[test]
fn test_the_whole_payload_is_small_enough_to_be_worth_calling() {
    let article_body = "A long sentence of the article body repeated many times. ".repeat(300);
    assert!(article_body.len() > 10_000);
    let html = format!(
        r#"<html><head><title>Big Article</title>
        <script type="application/ld+json">
        {{"@context":"https://schema.org","@type":"Article","headline":"Compactness Test",
          "articleBody":"{article_body}","datePublished":"2026-01-02"}}
        </script></head><body></body></html>"#
    );
    let payload = extract_structured_from_html(&html, "https://example.com/big", 200);
    let serialized = serde_json::to_string(&payload).unwrap();
    let tokens = estimate_tokens(&serialized);
    assert!(
        tokens < 700,
        "structured payload with clipped 10KB articleBody should be < 700 tokens, got {tokens}"
    );
}

#[tokio::test]
async fn test_sha256_stable_text_unaffected_and_inline_bytes_stay_out_of_json() {
    let png = make_png(32, 16);
    let net = Arc::new(MockNet::with_responses(vec![
        Ok(HttpBytes::new(
            "https://x.example/a.png",
            200,
            vec![("content-type".into(), "image/png".into())],
            png.clone(),
        )),
        Ok(HttpBytes::new(
            "https://x.example/b.png",
            200,
            vec![("content-type".into(), "image/png".into())],
            png.clone(),
        )),
        Ok(HttpBytes::new(
            "https://x.example/readme.txt",
            200,
            vec![("content-type".into(), "text/plain; charset=utf-8".into())],
            "plain text resource stays verbatim",
        )),
        Ok(HttpBytes::new(
            "https://x.example/c.png",
            200,
            vec![("content-type".into(), "image/png".into())],
            png,
        )),
    ]));
    let svc = make_service(
        net,
        Arc::new(MemStore::new()),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );
    let cx = CallCx::default();

    // 1. sha256 stability across identical bytes.
    let FetchOutcome::Page(a) = svc
        .fetch("https://x.example/a.png", FetchOpts::default(), &cx)
        .await
        .unwrap()
    else {
        panic!("expected Page");
    };
    let FetchOutcome::Page(b) = svc
        .fetch("https://x.example/b.png", FetchOpts::default(), &cx)
        .await
        .unwrap()
    else {
        panic!("expected Page");
    };
    assert_eq!(a.sha256, b.sha256);
    assert_eq!(a.sha256.as_deref().unwrap().len(), 64);

    // 2. text/plain unaffected by asset handling.
    let FetchOutcome::Page(txt) = svc
        .fetch("https://x.example/readme.txt", FetchOpts::default(), &cx)
        .await
        .unwrap()
    else {
        panic!("expected Page");
    };
    assert_eq!(txt.method, FetchMethod::Http);
    assert_eq!(txt.content, "plain text resource stays verbatim");
    assert!(txt.sha256.is_none());
    assert!(txt.bytes.is_none());

    // 3. InlineImageMeta JSON payload contains NO raw/base64 image bytes.
    let mut opts = FetchOpts::default();
    opts.inline_image = true;
    let FetchOutcome::InlineImage { meta, binary } = svc
        .fetch("https://x.example/c.png", opts, &cx)
        .await
        .unwrap()
    else {
        panic!("expected InlineImage");
    };
    let json_str = serde_json::to_string(&meta).unwrap();
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &binary.bytes);
    assert!(
        !json_str.contains(&b64),
        "inline image bytes leaked into JSON meta"
    );
    assert!(
        json_str.len() < 400,
        "meta JSON unexpectedly large: {json_str}"
    );
}

#[test]
fn test_many_script_tags_scan_in_linear_time() {
    let mut html = String::with_capacity(250_000);
    html.push_str("<html><head><title>Linear Scan</title></head><body><article><h1>Heading</h1>");
    for _ in 0..12_000 {
        html.push_str("<script>x</script>");
    }
    html.push_str(
        "<p>Readable article body survives many script tags.</p></article></body></html>",
    );

    let t0 = Instant::now();
    let flat = fsm_extract::html::strip_html_tags_flat(&html);
    let extracted = extract_html(&html, "https://example.test/scripts");
    let elapsed = t0.elapsed();

    assert!(
        elapsed < Duration::from_secs(2),
        "scanning 12k <script> tags took {elapsed:?} (O(N^2) regression)"
    );
    assert!(flat.contains("Readable article body survives many script tags."));
    assert!(
        extracted
            .markdown
            .contains("Readable article body survives many script tags.")
    );
}
