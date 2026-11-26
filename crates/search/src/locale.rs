//! Query script → language (port of `engines/base.detect_query_region` and
//! `aggregator._query_language`).

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

const SCRIPT_REGION: &[(&str, &str)] = &[
    ("han", "cn-zh"),
    ("kana", "jp-ja"),
    ("hangul", "kr-ko"),
    ("cyrillic", "ru-ru"),
    ("greek", "gr-el"),
    ("hebrew", "il-he"),
    ("arabic", "eg-ar"),
    ("thai", "th-th"),
    ("lao", "la-lo"),
    ("khmer", "kh-km"),
    ("myanmar", "mm-my"),
    ("devanagari", "in-hi"),
    ("bengali", "in-bn"),
    ("gurmukhi", "in-pa"),
    ("gujarati", "in-gu"),
    ("tamil", "in-ta"),
    ("telugu", "in-te"),
    ("kannada", "in-kn"),
    ("malayalam", "in-ml"),
    ("sinhala", "lk-si"),
    ("georgian", "ge-ka"),
    ("armenian", "am-hy"),
    ("ethiopic", "et-am"),
];

const PERSIAN_CHARS: &str = "پچژگک";
const URDU_CHARS: &str = "ٹڈڑںھے";
/// Share of letters one non-Latin script needs before the edition switches.
const SCRIPT_SWITCH_RATIO: f64 = 0.3;

/// Region token for `query` given the configured one (`"us-en"`, ...):
/// unchanged for Latin-script queries and when the configured region already
/// speaks the detected language.
pub fn detect_query_region(query: &str, configured: &str) -> String {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    let mut letters = 0usize;
    for ch in query.chars().filter(|c| c.is_alphabetic()) {
        letters += 1;
        let o = ch as u32;
        if let Some((script, _)) = SCRIPT_RANGES
            .iter()
            .find(|(_, ranges)| ranges.iter().any(|(lo, hi)| (*lo..=*hi).contains(&o)))
        {
            match counts.iter_mut().find(|(s, _)| s == script) {
                Some((_, n)) => *n += 1,
                None => counts.push((script, 1)),
            }
        }
    }
    if letters == 0 || counts.is_empty() {
        return configured.to_owned();
    }
    let count = |s: &str| counts.iter().find(|(k, _)| *k == s).map_or(0, |(_, n)| *n);
    let cjk = count("han") + count("kana") + count("hangul");
    let (script, n) = if count("kana") > 0 || count("hangul") > 0 {
        (
            if count("hangul") > 0 {
                "hangul"
            } else {
                "kana"
            },
            cjk,
        )
    } else {
        // max() with Python's tie rule: the first maximal entry wins.
        counts.iter().fold(
            ("", 0),
            |best, (s, n)| if *n > best.1 { (s, *n) } else { best },
        )
    };
    if (n as f64) / (letters as f64) < SCRIPT_SWITCH_RATIO {
        return configured.to_owned();
    }
    let detected = if script == "arabic" {
        if query.chars().any(|c| URDU_CHARS.contains(c)) {
            "pk-ur"
        } else if query.chars().any(|c| PERSIAN_CHARS.contains(c)) {
            "ir-fa"
        } else {
            "eg-ar"
        }
    } else {
        SCRIPT_REGION
            .iter()
            .find(|(s, _)| *s == script)
            .map_or("", |(_, r)| *r)
    };
    if detected.is_empty() {
        return configured.to_owned();
    }
    let conf_lang = configured
        .split_once('-')
        .map_or("", |(_, l)| l)
        .trim()
        .to_ascii_lowercase();
    if !conf_lang.is_empty()
        && detected
            .split_once('-')
            .is_some_and(|(_, l)| l == conf_lang)
    {
        return configured.to_owned();
    }
    detected.to_owned()
}

/// The language the query's SCRIPT implies (`zh`, `ja`, `ko`, `ru`, ...),
/// or `None` for Latin script. The configured region is deliberately ignored.
pub fn query_language(query: &str) -> Option<String> {
    let region = detect_query_region(query, "");
    region
        .split_once('-')
        .map(|(_, l)| l.to_owned())
        .filter(|l| !l.is_empty())
}

#[cfg(test)]
mod tests {
    //! Ported from `tests/test_category_native_and_locale.py` (vectors verbatim).
    use super::*;

    #[test]
    fn detect_query_region_vectors() {
        // test_detect_query_region (parametrized)
        let cases = [
            ("artificial intelligence news", "us-en", "us-en"),
            ("AI", "us-en", "us-en"),
            ("AI 新闻 最新进展", "us-en", "cn-zh"),
            ("人工知能 ニュース", "us-en", "jp-ja"),
            ("인공지능 뉴스", "us-en", "kr-ko"),
            ("искусственный интеллект новости", "us-en", "ru-ru"),
            ("أخبار الذكاء الاصطناعي", "us-en", "eg-ar"),
            ("ειδήσεις τεχνητής νοημοσύνης", "us-en", "gr-el"),
            ("חדשות בינה מלאכותית", "us-en", "il-he"),
            ("ข่าวปัญญาประดิษฐ์", "us-en", "th-th"),
            ("कृत्रिम बुद्धिमत्ता समाचार", "us-en", "in-hi"),
            ("কৃত্রিম বুদ্ধিমত্তা সংবাদ", "us-en", "in-bn"),
            ("செயற்கை நுண்ணறிவு செய்திகள்", "us-en", "in-ta"),
            ("ხელოვნური ინტელექტის ახალი ამბები", "us-en", "ge-ka"),
            ("اخبار هوش مصنوعی پژوهش", "us-en", "ir-fa"),
            ("مصنوعی ذہانت کی خبریں ٹیکنالوجی", "us-en", "pk-ur"),
            ("AI 新闻", "tw-zh", "tw-zh"),
            ("", "us-en", "us-en"),
            ("!!!", "us-en", "us-en"),
        ];
        for (q, conf, want) in cases {
            assert_eq!(detect_query_region(q, conf), want, "{q:?}");
        }
    }

    #[test]
    fn kana_beats_han_for_japanese() {
        assert_eq!(detect_query_region("人工知能 ニュース", "us-en"), "jp-ja");
        assert_eq!(
            detect_query_region("最新人工知能技術研究開発の動向", "us-en"),
            "jp-ja"
        );
        assert_eq!(
            detect_query_region("最新人工智能技术研究", "us-en"),
            "cn-zh"
        );
        assert_eq!(detect_query_region("인공지능 漢字 뉴스", "us-en"), "kr-ko");
    }

    #[test]
    fn latin_script_queries_are_never_rerouted() {
        for q in [
            "künstliche Intelligenz Nachrichten",
            "actualités intelligence artificielle",
            "noticias inteligencia artificial",
            "tin tức trí tuệ nhân tạo",
            "yapay zeka haberleri",
        ] {
            assert_eq!(detect_query_region(q, "us-en"), "us-en", "{q:?}");
        }
    }

    // --- 1.0: the kernel's query language (`aggregator._query_language`) ---

    #[test]
    fn query_language_is_the_script_s_language() {
        assert_eq!(query_language("rust ownership"), None);
        assert_eq!(query_language("인공지능 뉴스").as_deref(), Some("ko"));
        assert_eq!(query_language("人工知能のニュース").as_deref(), Some("ja"));
        assert_eq!(query_language("图神经网络 异常检测").as_deref(), Some("zh"));
        assert_eq!(
            detect_query_region("the word 猫 in english prose", "us-en"),
            "us-en"
        );
    }
}
