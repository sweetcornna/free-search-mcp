//! Query-shape and script helpers ported from Python `engines/facts.py` and `engines/base.py`.

use regex::Regex;
use std::collections::HashSet;
use std::sync::LazyLock;

/// Words a "what version / which release / is it supported" question carries that
/// are never the name of the package or repository asked about (EN + ZH).
pub static STOP_WORDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
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
    ]
    .into_iter()
    .collect()
});

static TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"@?[A-Za-z0-9][A-Za-z0-9._@/-]*[A-Za-z0-9]|[A-Za-z0-9]").expect("regex")
});

static VERSION_LIKE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^v?[0-9]+(\.[0-9]+)+$").expect("regex"));

static VERSION_ASK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(latest|newest|current|stable|version|versions|release|released|releases|changelog|upgrade|update|updated|eol|end[- ]of[- ]life|lifecycle|supported|support ends?|still (?:supported|maintained))\b|最新|版本|发布|更新|升级|停止支持|支持到|支持周期|生命周期|还(?:在|有)?(?:维护|支持)",
    )
    .expect("regex")
});

static ECOSYSTEM_PATTERNS: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    [
        ("pypi", r"(?i)\b(pip|pypi|python|py)\b|python 包|python 库"),
        (
            "npm",
            r"(?i)\b(npm|node|nodejs|node\.js|javascript|typescript|js|yarn|pnpm)\b",
        ),
        ("cargo", r"(?i)\b(cargo|crate|crates|rust)\b"),
        ("maven", r"(?i)\b(maven|gradle|java|kotlin|jar)\b"),
        ("rubygems", r"(?i)\b(gem|ruby|rails|bundler)\b"),
        ("go", r"(?i)\b(go|golang)\b|go 模块|go 库"),
        ("homebrew", r"(?i)\b(brew|homebrew|formula)\b"),
        (
            "docker",
            r"(?i)\b(docker|dockerhub|image|container)\b|镜像|容器",
        ),
        (
            "packagist",
            r"(?i)\b(composer|packagist|php|laravel|symfony)\b",
        ),
        ("nuget", r"(?i)\b(nuget|dotnet|\.net|c#|csharp)\b"),
        (
            "app",
            r"(?i)\b(app store|ios|iphone|ipad|android|app)\b|应用商店|苹果版|安卓",
        ),
    ]
    .into_iter()
    .map(|(name, pat)| (name, Regex::new(pat).expect("regex")))
    .collect()
});

/// Tokens of `query` that could name a package or product, in order.
///
/// Lower-cased, quotes and trailing `._-` dropped, stop words, bare numbers and
/// version numbers removed, duplicates removed. `@scope/name` and `owner/repo`
/// survive as one token.
pub fn candidate_names(query: &str, limit: usize) -> Vec<String> {
    let mut seen = Vec::new();
    for m in TOKEN_RE.find_iter(query) {
        let token = m
            .as_str()
            .to_ascii_lowercase()
            .trim_matches(['.', '_', '-'])
            .to_owned();
        if token.is_empty()
            || STOP_WORDS.contains(token.as_str())
            || seen.contains(&token)
            || token.chars().all(|c| c.is_ascii_digit())
            || VERSION_LIKE_RE.is_match(&token)
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

/// Whether `query` asks which version, release, or support state applies.
pub fn version_question(query: &str) -> bool {
    VERSION_ASK_RE.is_match(query)
}

/// Package ecosystems named in `query` (e.g. `{"maven"}`, `{"go"}`, `{"docker"}`).
pub fn ecosystem_hints(query: &str) -> HashSet<&'static str> {
    ECOSYSTEM_PATTERNS
        .iter()
        .filter(|(_, re)| re.is_match(query))
        .map(|(name, _)| *name)
        .collect()
}

// ---------------------------------------------------------------------------
// Script / region detection (ported verbatim from Python engines/base.py)
// ---------------------------------------------------------------------------

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

const PERSIAN_CHARS: &[char] = &['پ', 'چ', 'ژ', 'گ', 'ک'];
const URDU_CHARS: &[char] = &['ٹ', 'ڈ', 'ڑ', 'ں', 'ھ', 'ے'];
const SCRIPT_SWITCH_RATIO: f64 = 0.3;

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

/// Detect the region token (`cc-lang`) implied by non-Latin script in `query`.
///
/// Ties in script character counts break deterministically in favour of the script
/// that appeared first in `query` (matching Python's insertion-ordered `dict`).
pub fn detect_query_region(query: &str, configured: &str) -> String {
    let mut counts: Vec<(&'static str, usize)> = Vec::new();
    let mut letters = 0usize;
    for ch in query.chars() {
        if !ch.is_alphabetic() {
            continue;
        }
        letters += 1;
        let cp = ch as u32;
        for &(script, ranges) in SCRIPT_RANGES {
            if ranges.iter().any(|&(lo, hi)| (lo..=hi).contains(&cp)) {
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

    let count_of = |name: &str| -> usize {
        counts
            .iter()
            .find(|(s, _)| *s == name)
            .map_or(0, |&(_, c)| c)
    };
    let han = count_of("han");
    let kana = count_of("kana");
    let hangul = count_of("hangul");
    let cjk = han + kana + hangul;

    let (script, n) = if kana > 0 || hangul > 0 {
        if (cjk as f64) / (letters as f64) < SCRIPT_SWITCH_RATIO {
            return configured.to_owned();
        }
        let s = if hangul > 0 { "hangul" } else { "kana" };
        (s, cjk)
    } else {
        // Strict `>` keeps the first-seen script on ties (insertion order).
        let mut best = counts[0];
        for &entry in &counts[1..] {
            if entry.1 > best.1 {
                best = entry;
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
        .map_or("", |(_, l)| l.trim())
        .to_ascii_lowercase();
    let det_lang = detected.split_once('-').map_or("", |(_, l)| l);
    if !conf_lang.is_empty() && conf_lang == det_lang {
        return configured.to_owned();
    }
    detected.to_owned()
}

/// Whether `query` is primarily Chinese (Han without Kana/Hangul).
pub fn query_is_chinese(query: &str) -> bool {
    detect_query_region(query, "").ends_with("-zh")
}
