//! FTS5 `MATCH` error classification and sanitized syntax-hint helpers.

pub use fsm_core::ext::w0c::invalid_fts_hint;

/// Substrings emitted by SQLite FTS5 when the caller's `MATCH` syntax itself is
/// invalid (as opposed to an external-content index desync / corruption error).
///
/// Mirrors Python `cache._BAD_MATCH_MARKERS`: classifying the safe-to-ignore
/// syntax errors is portable across SQLite versions, whereas index desync errors
/// vary between `fts5: missing row N from content table ...` (SQLite 3.42+) and
/// `database disk image is malformed` (older builds).
const BAD_MATCH_MARKERS: [&str; 5] = [
    "syntax error",
    "no such column",
    "unknown special query",
    "unterminated string",
    "fts5: parse error",
];

/// True when a SQLite error string indicates a malformed caller `MATCH` expression
/// (which must return `[]` without triggering an O(corpus) FTS5 rebuild).
pub fn is_bad_match_query(error_text: &str) -> bool {
    let lower = error_text.to_ascii_lowercase();
    BAD_MATCH_MARKERS.iter().any(|m| lower.contains(m))
}
