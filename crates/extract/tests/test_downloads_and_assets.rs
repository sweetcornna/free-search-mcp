//! Port of `tests/test_downloads.py` and `tests/test_assets.py` + Rust 1.0 download caps.

mod common;

use common::{MemStore, MockDocs, MockNet, MockSearch, make_png, make_service};
use fsm_core::config::Config;
use fsm_core::{CallCx, FetchApi, FetchMethod, FetchOpts, FetchOutcome, FsmError, HttpBytes};
use fsm_extract::downloads::{purge_expired, require_download_dir, safe_filename, save_bytes};
use fsm_extract::fetch::{image_dimensions, is_asset_ctype, is_asset_url};
use std::fs;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Downloads (port of tests/test_downloads.py + Rust 1.0 caps)
// ---------------------------------------------------------------------------

#[test]
fn test_traversal_attempts_collapse_to_one_component() {
    for url in [
        "https://x.example/../../etc/passwd",
        "https://x.example/%2e%2e%2f%2e%2e%2fetc%2fpasswd",
        "https://x.example/a/b/../../../../root/.ssh/id_rsa",
        "https://x.example/..%5c..%5cwindows%5csystem32",
    ] {
        let name = safe_filename(url, "", b"");
        assert!(!name.contains('/'), "{name}");
        assert!(!name.contains('\\'), "{name}");
        assert!(!name.contains(".."), "{name}");
    }
}

#[test]
fn test_saved_path_content_addressed_and_reused() {
    let dir = TempDir::new().unwrap();
    let mut cfg = Config::default();
    cfg.download_enabled = true;
    cfg.download_dir = dir.path().join("dl");

    // Traversal stays inside download dir.
    let escaped = save_bytes(&cfg, "https://x.example/../../escape.txt", b"x", "").unwrap();
    assert_eq!(
        escaped.parent().unwrap(),
        fs::canonicalize(&cfg.download_dir).unwrap()
    );

    // Different bytes with same filename cannot collide.
    let first = save_bytes(
        &cfg,
        "https://a.example/report.pdf",
        b"one",
        "application/pdf",
    )
    .unwrap();
    let second = save_bytes(
        &cfg,
        "https://b.example/report.pdf",
        b"two",
        "application/pdf",
    )
    .unwrap();
    assert_ne!(first, second);
    assert_eq!(fs::read(&first).unwrap(), b"one");
    assert_eq!(fs::read(&second).unwrap(), b"two");

    // Identical bytes reuse one file.
    let a = save_bytes(&cfg, "https://a.example/x.bin", b"same", "").unwrap();
    let b = save_bytes(&cfg, "https://a.example/x.bin", b"same", "").unwrap();
    assert_eq!(a, b);

    // Long names capped.
    let long_url = format!("https://x.example/{}.bin", "n".repeat(500));
    let long_path = save_bytes(&cfg, &long_url, b"x", "").unwrap();
    assert!(long_path.file_name().unwrap().to_string_lossy().len() < 120);

    // Extension inferred from media type when missing.
    let png_name = safe_filename("https://x.example/download", "image/png", b"x");
    assert!(png_name.ends_with(".png"), "{png_name}");

    // Nameless URL still produces a file.
    let nameless = save_bytes(&cfg, "https://x.example/", b"x", "").unwrap();
    assert!(nameless.is_file());
}

#[test]
fn test_explicit_disable_and_oversized_refused_without_writing() {
    let dir = TempDir::new().unwrap();
    let mut cfg = Config::default();
    cfg.download_enabled = false;
    cfg.download_dir = dir.path().join("disabled_dl");

    let err = require_download_dir(&cfg).unwrap_err();
    assert!(matches!(err, FsmError::PermissionDenied(_)));
    assert!(err.to_string().contains("DOWNLOAD_ENABLED=false"));
    assert!(save_bytes(&cfg, "https://x.example/a.bin", b"data", "").is_err());
    assert!(!cfg.download_dir.exists());

    // Size cap: 1 MB limit refuses 2 MB payload and writes nothing.
    let dl_dir = dir.path().join("capped_dl");
    fs::create_dir_all(&dl_dir).unwrap();
    cfg.download_enabled = true;
    cfg.download_dir = dl_dir.clone();
    cfg.download_max_mb = 1;
    let big = vec![b'x'; 2 * 1024 * 1024];
    let err = save_bytes(&cfg, "https://x.example/big.bin", &big, "").unwrap_err();
    assert!(matches!(err, FsmError::InvalidInput(_)));
    assert!(err.to_string().contains("over the 1 MB"), "{err}");
    assert_eq!(fs::read_dir(&dl_dir).unwrap().count(), 0);
}

#[test]
fn test_download_dir_max_mb_cap_refuses_write() {
    let dir = TempDir::new().unwrap();
    let mut cfg = Config::default();
    cfg.download_enabled = true;
    cfg.download_dir = dir.path().join("quota_dl");
    cfg.download_max_mb = 10;
    cfg.download_dir_max_mb = 1; // 1 MB total dir cap

    let chunk = vec![b'a'; 700 * 1024]; // 700 KB fits
    save_bytes(&cfg, "https://x.example/first.bin", &chunk, "").unwrap();

    let second = vec![b'b'; 700 * 1024]; // 700 KB + 700 KB > 1 MB
    let err = save_bytes(&cfg, "https://x.example/second.bin", &second, "").unwrap_err();
    assert!(matches!(err, FsmError::BudgetExceeded(_)), "{err:?}");
    assert!(err.to_string().contains("SEARCH_MCP_DOWNLOAD_DIR_MAX_MB"));
    assert_eq!(fs::read_dir(&cfg.download_dir).unwrap().count(), 1);
}

#[test]
fn test_purge_expired_handles_ttl_and_subdirectories() {
    let dir = TempDir::new().unwrap();
    let mut cfg = Config::default();
    cfg.download_enabled = true;
    cfg.download_dir = dir.path().join("purge_dl");
    cfg.download_ttl_hours = 24;

    // Missing directory is a noop.
    assert_eq!(purge_expired(&cfg, None), 0);

    let file = save_bytes(&cfg, "https://x.example/a.bin", b"hello", "").unwrap();
    let sub = cfg.download_dir.join("nested");
    fs::create_dir(&sub).unwrap();

    // Fresh file survives.
    assert_eq!(purge_expired(&cfg, Some(SystemTime::now())), 0);
    assert!(file.exists());

    // Simulate 25 hours in the future -> file purged, subdir ignored.
    let future = SystemTime::now() + Duration::from_secs(25 * 3600);
    assert_eq!(purge_expired(&cfg, Some(future)), 1);
    assert!(!file.exists());
    assert!(sub.exists());

    // TTL = 0 disables expiry.
    let keep = save_bytes(&cfg, "https://x.example/keep.bin", b"stay", "").unwrap();
    cfg.download_ttl_hours = 0;
    assert_eq!(purge_expired(&cfg, Some(future)), 0);
    assert!(keep.exists());

    // Disabled downloads -> noop.
    cfg.download_enabled = false;
    assert_eq!(purge_expired(&cfg, Some(future)), 0);
}

#[tokio::test]
async fn test_download_explicit_disable_rejects_before_network() {
    let mut cfg = Config::default();
    cfg.download_enabled = false;
    let net = Arc::new(MockNet::new());
    let svc = make_service(
        net.clone(),
        Arc::new(MemStore::new()),
        cfg,
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );
    let err = svc
        .download("https://x.example/a.bin", &CallCx::default())
        .await
        .unwrap_err();
    assert!(matches!(err, FsmError::PermissionDenied(_)));
    assert!(net.recorded().is_empty());
}

#[tokio::test]
async fn test_download_max_five_per_exec_and_records_in_store() {
    let dir = TempDir::new().unwrap();
    let mut cfg = Config::default();
    cfg.download_enabled = true;
    cfg.download_dir = dir.path().join("dl");

    let png = make_png(16, 8);
    let net = Arc::new(MockNet::with_handler(move |req, _, _| {
        Ok(HttpBytes::new(
            req.url.clone(),
            200,
            vec![("content-type".into(), "image/png".into())],
            png.clone(),
        ))
    }));
    let store = Arc::new(MemStore::new());
    let svc = make_service(
        net.clone(),
        store.clone(),
        cfg,
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );

    let cx = CallCx::default();
    for i in 0..5 {
        let res = svc
            .download(&format!("https://x.example/img-{i}.png"), &cx)
            .await
            .unwrap();
        assert_eq!(res.media_type, "image/png");
        assert_eq!((res.width, res.height), (Some(16), Some(8)));
        assert!(std::path::Path::new(&res.saved_path).is_file());
    }
    assert_eq!(store.downloads.lock().unwrap().len(), 5);

    // 6th download in the same exec hits BudgetExceeded BEFORE touching the network!
    let reqs_before = net.recorded().len();
    let err = svc
        .download("https://x.example/img-6.png", &cx)
        .await
        .unwrap_err();
    assert!(matches!(err, FsmError::BudgetExceeded(_)), "{err:?}");
    assert_eq!(net.recorded().len(), reqs_before);
}

// ---------------------------------------------------------------------------
// Assets / Images (port of tests/test_assets.py)
// ---------------------------------------------------------------------------

#[test]
fn test_asset_content_types_and_urls() {
    for (ct, expected) in [
        ("image/png", true),
        ("image/jpeg", true),
        ("video/mp4", true),
        ("audio/mpeg", true),
        ("font/woff2", true),
        ("application/octet-stream", true),
        ("application/zip", true),
        ("text/html", false),
        ("application/json", false),
        ("text/plain", false),
        ("", false),
        ("image/svg+xml", false),
    ] {
        assert_eq!(is_asset_ctype(ct), expected, "{ct}");
    }

    for (u, expected) in [
        ("https://x.example/a.png", true),
        ("https://x.example/a.JPG", true),
        ("https://x.example/a.webp", true),
        ("https://x.example/a.svg", false),
        ("https://x.example/a.html", false),
        ("https://x.example/page", false),
    ] {
        assert_eq!(is_asset_url(u), expected, "{u}");
    }
}

#[test]
fn test_image_dimensions_header_and_non_image() {
    assert_eq!(image_dimensions(&make_png(12, 7)), (Some(12), Some(7)));
    assert_eq!(image_dimensions(b"not an image"), (None, None));
}

#[tokio::test]
async fn test_image_described_by_default_and_inlined_when_requested() {
    let blob = make_png(580, 164);
    let blob_c = blob.clone();
    let net = Arc::new(MockNet::with_handler(move |req, _, _| {
        Ok(HttpBytes::new(
            req.url.clone(),
            200,
            vec![("content-type".into(), "image/png".into())],
            blob_c.clone(),
        ))
    }));
    let svc = make_service(
        net,
        Arc::new(MemStore::new()),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );

    // Default: described, not decoded or inlined.
    let FetchOutcome::Page(page) = svc
        .fetch(
            "https://x.example/logo.png",
            FetchOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap()
    else {
        panic!("expected Page outcome for non-inline image");
    };
    assert_eq!(page.method, FetchMethod::Asset);
    assert_eq!(page.media_type.as_deref(), Some("image/png"));
    assert_eq!((page.width, page.height), (Some(580), Some(164)));
    assert_eq!(page.sha256.as_deref().unwrap().len(), 64);
    assert!(page.content.contains("logo.png"));
    assert!(page.content.contains("580×164px"));
    assert!(page.content.contains("inlineImage"));
    assert!(!page.content.contains('\u{FFFD}'));

    // inline_image = true: returns FetchOutcome::InlineImage with opaque BinaryHandle.
    let cx = CallCx::default();
    let mut inline_opts = FetchOpts::default();
    inline_opts.inline_image = true;
    let FetchOutcome::InlineImage { meta, binary } = svc
        .fetch("https://x.example/logo.png", inline_opts, &cx)
        .await
        .unwrap()
    else {
        panic!("expected InlineImage outcome");
    };
    assert_eq!(meta.media_type, "image/png");
    assert_eq!((meta.width, meta.height), (Some(580), Some(164)));
    assert_eq!(binary.bytes.as_ref(), blob.as_slice());
    assert!(cx.images.lock().await.contains_key(&binary.id));
    // Serialized metadata carries no raw bytes.
    let json_meta = serde_json::to_value(&meta).unwrap();
    assert!(json_meta.get("data").is_none());
    assert_eq!(json_meta["width"], 580);
}

#[tokio::test]
async fn test_extensionless_asset_and_opaque_binary() {
    let png = make_png(2, 2);
    let net = Arc::new(MockNet::with_responses(vec![
        Ok(HttpBytes::new(
            "https://x.example/download?id=9",
            200,
            vec![("content-type".into(), "image/png".into())],
            png,
        )),
        Ok(HttpBytes::new(
            "https://x.example/blob",
            200,
            vec![("content-type".into(), "application/octet-stream".into())],
            [0u8, 1, 2, 3].repeat(100),
        )),
    ]));
    let svc = make_service(
        net.clone(),
        Arc::new(MemStore::new()),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );

    let FetchOutcome::Page(img) = svc
        .fetch(
            "https://x.example/download?id=9",
            FetchOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap()
    else {
        panic!("expected Page");
    };
    assert_eq!(img.method, FetchMethod::Asset);
    assert_eq!((img.width, img.height), (Some(2), Some(2)));
    // Only ONE network request was needed (reused already-fetched bytes).
    assert_eq!(net.recorded().len(), 1);

    let FetchOutcome::Page(bin) = svc
        .fetch(
            "https://x.example/blob",
            FetchOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap()
    else {
        panic!("expected Page");
    };
    assert_eq!(bin.method, FetchMethod::Asset);
    assert!(bin.width.is_none());
    let serialized = serde_json::to_value(&bin).unwrap();
    assert!(serialized.get("width").is_none());
}
