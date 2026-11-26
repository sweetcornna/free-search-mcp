//! Compression, JSON serialization, timestamp formatting, and scholarly PID normalization.
//!
//! `fsm_core::types` structs are JSON-only (they use `skip_serializing_if` and
//! `#[serde(flatten)]`), so all persisted wire types are encoded with `serde_json`
//! and compressed with zstd (`D/handoff/w0a.md`).

use fsm_core::{FetchMethod, FsmError};
use jiff::Timestamp;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::io::Read;
use std::time::{SystemTime, UNIX_EPOCH};

/// Zstd compression level (3 = zstd default, fast with strong JSON compression).
const ZSTD_LEVEL: i32 = 3;

/// Maximum decompressed payload for `search_cache` / `scholar_works` JSON blobs (16 MiB).
pub const MAX_JSON_BLOB_BYTES: usize = 16 * 1024 * 1024;

/// Maximum decompressed payload for `api_cache` raw HTTP bodies (64 MiB).
pub const MAX_API_BLOB_BYTES: usize = 64 * 1024 * 1024;

/// Current Unix timestamp in whole seconds.
pub fn now_unix_s() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Current Unix timestamp in whole milliseconds.
pub fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

/// Format a Unix epoch second timestamp as UTC RFC 3339 (`YYYY-MM-DDTHH:MM:SSZ`).
pub fn format_epoch_iso8601(epoch_s: i64) -> String {
    Timestamp::from_second(epoch_s)
        .map(|ts| ts.to_string())
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

/// Compress raw bytes with zstd.
pub fn zstd_compress(raw: &[u8]) -> Result<Vec<u8>, FsmError> {
    zstd::bulk::compress(raw, ZSTD_LEVEL)
        .map_err(|e| FsmError::internal("store_zstd_compress", e.to_string()))
}

/// Decompress zstd bytes with a strict upper bound to guard against corrupt/bomb blobs.
pub fn zstd_decompress_bounded(compressed: &[u8], max_bytes: usize) -> Result<Vec<u8>, FsmError> {
    let decoder = zstd::stream::Decoder::new(compressed)
        .map_err(|e| FsmError::internal("store_zstd_decompress", e.to_string()))?;
    let limit = (max_bytes as u64).saturating_add(1);
    let mut out = Vec::new();
    decoder
        .take(limit)
        .read_to_end(&mut out)
        .map_err(|e| FsmError::internal("store_zstd_decompress", e.to_string()))?;
    if out.len() > max_bytes {
        return Err(FsmError::internal(
            "store_zstd_limit",
            format!("decompressed blob exceeded {max_bytes} byte safety cap"),
        ));
    }
    Ok(out)
}

/// Serialize `value` as compact JSON and compress with zstd.
pub fn encode_json_zstd<T: Serialize>(value: &T) -> Result<(Vec<u8>, usize), FsmError> {
    let raw = serde_json::to_vec(value)
        .map_err(|e| FsmError::internal("store_json_encode", e.to_string()))?;
    let raw_len = raw.len();
    let compressed = zstd_compress(&raw)?;
    Ok((compressed, raw_len))
}

/// Decompress a bounded zstd blob and deserialize JSON into `T`.
pub fn decode_json_zstd<T: DeserializeOwned>(
    compressed: &[u8],
    max_bytes: usize,
) -> Result<T, FsmError> {
    let raw = zstd_decompress_bounded(compressed, max_bytes)?;
    serde_json::from_slice(&raw).map_err(|e| FsmError::internal("store_json_decode", e.to_string()))
}

/// Additive metadata stored in `pages.meta_json` for lossless `PageResult` round-trips.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PageMetaJson {
    pub total_chars: usize,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub retrieved_at: String,
}

/// Map `FetchMethod` to its stable lowercase DB tag.
pub fn fetch_method_to_str(method: FetchMethod) -> &'static str {
    match method {
        FetchMethod::Http => "http",
        FetchMethod::Browser => "browser",
        FetchMethod::Cache => "cache",
        FetchMethod::Document => "document",
        FetchMethod::Asset => "asset",
        _ => "http",
    }
}

/// Parse a stored DB tag back into `FetchMethod`.
pub fn fetch_method_from_str(s: &str) -> FetchMethod {
    match s {
        "http" => FetchMethod::Http,
        "browser" => FetchMethod::Browser,
        "cache" => FetchMethod::Cache,
        "document" => FetchMethod::Document,
        "asset" => FetchMethod::Asset,
        _ => FetchMethod::Http,
    }
}

/// Normalize a DOI via `fsm_core::ext::w0c::normalize_doi`.
pub fn normalize_doi(raw: &str) -> Option<String> {
    fsm_core::ext::w0c::normalize_doi(raw)
}

/// Normalize an arXiv identifier (`arXiv:1706.03762v5`, `10.48550/arXiv.1706.03762`,
/// `https://arxiv.org/abs/1706.03762v2`, or legacy `hep-th/9901001v1`) to bare
/// version-stripped lowercase ID (`1706.03762`, `hep-th/9901001`).
pub fn normalize_arxiv(raw: &str) -> Option<String> {
    use fsm_core::ext::w0c::strip_prefix_ascii_ci;

    let mut s = raw.trim();
    if s.is_empty() {
        return None;
    }
    // Strip resolver URL prefixes safely without slicing mid-codepoint.
    for prefix in [
        "https://arxiv.org/abs/",
        "http://arxiv.org/abs/",
        "https://arxiv.org/pdf/",
        "http://arxiv.org/pdf/",
        "https://doi.org/10.48550/arxiv.",
        "http://doi.org/10.48550/arxiv.",
    ] {
        if let Some(rest) = strip_prefix_ascii_ci(s, prefix) {
            s = rest;
            break;
        }
    }
    if let Some(rest) = strip_prefix_ascii_ci(s, "10.48550/arxiv.") {
        s = rest;
    }
    if let Some(rest) = strip_prefix_ascii_ci(s, "arxiv:") {
        s = rest.trim_start();
    }
    if let Some(stripped) = s.strip_suffix(".pdf") {
        s = stripped;
    }
    let lower = s.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return None;
    }
    // Strip trailing `v<digits>` version suffix without truncating legacy IDs like `solv-int/9901001v1`.
    let base = strip_arxiv_version(&lower);
    if base.is_empty() {
        None
    } else {
        Some(base.to_owned())
    }
}

fn strip_arxiv_version(s: &str) -> &str {
    if let Some(pos) = s.rfind('v') {
        let suffix = &s[pos + 1..];
        if !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()) {
            return &s[..pos];
        }
    }
    s
}

/// Normalize a PubMed ID (`PMID:25970246` -> `25970246`).
pub fn normalize_pmid(raw: &str) -> Option<String> {
    let mut s = raw.trim();
    if let Some(rest) = fsm_core::ext::w0c::strip_prefix_ascii_ci(s, "pmid:") {
        s = rest.trim_start();
    }
    if s.is_empty() {
        None
    } else {
        Some(s.to_owned())
    }
}

/// Normalize an OpenAlex work ID (`https://openalex.org/W2100837269` -> `W2100837269`).
pub fn normalize_openalex(raw: &str) -> Option<String> {
    let mut s = raw.trim();
    for prefix in ["https://openalex.org/", "http://openalex.org/", "openalex:"] {
        if let Some(rest) = fsm_core::ext::w0c::strip_prefix_ascii_ci(s, prefix) {
            s = rest;
            break;
        }
    }
    let s = s.trim().to_ascii_uppercase();
    if s.is_empty() { None } else { Some(s) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn normalizers_never_panic_on_cjk_latin_or_fullwidth_inputs() {
        for sample in [
            "深度学习",
            "深度学习的可解释性",
            "注意力机制",
            "Café society",
            "DOI：10.1000/xyz",
            "10.1000/abcdef日本",
            "PMID：12345678",
            "abcd日",
            "abcdé",
            "abcdefghé",
            "arXiv：1706.03762",
            "https://openalex.org/日本",
            "😀🎉🚀✨🔥",
        ] {
            let _ = normalize_doi(sample);
            let _ = normalize_arxiv(sample);
            let _ = normalize_pmid(sample);
            let _ = normalize_openalex(sample);
        }
    }

    proptest! {
        #[test]
        fn prop_pid_normalizers_never_panic_on_arbitrary_unicode(s in "\\PC{0,64}") {
            let _ = normalize_doi(&s);
            let _ = normalize_arxiv(&s);
            let _ = normalize_pmid(&s);
            let _ = normalize_openalex(&s);
        }
    }
}
