//! Ephemeral file download sandbox (port of Python `downloads.py` + `download_dir_max_mb`).
//!
//! Files are saved under `<cache_dir>/downloads` unless overridden by
//! `SEARCH_MCP_DOWNLOAD_DIR` or disabled via `SEARCH_MCP_DOWNLOAD_ENABLED=false`.
//! Remote URL filenames are treated as untrusted hints: collapsed to a single
//! ASCII component, length-capped, and prefixed with an 8-hex-char SHA-256 digest.

use fsm_core::{Config, FsmError};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Permission message matching Python `downloads._DISABLED_MESSAGE`.
pub const DISABLED_MESSAGE: &str = "Downloads are disabled by SEARCH_MCP_DOWNLOAD_ENABLED=false. \
     Remove that setting or set it to true; SEARCH_MCP_DOWNLOAD_DIR \
     only overrides the destination.";

/// Maximum length of the sanitized remote name component (before `<8-hex>-` prefix).
const MAX_NAME_LEN: usize = 80;

/// Metadata produced by [`save`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedDownload {
    pub path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
}

/// Default download directory derived from the active cache root (`<cache_dir>/downloads`).
pub fn default_download_dir(cfg: &Config) -> PathBuf {
    cfg.cache_dir.join("downloads")
}

/// Active download directory, or `None` when `download_enabled` is false.
///
/// Resolving this path does NOT create the directory on disk.
pub fn download_dir(cfg: &Config) -> Option<PathBuf> {
    if !cfg.download_enabled {
        return None;
    }
    Some(cfg.download_dir.clone())
}

/// Return the active download directory or reject operator disablement.
pub fn require_download_dir(cfg: &Config) -> Result<PathBuf, FsmError> {
    download_dir(cfg).ok_or_else(|| FsmError::PermissionDenied(DISABLED_MESSAGE.to_owned()))
}

/// Derive a safe, single-component, collision-resistant filename for a downloaded URL.
///
/// Mirrors Python `downloads.safe_filename`:
/// 1. Extracts the URL path, percent-decodes it, and strips directory separators (`/` and `\`).
/// 2. Replaces unsafe characters outside `[A-Za-z0-9._-]` with `_`, collapses `..`,
///    and trims leading/trailing `.`, `_`, `-` (defaulting to `"download"`).
/// 3. Infers an extension from `media_type` when the name has no `.`.
/// 4. Caps the name to 80 chars (preserving up to 16 chars of extension).
/// 5. Prefixes `<sha256(blob_or_url)[..8]>-`.
pub fn safe_filename(url: &str, media_type: &str, blob: &[u8]) -> String {
    let path = extract_url_path(url);
    let decoded = percent_decode_lossy(path);
    let last_slash = decoded.rsplit('/').next().unwrap_or("");
    let last_comp = last_slash.replace('\\', "/");
    let raw = last_comp.rsplit('/').next().unwrap_or("");

    let mut name = sanitize_component(raw);
    if name.is_empty() {
        name = "download".to_owned();
    }

    if !name.contains('.') && !media_type.is_empty() {
        let ext = extension_for(media_type);
        if !ext.is_empty() {
            name.push('.');
            name.push_str(ext);
        }
    }

    if name.len() > MAX_NAME_LEN {
        if let Some((stem, ext)) = name.rsplit_once('.') {
            let ext_capped = &ext[..ext.len().min(16)];
            let stem_budget = MAX_NAME_LEN.saturating_sub(ext_capped.len() + 1);
            let stem_capped = &stem[..stem.len().min(stem_budget)];
            name = format!("{stem_capped}.{ext_capped}");
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

/// Delete expired files inside the active download directory. Returns count removed.
///
/// No-op when downloads are disabled, `download_ttl_hours == 0`, or the directory
/// does not exist. Ignores subdirectories and never follows symlinks.
pub fn purge_expired(cfg: &Config, now: Option<SystemTime>) -> usize {
    let Some(root) = download_dir(cfg) else {
        return 0;
    };
    if cfg.download_ttl_hours == 0 {
        return 0;
    }
    let ttl = Duration::from_secs(u64::from(cfg.download_ttl_hours) * 3600);
    let now = now.unwrap_or_else(SystemTime::now);
    let Some(cutoff) = now.checked_sub(ttl) else {
        return 0;
    };

    let Ok(entries) = fs::read_dir(&root) else {
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
        if mtime < cutoff && fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Enforce `download_dir_max_mb` by deleting oldest regular files until
/// `existing_bytes + incoming_bytes <= cap`. Returns the number of files evicted.
pub fn enforce_dir_cap(
    root: &Path,
    incoming_bytes: u64,
    max_dir_mb: u64,
) -> Result<usize, FsmError> {
    if max_dir_mb == 0 {
        return Ok(0);
    }
    let cap = max_dir_mb.saturating_mul(1024 * 1024);
    if incoming_bytes > cap {
        return Err(FsmError::InvalidInput(format!(
            "Refusing to save {:.1} MB: over the {max_dir_mb} MB SEARCH_MCP_DOWNLOAD_DIR_MAX_MB limit.",
            incoming_bytes as f64 / (1024.0 * 1024.0)
        )));
    }
    let Ok(entries) = fs::read_dir(root) else {
        return Ok(0);
    };

    let mut files: Vec<(SystemTime, PathBuf, u64)> = Vec::new();
    let mut total: u64 = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let len = meta.len();
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        total = total.saturating_add(len);
        files.push((mtime, path, len));
    }

    if total.saturating_add(incoming_bytes) <= cap {
        return Ok(0);
    }

    // Oldest-first eviction.
    files.sort_by_key(|(mtime, _, _)| *mtime);
    let mut evicted = 0usize;
    for (_, path, len) in files {
        if total.saturating_add(incoming_bytes) <= cap {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(len);
            evicted += 1;
        }
    }
    Ok(evicted)
}

/// Write `blob` into the configured download directory and return path + SHA-256 metadata.
///
/// Rejects disabled downloads (`PermissionDenied`), payloads exceeding
/// `download_max_mb` or `download_dir_max_mb` (`InvalidInput`, writing nothing),
/// purges oldest files if needed to stay within `download_dir_max_mb`, and
/// verifies the final canonical path remains strictly inside the download root.
pub fn save(
    cfg: &Config,
    url: &str,
    blob: &[u8],
    media_type: &str,
) -> Result<SavedDownload, FsmError> {
    let root = require_download_dir(cfg)?;
    let byte_len = blob.len() as u64;

    if cfg.download_max_mb > 0 {
        let cap = cfg.download_max_mb.saturating_mul(1024 * 1024);
        if byte_len > cap {
            return Err(FsmError::InvalidInput(format!(
                "Refusing to save {:.1} MB: over the {} MB SEARCH_MCP_DOWNLOAD_MAX_MB limit.",
                byte_len as f64 / (1024.0 * 1024.0),
                cfg.download_max_mb
            )));
        }
    }
    if cfg.download_dir_max_mb > 0 {
        let dir_cap = cfg.download_dir_max_mb.saturating_mul(1024 * 1024);
        if byte_len > dir_cap {
            return Err(FsmError::InvalidInput(format!(
                "Refusing to save {:.1} MB: over the {} MB SEARCH_MCP_DOWNLOAD_DIR_MAX_MB limit.",
                byte_len as f64 / (1024.0 * 1024.0),
                cfg.download_dir_max_mb
            )));
        }
    }

    fs::create_dir_all(&root).map_err(|e| {
        FsmError::internal(
            "download_dir",
            format!("failed to create download dir {}: {e}", root.display()),
        )
    })?;
    let resolved_root = fs::canonicalize(&root).map_err(|e| {
        FsmError::internal(
            "download_dir",
            format!("failed to resolve download dir {}: {e}", root.display()),
        )
    })?;

    let filename = safe_filename(url, media_type, blob);
    if filename.contains('/') || filename.contains('\\') || filename == "." || filename == ".." {
        return Err(FsmError::PermissionDenied(format!(
            "Refusing to write outside {}",
            resolved_root.display()
        )));
    }
    let target = resolved_root.join(&filename);
    if target.parent() != Some(resolved_root.as_path()) {
        return Err(FsmError::PermissionDenied(format!(
            "Refusing to write outside {}",
            resolved_root.display()
        )));
    }
    if let Ok(meta) = fs::symlink_metadata(&target)
        && meta.file_type().is_symlink()
    {
        return Err(FsmError::PermissionDenied(format!(
            "Refusing to follow symlink at {}",
            target.display()
        )));
    }

    // Enforce directory cap only after accounting for an existing identical file
    // (re-saving identical bytes overwrites in place).
    let already_present = fs::symlink_metadata(&target)
        .map(|m| if m.is_file() { m.len() } else { 0 })
        .unwrap_or(0);
    let delta = byte_len.saturating_sub(already_present);
    enforce_dir_cap(&resolved_root, delta, cfg.download_dir_max_mb)?;

    // Write via a unique O_CREAT|O_EXCL temporary file in `resolved_root` and
    // atomically rename onto `target` so a concurrent symlink race at `target`
    // can never trick `save()` into writing outside `resolved_root`.
    let sha = sha256_hex(blob);
    let tmp_path = write_atomic_temp(&resolved_root, &sha, blob)?;

    // Re-check symlink_metadata immediately before rename so pre-existing
    // symlinks still fail closed with PermissionDenied instead of being unlinked.
    if let Ok(meta) = fs::symlink_metadata(&target)
        && meta.file_type().is_symlink()
    {
        let _ = fs::remove_file(&tmp_path);
        return Err(FsmError::PermissionDenied(format!(
            "Refusing to follow symlink at {}",
            target.display()
        )));
    }

    if let Err(e) = fs::rename(&tmp_path, &target) {
        let _ = fs::remove_file(&tmp_path);
        return Err(FsmError::internal(
            "download_write",
            format!("failed to commit {}: {e}", target.display()),
        ));
    }

    Ok(SavedDownload {
        path: target,
        bytes: byte_len,
        sha256: sha,
    })
}

fn write_atomic_temp(root: &Path, sha: &str, blob: &[u8]) -> Result<PathBuf, FsmError> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let prefix = &sha[..sha.len().min(8)];

    for _ in 0..16 {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let candidate = root.join(format!(".tmp-{pid}-{seq}-{prefix}"));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut file) => {
                if let Err(e) = file.write_all(blob) {
                    drop(file);
                    let _ = fs::remove_file(&candidate);
                    return Err(FsmError::internal(
                        "download_write",
                        format!("failed to write temp download {}: {e}", candidate.display()),
                    ));
                }
                return Ok(candidate);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(FsmError::internal(
                    "download_write",
                    format!("failed to create temp download in {}: {e}", root.display()),
                ));
            }
        }
    }
    Err(FsmError::internal(
        "download_write",
        format!(
            "failed to allocate unique temp download path in {}",
            root.display()
        ),
    ))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Extract the path component from an HTTP(S) or schemeless URL (stripping `?query` and `#frag`).
fn extract_url_path(url: &str) -> &str {
    let no_frag = url.split_once('#').map_or(url, |(a, _)| a);
    let no_query = no_frag.split_once('?').map_or(no_frag, |(a, _)| a);
    if let Some((_, rest)) = no_query.split_once("://") {
        match rest.find('/') {
            Some(idx) => &rest[idx..],
            None => "",
        }
    } else {
        no_query
    }
}

/// RFC 3986 percent-decoding (`%XX`) to UTF-8 lossily.
fn percent_decode_lossy(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Replace runs of characters outside `[A-Za-z0-9._-]` with `_`, collapse `..`,
/// and strip leading/trailing `.`, `_`, `-`.
fn sanitize_component(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut prev_unsafe = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            out.push(ch);
            prev_unsafe = false;
        } else if !prev_unsafe {
            out.push('_');
            prev_unsafe = true;
        }
    }
    while out.contains("..") {
        out = out.replace("..", ".");
    }
    out.trim_matches(['.', '_', '-']).to_owned()
}

/// Common MIME-to-extension map matching Python's `mimetypes.guess_extension`.
fn extension_for(media_type: &str) -> &'static str {
    let base = media_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match base.as_str() {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "image/bmp" => "bmp",
        "image/tiff" => "tiff",
        "image/x-icon" | "image/vnd.microsoft.icon" => "ico",
        "image/avif" => "avif",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        "text/html" => "html",
        "text/css" => "css",
        "text/csv" => "csv",
        "text/markdown" => "md",
        "text/xml" | "application/xml" => "xml",
        "application/json" => "json",
        "application/javascript" | "text/javascript" => "js",
        "application/zip" => "zip",
        "application/gzip" | "application/x-gzip" => "gz",
        "application/x-tar" => "tar",
        "application/x-bzip2" => "bz2",
        "application/x-xz" => "xz",
        "application/octet-stream" => "bin",
        "application/epub+zip" => "epub",
        "application/msword" => "doc",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "application/vnd.ms-excel" => "xls",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
        "application/vnd.ms-powerpoint" => "ppt",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => "pptx",
        "audio/mpeg" => "mp3",
        "audio/ogg" => "ogg",
        "audio/wav" => "wav",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        _ => "",
    }
}

#[cfg(test)]
mod atomic_download_tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn save_is_atomic_and_refuses_symlink_target_without_leaving_temp_files() {
        let dir = TempDir::new().unwrap();
        let mut cfg = Config::default();
        cfg.download_enabled = true;
        cfg.download_dir = dir.path().to_path_buf();

        let blob = b"safe downloaded content";
        let url = "https://example.com/report.txt";
        let expected_name = safe_filename(url, "text/plain", blob);

        // 1. Pre-existing symlink at target path MUST fail closed with PermissionDenied
        //    and MUST NOT follow the symlink or leave temp files behind.
        #[cfg(unix)]
        {
            let outside = TempDir::new().unwrap();
            let victim = outside.path().join("victim.txt");
            fs::write(&victim, b"untouched").unwrap();
            std::os::unix::fs::symlink(&victim, dir.path().join(&expected_name)).unwrap();

            let err = save(&cfg, url, blob, "text/plain")
                .expect_err("symlink destination must be refused");
            assert!(matches!(err, FsmError::PermissionDenied(_)), "{err:?}");
            assert_eq!(fs::read(&victim).unwrap(), b"untouched");
            fs::remove_file(dir.path().join(&expected_name)).unwrap();
        }

        // 2. Normal save writes atomically and leaves no `.tmp-*` leftovers.
        let saved = save(&cfg, url, blob, "text/plain").unwrap();
        assert_eq!(fs::read(&saved.path).unwrap(), blob);
        let entries: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            !entries.iter().any(|n| n.starts_with(".tmp-")),
            "leftover temp file in {entries:?}"
        );
    }
}
