//! Port of storage-layer caching & `max_age_s` regression tests from
//! `tests/test_cache_maxage.py` and `tests/test_resources.py`.

use fsm_core::{FetchMethod, Hit, PageResult, SearchResponse, StoreApi};
use fsm_store::{DB_FILENAME, SqliteStore};
use rusqlite::Connection;
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::TempDir;

fn open_temp_store(dir: &TempDir) -> SqliteStore {
    SqliteStore::open_path(dir.path().join(DB_FILENAME)).expect("open store")
}

fn sample_response(query: &str) -> SearchResponse {
    let mut hit = Hit::new(
        "Python Async Tutorial Guide",
        "https://duckduckgo.example/result",
    );
    hit.snippet = "This long article explains python async programming and covers tutorial material across coroutines and event loops.".into();
    hit.source = "duckduckgo".into();
    hit.sources = vec!["duckduckgo".into()];
    hit.score = 1.0 / 61.0;

    let mut resp = SearchResponse::new(query, "web");
    resp.results = vec![hit];
    resp.lead = Some("Python async programming uses coroutines and event loops.".into());
    resp.engines = vec!["duckduckgo".into()];
    resp.notes = vec!["rescued by reserve".into()];
    resp.cache_key = Some("v4:test-key".into());
    resp
}

#[tokio::test]
async fn test_max_age_hours_writes_and_serves_search_cache() {
    let dir = TempDir::new().unwrap();
    let store = open_temp_store(&dir);
    let key = "v4:async-guide";
    let resp = sample_response("python async tutorial guide");

    store.search_cache_put(key, &resp, 24 * 3600).await.unwrap();

    let (got, age_s) = store
        .search_cache_get(key, Some(24 * 3600))
        .await
        .unwrap()
        .expect("fresh entry must hit");
    assert_eq!(got, resp);
    assert!(age_s < 5);
    store.close().await.unwrap();
}

#[tokio::test]
async fn test_max_age_zero_forces_miss_and_allows_rewrite() {
    let dir = TempDir::new().unwrap();
    let store = open_temp_store(&dir);
    let key = "v4:force-refresh";
    let resp = sample_response("rust sqlite");

    store.search_cache_put(key, &resp, 3600).await.unwrap();
    // max_age_s = Some(0) MUST miss even within the exact same second.
    assert!(
        store
            .search_cache_get(key, Some(0))
            .await
            .unwrap()
            .is_none()
    );
    // Normal read still hits what was written.
    assert!(store.search_cache_get(key, None).await.unwrap().is_some());

    // Same guarantee for pages.
    let p = PageResult::new("https://example.com/p", "P", "body", FetchMethod::Http);
    store.put_page(&p, 3600).await.unwrap();
    assert!(
        store
            .get_page("https://example.com/p", Some(0))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .get_page("https://example.com/p", None)
            .await
            .unwrap()
            .is_some()
    );

    // Overwriting with fresh response persists cleanly.
    let mut updated = resp.clone();
    updated.lead = Some("Fresh lead after forced refresh.".into());
    store.search_cache_put(key, &updated, 3600).await.unwrap();
    let (got, _) = store.search_cache_get(key, None).await.unwrap().unwrap();
    assert_eq!(got.lead, updated.lead);
    store.close().await.unwrap();
}

#[tokio::test]
async fn test_stale_beyond_max_age_is_miss_and_fresh_within_hits() {
    let dir = TempDir::new().unwrap();
    let store = open_temp_store(&dir);
    let key = "v4:rewind-test";
    store
        .search_cache_put(key, &sample_response("rust"), 6 * 3600)
        .await
        .unwrap();

    // Rewind `created_at` to 2 hours ago while `expires_at` remains in the future.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let two_hours_ago = now - 2 * 3600;
    {
        let conn = Connection::open(store.path()).unwrap();
        conn.execute(
            "UPDATE search_cache SET created_at = ?1 WHERE cache_key = ?2",
            rusqlite::params![two_hours_ago, key],
        )
        .unwrap();
    }

    // max_age = 1h -> stale miss.
    assert!(
        store
            .search_cache_get(key, Some(3600))
            .await
            .unwrap()
            .is_none()
    );
    // max_age = 3h -> fresh hit, reporting ~7200s age.
    let (_, age_s) = store
        .search_cache_get(key, Some(3 * 3600))
        .await
        .unwrap()
        .expect("within 3h must hit");
    assert!((7195..=7210).contains(&age_s), "age_s = {age_s}");
    // No override -> hits until `expires_at`.
    assert!(store.search_cache_get(key, None).await.unwrap().is_some());
    store.close().await.unwrap();
}

#[tokio::test]
async fn test_get_page_max_age_seconds_override() {
    let dir = TempDir::new().unwrap();
    let store = open_temp_store(&dir);
    let url = "https://example.com/aged";
    store
        .put_page(
            &PageResult::new(url, "Aged", "cached content", FetchMethod::Http),
            7 * 86_400,
        )
        .await
        .unwrap();

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let one_hour_ago = now - 3600;
    {
        let conn = Connection::open(store.path()).unwrap();
        conn.execute(
            "UPDATE pages SET created_at = ?1 WHERE url = ?2",
            rusqlite::params![one_hour_ago, url],
        )
        .unwrap();
    }

    assert!(store.get_page(url, None).await.unwrap().is_some());
    assert!(store.get_page(url, Some(10 * 60)).await.unwrap().is_none());
    let hit = store
        .get_page(url, Some(2 * 3600))
        .await
        .unwrap()
        .expect("2h override should hit 1h-old row");
    assert!(hit.cache_age_s.unwrap() >= 3595);
    store.close().await.unwrap();
}

#[tokio::test]
async fn test_search_cache_preserves_lead_and_provenance_notes() {
    let dir = TempDir::new().unwrap();
    let store = open_temp_store(&dir);
    let resp = sample_response("lead block test");
    store.search_cache_put("k-lead", &resp, 3600).await.unwrap();

    let (cached, _) = store
        .search_cache_get("k-lead", None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cached.lead, resp.lead);
    assert_eq!(cached.notes, vec!["rescued by reserve"]);
    assert_eq!(cached.engines, vec!["duckduckgo"]);
    assert_eq!(cached.results.len(), 1);
    store.close().await.unwrap();
}

#[tokio::test]
async fn test_search_cache_ttl_zero_never_caches() {
    let dir = TempDir::new().unwrap();
    let store = open_temp_store(&dir);
    let resp = sample_response("current time in tokyo");

    // Seed first, then overwrite with ttl_s = 0 (worldclock behavior).
    store.search_cache_put("clock", &resp, 3600).await.unwrap();
    assert!(
        store
            .search_cache_get("clock", None)
            .await
            .unwrap()
            .is_some()
    );

    store.search_cache_put("clock", &resp, 0).await.unwrap();
    assert!(
        store
            .search_cache_get("clock", None)
            .await
            .unwrap()
            .is_none()
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn test_page_result_lossless_round_trip_and_null_meta_compat() {
    let dir = TempDir::new().unwrap();
    let store = open_temp_store(&dir);

    let mut p = PageResult::new(
        "https://example.com/doc.pdf",
        "Quarterly Report",
        "Extracted markdown text...",
        FetchMethod::Document,
    );
    p.total_chars = 120_000;
    p.truncated = true;
    p.published_date = Some("2026-10-01".into());
    p.author = Some("Jane Doe".into());
    p.sitename = Some("Example Corp".into());
    p.media_type = Some("application/pdf".into());
    p.bytes = Some(4_194_304);
    p.width = Some(1200);
    p.height = Some(900);
    p.sha256 = Some("deadbeef".repeat(8));
    p.retrieved_at = "2026-11-25T10:00:00Z".into();

    store.put_page(&p, 3600).await.unwrap();
    let mut got = store
        .get_page("https://example.com/doc.pdf", None)
        .await
        .unwrap()
        .unwrap();
    assert!(got.cache_age_s.is_some());
    got.cache_age_s = None;
    assert_eq!(got, p);

    // Compatibility with a row inserted with only the 12 core SPEC §8 columns (`meta_json = NULL`).
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    {
        let conn = Connection::open(store.path()).unwrap();
        conn.execute(
            "INSERT INTO pages (url, title, content, author, published_date, sitename, media_type, method, tokens_estimated, created_at, expires_at)
             VALUES ('https://example.com/raw', 'Raw', 'hello world', NULL, NULL, NULL, 'text/html', 'http', 3, ?1, ?2)",
            rusqlite::params![now, now + 3600],
        )
        .unwrap();
    }
    let raw = store
        .get_page("https://example.com/raw", None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(raw.title, "Raw");
    assert_eq!(raw.total_chars, "hello world".len());
    assert!(!raw.truncated);
    assert!(raw.retrieved_at.ends_with('Z'));
    store.close().await.unwrap();
}
