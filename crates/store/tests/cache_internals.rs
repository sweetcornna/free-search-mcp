//! Port of Python `tests/test_cache_internals.py` + `tests/test_resources.py` cache & FTS5 tests,
//! plus 1.0 legacy `cache.sqlite` coexistence and 200-write sweep tests.

use fsm_core::{Config, FetchMethod, PageResult, SearchResponse, StoreApi};
use fsm_store::fts::{invalid_fts_hint, is_bad_match_query};
use fsm_store::{DB_FILENAME, SCHEMA_USER_VERSION, SqliteStore, StoreConfig};
use rusqlite::Connection;
use std::time::SystemTime;
use tempfile::TempDir;

fn open_temp_store(dir: &TempDir) -> SqliteStore {
    SqliteStore::open_path(dir.path().join(DB_FILENAME)).expect("open store")
}

fn page(url: &str, title: &str, content: &str) -> PageResult {
    PageResult::new(url, title, content, FetchMethod::Http)
}

#[tokio::test]
async fn test_opens_search_mcp_sqlite_and_never_touches_legacy_cache_sqlite() {
    let dir = TempDir::new().unwrap();
    let legacy_path = dir.path().join("cache.sqlite");
    let legacy_bytes = b"LEGACY-V0182-DO-NOT-TOUCH";
    std::fs::write(&legacy_path, legacy_bytes).unwrap();
    let legacy_mtime_before = std::fs::metadata(&legacy_path).unwrap().modified().unwrap();

    let mut cfg = Config::defaults_for_home(dir.path());
    cfg.cache_dir = dir.path().to_path_buf();

    let store = SqliteStore::open(&cfg).unwrap();
    assert_eq!(store.path(), dir.path().join(DB_FILENAME));
    store
        .put_page(&page("https://example.com/1", "T", "hello"), 3600)
        .await
        .unwrap();
    store.maintain_now().await.unwrap();
    store.close().await.unwrap();

    // Legacy `cache.sqlite` must be completely untouched.
    assert_eq!(std::fs::read(&legacy_path).unwrap(), legacy_bytes);
    assert_eq!(
        std::fs::metadata(&legacy_path).unwrap().modified().unwrap(),
        legacy_mtime_before
    );

    // New DB must have user_version = 1 and all 10 tables.
    let conn = Connection::open(store.path()).unwrap();
    let ver: u32 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(ver, SCHEMA_USER_VERSION);

    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .unwrap();
    let tables: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    for expected in [
        "api_cache",
        "downloads",
        "exec_kv",
        "health_state",
        "pages",
        "pages_fts",
        "retractions",
        "scholar_works",
        "search_cache",
        "sync_state",
    ] {
        assert!(
            tables.iter().any(|t| t == expected),
            "missing table {expected}; found {tables:?}"
        );
    }
}

#[tokio::test]
async fn test_journal_mode_is_wal() {
    let dir = TempDir::new().unwrap();
    let store = open_temp_store(&dir);
    let conn = Connection::open(store.path()).unwrap();
    let mode: String = conn
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .unwrap();
    assert_eq!(mode.to_ascii_lowercase(), "wal");
    store.close().await.unwrap();
}

#[tokio::test]
async fn test_busy_timeout_and_user_version_are_set() {
    let dir = TempDir::new().unwrap();
    let store = open_temp_store(&dir);
    // Verify via a direct check that DB opened with user_version = 1 and concurrent lock wait works.
    let conn = Connection::open(store.path()).unwrap();
    let ver: u32 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(ver, 1);

    // Hold a short write lock on an external connection (150ms) and ensure the actor's
    // busy_timeout=5000 waits and succeeds rather than failing immediately with SQLITE_BUSY.
    conn.execute_batch("BEGIN IMMEDIATE;").unwrap();
    let store_clone = store.clone();
    let writer = tokio::spawn(async move {
        store_clone
            .put_page(&page("https://example.com/busy", "B", "waited"), 3600)
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    conn.execute_batch("COMMIT;").unwrap();
    drop(conn);

    writer.await.unwrap().unwrap();
    let got = store
        .get_page("https://example.com/busy", None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.content, "waited");
    store.close().await.unwrap();
}

#[tokio::test]
async fn test_concurrent_first_access_initializes_once() {
    // Missing test #8: exercise real concurrent first-open initialization on fresh DB files
    // through a barrier (5 rounds x 16 threads), verifying BEGIN IMMEDIATE prevents races.
    for round in 0..5 {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(format!("race_{round}.sqlite"));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
        let mut threads = Vec::new();
        for _ in 0..16 {
            let p = path.clone();
            let b = barrier.clone();
            threads.push(std::thread::spawn(move || {
                b.wait();
                SqliteStore::open_path(p)
            }));
        }
        let mut stores = Vec::new();
        for t in threads {
            stores.push(
                t.join()
                    .expect("thread panicked")
                    .expect("concurrent open_path must succeed"),
            );
        }
        for s in &stores {
            assert!(
                s.get_page("https://example.com/miss", None)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        for s in stores {
            s.close().await.unwrap();
        }
        let conn = Connection::open(&path).unwrap();
        let ver: u32 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(ver, SCHEMA_USER_VERSION);
    }
}

/// m3 regression: lazy reopen after `close()` runs `ActorHandle::spawn` via
/// `spawn_blocking`, so a SQLite `BEGIN IMMEDIATE` busy-wait never stalls a
/// single-threaded Tokio worker.
#[tokio::test(flavor = "current_thread")]
async fn test_lazy_reopen_does_not_block_single_threaded_tokio_worker() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    let dir = TempDir::new().unwrap();
    let store = open_temp_store(&dir);
    store
        .put_page(&page("https://example.com/reopen", "R", "ok"), 3600)
        .await
        .unwrap();
    store.close().await.unwrap();

    // Hold a write transaction on a side connection so `init_connection`'s `BEGIN IMMEDIATE`
    // blocks until the async ticker releases it. If lazy reopen blocked the current_thread
    // worker, the ticker could never run and `open` would time out after 5s!
    let (unlock_tx, unlock_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let (locked_tx, locked_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let lock_path = store.path().to_path_buf();
    let holder = std::thread::spawn(move || {
        let conn = Connection::open(&lock_path).unwrap();
        conn.execute_batch("BEGIN IMMEDIATE;").unwrap();
        let _ = locked_tx.send(());
        let _ = unlock_rx.recv();
        conn.execute_batch("COMMIT;").unwrap();
    });
    locked_rx.recv().unwrap();

    let ticks = Arc::new(AtomicU32::new(0));
    let ticks_bg = ticks.clone();
    let ticker = tokio::spawn(async move {
        for _ in 0..5 {
            tokio::time::sleep(std::time::Duration::from_millis(15)).await;
            ticks_bg.fetch_add(1, Ordering::SeqCst);
        }
        let _ = unlock_tx.send(());
    });

    let got = store
        .get_page("https://example.com/reopen", None)
        .await
        .unwrap()
        .expect("lazy reopen should succeed once lock releases");
    assert_eq!(got.content, "ok");

    ticker.await.unwrap();
    holder.join().unwrap();
    assert!(
        ticks.load(Ordering::SeqCst) >= 5,
        "Tokio worker must keep ticking during lazy reopen"
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn test_concurrent_put_page_all_succeed() {
    let dir = TempDir::new().unwrap();
    let store = open_temp_store(&dir);
    let mut handles = Vec::new();
    for i in 0..25 {
        let s = store.clone();
        handles.push(tokio::spawn(async move {
            s.put_page(
                &page(
                    &format!("https://example.com/{i}"),
                    &format!("t{i}"),
                    &format!("body {i}"),
                ),
                3600,
            )
            .await
        }));
    }
    for h in handles {
        h.await.unwrap().unwrap();
    }
    for i in 0..25 {
        let got = store
            .get_page(&format!("https://example.com/{i}"), None)
            .await
            .unwrap()
            .expect("page must exist");
        assert_eq!(got.content, format!("body {i}"));
    }
    store.close().await.unwrap();
}

#[tokio::test]
async fn test_external_writer_visible_through_wal() {
    let dir = TempDir::new().unwrap();
    let store = open_temp_store(&dir);
    let url = "https://example.com/wal";
    store
        .put_page(&page(url, "t", "original"), 3600)
        .await
        .unwrap();

    let conn = Connection::open(store.path()).unwrap();
    conn.busy_timeout(std::time::Duration::from_millis(5000))
        .unwrap();
    conn.execute(
        "UPDATE pages SET content = ?1 WHERE url = ?2",
        ["rewritten", url],
    )
    .unwrap();
    drop(conn);

    let got = store.get_page(url, None).await.unwrap().unwrap();
    assert_eq!(got.content, "rewritten");
    store.close().await.unwrap();
}

#[tokio::test]
async fn test_close_is_idempotent() {
    let dir = TempDir::new().unwrap();
    let store = open_temp_store(&dir);
    store.close().await.unwrap();
    // Second close must not error.
    store.close().await.unwrap();
    // Store remains usable afterwards (lazily re-opens).
    store
        .put_page(&page("https://x", "t", "b"), 3600)
        .await
        .unwrap();
    let got = store.get_page("https://x", None).await.unwrap().unwrap();
    assert_eq!(got.content, "b");
    store.close().await.unwrap();
}

#[tokio::test]
async fn test_search_pages_malformed_query_returns_empty() {
    let dir = TempDir::new().unwrap();
    let store = open_temp_store(&dir);
    store
        .put_page(
            &page("https://example.com/a", "Hello", "the quick brown fox"),
            3600,
        )
        .await
        .unwrap();

    for bad_query in [
        "a AND",
        "\"",
        "nosuchcol:val",
        "AND OR NOT",
        "foo(",
        "NEAR(",
        "*",
        "",
        "   ",
    ] {
        let hits = store.cache_search(bad_query, 10).await.unwrap();
        assert!(hits.is_empty(), "expected empty hits for {bad_query:?}");
    }
    store.close().await.unwrap();
}

/// Missing test #5: malformed FTS5 queries must return `[]` WITHOUT triggering
/// an O(corpus) rebuild (proved by planting a ghost posting that remains un-rebuilt).
#[tokio::test]
async fn test_malformed_fts_queries_never_trigger_rebuild() {
    let dir = TempDir::new().unwrap();
    let store = open_temp_store(&dir);
    store
        .put_page(
            &page("https://example.com/a", "Hello", "the quick brown fox"),
            3600,
        )
        .await
        .unwrap();

    // Plant an orphan posting pointing at missing rowid 9999.
    {
        let conn = Connection::open(store.path()).unwrap();
        conn.execute(
            "INSERT INTO pages_fts(rowid, title, content, author, sitename, url)
             VALUES (9999, 'ghost', 'ghostterm', NULL, NULL, 'https://example.com/ghost')",
            [],
        )
        .unwrap();
    }

    for bad_query in [
        "a AND",
        "\"",
        "\"unclosed",
        "nosuchcol:val",
        "AND OR NOT",
        "foo(",
        "NEAR(",
        "*",
        "site:example.com",
        "C++",
        "node.js",
        "foo-bar",
        "NEAR(a b, -1)",
        "@user",
        "100%",
    ] {
        let hits = store.cache_search(bad_query, 10).await.unwrap();
        assert!(hits.is_empty(), "expected empty hits for {bad_query:?}");
    }

    // Raw query for `'ghostterm'` MUST still fail — proving none of the malformed
    // queries above triggered a rebuild of `pages_fts`.
    {
        let conn = Connection::open(store.path()).unwrap();
        let raw_res = conn
            .prepare("SELECT url FROM pages_fts WHERE pages_fts MATCH 'ghostterm'")
            .and_then(|mut s| {
                s.query_map([], |r| r.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()
            });
        assert!(
            raw_res.is_err(),
            "orphan ghost posting disappeared — a malformed query triggered an FTS rebuild!"
        );
    }
    store.close().await.unwrap();
}

#[tokio::test]
async fn test_search_pages_valid_query_still_works_and_preserves_metadata() {
    let dir = TempDir::new().unwrap();
    let store = open_temp_store(&dir);

    let mut p1 = page(
        "https://example.com/a",
        "Café Guide",
        "the quick brown fox jumps over résumé",
    );
    p1.author = Some("Ada Lovelace".into());
    p1.published_date = Some("2026-01-15".into());
    p1.sitename = Some("Example Blog".into());
    store.put_page(&p1, 3600).await.unwrap();
    store
        .put_page(
            &page("https://example.com/b", "Other", "lazy dog sleeps"),
            3600,
        )
        .await
        .unwrap();

    let hits = store.cache_search("quick", 10).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].url, "https://example.com/a");
    assert_eq!(hits[0].title, "Café Guide");
    assert!(hits[0].snippet.contains("[quick]"), "{}", hits[0].snippet);
    assert_eq!(hits[0].author.as_deref(), Some("Ada Lovelace"));
    assert_eq!(hits[0].date.as_deref(), Some("2026-01-15"));
    assert_eq!(hits[0].sitename.as_deref(), Some("Example Blog"));

    // Diacritics removal (`unicode61 remove_diacritics 2`).
    let cafe_hits = store.cache_search("cafe", 10).await.unwrap();
    assert_eq!(cafe_hits.len(), 1);
    assert_eq!(cafe_hits[0].url, "https://example.com/a");
    assert!(
        cafe_hits[0].snippet.contains('['),
        "title match should still produce highlighted snippet: {}",
        cafe_hits[0].snippet
    );

    let resume_hits = store.cache_search("resume", 10).await.unwrap();
    assert_eq!(resume_hits.len(), 1);
    assert_eq!(resume_hits[0].url, "https://example.com/a");

    // m5: valid column-qualified FTS5 queries on `title` (hit and legitimate 0-hit miss).
    let title_hit = store.cache_search("title:Guide", 10).await.unwrap();
    assert_eq!(title_hit.len(), 1);
    assert_eq!(title_hit[0].url, "https://example.com/a");
    assert!(
        store
            .cache_search("title:val", 10)
            .await
            .unwrap()
            .is_empty()
    );

    // m1: valid FTS5 phrase prefix query `"quick" *` works.
    let prefix_phrase_hits = store.cache_search(r#""quick" *"#, 10).await.unwrap();
    assert_eq!(prefix_phrase_hits.len(), 1);
    assert_eq!(prefix_phrase_hits[0].url, "https://example.com/a");

    store.close().await.unwrap();
}

#[tokio::test]
async fn test_maintain_purges_expired_rows() {
    let dir = TempDir::new().unwrap();
    let store = open_temp_store(&dir);

    store
        .search_cache_put("old_s", &SearchResponse::new("q", "web"), 3600)
        .await
        .unwrap();
    store
        .put_page(&page("https://old.example/", "t", "old body text"), 3600)
        .await
        .unwrap();
    store
        .put_page(&page("https://new.example/", "t", "fresh body text"), 3600)
        .await
        .unwrap();
    store
        .api_cache_put("old_api", "arxiv", b"abc", None, None, 3600)
        .await
        .unwrap();
    store
        .kv_put("sess", "old_kv", &serde_json::json!({"x": 1}), 3600)
        .await
        .unwrap();

    // Rewind `expires_at` on the "old" rows to the past.
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let stale = now - 10_000;
    {
        let conn = Connection::open(store.path()).unwrap();
        conn.execute(
            "UPDATE search_cache SET created_at = ?1, expires_at = ?1 WHERE cache_key = 'old_s'",
            [stale],
        )
        .unwrap();
        conn.execute(
            "UPDATE pages SET created_at = ?1, expires_at = ?1 WHERE url = 'https://old.example/'",
            [stale],
        )
        .unwrap();
        conn.execute(
            "UPDATE api_cache SET created_at = ?1, expires_at = ?1 WHERE cache_key = 'old_api'",
            [stale],
        )
        .unwrap();
        conn.execute(
            "UPDATE exec_kv SET expires_at = ?1 WHERE key = 'old_kv'",
            [stale],
        )
        .unwrap();
    }

    store.maintain_now().await.unwrap();

    assert!(
        store
            .search_cache_get("old_s", None)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .get_page("https://old.example/", None)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .get_page("https://new.example/", None)
            .await
            .unwrap()
            .is_some()
    );
    assert!(store.api_cache_get("old_api").await.unwrap().is_none());
    assert!(store.kv_get("sess", "old_kv").await.unwrap().is_none());

    // FTS stays consistent: purged page is gone, fresh page remains searchable.
    assert!(store.cache_search("old", 10).await.unwrap().is_empty());
    assert_eq!(store.cache_search("fresh", 10).await.unwrap().len(), 1);
    store.fts_integrity_check().await.unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn test_maintain_enforces_size_cap_dropping_oldest() {
    let dir = TempDir::new().unwrap();
    let mut cfg = StoreConfig::at_path(dir.path().join(DB_FILENAME));
    cfg.cache_max_mb = 1;
    cfg.maintain_every = 0; // trigger manually via maintain_now
    let store = SqliteStore::open_with(cfg).unwrap();

    // 40 pages x ~50KB => > 2MB DB + FTS; cap at 1MB.
    let body = "x".repeat(50_000);
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    for i in 0..40_i64 {
        store
            .put_page(
                &page(&format!("https://example.com/{i}"), &format!("t{i}"), &body),
                1_000_000_000,
            )
            .await
            .unwrap();
    }
    // Give rows deterministic increasing created_at timestamps so oldest = 0, newest = 39.
    {
        let conn = Connection::open(store.path()).unwrap();
        for i in 0..40_i64 {
            conn.execute(
                "UPDATE pages SET created_at = ?1 WHERE url = ?2",
                rusqlite::params![now - (40 - i), format!("https://example.com/{i}")],
            )
            .unwrap();
        }
    }
    assert!(store.db_size_bytes() > 1024 * 1024);

    store.maintain_now().await.unwrap();
    assert!(
        store.db_size_bytes() <= 1024 * 1024,
        "db_size_bytes {} > 1 MiB",
        store.db_size_bytes()
    );

    let conn = Connection::open(store.path()).unwrap();
    let (count, newest, oldest): (i64, i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), MAX(created_at), MIN(created_at) FROM pages",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert!(count > 0 && count < 40, "count = {count}");
    assert_eq!(newest, now - 1);
    assert!(oldest > now - 40);

    store.fts_integrity_check().await.unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn test_maintain_disabled_when_cap_is_zero() {
    let dir = TempDir::new().unwrap();
    let mut cfg = StoreConfig::at_path(dir.path().join(DB_FILENAME));
    cfg.cache_max_mb = 0;
    cfg.maintain_every = 0;
    let store = SqliteStore::open_with(cfg).unwrap();

    let body = "y".repeat(50_000);
    for i in 0..30 {
        store
            .put_page(
                &page(&format!("https://example.com/z{i}"), "t", &body),
                1_000_000_000,
            )
            .await
            .unwrap();
    }
    assert!(store.db_size_bytes() > 0);
    store.maintain_now().await.unwrap();

    let conn = Connection::open(store.path()).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM pages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 30);
    store.close().await.unwrap();
}

#[tokio::test]
async fn test_writes_trigger_maintenance_on_cadence() {
    let dir = TempDir::new().unwrap();
    let mut cfg = StoreConfig::at_path(dir.path().join(DB_FILENAME));
    cfg.maintain_every = 5;
    cfg.maintain_on_open = false;
    let store = SqliteStore::open_with(cfg).unwrap();

    for i in 0..12 {
        store
            .put_page(
                &page(&format!("https://example.com/c{i}"), "t", "body"),
                3600,
            )
            .await
            .unwrap();
    }
    assert_eq!(store.maintain_count().await.unwrap(), 2); // writes #5 and #10
    store.close().await.unwrap();
}

#[tokio::test]
async fn test_default_200_write_sweep_runs_automatically() {
    let dir = TempDir::new().unwrap();
    let store = open_temp_store(&dir);
    // Default config runs 1 startup sweep on open (I1).
    assert_eq!(store.maintain_count().await.unwrap(), 1);

    // Seed an expired row that should be swept on write #200.
    store
        .put_page(&page("https://example.com/expired", "E", "stale"), 3600)
        .await
        .unwrap();
    {
        let conn = Connection::open(store.path()).unwrap();
        conn.execute(
            "UPDATE pages SET expires_at = 1 WHERE url = 'https://example.com/expired'",
            [],
        )
        .unwrap();
    }

    // 199 more writes bring total to 200 -> triggers cadence sweep #2.
    for i in 0..199 {
        store
            .api_cache_put(&format!("k{i}"), "src", b"v", None, None, 3600)
            .await
            .unwrap();
    }
    assert_eq!(store.maintain_count().await.unwrap(), 2);

    let conn = Connection::open(store.path()).unwrap();
    let remaining: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pages WHERE url = 'https://example.com/expired'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(remaining, 0);
    store.close().await.unwrap();
}

/// I1 regression / Missing test #3: opening a store with default `maintain_on_open = true`
/// purges expired rows and enforces `cache_max_mb` even with zero writes (short-lived stdio).
#[tokio::test]
async fn test_maintain_on_open_purges_expired_rows_and_enforces_cap_without_writes() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join(DB_FILENAME);

    // Seed 20 expired pages + 30 fresh 50 KB pages (> 1 MiB) with maintenance disabled.
    {
        let mut seed_cfg = StoreConfig::at_path(&db_path);
        seed_cfg.cache_max_mb = 0;
        seed_cfg.maintain_every = 0;
        seed_cfg.maintain_on_open = false;
        let seed = SqliteStore::open_with(seed_cfg).unwrap();
        let body = "z".repeat(50_000);
        for i in 0..50 {
            seed.put_page(
                &page(&format!("https://example.com/p{i}"), "T", &body),
                1_000_000,
            )
            .await
            .unwrap();
        }
        {
            let conn = Connection::open(&db_path).unwrap();
            for i in 0..20 {
                conn.execute(
                    "UPDATE pages SET expires_at = 1 WHERE url = ?1",
                    [format!("https://example.com/p{i}")],
                )
                .unwrap();
            }
        }
        // Also verify `complete_page_urls` filters out expired rows even before maintenance.
        let active_urls = seed.complete_page_urls("example.com/p", 100).await.unwrap();
        assert_eq!(active_urls.len(), 30);
        assert!(seed.db_size_bytes() > 1024 * 1024);
        seed.close().await.unwrap();
    }

    // Reopen with `cache_max_mb = 1` and default `maintain_on_open = true`; perform ZERO writes.
    let mut cfg = StoreConfig::at_path(&db_path);
    cfg.cache_max_mb = 1;
    let store = SqliteStore::open_with(cfg).unwrap();

    // `maintain_count()` queues behind startup maintenance on the actor thread, so it
    // deterministically observes the completed open sweep without any sleep.
    assert_eq!(store.maintain_count().await.unwrap(), 1);
    assert!(
        store.db_size_bytes() <= 1024 * 1024,
        "db_size_bytes {} > 1 MiB after open sweep",
        store.db_size_bytes()
    );

    let conn = Connection::open(&db_path).unwrap();
    let expired_left: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pages WHERE expires_at <= 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(expired_left, 0);
    store.fts_integrity_check().await.unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn test_refetching_a_url_keeps_it_findable() {
    let dir = TempDir::new().unwrap();
    let store = open_temp_store(&dir);
    for i in 0..4 {
        store
            .put_page(
                &page(
                    "https://example.com/a",
                    &format!("Title {i}"),
                    &format!("body revision {i} zebrafish"),
                ),
                3600,
            )
            .await
            .unwrap();
    }
    let hits = store.cache_search("zebrafish", 10).await.unwrap();
    let urls: Vec<&str> = hits.iter().map(|h| h.url.as_str()).collect();
    assert_eq!(urls, vec!["https://example.com/a"]);
    store.close().await.unwrap();
}

#[tokio::test]
async fn test_refetching_replaces_the_old_text_in_the_index() {
    let dir = TempDir::new().unwrap();
    let store = open_temp_store(&dir);
    store
        .put_page(
            &page("https://example.com/a", "T", "aardvark original"),
            3600,
        )
        .await
        .unwrap();
    store
        .put_page(
            &page("https://example.com/a", "T", "buffalo replacement"),
            3600,
        )
        .await
        .unwrap();
    assert!(store.cache_search("aardvark", 10).await.unwrap().is_empty());
    assert_eq!(store.cache_search("buffalo", 10).await.unwrap().len(), 1);
    store.close().await.unwrap();
}

#[tokio::test]
async fn test_refetching_keeps_the_rowid() {
    let dir = TempDir::new().unwrap();
    let store = open_temp_store(&dir);
    let mut p = page("https://example.com/a", "T", "one");
    store.put_page(&p, 3600).await.unwrap();

    let conn = Connection::open(store.path()).unwrap();
    let before: i64 = conn
        .query_row(
            "SELECT id FROM pages WHERE url = 'https://example.com/a'",
            [],
            |r| r.get(0),
        )
        .unwrap();

    p.content = "two".into();
    p.author = Some("Ada".into());
    store.put_page(&p, 3600).await.unwrap();
    p.content = "three".into();
    p.author = None;
    store.put_page(&p, 3600).await.unwrap();

    let after: i64 = conn
        .query_row(
            "SELECT id FROM pages WHERE url = 'https://example.com/a'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(after, before);
    store.fts_integrity_check().await.unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn test_fts_integrity_survives_many_rewrites() {
    let dir = TempDir::new().unwrap();
    let store = open_temp_store(&dir);
    for i in 0..8 {
        store
            .put_page(
                &page("https://example.com/a", "T", &format!("content {i}")),
                3600,
            )
            .await
            .unwrap();
        store
            .put_page(
                &page("https://example.com/b", "T", &format!("other {i}")),
                3600,
            )
            .await
            .unwrap();
    }
    store.fts_integrity_check().await.unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn test_a_desynced_index_is_rebuilt_instead_of_reported_as_empty() {
    let dir = TempDir::new().unwrap();
    let store = open_temp_store(&dir);
    store
        .put_page(&page("https://example.com/a", "T", "narwhal"), 3600)
        .await
        .unwrap();

    // Forge orphaned posting in `pages_fts` pointing to a missing rowid 9999.
    {
        let conn = Connection::open(store.path()).unwrap();
        conn.execute(
            "INSERT INTO pages_fts(rowid, title, content, author, sitename, url)
             VALUES (9999, 'ghost', 'narwhal', NULL, NULL, 'https://example.com/ghost')",
            [],
        )
        .unwrap();
        let raw_err = conn
            .prepare("SELECT url FROM pages_fts WHERE pages_fts MATCH 'narwhal'")
            .and_then(|mut s| {
                s.query_map([], |r| r.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()
            });
        assert!(
            raw_err.is_err(),
            "corrupted FTS index should fail raw query"
        );
    }

    // `cache_search` detects the desync, rebuilds `pages_fts`, and returns the real row.
    let hits = store.cache_search("narwhal", 10).await.unwrap();
    let urls: Vec<&str> = hits.iter().map(|h| h.url.as_str()).collect();
    assert_eq!(urls, vec!["https://example.com/a"]);
    store.fts_integrity_check().await.unwrap();
    store.close().await.unwrap();
}

#[test]
fn test_a_desync_is_classified_the_same_under_either_sqlite_wording() {
    for text in [
        "fts5: missing row 9999 from content table 'main'.'pages'",
        "database disk image is malformed",
        "database or disk is full",
    ] {
        assert!(!is_bad_match_query(text), "{text}");
    }
}

#[test]
fn test_a_bad_match_query_is_never_treated_as_corruption() {
    for text in [
        "fts5: syntax error near \"\"",
        "no such column: nosuchcol",
        "unknown special query: foo",
        "unterminated string",
    ] {
        assert!(is_bad_match_query(text), "{text}");
    }
}

#[tokio::test]
async fn test_a_malformed_query_is_still_just_no_matches() {
    let dir = TempDir::new().unwrap();
    let store = open_temp_store(&dir);
    store
        .put_page(&page("https://example.com/a", "T", "narwhal"), 3600)
        .await
        .unwrap();
    assert!(
        store
            .cache_search("\"unclosed", 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .cache_search("nosuchcol:x", 10)
            .await
            .unwrap()
            .is_empty()
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn test_existing_caches_are_rebuilt_once_on_open() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join(DB_FILENAME);

    let first = SqliteStore::open_path(&path).unwrap();
    first
        .put_page(&page("https://example.com/a", "T", "pangolin"), 3600)
        .await
        .unwrap();
    first.close().await.unwrap();

    // Reset `user_version = 0` and inject an orphaned FTS5 row.
    {
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", 0u32).unwrap();
        conn.execute(
            "INSERT INTO pages_fts(rowid, title, content, author, sitename, url)
             VALUES (4242, 'g', 'pangolin', NULL, NULL, 'https://example.com/ghost')",
            [],
        )
        .unwrap();
    }

    let reopened = SqliteStore::open_path(&path).unwrap();
    let hits = reopened.cache_search("pangolin", 10).await.unwrap();
    let urls: Vec<&str> = hits.iter().map(|h| h.url.as_str()).collect();
    assert_eq!(urls, vec!["https://example.com/a"]);
    reopened.fts_integrity_check().await.unwrap();
    reopened.close().await.unwrap();

    let conn = Connection::open(&path).unwrap();
    let ver: u32 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(ver, SCHEMA_USER_VERSION);
}

#[test]
fn test_invalid_fts_hint_helper() {
    // Port of `tests/test_resources.py::test_invalid_fts_hint_helper`.
    for bad in [
        "a AND",
        "AND",
        "NOT term",
        "\"open phrase",
        "(a OR b",
        ")a(",
        "a AND OR b",
    ] {
        let hint = invalid_fts_hint(bad);
        assert!(hint.is_some(), "expected hint for {bad:?}");
        let h = hint.unwrap().to_ascii_lowercase();
        assert!(!h.contains("operationalerror"), "{h}");
        assert!(!h.contains("syntax error"), "{h}");
    }
    assert!(invalid_fts_hint("a AND").unwrap().contains("AND"));
    assert!(
        invalid_fts_hint("\"unterminated")
            .unwrap()
            .to_ascii_lowercase()
            .contains("quote")
    );

    // Valid queries -> None (including m1 phrase-prefix `"a b" *`).
    for good in [
        "hello world",
        "cats AND dogs",
        "\"exact phrase\"",
        r#""a b" *"#,
        "(a OR b) c",
        "async*",
        "",
        "   ",
    ] {
        assert_eq!(invalid_fts_hint(good), None, "unexpected hint for {good:?}");
    }
}

#[tokio::test]
async fn test_complete_page_urls_and_recent_search_keys() {
    let dir = TempDir::new().unwrap();
    let store = open_temp_store(&dir);

    store
        .put_page(&page("https://example.com/100%_done", "A", "body"), 3600)
        .await
        .unwrap();
    store
        .put_page(&page("https://example.com/100x_done", "B", "body"), 3600)
        .await
        .unwrap();

    // `%` must be escaped literally, not match `x`.
    let urls = store.complete_page_urls("100%", 10).await.unwrap();
    assert_eq!(urls, vec!["https://example.com/100%_done"]);

    store
        .search_cache_put("v4:web:a", &SearchResponse::new("q1", "web"), 3600)
        .await
        .unwrap();
    store
        .search_cache_put("v4:news:b", &SearchResponse::new("q2", "news"), 3600)
        .await
        .unwrap();
    let keys = store.recent_search_keys("v4:web:", 10).await.unwrap();
    assert_eq!(keys, vec!["v4:web:a"]);

    store.close().await.unwrap();
}

#[tokio::test]
async fn test_cache_search_excludes_expired_pages_before_maintenance_and_highlights_title_with_brackets_in_body()
 {
    let dir = TempDir::new().unwrap();
    let store = open_temp_store(&dir);

    // Body contains literal Markdown `[link]` brackets, while query matches ONLY in title.
    store
        .put_page(
            &page(
                "https://example.com/md",
                "Quantum Entanglement Primer",
                "See [docs](https://example.com) and array [1, 2, 3] for details.",
            ),
            3600,
        )
        .await
        .unwrap();

    let hits = store.cache_search("entanglement", 10).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert!(
        hits[0].snippet.contains("[Entanglement]"),
        "expected title highlight, got: {}",
        hits[0].snippet
    );

    // Expire the page without running maintenance; `cache_search` must exclude it immediately.
    {
        let conn = Connection::open(store.path()).unwrap();
        conn.execute(
            "UPDATE pages SET expires_at = 1 WHERE url = 'https://example.com/md'",
            [],
        )
        .unwrap();
    }
    assert!(
        store
            .cache_search("entanglement", 10)
            .await
            .unwrap()
            .is_empty()
    );
    store.close().await.unwrap();
}
