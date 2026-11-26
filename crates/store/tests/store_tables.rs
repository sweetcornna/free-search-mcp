//! Comprehensive tests for all 10 tables, all 3 M7 `exec_kv` session modes,
//! 2 MiB value cap, 64 MiB LRU, 168h TTL ceiling, key regex proptest,
//! `api_cache`, `scholar_works`, `retractions`, `sync_state`, `health_state`,
//! and real two-process cross-process `fd-lock` leases.

use fsm_core::{
    AuthorRef, FsmError, HealthRecord, OaLocation, RetractionNotice, ScholarlyWork, StoreApi,
    WorkVersion,
};
use fsm_store::lock::{FileLease, FileLock};
use fsm_store::{
    DB_FILENAME, EXEC_KV_DEFAULT_TTL_S, EXEC_KV_MAX_TTL_S, EXEC_KV_MAX_VAL_BYTES,
    EXEC_KV_OVERFLOW_SESSION, EXEC_KV_SHARED_SESSION, SqliteStore, StoreConfig, validate_kv_key,
    validate_user_kv_key,
};
use proptest::prelude::*;
use regex::Regex;
use rusqlite::Connection;
use serde_json::json;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::TempDir;

fn open_temp_store(dir: &TempDir) -> SqliteStore {
    SqliteStore::open_path(dir.path().join(DB_FILENAME)).expect("open store")
}

// ===========================================================================
// M7: All 3 `exec_kv` session modes + guardrails
// ===========================================================================

#[tokio::test]
async fn test_exec_kv_m7_all_three_session_modes() {
    let dir = TempDir::new().unwrap();
    let store = open_temp_store(&dir);

    // Mode 1: `exec:<id>` overflow handle stored under session_id = ""
    // and readable from ANY session (`kv_get` ignores `session_id` for `exec:*`).
    let overflow_val = json!({"rows": [1, 2, 3], "truncated": true});
    store
        .kv_put(
            EXEC_KV_OVERFLOW_SESSION,
            "exec:k7m9p2q1",
            &overflow_val,
            86_400,
        )
        .await
        .unwrap();
    assert_eq!(
        store.kv_get("", "exec:k7m9p2q1").await.unwrap(),
        Some(overflow_val.clone())
    );
    assert_eq!(
        store
            .kv_get("session-alpha", "exec:k7m9p2q1")
            .await
            .unwrap(),
        Some(overflow_val.clone())
    );
    assert_eq!(
        store.kv_get("session-beta", "exec:k7m9p2q1").await.unwrap(),
        Some(overflow_val.clone())
    );
    assert_eq!(
        store
            .kv_get(EXEC_KV_SHARED_SESSION, "exec:k7m9p2q1")
            .await
            .unwrap(),
        Some(overflow_val)
    );

    // Mode 2: `{shared: true}` stored under `session_id = "shared"`
    // and readable by `"shared"` AND any caller session.
    let shared_val = json!({"topic": "cross-session"});
    store
        .kv_put_user("session-alpha", "shared_notes", &shared_val, None, true)
        .await
        .unwrap();
    assert_eq!(
        store
            .kv_get(EXEC_KV_SHARED_SESSION, "shared_notes")
            .await
            .unwrap(),
        Some(shared_val.clone())
    );
    assert_eq!(
        store.kv_get("session-alpha", "shared_notes").await.unwrap(),
        Some(shared_val.clone())
    );
    assert_eq!(
        store.kv_get("session-beta", "shared_notes").await.unwrap(),
        Some(shared_val.clone())
    );

    // Mode 3: Normal `cache.put` stored under caller `session_id` — isolated from other sessions!
    let alpha_val = json!({"secret": "alpha-only"});
    store
        .kv_put_user(
            "session-alpha",
            "private_item",
            &alpha_val,
            Some(3600),
            false,
        )
        .await
        .unwrap();
    assert_eq!(
        store.kv_get("session-alpha", "private_item").await.unwrap(),
        Some(alpha_val)
    );
    assert_eq!(
        store.kv_get("session-beta", "private_item").await.unwrap(),
        None
    );
    assert_eq!(
        store
            .kv_get(EXEC_KV_SHARED_SESSION, "private_item")
            .await
            .unwrap(),
        None
    );

    // Caller's private key shadows a shared key of the same name for that session only.
    let alpha_override = json!({"topic": "alpha-override"});
    store
        .kv_put("session-alpha", "shared_notes", &alpha_override, 3600)
        .await
        .unwrap();
    assert_eq!(
        store.kv_get("session-alpha", "shared_notes").await.unwrap(),
        Some(alpha_override)
    );
    assert_eq!(
        store.kv_get("session-beta", "shared_notes").await.unwrap(),
        Some(shared_val.clone())
    );
    assert_eq!(
        store
            .kv_get(EXEC_KV_SHARED_SESSION, "shared_notes")
            .await
            .unwrap(),
        Some(shared_val)
    );

    // Verify exact `session_id` values persisted in SQLite.
    let conn = Connection::open(store.path()).unwrap();
    let sess: String = conn
        .query_row(
            "SELECT session_id FROM exec_kv WHERE key = 'exec:k7m9p2q1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(sess, "");

    store.close().await.unwrap();
}

#[tokio::test]
async fn test_exec_kv_rejects_bad_keys_and_user_exec_prefix() {
    let dir = TempDir::new().unwrap();
    let store = open_temp_store(&dir);
    let v = json!(1);

    // User sessions and "shared" MUST NOT write `exec:*` keys.
    for sess in ["session-1", "shared"] {
        let err = store
            .kv_put(sess, "exec:abc12345", &v, 3600)
            .await
            .unwrap_err();
        assert!(matches!(err, FsmError::InvalidInput(_)), "{sess}: {err}");
    }
    // `kv_put_user` rejects `exec:*` even if caller session happens to be "".
    assert!(matches!(
        store
            .kv_put_user("", "exec:abc12345", &v, None, false)
            .await,
        Err(FsmError::InvalidInput(_))
    ));
    // Bare "exec:" without an identifier is rejected even for overflow.
    assert!(matches!(
        store.kv_put("", "exec:", &v, 3600).await,
        Err(FsmError::InvalidInput(_))
    ));

    // Invalid regex characters & lengths.
    let too_long = "a".repeat(129);
    for bad_key in [
        "",
        "has space",
        "path/slash",
        "back\\slash",
        "dollar$sign",
        "unicode_中文",
        too_long.as_str(),
    ] {
        let err = store.kv_put("sess", bad_key, &v, 3600).await.unwrap_err();
        assert!(
            matches!(err, FsmError::InvalidInput(_)),
            "{bad_key:?}: {err}"
        );
    }

    // Boundary lengths 1 and 128 + all allowed punctuation `._:-`.
    let max_len_key = format!("aA0._:-{}", "z".repeat(121));
    assert_eq!(max_len_key.len(), 128);
    store.kv_put("sess", "x", &v, 3600).await.unwrap();
    store.kv_put("sess", &max_len_key, &v, 3600).await.unwrap();
    assert_eq!(store.kv_get("sess", &max_len_key).await.unwrap(), Some(v));
    store.close().await.unwrap();
}

#[tokio::test]
async fn test_exec_kv_2mib_val_cap_and_168h_ttl_cap() {
    let dir = TempDir::new().unwrap();
    let store = open_temp_store(&dir);

    // Exact 2 MiB raw JSON (`"..."` adds 2 bytes for quotes).
    let exact_ok = json!("a".repeat(EXEC_KV_MAX_VAL_BYTES - 2));
    assert_eq!(
        serde_json::to_vec(&exact_ok).unwrap().len(),
        EXEC_KV_MAX_VAL_BYTES
    );
    store.kv_put("s", "max_ok", &exact_ok, 3600).await.unwrap();
    assert_eq!(store.kv_get("s", "max_ok").await.unwrap(), Some(exact_ok));

    // 2 MiB + 1 byte -> InvalidInput.
    let over = json!("a".repeat(EXEC_KV_MAX_VAL_BYTES - 1));
    assert_eq!(
        serde_json::to_vec(&over).unwrap().len(),
        EXEC_KV_MAX_VAL_BYTES + 1
    );
    let err = store.kv_put("s", "too_big", &over, 3600).await.unwrap_err();
    assert!(matches!(err, FsmError::InvalidInput(_)));

    // TTL > 168h is capped to 168h; TTL == 0 defaults to 24h.
    let before = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    store
        .kv_put("s", "ttl_clamped", &json!(1), 999 * 3600)
        .await
        .unwrap();
    store
        .kv_put("s", "ttl_default", &json!(2), 0)
        .await
        .unwrap();
    let after = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    let conn = Connection::open(store.path()).unwrap();
    let exp_clamped: i64 = conn
        .query_row(
            "SELECT expires_at FROM exec_kv WHERE key = 'ttl_clamped'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let exp_default: i64 = conn
        .query_row(
            "SELECT expires_at FROM exec_kv WHERE key = 'ttl_default'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(exp_clamped <= after + EXEC_KV_MAX_TTL_S as i64);
    assert!(exp_clamped >= before + EXEC_KV_MAX_TTL_S as i64);
    assert!(exp_default <= after + EXEC_KV_DEFAULT_TTL_S as i64);
    assert!(exp_default >= before + EXEC_KV_DEFAULT_TTL_S as i64);

    store.close().await.unwrap();
}

#[tokio::test]
async fn test_exec_kv_lru_eviction_respects_kv_get_recency() {
    let dir = TempDir::new().unwrap();
    let mut cfg = StoreConfig::at_path(dir.path().join(DB_FILENAME));
    cfg.exec_kv_lru_max_bytes = 200; // tight cap for fast deterministic LRU test
    let store = SqliteStore::open_with(cfg).unwrap();

    // Each value is ~82 raw JSON bytes (`"..."` of 80 chars).
    let val = |c: char| json!(std::iter::repeat_n(c, 80).collect::<String>());

    store.kv_put("s", "k1", &val('a'), 3600).await.unwrap(); // ~82 B
    store.kv_put("s", "k2", &val('b'), 3600).await.unwrap(); // ~164 B <= 200 B

    // Access `k1` via `kv_get` so `k1` becomes MORE recently used than `k2`.
    assert!(store.kv_get("s", "k1").await.unwrap().is_some());

    // Inserting `k3` (~246 B > 200 B) must evict LRU `k2`, preserving `k1` and `k3`.
    store.kv_put("s", "k3", &val('c'), 3600).await.unwrap();
    assert!(
        store.kv_get("s", "k2").await.unwrap().is_none(),
        "k2 should be evicted"
    );
    assert!(
        store.kv_get("s", "k1").await.unwrap().is_some(),
        "k1 was touched and should survive"
    );
    assert!(
        store.kv_get("s", "k3").await.unwrap().is_some(),
        "k3 just inserted should survive"
    );

    // Verify SUM(raw_bytes) <= cap in SQLite.
    let conn = Connection::open(store.path()).unwrap();
    let sum_bytes: i64 = conn
        .query_row("SELECT COALESCE(SUM(raw_bytes), 0) FROM exec_kv", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(sum_bytes <= 200, "sum_bytes = {sum_bytes}");

    store.close().await.unwrap();
}

// ===========================================================================
// `api_cache`, `scholar_works`, `retractions`, `sync_state`, `health_state`
// ===========================================================================

#[tokio::test]
async fn test_api_cache_round_trip_and_expiry() {
    let dir = TempDir::new().unwrap();
    let store = open_temp_store(&dir);

    store
        .api_cache_put(
            "api:wb:usa",
            "worldbank",
            b"{\"gdp\":123}",
            Some("W/\"etag-1\""),
            Some("Wed, 25 Nov 2026 00:00:00 GMT"),
            3600,
        )
        .await
        .unwrap();

    let (body, etag, last_mod) = store.api_cache_get("api:wb:usa").await.unwrap().unwrap();
    assert_eq!(body.as_ref(), b"{\"gdp\":123}");
    assert_eq!(etag.as_deref(), Some("W/\"etag-1\""));
    assert_eq!(last_mod.as_deref(), Some("Wed, 25 Nov 2026 00:00:00 GMT"));

    // Empty body round-trips cleanly.
    store
        .api_cache_put("api:empty", "src", b"", None, None, 3600)
        .await
        .unwrap();
    let (empty, _, _) = store.api_cache_get("api:empty").await.unwrap().unwrap();
    assert!(empty.is_empty());

    // ttl_s == 0 clears / never caches.
    store
        .api_cache_put("api:wb:usa", "worldbank", b"x", None, None, 0)
        .await
        .unwrap();
    assert!(store.api_cache_get("api:wb:usa").await.unwrap().is_none());

    store.close().await.unwrap();
}

#[tokio::test]
async fn test_scholar_works_multi_pid_lookup() {
    let dir = TempDir::new().unwrap();
    let store = open_temp_store(&dir);

    let mut work = ScholarlyWork::new(
        "Attention Is All You Need",
        "https://arxiv.org/abs/1706.03762",
    );
    work.authors = vec![AuthorRef::new("Ashish Vaswani")];
    work.year = Some(2017);
    work.work_type = "preprint".into();
    work.ids.doi = Some("https://doi.org/10.48550/arXiv.1706.03762".into());
    work.ids.arxiv = Some("arXiv:1706.03762v7".into());
    work.ids.pmid = Some("PMID:99887766".into());
    work.ids.openalex = Some("https://openalex.org/W2626778328".into());
    work.oa = Some(OaLocation::new("green"));
    work.versions = vec![WorkVersion::new(
        "preprint",
        "https://arxiv.org/abs/1706.03762v7",
    )];
    work.sources = vec!["arxiv".into(), "openalex".into()];

    store
        .scholar_put("doi:10.48550/arxiv.1706.03762", &work, 30 * 86_400)
        .await
        .unwrap();

    // Lookup by canonical_key, DOI variants, arXiv variants, PMID, and OpenAlex.
    for probe in [
        "doi:10.48550/arxiv.1706.03762",
        "10.48550/ARXIV.1706.03762",
        "https://doi.org/10.48550/arXiv.1706.03762",
        "1706.03762",
        "arXiv:1706.03762v5",
        "https://arxiv.org/abs/1706.03762v2",
        "99887766",
        "PMID:99887766",
        "W2626778328",
        "https://openalex.org/w2626778328",
    ] {
        let got = store
            .scholar_get(probe)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("missed scholar_get({probe:?})"));
        assert_eq!(got, work, "mismatch for {probe:?}");
    }

    // Legacy arXiv ID with 'v' inside category (`solv-int/9901001v2`, S5).
    let mut legacy = ScholarlyWork::new("Legacy", "https://arxiv.org/abs/solv-int/9901001");
    legacy.ids.arxiv = Some("solv-int/9901001v2".into());
    store
        .scholar_put("arxiv:solv-int/9901001", &legacy, 3600)
        .await
        .unwrap();
    assert_eq!(
        store.scholar_get("solv-int/9901001v1").await.unwrap(),
        Some(legacy)
    );

    assert!(
        store
            .scholar_get("10.9999/nonexistent")
            .await
            .unwrap()
            .is_none()
    );

    // Reverse arXiv crosswalk: stored with arxiv ID only (no DOI), looked up via 10.48550/arXiv.* DOI.
    let mut arxiv_only = ScholarlyWork::new("Arxiv Only", "https://arxiv.org/abs/2401.00001");
    arxiv_only.ids.arxiv = Some("2401.00001".into());
    store
        .scholar_put("arxiv:2401.00001", &arxiv_only, 3600)
        .await
        .unwrap();
    assert_eq!(
        store
            .scholar_get("10.48550/arXiv.2401.00001")
            .await
            .unwrap(),
        Some(arxiv_only)
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn test_exec_kv_lru_evicts_across_mixed_m7_sessions() {
    let dir = TempDir::new().unwrap();
    let mut cfg = StoreConfig::at_path(dir.path().join(DB_FILENAME));
    cfg.exec_kv_lru_max_bytes = 200;
    let store = SqliteStore::open_with(cfg).unwrap();

    let payload = |c: char| json!(std::iter::repeat_n(c, 80).collect::<String>());
    // Mode 1 (overflow ""), Mode 2 ("shared"), Mode 3 (session "s1").
    store
        .kv_put(
            EXEC_KV_OVERFLOW_SESSION,
            "exec:old12345",
            &payload('a'),
            3600,
        )
        .await
        .unwrap();
    store
        .kv_put(EXEC_KV_SHARED_SESSION, "shared_k", &payload('b'), 3600)
        .await
        .unwrap();
    // Third write crosses 200 B cap -> evicts oldest (`exec:old12345`).
    store
        .kv_put("s1", "priv_k", &payload('c'), 3600)
        .await
        .unwrap();

    assert!(store.kv_get("s1", "exec:old12345").await.unwrap().is_none());
    assert!(store.kv_get("s1", "shared_k").await.unwrap().is_some());
    assert!(store.kv_get("s1", "priv_k").await.unwrap().is_some());
    store.close().await.unwrap();
}

/// Missing test #6 (M7 namespace isolation): a non-`exec:` key stored under `""`
/// must NEVER leak into other sessions or `"shared"`.
#[tokio::test]
async fn test_exec_kv_m7_non_exec_key_in_empty_session_never_leaks_to_other_sessions() {
    let dir = TempDir::new().unwrap();
    let store = open_temp_store(&dir);

    store
        .kv_put("", "foo", &json!({"owner": "empty"}), 3600)
        .await
        .unwrap();
    assert_eq!(store.kv_get("sess", "foo").await.unwrap(), None);
    assert_eq!(
        store.kv_get(EXEC_KV_SHARED_SESSION, "foo").await.unwrap(),
        None
    );
    assert_eq!(
        store.kv_get("", "foo").await.unwrap(),
        Some(json!({"owner": "empty"}))
    );
    store.close().await.unwrap();
}

/// I2 regression / Missing test #4: partial 2–4 MiB overflow writes the first 2 MiB JSON prefix
/// (containing quotes whose escaped JSON serialization exceeds 2 MiB) as `Value::String` under
/// `("", "exec:<id>")`, and `kv_get` round-trips the exact string.
#[tokio::test]
async fn test_exec_kv_overflow_2mib_partial_json_prefix_round_trips() {
    let dir = TempDir::new().unwrap();
    let store = open_temp_store(&dir);

    // Build a realistic > 2 MiB JSON array full of quotes, then cut to exact 2 MiB prefix
    // (mirroring `crates/script/src/reply.rs::save_overflow`).
    let mut full_json = String::with_capacity(2_500_000);
    full_json.push('[');
    for i in 0..55_000 {
        if i > 0 {
            full_json.push(',');
        }
        full_json.push_str(&format!(
            r#"{{"id":{i},"title":"quote \"test\" entry","k":"v"}}"#
        ));
    }
    full_json.push(']');
    assert!(full_json.len() > EXEC_KV_MAX_VAL_BYTES);

    let mut cut = EXEC_KV_MAX_VAL_BYTES;
    while !full_json.is_char_boundary(cut) {
        cut -= 1;
    }
    let prefix = full_json[..cut].to_owned();
    let partial_val = serde_json::Value::String(prefix.clone());
    let serialized_len = serde_json::to_vec(&partial_val).unwrap().len();
    assert!(
        serialized_len > EXEC_KV_MAX_VAL_BYTES,
        "test premise: escaped JSON string must exceed 2 MiB ({serialized_len} bytes)"
    );

    // Overflow handle accepts the 2 MiB logical prefix and round-trips losslessly.
    store
        .kv_put(
            EXEC_KV_OVERFLOW_SESSION,
            "exec:part1234",
            &partial_val,
            86_400,
        )
        .await
        .unwrap();
    assert_eq!(
        store.kv_get("any-session", "exec:part1234").await.unwrap(),
        Some(partial_val.clone())
    );

    // Logical overflow prefix > 2 MiB is STILL rejected.
    let too_long_prefix = serde_json::Value::String("a".repeat(EXEC_KV_MAX_VAL_BYTES + 1));
    assert!(matches!(
        store
            .kv_put(
                EXEC_KV_OVERFLOW_SESSION,
                "exec:toobig01",
                &too_long_prefix,
                86_400,
            )
            .await,
        Err(FsmError::InvalidInput(_))
    ));

    // User keys STILL enforce the strict 2 MiB serialized cap.
    assert!(matches!(
        store
            .kv_put("sess", "user_partial", &partial_val, 3600)
            .await,
        Err(FsmError::InvalidInput(_))
    ));

    store.close().await.unwrap();
}

/// B1 regression / Missing test #1: CJK, Latin accented, and full-width punctuation inputs
/// must never panic inside PID normalizers or kill the store actor.
#[tokio::test]
async fn test_scholar_and_retractions_handle_non_ascii_and_cjk_without_panic() {
    let dir = TempDir::new().unwrap();
    let store = open_temp_store(&dir);

    for query in [
        "深度学习",
        "深度学习的可解释性",
        "注意力机制",
        "Café society",
        "DOI：10.1000/x",
        "10.1000/abcdef日本",
        "PMID：12345678",
        "abcd日",
        "abcdé",
        "abcdefghé",
    ] {
        assert_eq!(
            store.scholar_get(query).await.unwrap(),
            None,
            "scholar_get({query:?}) should cleanly miss"
        );
        assert_eq!(
            store.retraction_lookup(query).await.unwrap(),
            None,
            "retraction_lookup({query:?}) should cleanly miss"
        );
    }

    // `scholar_put` with full-width CJK DOI/arXiv/PMID/OpenAlex metadata succeeds without panic.
    let mut work = ScholarlyWork::new("深度学习的可解释性", "https://example.org/cjk");
    work.ids.doi = Some("DOI：10.1000/xyz".into());
    work.ids.arxiv = Some("abcd日".into());
    work.ids.pmid = Some("abcdé".into());
    work.ids.openalex = Some("abcdefghé".into());
    store.scholar_put("cjk:1", &work, 3600).await.unwrap();
    assert_eq!(store.scholar_get("cjk:1").await.unwrap(), Some(work));

    // `retractions_replace_batch` with multi-byte DOIs commits cleanly.
    let notice = RetractionNotice::new("Retraction", "retraction_watch");
    store
        .retractions_replace_batch(
            vec![
                ("10.1000/abcdef日本".into(), notice.clone()),
                ("DOI：10.1000/xyz".into(), notice.clone()),
            ],
            Some("\"cjk-etag\"".into()),
        )
        .await
        .unwrap();
    assert_eq!(
        store.retraction_lookup("10.1000/abcdef日本").await.unwrap(),
        Some(notice)
    );

    // Follow-up unrelated call proves the actor remained alive throughout.
    assert!(
        store
            .get_page("https://example.org/alive", None)
            .await
            .unwrap()
            .is_none()
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn test_retractions_large_atomic_batch_is_fast() {
    let dir = TempDir::new().unwrap();
    let store = open_temp_store(&dir);

    let batch: Vec<(String, RetractionNotice)> = (0..5_000)
        .map(|i| {
            let mut n = RetractionNotice::new("Retraction", "retraction_watch");
            n.date = Some("2025-01-01".into());
            n.reason = Some("Plagiarism".into());
            (format!("10.9999/paper.{i}"), n)
        })
        .collect();

    store
        .retractions_replace_batch(batch, Some("\"bulk-etag\"".into()))
        .await
        .unwrap();

    let hit = store
        .retraction_lookup("10.9999/PAPER.4999")
        .await
        .unwrap()
        .expect("5000th row must be indexed");
    assert_eq!(hit.nature, "Retraction");
    assert_eq!(hit.reason.as_deref(), Some("Plagiarism"));
    store.close().await.unwrap();
}

#[tokio::test]
async fn test_retractions_batch_replace_and_sync_state() {
    let dir = TempDir::new().unwrap();
    let store = open_temp_store(&dir);

    assert!(
        store
            .sync_state_get("retraction_watch")
            .await
            .unwrap()
            .is_none()
    );
    store
        .sync_state_mark("retraction_watch", None, 1000, None)
        .await
        .unwrap();
    assert_eq!(
        store.sync_state_get("retraction_watch").await.unwrap(),
        Some((None, 1000, None))
    );

    let mut n1 = RetractionNotice::new("Retraction", "retraction_watch");
    n1.date = Some("2025-06-01".into());
    n1.reason = Some("Data fabrication".into());
    n1.notice_doi = Some("10.1000/retract1".into());

    let mut n2 = RetractionNotice::new("Expression of Concern", "retraction_watch");
    n2.date = Some("2024-01-10".into());

    // Include a duplicate DOI in mixed case + URL prefix to verify deduplicated upsert.
    store
        .retractions_replace_batch(
            vec![
                ("10.1038/OLD-PAPER".into(), n2.clone()),
                ("https://doi.org/10.1038/BAD-PAPER".into(), n2),
                ("10.1038/bad-paper".into(), n1.clone()),
            ],
            Some("\"rw-etag-v1\"".into()),
        )
        .await
        .unwrap();

    // Lookup normalizes input DOI.
    let found = store
        .retraction_lookup("https://doi.org/10.1038/BAD-PAPER.")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found, n1);

    // `sync_state` recorded ETag + finished_at atomically while keeping started_at = 1000.
    let (etag, started_at, finished_at) = store
        .sync_state_get("retraction_watch")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(etag.as_deref(), Some("\"rw-etag-v1\""));
    assert_eq!(started_at, 1000);
    assert!(finished_at.is_some());

    // Second batch replaces previous rows completely.
    let n3 = RetractionNotice::new("Retraction", "retraction_watch");
    store
        .retractions_replace_batch(
            vec![("10.1000/new-only".into(), n3.clone())],
            Some("\"rw-etag-v2\"".into()),
        )
        .await
        .unwrap();
    assert!(
        store
            .retraction_lookup("10.1038/old-paper")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store.retraction_lookup("10.1000/new-only").await.unwrap(),
        Some(n3)
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn test_health_state_round_trip_and_survives_maintenance() {
    let dir = TempDir::new().unwrap();
    let store = open_temp_store(&dir);

    let mut h1 = HealthRecord::new("duckduckgo", "open", 1_700_000_000);
    h1.failures = 3;
    h1.benched_until = Some(1_700_000_600);
    h1.last_error = Some("HTTP 429".into());

    let h2 = HealthRecord::new("arxiv", "closed", 1_700_000_001);

    store.health_save(&h1).await.unwrap();
    store.health_save(&h2).await.unwrap();

    // Update existing record.
    h1.failures = 4;
    h1.benched_until = Some(1_700_001_200);
    store.health_save(&h1).await.unwrap();

    store.maintain_now().await.unwrap();

    let all = store.health_load_all().await.unwrap();
    assert_eq!(all, vec![h2, h1]); // sorted by source_id
    store.close().await.unwrap();
}

// ===========================================================================
// Cross-process `fd-lock` tests (same-process & real two-process OS test)
// ===========================================================================

#[test]
fn test_file_lock_and_lease_exclusion_in_process() {
    let dir = TempDir::new().unwrap();
    let lock_path = dir.path().join("test.lock");

    let lease = FileLease::try_acquire(&lock_path)
        .unwrap()
        .expect("first acquire must succeed");

    // Second descriptor cannot acquire non-blocking while lease is held.
    let mut second = FileLock::open(&lock_path).unwrap();
    assert!(second.try_write().unwrap().is_none());
    assert!(FileLease::try_acquire(&lock_path).unwrap().is_none());

    // After explicit release, acquisition succeeds immediately.
    lease.release();
    assert!(second.try_write().unwrap().is_some());

    // Convenience helper creates `<cache_dir>/rw-sync.lock`.
    let rw = FileLease::try_acquire_rw_sync(dir.path())
        .unwrap()
        .expect("rw-sync acquire");
    assert_eq!(rw.path(), dir.path().join("rw-sync.lock"));
    assert!(rw.path().exists());
    drop(rw);
}

/// Child entry point invoked by `test_cross_process_fd_lock_blocks_other_process`.
/// Runs as a fast no-op when `FSM_STORE_LOCK_CHILD_PATH` is unset.
#[test]
fn lock_child_helper() {
    let Ok(path) = std::env::var("FSM_STORE_LOCK_CHILD_PATH") else {
        return;
    };
    let expect = std::env::var("FSM_STORE_LOCK_EXPECT").unwrap_or_default();
    let acquired = FileLease::try_acquire(&path).expect("child try_acquire");
    match expect.as_str() {
        "busy" => assert!(
            acquired.is_none(),
            "child unexpectedly acquired lock held by parent process"
        ),
        "acquired" => assert!(
            acquired.is_some(),
            "child failed to acquire lock after parent released it"
        ),
        other => panic!("unexpected FSM_STORE_LOCK_EXPECT={other:?}"),
    }
}

#[test]
fn test_cross_process_fd_lock_blocks_other_process() {
    let dir = TempDir::new().unwrap();
    let lock_path = dir.path().join("rw-sync.lock");
    let exe = std::env::current_exe().expect("current test binary");

    let lease = FileLease::try_acquire(&lock_path)
        .unwrap()
        .expect("parent acquires lock");

    // Child OS process MUST observe the lock as busy (WouldBlock -> None).
    let status_busy = Command::new(&exe)
        .args(["--exact", "lock_child_helper", "--nocapture"])
        .env("FSM_STORE_LOCK_CHILD_PATH", &lock_path)
        .env("FSM_STORE_LOCK_EXPECT", "busy")
        .status()
        .expect("spawn child (busy)");
    assert!(
        status_busy.success(),
        "child busy check failed: {status_busy}"
    );

    // Release in parent, then child OS process MUST acquire cleanly.
    lease.release();
    let status_free = Command::new(&exe)
        .args(["--exact", "lock_child_helper", "--nocapture"])
        .env("FSM_STORE_LOCK_CHILD_PATH", &lock_path)
        .env("FSM_STORE_LOCK_EXPECT", "acquired")
        .status()
        .expect("spawn child (acquired)");
    assert!(
        status_free.success(),
        "child acquired check failed: {status_free}"
    );
}

// ===========================================================================
// Property tests: key regex bijection with `regex` crate
// ===========================================================================

proptest! {
    #[test]
    fn prop_kv_key_validator_matches_spec_regex(s in "\\PC{0,140}") {
        let re = Regex::new(r"^[a-zA-Z0-9._:-]{1,128}$").unwrap();
        prop_assert_eq!(validate_kv_key(&s).is_ok(), re.is_match(&s));
        prop_assert_eq!(
            validate_user_kv_key(&s).is_ok(),
            re.is_match(&s) && !s.starts_with("exec:")
        );
    }
}
