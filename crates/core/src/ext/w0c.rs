//! Extension point of lane `W0c` — SQLite Storage Layer (`fsm-store`).
//!
//! Shared pure constants and validation/diagnostic helpers that crates
//! depending only on `fsm-core` (such as `fsm-surface` and `fsm-script`)
//! can call without taking a dependency on `fsm-store`.

use crate::error::FsmError;

/// Filename of the 1.0 SQLite database inside `Config::cache_dir`.
///
/// Legacy `cache.sqlite` (Python v0.18.2) is never opened or migrated.
pub const DB_FILENAME: &str = "search-mcp.sqlite";

/// Schema `PRAGMA user_version` for `search-mcp.sqlite`.
pub const SCHEMA_USER_VERSION: u32 = 1;

/// Default write cadence for opportunistic maintenance sweeps (`SPEC.md` §8).
pub const DEFAULT_MAINTAIN_EVERY: u64 = 200;

/// M7 `exec_kv` session ID reserved for `exec:<id>` overflow handles.
pub const EXEC_KV_OVERFLOW_SESSION: &str = "";

/// M7 `exec_kv` session ID used when `cache.put(..., { shared: true })` is called.
pub const EXEC_KV_SHARED_SESSION: &str = "shared";

/// Reserved prefix for `exec` output overflow handles (`exec:<id>`).
pub const EXEC_KV_OVERFLOW_PREFIX: &str = "exec:";

/// Maximum raw (pre-zstd) JSON byte length of a single `exec_kv` value (2 MiB).
pub const EXEC_KV_MAX_VAL_BYTES: usize = 2 * 1024 * 1024;

/// Maximum total raw JSON bytes retained across `exec_kv` under LRU (64 MiB).
pub const EXEC_KV_LRU_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Default TTL for `exec_kv` entries (24 hours).
pub const EXEC_KV_DEFAULT_TTL_S: u64 = 24 * 3600;

/// Hard maximum TTL for `exec_kv` entries (168 hours / 7 days).
pub const EXEC_KV_MAX_TTL_S: u64 = 168 * 3600;

/// Maximum length of an `exec_kv` key (`^[a-zA-Z0-9._:-]{1,128}$`).
pub const EXEC_KV_MAX_KEY_LEN: usize = 128;

/// Lockfile name inside `Config::cache_dir` for cross-process Retraction Watch sync (I15).
pub const RW_SYNC_LOCK_FILENAME: &str = "rw-sync.lock";

/// Validate an `exec_kv` key against `^[a-zA-Z0-9._:-]{1,128}$`.
pub fn validate_kv_key(key: &str) -> Result<(), FsmError> {
    if key.is_empty() || key.len() > EXEC_KV_MAX_KEY_LEN {
        return Err(FsmError::InvalidInput(format!(
            "cache key must be 1..={EXEC_KV_MAX_KEY_LEN} ASCII characters matching ^[a-zA-Z0-9._:-]+$"
        )));
    }
    if !key
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
    {
        return Err(FsmError::InvalidInput(format!(
            "invalid cache key {key:?}: only ASCII letters, digits, '.', '_', ':', '-' are allowed"
        )));
    }
    Ok(())
}

/// Validate a user-supplied `cache.put` key: must match `^[a-zA-Z0-9._:-]{1,128}$`
/// and must NOT start with the reserved `exec:` prefix (`SPEC.md` §2.5.3).
pub fn validate_user_kv_key(key: &str) -> Result<(), FsmError> {
    validate_kv_key(key)?;
    if key.starts_with(EXEC_KV_OVERFLOW_PREFIX) {
        return Err(FsmError::InvalidInput(
            "cache key must not start with 'exec:' (reserved for exec overflow handles)".into(),
        ));
    }
    Ok(())
}

/// Validate an internal overflow handle key (`exec:<non-empty-id>`).
pub fn validate_overflow_kv_key(key: &str) -> Result<(), FsmError> {
    validate_kv_key(key)?;
    match key.strip_prefix(EXEC_KV_OVERFLOW_PREFIX) {
        Some(rest) if !rest.is_empty() => Ok(()),
        _ => Err(FsmError::InvalidInput(
            "overflow key must start with 'exec:' followed by a non-empty identifier".into(),
        )),
    }
}

/// Case-insensitive ASCII prefix strip that never slices mid-UTF-8 character.
///
/// Compares raw bytes first so a multi-byte character straddling `prefix.len()`
/// returns `None` instead of panicking.
pub fn strip_prefix_ascii_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.as_bytes().get(..prefix.len())?;
    (head.eq_ignore_ascii_case(prefix.as_bytes()) && s.is_char_boundary(prefix.len()))
        .then(|| &s[prefix.len()..])
}

/// Normalize a DOI string to canonical lowercase `10.xxxx/...` form.
///
/// Strips surrounding whitespace, `https?://(dx.)?doi.org/` resolver prefixes,
/// case-insensitive `doi:` prefixes, and trailing punctuation (`.`, `,`, `;`, `)`).
/// Returns `None` if the result is empty.
pub fn normalize_doi(raw: &str) -> Option<String> {
    let mut s = raw.trim();
    if s.is_empty() {
        return None;
    }
    // Strip resolver URL prefixes case-insensitively without slicing mid-codepoint.
    for prefix in [
        "https://doi.org/",
        "http://doi.org/",
        "https://dx.doi.org/",
        "http://dx.doi.org/",
    ] {
        if let Some(rest) = strip_prefix_ascii_ci(s, prefix) {
            s = rest;
            break;
        }
    }
    // Strip leading `doi:` case-insensitively.
    if let Some(rest) = strip_prefix_ascii_ci(s, "doi:") {
        s = rest.trim_start();
    }
    let s = s.trim_end_matches(['.', ',', ';', ')']).trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_ascii_lowercase())
    }
}

/// FTS5 boolean keywords that are only valid as infix operators between terms.
const FTS_OPERATORS: [&str; 4] = ["AND", "OR", "NOT", "NEAR"];

/// Return a sanitized hint if `query` looks like malformed FTS5 syntax, else `None`.
///
/// Direct port of Python `server._invalid_fts_hint`: the storage layer swallows
/// malformed `MATCH` `OperationalError`s and returns `[]` so raw SQLite text
/// never leaks to callers. Surface renderers call this helper on empty results
/// to distinguish syntax errors from genuine cache misses.
pub fn invalid_fts_hint(query: &str) -> Option<&'static str> {
    let q = query.trim();
    if q.is_empty() {
        return None;
    }
    // Unbalanced double quotes -> unterminated phrase.
    if q.bytes().filter(|&b| b == b'"').count() % 2 == 1 {
        return Some(
            "Your query has an unterminated quote. FTS5 phrases need matching \
             double quotes, e.g. `\"exact phrase\"`.",
        );
    }
    // Unbalanced parentheses (including inverted `)(`).
    let mut depth: i32 = 0;
    for b in q.bytes() {
        match b {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth < 0 {
                    return Some(
                        "Your query has unbalanced parentheses. Group sub-expressions like \
                         `(a OR b) c`.",
                    );
                }
            }
            _ => {}
        }
    }
    if depth != 0 {
        return Some(
            "Your query has unbalanced parentheses. Group sub-expressions like \
             `(a OR b) c`.",
        );
    }

    let tokens: Vec<String> = q.split_whitespace().map(str::to_ascii_uppercase).collect();
    if tokens.is_empty() {
        return None;
    }
    let is_op = |tok: &str| FTS_OPERATORS.contains(&tok);
    if is_op(&tokens[0]) || is_op(&tokens[tokens.len() - 1]) {
        return Some(
            "Your query starts or ends with a boolean operator (AND/OR/NOT/NEAR). \
             These join two terms, e.g. `cats AND dogs`, not `cats AND`.",
        );
    }
    for pair in tokens.windows(2) {
        if is_op(&pair[0]) && is_op(&pair[1]) {
            return Some(
                "Your query has two boolean operators in a row. Put a term \
                 between them, e.g. `a AND b OR c`.",
            );
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kv_key_validation_enforces_regex_and_exec_prefix() {
        assert!(validate_kv_key("a_b-c.d:1").is_ok());
        assert!(validate_kv_key(&"x".repeat(128)).is_ok());
        assert!(validate_kv_key("").is_err());
        assert!(validate_kv_key(&"x".repeat(129)).is_err());
        assert!(validate_kv_key("bad key").is_err());
        assert!(validate_user_kv_key("my:key").is_ok());
        assert!(validate_user_kv_key("exec:abc12345").is_err());
        assert!(validate_overflow_kv_key("exec:abc12345").is_ok());
        assert!(validate_overflow_kv_key("exec:").is_err());
        assert!(validate_overflow_kv_key("other").is_err());
    }

    #[test]
    fn normalize_doi_strips_prefixes_and_punctuation() {
        assert_eq!(
            normalize_doi("  https://dx.doi.org/10.1038/NATURE14539. ").as_deref(),
            Some("10.1038/nature14539")
        );
        assert_eq!(
            normalize_doi("DOI:10.48550/arXiv.1706.03762);").as_deref(),
            Some("10.48550/arxiv.1706.03762")
        );
        assert_eq!(normalize_doi("   "), None);
        // B1 regression: non-ASCII / CJK / full-width punctuation must never panic.
        assert_eq!(normalize_doi("深度学习").as_deref(), Some("深度学习"));
        assert_eq!(
            normalize_doi("DOI：10.1000/xyz").as_deref(),
            Some("doi：10.1000/xyz")
        );
        assert_eq!(
            normalize_doi("10.1000/abcdef日本").as_deref(),
            Some("10.1000/abcdef日本")
        );
        assert_eq!(
            normalize_doi("Café society").as_deref(),
            Some("café society")
        );
    }

    #[test]
    fn invalid_fts_hint_flags_malformed_queries_only() {
        assert!(invalid_fts_hint("a AND").is_some());
        assert!(invalid_fts_hint("\"open").is_some());
        assert!(invalid_fts_hint("(a OR b").is_some());
        assert!(invalid_fts_hint(")(").is_some());
        assert!(invalid_fts_hint("a AND OR b").is_some());
        assert_eq!(invalid_fts_hint("cats AND dogs"), None);
        // m1 regression: valid FTS5 phrase prefix syntax `"a b" *` must not be flagged.
        assert_eq!(invalid_fts_hint(r#""a b" *"#), None);
        assert_eq!(invalid_fts_hint(""), None);
    }
}
