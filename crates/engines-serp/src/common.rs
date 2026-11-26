//! Shared SERP helpers: request parameter parsing, HTML/text helpers, query
//! operators, locale/script routing, charset decoding, wall detection, and
//! hit finalization.

use dom_query::{Document, NodeRef, Selection};
use fsm_core::api::Freshness;
use fsm_core::config::{Config, SafeSearch};
use fsm_core::dates::extract_date_hint;
use fsm_core::net::{HttpBytes, HttpRequest, NetCx};
use fsm_core::source::{OpDecl, Param, ParamType, ProbeSpec, ReturnKind, SrcCx, TransportProfile};
use fsm_core::types::{Hit, HitKind};
use fsm_core::url_canon::{host_matches, host_of};
use fsm_core::{FsmError, SourceDeps};
use jiff::Timestamp;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use regex::Regex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

/// Default `Accept` header for HTML SERP requests (matches Python `base.py`).
pub const ACCEPT_HTML: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";

/// Default `Accept-Language` fallback when both `SrcCx` and `Config` are empty.
pub const DEFAULT_ACCEPT_LANGUAGE: &str = "en-US,en;q=0.9";

/// Shared gate markers checked AFTER engine-specific markers, ONLY when a page
/// parses to zero results (port of `engines/base.py::_GATE_MARKERS` + Spike N).
pub const SHARED_GATE_MARKERS: &[(&str, &[&str])] = &[
    (
        "captcha",
        &[
            "/sorry/index",
            "unusual traffic",
            "/recaptcha/",
            "g-recaptcha",
            "h-captcha",
            "captcha-delivery",
            "px-captcha",
            "are you a robot",
            "verify you are a human",
            "anomaly-modal",
            "made by a human",
            "captcha-wrap",
            "altcha",
            "verification required",
            "complete the challenge",
            "anubis",
            "schedule a captcha",
            "showcaptcha",
            "startpage blocked",
        ],
    ),
    (
        "javascript",
        &[
            "if you are not redirected",
            "please click here if you are not",
            "enable javascript to continue",
            "javascript is required",
        ],
    ),
    (
        "consent",
        &[
            "consent.google.com",
            "consent.youtube.com",
            "before you continue",
            "consent.bing.com",
        ],
    ),
    (
        "login",
        &[
            "请登录",
            "登录知乎",
            "signflow",
            "sign in to continue",
            "you must log in",
            "please log in to continue",
        ],
    ),
];

/// Standard `op.search` declaration shared by all 13 native SERP engines.
pub fn search_op_decl(summary: &str, example_query: &str, source_id: &str) -> OpDecl {
    OpDecl::new("search", summary, ReturnKind::Hits)
        .with_param(Param::new("query", ParamType::String, "Search query").required())
        .with_param(
            Param::new("limit", ParamType::Int, "Maximum results to return")
                .with_default(json!(10))
                .with_range(1.0, 50.0),
        )
        .with_param(
            Param::new("freshness", ParamType::String, "Optional time filter")
                .with_enum(&["day", "week", "month", "year"]),
        )
        .with_param(Param::new(
            "domains",
            ParamType::StringArray,
            "Domain filters ('example.com' include, '-example.com' exclude)",
        ))
        .with_example(format!(
            "source.call(\"{source_id}.search\", {{ query: \"{example_query}\", limit: 10 }})"
        ))
        .with_probe(ProbeSpec::new(
            json!({"query": example_query, "limit": 3}),
            1,
        ))
}

/// Normalized input for a SERP `search` call.
#[derive(Debug, Clone)]
pub struct SerpQuery {
    pub query: String,
    pub limit: usize,
    pub freshness: Option<Freshness>,
    pub include_domains: Vec<String>,
    pub exclude_domains: Vec<String>,
    pub filetype: Option<String>,
    pub region: String,
    pub safesearch: SafeSearch,
    pub accept_language: String,
    pub timeout: Duration,
}

impl SerpQuery {
    /// Parse and validate `op` + `params` + `SrcCx` for a SERP engine.
    pub fn from_call(
        source_id: &str,
        op: &str,
        params: &Value,
        cx: &SrcCx,
        cfg: &Config,
    ) -> Result<Self, FsmError> {
        if op != "search" {
            return Err(FsmError::invalid(format!(
                "source '{source_id}' has no op '{op}'; call '{source_id}.search'"
            )));
        }
        let query = params
            .get("query")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .ok_or_else(|| FsmError::invalid("query must not be empty"))?
            .to_owned();

        let limit = match params.get("limit") {
            None | Some(Value::Null) => 10usize,
            Some(v) => {
                let n = v.as_i64().ok_or_else(|| {
                    FsmError::invalid("limit must be an integer between 1 and 50")
                })?;
                n.clamp(1, 50) as usize
            }
        };

        let freshness = match params.get("freshness") {
            Some(Value::String(s)) if !s.trim().is_empty() => Some(parse_freshness(s)?),
            Some(Value::Null) | None => cx.freshness,
            Some(_) => {
                return Err(FsmError::invalid(
                    "freshness must be one of: day, week, month, year",
                ));
            }
        };

        let mut include_domains = Vec::new();
        let mut exclude_domains = Vec::new();
        // Read domains from SrcCx first, then optional params overrides/additions.
        for d in &cx.domains {
            push_domain_token(d, &mut include_domains, &mut exclude_domains);
        }
        if let Some(arr) = params.get("domains").and_then(Value::as_array) {
            for item in arr {
                if let Some(s) = item.as_str() {
                    push_domain_token(s, &mut include_domains, &mut exclude_domains);
                }
            }
        }
        if let Some(arr) = params.get("include_domains").and_then(Value::as_array) {
            for item in arr {
                if let Some(s) = item.as_str() {
                    let clean = s.trim().trim_start_matches('.').to_ascii_lowercase();
                    if !clean.is_empty() && !include_domains.contains(&clean) {
                        include_domains.push(clean);
                    }
                }
            }
        }
        if let Some(arr) = params.get("exclude_domains").and_then(Value::as_array) {
            for item in arr {
                if let Some(s) = item.as_str() {
                    let clean = s
                        .trim()
                        .trim_start_matches('-')
                        .trim_start_matches('.')
                        .to_ascii_lowercase();
                    if !clean.is_empty() && !exclude_domains.contains(&clean) {
                        exclude_domains.push(clean);
                    }
                }
            }
        }

        let filetype = params
            .get("filetype")
            .and_then(Value::as_str)
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty());

        let accept_language = if !cx.accept_language.trim().is_empty() {
            cx.accept_language.clone()
        } else if !cfg.accept_language.trim().is_empty() {
            cfg.accept_language.clone()
        } else {
            DEFAULT_ACCEPT_LANGUAGE.to_owned()
        };

        let timeout = if cx.timeout.is_zero() {
            Duration::from_secs_f64(cfg.request_timeout_s.max(0.5))
        } else {
            cx.timeout
        };

        Ok(Self {
            query,
            limit,
            freshness,
            include_domains,
            exclude_domains,
            filetype,
            region: cx.region.clone(),
            safesearch: cx.safesearch,
            accept_language,
            timeout,
        })
    }
}

fn push_domain_token(token: &str, include: &mut Vec<String>, exclude: &mut Vec<String>) {
    let t = token.trim();
    if t.is_empty() {
        return;
    }
    if let Some(rest) = t.strip_prefix('-') {
        let clean = rest.trim().trim_start_matches('.').to_ascii_lowercase();
        if !clean.is_empty() && !exclude.contains(&clean) {
            exclude.push(clean);
        }
    } else {
        let clean = t
            .strip_prefix('+')
            .unwrap_or(t)
            .trim()
            .trim_start_matches('.')
            .to_ascii_lowercase();
        if !clean.is_empty() && !include.contains(&clean) {
            include.push(clean);
        }
    }
}

pub fn parse_freshness(s: &str) -> Result<Freshness, FsmError> {
    match s.trim().to_ascii_lowercase().as_str() {
        "day" => Ok(Freshness::Day),
        "week" => Ok(Freshness::Week),
        "month" => Ok(Freshness::Month),
        "year" => Ok(Freshness::Year),
        other => Err(FsmError::invalid(format!(
            "invalid freshness '{other}' (expected day, week, month, year)"
        ))),
    }
}

pub fn freshness_as_str(f: Freshness) -> &'static str {
    match f {
        Freshness::Day => "day",
        Freshness::Week => "week",
        Freshness::Month => "month",
        Freshness::Year => "year",
        _ => "week",
    }
}

/// Form-urlencode a query parameter value (`+` for space, matching Python `quote_plus`).
pub fn quote_plus(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// Append `site:` / `-site:` / `filetype:` operators to a free-text query
/// (exact port of `engines/base.py::augment_query_with_operators`).
pub fn augment_query_with_operators(
    query: &str,
    include_domains: &[String],
    exclude_domains: &[String],
    filetype: Option<&str>,
) -> String {
    let mut parts: Vec<String> = vec![query.to_owned()];
    if !include_domains.is_empty() {
        if include_domains.len() == 1 {
            parts.push(format!("site:{}", include_domains[0]));
        } else {
            let joined = include_domains
                .iter()
                .map(|d| format!("site:{d}"))
                .collect::<Vec<_>>()
                .join(" OR ");
            parts.push(format!("({joined})"));
        }
    }
    for d in exclude_domains {
        parts.push(format!("-site:{d}"));
    }
    if let Some(ft) = filetype.filter(|s| !s.is_empty()) {
        parts.push(format!("filetype:{ft}"));
    }
    parts.join(" ")
}

/// Turn a `'cc-lang'` region token (`"us-en"`, `"uk-en"`) into a Bing `mkt`
/// code (`"en-US"`, `"en-GB"`).
pub fn region_to_bing_market(region: &str) -> String {
    let Some((cc_raw, lang_raw)) = region.split_once('-') else {
        return "en-US".to_owned();
    };
    let mut cc = cc_raw.trim().to_ascii_uppercase();
    let lang = lang_raw.trim().to_ascii_lowercase();
    let lang = if lang.is_empty() { "en" } else { lang.as_str() };
    if cc == "UK" {
        cc = "GB".to_owned();
    }
    format!("{lang}-{cc}")
}

/// Turn a `'cc-lang'` region token into Google's `(hl, gl)` pair.
pub fn region_to_google_params(region: &str) -> (String, String) {
    let Some((cc_raw, lang_raw)) = region.split_once('-') else {
        return ("en-US".to_owned(), "US".to_owned());
    };
    let mut cc = cc_raw.trim().to_ascii_uppercase();
    let lang_trimmed = lang_raw.trim().to_ascii_lowercase();
    let lang = if lang_trimmed.is_empty() {
        "en"
    } else {
        lang_trimmed.as_str()
    };
    if cc == "UK" {
        cc = "GB".to_owned();
    }
    if !(cc.len() == 2
        && cc.bytes().all(|b| b.is_ascii_alphabetic())
        && !lang.is_empty()
        && lang.bytes().all(|b| b.is_ascii_alphabetic()))
    {
        return ("en-US".to_owned(), "US".to_owned());
    }
    (format!("{lang}-{cc}"), cc)
}

/// Google News edition token, e.g. `"US:en"`, `"CN:zh-Hans"`, `"TW:zh-Hant"`.
pub fn region_to_google_news_ceid(region: &str) -> String {
    let (hl, gl) = region_to_google_params(region);
    let lang = hl.split_once('-').map_or(hl.as_str(), |(l, _)| l);
    let script = match (lang, gl.as_str()) {
        ("zh", "TW" | "HK" | "MO") => "zh-Hant",
        ("zh", "CN" | "SG") => "zh-Hans",
        _ => lang,
    };
    format!("{gl}:{script}")
}

const SCRIPT_SWITCH_RATIO: f64 = 0.3;

const SCRIPT_RANGES: &[(&str, &[(u32, u32)])] = &[
    (
        "hangul",
        &[(0x1100, 0x11FF), (0xA960, 0xA97F), (0xAC00, 0xD7A3)],
    ),
    (
        "kana",
        &[(0x3040, 0x309F), (0x30A0, 0x30FF), (0x31F0, 0x31FF)],
    ),
    (
        "han",
        &[(0x3400, 0x4DBF), (0x4E00, 0x9FFF), (0xF900, 0xFAFF)],
    ),
    ("cyrillic", &[(0x0400, 0x04FF), (0x0500, 0x052F)]),
    ("greek", &[(0x0370, 0x03FF), (0x1F00, 0x1FFF)]),
    ("hebrew", &[(0x0590, 0x05FF)]),
    (
        "arabic",
        &[(0x0600, 0x06FF), (0x0750, 0x077F), (0x08A0, 0x08FF)],
    ),
    ("thai", &[(0x0E00, 0x0E7F)]),
    ("lao", &[(0x0E80, 0x0EFF)]),
    ("khmer", &[(0x1780, 0x17FF)]),
    ("myanmar", &[(0x1000, 0x109F)]),
    ("devanagari", &[(0x0900, 0x097F)]),
    ("bengali", &[(0x0980, 0x09FF)]),
    ("gurmukhi", &[(0x0A00, 0x0A7F)]),
    ("gujarati", &[(0x0A80, 0x0AFF)]),
    ("tamil", &[(0x0B80, 0x0BFF)]),
    ("telugu", &[(0x0C00, 0x0C7F)]),
    ("kannada", &[(0x0C80, 0x0CFF)]),
    ("malayalam", &[(0x0D00, 0x0D7F)]),
    ("sinhala", &[(0x0D80, 0x0DFF)]),
    ("georgian", &[(0x10A0, 0x10FF), (0x1C90, 0x1CBF)]),
    ("armenian", &[(0x0530, 0x058F)]),
    ("ethiopic", &[(0x1200, 0x137F)]),
];

fn script_to_region(script: &str) -> Option<&'static str> {
    Some(match script {
        "han" => "cn-zh",
        "kana" => "jp-ja",
        "hangul" => "kr-ko",
        "cyrillic" => "ru-ru",
        "greek" => "gr-el",
        "hebrew" => "il-he",
        "arabic" => "eg-ar",
        "thai" => "th-th",
        "lao" => "la-lo",
        "khmer" => "kh-km",
        "myanmar" => "mm-my",
        "devanagari" => "in-hi",
        "bengali" => "in-bn",
        "gurmukhi" => "in-pa",
        "gujarati" => "in-gu",
        "tamil" => "in-ta",
        "telugu" => "in-te",
        "kannada" => "in-kn",
        "malayalam" => "in-ml",
        "sinhala" => "lk-si",
        "georgian" => "ge-ka",
        "armenian" => "am-hy",
        "ethiopic" => "et-am",
        _ => return None,
    })
}

/// Detect query script and return the appropriate region token when the
/// configured region cannot serve it (exact port of `base.py::detect_query_region`).
pub fn detect_query_region(query: &str, configured: &str) -> String {
    let mut counts: HashMap<&'static str, usize> = HashMap::new();
    let mut letters = 0usize;
    for ch in query.chars() {
        if !ch.is_alphabetic() {
            continue;
        }
        letters += 1;
        let cp = ch as u32;
        for &(script, ranges) in SCRIPT_RANGES {
            if ranges.iter().any(|&(lo, hi)| cp >= lo && cp <= hi) {
                *counts.entry(script).or_insert(0) += 1;
                break;
            }
        }
    }
    if letters == 0 || counts.is_empty() {
        return configured.to_owned();
    }

    let cjk = counts.get("han").copied().unwrap_or(0)
        + counts.get("kana").copied().unwrap_or(0)
        + counts.get("hangul").copied().unwrap_or(0);
    let (script, n) = if counts.contains_key("kana") || counts.contains_key("hangul") {
        if (cjk as f64) / (letters as f64) < SCRIPT_SWITCH_RATIO {
            return configured.to_owned();
        }
        let s = if counts.contains_key("hangul") {
            "hangul"
        } else {
            "kana"
        };
        (s, cjk)
    } else {
        // Deterministic max (tie-break by SCRIPT_RANGES order).
        let mut best = ("", 0usize);
        for &(s, _) in SCRIPT_RANGES {
            let c = counts.get(s).copied().unwrap_or(0);
            if c > best.1 {
                best = (s, c);
            }
        }
        best
    };

    if (n as f64) / (letters as f64) < SCRIPT_SWITCH_RATIO {
        return configured.to_owned();
    }

    let detected = if script == "arabic" {
        if query.chars().any(|c| "ٹڈڑںھے".contains(c)) {
            "pk-ur"
        } else if query.chars().any(|c| "پچژگک".contains(c)) {
            "ir-fa"
        } else {
            "eg-ar"
        }
    } else {
        match script_to_region(script) {
            Some(r) => r,
            None => return configured.to_owned(),
        }
    };

    let conf_lang = configured
        .split_once('-')
        .map_or("", |(_, l)| l.trim())
        .to_ascii_lowercase();
    let det_lang = detected.split_once('-').map_or("", |(_, l)| l);
    if !conf_lang.is_empty() && conf_lang == det_lang {
        return configured.to_owned();
    }
    detected.to_owned()
}

/// DuckDuckGo `kp` parameter value for `SafeSearch`.
pub fn ddg_safesearch(ss: SafeSearch) -> &'static str {
    match ss {
        SafeSearch::Strict => "1",
        SafeSearch::Moderate => "-1",
        SafeSearch::Off => "-2",
        _ => "-1",
    }
}

/// Bing `adlt` parameter value for `SafeSearch`.
pub fn bing_safesearch(ss: SafeSearch) -> &'static str {
    ss.as_str()
}

/// Parse HTTP response body respecting `Content-Type` charset, falling back to
/// UTF-8 and then `chardetng` + `encoding_rs`.
pub fn decode_body(resp: &HttpBytes) -> String {
    if let Some(ct) = resp.header("content-type")
        && let Some(charset) = extract_charset(ct)
        && let Some(enc) = encoding_rs::Encoding::for_label(charset.as_bytes())
    {
        let (cow, _, _) = enc.decode(&resp.body);
        return cow.into_owned();
    }
    if let Ok(s) = std::str::from_utf8(&resp.body) {
        return s.to_owned();
    }
    let mut det = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Allow);
    det.feed(&resp.body, true);
    let enc = det.guess(None, chardetng::Utf8Detection::Allow);
    let (cow, _, _) = enc.decode(&resp.body);
    cow.into_owned()
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

/// Parse integer `Retry-After` delta-seconds header (if present and non-negative).
pub fn parse_retry_after(resp: &HttpBytes) -> Option<u64> {
    let raw = resp.header("retry-after")?.trim();
    if let Ok(secs) = raw.parse::<u64>() {
        return Some(secs);
    }
    if let Ok(f) = raw.parse::<f64>()
        && f.is_finite()
        && f >= 0.0
    {
        return Some(f.ceil() as u64);
    }
    None
}

/// Collapse runs of Unicode whitespace to a single ASCII space and trim.
pub fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Port of Python `base.py::text_of`: join every descendant text node with a
/// single space and collapse whitespace.
pub fn text_of_node(node: &NodeRef<'_>) -> String {
    let mut buf = String::new();
    for desc in node.descendants() {
        if desc.is_text() {
            let t = desc.text();
            let trimmed = t.trim();
            if !trimmed.is_empty() {
                if !buf.is_empty() {
                    buf.push(' ');
                }
                buf.push_str(trimmed);
            }
        }
    }
    collapse_ws(&buf)
}

/// Port of Python `base.py::text_of` on a `Selection` (uses first matched node).
pub fn text_of(sel: &Selection<'_>) -> String {
    sel.nodes().first().map(text_of_node).unwrap_or_default()
}

/// Port of Python `base.py::inline_text`: preserve the page's own inline
/// spacing across `<b>`/`<em>`/`<mark>` boundaries, collapsing whitespace.
pub fn inline_text_node(node: &NodeRef<'_>) -> String {
    collapse_ws(&node.text())
}

/// Port of Python `base.py::inline_text` on a `Selection` (uses first matched node).
pub fn inline_text(sel: &Selection<'_>) -> String {
    sel.nodes()
        .first()
        .map(inline_text_node)
        .unwrap_or_default()
}

/// Port of Python `base.py::enclosing_anchor`: nearest `<a>` at or above `node`.
pub fn enclosing_anchor<'a>(mut node: NodeRef<'a>) -> Option<NodeRef<'a>> {
    loop {
        if node
            .node_name()
            .is_some_and(|n| n.eq_ignore_ascii_case("a"))
        {
            return Some(node);
        }
        node = node.parent()?;
    }
}

/// Parse an HTML fragment and return its `inline_text` (used by Seznam).
pub fn plain_html_fragment(fragment: &str) -> String {
    if fragment.is_empty() {
        return String::new();
    }
    let doc = Document::fragment(fragment);
    collapse_ws(&doc.root().text())
}

static HTML_TAG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]+>").expect("regex"));

/// Strip HTML tags (replacing each tag with a space), decode HTML entities,
/// and collapse whitespace (matches Python `googlenews._strip_html` and
/// `bilibili._strip_html`).
pub fn strip_html_tags(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }
    let no_tags = HTML_TAG_RE.replace_all(input, " ");
    let decoded = unescape_html(&no_tags);
    collapse_ws(&decoded)
}

/// Longest HTML5 named character reference is 31 ASCII chars
/// (`CounterClockwiseContourIntegral`).
const MAX_ENTITY_LEN: usize = 32;

/// Decode HTML5 named entities and numeric (`&#...;` / `&#x...;`) refs in
/// guaranteed O(N) linear time.
pub fn unescape_html(input: &str) -> String {
    let Some(first_amp) = input.find('&') else {
        return input.to_owned();
    };
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut last = 0usize;
    let mut i = first_amp;
    while i < bytes.len() {
        if bytes[i] == b'&'
            && let Some(&next) = bytes.get(i + 1)
            && (next == b'#' || next.is_ascii_alphabetic())
            && let Some(rel_semi) = find_entity_end(bytes, i + 1)
        {
            let entity = &input[i + 1..i + 1 + rel_semi];
            if let Some(decoded) = decode_entity(entity) {
                out.push_str(&input[last..i]);
                out.push_str(&decoded);
                i += rel_semi + 2;
                last = i;
                continue;
            }
        }
        i += 1;
        while i < bytes.len() && bytes[i] != b'&' {
            i += 1;
        }
    }
    if last == 0 {
        return input.to_owned();
    }
    out.push_str(&input[last..]);
    out
}

/// Bounded scan for the closing `;` of an HTML character reference starting at
/// `start` (right after `&`). Stops immediately on any byte that cannot appear
/// inside an entity name or numeric reference, keeping `unescape_html` strictly
/// O(N) even on pathological runs of `&`.
fn find_entity_end(bytes: &[u8], start: usize) -> Option<usize> {
    let limit = bytes.len().min(start + MAX_ENTITY_LEN + 1);
    for (offset, &b) in bytes[start..limit].iter().enumerate() {
        if b == b';' {
            return (offset > 0).then_some(offset);
        }
        let allowed = if offset == 0 {
            b == b'#' || b.is_ascii_alphabetic()
        } else {
            b.is_ascii_alphanumeric()
        };
        if !allowed {
            return None;
        }
    }
    None
}

fn decode_entity(entity: &str) -> Option<String> {
    if let Some(num) = entity.strip_prefix('#') {
        let code = if let Some(hex) = num.strip_prefix('x').or_else(|| num.strip_prefix('X')) {
            u32::from_str_radix(hex, 16).ok()?
        } else {
            num.parse::<u32>().ok()?
        };
        let ch = char::from_u32(code)?;
        return Some(ch.to_string());
    }
    let fast = match entity {
        "amp" => Some("&"),
        "lt" => Some("<"),
        "gt" => Some(">"),
        "quot" => Some("\""),
        "apos" => Some("'"),
        "nbsp" => Some("\u{00a0}"),
        "ndash" => Some("\u{2013}"),
        "mdash" => Some("\u{2014}"),
        "lsquo" => Some("\u{2018}"),
        "rsquo" => Some("\u{2019}"),
        "ldquo" => Some("\u{201c}"),
        "rdquo" => Some("\u{201d}"),
        "hellip" => Some("\u{2026}"),
        "copy" => Some("\u{00a9}"),
        "reg" => Some("\u{00ae}"),
        "trade" => Some("\u{2122}"),
        "middot" => Some("\u{00b7}"),
        "bull" => Some("\u{2022}"),
        "laquo" => Some("\u{00ab}"),
        "raquo" => Some("\u{00bb}"),
        "rsaquo" => Some("\u{203a}"),
        "lsaquo" => Some("\u{2039}"),
        "semi" => Some(";"),
        _ => None,
    };
    if let Some(s) = fast {
        return Some(s.to_owned());
    }
    // Full HTML5 named character reference fallback via `html5ever` (through
    // `dom_query`), only reached for valid `[A-Za-z][A-Za-z0-9]*` names.
    // Reject partial legacy-prefix matches (e.g. `&not` inside `&notanentity;`),
    // which leave the trailing `;` unconsumed.
    if entity.bytes().all(|b| b.is_ascii_alphanumeric()) {
        let raw = format!("&{entity};");
        let parsed = Document::fragment(format!("<span>{raw}</span>"))
            .root()
            .text();
        if parsed.as_ref() != raw.as_str() && !parsed.ends_with(';') {
            return Some(parsed.to_string());
        }
    }
    None
}

/// Detect a gate/wall page when parsing returned 0 hits.
///
/// Engine-specific markers are checked BEFORE shared markers (per Spike N §4 #8
/// & `SPEC.md` §5). Callers MUST only invoke this when `hits.is_empty()`.
pub fn detect_gate(html: &str, engine_markers: &[(&'static str, &[&str])]) -> Option<&'static str> {
    detect_gate_with_exempt(html, engine_markers, &[])
}

/// Like [`detect_gate`], but skips any shared marker listed in `exempt_shared`
/// (case-insensitive) so engines whose normal SERP markup embeds a benign
/// colliding string (e.g. Naver's `refresh-captcha` colliding with `h-captcha`)
/// do not falsely report a wall on legitimately empty pages.
pub fn detect_gate_with_exempt(
    html: &str,
    engine_markers: &[(&'static str, &[&str])],
    exempt_shared: &[&str],
) -> Option<&'static str> {
    if html.is_empty() {
        return None;
    }
    let low = html.to_ascii_lowercase();
    for &(reason, markers) in engine_markers {
        if markers
            .iter()
            .any(|m| low.contains(&m.to_ascii_lowercase()))
        {
            return Some(reason);
        }
    }
    for &(reason, markers) in SHARED_GATE_MARKERS {
        if markers.iter().any(|m| {
            !exempt_shared.iter().any(|ex| ex.eq_ignore_ascii_case(m))
                && low.contains(&m.to_ascii_lowercase())
        }) {
            return Some(reason);
        }
    }
    None
}

/// Current UTC calendar date.
pub fn today_utc() -> Date {
    Timestamp::now().to_zoned_datetime(TimeZone::UTC).date()
}

/// Extract a date hint from snippet/title text using today's UTC date.
pub fn extract_date_hint_now(text: &str) -> Option<String> {
    extract_date_hint(text, today_utc())
}

/// Format a unix-seconds timestamp as UTC `YYYY-MM-DD`, or `None` if invalid.
pub fn unix_seconds_to_iso_date(secs: i64) -> Option<String> {
    if secs <= 0 {
        return None;
    }
    let ts = Timestamp::from_second(secs).ok()?;
    Some(ts.to_zoned_datetime(TimeZone::UTC).date().to_string())
}

/// Build a `Hit` with consistent source/sources/kind/date/date_source fields.
pub fn make_hit(
    source_id: &str,
    kind: HitKind,
    title: String,
    url: String,
    snippet: String,
    date: Option<String>,
    date_source: Option<&str>,
) -> Hit {
    let mut hit = Hit::new(title, url);
    hit.snippet = snippet;
    hit.source = source_id.to_owned();
    hit.sources = vec![source_id.to_owned()];
    hit.kind = kind;
    if let Some(d) = date.filter(|s| !s.is_empty()) {
        hit.date = Some(d);
        hit.date_source = date_source.map(str::to_owned);
    }
    hit
}

/// Apply client-side domain + PDF post-filters and truncate to `limit`.
pub fn finalize_hits(
    hits: Vec<Hit>,
    include_domains: &[String],
    exclude_domains: &[String],
    filetype: Option<&str>,
    limit: usize,
) -> Vec<Hit> {
    let inc_refs: Vec<&str> = include_domains.iter().map(String::as_str).collect();
    let exc_refs: Vec<&str> = exclude_domains.iter().map(String::as_str).collect();
    let want_pdf = filetype.is_some_and(|ft| ft.eq_ignore_ascii_case("pdf"));

    hits.into_iter()
        .filter(|h| {
            let host = host_of(&h.url);
            if !inc_refs.is_empty() && !host_matches(&host, &inc_refs) {
                return false;
            }
            if !exc_refs.is_empty() && host_matches(&host, &exc_refs) {
                return false;
            }
            if want_pdf {
                let base = h.url.split(['?', '#']).next().unwrap_or("");
                if !base.to_ascii_lowercase().ends_with(".pdf") {
                    return false;
                }
            }
            true
        })
        .take(limit)
        .collect()
}

/// Per-call deadline shared across every HTTP hop of one `Source::call()`.
#[derive(Debug, Clone, Copy)]
pub struct CallDeadline {
    deadline: Instant,
    budget: Duration,
}

impl CallDeadline {
    pub fn from_timeout(timeout: Duration) -> Self {
        Self {
            deadline: Instant::now() + timeout,
            budget: timeout,
        }
    }

    /// Remaining time before the call deadline, or `FsmError::Timeout` once
    /// expired.
    pub fn remaining(&self) -> Result<Duration, FsmError> {
        let now = Instant::now();
        if now >= self.deadline {
            return Err(FsmError::Timeout(
                u64::try_from(self.budget.as_millis()).unwrap_or(u64::MAX),
            ));
        }
        Ok(self.deadline.saturating_duration_since(now))
    }

    /// Remaining time (saturating at zero).
    pub fn remaining_or_zero(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
}

/// Build the primary `NetCx` for a search call (charges the engine's GCRA
/// bucket once per `call()`).
pub fn net_cx_for(cx: &SrcCx, q: &SerpQuery, source_id: &str, rps: f64) -> NetCx {
    let mut ncx = NetCx::new(cx.cancel.clone(), q.timeout);
    ncx.host_bucket = Some(source_id.to_owned());
    ncx.rps = Some(rps);
    ncx
}

/// Build a `NetCx` bounded by a shared [`CallDeadline`].
///
/// Pass `Some(rps)` for the single primary search hop that charges the engine's
/// rate limiter, and `None` for auxiliary hops (warm-ups, handshakes, redirects,
/// page 2, link resolution). Keeping `host_bucket = Some(source_id)` preserves
/// shared `Retry-After` penalties (`SPEC.md` §5) while letting auxiliary hops
/// run under the op deadline without re-charging the per-call bucket.
pub fn net_cx_with_deadline(
    cancel: &tokio_util::sync::CancellationToken,
    deadline: CallDeadline,
    source_id: &str,
    rps: Option<f64>,
) -> Result<NetCx, FsmError> {
    let mut ncx = NetCx::new(cancel.clone(), deadline.remaining()?);
    ncx.host_bucket = Some(source_id.to_owned());
    ncx.rps = rps;
    Ok(ncx)
}

/// Derive an auxiliary `NetCx` from `base`: clears `rps` so internal hops do
/// not re-charge the per-call rate limit, while keeping `host_bucket` and
/// clamping `timeout` to `deadline`.
pub fn aux_net_cx(base: &NetCx, deadline: CallDeadline) -> Result<NetCx, FsmError> {
    let mut aux = base.clone();
    aux.rps = None;
    aux.timeout = deadline.remaining()?;
    Ok(aux)
}

/// Send an HTML GET request through `deps.net` and map 429 / non-2xx errors.
pub async fn send_html_get(
    deps: &SourceDeps,
    source_id: &str,
    profile: TransportProfile,
    url: &str,
    accept_language: &str,
    ncx: &NetCx,
) -> Result<HttpBytes, FsmError> {
    let req = HttpRequest::get(url)
        .header("Accept", ACCEPT_HTML)
        .header("Accept-Language", accept_language)
        .for_source(source_id);
    let resp = deps.net.send(req, profile, ncx).await?;
    check_http_status(source_id, &resp)?;
    Ok(resp)
}

/// Map non-2xx HTTP status to `FsmError::RateLimited` (429) or `FsmError::Upstream`.
pub fn check_http_status(source_id: &str, resp: &HttpBytes) -> Result<(), FsmError> {
    if resp.is_success() {
        return Ok(());
    }
    if resp.status == 429 {
        return Err(FsmError::RateLimited {
            source_id: source_id.to_owned(),
            retry_after_s: parse_retry_after(resp),
        });
    }
    Err(FsmError::upstream(
        source_id,
        Some(resp.status),
        format!("{source_id}: HTTP {}", resp.status),
    ))
}

/// Check for a gate/wall ONLY when `hits` is empty; returns `Err(Upstream)`
/// naming the gate reason if matched, otherwise `Ok(hits)`.
pub fn check_wall_if_empty(
    source_id: &str,
    status: u16,
    html: &str,
    engine_markers: &[(&'static str, &[&str])],
    hits: Vec<Hit>,
) -> Result<Vec<Hit>, FsmError> {
    check_wall_if_empty_with_exempt(source_id, status, html, engine_markers, &[], hits)
}

/// Like [`check_wall_if_empty`], exempting specified shared gate markers.
pub fn check_wall_if_empty_with_exempt(
    source_id: &str,
    status: u16,
    html: &str,
    engine_markers: &[(&'static str, &[&str])],
    exempt_shared: &[&str],
    hits: Vec<Hit>,
) -> Result<Vec<Hit>, FsmError> {
    if hits.is_empty()
        && let Some(reason) = detect_gate_with_exempt(html, engine_markers, exempt_shared)
    {
        return Err(FsmError::upstream(
            source_id,
            Some(status),
            format!("{source_id} was {reason}-gated"),
        ));
    }
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn augment_operators_matches_python() {
        assert_eq!(
            augment_query_with_operators("asyncio", &[], &[], None),
            "asyncio"
        );
        assert_eq!(
            augment_query_with_operators(
                "asyncio",
                &["docs.python.org".into()],
                &["reddit.com".into()],
                Some("pdf")
            ),
            "asyncio site:docs.python.org -site:reddit.com filetype:pdf"
        );
        assert_eq!(
            augment_query_with_operators(
                "taskgroup",
                &["docs.python.org".into(), "peps.python.org".into()],
                &[],
                None
            ),
            "taskgroup (site:docs.python.org OR site:peps.python.org)"
        );
    }

    #[test]
    fn region_helpers_match_python() {
        assert_eq!(region_to_bing_market("us-en"), "en-US");
        assert_eq!(region_to_bing_market("uk-en"), "en-GB");
        assert_eq!(region_to_bing_market("bad"), "en-US");

        assert_eq!(
            region_to_google_params("cn-zh"),
            ("zh-CN".to_owned(), "CN".to_owned())
        );
        assert_eq!(
            region_to_google_params("uk-en"),
            ("en-GB".to_owned(), "GB".to_owned())
        );
        assert_eq!(region_to_google_news_ceid("us-en"), "US:en");
        assert_eq!(region_to_google_news_ceid("cn-zh"), "CN:zh-Hans");
        assert_eq!(region_to_google_news_ceid("tw-zh"), "TW:zh-Hant");
        assert_eq!(region_to_google_news_ceid("de-de"), "DE:de");
    }

    #[test]
    fn detect_query_region_handles_scripts() {
        assert_eq!(detect_query_region("python asyncio", "us-en"), "us-en");
        assert_eq!(detect_query_region("AI 新闻 最新进展", "us-en"), "cn-zh");
        assert_eq!(detect_query_region("AI 新闻 最新进展", "tw-zh"), "tw-zh");
        assert_eq!(detect_query_region("人工知能ニュース", "us-en"), "jp-ja");
        assert_eq!(detect_query_region("파이썬 비동기", "us-en"), "kr-ko");
        assert_eq!(detect_query_region("اخبار پایتون", "us-en"), "ir-fa");
        assert_eq!(detect_query_region("پاکستان کی خبریں", "us-en"), "pk-ur");
    }

    #[test]
    fn detect_gate_engine_markers_win_over_shared() {
        let html = "<html><body><meta property=\"og:url\" content=\"https://privacy.yahoo.co.jp/areacheck\">captcha</body></html>";
        let eng: &[(&str, &[&str])] = &[(
            "region",
            &["<meta property=\"og:url\" content=\"https://privacy.yahoo.co.jp/areacheck"],
        )];
        assert_eq!(detect_gate(html, eng), Some("region"));
        assert_eq!(
            detect_gate("<div>anomaly-modal</div>", &[]),
            Some("captcha")
        );
        assert_eq!(
            detect_gate("<html><body>normal page</body></html>", &[]),
            None
        );
    }

    #[test]
    fn unescape_and_strip_html() {
        assert_eq!(
            strip_html_tags(
                r#"<a href="https://x">OpenAI announces</a>&nbsp;&nbsp;<font>Reuters</font>"#
            ),
            "OpenAI announces Reuters"
        );
        assert_eq!(
            strip_html_tags(r#"Learn <em class="keyword">Python</em> &#39;fast&#x21;"#),
            "Learn Python 'fast!"
        );
    }
}
