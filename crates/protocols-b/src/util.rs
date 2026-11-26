//! Shared helpers for `fsm-protocols-b` adapters: HTTP dispatch, charset & XML sniffing,
//! untrusted XML parsing, parameter validation, HTML stripping/entity decoding, dialect checks,
//! token matching, and URL utilities.

use encoding_rs::Encoding;
use fsm_core::dates::normalize_date;
use fsm_core::text::smart_truncate;
use fsm_core::{FsmError, Hit, HttpBytes, HttpRequest, NetClient, NetCx, SrcCx, TransportProfile};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use regex::{Regex, bytes::Regex as BytesRegex};
use roxmltree::{Document, ParsingOptions};
use serde_json::{Map, Value};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
use std::sync::LazyLock;
use url::Url;

/// Maximum characters preserved in an extracted feed/sitemap title (B1).
pub(crate) const MAX_TITLE_CHARS: usize = 300;
/// Maximum byte length allowed for an extracted feed/sitemap URL (B1).
pub(crate) const MAX_URL_BYTES: usize = 2_048;
/// Upper bound on XML tree nodes accepted by `roxmltree` (B1).
const MAX_XML_NODES: u32 = 2_000_000;

/// RFC 3986 unreserved set excluded from percent-encoding (`A-Z a-z 0-9 - . _ ~`).
const SEGMENT_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

static STYLE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<style\b[^>]*>.*?</style>").expect("style regex"));
static SCRIPT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<script\b[^>]*>.*?</script>").expect("script regex"));
static BLOCK_TAG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)</?(?:p|div|br|hr|li|ul|ol|tr|td|th|h[1-6]|blockquote|section|article|header|footer|pre|dd|dt|nav|aside|main)\b[^>]*>",
    )
    .expect("block tag regex")
});
/// Matches real HTML/XML comments and tags (`<tag ...>`, `</tag>`, `<br/>`), while preserving
/// non-tag angle brackets like `<10 µg/m3` or `>5` (I5).
static TAG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)<!--.*?-->|</?[A-Za-z][A-Za-z0-9:-]*(?:\s[^<>]*)?/?>").expect("tag regex")
});
static XML_ENCODING_RE: LazyLock<BytesRegex> = LazyLock::new(|| {
    BytesRegex::new(r#"(?i)^\s*<\?xml[^>]*encoding\s*=\s*["']([A-Za-z0-9._-]+)["']"#)
        .expect("xml encoding regex")
});

/// Percent-encode a single URL path segment (RFC 3986 unreserved preserved).
pub(crate) fn encode_segment(raw: &str) -> String {
    utf8_percent_encode(raw, SEGMENT_ENCODE_SET).to_string()
}

/// Cap a title string to [`MAX_TITLE_CHARS`] characters.
pub(crate) fn cap_title(title: &str) -> String {
    let trimmed = collapse_ws(title);
    if trimmed.is_empty() {
        return String::new();
    }
    smart_truncate(&trimmed, MAX_TITLE_CHARS).0
}

/// Validate a base URL via `net.check_url_offline` and parse it as `http(s)`.
/// Returns `(parsed_url, base_without_trailing_slash)`.
pub(crate) fn parse_base_url(
    net: &dyn NetClient,
    base_url: &str,
) -> Result<(Url, String), FsmError> {
    let trimmed = base_url.trim();
    if trimmed.is_empty() {
        return Err(FsmError::invalid("base_url must not be empty"));
    }
    net.check_url_offline(trimmed)?;
    let url = Url::parse(trimmed)
        .map_err(|e| FsmError::invalid(format!("invalid base_url {trimmed:?}: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(FsmError::invalid(format!(
            "base_url must be an http(s) URL with a host: {trimmed:?}"
        )));
    }
    let no_slash = trimmed.trim_end_matches('/').to_owned();
    Ok((url, no_slash))
}

/// Validate a path segment value against traversal and control characters.
pub(crate) fn validate_path_segment(name: &str, value: &str) -> Result<String, FsmError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(FsmError::invalid(format!("{name} must not be empty")));
    }
    if trimmed == "."
        || trimmed == ".."
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.contains('\0')
        || trimmed.contains('\r')
        || trimmed.contains('\n')
    {
        return Err(FsmError::invalid(format!(
            "invalid {name} {trimmed:?}: path separators and control characters are not allowed"
        )));
    }
    Ok(trimmed.to_owned())
}

/// Validate a relative subpath like Discourse category (`slug` or `parent/slug` or `slug/id`).
pub(crate) fn validate_subpath(name: &str, value: &str) -> Result<String, FsmError> {
    let trimmed = value.trim().trim_matches('/');
    if trimmed.is_empty() {
        return Err(FsmError::invalid(format!("{name} must not be empty")));
    }
    if trimmed.contains('\\')
        || trimmed.contains('\0')
        || trimmed.contains('\r')
        || trimmed.contains('\n')
        || trimmed.contains("://")
        || trimmed.starts_with("//")
        || trimmed.split('/').any(|seg| seg == "." || seg == "..")
    {
        return Err(FsmError::invalid(format!(
            "invalid {name} {trimmed:?}: traversal segments or control characters are not allowed"
        )));
    }
    Ok(trimmed.to_owned())
}

/// Parse HTTP `Retry-After` header value as either integer seconds or an RFC 2822 HTTP-date (M4).
pub(crate) fn parse_retry_after(raw: &str) -> Option<u64> {
    let trimmed = raw.trim();
    if let Ok(secs) = trimmed.parse::<u64>() {
        return Some(secs);
    }
    if let Ok(zoned) = jiff::fmt::rfc2822::parse(trimmed) {
        let delta = zoned.timestamp().as_second() - jiff::Timestamp::now().as_second();
        return Some(delta.max(0) as u64);
    }
    None
}

/// Send an HTTP request through `NetClient` with standard status/rate-limit handling.
pub(crate) async fn send_request(
    net: &dyn NetClient,
    req: HttpRequest,
    cx: &SrcCx,
    kind: &str,
) -> Result<HttpBytes, FsmError> {
    net.check_url_offline(&req.url)?;
    // N1: Leave `req.source_id` untouched so `fsm-net` keys rate-limit / Retry-After
    // buckets per target host instead of sharing one protocol-wide bucket across portals.
    let source_id = req.source_id.clone().unwrap_or_else(|| kind.to_owned());
    let ncx = NetCx::new(cx.cancel.clone(), cx.timeout);
    let resp = net.send(req, TransportProfile::Plain, &ncx).await?;
    if resp.status == 429 {
        let retry_after_s = resp.header("retry-after").and_then(parse_retry_after);
        return Err(FsmError::RateLimited {
            source_id,
            retry_after_s,
        });
    }
    if !resp.is_success() {
        let text = decode_body(&resp);
        let excerpt = collapse_ws(&text);
        let (short, _) = smart_truncate(&excerpt, 200);
        return Err(FsmError::upstream(
            source_id,
            Some(resp.status),
            if short.is_empty() {
                format!("HTTP {}", resp.status)
            } else {
                format!("HTTP {}: {short}", resp.status)
            },
        ));
    }
    Ok(resp)
}

/// Decode response bytes using `Content-Type` charset parameter if present, else BOM, else UTF-8.
pub(crate) fn decode_body(resp: &HttpBytes) -> String {
    decode_bytes_with_charset(&resp.body, resp.header("content-type"), false)
}

/// Decode XML response bytes with precedence: HTTP `charset=` > BOM > XML prolog `encoding=` > UTF-8 (I3, N4).
pub(crate) fn decode_xml_body(resp: &HttpBytes) -> String {
    decode_xml_bytes(&resp.body, resp.header("content-type"))
}

/// Decode raw XML bytes (e.g. post-gzip) using header charset, BOM, XML prolog encoding, or UTF-8 (I3, N4).
pub(crate) fn decode_xml_bytes(bytes: &[u8], content_type: Option<&str>) -> String {
    decode_bytes_with_charset(bytes, content_type, true)
}

fn decode_bytes_with_charset(
    bytes: &[u8],
    content_type: Option<&str>,
    sniff_xml_prolog: bool,
) -> String {
    // 1. Explicit HTTP Content-Type charset parameter
    if let Some(enc) = content_type
        .and_then(extract_charset)
        .and_then(|c| Encoding::for_label(c.as_bytes()))
    {
        let decoded = if enc == encoding_rs::UTF_8 {
            String::from_utf8_lossy(bytes).into_owned()
        } else {
            enc.decode(bytes).0.into_owned()
        };
        return decoded
            .strip_prefix('\u{feff}')
            .unwrap_or(&decoded)
            .to_owned();
    }

    // 2. Byte Order Mark (UTF-8, UTF-16LE, UTF-16BE)
    if let Some((enc, bom_len)) = Encoding::for_bom(bytes) {
        let slice = &bytes[bom_len..];
        let decoded = if enc == encoding_rs::UTF_8 {
            String::from_utf8_lossy(slice).into_owned()
        } else {
            enc.decode_without_bom_handling(slice).0.into_owned()
        };
        return decoded
            .strip_prefix('\u{feff}')
            .unwrap_or(&decoded)
            .to_owned();
    }

    // 3. XML prolog <?xml ... encoding="..."?> over first 1 KiB
    if sniff_xml_prolog {
        let head = &bytes[..bytes.len().min(1024)];
        if let Some(caps) = XML_ENCODING_RE.captures(head)
            && let Some(label) = caps.get(1)
            && let Some(enc) = Encoding::for_label(label.as_bytes())
        {
            // WHATWG prescan rule (N4): a prolog matched as ASCII cannot be UTF-16
            // (classic .NET StringWriter UTF-8 feed declaring encoding="utf-16").
            let effective_enc = if enc == encoding_rs::UTF_16LE || enc == encoding_rs::UTF_16BE {
                encoding_rs::UTF_8
            } else {
                enc
            };
            let decoded = if effective_enc == encoding_rs::UTF_8 {
                String::from_utf8_lossy(bytes).into_owned()
            } else {
                effective_enc.decode(bytes).0.into_owned()
            };
            return decoded
                .strip_prefix('\u{feff}')
                .unwrap_or(&decoded)
                .to_owned();
        }
    }

    // 4. UTF-8 fallback
    let decoded = String::from_utf8_lossy(bytes).into_owned();
    decoded
        .strip_prefix('\u{feff}')
        .unwrap_or(&decoded)
        .to_owned()
}

fn extract_charset(content_type: &str) -> Option<&str> {
    for part in content_type.split(';').skip(1) {
        if let Some((k, v)) = part.split_once('=')
            && k.trim().eq_ignore_ascii_case("charset")
        {
            let cleaned = v.trim().trim_matches('"').trim_matches('\'');
            if !cleaned.is_empty() {
                return Some(cleaned);
            }
        }
    }
    None
}

/// Safely parse untrusted XML (RSS/Atom feeds, sitemaps): refuses DTD `<!ENTITY` declarations
/// to prevent quadratic entity-expansion OOM attacks and bounds total XML nodes (B1, R3).
pub(crate) fn parse_untrusted_xml<'a>(
    xml: &'a str,
    kind: &str,
    status: u16,
) -> Result<Document<'a>, FsmError> {
    if xml
        .as_bytes()
        .windows(8)
        .any(|w| w.eq_ignore_ascii_case(b"<!entity"))
    {
        return Err(FsmError::upstream(
            kind,
            Some(status),
            "XML declares DTD entities; refused",
        ));
    }
    let opts = ParsingOptions {
        allow_dtd: true,
        nodes_limit: MAX_XML_NODES,
        ..ParsingOptions::default()
    };
    Document::parse_with_options(xml, opts)
        .map_err(|e| FsmError::upstream(kind, Some(status), format!("invalid XML: {e}")))
}

/// Parse response bytes as JSON, attributing parse failures to `kind`.
pub(crate) fn parse_json_body(resp: &HttpBytes, kind: &str) -> Result<Value, FsmError> {
    let text = decode_body(resp);
    serde_json::from_str(&text).map_err(|e| {
        FsmError::upstream(
            kind,
            Some(resp.status),
            format!("invalid JSON response: {e}"),
        )
    })
}

/// Extract the parameter map from a `Value` (`Null` is treated as `{}`).
pub(crate) fn params_map(params: &Value) -> Result<&Map<String, Value>, FsmError> {
    static EMPTY: LazyLock<Map<String, Value>> = LazyLock::new(Map::new);
    match params {
        Value::Object(m) => Ok(m),
        Value::Null => Ok(&EMPTY),
        _ => Err(FsmError::invalid("params must be a JSON object")),
    }
}

/// Required non-empty trimmed string parameter.
pub(crate) fn req_str(params: &Map<String, Value>, key: &str) -> Result<String, FsmError> {
    match params.get(key) {
        Some(Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() {
                Err(FsmError::invalid(format!(
                    "parameter {key:?} must not be empty"
                )))
            } else {
                Ok(t.to_owned())
            }
        }
        Some(_) => Err(FsmError::invalid(format!(
            "parameter {key:?} must be a string"
        ))),
        None => Err(FsmError::invalid(format!(
            "missing required parameter {key:?}"
        ))),
    }
}

/// Optional trimmed string parameter (`None` if omitted, `null`, or blank).
pub(crate) fn opt_str(params: &Map<String, Value>, key: &str) -> Result<Option<String>, FsmError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() {
                Ok(None)
            } else {
                Ok(Some(t.to_owned()))
            }
        }
        Some(_) => Err(FsmError::invalid(format!(
            "parameter {key:?} must be a string"
        ))),
    }
}

/// Required integer parameter with minimum bound.
pub(crate) fn req_int(params: &Map<String, Value>, key: &str, min: i64) -> Result<i64, FsmError> {
    match params.get(key) {
        Some(v) => {
            let n = as_exact_i64(v).ok_or_else(|| {
                FsmError::invalid(format!("parameter {key:?} must be an integer"))
            })?;
            if n < min {
                return Err(FsmError::invalid(format!(
                    "parameter {key:?} must be >= {min} (got {n})"
                )));
            }
            Ok(n)
        }
        None => Err(FsmError::invalid(format!(
            "missing required parameter {key:?}"
        ))),
    }
}

/// Optional integer parameter enforcing strict `min..=max` bounds (M1).
pub(crate) fn opt_int_range(
    params: &Map<String, Value>,
    key: &str,
    min: i64,
    max: i64,
) -> Result<Option<i64>, FsmError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let n = as_exact_i64(v).ok_or_else(|| {
                FsmError::invalid(format!("parameter {key:?} must be an integer"))
            })?;
            if n < min || n > max {
                return Err(FsmError::invalid(format!(
                    "parameter {key:?} must be in {min}..={max} (got {n})"
                )));
            }
            Ok(Some(n))
        }
    }
}

/// Optional limit parameter with default: rejects `< min`, clamps positive values above `max`.
pub(crate) fn opt_limit(
    params: &Map<String, Value>,
    default: usize,
    min: usize,
    max: usize,
) -> Result<usize, FsmError> {
    match params.get("limit") {
        None | Some(Value::Null) => Ok(default),
        Some(v) => {
            let n = as_exact_i64(v)
                .ok_or_else(|| FsmError::invalid("parameter \"limit\" must be an integer"))?;
            if n < min as i64 {
                return Err(FsmError::invalid(format!(
                    "parameter \"limit\" must be >= {min} (got {n})"
                )));
            }
            Ok((n.min(max as i64)) as usize)
        }
    }
}

fn as_exact_i64(v: &Value) -> Option<i64> {
    if let Some(n) = v.as_i64() {
        return Some(n);
    }
    if let Some(u) = v.as_u64() {
        return i64::try_from(u).ok();
    }
    None
}

/// Check that every key in `dialect` belongs to `allowed`.
pub(crate) fn check_allowed_dialect_keys(
    kind: &str,
    dialect: &BTreeMap<String, Value>,
    allowed: &[&str],
) -> Result<(), FsmError> {
    let unknown: Vec<&str> = dialect
        .keys()
        .map(String::as_str)
        .filter(|k| !allowed.contains(k))
        .collect();
    if !unknown.is_empty() {
        return Err(FsmError::invalid(format!(
            "{kind} does not accept dialect key(s) {} (allowed: {})",
            unknown.join(", "),
            allowed.join(", ")
        )));
    }
    Ok(())
}

/// Read an optional string key from `dialect`.
pub(crate) fn dialect_str<'a>(
    kind: &str,
    dialect: &'a BTreeMap<String, Value>,
    key: &str,
) -> Result<Option<&'a str>, FsmError> {
    match dialect.get(key) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(FsmError::invalid(format!(
            "{kind} dialect.{key} must be a string"
        ))),
    }
}

/// Read an optional bool key from `dialect`.
pub(crate) fn dialect_bool(
    kind: &str,
    dialect: &BTreeMap<String, Value>,
    key: &str,
) -> Result<Option<bool>, FsmError> {
    match dialect.get(key) {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(FsmError::invalid(format!(
            "{kind} dialect.{key} must be a boolean"
        ))),
    }
}

/// Read an optional integer key in `min..=max` from `dialect`.
pub(crate) fn dialect_int_range(
    kind: &str,
    dialect: &BTreeMap<String, Value>,
    key: &str,
    min: u64,
    max: u64,
) -> Result<Option<u64>, FsmError> {
    match dialect.get(key) {
        None => Ok(None),
        Some(v) => {
            let n = v.as_u64().ok_or_else(|| {
                FsmError::invalid(format!(
                    "{kind} dialect.{key} must be an integer in {min}..={max}"
                ))
            })?;
            if !(min..=max).contains(&n) {
                return Err(FsmError::invalid(format!(
                    "{kind} dialect.{key} must be in {min}..={max} (got {n})"
                )));
            }
            Ok(Some(n))
        }
    }
}

/// Resolve a relative or protocol-relative URL against `base_url`, allowing only http(s).
pub(crate) fn resolve_http_url(base_url: &str, href: &str) -> Option<String> {
    let trimmed = href.trim();
    if trimmed.is_empty() {
        return None;
    }
    let resolved = if let Ok(abs) = Url::parse(trimmed) {
        abs
    } else {
        let base = Url::parse(base_url).ok()?;
        base.join(trimmed).ok()?
    };
    if matches!(resolved.scheme(), "http" | "https") {
        Some(resolved.into())
    } else {
        None
    }
}

/// True when `target_url` shares the same registrable host (exact host, subdomain, or shared
/// eTLD+1) as `base_url` (M7).
pub(crate) fn same_registrable_host(base_url: &str, target_url: &str) -> bool {
    let Ok(base) = Url::parse(base_url) else {
        return false;
    };
    let Ok(target) = Url::parse(target_url) else {
        return false;
    };
    let (Some(bh), Some(th)) = (base.host_str(), target.host_str()) else {
        return false;
    };
    let bh = bh.trim_end_matches('.').to_ascii_lowercase();
    let th = th.trim_end_matches('.').to_ascii_lowercase();
    if bh == th {
        return true;
    }
    if bh.starts_with('[')
        || th.starts_with('[')
        || bh.parse::<std::net::IpAddr>().is_ok()
        || th.parse::<std::net::IpAddr>().is_ok()
    {
        return false;
    }
    registrable_domain(&bh) == registrable_domain(&th)
}

fn registrable_domain(host: &str) -> &str {
    const TWO_LABEL_TLDS: &[&str] = &[
        "co.uk", "org.uk", "gov.uk", "ac.uk", "co.jp", "ne.jp", "or.jp", "go.jp", "ac.jp",
        "com.au", "org.au", "gov.au", "edu.au", "co.nz", "co.in", "com.br", "com.cn", "org.cn",
        "gov.cn", "edu.cn", "com.tw", "org.tw", "gov.tw", "co.kr", "or.kr", "com.mx", "com.sg",
    ];
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() <= 2 {
        return host;
    }
    let tail2 = format!("{}.{}", labels[labels.len() - 2], labels[labels.len() - 1]);
    let take = if TWO_LABEL_TLDS.contains(&tail2.as_str()) {
        3
    } else {
        2
    };
    if labels.len() <= take {
        return host;
    }
    let skip = labels.len() - take;
    let offset: usize = labels[..skip].iter().map(|l| l.len() + 1).sum();
    &host[offset..]
}

/// Extract text from a JSON value that may be either a string or a multilingual map
/// (e.g. `{"en": "...", "de": "...", "fr": "..."}` as seen in CKAN `opendata.swiss`).
pub(crate) fn multilingual_text(val: Option<&Value>, prefer_lang: Option<&str>) -> Option<String> {
    let val = val?;
    match val {
        Value::String(s) => {
            let t = s.trim();
            (!t.is_empty()).then(|| t.to_owned())
        }
        Value::Object(map) => {
            if let Some(lang) = prefer_lang {
                let primary = lang
                    .split(['-', '_'])
                    .next()
                    .unwrap_or(lang)
                    .to_ascii_lowercase();
                for key in [lang.to_ascii_lowercase(), primary] {
                    if let Some(s) = map
                        .get(&key)
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        return Some(s.to_owned());
                    }
                }
            }
            if let Some(s) = map
                .get("en")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                return Some(s.to_owned());
            }
            map.values()
                .filter_map(Value::as_str)
                .map(str::trim)
                .find(|s| !s.is_empty())
                .map(str::to_owned)
        }
        _ => None,
    }
}

/// Collapse runs of Unicode whitespace to a single ASCII space and trim.
pub(crate) fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = true;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
        } else {
            out.push(ch);
            in_ws = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Decode standard HTML named entities and Unicode numeric character references.
pub(crate) fn unescape_html(input: &str) -> String {
    if !input.contains('&') {
        return input.to_owned();
    }
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'&'
            && let Some(rel_semi) = bytes[i + 1..bytes.len().min(i + 16)]
                .iter()
                .position(|&b| b == b';')
        {
            let entity = &input[i + 1..i + 1 + rel_semi];
            if let Some(decoded) = decode_entity(entity) {
                out.push_str(decoded.as_str());
                i += rel_semi + 2;
                continue;
            }
        }
        let ch = input[i..].chars().next().expect("valid utf-8 boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn decode_entity(entity: &str) -> Option<String> {
    if let Some(num) = entity.strip_prefix('#') {
        let code = if let Some(hex) = num.strip_prefix('x').or_else(|| num.strip_prefix('X')) {
            u32::from_str_radix(hex, 16).ok()?
        } else {
            num.parse::<u32>().ok()?
        };
        if code == 0 {
            return None;
        }
        let ch = char::from_u32(code)?;
        return Some(ch.to_string());
    }
    let s = match entity {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "nbsp" => " ",
        "ndash" => "\u{2013}",
        "mdash" => "\u{2014}",
        "lsquo" => "\u{2018}",
        "rsquo" => "\u{2019}",
        "ldquo" => "\u{201C}",
        "rdquo" => "\u{201D}",
        "hellip" => "\u{2026}",
        "copy" => "\u{00A9}",
        "reg" => "\u{00AE}",
        "trade" => "\u{2122}",
        "times" => "\u{00D7}",
        "divide" => "\u{00F7}",
        "plusmn" => "\u{00B1}",
        "deg" => "\u{00B0}",
        "micro" => "\u{00B5}",
        "laquo" => "\u{00AB}",
        "raquo" => "\u{00BB}",
        "bull" => "\u{2022}",
        "middot" => "\u{00B7}",
        "euro" => "\u{20AC}",
        "pound" => "\u{00A3}",
        "yen" => "\u{00A5}",
        "cent" => "\u{00A2}",
        "sect" => "\u{00A7}",
        "para" => "\u{00B6}",
        "shy" => "",
        "alpha" => "\u{03B1}",
        "beta" => "\u{03B2}",
        "gamma" => "\u{03B3}",
        "delta" => "\u{03B4}",
        "epsilon" => "\u{03B5}",
        "lambda" => "\u{03BB}",
        "mu" => "\u{03BC}",
        "pi" => "\u{03C0}",
        "sigma" => "\u{03C3}",
        "omega" => "\u{03C9}",
        _ => return None,
    };
    Some(s.to_owned())
}

/// Strip `<style>`/`<script>` blocks, strip real HTML/XML markup tags (inserting spaces for
/// block-level tags, preserving non-tag comparisons like `<10` / `>5`), decode entities,
/// and collapse whitespace (I5).
pub(crate) fn strip_tags(input: &str) -> String {
    let no_style = STYLE_RE.replace_all(input, " ");
    let no_script = SCRIPT_RE.replace_all(&no_style, " ");
    let spaced_blocks = BLOCK_TAG_RE.replace_all(&no_script, " ");
    let no_tags = TAG_RE.replace_all(&spaced_blocks, "");
    collapse_ws(&unescape_html(&no_tags))
}

/// Clean HTML/markup into a compact snippet bounded at `max_chars`.
pub(crate) fn clean_snippet(raw: &str, max_chars: usize) -> String {
    let stripped = strip_tags(raw);
    if stripped.is_empty() {
        return String::new();
    }
    let (cut, _) = smart_truncate(&stripped, max_chars);
    cut
}

/// Clean plain text (no tag stripping; decodes HTML entities, collapses whitespace, truncates) (I5).
pub(crate) fn clean_plain_text(raw: &str, max_chars: usize) -> String {
    let cleaned = collapse_ws(&unescape_html(raw));
    if cleaned.is_empty() {
        return String::new();
    }
    let (cut, _) = smart_truncate(&cleaned, max_chars);
    cut
}

/// Normalize a date string via `fsm_core::dates::normalize_date`.
pub(crate) fn norm_date(raw: Option<&str>) -> Option<String> {
    raw.and_then(|s| normalize_date(s.trim()))
}

/// True for CJK Unified Ideographs, Kana, and Hangul syllables.
pub(crate) fn is_cjk(ch: char) -> bool {
    matches!(
        ch as u32,
        0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xF900..=0xFAFF
            | 0x3040..=0x309F
            | 0x30A0..=0x30FF
            | 0xAC00..=0xD7AF
            | 0x20000..=0x2A6DF
    )
}

/// Tokenize a query into lowercase alphanumeric/CJK terms for client-side filtering (I6).
pub(crate) fn tokenize_terms(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_is_cjk = false;

    for ch in text.chars() {
        if is_cjk(ch) {
            if !cur.is_empty() && !cur_is_cjk {
                out.push(std::mem::take(&mut cur));
            }
            cur_is_cjk = true;
            cur.push(ch);
        } else if ch.is_alphanumeric() {
            if !cur.is_empty() && cur_is_cjk {
                out.push(std::mem::take(&mut cur));
            }
            cur_is_cjk = false;
            for lc in ch.to_lowercase() {
                cur.push(lc);
            }
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Extract lowercased alphanumeric tokens for word-boundary Latin matching (I6, N2).
pub(crate) fn word_set(text_lower: &str) -> HashSet<&str> {
    text_lower
        .split(|c: char| !c.is_alphanumeric() || is_cjk(c))
        .filter(|t| !t.is_empty())
        .collect()
}

fn normalize_plural(word: &str) -> Cow<'_, str> {
    if word.len() > 4
        && let Some(stem) = word.strip_suffix("ies")
    {
        return Cow::Owned(format!("{stem}y"));
    }
    if word.len() > 4
        && (word.ends_with("ches")
            || word.ends_with("shes")
            || word.ends_with("sses")
            || word.ends_with("xes")
            || word.ends_with("zes"))
    {
        return Cow::Borrowed(&word[..word.len() - 2]);
    }
    if word.len() > 3
        && word.ends_with('s')
        && !word.ends_with("ss")
        && !word.ends_with("us")
        && !word.ends_with("is")
    {
        return Cow::Borrowed(&word[..word.len() - 1]);
    }
    Cow::Borrowed(word)
}

fn token_matches_term(token: &str, term: &str) -> bool {
    if token == term {
        return true;
    }
    if term.len() >= 3 && token.starts_with(term) {
        return true;
    }
    let term_norm = normalize_plural(term);
    let token_norm = normalize_plural(token);
    token_norm == term_norm || (term_norm.len() >= 3 && token_norm.starts_with(term_norm.as_ref()))
}

/// Match a single query term against a lowercased haystack: substring containment for CJK terms,
/// and word-start exact / prefix (`len >= 3`) / singular-plural matching for Latin terms (I6, N2).
pub(crate) fn matches_term(haystack_lower: &str, tokens: &HashSet<&str>, term: &str) -> bool {
    if term.chars().any(is_cjk) {
        haystack_lower.contains(term)
    } else {
        tokens.contains(term) || tokens.iter().any(|tok| token_matches_term(tok, term))
    }
}

/// Construct a serialized [`Hit`] JSON object suitable for `ProtocolOutput.items`
/// on `ReturnKind::Hits` ops. Omits `"kind"` so `InstanceSource` injects `Record` or
/// `Academic` per A1 §D.3 (I2).
pub(crate) fn hit_to_value(
    title: String,
    url: String,
    snippet: String,
    date: Option<String>,
    date_source: Option<&str>,
    extra: Option<Value>,
) -> Value {
    let mut hit = Hit::new(title, url);
    hit.snippet = snippet;
    hit.date = date;
    hit.date_source = hit
        .date
        .as_ref()
        .and_then(|_| date_source.map(str::to_owned));
    hit.extra = extra;
    let mut val = serde_json::to_value(hit).expect("Hit always serializes");
    if let Value::Object(map) = &mut val {
        map.remove("kind");
    }
    val
}

/// Return a standard unknown-op error listing the adapter's supported ops.
pub(crate) fn unknown_op(kind: &str, op: &str, supported: &[&str]) -> FsmError {
    FsmError::invalid(format!(
        "protocol {kind:?} has no op {op:?}; supported ops: {}",
        supported.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_tags_distinguishes_inline_and_block_and_removes_style_script() {
        assert_eq!(
            strip_tags("<p>Short <em>summary</em>.</p>"),
            "Short summary."
        );
        assert_eq!(strip_tags("<p>First</p><p>Second</p>"), "First Second");
        assert_eq!(
            strip_tags(
                "<style>.x { color: red; }</style><script>var a=1;</script><div>Hello &amp; &#8211; &#x2019; world</div>"
            ),
            "Hello & \u{2013} \u{2019} world"
        );
        assert_eq!(
            strip_tags("Days with PM2.5 <10 µg/m3 and >5 µg/m3 are counted."),
            "Days with PM2.5 <10 µg/m3 and >5 µg/m3 are counted."
        );
        assert_eq!(
            unescape_html("Keep &unknown; and &#0; intact"),
            "Keep &unknown; and &#0; intact"
        );
        assert_eq!(encode_segment("a/b c#d~1"), "a%2Fb%20c%23d~1");
    }

    #[test]
    fn multilingual_text_prefers_requested_lang_then_en_then_first() {
        let obj = serde_json::json!({"de": "Hallo", "en": "Hello", "fr": "Bonjour"});
        assert_eq!(
            multilingual_text(Some(&obj), Some("fr-FR")).as_deref(),
            Some("Bonjour")
        );
        assert_eq!(
            multilingual_text(Some(&obj), Some("ja")).as_deref(),
            Some("Hello")
        );
        let no_en = serde_json::json!({"de": "Hallo", "en": ""});
        assert_eq!(
            multilingual_text(Some(&no_en), None).as_deref(),
            Some("Hallo")
        );
    }

    #[test]
    fn decode_xml_sniffs_iso8859_gbk_utf16_bom_and_ignores_misleading_utf16_prolog() {
        // ISO-8859-1 prolog without header charset
        let mut latin = br#"<?xml version="1.0" encoding="ISO-8859-1"?><rss><title>Caf"#.to_vec();
        latin.push(0xe9);
        latin.extend_from_slice(b" r");
        latin.push(0xe9);
        latin.extend_from_slice(b"sum");
        latin.push(0xe9);
        latin.extend_from_slice(b"</title></rss>");
        assert!(decode_xml_bytes(&latin, Some("application/rss+xml")).contains("Café résumé"));

        // GBK prolog without header charset ("中文" = D6 D0 CE C4)
        let mut gbk = br#"<?xml version="1.0" encoding="GBK"?><rss><title>"#.to_vec();
        gbk.extend_from_slice(&[0xD6, 0xD0, 0xCE, 0xC4]);
        gbk.extend_from_slice(b"</title></rss>");
        assert!(decode_xml_bytes(&gbk, Some("text/xml")).contains("中文"));

        // UTF-16LE BOM without header charset
        let utf16_str = "<?xml version=\"1.0\"?><rss><title>Café</title></rss>";
        let mut utf16 = vec![0xFF, 0xFE];
        for unit in utf16_str.encode_utf16() {
            utf16.extend_from_slice(&unit.to_le_bytes());
        }
        assert!(decode_xml_bytes(&utf16, Some("text/xml")).contains("Café"));

        // UTF-8 stream declaring encoding="utf-16" (.NET StringWriter pitfall, N4)
        let misleading =
            br#"<?xml version="1.0" encoding="utf-16"?><rss><title>UTF8 OK</title></rss>"#;
        assert!(decode_xml_bytes(misleading, Some("application/rss+xml")).contains("UTF8 OK"));
    }

    #[test]
    fn matches_term_handles_inflections_prefixes_and_blocks_short_substrings() {
        let check = |hay: &str, term: &str| {
            let low = hay.to_lowercase();
            let ws = word_set(&low);
            matches_term(&low, &ws, term)
        };
        assert!(check("validators guide", "validator"));
        assert!(check("dependencies overview", "dependency"));
        assert!(check("alerting rules", "alert"));
        assert!(check("authentication cheat sheet", "auth"));
        assert!(check("password storage", "passwords"));
        assert!(check("cloudflare queues", "queue"));
        assert!(!check("brain training said to help", "ai"));
        assert!(check("open ai systems", "ai"));
        assert!(check("量子计算新突破", "量子"));
    }

    #[test]
    fn retry_after_parses_seconds_and_http_date() {
        assert_eq!(parse_retry_after("45"), Some(45));
        // Far-past RFC 2822 HTTP-date clamps to 0; far-future HTTP-date yields > 0
        assert_eq!(parse_retry_after("Thu, 01 Oct 2020 10:00:00 GMT"), Some(0));
        assert!(parse_retry_after("Mon, 01 Jan 2035 00:00:00 GMT").is_some_and(|s| s > 0));
    }

    #[test]
    fn registrable_host_allows_subdomains_and_blocks_foreign_hosts() {
        assert!(same_registrable_host(
            "https://mastodon.social",
            "https://mastodon.social/nodeinfo/2.0"
        ));
        assert!(same_registrable_host(
            "https://social.example.org",
            "https://api.example.org/nodeinfo"
        ));
        assert!(!same_registrable_host(
            "https://mastodon.social",
            "https://evil.example.net/nodeinfo"
        ));
        assert!(!same_registrable_host(
            "https://a.co.uk",
            "https://b.co.uk/nodeinfo"
        ));
        assert!(!same_registrable_host(
            "https://198.51.100.1",
            "https://198.51.100.2/nodeinfo"
        ));
    }
}
