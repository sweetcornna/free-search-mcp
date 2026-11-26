//! Port of `tests/test_gnews.py` + end-to-end Google News fetch resolution tests.

mod common;

use common::{MemStore, MockDocs, MockNet, MockSearch, make_service};
use fsm_core::config::Config;
use fsm_core::{CallCx, FetchApi, FetchOpts, FetchOutcome, FsmError, HttpBytes};
use fsm_extract::gnews::{GnewsResolver, build_freq, is_google_news_url, parse_batchexecute};
use percent_encoding::percent_decode_str;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::Notify;

#[test]
fn test_is_google_news_url_matches_article_blobs() {
    assert!(is_google_news_url(
        "https://news.google.com/rss/articles/CBMiXXXX"
    ));
    assert!(is_google_news_url(
        "https://news.google.com/articles/CBMiYYYY?oc=5"
    ));
    assert!(is_google_news_url("https://news.google.com/read/CBMiZZZZ"));
}

#[test]
fn test_is_google_news_url_rejects_non_news_and_non_article() {
    assert!(!is_google_news_url("https://www.reuters.com/world/x"));
    assert!(!is_google_news_url("https://news.google.com/topics/abc"));
    assert!(!is_google_news_url("https://example.com/rss/articles/CBM"));
    assert!(!is_google_news_url("not-a-url"));
}

#[test]
fn test_build_freq_encodes_id_ts_sig() {
    let freq = build_freq("ARTICLE_ID", "1700000000", "SIGNATURE").unwrap();
    assert!(freq.starts_with("f.req="));
    let decoded = percent_decode_str(&freq["f.req=".len()..])
        .decode_utf8()
        .unwrap();
    assert!(decoded.contains("Fbv4je"));
    assert!(decoded.contains("garturlreq"));
    assert!(decoded.contains("ARTICLE_ID"));
    assert!(decoded.contains("SIGNATURE"));
    assert!(decoded.contains("1700000000"));
}

#[test]
fn test_parse_batchexecute_extracts_publisher_url() {
    let inner = json!(["garturlres", "https://www.reuters.com/world/story-2026", 1]).to_string();
    let body = format!(
        ")]}}'\n\n{}",
        json!([
            ["wrb.fr", "Fbv4je", inner, null, null, null, "generic"],
            ["di", 13]
        ])
    );
    assert_eq!(
        parse_batchexecute(&body).as_deref(),
        Some("https://www.reuters.com/world/story-2026")
    );
}

#[test]
fn test_parse_batchexecute_returns_none_on_garbage() {
    assert_eq!(parse_batchexecute(""), None);
    assert_eq!(parse_batchexecute(")]}'\n\nnot json at all"), None);
    assert_eq!(
        parse_batchexecute(&format!(")]}}'\n{}", json!([["di", 13]]))),
        None
    );
}

#[test]
fn test_parse_batchexecute_handles_multiline_and_length_prefix() {
    let body = ")]}'\n\n359\n[[\"wrb.fr\",\"Fbv4je\",\n  \"[\\\"garturlres\\\",\\\"https://www.bbc.com/news/articles/abc123\\\",1]\",\n  null,null,null,\"generic\"]]\n";
    assert_eq!(
        parse_batchexecute(body).as_deref(),
        Some("https://www.bbc.com/news/articles/abc123")
    );
}

async fn run_singleflight_cancel_case(cancel_owner: bool) {
    let resolver = Arc::new(GnewsResolver::new());
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let url = "https://news.google.com/read/CBM-cancellation";
    let publisher = "https://example.org/story";

    let r_owner = Arc::clone(&resolver);
    let entered_c = Arc::clone(&entered);
    let release_c = Arc::clone(&release);
    let calls_c = Arc::clone(&calls);
    let owner = tokio::spawn(async move {
        r_owner
            .resolve_with(url, move |_u| async move {
                calls_c.fetch_add(1, Ordering::SeqCst);
                entered_c.notify_one();
                release_c.notified().await;
                Some(publisher.to_owned())
            })
            .await
    });

    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .expect("owner entered worker");

    let mut followers = Vec::new();
    for _ in 0..2 {
        let r_f = Arc::clone(&resolver);
        let calls_f = Arc::clone(&calls);
        followers.push(tokio::spawn(async move {
            r_f.resolve_with(url, move |_u| async move {
                calls_f.fetch_add(1, Ordering::SeqCst);
                Some("https://should-not-run.example".to_owned())
            })
            .await
        }));
    }

    // Give followers a moment to attach to the single-flight watch channel.
    tokio::time::sleep(Duration::from_millis(25)).await;

    if cancel_owner {
        owner.abort();
        let _ = owner.await;
        release.notify_waiters();
        for f in followers {
            let res = tokio::time::timeout(Duration::from_secs(1), f)
                .await
                .expect("follower must not hang")
                .unwrap();
            assert_eq!(res, None);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(resolver.inflight_len(), 0);
    } else {
        let f0 = followers.remove(0);
        let f1 = followers.remove(0);
        f0.abort();
        let _ = f0.await;
        release.notify_waiters();
        let owner_res = tokio::time::timeout(Duration::from_secs(1), owner)
            .await
            .unwrap()
            .unwrap();
        let f1_res = tokio::time::timeout(Duration::from_secs(1), f1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(owner_res.as_deref(), Some(publisher));
        assert_eq!(f1_res.as_deref(), Some(publisher));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(resolver.inflight_len(), 0);

        // Memoized hit must not invoke the worker again.
        let memo_hit = resolver
            .resolve_with(url, |_| async {
                panic!("memoized URL must not call worker");
            })
            .await;
        assert_eq!(memo_hit.as_deref(), Some(publisher));
    }
}

#[tokio::test]
async fn test_singleflight_cancellation_follower_does_not_cancel_peers() {
    run_singleflight_cancel_case(false).await;
}

#[tokio::test]
async fn test_singleflight_cancellation_owner_does_not_hang_followers() {
    run_singleflight_cancel_case(true).await;
}

#[tokio::test]
async fn test_resolve_returns_none_for_non_gnews_url() {
    let resolver = GnewsResolver::new();
    let res = resolver
        .resolve_with("https://www.bbc.com/news/x", |_| async {
            panic!("non-gnews URL must short-circuit without network");
        })
        .await;
    assert_eq!(res, None);
}

#[tokio::test]
async fn test_fetch_resolves_gnews_and_caches_under_publisher_url() {
    let gn_url = "https://news.google.com/rss/articles/CBMiTestBlob123";
    let pub_url = "https://publisher.example/news/story-1";
    let shell_html = r#"<html><body><c-wiz data-n-a-id="CBMiTestBlob123" data-n-a-ts="1700000000" data-n-a-sg="SIG_XYZ"></c-wiz></body></html>"#;
    let inner = json!(["garturlres", pub_url, 1]).to_string();
    let rpc_body = format!(
        ")]}}'\n\n{}",
        json!([["wrb.fr", "Fbv4je", inner, null, null, null, "generic"]])
    );
    let article_html = "<html><head><title>Real Story</title></head><body><article><h1>Real Story</h1><p>Full news story content from the real publisher page for testing resolution.</p><p>Second paragraph ensures trafilatura extracts cleanly.</p></article></body></html>";

    let net = Arc::new(MockNet::with_responses(vec![
        Ok(HttpBytes::new(
            gn_url,
            200,
            vec![("content-type".into(), "text/html".into())],
            shell_html,
        )),
        Ok(HttpBytes::new(
            "https://news.google.com/_/DotsSplashUi/data/batchexecute",
            200,
            vec![("content-type".into(), "application/json".into())],
            rpc_body,
        )),
        Ok(HttpBytes::new(
            pub_url,
            200,
            vec![("content-type".into(), "text/html".into())],
            article_html,
        )),
    ]));
    let store = Arc::new(MemStore::new());
    let svc = make_service(
        net.clone(),
        store.clone(),
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
    assert_eq!(page.url, pub_url);
    assert!(page.content.contains("Full news story content"));
    assert!(store.stored_page(pub_url).is_some());

    // Second fetch of the same GN URL hits memo + cache without any network calls!
    let req_count_before = net.recorded().len();
    let FetchOutcome::Page(cached) = svc
        .fetch(gn_url, FetchOpts::default(), &CallCx::default())
        .await
        .unwrap()
    else {
        panic!("expected Page");
    };
    assert_eq!(cached.url, pub_url);
    assert_eq!(net.recorded().len(), req_count_before);
}

#[tokio::test]
async fn test_gnews_resolution_to_private_ip_is_blocked() {
    let gn_url = "https://news.google.com/rss/articles/CBMiEvil";
    let shell_html =
        r#"<c-wiz data-n-a-id="CBMiEvil" data-n-a-ts="1700000000" data-n-a-sg="SIG"></c-wiz>"#;
    let inner = json!(["garturlres", "http://127.0.0.1/secret", 1]).to_string();
    let rpc_body = format!(
        ")]}}'\n\n{}",
        json!([["wrb.fr", "Fbv4je", inner, null, null, null, "generic"]])
    );

    let net = Arc::new(MockNet::with_responses(vec![
        Ok(HttpBytes::new(gn_url, 200, vec![], shell_html)),
        Ok(HttpBytes::new(
            "https://news.google.com/_/DotsSplashUi/data/batchexecute",
            200,
            vec![],
            rpc_body,
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
        .fetch(gn_url, FetchOpts::default(), &CallCx::default())
        .await
        .unwrap_err();
    assert!(matches!(err, FsmError::UnsafeUrl(_)));
    // Only shell + batchexecute ran; private IP was never connected to.
    assert_eq!(net.recorded().len(), 2);
}
