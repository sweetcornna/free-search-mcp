//! Port of `tests/test_structured.py` + structured safety tests from `tests/test_fetch_safety.py`.

mod common;

use common::{MemStore, MockDocs, MockNet, MockSearch, make_service};
use fsm_core::config::Config;
use fsm_core::{CallCx, FetchApi, FsmError, HttpBytes, TransportProfile};
use fsm_extract::structured::{LONG_TEXT_CAP, extract_structured_from_html};
use std::sync::Arc;

const JSONLD_HTML: &str = r#"<!DOCTYPE html>
<html>
<head>
  <title>Test article</title>
  <meta property="og:title" content="OG Title">
  <meta property="og:type" content="article">
  <meta property="og:url" content="https://example.com/article">
  <meta name="twitter:card" content="summary_large_image">
  <meta name="twitter:title" content="Twitter Title">
  <script type="application/ld+json">
  {
    "@context": "https://schema.org",
    "@type": "Article",
    "headline": "X",
    "author": {"@type": "Person", "name": "Jane Doe"},
    "datePublished": "2024-01-15"
  }
  </script>
</head>
<body>
  <article itemscope itemtype="https://schema.org/Article">
    <h1 itemprop="headline">Microdata headline</h1>
    <span itemprop="author">Bob</span>
  </article>
</body>
</html>"#;

#[test]
fn test_extract_structured_from_html_parses_jsonld() {
    let payload = extract_structured_from_html(JSONLD_HTML, "https://example.com/article", 200);
    assert_eq!(payload.url, "https://example.com/article");
    assert!(!payload.json_ld.is_empty());
    let article = &payload.json_ld[0];
    assert_eq!(
        article.get("@type").and_then(|v| v.as_str()),
        Some("Article")
    );
    assert_eq!(article.get("headline").and_then(|v| v.as_str()), Some("X"));
}

#[test]
fn test_extract_structured_from_html_parses_opengraph() {
    let payload = extract_structured_from_html(JSONLD_HTML, "https://example.com/article", 200);
    assert_eq!(
        payload.opengraph.get("og:title").map(String::as_str),
        Some("OG Title")
    );
    assert_eq!(
        payload.opengraph.get("og:type").map(String::as_str),
        Some("article")
    );
}

#[test]
fn test_extract_structured_from_html_parses_microdata() {
    let payload = extract_structured_from_html(JSONLD_HTML, "https://example.com/article", 200);
    assert!(!payload.microdata.is_empty());
    let blob = serde_json::to_string(&payload.microdata).unwrap();
    assert!(blob.contains("Microdata headline"), "{blob}");
    assert!(blob.contains("Bob"), "{blob}");
}

#[test]
fn test_extract_structured_from_html_empty_page() {
    let payload = extract_structured_from_html(
        "<html><body><p>nothing</p></body></html>",
        "https://example.com/blank",
        200,
    );
    assert_eq!(payload.url, "https://example.com/blank");
    assert!(payload.json_ld.is_empty());
    assert!(payload.microdata.is_empty());
    assert!(payload.opengraph.is_empty());
}

#[test]
fn test_extract_structured_meta_fallback_when_no_structured_data() {
    let html = r#"<!DOCTYPE html>
<html>
<head>
  <title>Plain page</title>
  <meta name="description" content="A plain page about cats.">
  <meta name="keywords" content="cats, felines">
  <meta name="author" content="A. Person">
  <meta name="twitter:title" content="Cats!">
</head>
<body><p>hi</p></body>
</html>"#;
    let payload = extract_structured_from_html(html, "https://example.com/plain", 200);
    assert!(payload.json_ld.is_empty());
    assert!(payload.microdata.is_empty());
    assert!(payload.opengraph.is_empty());
    assert_eq!(
        payload.meta_fallback.get("description").map(String::as_str),
        Some("A plain page about cats.")
    );
    assert_eq!(
        payload.meta_fallback.get("keywords").map(String::as_str),
        Some("cats, felines")
    );
    assert_eq!(
        payload.meta_fallback.get("author").map(String::as_str),
        Some("A. Person")
    );
    assert_eq!(
        payload
            .meta_fallback
            .get("twitter:title")
            .map(String::as_str),
        Some("Cats!")
    );
    let hint = payload.hint.as_deref().unwrap();
    assert!(hint.contains("metaFallback"), "{hint}");
    assert!(!hint.contains("bot-block shell"), "{hint}");
}

#[test]
fn test_extract_structured_completely_empty_html_strong_hint() {
    let payload = extract_structured_from_html("<html></html>", "https://example.com/empty", 200);
    assert!(payload.json_ld.is_empty());
    assert!(payload.microdata.is_empty());
    assert!(payload.opengraph.is_empty());
    assert!(payload.meta_fallback.is_empty());
    let hint = payload.hint.as_deref().unwrap();
    assert!(hint.contains("bot-block shell"), "{hint}");
}

#[test]
fn test_extract_structured_jsonld_plus_meta_no_hint() {
    let html = r#"<!DOCTYPE html>
<html>
<head>
  <meta name="description" content="A described page.">
  <script type="application/ld+json">
  {"@context": "https://schema.org", "@type": "Article", "headline": "Y"}
  </script>
</head>
<body></body>
</html>"#;
    let payload = extract_structured_from_html(html, "https://example.com/rich", 200);
    assert!(!payload.json_ld.is_empty());
    assert!(payload.hint.is_none());
    assert!(payload.meta_fallback.is_empty());
}

#[test]
fn test_long_prose_is_clipped_and_every_small_field_survives() {
    let article_body = "A sentence of the article. ".repeat(400);
    let html = format!(
        r#"<html><head><title>Post</title>
        <script type="application/ld+json">
        {{"@context": "https://schema.org", "@type": "BlogPosting",
          "headline": "What changed in 0.12",
          "articleBody": "{article_body}",
          "description": "A short summary.",
          "datePublished": "2026-09-01", "dateModified": "2026-09-14",
          "author": {{"@type": "Person", "name": "Ada"}}}}
        </script></head><body></body></html>"#
    );

    let out = extract_structured_from_html(&html, "https://blog.example/post", 200);
    let post = &out.json_ld[0];
    assert_eq!(post["datePublished"], "2026-09-01");
    assert_eq!(post["dateModified"], "2026-09-14");
    assert_eq!(post["author"]["name"], "Ada");
    assert_eq!(post["headline"], "What changed in 0.12");
    assert_eq!(post["description"], "A short summary.");

    let body = post["articleBody"].as_str().unwrap();
    assert!(body.starts_with("A sentence of the article."));
    assert!(body.chars().count() < LONG_TEXT_CAP + 80);
    assert!(body.contains(&format!(
        "[clipped: {} characters in the page]",
        article_body.chars().count()
    )));
    let hint = out.hint.as_deref().unwrap();
    assert!(hint.contains("1 long text field(s) clipped"), "{hint}");
    assert!(hint.contains("use `web.fetch` for the full text"), "{hint}");
}

#[tokio::test]
async fn test_extract_structured_403_returns_hint_no_raise() {
    let html =
        b"<html><head><meta name='description' content='blocked shell'></head><body></body></html>";
    let net = Arc::new(MockNet::with_responses(vec![Ok(HttpBytes::new(
        "https://example.com/blocked",
        403,
        vec![("content-type".into(), "text/html".into())],
        html.as_slice(),
    ))]));
    let svc = make_service(
        net.clone(),
        Arc::new(MemStore::new()),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );
    let payload = svc
        .structured("https://example.com/blocked", &CallCx::default())
        .await
        .unwrap();
    let hint = payload.hint.as_deref().unwrap();
    assert!(hint.contains("403"), "{hint}");
    assert_eq!(
        payload.meta_fallback.get("description").map(String::as_str),
        Some("blocked shell")
    );
    assert_eq!(net.recorded()[0].1, TransportProfile::Plain);
}

#[tokio::test]
async fn test_extract_structured_blocks_ssrf_before_connect() {
    for url in [
        "http://169.254.169.254/",
        "http://127.0.0.1/",
        "http://10.0.0.1/",
    ] {
        let net = Arc::new(MockNet::new());
        let svc = make_service(
            net.clone(),
            Arc::new(MemStore::new()),
            Config::default(),
            Arc::new(MockDocs::default()),
            Arc::new(MockSearch::default()),
        );
        let err = svc.structured(url, &CallCx::default()).await.unwrap_err();
        assert!(matches!(err, FsmError::UnsafeUrl(_)));
        assert!(net.recorded().is_empty());
    }
}
