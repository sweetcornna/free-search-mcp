//! Shared HTTP, XML, HTML, date, DOI, parameter, and dialect utilities for `fsm-protocols-a`.

use fsm_core::{
    AuthorRef, FsmError, Hit, HttpBytes, HttpRequest, NetCx, OaLocation, ScholarlyWork, SourceDeps,
    SrcCx, TransportProfile,
};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use regex::Regex;
use roxmltree::{Document, Node, ParsingOptions};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::sync::LazyLock;
use std::time::Duration;
use url::Url;

/// RFC 3986 unreserved characters (`A-Z`, `a-z`, `0-9`, `-`, `.`, `_`, `~`) stay unencoded.
pub const PATH_SEGMENT_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

static TAG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]*>").expect("regex"));
static WS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").expect("regex"));
static DOI_PREFIX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:https?://(?:dx\.)?doi\.org/|doi:\s*)").expect("regex"));
static DOI_VALID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^10\.\d{4,9}/\S+$").expect("regex"));
static YEAR_PREFIX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d{4})(?:\b|[-/.T ])").expect("regex"));
static DOT_YMD_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d{4})\.(\d{1,2})\.(\d{1,2})\b").expect("regex"));
static SLASH_YM_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d{4})/(\d{1,2})$").expect("regex"));
static YMD_MON_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(\d{4})\s+([a-z]{3,9})\.?(?:\s+(\d{1,2}))?$").expect("regex")
});
static DMY_TZ_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:[a-z]{3,9},?\s+)?(\d{1,2})\s+([a-z]{3,9})\.?\s+(\d{4})(?:\s+.*)?$")
        .expect("regex")
});
static CATALOG_YEAR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(?:\[|c\.?\s*|ca\.?\s*|©\s*)*((?:1[0-9]|2[0-9])\d{2})(?:\s*[-–]\s*(?:(?:\d{4})?|\?))?[.\]?,\s]*$",
    )
    .expect("regex")
});
static VENUE_ID_PREFIX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:e?issn|isbn|issn-l)\s*[:\s]").expect("regex"));
static OA_CC_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:creativecommons|\bcc(?:0|[\s_-]?by|\s+zero|\s+public)|\bopen\s*access\b|\bopen\.access\b|\bopenaccess\b)",
    )
    .expect("regex")
});
static OA_DENY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:restricted|embargo\w*|closed\s*access|closedaccess)\b").expect("regex")
});

/// Returns `true` when rights/access metadata explicitly indicates restricted,
/// embargoed, or closed access (without false-matching `"unrestricted"`).
pub(crate) fn is_oa_denied(s: &str) -> bool {
    OA_DENY_RE.is_match(s)
}
static CHAR_REF_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"&(#x[0-9a-fA-F]+|#[0-9]+|[a-zA-Z][a-zA-Z0-9]+);").expect("regex")
});

static EMPTY_MAP: LazyLock<Map<String, Value>> = LazyLock::new(Map::new);

/// Validate that `base_url` is an http(s) URL.
pub fn validate_base_url(base_url: &str) -> Result<Url, FsmError> {
    let trimmed = base_url.trim();
    let parsed = Url::parse(trimmed)
        .map_err(|e| FsmError::invalid(format!("invalid base_url {base_url:?}: {e}")))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(FsmError::invalid(format!(
            "base_url {base_url:?} must be an http(s) URL with a host"
        )));
    }
    Ok(parsed)
}

pub fn trim_trailing_slash(url: &str) -> &str {
    url.trim().trim_end_matches('/')
}

/// Encode a single path segment strictly per A1 §B.3 (`encode = "segment"`).
pub fn encode_path_segment(value: &str, param_name: &str) -> Result<String, FsmError> {
    let v = value.trim();
    if v.is_empty() {
        return Err(FsmError::invalid(format!(
            "parameter {param_name:?} must not be empty"
        )));
    }
    if v.bytes().any(|b| matches!(b, 0 | b'\r' | b'\n' | b'\\'))
        || v == "."
        || v == ".."
        || v.contains('/')
    {
        return Err(FsmError::invalid(format!(
            "parameter {param_name:?} contains forbidden path characters"
        )));
    }
    Ok(utf8_percent_encode(v, PATH_SEGMENT_SET).to_string())
}

/// Decode HTTP response bytes respecting any `charset=` parameter in `Content-Type`.
pub fn decode_body(resp: &HttpBytes) -> String {
    let charset = resp.header("content-type").and_then(extract_charset);
    let encoding = charset
        .and_then(|label| encoding_rs::Encoding::for_label(label.as_bytes()))
        .unwrap_or(encoding_rs::UTF_8);
    let (decoded, _, _) = encoding.decode(&resp.body);
    decoded.into_owned()
}

fn extract_charset(content_type: &str) -> Option<&str> {
    for part in content_type.split(';').skip(1) {
        if let Some((k, v)) = part.split_once('=')
            && k.trim().eq_ignore_ascii_case("charset")
        {
            let val = v.trim().trim_matches('"').trim_matches('\'');
            if !val.is_empty() {
                return Some(val);
            }
        }
    }
    None
}

fn parse_retry_after(resp: &HttpBytes) -> Option<u64> {
    resp.header("retry-after")
        .and_then(|v| v.trim().parse::<u64>().ok())
}

/// Send an HTTP request via `deps.net`, enforcing offline URL check and mapping
/// 429 / non-2xx statuses to `FsmError`.
pub async fn send_checked(
    deps: &SourceDeps,
    req: HttpRequest,
    cx: &SrcCx,
    kind: &str,
) -> Result<HttpBytes, FsmError> {
    deps.net.check_url_offline(&req.url)?;
    let timeout = if cx.timeout.is_zero() {
        Duration::from_secs(10)
    } else {
        cx.timeout
    };
    let ncx = NetCx::new(cx.cancel.clone(), timeout);
    let resp = deps.net.send(req, TransportProfile::Plain, &ncx).await?;
    if resp.status == 429 {
        return Err(FsmError::RateLimited {
            source_id: kind.to_owned(),
            retry_after_s: parse_retry_after(&resp),
        });
    }
    if !resp.is_success() {
        let preview = clip(&decode_body(&resp), 200);
        return Err(FsmError::upstream(
            kind,
            Some(resp.status),
            if preview.is_empty() {
                format!("HTTP {}", resp.status)
            } else {
                format!("HTTP {}: {preview}", resp.status)
            },
        ));
    }
    Ok(resp)
}

/// Decode and parse a JSON response body.
pub fn parse_json(resp: &HttpBytes, kind: &str) -> Result<Value, FsmError> {
    let text = decode_body(resp);
    serde_json::from_str(&text).map_err(|e| {
        let preview = clip(&text, 160);
        FsmError::upstream(
            kind,
            Some(resp.status),
            format!("invalid JSON ({e}): {preview}"),
        )
    })
}

/// Hard XML safety limits enforced on all untrusted upstream XML (Ruling R3).
pub const MAX_XML_DOC_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_XML_NODES: u32 = 200_000;
pub const MAX_XML_TEXT_BYTES: usize = 1_048_576;
pub const MAX_XML_ATTR_BYTES: usize = 65_536;

/// Strip BOM / leading whitespace and parse an untrusted upstream XML document
/// per Ruling R3: refuse `<!ENTITY` declarations, forbid DTD entity expansion,
/// cap total size & node count, and cap individual text and attribute lengths.
pub fn parse_xml<'a>(body: &'a str, kind: &str, status: u16) -> Result<Document<'a>, FsmError> {
    let cleaned = body.trim_start_matches('\u{feff}').trim_start();
    if cleaned.len() > MAX_XML_DOC_BYTES {
        return Err(FsmError::upstream(
            kind,
            Some(status),
            format!("XML document exceeds {MAX_XML_DOC_BYTES} bytes"),
        ));
    }
    if cleaned
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
        allow_dtd: false,
        nodes_limit: MAX_XML_NODES,
        ..ParsingOptions::default()
    };
    let doc = Document::parse_with_options(cleaned, opts).map_err(|e| {
        let preview = clip(cleaned, 160);
        FsmError::upstream(kind, Some(status), format!("invalid XML ({e}): {preview}"))
    })?;
    for node in doc.descendants() {
        if let Some(t) = node.text()
            && t.len() > MAX_XML_TEXT_BYTES
        {
            return Err(FsmError::upstream(
                kind,
                Some(status),
                format!("XML text node exceeds {MAX_XML_TEXT_BYTES} bytes"),
            ));
        }
        for attr in node.attributes() {
            if attr.value().len() > MAX_XML_ATTR_BYTES {
                return Err(FsmError::upstream(
                    kind,
                    Some(status),
                    format!("XML attribute value exceeds {MAX_XML_ATTR_BYTES} bytes"),
                ));
            }
        }
    }
    Ok(doc)
}

// ---------------------------------------------------------------------------
// Parameter extraction
// ---------------------------------------------------------------------------

pub fn params_map(params: &Value) -> Result<&Map<String, Value>, FsmError> {
    match params {
        Value::Object(map) => Ok(map),
        Value::Null => Ok(&EMPTY_MAP),
        other => Err(FsmError::invalid(format!(
            "params must be a JSON object, got {other}"
        ))),
    }
}

pub fn req_str(map: &Map<String, Value>, key: &str) -> Result<String, FsmError> {
    match map.get(key) {
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(s.trim().to_owned()),
        Some(Value::String(_)) | None | Some(Value::Null) => Err(FsmError::invalid(format!(
            "missing required string parameter {key:?}"
        ))),
        Some(_) => Err(FsmError::invalid(format!(
            "parameter {key:?} must be a string"
        ))),
    }
}

pub fn opt_str(map: &Map<String, Value>, key: &str) -> Result<Option<String>, FsmError> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed.to_owned()))
            }
        }
        Some(_) => Err(FsmError::invalid(format!(
            "parameter {key:?} must be a string"
        ))),
    }
}

pub fn opt_str_aliases(
    map: &Map<String, Value>,
    keys: &[&str],
) -> Result<Option<String>, FsmError> {
    for &k in keys {
        if let Some(v) = opt_str(map, k)? {
            return Ok(Some(v));
        }
    }
    Ok(None)
}

pub fn opt_int(
    map: &Map<String, Value>,
    key: &str,
    min: i64,
    max: i64,
) -> Result<Option<i64>, FsmError> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let n = v
                .as_i64()
                .or_else(|| v.as_u64().and_then(|u| i64::try_from(u).ok()))
                .ok_or_else(|| {
                    FsmError::invalid(format!("parameter {key:?} must be an integer"))
                })?;
            if n < min || n > max {
                return Err(FsmError::invalid(format!(
                    "parameter {key:?} must be between {min} and {max}, got {n}"
                )));
            }
            Ok(Some(n))
        }
    }
}

/// Extract an optional `limit` parameter, rejecting `< min` and clamping `> max`
/// (Python oracle & `protocols-b` `opt_limit` parity).
pub fn opt_limit(
    map: &Map<String, Value>,
    key: &str,
    default: usize,
    min: usize,
    max: usize,
) -> Result<usize, FsmError> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => {
            let n = v
                .as_i64()
                .or_else(|| v.as_u64().and_then(|u| i64::try_from(u).ok()))
                .ok_or_else(|| {
                    FsmError::invalid(format!("parameter {key:?} must be an integer"))
                })?;
            if n < min as i64 {
                return Err(FsmError::invalid(format!(
                    "parameter {key:?} must be at least {min}, got {n}"
                )));
            }
            Ok((n as usize).min(max))
        }
    }
}

// ---------------------------------------------------------------------------
// Dialect validation & extraction
// ---------------------------------------------------------------------------

pub fn reject_unknown_dialect_keys(
    kind: &str,
    dialect: &BTreeMap<String, Value>,
    allowed: &[&str],
) -> Result<(), FsmError> {
    let bad: Vec<&str> = dialect
        .keys()
        .map(String::as_str)
        .filter(|k| !allowed.contains(k))
        .collect();
    if !bad.is_empty() {
        return Err(FsmError::invalid(format!(
            "{kind}: unknown dialect key(s): {} (allowed: {})",
            bad.join(", "),
            allowed.join(", ")
        )));
    }
    Ok(())
}

pub fn dialect_opt_str(
    kind: &str,
    dialect: &BTreeMap<String, Value>,
    key: &str,
) -> Result<Option<String>, FsmError> {
    match dialect.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Err(FsmError::invalid(format!(
                    "{kind}: dialect.{key} must be a non-empty string"
                )))
            } else {
                Ok(Some(trimmed.to_owned()))
            }
        }
        Some(_) => Err(FsmError::invalid(format!(
            "{kind}: dialect.{key} must be a string"
        ))),
    }
}

pub fn dialect_opt_bool(
    kind: &str,
    dialect: &BTreeMap<String, Value>,
    key: &str,
) -> Result<Option<bool>, FsmError> {
    match dialect.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(FsmError::invalid(format!(
            "{kind}: dialect.{key} must be a boolean"
        ))),
    }
}

pub fn dialect_opt_i64(
    kind: &str,
    dialect: &BTreeMap<String, Value>,
    key: &str,
    min: i64,
    max: i64,
) -> Result<Option<i64>, FsmError> {
    match dialect.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let n = v
                .as_i64()
                .or_else(|| v.as_u64().and_then(|u| i64::try_from(u).ok()))
                .ok_or_else(|| {
                    FsmError::invalid(format!("{kind}: dialect.{key} must be an integer"))
                })?;
            if n < min || n > max {
                return Err(FsmError::invalid(format!(
                    "{kind}: dialect.{key} must be in {min}..={max}, got {n}"
                )));
            }
            Ok(Some(n))
        }
    }
}

pub fn dialect_opt_str_vec(
    kind: &str,
    dialect: &BTreeMap<String, Value>,
    key: &str,
) -> Result<Option<Vec<String>>, FsmError> {
    match dialect.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(arr)) => {
            if arr.is_empty() {
                return Err(FsmError::invalid(format!(
                    "{kind}: dialect.{key} must not be empty"
                )));
            }
            let mut out = Vec::with_capacity(arr.len());
            for item in arr {
                let s = item
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        FsmError::invalid(format!(
                            "{kind}: dialect.{key} entries must be non-empty strings"
                        ))
                    })?;
                out.push(s.to_owned());
            }
            Ok(Some(out))
        }
        Some(_) => Err(FsmError::invalid(format!(
            "{kind}: dialect.{key} must be an array of strings"
        ))),
    }
}

pub fn dialect_opt_str_map(
    kind: &str,
    dialect: &BTreeMap<String, Value>,
    key: &str,
) -> Result<Option<BTreeMap<String, String>>, FsmError> {
    match dialect.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(obj)) => {
            let mut out = BTreeMap::new();
            for (k, v) in obj {
                let s = v
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        FsmError::invalid(format!(
                            "{kind}: dialect.{key}.{k} must be a non-empty string"
                        ))
                    })?;
                out.insert(k.clone(), s.to_owned());
            }
            Ok(Some(out))
        }
        Some(_) => Err(FsmError::invalid(format!(
            "{kind}: dialect.{key} must be a string map"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Text / HTML / DOI / Date normalization
// ---------------------------------------------------------------------------

pub fn collapse_ws(s: &str) -> String {
    WS_RE.replace_all(s.trim(), " ").into_owned()
}

pub fn unescape_html(input: &str) -> String {
    if !input.contains('&') {
        return input.to_owned();
    }
    CHAR_REF_RE
        .replace_all(input, |caps: &regex::Captures<'_>| {
            let token = &caps[1];
            if let Some(hex) = token
                .strip_prefix("#x")
                .or_else(|| token.strip_prefix("#X"))
            {
                if let Some(ch) = u32::from_str_radix(hex, 16).ok().and_then(char::from_u32) {
                    return ch.to_string();
                }
            } else if let Some(dec) = token.strip_prefix('#') {
                if let Some(ch) = dec.parse::<u32>().ok().and_then(char::from_u32) {
                    return ch.to_string();
                }
            } else if let Some(rep) = named_entity(token) {
                return rep.to_owned();
            }
            caps[0].to_owned()
        })
        .into_owned()
}

fn named_entity(name: &str) -> Option<&'static str> {
    Some(match name {
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
        "middot" => "\u{00B7}",
        "bull" => "\u{2022}",
        "laquo" => "\u{00AB}",
        "raquo" => "\u{00BB}",
        "euro" => "\u{20AC}",
        "pound" => "\u{00A3}",
        "yen" => "\u{00A5}",
        "alpha" => "\u{03B1}",
        "beta" => "\u{03B2}",
        "gamma" => "\u{03B3}",
        "delta" => "\u{03B4}",
        "epsilon" => "\u{03B5}",
        "theta" => "\u{03B8}",
        "lambda" => "\u{03BB}",
        "mu" => "\u{03BC}",
        "pi" => "\u{03C0}",
        "sigma" => "\u{03C3}",
        "tau" => "\u{03C4}",
        "phi" => "\u{03C6}",
        "omega" => "\u{03C9}",
        "uuml" => "\u{00FC}",
        "Uuml" => "\u{00DC}",
        "ouml" => "\u{00F6}",
        "Ouml" => "\u{00D6}",
        "auml" => "\u{00E4}",
        "Auml" => "\u{00C4}",
        "eacute" => "\u{00E9}",
        "egrave" => "\u{00E8}",
        "aacute" => "\u{00E1}",
        "ntilde" => "\u{00F1}",
        "ccedil" => "\u{00E7}",
        "szlig" => "\u{00DF}",
        _ => return None,
    })
}

/// Remove HTML/XML tags (replacing tags with a space for block-level separation),
/// decode HTML entities, and collapse whitespace.
pub fn strip_tags(s: &str) -> String {
    let no_tags = TAG_RE.replace_all(s, " ");
    collapse_ws(&unescape_html(&no_tags))
}

/// Remove inline HTML/XML tags without injecting spaces (matches Python
/// `wikipedia.py`'s `_TAG_RE.sub("", raw)` for `<span class="searchmatch">`),
/// decode HTML entities, and collapse whitespace.
pub fn strip_tags_joined(s: &str) -> String {
    let no_tags = TAG_RE.replace_all(s, "");
    collapse_ws(&unescape_html(&no_tags))
}

/// Collapse whitespace and truncate cleanly to at most `max_chars` characters.
pub fn clip(s: &str, max_chars: usize) -> String {
    let cleaned = collapse_ws(s);
    if cleaned.chars().count() <= max_chars {
        return cleaned;
    }
    let prefix = fsm_core::text::clip_chars(&cleaned, max_chars.saturating_sub(1)).trim_end();
    format!("{prefix}\u{2026}")
}

/// Normalize a DOI string (strip `https://doi.org/`, `doi:`, trailing punctuation, lowercase).
pub fn normalize_doi(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let stripped = DOI_PREFIX_RE.replace(trimmed, "");
    let cleaned = stripped
        .trim()
        .trim_end_matches(['.', ',', ';', ')'])
        .to_ascii_lowercase();
    DOI_VALID_RE.is_match(&cleaned).then_some(cleaned)
}

/// Leniently normalize a date string to `YYYY-MM-DD`, `YYYY-MM`, or `YYYY`,
/// rejecting sentinels (`0001-01-01`, `9999`, years outside `1000..=2999`).
pub fn normalize_date(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let candidate = fsm_core::dates::normalize_date(s).or_else(|| parse_extra_date_formats(s))?;
    let year = year_from_date(&candidate)?;
    if !(1000..=2999).contains(&year) || candidate == "0001-01-01" {
        return None;
    }
    Some(candidate)
}

fn parse_extra_date_formats(s: &str) -> Option<String> {
    if let Some(c) = DOT_YMD_RE.captures(s) {
        let d = jiff::civil::Date::new(c[1].parse().ok()?, c[2].parse().ok()?, c[3].parse().ok()?)
            .ok()?;
        return Some(d.to_string());
    }
    if let Some(c) = SLASH_YM_RE.captures(s) {
        let m: i8 = c[2].parse().ok()?;
        if (1..=12).contains(&m) {
            return Some(format!("{}-{m:02}", &c[1]));
        }
    }
    if let Some(c) = YMD_MON_RE.captures(s) {
        let m = month_num(&c[2])?;
        if let Some(day) = c.get(3) {
            let d =
                jiff::civil::Date::new(c[1].parse().ok()?, m, day.as_str().parse().ok()?).ok()?;
            return Some(d.to_string());
        }
        return Some(format!("{}-{m:02}", &c[1]));
    }
    if let Some(c) = DMY_TZ_RE.captures(s) {
        let m = month_num(&c[2])?;
        let d = jiff::civil::Date::new(c[3].parse().ok()?, m, c[1].parse().ok()?).ok()?;
        return Some(d.to_string());
    }
    // Catalogue-style ISBD/MARC dates: "1994.", "[1996]", "c2014.", "1994-", "©2018".
    if let Some(c) = CATALOG_YEAR_RE.captures(s) {
        return Some(c[1].to_owned());
    }
    None
}

fn month_num(name: &str) -> Option<i8> {
    match name.get(..3)?.to_ascii_lowercase().as_str() {
        "jan" => Some(1),
        "feb" => Some(2),
        "mar" => Some(3),
        "apr" => Some(4),
        "may" => Some(5),
        "jun" => Some(6),
        "jul" => Some(7),
        "aug" => Some(8),
        "sep" => Some(9),
        "oct" => Some(10),
        "nov" => Some(11),
        "dec" => Some(12),
        _ => None,
    }
}

pub fn year_from_date(date: &str) -> Option<u16> {
    let s = date.trim();
    let y: u16 = YEAR_PREFIX_RE.captures(s)?.get(1)?.as_str().parse().ok()?;
    (1000..=2999).contains(&y).then_some(y)
}

/// Normalize free-form repository resource types into canonical `ScholarlyWork.work_type`.
pub fn normalize_work_type(raw: Option<&str>) -> String {
    let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return "article".to_owned();
    };
    let lower = s.to_ascii_lowercase();
    if lower.contains("thesis") || lower.contains("dissertation") || lower.contains("etd") {
        "thesis".to_owned()
    } else if lower.contains("preprint")
        || lower.contains("working paper")
        || lower.contains("workingpaper")
    {
        "preprint".to_owned()
    } else if lower.contains("dataset") || lower == "data" {
        "dataset".to_owned()
    } else if lower.contains("software") || lower.contains("code") {
        "software".to_owned()
    } else if lower.contains("book") || lower.contains("monograph") {
        "book".to_owned()
    } else if lower.contains("report") {
        "report".to_owned()
    } else if lower.contains("proceeding") || lower.contains("conference") {
        "inproceedings".to_owned()
    } else if lower.contains("article") || lower.contains("journal") {
        "article".to_owned()
    } else {
        lower
    }
}

// ---------------------------------------------------------------------------
// Namespace-agnostic XML helpers
// ---------------------------------------------------------------------------

pub fn child_by_local<'a, 'input>(node: Node<'a, 'input>, local: &str) -> Option<Node<'a, 'input>> {
    node.children()
        .find(|n| n.is_element() && n.tag_name().name().eq_ignore_ascii_case(local))
}

pub fn children_by_local<'a, 'input>(
    node: Node<'a, 'input>,
    local: &'a str,
) -> impl Iterator<Item = Node<'a, 'input>> + 'a {
    node.children()
        .filter(move |n| n.is_element() && n.tag_name().name().eq_ignore_ascii_case(local))
}

pub fn descendants_by_local<'a, 'input>(
    node: Node<'a, 'input>,
    local: &'a str,
) -> impl Iterator<Item = Node<'a, 'input>> + 'a {
    node.descendants()
        .filter(move |n| n.is_element() && n.tag_name().name().eq_ignore_ascii_case(local))
}

pub fn node_text(node: Node<'_, '_>) -> String {
    let mut buf = String::new();
    for d in node.descendants() {
        if d.is_text()
            && let Some(t) = d.text()
        {
            buf.push_str(t);
        }
    }
    collapse_ws(&buf)
}

pub fn child_text(node: Node<'_, '_>, local: &str) -> Option<String> {
    let child = child_by_local(node, local)?;
    let text = node_text(child);
    (!text.is_empty()).then_some(text)
}

pub fn attr_by_local<'a>(node: Node<'a, 'a>, local: &str) -> Option<&'a str> {
    node.attributes()
        .find(|a| a.name().eq_ignore_ascii_case(local))
        .map(|a| a.value())
}

// ---------------------------------------------------------------------------
// Shared Dublin Core / JPCOAR / RDF XML -> ScholarlyWork extraction
// ---------------------------------------------------------------------------

/// Extract a [`ScholarlyWork`] from a Dublin Core (`oai_dc`, `srw_dc`, `dc`),
/// JPCOAR, or `dcndl` XML subtree.
pub fn dc_node_to_work(
    node: Node<'_, '_>,
    fallback_url: Option<&str>,
    default_work_type: &str,
) -> Option<ScholarlyWork> {
    // Title: prefer direct text on <title> (avoiding duplicate nested rdf:value if present).
    let mut title = String::new();
    for n in descendants_by_local(node, "title") {
        let t = node_text(n);
        if !t.is_empty() {
            title = t;
            break;
        }
    }
    if title.is_empty() {
        return None;
    }

    // Authors: <creator>, then fallback <contributor> (checking foaf:name / creatorName).
    let mut authors = Vec::new();
    for tag in ["creator", "creatorname"] {
        for n in descendants_by_local(node, tag) {
            let name = child_by_local(n, "Agent")
                .and_then(|a| child_text(a, "name"))
                .or_else(|| child_text(n, "creatorName"))
                .unwrap_or_else(|| node_text(n));
            if !name.is_empty() && !authors.iter().any(|a: &AuthorRef| a.name == name) {
                authors.push(AuthorRef::new(name));
            }
        }
    }
    if authors.is_empty() {
        for n in descendants_by_local(node, "contributor") {
            let name = child_by_local(n, "Agent")
                .and_then(|a| child_text(a, "name"))
                .unwrap_or_else(|| node_text(n));
            if !name.is_empty() && !authors.iter().any(|a: &AuthorRef| a.name == name) {
                authors.push(AuthorRef::new(name));
            }
        }
    }

    // Identifiers: find DOI and HTTP URL.
    let mut doi: Option<String> = None;
    let mut url: Option<String> = None;
    for n in descendants_by_local(node, "identifier") {
        let val = node_text(n);
        if val.is_empty() {
            continue;
        }
        if doi.is_none()
            && let Some(d) = normalize_doi(&val)
        {
            doi = Some(d);
        }
        if url.is_none() && (val.starts_with("http://") || val.starts_with("https://")) {
            url = Some(val);
        }
    }
    // Check RDF seeAlso / rdf:about / relation URLs.
    if url.is_none() {
        for n in descendants_by_local(node, "seeAlso") {
            if let Some(r) = attr_by_local(n, "resource")
                && (r.starts_with("http://") || r.starts_with("https://"))
            {
                if doi.is_none()
                    && let Some(d) = normalize_doi(r)
                {
                    doi = Some(d);
                }
                url = Some(r.to_owned());
                break;
            }
        }
    }
    if url.is_none() {
        for n in node.descendants().filter(|n| n.is_element()) {
            if let Some(about) = attr_by_local(n, "about")
                && (about.starts_with("http://") || about.starts_with("https://"))
            {
                url = Some(about.trim_end_matches("#material").to_owned());
                break;
            }
        }
    }
    if url.is_none() {
        for n in descendants_by_local(node, "relation") {
            let val = node_text(n);
            if val.starts_with("http://") || val.starts_with("https://") {
                url = Some(val);
                break;
            }
        }
    }
    let final_url = url
        .or_else(|| doi.as_ref().map(|d| format!("https://doi.org/{d}")))
        .or_else(|| fallback_url.map(str::to_owned))
        .unwrap_or_default();

    // Date & year (check issued before generic date).
    let mut published_date: Option<String> = None;
    for tag in ["issued", "date"] {
        for n in descendants_by_local(node, tag) {
            let t = node_text(n);
            if let Some(norm) = normalize_date(&t) {
                published_date = Some(norm);
                break;
            }
        }
        if published_date.is_some() {
            break;
        }
    }
    let year = published_date.as_deref().and_then(year_from_date);

    // Venue / publisher / source: skip ISSN/ISBN lines, bare URLs, and ISBD place-of-publication stubs ending in ':'.
    let venue = descendants_by_local(node, "source")
        .chain(descendants_by_local(node, "publisher"))
        .find_map(|n| {
            let t = child_by_local(n, "Agent")
                .and_then(|a| child_text(a, "name"))
                .unwrap_or_else(|| node_text(n));
            is_valid_dc_venue(&t).then_some(t)
        });

    // Abstract / description.
    let abstract_text = descendants_by_local(node, "abstract")
        .chain(descendants_by_local(node, "description"))
        .find_map(|n| {
            let t = strip_tags(&node_text(n));
            (!t.is_empty()).then(|| clip(&t, 1200))
        });

    // Work type: prefer info:eu-repo/semantics/* (used by HAL/OpenAIRE/OAI repos) over generic labels.
    let all_types: Vec<String> = descendants_by_local(node, "type")
        .map(node_text)
        .filter(|t| !t.is_empty())
        .collect();
    let eu_repo_type = all_types.iter().find_map(|t| {
        t.to_ascii_lowercase()
            .find("info:eu-repo/semantics/")
            .map(|idx| &t[idx + "info:eu-repo/semantics/".len()..])
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    });
    let raw_type = eu_repo_type.or_else(|| {
        all_types
            .into_iter()
            .find(|t| !t.eq_ignore_ascii_case("text"))
    });
    let work_type = raw_type
        .as_deref()
        .map(|t| normalize_work_type(Some(t)))
        .unwrap_or_else(|| default_work_type.to_owned());

    // Rights / OA hint if present (scan all <dc:rights>, e.g. HAL emits a policy URL then OpenAccess).
    let rights_vals: Vec<String> = descendants_by_local(node, "rights")
        .map(node_text)
        .filter(|t| !t.is_empty())
        .collect();
    let rights_denied = rights_vals.iter().any(|t| is_oa_denied(t));
    let oa_rights = if rights_denied {
        None
    } else {
        rights_vals.into_iter().find(|t| OA_CC_RE.is_match(t))
    };

    let mut work = ScholarlyWork::new(title, final_url);
    work.authors = authors;
    work.year = year;
    work.published_date = published_date;
    work.venue = venue;
    work.work_type = work_type;
    work.ids.doi = doi;
    work.abstract_text = abstract_text;
    if let Some(lic) = oa_rights {
        let mut oa = OaLocation::new("green");
        oa.url = (!work.url.is_empty()).then(|| work.url.clone());
        oa.license = Some(lic);
        work.oa = Some(oa);
    }
    Some(work)
}

fn is_valid_dc_venue(s: &str) -> bool {
    let t = s.trim();
    if t.is_empty() || t.ends_with(':') || t.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let lower = t.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return false;
    }
    !VENUE_ID_PREFIX_RE.is_match(t)
}

pub fn work_to_value(work: ScholarlyWork) -> Value {
    serde_json::to_value(work).unwrap_or(Value::Null)
}

/// Serialize a [`Hit`] while omitting `"kind"` so `InstanceSource` (A1 §D.3)
/// injects `HitKind::Academic` for academic-scoped instances and `HitKind::Record` otherwise.
pub fn hit_to_value(hit: Hit) -> Value {
    let mut val = serde_json::to_value(hit).unwrap_or(Value::Null);
    if let Some(obj) = val.as_object_mut() {
        obj.remove("kind");
    }
    val
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescape_html_handles_named_decimal_hex_and_unknown() {
        assert_eq!(
            unescape_html("A &amp; B &ndash; C &mdash; &#39;ok&#x27; &uuml; &unknown;"),
            "A & B \u{2013} C \u{2014} 'ok' \u{00FC} &unknown;"
        );
        assert_eq!(unescape_html("plain text"), "plain text");
    }

    #[test]
    fn strip_tags_removes_markup_and_collapses_whitespace() {
        assert_eq!(
            strip_tags(
                "  <p>Hello <span class=\"searchmatch\">Rust</span> &amp; <b>world</b></p>\n "
            ),
            "Hello Rust & world"
        );
    }

    #[test]
    fn normalize_doi_strips_prefixes_and_trailing_punctuation() {
        assert_eq!(
            normalize_doi("https://doi.org/10.1038/NATURE12373.").as_deref(),
            Some("10.1038/nature12373")
        );
        assert_eq!(
            normalize_doi("http://dx.doi.org/10.5281/zenodo.123;").as_deref(),
            Some("10.5281/zenodo.123")
        );
        assert_eq!(
            normalize_doi("DOI: 10.1234/abc)").as_deref(),
            Some("10.1234/abc")
        );
        assert_eq!(normalize_doi("not-a-doi"), None);
    }

    #[test]
    fn normalize_date_handles_extra_formats_and_rejects_sentinels() {
        assert_eq!(
            normalize_date("2025-10-12T15:00:00Z").as_deref(),
            Some("2025-10-12")
        );
        assert_eq!(
            normalize_date("25 Nov 2026  EST").as_deref(),
            Some("2026-11-25")
        );
        assert_eq!(normalize_date("2024.08.30").as_deref(), Some("2024-08-30"));
        assert_eq!(normalize_date("2024/09").as_deref(), Some("2024-09"));
        assert_eq!(normalize_date("2026 Nov 25").as_deref(), Some("2026-11-25"));
        assert_eq!(normalize_date("2026 Nov").as_deref(), Some("2026-11"));
        assert_eq!(normalize_date("9999"), None);
        assert_eq!(normalize_date("0001-01-01"), None);
        assert_eq!(normalize_date("2024-02-30"), None);
    }

    #[test]
    fn encode_path_segment_enforces_traversal_guardrails() {
        assert_eq!(
            encode_path_segment("abc-123_4.5~", "id").unwrap(),
            "abc-123_4.5~"
        );
        assert_eq!(
            encode_path_segment("a b?c#d", "id").unwrap(),
            "a%20b%3Fc%23d"
        );
        for bad in ["", "  ", "..", ".", "a/b", "a\\b", "a\0b", "a\nb", "a\rb"] {
            assert!(
                encode_path_segment(bad, "id").is_err(),
                "{bad:?} should fail"
            );
        }
    }

    #[test]
    fn decode_body_respects_latin1_charset_header() {
        let resp = HttpBytes::new(
            "https://example.org/",
            200,
            vec![("Content-Type".into(), "text/xml; charset=ISO-8859-1".into())],
            vec![b'c', b'a', b'f', 0xe9],
        );
        assert_eq!(decode_body(&resp), "caf\u{00E9}");
    }

    #[test]
    fn strip_tags_joined_preserves_adjacent_punctuation() {
        assert_eq!(
            strip_tags_joined(
                r#"memetic <span class="searchmatch">algorithms</span>) &amp; <span class="searchmatch">highway</span>=crossing; <span class="searchmatch">Constitution</span>. [[de:<span class="searchmatch">Systemd</span>]] <span class="searchmatch">systemd</span>-tmpfiles"#
            ),
            "memetic algorithms) & highway=crossing; Constitution. [[de:Systemd]] systemd-tmpfiles"
        );
    }

    #[test]
    fn opt_limit_clamps_above_max_and_rejects_below_min() {
        let mut m = Map::new();
        m.insert("limit".into(), serde_json::json!(100));
        assert_eq!(opt_limit(&m, "limit", 10, 1, 50).unwrap(), 50);

        m.insert("limit".into(), serde_json::json!(0));
        assert!(matches!(
            opt_limit(&m, "limit", 10, 1, 50),
            Err(FsmError::InvalidInput(_))
        ));

        m.insert("limit".into(), serde_json::json!(-5));
        assert!(matches!(
            opt_limit(&m, "limit", 10, 1, 50),
            Err(FsmError::InvalidInput(_))
        ));

        assert_eq!(opt_limit(&Map::new(), "limit", 10, 1, 50).unwrap(), 10);
    }

    #[test]
    fn normalize_date_parses_catalog_years_and_rejects_invalid() {
        assert_eq!(normalize_date("1994.").as_deref(), Some("1994"));
        assert_eq!(normalize_date("[1996]").as_deref(), Some("1996"));
        assert_eq!(normalize_date("c2014.").as_deref(), Some("2014"));
        assert_eq!(normalize_date("1994-").as_deref(), Some("1994"));
        assert_eq!(normalize_date("©2018").as_deref(), Some("2018"));
        assert_eq!(normalize_date("9999"), None);
        assert_eq!(normalize_date("2024-02-30"), None);
        assert_eq!(normalize_date("2024-13"), None);
    }

    #[test]
    fn valid_dc_venue_skips_issns_urls_and_isbd_places() {
        assert!(!is_valid_dc_venue("ISSN: 1614-6832"));
        assert!(!is_valid_dc_venue("EISSN: 1436-5081"));
        assert!(!is_valid_dc_venue("ISBN: 978-2-84242-533-3"));
        assert!(!is_valid_dc_venue("https://hal.science/hal-00925108"));
        assert!(!is_valid_dc_venue("South Australia :"));
        assert!(!is_valid_dc_venue("Perth :"));
        assert!(is_valid_dc_venue("Monatshefte für Mathematik"));
        assert!(is_valid_dc_venue("Dept. of Mines and Energy."));
    }

    #[test]
    fn hit_to_value_omits_kind_key() {
        let hit = Hit::new("Title", "https://example.org/a");
        let val = hit_to_value(hit);
        assert!(val.get("kind").is_none(), "expected kind omitted: {val}");
    }
}
