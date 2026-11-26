//! Hot-reloaded secret store for `<config_dir>/config.json` (port of `src/search_mcp/keystore.py`).
//!
//! Precedence: process environment `SEARCH_MCP_<UPPER>` > `<config_dir>/config.json` > `None`.
//! Files are written atomically with `0600` permissions on POSIX (`0700` parent directory).

use fsm_core::Config;
use fsm_core::config::ENV_PREFIX;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::SystemTime;

#[derive(Debug, Clone)]
struct CachedFile {
    mtime: Option<SystemTime>,
    len: u64,
    secrets: BTreeMap<String, String>,
}

static CACHE: LazyLock<Mutex<BTreeMap<PathBuf, CachedFile>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

/// Resolve the config directory (`SEARCH_MCP_CONFIG_DIR` or `~/.config/search-mcp`).
pub fn config_dir(override_dir: Option<&Path>) -> PathBuf {
    if let Some(dir) = override_dir {
        return dir.to_path_buf();
    }
    if let Ok(env_dir) = std::env::var("SEARCH_MCP_CONFIG_DIR") {
        let trimmed = env_dir.trim();
        if !trimmed.is_empty() && !fsm_core::config::is_unfilled(trimmed) {
            if let Some(rest) = trimmed.strip_prefix("~/")
                && let Some(home) = fsm_core::config::system_home()
            {
                return home.join(rest);
            }
            return PathBuf::from(trimmed);
        }
    }
    Config::default().config_dir
}

/// Path to `<config_dir>/config.json`.
pub fn config_path(dir: &Path) -> PathBuf {
    dir.join("config.json")
}

/// Clear the in-process hot-reload cache (used by tests).
pub fn reset_cache() {
    if let Ok(mut guard) = CACHE.lock() {
        guard.clear();
    }
}

fn read_file_secrets(path: &Path) -> BTreeMap<String, String> {
    let Ok(meta) = fs::metadata(path) else {
        if let Ok(mut guard) = CACHE.lock() {
            guard.remove(path);
        }
        return BTreeMap::new();
    };
    let mtime = meta.modified().ok();
    let len = meta.len();

    if let Ok(guard) = CACHE.lock()
        && let Some(entry) = guard.get(path)
        && entry.mtime == mtime
        && entry.len == len
    {
        return entry.secrets.clone();
    }

    let mut out = BTreeMap::new();
    if let Ok(raw) = fs::read_to_string(path)
        && let Ok(Value::Object(root)) = serde_json::from_str::<Value>(&raw)
    {
        let table = match root.get("secrets") {
            Some(Value::Object(sub)) => sub,
            _ => &root,
        };
        for (k, v) in table {
            let s = match v {
                Value::String(s) => s.trim().to_owned(),
                Value::Number(n) => n.to_string(),
                _ => continue,
            };
            if !s.is_empty() {
                out.insert(k.to_ascii_lowercase(), s);
            }
        }
    }

    if let Ok(mut guard) = CACHE.lock() {
        guard.insert(
            path.to_path_buf(),
            CachedFile {
                mtime,
                len,
                secrets: out.clone(),
            },
        );
    }
    out
}

/// Return all secrets currently persisted in `<config_dir>/config.json` (file view only; ignores env).
pub fn all_secrets(dir: &Path) -> BTreeMap<String, String> {
    read_file_secrets(&config_path(dir))
}

/// Look up a secret by snake_case field name (e.g. `"antigravity_client_id"`).
///
/// Environment variable `SEARCH_MCP_<UPPER>` takes precedence over `config.json`.
pub fn get_secret(field: &str, dir: &Path) -> Option<String> {
    let env_name = format!("{ENV_PREFIX}{}", field.to_ascii_uppercase());
    if let Ok(val) = std::env::var(&env_name) {
        let trimmed = val.trim();
        if !trimmed.is_empty() && !fsm_core::config::is_unfilled(trimmed) {
            return Some(trimmed.to_owned());
        }
    }
    let key = field.to_ascii_lowercase();
    read_file_secrets(&config_path(dir)).remove(&key)
}

/// Merge `updates` into `<config_dir>/config.json` under `{"secrets": {...}}`.
///
/// Empty or whitespace-only values delete the key. Written atomically with `0600`
/// permissions on Unix (`0700` parent directory when created).
pub fn set_secrets(updates: &BTreeMap<String, String>, dir: &Path) -> std::io::Result<()> {
    ensure_private_dir(dir)?;
    let path = config_path(dir);

    let mut root: Map<String, Value> = fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();

    let mut secrets: Map<String, Value> = match root.remove("secrets") {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    };

    for (k, v) in updates {
        let key = k.to_ascii_lowercase();
        let trimmed = v.trim();
        if trimmed.is_empty() {
            secrets.remove(&key);
        } else {
            secrets.insert(key, Value::String(trimmed.to_owned()));
        }
    }
    root.insert("secrets".to_owned(), Value::Object(secrets));

    let payload = serde_json::to_vec_pretty(&Value::Object(root))
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    write_private_atomic(&path, &payload)?;

    if let Ok(mut guard) = CACHE.lock() {
        guard.remove(&path);
    }
    Ok(())
}

/// Create `dir` (and parents) with `0700` permissions on POSIX.
pub fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

/// Atomically write `bytes` to `path` with `0600` permissions on POSIX.
pub fn write_private_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    ensure_private_dir(parent)?;

    let mut tmp = tempfile::Builder::new()
        .prefix(".tmp-")
        .tempfile_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o600))?;
    }
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}
