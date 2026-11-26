//! Port of `tests/test_fetch_safety.py`, `tests/test_httpfetch.py`, and Rust 1.0 fetch caps.

mod common;

use common::{MemStore, MockDocs, MockNet, MockSearch, make_service};
use fsm_core::config::Config;
use fsm_core::{
    CallCx, DocSlice, FetchApi, FetchMethod, FetchOpts, FetchOutcome, FsmError, HttpBytes,
    PageResult, RenderMode,
};
use fsm_extract::html::extract_html;
use std::sync::Arc;

const FIXTURE_HTML: &str = r#"<!DOCTYPE html>
<html>
<head><title>Fixture Title</title></head>
<body>
  <article>
    <h1>Heading One</h1>
    <p>This is a sufficiently long paragraph of body text so that trafilatura's
    content-extraction heuristics consider it the main article content and emit
    it into the markdown output without discarding it as boilerplate.</p>
    <p>A second paragraph keeps the heading: trafilatura 2.2 treats a page with
    one paragraph as a fragment and drops everything but the text.</p>
  </article>
</body>
</html>"#;

#[tokio::test]
async fn test_http_fetch_blocks_ssrf_before_connect() {
    for url in [
        "http://169.254.169.254/latest/meta-data/",
        "http://127.0.0.1:8080/",
        "http://10.0.0.1/",
        "http://localhost/admin",
        "http://metadata.google.internal/computeMetadata/v1/",
    ] {
        let net = Arc::new(MockNet::new());
        let store = Arc::new(MemStore::new());
        let svc = make_service(
            net.clone(),
            store,
            Config::default(),
            Arc::new(MockDocs::default()),
            Arc::new(MockSearch::default()),
        );
        let err = svc
            .fetch(url, FetchOpts::default(), &CallCx::default())
            .await
            .unwrap_err();
        assert!(
            matches!(err, FsmError::UnsafeUrl(_)),
            "expected UnsafeUrl for {url}, got {err:?}"
        );
        assert!(
            net.recorded().is_empty(),
            "must not connect to network for blocked URL {url}"
        );
    }
}

#[tokio::test]
async fn test_non_http_scheme_blocked() {
    for bad in ["file:///etc/passwd", "ftp://example.com/x", "gopher://x"] {
        let net = Arc::new(MockNet::new());
        let svc = make_service(
            net.clone(),
            Arc::new(MemStore::new()),
            Config::default(),
            Arc::new(MockDocs::default()),
            Arc::new(MockSearch::default()),
        );
        let err = svc
            .fetch(bad, FetchOpts::default(), &CallCx::default())
            .await
            .unwrap_err();
        assert!(matches!(err, FsmError::UnsafeUrl(_)), "{bad}: {err:?}");
        assert!(net.recorded().is_empty());
    }
}

#[tokio::test]
async fn test_offline_ssrf_runs_before_cache_hit() {
    let net = Arc::new(MockNet::new());
    let store = Arc::new(MemStore::new());
    // Seed a private URL directly into cache: SSRF layers 1-3 MUST block BEFORE reading cache!
    store.seed_page(
        PageResult::new(
            "http://127.0.0.1/secret",
            "Secret",
            "leaked token",
            FetchMethod::Http,
        ),
        5,
    );
    let svc = make_service(
        net.clone(),
        store.clone(),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );
    let err = svc
        .fetch(
            "http://127.0.0.1/secret",
            FetchOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, FsmError::UnsafeUrl(_)));
    assert!(
        store.page_gets.lock().unwrap().is_empty(),
        "cache must never be queried for a URL blocked by offline SSRF"
    );
}

#[tokio::test]
async fn test_http_fetch_blocks_redirect_to_private_ip() {
    let net = Arc::new(MockNet::with_responses(vec![
        Ok(HttpBytes::new(
            "https://example.com/start",
            302,
            vec![("location".into(), "http://127.0.0.1/secret".into())],
            "",
        )),
        Ok(HttpBytes::new(
            "http://127.0.0.1/secret",
            200,
            vec![("content-type".into(), "text/html".into())],
            "<html>leak</html>",
        )),
    ]));
    let svc = make_service(
        net.clone(),
        Arc::new(MemStore::new()),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );
    let err = svc
        .fetch(
            "https://example.com/start",
            FetchOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, FsmError::UnsafeUrl(_)));
    let reqs = net.recorded();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].0.url, "https://example.com/start");
}

#[tokio::test]
async fn test_http_fetch_follows_safe_redirect() {
    let net = Arc::new(MockNet::with_responses(vec![
        Ok(HttpBytes::new(
            "https://example.com/start",
            301,
            vec![("location".into(), "https://example.org/final".into())],
            "",
        )),
        Ok(HttpBytes::new(
            "https://example.org/final",
            200,
            vec![("content-type".into(), "text/html; charset=utf-8".into())],
            FIXTURE_HTML,
        )),
    ]));
    let svc = make_service(
        net.clone(),
        Arc::new(MemStore::new()),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );
    let FetchOutcome::Page(page) = svc
        .fetch(
            "https://example.com/start",
            FetchOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap()
    else {
        panic!("expected Page outcome");
    };
    assert_eq!(page.url, "https://example.org/final");
    assert!(page.content.contains("Heading One"));
    let urls: Vec<String> = net.recorded().into_iter().map(|(r, _)| r.url).collect();
    assert_eq!(
        urls,
        vec!["https://example.com/start", "https://example.org/final"]
    );
}

#[tokio::test]
async fn test_http_fetch_caps_redirect_chain() {
    let hops = (0..10)
        .map(|i| {
            Ok(HttpBytes::new(
                format!("https://example.com/{i}"),
                302,
                vec![("location".into(), format!("https://example.com/{}", i + 1))],
                "",
            ))
        })
        .collect();
    let net = Arc::new(MockNet::with_responses(hops));
    let svc = make_service(
        net.clone(),
        Arc::new(MemStore::new()),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );
    let err = svc
        .fetch(
            "https://example.com/0",
            FetchOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("too many redirects"), "{err}");
    // Initial request + 5 redirects = 6 requests max.
    assert_eq!(net.recorded().len(), 6);
}

#[tokio::test]
async fn test_fetch_page_json_returned_verbatim() {
    let json_body = r#"{"answer": 42, "nested": {"x": [1, 2, 3]}}"#;
    let net = Arc::new(MockNet::with_responses(vec![Ok(HttpBytes::new(
        "https://example.com/data.json",
        200,
        vec![("content-type".into(), "application/json".into())],
        json_body,
    ))]));
    let svc = make_service(
        net,
        Arc::new(MemStore::new()),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );
    let mut opts = FetchOpts::default();
    opts.render = RenderMode::Http;
    opts.force_refresh = true;
    let FetchOutcome::Page(page) = svc
        .fetch("https://example.com/data.json", opts, &CallCx::default())
        .await
        .unwrap()
    else {
        panic!("expected Page");
    };
    assert_eq!(page.content, json_body);
}

#[tokio::test]
async fn test_fetch_page_html_still_extracted() {
    let net = Arc::new(MockNet::with_responses(vec![Ok(HttpBytes::new(
        "https://example.com/page",
        200,
        vec![("content-type".into(), "text/html".into())],
        FIXTURE_HTML,
    ))]));
    let svc = make_service(
        net,
        Arc::new(MemStore::new()),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );
    let FetchOutcome::Page(page) = svc
        .fetch(
            "https://example.com/page",
            FetchOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap()
    else {
        panic!("expected Page");
    };
    assert!(page.content.contains("Heading One"), "{}", page.content);
    assert!(page.content.contains("body text"), "{}", page.content);
    assert!(!page.title.is_empty());
}

#[test]
fn test_extract_output_stable_fixture() {
    let res = extract_html(FIXTURE_HTML, "https://example.com/a");
    assert!(res.markdown.contains("Heading One"), "{}", res.markdown);
    assert!(res.markdown.contains("body text"), "{}", res.markdown);
    assert!(!res.title.is_empty());
}

#[tokio::test]
async fn test_fetch_50k_cap_and_full_text_cached() {
    let big_text = "alpha beta gamma delta epsilon zeta. ".repeat(2000); // ~74,000 chars
    assert!(big_text.chars().count() > 50_000);
    let net = Arc::new(MockNet::with_responses(vec![Ok(HttpBytes::new(
        "https://example.com/big.txt",
        200,
        vec![("content-type".into(), "text/plain; charset=utf-8".into())],
        big_text.clone(),
    ))]));
    let store = Arc::new(MemStore::new());
    let svc = make_service(
        net,
        store.clone(),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );

    let FetchOutcome::Page(page) = svc
        .fetch(
            "https://example.com/big.txt",
            FetchOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap()
    else {
        panic!("expected Page");
    };
    assert!(page.truncated);
    assert!(page.content.chars().count() <= 50_100);
    assert_eq!(page.total_chars, big_text.chars().count());

    // Full untruncated content must remain in cache for cache.getPage / doc.read.
    let cached = store.stored_page("https://example.com/big.txt").unwrap();
    assert_eq!(cached.content.chars().count(), big_text.chars().count());
    assert!(!cached.truncated);
}

#[tokio::test]
async fn test_fetch_pdf_and_office_delegate_to_doc_api() {
    let mut slice = DocSlice::new(
        "https://arxiv.org/pdf/1706.03762.pdf",
        "pdf",
        "## Page 1\n\nAttention Is All You Need.",
    );
    slice.title = Some("Attention Is All You Need".into());
    slice.returned_chars = 39;
    slice.total_chars = 120_000;
    slice.truncated = true;

    let net = Arc::new(MockNet::new());
    let docs = Arc::new(MockDocs::new(slice));
    let svc = make_service(
        net.clone(),
        Arc::new(MemStore::new()),
        Config::default(),
        docs.clone(),
        Arc::new(MockSearch::default()),
    );

    let FetchOutcome::Page(page) = svc
        .fetch(
            "https://arxiv.org/pdf/1706.03762.pdf",
            FetchOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap()
    else {
        panic!("expected Page");
    };
    assert_eq!(page.method, FetchMethod::Document);
    assert_eq!(page.title, "Attention Is All You Need");
    assert!(page.truncated);
    assert_eq!(page.total_chars, 120_000);
    assert!(page.content.contains("doc.read"), "{}", page.content);
    assert!(page.content.contains("doc.outline"), "{}", page.content);
    assert!(
        net.recorded().is_empty(),
        "PDF URL routed directly to DocApi without duplicate HTTP fetch"
    );
    let doc_calls = docs.calls.lock().unwrap();
    assert_eq!(doc_calls.len(), 1);
    assert_eq!(doc_calls[0].1, 0);
    assert_eq!(doc_calls[0].2, Some(50_000));
}

#[tokio::test]
async fn test_fetch_extensionless_pdf_ctype_delegates_to_doc_api() {
    let mut slice = DocSlice::new("https://example.com/file?id=7", "pdf", "Parsed PDF body");
    slice.title = Some("Report".into());
    slice.total_chars = 15;
    let docs = Arc::new(MockDocs::new(slice));
    let net = Arc::new(MockNet::with_responses(vec![Ok(HttpBytes::new(
        "https://example.com/file?id=7",
        200,
        vec![("content-type".into(), "application/pdf".into())],
        b"%PDF-1.7 fake".as_slice(),
    ))]));
    let svc = make_service(
        net,
        Arc::new(MemStore::new()),
        Config::default(),
        docs.clone(),
        Arc::new(MockSearch::default()),
    );
    let FetchOutcome::Page(page) = svc
        .fetch(
            "https://example.com/file?id=7",
            FetchOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap()
    else {
        panic!("expected Page");
    };
    assert_eq!(page.method, FetchMethod::Document);
    assert!(page.content.contains("Parsed PDF body"));
    assert_eq!(docs.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn test_fetch_render_browser_returns_browser_unavailable() {
    let svc = make_service(
        Arc::new(MockNet::new()),
        Arc::new(MemStore::new()),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );
    let mut opts = FetchOpts::default();
    opts.render = RenderMode::Browser;
    let err = svc
        .fetch("https://example.com/app", opts, &CallCx::default())
        .await
        .unwrap_err();
    assert!(matches!(err, FsmError::BrowserUnavailable(_)), "{err:?}");
}

#[tokio::test]
async fn test_fetch_batch_caps_urls_and_runs_concurrently() {
    let net = Arc::new(MockNet::with_handler(|req, _, _| {
        Ok(HttpBytes::new(
            req.url.clone(),
            200,
            vec![("content-type".into(), "text/plain".into())],
            format!("body of {}", req.url),
        ))
    }));
    let svc = make_service(
        net.clone(),
        Arc::new(MemStore::new()),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );

    // > 20 URLs rejected.
    let too_many: Vec<String> = (0..21)
        .map(|i| format!("https://example.com/{i}"))
        .collect();
    let err = svc
        .fetch_batch(too_many, FetchOpts::default(), 8, &CallCx::default())
        .await
        .unwrap_err();
    assert!(matches!(err, FsmError::InvalidInput(_)));
    assert!(net.recorded().is_empty());

    // 20 URLs accepted and order preserved.
    let twenty: Vec<String> = (0..20)
        .map(|i| format!("https://example.com/{i}"))
        .collect();
    let pages = svc
        .fetch_batch(twenty.clone(), FetchOpts::default(), 16, &CallCx::default())
        .await
        .unwrap();
    assert_eq!(pages.len(), 20);
    assert_eq!(pages[0].url, "https://example.com/0");
    assert_eq!(pages[19].url, "https://example.com/19");
}

#[tokio::test]
async fn test_fetch_cache_max_age_zero_forces_refresh_and_writes_back() {
    let store = Arc::new(MemStore::new());
    store.seed_page(
        PageResult::new(
            "https://example.com/news",
            "Old",
            "stale body",
            FetchMethod::Http,
        ),
        60,
    );
    let net = Arc::new(MockNet::with_responses(vec![Ok(HttpBytes::new(
        "https://example.com/news",
        200,
        vec![("content-type".into(), "text/plain".into())],
        "fresh body",
    ))]));
    let svc = make_service(
        net.clone(),
        store.clone(),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );

    let mut opts = FetchOpts::default();
    opts.max_age_hours = Some(0.0);
    let FetchOutcome::Page(page) = svc
        .fetch("https://example.com/news", opts, &CallCx::default())
        .await
        .unwrap()
    else {
        panic!("expected Page");
    };
    assert_eq!(page.content, "fresh body");
    assert_eq!(page.method, FetchMethod::Http);
    assert_eq!(
        store
            .stored_page("https://example.com/news")
            .unwrap()
            .content,
        "fresh body"
    );
}

#[tokio::test]
async fn test_html_to_md_converts_and_strips_scripts() {
    let svc = make_service(
        Arc::new(MockNet::new()),
        Arc::new(MemStore::new()),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );
    let md = svc
        .html_to_md(
            "<h1>Title</h1><script>alert(1)</script><p>Hello <a href=\"https://example.com\">world</a></p><ul><li>One</li><li>Two</li></ul>",
        )
        .await
        .unwrap();
    assert!(md.contains("# Title"), "{md}");
    assert!(md.contains("[world](https://example.com)"), "{md}");
    assert!(md.contains("- One"), "{md}");
    assert!(!md.contains("alert(1)"), "{md}");
}
