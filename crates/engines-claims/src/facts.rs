//! Shared query-parsing, locale, date, and number-formatting helpers ported
//! from `src/search_mcp/engines/facts.py`, `base.py::detect_query_region`, and
//! `jsonapi.py::clip`.

use jiff::Timestamp;
use jiff::tz::TimeZone;
use regex::Regex;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

/// Default snippet cap matching Python `jsonapi.SNIPPET_CAP`.
pub const SNIPPET_CAP: usize = 400;

/// Maximum query characters inspected by `Source::claims()` and fuzzy matchers
/// so pathological multi-kilobyte queries stay strictly bounded in CPU time.
pub const MAX_CLAIM_QUERY_CHARS: usize = 512;

/// Slice `query` to at most `MAX_CLAIM_QUERY_CHARS` Unicode scalar values.
pub fn clip_query(query: &str) -> &str {
    fsm_core::text::clip_chars(query, MAX_CLAIM_QUERY_CHARS)
}

/// Normalize any text to a single-line snippet of at most `cap` Unicode chars.
pub fn clip(text: &str, cap: usize) -> String {
    let s: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= cap {
        return s;
    }
    let prefix: String = s.chars().take(cap).collect();
    format!("{} …", prefix.trim_end())
}

// ---------------------------------------------------------------------------
// Script / region detection (port of `engines/base.py::detect_query_region`)
// ---------------------------------------------------------------------------

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

fn script_region(script: &str) -> &'static str {
    match script {
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
        _ => "",
    }
}

const PERSIAN_CHARS: &[char] = &['پ', 'چ', 'ژ', 'گ', 'ک'];
const URDU_CHARS: &[char] = &['ٹ', 'ڈ', 'ڑ', 'ں', 'ھ', 'ے'];

/// Region token for `query`, given a configured default.
pub fn detect_query_region(query: &str, configured: &str) -> String {
    let mut counts: Vec<(&'static str, usize)> = Vec::new();
    let mut letters = 0usize;
    for ch in query.chars() {
        if !ch.is_alphabetic() {
            continue;
        }
        letters += 1;
        let o = ch as u32;
        for &(script, ranges) in SCRIPT_RANGES {
            if ranges.iter().any(|&(lo, hi)| lo <= o && o <= hi) {
                if let Some((_, c)) = counts.iter_mut().find(|(s, _)| *s == script) {
                    *c += 1;
                } else {
                    counts.push((script, 1));
                }
                break;
            }
        }
    }
    if letters == 0 || counts.is_empty() {
        return configured.to_owned();
    }
    let get = |name: &str| {
        counts
            .iter()
            .find(|(s, _)| *s == name)
            .map_or(0, |(_, c)| *c)
    };
    let han = get("han");
    let kana = get("kana");
    let hangul = get("hangul");
    let cjk = han + kana + hangul;
    let (script, n) = if kana > 0 || hangul > 0 {
        if (cjk as f64) / (letters as f64) < SCRIPT_SWITCH_RATIO {
            return configured.to_owned();
        }
        (if hangul > 0 { "hangul" } else { "kana" }, cjk)
    } else {
        // Python `max` picks the first element with the maximum value.
        let mut best = counts[0];
        for &item in &counts[1..] {
            if item.1 > best.1 {
                best = item;
            }
        }
        best
    };
    if (n as f64) / (letters as f64) < SCRIPT_SWITCH_RATIO {
        return configured.to_owned();
    }
    let detected = if script == "arabic" {
        if query.chars().any(|c| URDU_CHARS.contains(&c)) {
            "pk-ur"
        } else if query.chars().any(|c| PERSIAN_CHARS.contains(&c)) {
            "ir-fa"
        } else {
            script_region("arabic")
        }
    } else {
        script_region(script)
    };
    if detected.is_empty() {
        return configured.to_owned();
    }
    let conf_lang = configured
        .split_once('-')
        .map_or("", |(_, l)| l)
        .trim()
        .to_ascii_lowercase();
    let det_lang = detected.split_once('-').map_or("", |(_, l)| l);
    if !conf_lang.is_empty() && conf_lang == det_lang {
        return configured.to_owned();
    }
    detected.to_owned()
}

/// Whether the query is primarily Chinese (Han script without Kana/Hangul).
pub fn query_is_chinese(query: &str) -> bool {
    detect_query_region(query, "").ends_with("-zh")
}

// ---------------------------------------------------------------------------
// Candidate package/product names
// ---------------------------------------------------------------------------

pub static STOP_WORDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HashSet::from([
        "a",
        "an",
        "the",
        "of",
        "for",
        "in",
        "on",
        "to",
        "is",
        "are",
        "was",
        "be",
        "what",
        "which",
        "when",
        "who",
        "how",
        "latest",
        "newest",
        "current",
        "stable",
        "new",
        "recent",
        "last",
        "next",
        "version",
        "versions",
        "release",
        "released",
        "releases",
        "update",
        "updated",
        "upgrade",
        "changelog",
        "history",
        "date",
        "year",
        "years",
        "month",
        "months",
        "day",
        "days",
        "week",
        "weeks",
        "first",
        "originally",
        "initially",
        "language",
        "compiler",
        "runtime",
        "edition",
        "package",
        "library",
        "lib",
        "module",
        "framework",
        "tool",
        "cli",
        "sdk",
        "api",
        "app",
        "plugin",
        "python",
        "pip",
        "pypi",
        "node",
        "npm",
        "js",
        "javascript",
        "typescript",
        "rust",
        "cargo",
        "crate",
        "crates",
        "go",
        "golang",
        "java",
        "maven",
        "gem",
        "ruby",
        "php",
        "composer",
        "dotnet",
        "nuget",
        "eol",
        "end",
        "life",
        "support",
        "supported",
        "lifecycle",
        "security",
        "vulnerability",
        "vulnerabilities",
        "cve",
        "exploit",
        "patch",
        "advisory",
        "install",
        "download",
        "still",
        "yet",
        "does",
        "do",
        "did",
        "has",
        "have",
        "with",
        "and",
        "or",
        "not",
        "any",
        "there",
        "it",
        "its",
        "this",
        "that",
        "please",
        "tell",
        "me",
        "find",
        "check",
        "look",
        "up",
        "最新",
        "版本",
        "发布",
        "更新",
        "发行",
        "支持",
        "周期",
        "漏洞",
        "安全",
        "下载",
        "安装",
        "什么",
        "哪个",
        "多少",
        "是",
        "的",
        "了",
        "吗",
        "还",
        "还是",
        "现在",
        "目前",
        "有",
        "没有",
        "在",
        "查",
        "一下",
        "请",
        "帮",
        "我",
        "要",
    ])
});

pub static TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"@?[A-Za-z0-9][A-Za-z0-9._@/-]*[A-Za-z0-9]|[A-Za-z0-9]").expect("token regex")
});

static VERSION_LIKE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^v?\d+(\.\d+)+$").expect("version regex"));

/// Tokens of `query` that could name a package or product, in order.
pub fn candidate_names(query: &str, limit: usize) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for m in TOKEN_RE.find_iter(query) {
        let token = m
            .as_str()
            .to_ascii_lowercase()
            .trim_matches(|c| matches!(c, '.' | '_' | '-'))
            .to_owned();
        if token.is_empty()
            || STOP_WORDS.contains(token.as_str())
            || seen.contains(&token)
            || token.bytes().all(|b| b.is_ascii_digit())
            || VERSION_LIKE.is_match(&token)
        {
            continue;
        }
        seen.push(token);
        if seen.len() >= limit {
            break;
        }
    }
    seen
}

// ---------------------------------------------------------------------------
// Date & number formatting
// ---------------------------------------------------------------------------

/// `YYYY-MM-DD` from an ISO timestamp string, date string, or epoch in seconds.
pub fn iso_day(value: Option<&Value>) -> String {
    let Some(v) = value else {
        return String::new();
    };
    if let Some(n) = v.as_f64() {
        return iso_day_epoch(n);
    }
    if let Some(s) = v.as_str() {
        return iso_day_str(s);
    }
    String::new()
}

pub fn iso_day_str(s: &str) -> String {
    let b = s.as_bytes();
    if b.len() >= 10 && b[4] == b'-' && b[7] == b'-' && s.is_char_boundary(10) {
        s[..10].to_owned()
    } else {
        String::new()
    }
}

pub fn iso_day_epoch(secs: f64) -> String {
    if !secs.is_finite() {
        return String::new();
    }
    let whole = secs.floor();
    if !(-62_135_596_800.0..=253_402_300_799.0).contains(&whole) {
        return String::new();
    }
    let Ok(ts) = Timestamp::from_second(whole as i64) else {
        return String::new();
    };
    let z = ts.to_zoned_datetime(TimeZone::UTC);
    format!("{:04}-{:02}-{:02}", z.year(), z.month(), z.day())
}

/// UTC `YYYY-MM-DD HH:MM UTC` from a Unix epoch in seconds.
pub fn utc_minute_epoch(secs: f64) -> String {
    if !secs.is_finite() {
        return String::new();
    }
    let whole = secs.floor();
    if !(-62_135_596_800.0..=253_402_300_799.0).contains(&whole) {
        return String::new();
    }
    let Ok(ts) = Timestamp::from_second(whole as i64) else {
        return String::new();
    };
    let z = ts.to_zoned_datetime(TimeZone::UTC);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02} UTC",
        z.year(),
        z.month(),
        z.day(),
        z.hour(),
        z.minute()
    )
}

/// Format an integer with comma thousands separators (`24870895` -> `"24,870,895"`).
pub fn format_commas_i64(n: i64) -> String {
    let neg = n < 0;
    let s = n.unsigned_abs().to_string();
    let grouped = insert_commas(&s);
    if neg { format!("-{grouped}") } else { grouped }
}

fn insert_commas(digits: &str) -> String {
    let len = digits.len();
    if len <= 3 {
        return digits.to_owned();
    }
    let mut out = String::with_capacity(len + len / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// `24870895` -> `24,870,895`; anything unparseable unchanged as text.
pub fn compact_number(raw: &str) -> String {
    match raw.trim().parse::<f64>() {
        Ok(f) if f.is_finite() => format_commas_i64(f as i64),
        _ => raw.to_owned(),
    }
}

/// Format a float with comma thousands separator and fixed decimal places
/// (Python `f"{value:,.{decimals}f}"`).
pub fn fmt_comma_fixed(value: f64, decimals: usize) -> String {
    let fixed = format!("{value:.decimals$}");
    let (sign, rest) = match fixed.strip_prefix('-') {
        Some(r) => ("-", r),
        None => ("", fixed.as_str()),
    };
    match rest.split_once('.') {
        Some((int_part, frac)) => format!("{sign}{}.{frac}", insert_commas(int_part)),
        None => format!("{sign}{}", insert_commas(rest)),
    }
}

/// Python `f"{value:.{sig}g}"` general float format (default `sig = 6` for `:g`).
pub fn fmt_g(value: f64, sig: usize) -> String {
    let p = sig.max(1);
    if value == 0.0 {
        return if value.is_sign_negative() {
            "-0".to_owned()
        } else {
            "0".to_owned()
        };
    }
    if !value.is_finite() {
        return value.to_string();
    }
    let neg = value.is_sign_negative();
    let abs = value.abs();
    // Format in scientific notation with `p - 1` digits after the decimal point
    // so rounding at the `p`-th significant digit is handled identically to C/Python.
    let sci = format!("{abs:.prec$e}", prec = p - 1);
    let (mantissa, exp_str) = sci.split_once('e').expect("scientific format");
    let exp: i32 = exp_str.parse().unwrap_or(0);
    let sign = if neg { "-" } else { "" };

    if exp < -4 || exp >= p as i32 {
        let trimmed = trim_decimal_zeros(mantissa);
        let exp_sign = if exp >= 0 { '+' } else { '-' };
        format!("{sign}{trimmed}e{exp_sign}{:02}", exp.unsigned_abs())
    } else {
        // Fixed representation with `p - 1 - exp` decimal places.
        let dec = (p as i32 - 1 - exp).max(0) as usize;
        let fixed = format!("{abs:.dec$}");
        let trimmed = trim_decimal_zeros(&fixed);
        format!("{sign}{trimmed}")
    }
}

fn trim_decimal_zeros(s: &str) -> &str {
    if !s.contains('.') {
        return s;
    }
    let s = s.trim_end_matches('0');
    s.strip_suffix('.').unwrap_or(s)
}

// ---------------------------------------------------------------------------
// Question shapes, for `Source::claims`. Offline, regex only.
// ---------------------------------------------------------------------------

static VERSION_ASK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(latest|newest|current|stable|version|versions|release|released|releases|changelog|upgrade|update|updated|eol|end[- ]of[- ]life|lifecycle|supported|support ends?|still (?:supported|maintained))\b|最新|版本|发布|更新|升级|停止支持|支持到|支持周期|生命周期|还(?:在|有)?(?:维护|支持)",
    )
    .expect("version_ask regex")
});

static ECOSYSTEMS: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    vec![
        (
            "pypi",
            Regex::new(r"(?i)\b(pip|pypi|python|py)\b|python 包|python 库").expect("re"),
        ),
        (
            "npm",
            Regex::new(r"(?i)\b(npm|node|nodejs|node\.js|javascript|typescript|js|yarn|pnpm)\b")
                .expect("re"),
        ),
        (
            "cargo",
            Regex::new(r"(?i)\b(cargo|crate|crates|rust)\b").expect("re"),
        ),
        (
            "maven",
            Regex::new(r"(?i)\b(maven|gradle|java|kotlin|jar)\b").expect("re"),
        ),
        (
            "rubygems",
            Regex::new(r"(?i)\b(gem|ruby|rails|bundler)\b").expect("re"),
        ),
        (
            "go",
            Regex::new(r"(?i)\b(go|golang)\b|go 模块|go 库").expect("re"),
        ),
        (
            "homebrew",
            Regex::new(r"(?i)\b(brew|homebrew|formula)\b").expect("re"),
        ),
        (
            "docker",
            Regex::new(r"(?i)\b(docker|dockerhub|image|container)\b|镜像|容器").expect("re"),
        ),
        (
            "packagist",
            Regex::new(r"(?i)\b(composer|packagist|php|laravel|symfony)\b").expect("re"),
        ),
        (
            "nuget",
            Regex::new(r"(?i)\b(nuget|dotnet|\.net|c#|csharp)\b").expect("re"),
        ),
        (
            "app",
            Regex::new(r"(?i)\b(app store|ios|iphone|ipad|android|app)\b|应用商店|苹果版|安卓")
                .expect("re"),
        ),
    ]
});

/// Whether the question asks which version, release or support state.
pub fn version_question(query: &str) -> bool {
    VERSION_ASK.is_match(query)
}

/// Package ecosystems the question names: `{"pypi"}`, `{"npm", "docker"}`, ...
pub fn ecosystem_hints(query: &str) -> HashSet<&'static str> {
    ECOSYSTEMS
        .iter()
        .filter(|(_, rx)| rx.is_match(query))
        .map(|(name, _)| *name)
        .collect()
}

// ---------------------------------------------------------------------------
// Countries table & extraction
// ---------------------------------------------------------------------------

/// `(iso3, iso2, english_name)`
pub type CountryRow = (&'static str, &'static str, &'static str);

struct CountryIndex {
    /// `(lowercase_key, is_ascii_word, row)` sorted by char count descending.
    keys: Vec<(String, bool, CountryRow)>,
    zh_by_iso3: HashMap<&'static str, &'static str>,
    zh_by_iso2: HashMap<&'static str, &'static str>,
}

const COUNTRY_ROWS: &[(&str, &str, &str, &[&str])] = &[
    (
        "CHN",
        "CN",
        "China",
        &["中国", "中华人民共和国", "prc", "mainland china"],
    ),
    (
        "USA",
        "US",
        "United States",
        &["美国", "usa", "u.s.", "america", "united states of america"],
    ),
    ("JPN", "JP", "Japan", &["日本"]),
    ("DEU", "DE", "Germany", &["德国"]),
    (
        "GBR",
        "GB",
        "United Kingdom",
        &["英国", "uk", "britain", "great britain"],
    ),
    ("FRA", "FR", "France", &["法国"]),
    ("IND", "IN", "India", &["印度"]),
    ("RUS", "RU", "Russia", &["俄罗斯", "russian federation"]),
    ("BRA", "BR", "Brazil", &["巴西"]),
    (
        "KOR",
        "KR",
        "South Korea",
        &["韩国", "korea", "republic of korea"],
    ),
    ("CAN", "CA", "Canada", &["加拿大"]),
    ("AUS", "AU", "Australia", &["澳大利亚", "澳洲"]),
    ("ITA", "IT", "Italy", &["意大利"]),
    ("ESP", "ES", "Spain", &["西班牙"]),
    ("MEX", "MX", "Mexico", &["墨西哥"]),
    ("IDN", "ID", "Indonesia", &["印尼", "印度尼西亚"]),
    ("TUR", "TR", "Turkey", &["土耳其", "türkiye", "turkiye"]),
    (
        "NLD",
        "NL",
        "Netherlands",
        &["荷兰", "the netherlands", "holland"],
    ),
    ("CHE", "CH", "Switzerland", &["瑞士"]),
    ("SAU", "SA", "Saudi Arabia", &["沙特", "沙特阿拉伯"]),
    ("VNM", "VN", "Vietnam", &["越南", "viet nam"]),
    ("THA", "TH", "Thailand", &["泰国"]),
    ("SGP", "SG", "Singapore", &["新加坡"]),
    ("MYS", "MY", "Malaysia", &["马来西亚"]),
    ("PHL", "PH", "Philippines", &["菲律宾", "the philippines"]),
    ("PAK", "PK", "Pakistan", &["巴基斯坦"]),
    ("BGD", "BD", "Bangladesh", &["孟加拉国", "孟加拉"]),
    ("NGA", "NG", "Nigeria", &["尼日利亚"]),
    ("EGY", "EG", "Egypt", &["埃及"]),
    ("ZAF", "ZA", "South Africa", &["南非"]),
    ("ARG", "AR", "Argentina", &["阿根廷"]),
    ("POL", "PL", "Poland", &["波兰"]),
    ("SWE", "SE", "Sweden", &["瑞典"]),
    ("NOR", "NO", "Norway", &["挪威"]),
    ("DNK", "DK", "Denmark", &["丹麦"]),
    ("FIN", "FI", "Finland", &["芬兰"]),
    ("BEL", "BE", "Belgium", &["比利时"]),
    ("AUT", "AT", "Austria", &["奥地利"]),
    ("PRT", "PT", "Portugal", &["葡萄牙"]),
    ("GRC", "GR", "Greece", &["希腊"]),
    ("IRL", "IE", "Ireland", &["爱尔兰"]),
    ("ISR", "IL", "Israel", &["以色列"]),
    ("IRN", "IR", "Iran", &["伊朗"]),
    ("IRQ", "IQ", "Iraq", &["伊拉克"]),
    ("ARE", "AE", "United Arab Emirates", &["阿联酋", "uae"]),
    ("QAT", "QA", "Qatar", &["卡塔尔"]),
    ("NZL", "NZ", "New Zealand", &["新西兰"]),
    ("CHL", "CL", "Chile", &["智利"]),
    ("COL", "CO", "Colombia", &["哥伦比亚"]),
    ("PER", "PE", "Peru", &["秘鲁"]),
    ("UKR", "UA", "Ukraine", &["乌克兰"]),
    ("CZE", "CZ", "Czechia", &["捷克", "czech republic"]),
    ("HUN", "HU", "Hungary", &["匈牙利"]),
    ("ROU", "RO", "Romania", &["罗马尼亚"]),
    ("KAZ", "KZ", "Kazakhstan", &["哈萨克斯坦"]),
    ("MNG", "MN", "Mongolia", &["蒙古", "蒙古国"]),
    ("PRK", "KP", "North Korea", &["朝鲜"]),
    ("KHM", "KH", "Cambodia", &["柬埔寨"]),
    ("LAO", "LA", "Laos", &["老挝"]),
    ("MMR", "MM", "Myanmar", &["缅甸"]),
    ("NPL", "NP", "Nepal", &["尼泊尔"]),
    ("LKA", "LK", "Sri Lanka", &["斯里兰卡"]),
    ("ETH", "ET", "Ethiopia", &["埃塞俄比亚"]),
    ("KEN", "KE", "Kenya", &["肯尼亚"]),
    ("MAR", "MA", "Morocco", &["摩洛哥"]),
    ("HKG", "HK", "Hong Kong", &["香港", "hong kong sar"]),
    ("MAC", "MO", "Macao", &["澳门", "macau"]),
    ("WLD", "1W", "World", &["世界", "全球", "global"]),
    ("EUU", "EU", "European Union", &["欧盟", "eu"]),
];

fn is_ascii_country_word(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b == b'.' || b == b' ')
}

fn contains_han(s: &str) -> bool {
    s.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
}

static COUNTRY_INDEX: LazyLock<CountryIndex> = LazyLock::new(|| {
    let mut keys: Vec<(String, bool, CountryRow)> = Vec::new();
    let mut seen_keys: HashMap<String, usize> = HashMap::new();
    let mut zh_by_iso3: HashMap<&'static str, &'static str> = HashMap::new();
    let mut zh_by_iso2: HashMap<&'static str, &'static str> = HashMap::new();

    for &(iso3, iso2, name, aliases) in COUNTRY_ROWS {
        let row = (iso3, iso2, name);
        let mut insert_key = |raw: &'static str| {
            let k = raw.to_lowercase();
            let ascii_word = is_ascii_country_word(&k);
            if let Some(&idx) = seen_keys.get(&k) {
                keys[idx] = (k, ascii_word, row);
            } else {
                let idx = keys.len();
                seen_keys.insert(k.clone(), idx);
                keys.push((k, ascii_word, row));
            }
            if contains_han(raw) {
                zh_by_iso3.entry(iso3).or_insert(raw);
                zh_by_iso2.entry(iso2).or_insert(raw);
            }
        };
        insert_key(name);
        for &alias in aliases {
            insert_key(alias);
        }
    }
    // Stable sort by character length descending (matches Python `sorted(COUNTRIES, key=len, reverse=True)`).
    keys.sort_by_key(|a| std::cmp::Reverse(a.0.chars().count()));
    CountryIndex {
        keys,
        zh_by_iso3,
        zh_by_iso2,
    }
});

/// Chinese display name for an ISO-3 country code, if present.
pub fn zh_country_by_iso3(iso3: &str) -> Option<&'static str> {
    COUNTRY_INDEX.zh_by_iso3.get(iso3).copied()
}

/// Chinese display name for an ISO-2 country code, if present.
pub fn zh_country_by_iso2(iso2: &str) -> Option<&'static str> {
    COUNTRY_INDEX.zh_by_iso2.get(iso2).copied()
}

/// Find the byte offset of `needle` inside `haystack` where neither the
/// preceding nor following byte is an ASCII lowercase letter (`(?<![a-z])...(?![a-z])`).
pub(crate) fn find_ascii_letter_bounded(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    let bytes = haystack.as_bytes();
    let mut start = 0usize;
    while let Some(rel) = haystack[start..].find(needle) {
        let at = start + rel;
        let end = at + needle.len();
        let before_ok = at == 0 || !bytes[at - 1].is_ascii_lowercase();
        let after_ok = end == bytes.len() || !bytes[end].is_ascii_lowercase();
        if before_ok && after_ok {
            return Some(at);
        }
        start = at + haystack[at..].chars().next().map_or(1, char::len_utf8);
    }
    None
}

/// `(iso3, iso2, name)` for each country the question names, in order.
pub fn countries_in(query: &str, limit: usize) -> Vec<CountryRow> {
    let mut lowered = query.to_lowercase();
    let mut hits: Vec<(usize, CountryRow)> = Vec::new();
    let mut taken: HashSet<&'static str> = HashSet::new();

    for (key, is_ascii_word, row) in &COUNTRY_INDEX.keys {
        let found = if *is_ascii_word {
            find_ascii_letter_bounded(&lowered, key)
        } else {
            lowered.find(key.as_str())
        };
        let Some(at) = found else {
            continue;
        };
        if !taken.insert(row.0) {
            continue;
        }
        hits.push((at, *row));
        // Replace matched byte slice with identical byte length of ASCII spaces
        // so all subsequent byte positions remain comparable.
        let end = at + key.len();
        let spaces = " ".repeat(key.len());
        lowered.replace_range(at..end, &spaces);
    }
    hits.sort_by_key(|(at, _)| *at);
    hits.into_iter().take(limit).map(|(_, row)| row).collect()
}

// ---------------------------------------------------------------------------
// Year extraction
// ---------------------------------------------------------------------------

static YEAR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(20\d\d)\b|(20\d\d)年").expect("year regex"));
static NEXT_YEAR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)明年|next year").expect("next year regex"));
static LAST_YEAR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)去年|last year").expect("last year regex"));

/// The four-digit year the question names, else `default`; 明年 / "next year"
/// adds one, 去年 / "last year" subtracts one.
pub fn year_in(query: &str, default: i32) -> i32 {
    if let Some(c) = YEAR_RE.captures(query)
        && let Some(m) = c.get(1).or_else(|| c.get(2))
        && let Ok(y) = m.as_str().parse::<i32>()
    {
        return y;
    }
    if NEXT_YEAR_RE.is_match(query) {
        return default + 1;
    }
    if LAST_YEAR_RE.is_match(query) {
        return default - 1;
    }
    default
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_g_matches_python() {
        assert_eq!(fmt_g(0.0, 6), "0");
        assert_eq!(fmt_g(7.0, 6), "7");
        assert_eq!(fmt_g(100.0, 6), "100");
        assert_eq!(fmt_g(10_000.0, 6), "10000");
        assert_eq!(fmt_g(1_000_000.0, 6), "1e+06");
        assert_eq!(fmt_g(6.6954, 6), "6.6954");
        assert_eq!(fmt_g(0.2105, 6), "0.2105");
        assert_eq!(fmt_g(0.04257, 6), "0.04257");
        assert_eq!(fmt_g(0.00001, 6), "1e-05");
        assert_eq!(fmt_g(19.498039388, 3), "19.5");
        assert_eq!(fmt_g(18.729668435, 3), "18.7");
        assert_eq!(fmt_g(0.92, 6), "0.92");
    }

    #[test]
    fn fmt_comma_fixed_matches_python() {
        assert_eq!(fmt_comma_fixed(669.54, 4), "669.5400");
        assert_eq!(fmt_comma_fixed(0.0, 4), "0.0000");
        assert_eq!(fmt_comma_fixed(12345.6789, 4), "12,345.6789");
        assert_eq!(fmt_comma_fixed(85485.0, 0), "85,485");
        assert_eq!(fmt_comma_fixed(1.6, 2), "1.60");
        assert_eq!(fmt_comma_fixed(-1234.5, 2), "-1,234.50");
    }

    #[test]
    fn detect_chinese_vs_japanese_vs_english() {
        assert!(query_is_chinese("1万日元等于多少人民币"));
        assert!(query_is_chinese("2026年中国放假安排"));
        assert!(query_is_chinese("微信 iOS 版最新版本"));
        assert!(!query_is_chinese("100 usd to cny"));
        assert!(!query_is_chinese("Notion iOS app version"));
        assert!(!query_is_chinese("人工知能ニュース"));
    }
}
