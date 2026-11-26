//! Port of Python `tests/test_downloads.py` plus 1.0 `download_dir_max_mb`
//! and SQLite `downloads` table tests.

use fsm_core::{Config, DownloadResult, FsmError, StoreApi};
use fsm_store::downloads;
use fsm_store::{DB_FILENAME, SqliteStore};
use std::fs;
use std::time::{Duration, SystemTime};
use tempfile::TempDir;

fn enabled_cfg(dir: &TempDir) -> Config {
    let root = dir.path().join("dl");
    fs::create_dir_all(&root).unwrap();
    let mut cfg = Config::defaults_for_home(dir.path());
    cfg.cache_dir = dir.path().join("cache");
    cfg.download_enabled = true;
    cfg.download_dir = root;
    cfg
}

// ---------------------------------------------------------------------------
// Default policy and operator overrides
// ---------------------------------------------------------------------------

#[test]
fn test_enabled_by_default() {
    let dir = TempDir::new().unwrap();
    let mut cfg = Config::defaults_for_home(dir.path());
    let cache_dir = dir.path().join("cache");
    cfg.cache_dir = cache_dir.clone();
    cfg.download_dir = downloads::default_download_dir(&cfg);
    cfg.download_enabled = true;

    assert_eq!(
        downloads::require_download_dir(&cfg).unwrap(),
        cache_dir.join("downloads")
    );
}

#[test]
fn test_default_directory_tracks_active_cache_root() {
    // Missing test #9: exercise `downloads::download_dir` on a loader-built `Config`
    // where ONLY `SEARCH_MCP_CACHE_DIR` is overridden (and `SEARCH_MCP_DOWNLOAD_DIR` is unset).
    let dir = TempDir::new().unwrap();

    let first_cache = dir.path().join("first");
    let mut src1 = fsm_core::config::EnvSources::new(dir.path());
    src1.process.insert(
        "SEARCH_MCP_CACHE_DIR".into(),
        first_cache.to_string_lossy().into_owned(),
    );
    let cfg1 = Config::from_sources(&src1).unwrap();
    assert_eq!(
        downloads::download_dir(&cfg1),
        Some(first_cache.join("downloads"))
    );

    let second_cache = dir.path().join("second");
    let mut src2 = fsm_core::config::EnvSources::new(dir.path());
    src2.process.insert(
        "SEARCH_MCP_CACHE_DIR".into(),
        second_cache.to_string_lossy().into_owned(),
    );
    let cfg2 = Config::from_sources(&src2).unwrap();
    assert_eq!(
        downloads::download_dir(&cfg2),
        Some(second_cache.join("downloads"))
    );
}

#[test]
fn test_resolving_default_directory_does_not_create_it() {
    let dir = TempDir::new().unwrap();
    let mut cfg = Config::defaults_for_home(dir.path());
    cfg.cache_dir = dir.path().join("cache");
    cfg.download_dir = cfg.cache_dir.join("downloads");
    cfg.download_enabled = true;

    let root = downloads::download_dir(&cfg).unwrap();
    assert_eq!(root, cfg.cache_dir.join("downloads"));
    assert!(!root.exists());

    let saved = downloads::save(&cfg, "https://x.example/a.bin", b"data", "").unwrap();
    assert_eq!(
        saved.path.parent().unwrap(),
        fs::canonicalize(&root).unwrap()
    );
}

#[test]
fn test_configured_directory_overrides_default() {
    let dir = TempDir::new().unwrap();
    let custom = dir.path().join("configured");
    let mut cfg = Config::defaults_for_home(dir.path());
    cfg.download_enabled = true;
    cfg.download_dir = custom.clone();

    assert_eq!(downloads::require_download_dir(&cfg).unwrap(), custom);
}

#[test]
fn test_explicit_disable_overrides_custom_directory() {
    let dir = TempDir::new().unwrap();
    let custom = dir.path().join("configured");
    let mut cfg = Config::defaults_for_home(dir.path());
    cfg.download_enabled = false;
    cfg.download_dir = custom.clone();

    assert!(downloads::download_dir(&cfg).is_none());
    let err = downloads::require_download_dir(&cfg).unwrap_err();
    assert!(matches!(err, FsmError::PermissionDenied(_)));
    assert!(err.to_string().contains("DOWNLOAD_ENABLED=false"));

    let err = downloads::save(&cfg, "https://x.example/a.bin", b"data", "").unwrap_err();
    assert!(matches!(err, FsmError::PermissionDenied(_)));
    assert!(err.to_string().contains("DOWNLOAD_ENABLED=false"));
    assert!(!custom.exists());
}

// ---------------------------------------------------------------------------
// Filename safety — remote controls the URL
// ---------------------------------------------------------------------------

#[test]
fn test_traversal_attempts_collapse_to_one_component() {
    for url in [
        "https://x.example/../../etc/passwd",
        "https://x.example/%2e%2e%2f%2e%2e%2fetc%2fpasswd",
        "https://x.example/a/b/../../../../root/.ssh/id_rsa",
        "https://x.example/..%5c..%5cwindows%5csystem32",
        "https://x.example/foo..bar",
    ] {
        let name = downloads::safe_filename(url, "", b"");
        assert!(!name.contains('/'), "{url} -> {name}");
        assert!(!name.contains('\\'), "{url} -> {name}");
        assert!(!name.contains(".."), "{url} -> {name}");
    }
}

#[test]
fn test_saved_path_stays_inside_the_download_dir() {
    let dir = TempDir::new().unwrap();
    let cfg = enabled_cfg(&dir);
    let saved = downloads::save(&cfg, "https://x.example/../../escape.txt", b"x", "").unwrap();
    assert_eq!(
        saved.path.parent().unwrap(),
        fs::canonicalize(&cfg.download_dir).unwrap()
    );
}

#[test]
fn test_filename_is_content_addressed_so_collisions_cannot_overwrite() {
    let dir = TempDir::new().unwrap();
    let cfg = enabled_cfg(&dir);
    let first = downloads::save(&cfg, "https://a.example/report.pdf", b"one", "").unwrap();
    let second = downloads::save(&cfg, "https://b.example/report.pdf", b"two", "").unwrap();
    assert_ne!(first.path, second.path);
    assert_eq!(fs::read(&first.path).unwrap(), b"one");
    assert_eq!(fs::read(&second.path).unwrap(), b"two");
}

#[test]
fn test_identical_bytes_reuse_one_file() {
    let dir = TempDir::new().unwrap();
    let cfg = enabled_cfg(&dir);
    let a = downloads::save(&cfg, "https://a.example/x.bin", b"same", "").unwrap();
    let b = downloads::save(&cfg, "https://a.example/x.bin", b"same", "").unwrap();
    assert_eq!(a.path, b.path);
    assert_eq!(a.sha256, b.sha256);
}

#[test]
fn test_long_names_are_capped() {
    let dir = TempDir::new().unwrap();
    let cfg = enabled_cfg(&dir);
    let url = format!("https://x.example/{}.bin", "n".repeat(500));
    let saved = downloads::save(&cfg, &url, b"x", "").unwrap();
    let fname = saved.path.file_name().unwrap().to_string_lossy();
    assert!(fname.len() < 120, "len = {}", fname.len());
    assert!(fname.ends_with(".bin"));
}

#[test]
fn test_extension_is_inferred_from_media_type_when_missing() {
    let name = downloads::safe_filename("https://x.example/download", "image/png", b"x");
    assert!(name.ends_with(".png"), "{name}");
    let name2 = downloads::safe_filename(
        "https://x.example/dl",
        "application/pdf; charset=utf-8",
        b"x",
    );
    assert!(name2.ends_with(".pdf"), "{name2}");
}

#[test]
fn test_nameless_url_still_produces_a_file() {
    let dir = TempDir::new().unwrap();
    let cfg = enabled_cfg(&dir);
    let saved = downloads::save(&cfg, "https://x.example/", b"x", "").unwrap();
    assert!(saved.path.is_file());
}

// ---------------------------------------------------------------------------
// Size cap & directory cap
// ---------------------------------------------------------------------------

#[test]
fn test_oversized_download_is_refused_and_writes_nothing() {
    let dir = TempDir::new().unwrap();
    let mut cfg = enabled_cfg(&dir);
    cfg.download_max_mb = 1;
    let big = vec![b'x'; 2 * 1024 * 1024];

    let err = downloads::save(&cfg, "https://x.example/big.bin", &big, "").unwrap_err();
    assert!(matches!(err, FsmError::InvalidInput(_)));
    assert!(err.to_string().contains("over the 1 MB"));
    assert_eq!(fs::read_dir(&cfg.download_dir).unwrap().count(), 0);
}

#[test]
fn test_download_dir_max_mb_evicts_oldest_files() {
    let dir = TempDir::new().unwrap();
    let mut cfg = enabled_cfg(&dir);
    cfg.download_max_mb = 2;
    cfg.download_dir_max_mb = 1; // 1 MiB directory cap

    // Write 600 KiB file #1, wait briefly for distinct mtime, then write 600 KiB file #2.
    let first =
        downloads::save(&cfg, "https://x.example/f1.bin", &vec![b'a'; 600_000], "").unwrap();
    std::thread::sleep(Duration::from_millis(20));
    let second =
        downloads::save(&cfg, "https://x.example/f2.bin", &vec![b'b'; 600_000], "").unwrap();

    // Directory cap (1 MiB) evicted file #1 so file #2 fits.
    assert!(!first.path.exists());
    assert!(second.path.exists());
}

// ---------------------------------------------------------------------------
// Expiry
// ---------------------------------------------------------------------------

#[test]
fn test_expired_files_are_purged() {
    let dir = TempDir::new().unwrap();
    let mut cfg = enabled_cfg(&dir);
    cfg.download_ttl_hours = 24;
    let saved = downloads::save(&cfg, "https://x.example/old.bin", b"x", "").unwrap();

    let future = SystemTime::now() + Duration::from_secs(25 * 3600);
    assert_eq!(downloads::purge_expired(&cfg, Some(future)), 1);
    assert!(!saved.path.exists());
}

#[test]
fn test_fresh_files_survive_the_purge() {
    let dir = TempDir::new().unwrap();
    let mut cfg = enabled_cfg(&dir);
    cfg.download_ttl_hours = 24;
    let saved = downloads::save(&cfg, "https://x.example/new.bin", b"x", "").unwrap();

    assert_eq!(downloads::purge_expired(&cfg, None), 0);
    assert!(saved.path.exists());
}

#[test]
fn test_ttl_zero_disables_expiry() {
    let dir = TempDir::new().unwrap();
    let mut cfg = enabled_cfg(&dir);
    cfg.download_ttl_hours = 0;
    let saved = downloads::save(&cfg, "https://x.example/keep.bin", b"x", "").unwrap();

    let far_future = SystemTime::now() + Duration::from_secs(999 * 3600);
    assert_eq!(downloads::purge_expired(&cfg, Some(far_future)), 0);
    assert!(saved.path.exists());
}

#[test]
fn test_purge_is_a_noop_when_downloads_are_disabled() {
    let dir = TempDir::new().unwrap();
    let mut cfg = enabled_cfg(&dir);
    cfg.download_enabled = false;
    assert_eq!(downloads::purge_expired(&cfg, None), 0);
}

#[test]
fn test_purge_is_a_noop_when_directory_is_missing() {
    let dir = TempDir::new().unwrap();
    let mut cfg = Config::defaults_for_home(dir.path());
    cfg.download_enabled = true;
    cfg.download_dir = dir.path().join("missing");
    assert_eq!(downloads::purge_expired(&cfg, None), 0);
}

#[test]
fn test_purge_ignores_subdirectories() {
    let dir = TempDir::new().unwrap();
    let mut cfg = enabled_cfg(&dir);
    cfg.download_ttl_hours = 1;
    let sub = cfg.download_dir.join("nested");
    fs::create_dir(&sub).unwrap();

    let far_future = SystemTime::now() + Duration::from_secs(999 * 3600);
    assert_eq!(downloads::purge_expired(&cfg, Some(far_future)), 0);
    assert!(sub.exists());
}

// ---------------------------------------------------------------------------
// SQLite `downloads` table recording & TTL=0 preservation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_download_record_sqlite_persistence_and_ttl_zero() {
    let dir = TempDir::new().unwrap();
    let store = SqliteStore::open_path(dir.path().join(DB_FILENAME)).unwrap();

    let mut rec_ephemeral = DownloadResult::new(
        "https://x.example/temp.pdf",
        "/tmp/dl/aaa-temp.pdf",
        "application/pdf",
        1024,
        "aaaa",
    );
    rec_ephemeral.expires_in_hours = 24;
    store.download_record(&rec_ephemeral, 3600).await.unwrap();

    let rec_permanent = DownloadResult::new(
        "https://x.example/perm.pdf",
        "/tmp/dl/bbb-perm.pdf",
        "application/pdf",
        2048,
        "bbbb",
    );
    // ttl_s = 0 disables expiry (expires_at = 0).
    store.download_record(&rec_permanent, 0).await.unwrap();

    assert_eq!(store.downloads_list_active().await.unwrap().len(), 2);

    // Expire the ephemeral row in SQLite.
    {
        let conn = rusqlite::Connection::open(store.path()).unwrap();
        conn.execute(
            "UPDATE downloads SET expires_at = 1 WHERE sha256 = 'aaaa'",
            [],
        )
        .unwrap();
    }

    let purged = store.downloads_purge_expired().await.unwrap();
    assert_eq!(purged, 1);

    let active = store.downloads_list_active().await.unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].sha256, "bbbb");
    assert_eq!(active[0].expires_at, 0);
    store.close().await.unwrap();
}

#[cfg(unix)]
#[test]
fn test_save_refuses_to_follow_existing_symlink_target() {
    use std::os::unix::fs::symlink;

    let dir = TempDir::new().unwrap();
    let cfg = enabled_cfg(&dir);
    let outside = dir.path().join("outside.txt");
    fs::write(&outside, b"original").unwrap();

    let blob = b"payload";
    let fname = downloads::safe_filename("https://x.example/file.bin", "", blob);
    let link_path = cfg.download_dir.join(fname);
    symlink(&outside, &link_path).unwrap();

    let err = downloads::save(&cfg, "https://x.example/file.bin", blob, "").unwrap_err();
    assert!(matches!(err, FsmError::PermissionDenied(_)), "{err}");
    assert_eq!(fs::read(&outside).unwrap(), b"original");
}
