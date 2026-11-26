//! The DOCUMENT_ROOT sandbox (port of `_resolve_local_path` /
//! `_read_local_capped`).
//!
//! Local reads are OFF unless `SEARCH_MCP_DOCUMENT_ROOT` names a directory.
//! `file://` is refused outright; relative paths resolve against the root
//! (never the CWD); the resolved path (symlinks followed) must stay inside
//! the canonical root; only regular files are opened (`O_NOFOLLOW` and
//! `O_NONBLOCK` on unix, then `fstat` on the handle); size is capped
//! before and during the read.

use fsm_core::FsmError;
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

/// Where a `source` points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SourceKind {
    Remote,
    Local,
}

/// Classify a source: `http(s)://` is remote; `file:` and any other URL
/// scheme are refused; everything else is a local path.
pub(crate) fn classify(source: &str) -> Result<SourceKind, FsmError> {
    if source.trim().is_empty() {
        return Err(FsmError::invalid(
            "source is required: an http(s) URL or a path inside SEARCH_MCP_DOCUMENT_ROOT",
        ));
    }
    let lower = source.trim_start().to_ascii_lowercase();
    if lower.starts_with("file:") {
        return Err(FsmError::invalid(
            "file:// URLs are not allowed for local reads. Pass a plain path inside \
             SEARCH_MCP_DOCUMENT_ROOT instead.",
        ));
    }
    if let Some((scheme, _)) = lower.split_once("://") {
        let valid = scheme
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'));
        if valid {
            return match scheme {
                "http" | "https" => Ok(SourceKind::Remote),
                _ => Err(FsmError::invalid(format!(
                    "unsupported URL scheme '{scheme}': doc.read/doc.outline take an http(s) URL \
                     or a path inside SEARCH_MCP_DOCUMENT_ROOT"
                ))),
            };
        }
    }
    Ok(SourceKind::Local)
}

/// Size and mtime of an opened local file (the parsed-document cache key).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct FileStamp {
    pub len: u64,
    pub modified: Option<SystemTime>,
}

fn expand_home(source: &str, home: Option<&Path>) -> PathBuf {
    match (source, home) {
        ("~", Some(h)) => h.to_path_buf(),
        (s, Some(h)) if s.starts_with("~/") => h.join(&s[2..]),
        (s, _) => PathBuf::from(s),
    }
}

/// Lexical normalisation (`.` dropped, `..` pops) of an absolute path.
fn normalize_lexically(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn expand_home_path(p: &Path, home: Option<&Path>) -> PathBuf {
    match p.to_str() {
        Some(s) => expand_home(s, home),
        None => p.to_path_buf(),
    }
}

/// Non-strict resolve for a path that does not exist: canonicalise the
/// deepest existing ancestor, following dangling symlinks so a symlink
/// targeting outside `root` still resolves outside `root`, then append
/// the remaining components.
fn resolve_missing(p: &Path) -> PathBuf {
    let mut current = normalize_lexically(p);
    for _ in 0..16 {
        let mut existing = current.as_path();
        let mut rest: Vec<std::ffi::OsString> = Vec::new();
        loop {
            if let Ok(c) = existing.canonicalize() {
                let mut out = c;
                for part in rest.iter().rev() {
                    out.push(part);
                }
                return out;
            }
            if let Ok(meta) = std::fs::symlink_metadata(existing)
                && meta.file_type().is_symlink()
                && let Ok(target) = std::fs::read_link(existing)
            {
                let mut next = if target.is_absolute() {
                    target
                } else {
                    existing
                        .parent()
                        .map_or_else(|| target.clone(), |par| par.join(&target))
                };
                for part in rest.iter().rev() {
                    next.push(part);
                }
                current = normalize_lexically(&next);
                break;
            }
            match (existing.parent(), existing.file_name()) {
                (Some(parent), Some(name)) => {
                    rest.push(name.to_os_string());
                    existing = parent;
                }
                _ => return current,
            }
        }
    }
    current
}

/// `_resolve_local_path`: resolve `source` under `root` or refuse.
pub(crate) fn resolve_local_path(
    source: &str,
    root: Option<&Path>,
    home: Option<&Path>,
) -> Result<PathBuf, FsmError> {
    let Some(root) = root else {
        return Err(FsmError::PermissionDenied(
            "Local file reads are disabled; set SEARCH_MCP_DOCUMENT_ROOT to a directory to opt \
             in. Remote http(s) sources are unaffected."
                .to_owned(),
        ));
    };
    let root_expanded = expand_home_path(root, home);
    let root = root_expanded.canonicalize().map_err(|e| {
        FsmError::PermissionDenied(format!(
            "SEARCH_MCP_DOCUMENT_ROOT ({}) is not an accessible directory: {e}",
            root.display()
        ))
    })?;
    if !root.is_dir() {
        return Err(FsmError::PermissionDenied(format!(
            "SEARCH_MCP_DOCUMENT_ROOT ({}) is not a directory.",
            root.display()
        )));
    }
    let mut candidate = expand_home(source, home);
    if !candidate.is_absolute() {
        candidate = root.join(candidate);
    }
    let outside = || {
        FsmError::PermissionDenied(format!(
            "Refusing to read {source:?}: resolves outside the document_root sandbox ({}).",
            root.display()
        ))
    };
    match candidate.canonicalize() {
        Ok(resolved) => {
            if !resolved.starts_with(&root) {
                return Err(outside());
            }
            Ok(resolved)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if !resolve_missing(&candidate).starts_with(&root) {
                return Err(outside());
            }
            Err(FsmError::invalid(format!(
                "No such file under SEARCH_MCP_DOCUMENT_ROOT ({}): {source:?}",
                root.display()
            )))
        }
        Err(e) => {
            if !resolve_missing(&candidate).starts_with(&root) {
                return Err(outside());
            }
            Err(FsmError::invalid(format!("Cannot open {source:?}: {e}")))
        }
    }
}

fn too_big(len: u64, cap: u64) -> FsmError {
    FsmError::BudgetExceeded(format!(
        "Local document byte limit exceeded ({len} > {cap} bytes, SEARCH_MCP_MAX_RESPONSE_BYTES)"
    ))
}

fn not_regular() -> FsmError {
    FsmError::invalid("Local document must be a regular file")
}

#[cfg(unix)]
fn open_no_follow(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

#[cfg(not(unix))]
fn open_no_follow(path: &Path) -> std::io::Result<File> {
    File::open(path)
}

/// `_read_local_capped`: regular files only, at most `cap` bytes (a file
/// that grows past the cap while being read is refused too).
pub(crate) fn read_local_capped(path: &Path, cap: u64) -> Result<(Vec<u8>, FileStamp), FsmError> {
    let io_err = |e: std::io::Error| FsmError::invalid(format!("Cannot read local document: {e}"));
    let meta = std::fs::symlink_metadata(path).map_err(io_err)?;
    if !meta.file_type().is_file() {
        return Err(not_regular());
    }
    if meta.len() > cap {
        return Err(too_big(meta.len(), cap));
    }
    let file = open_no_follow(path).map_err(io_err)?;
    let meta = file.metadata().map_err(io_err)?;
    if !meta.file_type().is_file() {
        return Err(not_regular());
    }
    if meta.len() > cap {
        return Err(too_big(meta.len(), cap));
    }
    let blob = read_capped(file, cap, meta.len())?;
    Ok((
        blob,
        FileStamp {
            len: meta.len(),
            modified: meta.modified().ok(),
        },
    ))
}

/// Read at most `cap + 1` bytes: a file that grows after `fstat` (or lies
/// about its size) is refused instead of being read without bound.
fn read_capped(reader: impl Read, cap: u64, size_hint: u64) -> Result<Vec<u8>, FsmError> {
    let mut blob = Vec::with_capacity(size_hint.min(cap) as usize);
    reader
        .take(cap.saturating_add(1))
        .read_to_end(&mut blob)
        .map_err(|e| FsmError::invalid(format!("Cannot read local document: {e}")))?;
    if blob.len() as u64 > cap {
        return Err(too_big(blob.len() as u64, cap));
    }
    Ok(blob)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Port of the oracle's `GrowingFile`: stat says 1 byte, reads never end.
    /// (The "off the event loop" half is structural here: `DocService` runs
    /// resolution, stat and the read inside `spawn_blocking`.)
    #[test]
    fn test_local_file_io_runs_off_loop_and_bounds_growing_files() {
        struct Growing(u64);
        impl Read for Growing {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                buf.fill(b'x');
                self.0 += buf.len() as u64;
                Ok(buf.len())
            }
        }
        let mut growing = Growing(0);
        let err = read_capped(&mut growing, 64, 1).unwrap_err();
        assert_eq!(err.kind(), "BudgetExceeded");
        assert!(err.to_string().to_lowercase().contains("limit"), "{err}");
        assert_eq!(growing.0, 65, "pulled exactly cap + 1 bytes");
        assert_eq!(read_capped(&b"exact"[..], 5, 5).unwrap(), b"exact");
    }

    #[test]
    fn classify_schemes() {
        assert_eq!(
            classify("https://x.example/a.pdf").unwrap(),
            SourceKind::Remote
        );
        assert_eq!(classify("HTTP://x.example/").unwrap(), SourceKind::Remote);
        assert_eq!(classify("notes/a.txt").unwrap(), SourceKind::Local);
        assert_eq!(classify("/abs/a.txt").unwrap(), SourceKind::Local);
        assert_eq!(classify("C:\\docs\\a.txt").unwrap(), SourceKind::Local);
        for bad in [
            "file:///etc/passwd",
            "FILE:/etc/passwd",
            "ftp://x/y",
            "data://x",
            "  ",
        ] {
            assert_eq!(classify(bad).unwrap_err().kind(), "InvalidInput", "{bad}");
        }
    }

    #[test]
    fn test_local_read_disabled_by_default() {
        let err = resolve_local_path("/etc/passwd", None, None).unwrap_err();
        assert_eq!(err.kind(), "PermissionDenied");
        assert!(err.to_string().contains("SEARCH_MCP_DOCUMENT_ROOT"));
    }

    #[test]
    fn test_local_read_traversal_blocked() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("sandbox");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(tmp.path().join("secret.txt"), "s").unwrap();
        for src in [
            "../secret.txt",
            "../../etc/passwd",
            "/etc/passwd",
            "../missing/x.txt",
            "a/../../secret.txt",
        ] {
            let err = resolve_local_path(src, Some(&root), None).unwrap_err();
            assert_eq!(err.kind(), "PermissionDenied", "{src}: {err}");
        }
        let missing = resolve_local_path("nope.txt", Some(&root), None).unwrap_err();
        assert_eq!(missing.kind(), "InvalidInput");
        assert!(missing.to_string().contains("No such file"));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_refused_and_inside_links_are_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(tmp.path().join("outside.txt"), "x").unwrap();
        std::fs::write(root.join("inside.txt"), "y").unwrap();
        std::os::unix::fs::symlink(tmp.path().join("outside.txt"), root.join("esc.txt")).unwrap();
        std::os::unix::fs::symlink(root.join("inside.txt"), root.join("ok.txt")).unwrap();
        assert_eq!(
            resolve_local_path("esc.txt", Some(&root), None)
                .unwrap_err()
                .kind(),
            "PermissionDenied"
        );
        let ok = resolve_local_path("ok.txt", Some(&root), None).unwrap();
        assert!(ok.ends_with("inside.txt"));
        // A dangling symlink pointing outside root must be PermissionDenied
        // rather than leaking existence as "No such file".
        std::os::unix::fs::symlink(
            tmp.path().join("never_existed_secret.txt"),
            root.join("dangling_esc.txt"),
        )
        .unwrap();
        assert_eq!(
            resolve_local_path("dangling_esc.txt", Some(&root), None)
                .unwrap_err()
                .kind(),
            "PermissionDenied"
        );
    }

    #[test]
    fn document_root_must_be_a_directory_and_expands_tilde() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("not_a_dir.txt");
        std::fs::write(&file, "x").unwrap();
        assert_eq!(
            resolve_local_path(&file.to_string_lossy(), Some(&file), None)
                .unwrap_err()
                .kind(),
            "PermissionDenied"
        );
        let docs = tmp.path().join("docs");
        std::fs::create_dir(&docs).unwrap();
        std::fs::write(docs.join("ok.txt"), "hi").unwrap();
        let p = resolve_local_path("ok.txt", Some(Path::new("~/docs")), Some(tmp.path())).unwrap();
        assert!(p.ends_with("docs/ok.txt"));
    }

    #[test]
    fn home_expansion_still_has_to_stay_inside_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("docs");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("n.txt"), "n").unwrap();
        let p = resolve_local_path("~/docs/n.txt", Some(&root), Some(tmp.path())).unwrap();
        assert!(p.ends_with("docs/n.txt"));
        assert_eq!(
            resolve_local_path("~", Some(&root), Some(tmp.path()))
                .unwrap_err()
                .kind(),
            "PermissionDenied"
        );
    }

    #[test]
    fn test_oversized_local_file_refused_before_read() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("big.txt");
        std::fs::write(&p, vec![b'x'; 100]).unwrap();
        let err = read_local_capped(&p, 50).unwrap_err();
        assert_eq!(err.kind(), "BudgetExceeded");
        assert!(err.to_string().to_lowercase().contains("limit"));
        let (blob, stamp) = read_local_capped(&p, 100).unwrap();
        assert_eq!((blob.len(), stamp.len), (100, 100));
    }

    #[cfg(unix)]
    #[test]
    fn test_local_special_file_refused_without_opening() {
        let tmp = tempfile::tempdir().unwrap();
        let fifo = tmp.path().join("pipe");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: c is a valid NUL-terminated path; mkfifo has no other preconditions.
        let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o600) };
        assert_eq!(rc, 0);
        // Opening a FIFO for reading would block forever; the check must refuse first.
        let err = read_local_capped(&fifo, 1024).unwrap_err();
        assert!(err.to_string().contains("regular file"), "{err}");
        let dir_err = read_local_capped(tmp.path(), 1024).unwrap_err();
        assert!(dir_err.to_string().contains("regular file"));
    }
}
