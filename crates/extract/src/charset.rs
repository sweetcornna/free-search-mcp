//! Charset detection and body decoding (`chardetng` + `encoding_rs`).
//!
//! Precedence (matching Python `httpfetch._decode_body` + `chardetng` fallback):
//! 1. Byte Order Mark (UTF-8 / UTF-16LE / UTF-16BE) via `encoding_rs::Encoding::for_bom`.
//! 2. `Content-Type` header `charset=...` parameter via `Encoding::for_label`.
//! 3. HTML `<meta ... charset=...>` or XML `<?xml ... encoding="..."?>` sniffed from the first 4 KiB
//!    (when `Content-Type` is empty or HTML/XML).
//! 4. Fast-path UTF-8 if the bytes are already valid UTF-8.
//! 5. Statistical detection via `chardetng::EncodingDetector` (hinted by URL TLD when available).
//!
//! Unknown or bogus charset labels never error or panic: they fall through to UTF-8 / `chardetng`.

use chardetng::{EncodingDetector, Iso2022JpDetection, Utf8Detection};
use encoding_rs::Encoding;
use regex::Regex;
use regex::bytes::Regex as BytesRegex;
use std::sync::LazyLock;
use url::Url;

static CTYPE_CHARSET_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)charset\s*=\s*["']?([\w\-]+)"#).expect("charset regex"));

static META_CHARSET_RE: LazyLock<BytesRegex> = LazyLock::new(|| {
    BytesRegex::new(r#"(?i)<meta[^>]+charset\s*=\s*["']?\s*([a-zA-Z0-9_\-]+)"#)
        .expect("meta charset regex")
});

static XML_ENCODING_RE: LazyLock<BytesRegex> = LazyLock::new(|| {
    BytesRegex::new(r#"(?i)<\?xml[^>]+encoding\s*=\s*["']\s*([a-zA-Z0-9_\-]+)\s*["']"#)
        .expect("xml encoding regex")
});

/// Extract a declared charset label from a `Content-Type` header, lowercased.
pub fn charset_from_ctype(ctype: &str) -> Option<String> {
    CTYPE_CHARSET_RE
        .captures(ctype)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_ascii_lowercase())
}

/// Sniff a declared charset label from an HTML `<meta>` tag or XML declaration in the first 4 KiB.
pub fn sniff_meta_charset(body: &[u8]) -> Option<String> {
    let head = &body[..body.len().min(4096)];
    if let Some(c) = META_CHARSET_RE.captures(head)
        && let Some(m) = c.get(1)
        && let Ok(s) = std::str::from_utf8(m.as_bytes())
    {
        return Some(s.to_ascii_lowercase());
    }
    if let Some(c) = XML_ENCODING_RE.captures(head)
        && let Some(m) = c.get(1)
        && let Ok(s) = std::str::from_utf8(m.as_bytes())
    {
        return Some(s.to_ascii_lowercase());
    }
    None
}

/// Extract the lowercase ASCII TLD label from a URL for `chardetng` hinting.
fn tld_from_url(url: Option<&str>) -> Option<String> {
    let parsed = Url::parse(url?).ok()?;
    let host = parsed.host_str()?;
    // Skip IP literals.
    if host.parse::<std::net::IpAddr>().is_ok() || host.starts_with('[') {
        return None;
    }
    let label = host.trim_end_matches('.').rsplit('.').next()?;
    if label.is_empty()
        || !label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return None;
    }
    Some(label.to_owned())
}

/// Decode a response body into a UTF-8 `String` using declared/sniffed/detected charset.
///
/// Never fails on invalid bytes or bogus charset labels.
pub fn decode_body(body: &[u8], content_type: &str, url: Option<&str>) -> String {
    if body.is_empty() {
        return String::new();
    }

    // 1. BOM takes precedence over declarations (WHATWG Encoding Standard).
    if let Some((enc, bom_len)) = Encoding::for_bom(body) {
        let (cow, _) = enc.decode_without_bom_handling(&body[bom_len..]);
        return cow.into_owned();
    }

    // 2. Content-Type header charset.
    if let Some(label) = charset_from_ctype(content_type)
        && let Some(enc) = Encoding::for_label(label.as_bytes())
    {
        let (cow, _) = enc.decode_without_bom_handling(body);
        return cow.into_owned();
    }

    // 3. HTML <meta> / XML <?xml encoding> sniff.
    let ctype_lower = content_type.to_ascii_lowercase();
    if (ctype_lower.is_empty() || ctype_lower.contains("html") || ctype_lower.contains("xml"))
        && let Some(label) = sniff_meta_charset(body)
        && let Some(enc) = Encoding::for_label(label.as_bytes())
    {
        let (cow, _) = enc.decode_without_bom_handling(body);
        return cow.into_owned();
    }

    // 4. Valid UTF-8 fast path.
    if let Ok(s) = std::str::from_utf8(body) {
        return s.to_owned();
    }

    // 5. Statistical detection via chardetng (sampled up to 64 KiB for speed on huge pages).
    let mut detector = EncodingDetector::new(Iso2022JpDetection::Deny);
    let sample_limit = 64 * 1024;
    let is_last = body.len() <= sample_limit;
    detector.feed(&body[..body.len().min(sample_limit)], is_last);
    if !is_last {
        detector.feed(&[], true);
    }
    let tld = tld_from_url(url);
    let enc = detector.guess(tld.as_deref().map(str::as_bytes), Utf8Detection::Allow);
    let (cow, _) = enc.decode_without_bom_handling(body);
    cow.into_owned()
}
