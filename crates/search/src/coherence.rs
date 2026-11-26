//! Does a bucket of results answer the query, or only its first word?
//!
//! Port of `coherence.py` (Bing's HTTP-200 decoy pages: ten well-formed
//! results about the query's FIRST word only) plus the CJK fix `SPEC.md` §5
//! asks for after Spike N's Naver false positive:
//!
//! * terms: CJK bigrams, as in Python;
//! * additive evidence: a row that carries at least three distinct, non-trivial
//!   CJK characters of the rest-of-query also counts (segmentation-free);
//! * rows written in another CJK script than a pure-CJK query (Korean or
//!   Japanese results for a Chinese query, ...) are *unjudgeable* and leave the
//!   denominator, so a Korean index answering a Chinese query in Korean is never
//!   called a decoy; if fewer than [`MIN_RESULTS`] rows remain judgeable the
//!   bucket cannot be judged (`None`, i.e. healthy);
//! * a suspect is dropped only when another bucket is a witness (`≥ WITNESS_MIN`),
//!   which the kernel enforces.
//!
//! Without foreign-script rows and CJK char evidence, every number is the
//! Python one (the `coherence.py` test vectors are ported unchanged).

use fsm_core::Hit;

/// Share of a bucket that must mention a query term beyond the first.
/// Measured 2026-09: healthy buckets 0.5–1.0, decoys 0.0–0.2.
pub const COHERENCE_MIN: f64 = 0.3;
/// A bucket this coherent proves the query's words do get echoed.
pub const WITNESS_MIN: f64 = 0.5;
/// Below this many (judgeable) rows the ratio is noise.
pub const MIN_RESULTS: usize = 4;
const MIN_REST_TERMS: usize = 2;
/// Distinct rest-of-query CJK characters a row must carry to count without a
/// bigram match.
const CJK_CHAR_EVIDENCE: usize = 3;

/// `coherence._is_cjk`: CJK unified ideographs, kana, Hangul syllables.
pub fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x4E00..=0x9FFF | 0x3040..=0x30FF | 0xAC00..=0xD7A3)
}

fn is_han(c: char) -> bool {
    matches!(c as u32, 0x4E00..=0x9FFF)
}

fn is_kana(c: char) -> bool {
    matches!(c as u32, 0x3040..=0x30FF)
}

fn is_hangul(c: char) -> bool {
    matches!(c as u32, 0xAC00..=0xD7A3)
}

/// Python `\w` minus nothing: a "word" character for boundary tests.
fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn push_unique(v: &mut Vec<String>, s: String) {
    if !v.contains(&s) {
        v.push(s);
    }
}

/// `_lead_query_terms`: ASCII-ish tokens longer than 3 characters
/// (lower-cased) and CJK character bigrams; a lone CJK character is skipped.
/// Set semantics, first-seen order.
pub fn lead_query_terms(query: &str) -> Vec<String> {
    let mut terms = Vec::new();
    for tok in query.split_whitespace() {
        let cjk: Vec<char> = tok.chars().filter(|c| is_cjk(*c)).collect();
        if cjk.len() >= 2 {
            for w in cjk.windows(2) {
                push_unique(&mut terms, w.iter().collect());
            }
        } else if cjk.is_empty() && tok.chars().count() > 3 {
            push_unique(&mut terms, tok.to_lowercase());
        }
    }
    terms
}

const OPERATORS: &[&str] = &[
    "site",
    "filetype",
    "ext",
    "intitle",
    "allintitle",
    "inurl",
    "allinurl",
    "intext",
    "inbody",
    "inanchor",
    "lang",
    "language",
    "loc",
    "location",
    "before",
    "after",
    "feed",
    "contains",
    "ip",
    "prefer",
];

fn is_operator(tok: &str) -> bool {
    let t = tok.strip_prefix(['+', '-']).unwrap_or(tok);
    t.split_once(':').is_some_and(|(name, _)| {
        !name.is_empty() && OPERATORS.contains(&name.to_lowercase().as_str())
    })
}

const BOOLEAN_TOKENS: [&str; 5] = ["OR", "AND", "NOT", "|", "&"];

const STOPWORDS: &[&str] = &[
    "a", "about", "an", "and", "are", "as", "at", "be", "been", "best", "by", "can", "could",
    "did", "do", "does", "for", "from", "get", "had", "has", "have", "how", "i", "if", "in",
    "into", "is", "it", "its", "me", "my", "new", "no", "not", "of", "on", "or", "our", "should",
    "so", "than", "that", "the", "their", "then", "there", "these", "they", "this", "to", "top",
    "up", "us", "use", "using", "vs", "was", "we", "were", "what", "when", "where", "which", "who",
    "why", "will", "with", "would", "you", "your",
];

/// Characters too common in Chinese/Japanese prose to be evidence alone.
const CJK_STOP_CHARS: &str =
    "的是在了和与及或等中之也有为對对年月日个個们們这這那一不上下大小我你他她它其将將被把从從到以";

const SUFFIXES: [&str; 4] = ["ing", "ed", "es", "s"];

/// `_stem`: strip one inflection from a pure-ASCII alphabetic word, never
/// below four letters and never from a word ending in "ss".
fn stem(word: &str) -> String {
    if !word.is_ascii() || !word.chars().all(|c| c.is_ascii_alphabetic()) || word.ends_with("ss") {
        return word.to_owned();
    }
    for s in SUFFIXES {
        if word.ends_with(s) && word.len() - s.len() >= 4 {
            return word[..word.len() - s.len()].to_owned();
        }
    }
    word.to_owned()
}

/// Maximal runs of CJK characters.
fn cjk_runs(s: &str) -> Vec<String> {
    let mut runs = Vec::new();
    let mut cur = String::new();
    for c in s.chars() {
        if is_cjk(c) {
            cur.push(c);
        } else if !cur.is_empty() {
            runs.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        runs.push(cur);
    }
    runs
}

fn bigrams(run: &str, start: usize) -> Vec<String> {
    let chars: Vec<char> = run.chars().collect();
    (start..chars.len().saturating_sub(1))
        .map(|i| chars[i..i + 2].iter().collect())
        .collect()
}

/// `_WORD_RE = [^\W_]+(?:\.\d+)+|[^\W_]+`: words, keeping a version tail.
fn words(latin: &str) -> Vec<String> {
    let chars: Vec<char> = latin.chars().collect();
    let alnum = |c: char| c.is_alphanumeric();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if !alnum(chars[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && alnum(chars[i]) {
            i += 1;
        }
        while i + 1 < chars.len() && chars[i] == '.' && chars[i + 1].is_numeric() {
            i += 1;
            while i < chars.len() && chars[i].is_numeric() {
                i += 1;
            }
        }
        out.push(chars[start..i].iter().collect());
    }
    out
}

/// `_token_terms`: the matchable terms of one whitespace-delimited token.
fn token_terms(token: &str) -> Vec<String> {
    let mut terms = Vec::new();
    for run in cjk_runs(token) {
        terms.extend(bigrams(&run, 0));
    }
    let latin: String = token
        .chars()
        .map(|c| if is_cjk(c) { ' ' } else { c })
        .collect::<String>()
        .to_lowercase();
    for word in words(&latin) {
        if word.chars().count() < 2 || STOPWORDS.contains(&word.as_str()) {
            continue;
        }
        terms.push(stem(&word));
    }
    terms
}

/// `_content_tokens`: query tokens a relevant result could echo (no
/// operators, exclusions or boolean words).
fn content_tokens(query: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut in_excluded_phrase = false;
    for tok in query.split_whitespace() {
        if in_excluded_phrase {
            in_excluded_phrase = !tok.ends_with('"');
            continue;
        }
        if let Some(rest) = tok.strip_prefix("-\"") {
            in_excluded_phrase = !(!rest.is_empty() && tok.ends_with('"'));
            continue;
        }
        if BOOLEAN_TOKENS.contains(&tok) || tok.starts_with('-') || is_operator(tok) {
            continue;
        }
        tokens.push(tok);
    }
    tokens
}

/// `coherence_terms`: split a query into `(anchor, rest)`. `anchor` is what a
/// decoy still matches (the first content token's terms); `rest` is the
/// remainder minus anything overlapping the anchor. An unsegmented CJK query
/// of ≥ 4 characters is split inside the run.
pub fn coherence_terms(query: &str) -> (Vec<String>, Vec<String>) {
    let tokens = content_tokens(query);
    let mut anchor: Vec<String> = Vec::new();
    let mut rest: Vec<String> = Vec::new();
    if tokens.len() == 1 {
        let runs = cjk_runs(tokens[0]);
        let stripped = tokens[0].trim_matches(|c| "\"'“”《》「」".contains(c));
        if runs.len() == 1 && runs[0] == stripped && runs[0].chars().count() >= 4 {
            anchor = vec![runs[0].chars().take(2).collect()];
            rest = bigrams(&runs[0], 2);
        }
    }
    if anchor.is_empty() {
        for (i, tok) in tokens.iter().enumerate() {
            let terms = token_terms(tok);
            if terms.is_empty() {
                continue;
            }
            anchor = terms;
            for later in &tokens[i + 1..] {
                rest.extend(token_terms(later));
            }
            break;
        }
    }
    let mut unique: Vec<String> = Vec::new();
    for term in rest {
        if unique.contains(&term)
            || anchor
                .iter()
                .any(|a| term.contains(a.as_str()) || a.contains(term.as_str()))
        {
            continue;
        }
        unique.push(term);
    }
    (anchor, unique)
}

fn all_cjk(term: &str) -> bool {
    !term.is_empty() && term.chars().all(is_cjk)
}

/// Whole-word occurrence (Python `(?<!\w)term(?!\w)`).
fn has_whole_word(hay: &str, term: &str) -> bool {
    hay.match_indices(term).any(|(i, m)| {
        hay[..i].chars().next_back().is_none_or(|c| !is_word(c))
            && hay[i + m.len()..]
                .chars()
                .next()
                .is_none_or(|c| !is_word(c))
    })
}

/// `_matcher`: does the (lower-cased) haystack mention any rest term? Terms
/// of up to three characters only count as whole words, unless they are CJK.
fn mentions(hay: &str, rest: &[String]) -> bool {
    rest.iter().any(|t| {
        if t.chars().count() <= 3 && !all_cjk(t) {
            has_whole_word(hay, t)
        } else {
            hay.contains(t.as_str())
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CjkClass {
    Han,
    Kana,
    Hangul,
}

/// The script a pure-CJK rest-of-query is written in; `None` when any rest
/// term is not CJK (then every row is judgeable, as in Python).
fn query_class(anchor: &[String], rest: &[String]) -> Option<CjkClass> {
    if rest.is_empty() || !rest.iter().all(|t| all_cjk(t)) {
        return None;
    }
    let chars = || anchor.iter().chain(rest).flat_map(|t| t.chars());
    if chars().any(is_hangul) {
        Some(CjkClass::Hangul)
    } else if chars().any(is_kana) {
        Some(CjkClass::Kana)
    } else {
        Some(CjkClass::Han)
    }
}

/// A row written in another CJK script than the query cannot echo it.
fn foreign(class: CjkClass, hay: &str) -> bool {
    let (mut han, mut kana, mut hangul) = (0usize, 0usize, 0usize);
    for c in hay.chars() {
        if is_han(c) {
            han += 1;
        } else if is_kana(c) {
            kana += 1;
        } else if is_hangul(c) {
            hangul += 1;
        }
    }
    match class {
        CjkClass::Han => (hangul > 0 && hangul >= han) || kana >= 2,
        CjkClass::Kana => hangul > 0 && hangul >= han + kana,
        CjkClass::Hangul => hangul == 0 && han + kana > 0,
    }
}

/// Distinct, non-trivial CJK characters of the rest terms that the anchor
/// does not already contain.
fn evidence_chars(anchor: &[String], rest: &[String]) -> Vec<char> {
    let mut out: Vec<char> = Vec::new();
    for c in rest.iter().filter(|t| all_cjk(t)).flat_map(|t| t.chars()) {
        if !out.contains(&c) && !CJK_STOP_CHARS.contains(c) && !anchor.iter().any(|a| a.contains(c))
        {
            out.push(c);
        }
    }
    out
}

fn haystack(h: &Hit) -> String {
    format!("{} {} {}", h.title, h.snippet, h.url).to_lowercase()
}

/// `bucket_coherence`: share of (judgeable) rows mentioning a query term
/// beyond the first token, or `None` = cannot tell (short query, too few
/// rows), which callers must treat as healthy. Title, snippet and URL count.
pub fn bucket_coherence(query: &str, rows: &[Hit]) -> Option<f64> {
    if rows.len() < MIN_RESULTS {
        return None;
    }
    let (anchor, rest) = coherence_terms(query);
    if anchor.is_empty() || rest.len() < MIN_REST_TERMS {
        return None;
    }
    let class = query_class(&anchor, &rest);
    let chars = evidence_chars(&anchor, &rest);
    let need = if chars.len() >= CJK_CHAR_EVIDENCE {
        CJK_CHAR_EVIDENCE
    } else {
        usize::MAX
    };
    let (mut judged, mut hits) = (0usize, 0usize);
    for row in rows {
        let hay = haystack(row);
        if class.is_some_and(|c| foreign(c, &hay)) {
            continue;
        }
        judged += 1;
        let by_chars =
            need != usize::MAX && chars.iter().filter(|c| hay.contains(**c)).count() >= need;
        if mentions(&hay, &rest) || by_chars {
            hits += 1;
        }
    }
    if judged < MIN_RESULTS {
        return None;
    }
    Some(hits as f64 / judged as f64)
}

/// The bucket is evaluable and almost none of it is on topic.
pub fn looks_like_decoy(query: &str, rows: &[Hit]) -> bool {
    bucket_coherence(query, rows).is_some_and(|s| s < COHERENCE_MIN)
}

/// The bucket proves this query's terms do get echoed.
pub fn is_witness(query: &str, rows: &[Hit]) -> bool {
    bucket_coherence(query, rows).is_some_and(|s| s >= WITNESS_MIN)
}

#[cfg(test)]
mod tests {
    //! Ported from `tests/test_coherence.py` (titles are the real Bing decoys
    //! of the 2026-08-29 capture), plus the CJK-fix regressions.
    use super::*;

    fn rows(pairs: &[(&str, &str)]) -> Vec<Hit> {
        pairs
            .iter()
            .enumerate()
            .map(|(i, (t, s))| {
                let mut h = Hit::new(*t, format!("https://example.org/{i}"));
                h.snippet = (*s).to_owned();
                h
            })
            .collect()
    }

    fn rust_decoy() -> Vec<Hit> {
        rows(&[
            (
                "Rust on Steam",
                "The only aim in Rust is to survive. Everything wants you to die.",
            ),
            (
                "Rust — Explore, Build and Survive",
                "The only aim in Rust is to survive.",
            ),
            (
                "Rust Programming Language",
                "Rust in production. Hundreds of companies around the world",
            ),
            (
                "Install Rust - Rust Programming Language",
                "A language empowering everyone to build",
            ),
            (
                "Rust (programming language) - Wikipedia",
                "Rust is a general-purpose programming language",
            ),
            (
                "Rust (video game) - Wikipedia",
                "Rust is a 2018 multiplayer survival video game",
            ),
            (
                "Rust+ - Official Companion App — Rust",
                "The official Rust companion app.",
            ),
            (
                "Rust - Download",
                "May 7, 2026 · Rust is a PvP-driven game with deep crafting",
            ),
        ])
    }

    fn rust_healthy() -> Vec<Hit> {
        rows(&[
            (
                "Understanding Ownership - The Rust Programming Language",
                "Ownership is Rust's most",
            ),
            (
                "References and Borrowing - The Rust Programming Language",
                "The issue with the tuple",
            ),
            (
                "Rust Basics Explained: Borrowing in Rust",
                "Borrowing is Rust's way of letting you use",
            ),
            (
                "Improved Rust Borrow Checker Enters Testing",
                "The new borrow checker, Polonius",
            ),
            (
                "Ownership & Borrowing · Learn Rust",
                "Ownership is Rust's most unique feature",
            ),
        ])
    }

    fn vietnam_decoy() -> Vec<Hit> {
        rows(&[
            (
                "Vietnam - Wikipedia",
                "Upon the North Vietnamese victory in 1975, Vietnam reunified",
            ),
            (
                "Vietnam | History, Population, Map, Flag, Government, & Facts",
                "Vietnam, country",
            ),
            (
                "Visit Vietnam: The Official Tourism Website of Vietnam",
                "Discover the highlights",
            ),
            (
                "Vietnam Maps & Facts - World Atlas",
                "Vietnam is situated in Southeast Asia",
            ),
            (
                "History of Vietnam - Wikipedia",
                "The Second World War brought a 5-year occupation",
            ),
        ])
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_owned()).collect()
    }

    #[test]
    fn the_captured_decoys_are_recognised() {
        assert_eq!(
            bucket_coherence("rust ownership borrowing", &rust_decoy()),
            Some(0.0)
        );
        assert!(looks_like_decoy("rust ownership borrowing", &rust_decoy()));
        assert!(looks_like_decoy("vietnam gdp growth", &vietnam_decoy()));
    }

    #[test]
    fn a_healthy_bucket_for_the_same_query_is_a_witness() {
        assert_eq!(
            bucket_coherence("rust ownership borrowing", &rust_healthy()),
            Some(1.0)
        );
        assert!(!looks_like_decoy(
            "rust ownership borrowing",
            &rust_healthy()
        ));
        assert!(is_witness("rust ownership borrowing", &rust_healthy()));
    }

    #[test]
    fn the_cut_sits_between_the_two_measured_populations() {
        const { assert!(0.2 < COHERENCE_MIN && COHERENCE_MIN < 0.5) };
    }

    #[test]
    fn a_mostly_off_topic_bucket_with_one_stray_hit_is_still_a_decoy() {
        let mut bucket = rust_decoy()[..7].to_vec();
        bucket.extend(rows(&[(
            "Rust ownership explained",
            "borrowing and lifetimes",
        )]));
        assert_eq!(
            bucket_coherence("rust ownership borrowing", &bucket),
            Some(1.0 / 8.0)
        );
        assert!(looks_like_decoy("rust ownership borrowing", &bucket));
    }

    #[test]
    fn short_terms_survive_and_match_whole_words_only() {
        assert_eq!(
            coherence_terms("vietnam gdp growth"),
            (s(&["vietnam"]), s(&["gdp", "growth"]))
        );
        let bucket = rows(&[("Vietnam travel", "the gdpr notice on a tourism page"); 4]);
        assert_eq!(bucket_coherence("vietnam gdp growth", &bucket), Some(0.0));
    }

    #[test]
    fn a_light_stem_lets_borrowing_find_borrow() {
        assert_eq!(
            coherence_terms("rust ownership borrowing").1,
            s(&["ownership", "borrow"])
        );
        assert_eq!(coherence_terms("class hierarchy").0, s(&["class"]));
        assert_eq!(coherence_terms("runs slowly").0, s(&["runs"]));
    }

    #[test]
    fn one_and_two_word_queries_cannot_be_evaluated() {
        assert_eq!(bucket_coherence("rust", &rust_decoy()), None);
        assert_eq!(bucket_coherence("rust ownership", &rust_decoy()), None);
        assert!(!looks_like_decoy("rust ownership", &rust_decoy()));
        assert!(!is_witness("rust ownership", &rust_healthy()));
    }

    #[test]
    fn too_few_results_cannot_be_evaluated() {
        assert_eq!(
            bucket_coherence("rust ownership borrowing", &rust_decoy()[..3]),
            None
        );
        assert_eq!(
            bucket_coherence("rust ownership borrowing", &rust_decoy()[..4]),
            Some(0.0)
        );
    }

    #[test]
    fn operators_and_exclusions_are_not_expected_to_be_echoed() {
        let (anchor, rest) = coherence_terms(
            "rust site:doc.rust-lang.org ownership -game -\"video game\" filetype:pdf OR borrowing",
        );
        assert_eq!(anchor, s(&["rust"]));
        assert_eq!(rest, s(&["ownership", "borrow"]));
    }

    #[test]
    fn stopwords_prove_nothing() {
        assert_eq!(
            coherence_terms("python how to read the file"),
            (s(&["python"]), s(&["read", "file"]))
        );
    }

    #[test]
    fn a_rest_term_that_overlaps_the_anchor_proves_nothing() {
        let (_, rest) = coherence_terms("postgres postgresql explain");
        assert_eq!(rest, s(&["explain"]));
        assert_eq!(
            bucket_coherence("postgres postgresql explain", &rust_decoy()),
            None
        );
    }

    #[test]
    fn segmented_chinese_uses_bigrams() {
        let (anchor, rest) = coherence_terms("上海 天气 预报");
        assert_eq!(anchor, s(&["上海"]));
        assert_eq!(rest, s(&["天气", "预报"]));
        let decoy = rows(&[("上海 - 维基百科", "上海市是中华人民共和国的直辖市"); 5]);
        let healthy = rows(&[("上海天气预报", "上海今天多云，未来一周天气"); 5]);
        assert!(looks_like_decoy("上海 天气 预报", &decoy));
        assert!(is_witness("上海 天气 预报", &healthy));
    }

    #[test]
    fn unsegmented_chinese_splits_inside_the_run() {
        let (anchor, rest) = coherence_terms("上海明珠塔门票价格");
        assert_eq!(anchor, s(&["上海"]));
        assert!(!rest.contains(&"海明".to_owned()));
        assert_eq!(rest, s(&["明珠", "珠塔", "塔门", "门票", "票价", "价格"]));
        assert_eq!(
            bucket_coherence("人工智能", &rows(&[("人工", "人工"); 5])),
            None
        );
    }

    #[test]
    fn mixed_script_tokens_contribute_both_scripts() {
        let (anchor, rest) = coherence_terms("RoboMaster 2026赛季 规则手册");
        assert_eq!(anchor, s(&["robomaster"]));
        assert_eq!(rest, s(&["赛季", "2026", "规则", "则手", "手册"]));
    }

    #[test]
    fn version_numbers_stay_whole_and_dotted_names_split() {
        assert_eq!(
            coherence_terms("python 3.12 asyncio.TaskGroup").1,
            s(&["3.12", "asyncio", "taskgroup"])
        );
    }

    #[test]
    fn the_url_counts_as_evidence() {
        let bucket: Vec<Hit> = (0..4)
            .map(|i| {
                Hit::new(
                    "Documentation",
                    format!("https://pg.example/docs/explain-{i}"),
                )
            })
            .collect();
        assert_eq!(
            bucket_coherence("postgres explain analyze", &bucket),
            Some(1.0)
        );
    }

    // --- the CJK fix (SPEC §5, Spike N): synthetic, no real Naver capture exists ---

    #[test]
    fn a_korean_bucket_for_a_chinese_query_is_not_a_decoy() {
        let q = "图神经网络 异常检测";
        let korean = rows(
            &[(
                "그래프 신경망 이상 탐지",
                "그래프 신경망을 이용한 이상 탐지 방법",
            ); 6],
        );
        assert_eq!(
            bucket_coherence(q, &korean),
            None,
            "unjudgeable, never a decoy"
        );
        assert!(!looks_like_decoy(q, &korean));
        // The same query answered in English (Bing's Spike N decoy) is still caught.
        let english = rows(&[("Netflix plans", "Streaming prices rise again"); 6]);
        assert!(looks_like_decoy(q, &english));
        // Chinese results that echo the rest of the query are a witness.
        let chinese = rows(&[("基于图神经网络的异常检测", "图异常检测综述"); 6]);
        assert!(is_witness(q, &chinese));
    }

    #[test]
    fn japanese_rows_for_a_chinese_query_are_unjudgeable() {
        let q = "人工智能 新闻 动态";
        let japanese = rows(&[("人工知能のニュース", "最新の人工知能に関する記事です"); 5]);
        assert_eq!(bucket_coherence(q, &japanese), None);
    }

    #[test]
    fn distinct_rest_characters_count_without_a_bigram() {
        let q = "上海明珠塔门票价格";
        // Splits every rest bigram but carries 门, 票, 价 (three distinct chars).
        let spaced = rows(&[("明 珠", "门 票 价 一览"); 5]);
        assert_eq!(bucket_coherence(q, &spaced), Some(1.0));
        // Two distinct characters are not enough.
        let weak = rows(&[("上海旅游", "今天门口有票"); 5]);
        assert_eq!(bucket_coherence(q, &weak), Some(0.0));
    }

    #[test]
    fn a_mixed_script_query_judges_every_row() {
        // A Latin rest term keeps foreign-script rows judgeable (Python behaviour).
        let q = "RoboMaster 2026赛季 规则手册";
        let korean = rows(&[("로보마스터 규칙", "대회 안내"); 5]);
        assert_eq!(bucket_coherence(q, &korean), Some(0.0));
    }
}
