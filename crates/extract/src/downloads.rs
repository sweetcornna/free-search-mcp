//! Ephemeral file downloads (`web.download`, port of `src/search_mcp/downloads.py`).
//!
//! Enforces:
//! - Operator disablement (`SEARCH_MCP_DOWNLOAD_ENABLED=false`) BEFORE network.
//! - Per-exec cap: max 5 downloads per `exec` (`CallCx.downloads_used`, I10).
//! - Offline SSRF L1–L3 + per-redirect-hop SSRF check before network.
//! - Single-file size cap (`SEARCH_MCP_DOWNLOAD_MAX_MB`, default 100 MB).
//! - Directory total quota (`SEARCH_MCP_DOWNLOAD_DIR_MAX_MB`, default 500 MB, I14).
//! - Expiry purge (`SEARCH_MCP_DOWNLOAD_TTL_HOURS`, default 24 h; 0 disables expiry).
//! - Untrusted remote filename sanitization (single path component, content-addressed
//!   `<sha256[:8]>-<name>`, canonicalized sandbox check, atomic temp-file write).

use crate::request::{
    check_http_url_offline, check_tainted_host, parse_retry_after, send_get_with_redirects,
};
use fsm_core::config::Config;
use fsm_core::url_canon::host_of;
use fsm_core::{CallCx, DownloadResult, FsmError, SourceDeps, TransportProfile};
use percent_encoding::percent_decode_str;
use regex::Regex;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime};
use tempfile::NamedTempFile;
use url::Url;

pub const MAX_DOWNLOADS_PER_EXEC: u32 = 5;
const MAX_NAME_LEN: usize = 80;

/// Serializes directory quota checks and atomic writes in `save_bytes` to prevent
/// concurrent `Promise.all` downloads from racing past `SEARCH_MCP_DOWNLOAD_DIR_MAX_MB`.
static DOWNLOAD_SAVE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

const DISABLED_MESSAGE: &str = "Downloads are disabled by SEARCH_MCP_DOWNLOAD_ENABLED=false. \
    Remove that setting or set it to true; SEARCH_MCP_DOWNLOAD_DIR only overrides the destination.";

static UNSAFE_FILENAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^A-Za-z0-9._-]+").expect("filename regex"));

/// Return the active download directory, or `None` when disabled.
pub fn download_dir(cfg: &Config) -> Option<&Path> {
    if !cfg.download_enabled {
        return None;
    }
    Some(cfg.download_dir.as_path())
}

/// Return the active download directory or `PermissionDenied` when disabled.
pub fn require_download_dir(cfg: &Config) -> Result<&Path, FsmError> {
    download_dir(cfg).ok_or_else(|| FsmError::PermissionDenied(DISABLED_MESSAGE.to_owned()))
}

/// Infer a file extension (without leading dot) from a media type.
pub fn extension_for_media_type(media_type: &str) -> &'static str {
    let media = media_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match media.as_str() {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        "image/tiff" => "tiff",
        "image/svg+xml" => "svg",
        "image/x-icon" | "image/vnd.microsoft.icon" => "ico",
        "image/avif" => "avif",
        "application/pdf" => "pdf",
        "application/json" => "json",
        "application/xml" | "text/xml" => "xml",
        "text/plain" => "txt",
        "text/html" => "html",
        "text/css" => "css",
        "text/csv" => "csv",
        "text/markdown" => "md",
        "application/zip" | "application/x-zip-compressed" => "zip",
        "application/gzip" | "application/x-gzip" => "gz",
        "application/x-tar" => "tar",
        "application/wasm" => "wasm",
        "application/epub+zip" => "epub",
        "application/msword" => "doc",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => "pptx",
        "audio/mpeg" => "mp3",
        "audio/wav" => "wav",
        "audio/ogg" => "ogg",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "font/woff2" => "woff2",
        "font/woff" => "woff",
        "application/octet-stream" => "bin",
        _ => "",
    }
}

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex::encode(digest)
}

/// Derive a safe, single-component, content-addressed filename from an untrusted URL.
pub fn safe_filename(url: &str, media_type: &str, blob: &[u8]) -> String {
    let path_str = Url::parse(url)
        .map(|u| u.path().to_owned())
        .unwrap_or_else(|_| url.to_owned());
    let decoded = percent_decode_str(&path_str).decode_utf8_lossy();
    let normalized = decoded.replace('\\', "/");
    let last_segment = normalized.rsplit('/').next().unwrap_or("");

    let replaced = UNSAFE_FILENAME_RE.replace_all(last_segment, "_");
    let mut name = replaced.trim_matches(['.', '_', '-']).to_owned();
    // Collapse any remaining `..` sequences so `..` can never appear in the filename.
    while name.contains("..") {
        name = name.replace("..", "_");
    }
    name = name.trim_matches(['.', '_', '-']).to_owned();
    if name.is_empty() {
        name = "download".to_owned();
    }

    if !name.contains('.') && !media_type.is_empty() {
        let ext = extension_for_media_type(media_type);
        if !ext.is_empty() {
            name.push('.');
            name.push_str(ext);
        }
    }

    if name.len() > MAX_NAME_LEN {
        if let Some((stem, ext)) = name.rsplit_once('.') {
            let ext_clipped = &ext[..ext.len().min(16)];
            let stem_budget = MAX_NAME_LEN.saturating_sub(ext_clipped.len() + 1);
            let stem_clipped = &stem[..stem.len().min(stem_budget)];
            name = format!("{stem_clipped}.{ext_clipped}");
        } else {
            name.truncate(MAX_NAME_LEN);
        }
    }

    let hash_input = if blob.is_empty() {
        url.as_bytes()
    } else {
        blob
    };
    let digest = sha256_hex(hash_input);
    format!("{}-{name}", &digest[..8])
}

/// Delete regular files in the download directory older than `cfg.download_ttl_hours`.
/// Ignores subdirectories and missing directories. Returns the number of files removed.
pub fn purge_expired(cfg: &Config, now: Option<SystemTime>) -> usize {
    let Some(root) = download_dir(cfg) else {
        return 0;
    };
    let ttl_hours = cfg.download_ttl_hours;
    if ttl_hours == 0 {
        return 0;
    }
    let ttl = Duration::from_secs(u64::from(ttl_hours) * 3600);
    let now = now.unwrap_or_else(SystemTime::now);
    let Ok(entries) = fs::read_dir(root) else {
        return 0;
    };
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let Ok(mtime) = meta.modified() else {
            continue;
        };
        let age = now.duration_since(mtime).unwrap_or(Duration::ZERO);
        if age > ttl && fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    if removed > 0 {
        tracing::info!(
            "purged {removed} expired download(s) from {}",
            root.display()
        );
    }
    removed
}

/// Total size in bytes of regular files directly inside `root`, excluding `skip_name` if given.
fn dir_total_bytes(root: &Path, skip_name: Option<&str>) -> u64 {
    let Ok(entries) = fs::read_dir(root) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let path = entry.path();
        if let Some(skip) = skip_name
            && path.file_name().and_then(|n| n.to_str()) == Some(skip)
        {
            continue;
        }
        if let Ok(meta) = fs::symlink_metadata(&path)
            && meta.is_file()
        {
            total = total.saturating_add(meta.len());
        }
    }
    total
}

/// Persist `blob` inside the configured download directory, enforcing file & directory caps
/// and verifying the canonicalized target path remains inside `root`.
pub fn save_bytes(
    cfg: &Config,
    url: &str,
    blob: &[u8],
    media_type: &str,
) -> Result<PathBuf, FsmError> {
    let root = require_download_dir(cfg)?;

    let file_cap = cfg.download_max_mb.saturating_mul(1024 * 1024);
    if file_cap > 0 && (blob.len() as u64) > file_cap {
        return Err(FsmError::invalid(format!(
            "Refusing to save {:.1} MB: over the {} MB SEARCH_MCP_DOWNLOAD_MAX_MB limit.",
            blob.len() as f64 / (1024.0 * 1024.0),
            cfg.download_max_mb
        )));
    }

    let filename = safe_filename(url, media_type, blob);

    // Serialize quota check + write to prevent concurrent downloads from exceeding the dir cap.
    let _save_guard = DOWNLOAD_SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // Check directory total cap before creating or writing.
    let dir_cap = cfg.download_dir_max_mb.saturating_mul(1024 * 1024);
    if dir_cap > 0 {
        let existing = dir_total_bytes(root, Some(&filename));
        if existing.saturating_add(blob.len() as u64) > dir_cap {
            return Err(FsmError::BudgetExceeded(format!(
                "Download directory would exceed {} MB limit (SEARCH_MCP_DOWNLOAD_DIR_MAX_MB)",
                cfg.download_dir_max_mb
            )));
        }
    }

    fs::create_dir_all(root).map_err(|e| {
        FsmError::internal(
            "download_mkdir",
            format!(
                "could not create download directory {}: {e}",
                root.display()
            ),
        )
    })?;
    let resolved_root = fs::canonicalize(root).map_err(|e| {
        FsmError::internal(
            "download_root",
            format!(
                "could not canonicalize download directory {}: {e}",
                root.display()
            ),
        )
    })?;

    // Belt-and-braces sandbox check.
    if filename.contains('/') || filename.contains('\\') || filename.contains("..") {
        return Err(FsmError::PermissionDenied(format!(
            "Refusing to write unsafe filename {filename:?}"
        )));
    }
    let target = resolved_root.join(&filename);
    if target.parent() != Some(resolved_root.as_path()) || !target.starts_with(&resolved_root) {
        return Err(FsmError::PermissionDenied(format!(
            "Refusing to write outside {}",
            resolved_root.display()
        )));
    }

    // Atomic write via temp file in the same directory so partial writes never remain on disk.
    let mut tmp = NamedTempFile::new_in(&resolved_root).map_err(|e| {
        FsmError::internal("download_tmp", format!("could not create temp file: {e}"))
    })?;
    tmp.write_all(blob).map_err(|e| {
        FsmError::internal("download_write", format!("could not write download: {e}"))
    })?;
    tmp.flush().map_err(|e| {
        FsmError::internal("download_flush", format!("could not flush download: {e}"))
    })?;
    tmp.persist(&target).map_err(|e| {
        FsmError::internal(
            "download_persist",
            format!("could not save download to {}: {e}", target.display()),
        )
    })?;

    Ok(target)
}

/// Full `web.download` flow: enforce disablement, SSRF, per-exec cap (≤ 5),
/// purge expired files, fetch raw bytes, save under sandbox, record in store.
pub async fn download_url(
    url: &str,
    deps: &SourceDeps,
    cx: &CallCx,
) -> Result<DownloadResult, FsmError> {
    // 1. Reject operator disablement BEFORE any network call or budget charge.
    require_download_dir(&deps.cfg)?;

    // 2. Offline SSRF + M6 taint check BEFORE network or budget charge.
    check_http_url_offline(url, deps)?;
    check_tainted_host(url, cx).await?;

    // 3. Per-exec download cap (max 5 downloads per exec).
    let slot = cx
        .downloads_used
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            (n < MAX_DOWNLOADS_PER_EXEC).then_some(n + 1)
        });
    if slot.is_err() {
        return Err(FsmError::BudgetExceeded(format!(
            "download limit of {MAX_DOWNLOADS_PER_EXEC} per exec reached"
        )));
    }

    // 4. Purge expired downloads before checking dir quota.
    let cfg_clone = deps.cfg.clone();
    let _ = tokio::task::spawn_blocking(move || purge_expired(&cfg_clone, None)).await;

    // 5. Fetch raw bytes with file cap (+1 byte to detect truncation over cap).
    let file_cap = if deps.cfg.download_max_mb > 0 {
        deps.cfg.download_max_mb.saturating_mul(1024 * 1024)
    } else {
        deps.cfg.max_response_bytes
    };
    let (final_url, resp) = send_get_with_redirects(
        url,
        &[],
        TransportProfile::Plain,
        file_cap,
        "download",
        deps,
        cx,
    )
    .await?;

    if resp.status == 429 {
        return Err(FsmError::RateLimited {
            source_id: host_of(&final_url),
            retry_after_s: parse_retry_after(&resp),
        });
    }
    if !resp.is_success() {
        return Err(FsmError::upstream(
            "download",
            Some(resp.status),
            format!("HTTP {} downloading {final_url}", resp.status),
        ));
    }

    if resp.truncated || (file_cap > 0 && (resp.body.len() as u64) > file_cap) {
        return Err(FsmError::invalid(format!(
            "Refusing to save >{} MB: over the {} MB SEARCH_MCP_DOWNLOAD_MAX_MB limit.",
            deps.cfg.download_max_mb, deps.cfg.download_max_mb
        )));
    }

    let media_type = resp
        .media_type()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "application/octet-stream".to_owned());

    let (width, height) = if media_type.starts_with("image/") {
        imagesize::blob_size(&resp.body)
            .ok()
            .map(|sz| (Some(sz.width as u32), Some(sz.height as u32)))
            .unwrap_or((None, None))
    } else {
        (None, None)
    };

    let bytes_len = resp.body.len() as u64;
    let hash = sha256_hex(&resp.body);

    let cfg_save = deps.cfg.clone();
    let url_save = final_url.clone();
    let media_save = media_type.clone();
    let body_save = resp.body.clone();
    let saved_path = tokio::task::spawn_blocking(move || {
        save_bytes(&cfg_save, &url_save, &body_save, &media_save)
    })
    .await
    .map_err(|e| FsmError::internal("download_save_task", e.to_string()))??;

    let mut result = DownloadResult::new(
        final_url,
        saved_path.to_string_lossy().into_owned(),
        media_type,
        bytes_len,
        hash,
    );
    result.expires_in_hours = deps.cfg.download_ttl_hours;
    result.width = width;
    result.height = height;

    let ttl_s = u64::from(deps.cfg.download_ttl_hours).saturating_mul(3600);
    if let Err(e) = deps.store.download_record(&result, ttl_s).await {
        tracing::warn!("could not record download in store: {e}");
    }

    Ok(result)
}
