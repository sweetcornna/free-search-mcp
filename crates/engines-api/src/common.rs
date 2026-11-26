//! Shared helpers for `fsm-engines-api` native engines.

use fsm_core::api::Freshness;
use fsm_core::config::Config;
use fsm_core::url_canon::{host_matches, host_of};
use fsm_core::{
    FsmError, Hit, HitKind, HttpBytes, HttpRequest, NetClient, NetCx, OpDecl, Param, ParamType,
    ReturnKind, SourceDecl, SrcCx, TransportProfile,
};
use jiff::Timestamp;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

/// Default snippet length cap matching Python `jsonapi.SNIPPET_CAP`.
pub const SNIPPET_CAP: usize = 400;

/// RFC 3986 path-segment encoding set (keeps unreserved `- . _ ~` and alphanumerics).
const PATH_SEG_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

static ANY_TAG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]+>").expect("regex"));

static HIGHLIGHT_TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)</?(?:strong|em)\b[^>]*>").expect("regex"));

/// Percent-encode a single URL path segment (`/` -> `%2F`, spaces -> `%20`).
pub fn encode_path_segment(s: &str) -> String {
    utf8_percent_encode(s, PATH_SEG_SET).to_string()
}

/// Percent-encode a string using `application/x-www-form-urlencoded` rules (spaces -> `+`).
pub fn quote_plus(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// Normalize whitespace and clip to at most `cap` Unicode scalar values.
///
/// Mirrors Python `jsonapi.clip(text, cap)`: collapses whitespace runs to a single
/// space, and when longer than `cap` chars cuts to `cap`, trims trailing whitespace,
/// and appends `" …"`.
pub fn clip(text: &str, cap: usize) -> String {
    let collapsed = collapse_ws(text);
    if collapsed.chars().count() <= cap {
        return collapsed;
    }
    let prefix: String = collapsed.chars().take(cap).collect();
    format!("{} …", prefix.trim_end())
}

/// Collapse consecutive Unicode whitespace into single ASCII spaces and trim ends.
pub fn collapse_ws(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Remove `<strong>` / `<em>` tags (with optional attributes) and collapse whitespace.
pub fn strip_highlight_tags(text: &str) -> String {
    collapse_ws(&HIGHLIGHT_TAG_RE.replace_all(text, ""))
}

/// Remove all `<...>` tags without inserting spaces, then trim.
pub fn strip_tags_tight(text: &str) -> String {
    ANY_TAG_RE.replace_all(text, "").trim().to_owned()
}

/// Remove all `<...>` tags (replacing with spaces), decode HTML entities, and collapse whitespace.
pub fn strip_html_and_unescape(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let no_tags = ANY_TAG_RE.replace_all(text, " ");
    collapse_ws(&unescape_html(&no_tags))
}

/// Decode HTML5 named entities and numeric (`&#...;` / `&#x...;`, with optional semicolon)
/// character references in strict O(n) time.
pub fn unescape_html(input: &str) -> String {
    if !input.contains('&') {
        return input.to_owned();
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let bytes = rest.as_bytes();
        if bytes.len() >= 3 && bytes[1] == b'#' {
            if let Some((ch, consumed)) = parse_numeric_entity(bytes) {
                out.push(ch);
                rest = &rest[consumed..];
                continue;
            }
        } else if bytes.len() >= 3 && bytes[1].is_ascii_alphabetic() {
            let max_name = bytes.len().min(33);
            let mut end = 1;
            while end < max_name && bytes[end].is_ascii_alphanumeric() {
                end += 1;
            }
            if end < bytes.len() && bytes[end] == b';' {
                let name = &rest[1..end];
                if let Some(decoded) = decode_named_entity(name) {
                    out.push_str(decoded);
                    rest = &rest[end + 1..];
                    continue;
                }
            }
        }
        out.push('&');
        rest = &rest[1..];
    }
    out.push_str(rest);
    out
}

fn parse_numeric_entity(bytes: &[u8]) -> Option<(char, usize)> {
    // bytes[0] == b'&', bytes[1] == b'#'
    let (radix, start) = match bytes.get(2) {
        Some(b'x' | b'X') => (16u32, 3usize),
        _ => (10u32, 2usize),
    };
    let mut end = start;
    let max_end = bytes.len().min(start + 8);
    while end < max_end {
        let ok = if radix == 16 {
            bytes[end].is_ascii_hexdigit()
        } else {
            bytes[end].is_ascii_digit()
        };
        if !ok {
            break;
        }
        end += 1;
    }
    if end == start {
        return None;
    }
    let digits = std::str::from_utf8(&bytes[start..end]).ok()?;
    let cp = u32::from_str_radix(digits, radix).ok()?;
    let ch = char::from_u32(cp).filter(|&c| c != '\0')?;
    let consumed = if bytes.get(end) == Some(&b';') {
        end + 1
    } else {
        end
    };
    Some((ch, consumed))
}

fn decode_named_entity(ent: &str) -> Option<&'static str> {
    let s = match ent {
        // Core XML/HTML punctuation & whitespace
        "amp" | "AMP" => "&",
        "lt" | "LT" => "<",
        "gt" | "GT" => ">",
        "quot" | "QUOT" => "\"",
        "apos" => "'",
        "nbsp" => " ",
        "ensp" => "\u{2002}",
        "emsp" => "\u{2003}",
        "thinsp" => "\u{2009}",
        "ndash" => "\u{2013}",
        "mdash" => "\u{2014}",
        "lsquo" => "\u{2018}",
        "rsquo" => "\u{2019}",
        "sbquo" => "\u{201A}",
        "ldquo" => "\u{201C}",
        "rdquo" => "\u{201D}",
        "bdquo" => "\u{201E}",
        "lsaquo" => "\u{2039}",
        "rsaquo" => "\u{203A}",
        "laquo" => "\u{00AB}",
        "raquo" => "\u{00BB}",
        "hellip" => "\u{2026}",
        "bull" => "\u{2022}",
        "middot" => "\u{00B7}",
        "prime" => "\u{2032}",
        "Prime" => "\u{2033}",
        "dagger" => "\u{2020}",
        "Dagger" => "\u{2021}",
        "permil" => "\u{2030}",
        // Symbols, currency & math
        "copy" | "COPY" => "\u{00A9}",
        "reg" | "REG" => "\u{00AE}",
        "trade" => "\u{2122}",
        "euro" => "\u{20AC}",
        "pound" => "\u{00A3}",
        "yen" => "\u{00A5}",
        "cent" => "\u{00A2}",
        "sect" => "\u{00A7}",
        "para" => "\u{00B6}",
        "iexcl" => "\u{00A1}",
        "iquest" => "\u{00BF}",
        "times" => "\u{00D7}",
        "divide" => "\u{00F7}",
        "plusmn" => "\u{00B1}",
        "minus" => "\u{2212}",
        "deg" => "\u{00B0}",
        "micro" => "\u{00B5}",
        "sup1" => "\u{00B9}",
        "sup2" => "\u{00B2}",
        "sup3" => "\u{00B3}",
        "frac14" => "\u{00BC}",
        "frac12" => "\u{00BD}",
        "frac34" => "\u{00BE}",
        "le" => "\u{2264}",
        "ge" => "\u{2265}",
        "ne" => "\u{2260}",
        "asymp" => "\u{2248}",
        "equiv" => "\u{2261}",
        "infin" => "\u{221E}",
        "larr" => "\u{2190}",
        "uarr" => "\u{2191}",
        "rarr" => "\u{2192}",
        "darr" => "\u{2193}",
        "harr" => "\u{2194}",
        // Latin-1 accented letters
        "Agrave" => "\u{00C0}",
        "Aacute" => "\u{00C1}",
        "Acirc" => "\u{00C2}",
        "Atilde" => "\u{00C3}",
        "Auml" => "\u{00C4}",
        "Aring" => "\u{00C5}",
        "AElig" => "\u{00C6}",
        "Ccedil" => "\u{00C7}",
        "Egrave" => "\u{00C8}",
        "Eacute" => "\u{00C9}",
        "Ecirc" => "\u{00CA}",
        "Euml" => "\u{00CB}",
        "Igrave" => "\u{00CC}",
        "Iacute" => "\u{00CD}",
        "Icirc" => "\u{00CE}",
        "Iuml" => "\u{00CF}",
        "Ntilde" => "\u{00D1}",
        "Ograve" => "\u{00D2}",
        "Oacute" => "\u{00D3}",
        "Ocirc" => "\u{00D4}",
        "Otilde" => "\u{00D5}",
        "Ouml" => "\u{00D6}",
        "Oslash" => "\u{00D8}",
        "Ugrave" => "\u{00D9}",
        "Uacute" => "\u{00DA}",
        "Ucirc" => "\u{00DB}",
        "Uuml" => "\u{00DC}",
        "Yacute" => "\u{00DD}",
        "szlig" => "\u{00DF}",
        "agrave" => "\u{00E0}",
        "aacute" => "\u{00E1}",
        "acirc" => "\u{00E2}",
        "atilde" => "\u{00E3}",
        "auml" => "\u{00E4}",
        "aring" => "\u{00E5}",
        "aelig" => "\u{00E6}",
        "ccedil" => "\u{00E7}",
        "egrave" => "\u{00E8}",
        "eacute" => "\u{00E9}",
        "ecirc" => "\u{00EA}",
        "euml" => "\u{00EB}",
        "igrave" => "\u{00EC}",
        "iacute" => "\u{00ED}",
        "icirc" => "\u{00EE}",
        "iuml" => "\u{00EF}",
        "ntilde" => "\u{00F1}",
        "ograve" => "\u{00F2}",
        "oacute" => "\u{00F3}",
        "ocirc" => "\u{00F4}",
        "otilde" => "\u{00F5}",
        "ouml" => "\u{00F6}",
        "oslash" => "\u{00F8}",
        "ugrave" => "\u{00F9}",
        "uacute" => "\u{00FA}",
        "ucirc" => "\u{00FB}",
        "uuml" => "\u{00FC}",
        "yacute" => "\u{00FD}",
        "yuml" => "\u{00FF}",
        "OElig" => "\u{0152}",
        "oelig" => "\u{0153}",
        "Scaron" => "\u{0160}",
        "scaron" => "\u{0161}",
        "Yuml" => "\u{0178}",
        // Greek letters
        "alpha" => "\u{03B1}",
        "beta" => "\u{03B2}",
        "gamma" => "\u{03B3}",
        "delta" => "\u{03B4}",
        "epsilon" => "\u{03B5}",
        "zeta" => "\u{03B6}",
        "eta" => "\u{03B7}",
        "theta" => "\u{03B8}",
        "iota" => "\u{03B9}",
        "kappa" => "\u{03BA}",
        "lambda" => "\u{03BB}",
        "mu" => "\u{03BC}",
        "nu" => "\u{03BD}",
        "xi" => "\u{03BE}",
        "omicron" => "\u{03BF}",
        "pi" => "\u{03C0}",
        "rho" => "\u{03C1}",
        "sigma" => "\u{03C3}",
        "tau" => "\u{03C4}",
        "upsilon" => "\u{03C5}",
        "phi" => "\u{03C6}",
        "chi" => "\u{03C7}",
        "psi" => "\u{03C8}",
        "omega" => "\u{03C9}",
        "Alpha" => "\u{0391}",
        "Beta" => "\u{0392}",
        "Gamma" => "\u{0393}",
        "Delta" => "\u{0394}",
        "Theta" => "\u{0398}",
        "Lambda" => "\u{039B}",
        "Pi" => "\u{03A0}",
        "Sigma" => "\u{03A3}",
        "Phi" => "\u{03A6}",
        "Psi" => "\u{03A8}",
        "Omega" => "\u{03A9}",
        _ => return None,
    };
    Some(s)
}

/// Extract `YYYY-MM-DD` from an ISO timestamp string, date string, or Unix epoch seconds.
///
/// Safe on arbitrary multibyte UTF-8 strings (never panics on non-ASCII bytes).
pub fn iso_day(value: Option<&Value>) -> String {
    let Some(v) = value else {
        return String::new();
    };
    match v {
        Value::Number(n) => {
            if let Some(sec) = n.as_i64() {
                epoch_seconds_to_ymd(sec)
            } else if let Some(f) = n.as_f64().filter(|f| f.is_finite()) {
                epoch_seconds_to_ymd(f.trunc() as i64)
            } else {
                String::new()
            }
        }
        Value::String(s) => match s.get(..10) {
            Some(d) if d.as_bytes()[4] == b'-' && d.as_bytes()[7] == b'-' => d.to_owned(),
            _ => String::new(),
        },
        _ => String::new(),
    }
}

/// Extract `YYYY-MM-DD` prefix (before `'T'`) from an ISO string, or empty string.
pub fn iso_date(value: Option<&Value>) -> String {
    let Some(s) = value.and_then(Value::as_str) else {
        return String::new();
    };
    let s = s.trim();
    if s.is_empty() {
        return String::new();
    }
    s.split('T').next().unwrap_or("").to_owned()
}

/// Convert Unix epoch seconds to UTC `YYYY-MM-DD`, returning `""` on out-of-range values.
pub fn epoch_seconds_to_ymd(sec: i64) -> String {
    let Ok(ts) = Timestamp::from_second(sec) else {
        return String::new();
    };
    let zdt = ts.to_zoned_datetime(jiff::tz::TimeZone::UTC);
    zdt.date().to_string()
}

/// Format an integer or numeric string with thousands commas (`24870895` -> `"24,870,895"`).
pub fn compact_number(raw: &str) -> String {
    let trimmed = raw.trim();
    let Ok(f) = trimmed.parse::<f64>() else {
        return raw.to_owned();
    };
    if !f.is_finite() || f < (i64::MIN as f64) || f > (i64::MAX as f64) {
        return raw.to_owned();
    }
    format_i64_commas(f.trunc() as i64)
}

/// Format an `i64` with thousands separators.
pub fn format_i64_commas(n: i64) -> String {
    let neg = n < 0;
    let digits = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + usize::from(neg));
    if neg {
        out.push('-');
    }
    let len = digits.len();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Append `site:` / `-site:` / `filetype:` operators to a free-text search query.
pub fn augment_query_with_operators(
    query: &str,
    include_domains: &[String],
    exclude_domains: &[String],
    filetype: Option<&str>,
) -> String {
    let mut parts = vec![query.to_owned()];
    if include_domains.len() == 1 {
        parts.push(format!("site:{}", include_domains[0]));
    } else if include_domains.len() > 1 {
        let joined = include_domains
            .iter()
            .map(|d| format!("site:{d}"))
            .collect::<Vec<_>>()
            .join(" OR ");
        parts.push(format!("({joined})"));
    }
    for d in exclude_domains {
        parts.push(format!("-site:{d}"));
    }
    if let Some(ft) = filetype.filter(|s| !s.is_empty()) {
        parts.push(format!("filetype:{ft}"));
    }
    parts.join(" ")
}

/// Standard `search` op declaration shared by all 15 engines.
pub fn search_op(summary: &str, max_limit: u32, example: &str) -> OpDecl {
    OpDecl::new("search", summary, ReturnKind::Hits)
        .with_param(Param::new("query", ParamType::String, "Search query").required())
        .with_param(
            Param::new("limit", ParamType::Int, "Maximum number of results")
                .with_default(serde_json::json!(10))
                .with_range(1.0, f64::from(max_limit)),
        )
        .with_example(example)
}

/// Validate `op` and extract `(query, limit)` from `params`.
pub fn parse_search_params<'a>(
    source_id: &str,
    op: &str,
    params: &'a Value,
    default_limit: usize,
) -> Result<(&'a str, usize), FsmError> {
    if op != "search" {
        return Err(FsmError::invalid(format!(
            "source '{source_id}' has no op '{op}'; call '{source_id}.search'"
        )));
    }
    let query = params
        .get("query")
        .and_then(Value::as_str)
        .ok_or_else(|| FsmError::invalid("missing required string parameter 'query'"))?;
    let limit = parse_limit(params, default_limit);
    Ok((query, limit))
}

/// Extract `limit` (or legacy `max_results`) from `params`, clamped to at least 1.
pub fn parse_limit(params: &Value, default_limit: usize) -> usize {
    let raw = params
        .get("limit")
        .or_else(|| params.get("max_results"))
        .and_then(Value::as_i64);
    match raw {
        Some(n) if n <= 0 => 1,
        Some(n) => usize::try_from(n).unwrap_or(default_limit),
        None => default_limit.max(1),
    }
}

/// Resolve effective freshness from `cx.freshness` or fallback `params["freshness"]`.
pub fn effective_freshness(params: &Value, cx: &SrcCx) -> Option<Freshness> {
    if let Some(f) = cx.freshness {
        return Some(f);
    }
    match params
        .get("freshness")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("day") => Some(Freshness::Day),
        Some("week") => Some(Freshness::Week),
        Some("month") => Some(Freshness::Month),
        Some("year") => Some(Freshness::Year),
        _ => None,
    }
}

/// Split requested domain filters into `(include_domains, exclude_domains)`.
///
/// Reads `cx.domains` (`"example.com"` includes, `"-example.com"` excludes), falling
/// back to `params["domains"]` or `params["include_domains"]` / `params["exclude_domains"]`.
pub fn effective_domains(params: &Value, cx: &SrcCx) -> (Vec<String>, Vec<String>) {
    let mut include = Vec::new();
    let mut exclude = Vec::new();
    let push_signed = |raw: &str, inc: &mut Vec<String>, exc: &mut Vec<String>| {
        let s = raw.trim();
        if let Some(rest) = s.strip_prefix('-') {
            let d = rest.trim().trim_start_matches('.').to_ascii_lowercase();
            if !d.is_empty() {
                exc.push(d);
            }
        } else {
            let d = s.trim_start_matches('.').to_ascii_lowercase();
            if !d.is_empty() {
                inc.push(d);
            }
        }
    };
    if !cx.domains.is_empty() {
        for d in &cx.domains {
            push_signed(d, &mut include, &mut exclude);
        }
        return (include, exclude);
    }
    if let Some(arr) = params.get("domains").and_then(Value::as_array) {
        for v in arr {
            if let Some(s) = v.as_str() {
                push_signed(s, &mut include, &mut exclude);
            }
        }
    }
    if let Some(arr) = params.get("include_domains").and_then(Value::as_array) {
        for v in arr {
            if let Some(s) = v.as_str() {
                let d = s.trim().trim_start_matches('.').to_ascii_lowercase();
                if !d.is_empty() {
                    include.push(d);
                }
            }
        }
    }
    if let Some(arr) = params.get("exclude_domains").and_then(Value::as_array) {
        for v in arr {
            if let Some(s) = v.as_str() {
                let d = s.trim().trim_start_matches('.').to_ascii_lowercase();
                if !d.is_empty() {
                    exclude.push(d);
                }
            }
        }
    }
    (include, exclude)
}

/// Apply client-side domain include/exclude filtering before truncating to `limit`.
pub fn filter_and_truncate_hits(
    mut hits: Vec<Hit>,
    include_domains: &[String],
    exclude_domains: &[String],
    limit: usize,
) -> Vec<Hit> {
    if !include_domains.is_empty() || !exclude_domains.is_empty() {
        let inc_refs: Vec<&str> = include_domains.iter().map(String::as_str).collect();
        let exc_refs: Vec<&str> = exclude_domains.iter().map(String::as_str).collect();
        hits.retain(|h| {
            let host = host_of(&h.url);
            if !inc_refs.is_empty() && !host_matches(&host, &inc_refs) {
                return false;
            }
            if !exc_refs.is_empty() && host_matches(&host, &exc_refs) {
                return false;
            }
            true
        });
    }
    hits.truncate(limit);
    hits
}

/// Construct a [`Hit`] stamped with source metadata.
pub fn make_hit(
    source_id: &str,
    title: String,
    url: String,
    snippet: String,
    date: Option<String>,
    date_source: Option<&str>,
    kind: HitKind,
) -> Hit {
    let clean_date = date.filter(|d| !d.is_empty());
    let clean_date_source = clean_date
        .as_ref()
        .and_then(|_| date_source.map(str::to_owned));
    let mut hit = Hit::new(title, url);
    hit.snippet = snippet;
    hit.date = clean_date;
    hit.date_source = clean_date_source;
    hit.source = source_id.to_owned();
    hit.sources = vec![source_id.to_owned()];
    hit.kind = kind;
    hit
}

/// Shared deadline budget across all sequential/parallel HTTP requests of one `Source::call`.
#[derive(Debug, Clone, Copy)]
pub struct CallBudget {
    started: Instant,
    total: Duration,
}

impl CallBudget {
    pub fn from_cx(cx: &SrcCx, decl: &SourceDecl) -> Self {
        let total = if !cx.timeout.is_zero() {
            cx.timeout
        } else if decl.limits.timeout_ms > 0 {
            Duration::from_millis(decl.limits.timeout_ms)
        } else {
            Duration::from_secs(4)
        };
        Self {
            started: Instant::now(),
            total,
        }
    }

    /// Build a [`NetCx`] carrying the remaining budget, or fail fast on cancel / expiry.
    pub fn net_cx(&self, cx: &SrcCx, decl: &SourceDecl) -> Result<NetCx, FsmError> {
        if cx.cancel.is_cancelled() {
            return Err(FsmError::Timeout(0));
        }
        let elapsed = self.started.elapsed();
        if elapsed >= self.total {
            return Err(FsmError::Timeout(
                u64::try_from(self.total.as_millis()).unwrap_or(u64::MAX),
            ));
        }
        let remaining = self.total.saturating_sub(elapsed);
        let mut ncx = NetCx::new(cx.cancel.clone(), remaining);
        ncx.host_bucket = decl.limits.host_bucket.clone();
        if decl.limits.rps > 0.0 {
            ncx.rps = Some(decl.limits.rps);
        }
        Ok(ncx)
    }
}

/// Whether an error is a fatal harness/cancellation signal that MUST propagate
/// immediately even during multi-request fan-out or sequential fallbacks.
pub fn is_fatal_fanout_error(err: &FsmError) -> bool {
    matches!(err, FsmError::Internal { .. } | FsmError::Timeout(0))
}

/// Resolve an optional API key from `cx.api_key` or `cfg.api_key(env_var)`.
pub fn resolve_optional_key(cx: &SrcCx, cfg: &Config, env_var: &str) -> Option<String> {
    cx.api_key
        .as_deref()
        .or_else(|| cfg.api_key(env_var))
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_owned)
}

/// Resolve a required API key, returning `FsmError::AuthRequired` when absent.
///
/// In `cx.replay_mode` (offline fixture replay), falls back to a synthetic placeholder
/// token that is marked redacted on the outgoing request (A1 §B.9 #4).
pub fn resolve_required_key(
    source_id: &str,
    env_var: &str,
    cx: &SrcCx,
    cfg: &Config,
) -> Result<String, FsmError> {
    if let Some(k) = resolve_optional_key(cx, cfg, env_var) {
        return Ok(k);
    }
    if cx.replay_mode {
        return Ok("__replay_key__".to_owned());
    }
    Err(FsmError::AuthRequired {
        source_id: source_id.to_owned(),
        env_var: env_var.to_owned(),
    })
}

/// Parse a `Retry-After` delta-seconds header if present.
pub fn retry_after_seconds(resp: &HttpBytes) -> Option<u64> {
    let raw = resp.header("retry-after")?.trim();
    raw.parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map(|v| v.ceil() as u64)
}

/// Decode HTTP response bytes honoring a `charset=` Content-Type parameter, defaulting to UTF-8.
pub fn decode_body(resp: &HttpBytes) -> String {
    if let Some(ct) = resp.header("content-type")
        && let Some(charset) = extract_charset(ct)
        && let Some(enc) = encoding_rs::Encoding::for_label(charset.as_bytes())
    {
        let (cow, _, _) = enc.decode(&resp.body);
        return cow.into_owned();
    }
    String::from_utf8_lossy(&resp.body).into_owned()
}

fn extract_charset(content_type: &str) -> Option<&str> {
    for part in content_type.split(';').skip(1) {
        let (k, v) = part.split_once('=')?;
        if k.trim().eq_ignore_ascii_case("charset") {
            let trimmed = v.trim().trim_matches('"').trim_matches('\'');
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }
    }
    None
}

fn body_preview(resp: &HttpBytes) -> String {
    let text = decode_body(resp);
    let snippet: String = text.trim().chars().take(200).collect();
    if snippet.is_empty() {
        format!("HTTP {}", resp.status)
    } else {
        format!("HTTP {}: {snippet}", resp.status)
    }
}

/// Send an HTTP request for a **keyless** JSON API engine and parse its JSON body.
///
/// Cross-lane I2 convention:
/// - `404` -> `Ok(None)` (resource/entity not found on REST lookup endpoints)
/// - `429` -> `Err(FsmError::RateLimited { source_id, retry_after_s })`
/// - Any other non-2xx or transport error -> `Err(FsmError::Upstream / Timeout / ...)`
/// - `2xx` with unparsable body -> `Err(FsmError::Upstream { status: Some(status), "invalid JSON: ..." })`
pub async fn send_json_keyless(
    net: &dyn NetClient,
    req: HttpRequest,
    profile: TransportProfile,
    decl: &SourceDecl,
    cx: &SrcCx,
    budget: &CallBudget,
) -> Result<Option<Value>, FsmError> {
    let ncx = budget.net_cx(cx, decl)?;
    let resp = net.send(req, profile, &ncx).await?;
    if resp.status == 404 {
        return Ok(None);
    }
    if resp.status == 429 {
        return Err(FsmError::RateLimited {
            source_id: decl.id.clone(),
            retry_after_s: retry_after_seconds(&resp),
        });
    }
    if !resp.is_success() {
        return Err(FsmError::upstream(
            &decl.id,
            Some(resp.status),
            body_preview(&resp),
        ));
    }
    if resp.status == 204 && resp.body.is_empty() {
        return Ok(None);
    }
    let text = decode_body(&resp);
    serde_json::from_str::<Value>(&text)
        .map(Some)
        .map_err(|err| {
            FsmError::upstream(&decl.id, Some(resp.status), format!("invalid JSON: {err}"))
        })
}

/// Map a keyed engine's non-2xx HTTP status to an actionable [`FsmError`].
pub fn keyed_http_error(source_id: &str, env_var: &str, resp: &HttpBytes) -> FsmError {
    match resp.status {
        401 | 403 => FsmError::upstream(
            source_id,
            Some(resp.status),
            format!(
                "{source_id}: the configured credential was rejected (HTTP {}). Check {env_var}.",
                resp.status
            ),
        ),
        422 => FsmError::upstream(
            source_id,
            Some(422),
            format!(
                "{source_id}: the API rejected the request (HTTP 422), usually due to an invalid key or malformed parameters."
            ),
        ),
        429 => FsmError::RateLimited {
            source_id: source_id.to_owned(),
            retry_after_s: retry_after_seconds(resp),
        },
        _ => FsmError::upstream(source_id, Some(resp.status), body_preview(resp)),
    }
}

/// Send an HTTP request for a **keyed** JSON API engine.
///
/// Cross-lane I2 convention: every transport failure, non-2xx status, and unparsable
/// response body returns an `FsmError` carrying `source_id` and `status`.
pub async fn send_json_keyed(
    net: &dyn NetClient,
    req: HttpRequest,
    profile: TransportProfile,
    decl: &SourceDecl,
    env_var: &str,
    cx: &SrcCx,
    budget: &CallBudget,
) -> Result<Value, FsmError> {
    let ncx = budget.net_cx(cx, decl)?;
    let resp = net.send(req, profile, &ncx).await?;
    if !resp.is_success() {
        return Err(keyed_http_error(&decl.id, env_var, &resp));
    }
    let text = decode_body(&resp);
    serde_json::from_str::<Value>(&text).map_err(|err| {
        FsmError::upstream(&decl.id, Some(resp.status), format!("invalid JSON: {err}"))
    })
}

/// Extract today's UTC date as `jiff::civil::Date`.
pub fn today_utc() -> jiff::civil::Date {
    Timestamp::now()
        .to_zoned_datetime(jiff::tz::TimeZone::UTC)
        .date()
}
