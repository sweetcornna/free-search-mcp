//! Title similarity (ports of the rapidfuzz measures the Python oracle uses).
//!
//! The `rapidfuzz` crate only ships `fuzz::ratio` (normalised InDel
//! similarity, 0..1); `token_sort_ratio` and `token_set_ratio` are rebuilt
//! on top of it exactly as rapidfuzz defines them, scaled to 0..100.
//! [`title_score`] and [`best_title_match`] port `paper_graph._title_score` /
//! `_best_title_match`: the score is a GATE (≥ [`TITLE_MIN`]), relevance order
//! decides among candidates that pass it.

use regex::Regex;
use std::collections::BTreeSet;
use std::sync::LazyLock;
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

/// How close a candidate's title must be to count as "the paper you meant".
pub const TITLE_MIN: f64 = 85.0;

/// rapidfuzz `utils.default_process`: lower-case, every non-alphanumeric
/// character becomes a space, trimmed (inner runs of spaces are kept).
pub fn default_process(s: &str) -> String {
    let mapped: String = s
        .chars()
        .flat_map(|c| {
            if c.is_alphanumeric() {
                c.to_lowercase().collect::<Vec<_>>()
            } else {
                vec![' ']
            }
        })
        .collect();
    mapped.trim().to_owned()
}

/// `fold(title)`: compatibility-decomposed, diacritics dropped, processed
/// as [`default_process`] and single-spaced. "Schrödinger’s  Cat" →
/// "schrodinger s cat".
pub fn fold_title(s: &str) -> String {
    let stripped: String = s.nfkd().filter(|c| !is_combining_mark(*c)).collect();
    default_process(&stripped)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whitespace-normalised [`default_process`] (the oracle's exact-title key).
pub fn exact_key(s: &str) -> String {
    default_process(s)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// rapidfuzz `fuzz.ratio`, 0..100.
pub fn ratio(a: &str, b: &str) -> f64 {
    100.0 * rapidfuzz::fuzz::ratio(a.chars(), b.chars())
}

fn sorted_tokens(s: &str) -> String {
    let mut t: Vec<&str> = s.split_whitespace().collect();
    t.sort_unstable();
    t.join(" ")
}

/// rapidfuzz `fuzz.token_sort_ratio` (no processor), 0..100.
pub fn token_sort_ratio(a: &str, b: &str) -> f64 {
    ratio(&sorted_tokens(a), &sorted_tokens(b))
}

/// rapidfuzz `fuzz.token_set_ratio` (no processor), 0..100. Scores the
/// intersection: when one token set contains the other it is 100.
pub fn token_set_ratio(a: &str, b: &str) -> f64 {
    let ta: BTreeSet<&str> = a.split_whitespace().collect();
    let tb: BTreeSet<&str> = b.split_whitespace().collect();
    if ta.is_empty() || tb.is_empty() {
        return 0.0;
    }
    let inter: Vec<&str> = ta.intersection(&tb).copied().collect();
    let ab: Vec<&str> = ta.difference(&tb).copied().collect();
    let ba: Vec<&str> = tb.difference(&ta).copied().collect();
    if !inter.is_empty() && (ab.is_empty() || ba.is_empty()) {
        return 100.0;
    }
    let sect = inter.join(" ");
    let join = |rest: &[&str]| {
        if sect.is_empty() {
            rest.join(" ")
        } else {
            format!("{sect} {}", rest.join(" "))
        }
    };
    let (s1, s2) = (join(&ab), join(&ba));
    let mut best = ratio(&s1, &s2);
    if !sect.is_empty() {
        best = best.max(ratio(&sect, &s1)).max(ratio(&sect, &s2));
    }
    best
}

/// How faithfully `query` renders `title`, 0..100: the better of the
/// whole-string `token_sort_ratio` and the same ratio against the title cut
/// to the query's word count (so a typed prefix still scores 100, while a
/// title that merely CONTAINS the words further in does not).
pub fn title_score(query: &str, title: &str) -> f64 {
    let q = default_process(query);
    let t = default_process(title);
    let whole = token_sort_ratio(&q, &t);
    let n = q.split_whitespace().count();
    let prefix: Vec<&str> = t.split_whitespace().take(n).collect();
    whole.max(token_sort_ratio(&q, &prefix.join(" ")))
}

/// The FIRST candidate (relevance order) whose title clears [`TITLE_MIN`].
pub fn best_title_match<'a>(
    query: &str,
    titles: impl IntoIterator<Item = &'a str>,
) -> Option<usize> {
    titles
        .into_iter()
        .position(|t| !t.trim().is_empty() && title_score(query, t) >= TITLE_MIN)
}

static DIGITS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[0-9]+").unwrap_or_else(|e| panic!("digits regex: {e}")));

/// The digit runs of a title (the aggregator's numeric guard: "GPT-3" and
/// "GPT-4" are different papers however similar the rest is).
pub fn digit_tokens(s: &str) -> Vec<String> {
    DIGITS.find_iter(s).map(|m| m.as_str().to_owned()).collect()
}

/// Two titles that both carry digits must carry the same ones.
pub fn numbers_agree(a: &[String], b: &[String]) -> bool {
    a.is_empty() || b.is_empty() || a == b
}

/// Generational suffixes stripped before choosing a surname.
fn is_gen_suffix(w: &str) -> bool {
    let t = w.trim_end_matches('.').to_ascii_lowercase();
    matches!(t.as_str(), "jr" | "sr" | "ii" | "iii" | "iv")
}

/// Remove trailing generational suffixes (`"John Smith Jr."`, `"John Smith, Jr."`,
/// `"Smith III"`) while keeping at least one word.
fn strip_gen_suffix(raw: &str) -> String {
    let mut s = raw.trim().to_owned();
    loop {
        let trimmed = s.trim_end();
        // Handle `"John Smith, Jr."` where the part after the last comma is ONLY a suffix.
        if let Some((before, after)) = trimmed.rsplit_once(',') {
            let after_t = after.trim();
            if !before.trim().is_empty()
                && !after_t.is_empty()
                && after_t.split_whitespace().all(is_gen_suffix)
            {
                s = before.trim_end().to_owned();
                continue;
            }
        }
        let words: Vec<&str> = trimmed.split_whitespace().collect();
        if words.len() >= 2 && is_gen_suffix(words[words.len() - 1]) {
            s = words[..words.len() - 1].join(" ");
            continue;
        }
        break;
    }
    s
}

fn last_alpha_word(s: &str) -> Option<String> {
    fold_title(s)
        .split_whitespace()
        .rfind(|w| w.chars().any(char::is_alphabetic))
        .map(str::to_owned)
}

fn first_alpha_char(s: &str) -> Option<char> {
    fold_title(s).chars().find(|c| c.is_alphabetic())
}

fn is_all_caps_word(w: &str) -> bool {
    let mut has = false;
    for c in w.chars().filter(|c| c.is_alphabetic()) {
        has = true;
        if !c.is_uppercase() {
            return false;
        }
    }
    has
}

fn is_initials_token(w: &str) -> bool {
    let letters: Vec<char> = w.chars().filter(|c| c.is_alphabetic()).collect();
    !letters.is_empty() && letters.len() <= 3 && letters.iter().all(|c| c.is_uppercase())
}

/// All plausible folded surname keys for an author name.
///
/// Usually returns one key; for ambiguous `<Word> <2-3 ALL-CAPS>` strings
/// without periods (e.g. `"Kaiming HE"` vs PubMed `"Wilding JPH"`), returns
/// both candidates so [`surnames_match`] matches either convention.
pub fn surname_candidates(name: &str) -> Vec<String> {
    let cleaned = strip_gen_suffix(name);
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        return Vec::new();
    }
    if let Some((family, _)) = cleaned.split_once(',') {
        let family = strip_gen_suffix(family);
        return last_alpha_word(&family).into_iter().collect();
    }
    let words: Vec<&str> = cleaned.split_whitespace().collect();
    match words.as_slice() {
        [] => Vec::new(),
        [one] => last_alpha_word(one).into_iter().collect(),
        [rest @ .., last] => {
            let last_letters: Vec<char> = last.chars().filter(|c| c.is_alphabetic()).collect();
            let all_words_caps = words.iter().all(|w| is_all_caps_word(w));
            let rest_all_initials = rest.iter().all(|w| is_initials_token(w));
            // Single-letter uppercase tail (`"Vaswani A"`, `"HE K"`) or punctuated
            // initials (`"Smith J.P."`) following a non-initial word: unambiguously PubMed.
            let unambiguous_pubmed = !rest_all_initials
                && !last_letters.is_empty()
                && last_letters.iter().all(|c| c.is_uppercase())
                && ((last_letters.len() == 1
                    && rest
                        .iter()
                        .any(|w| w.chars().filter(|c| c.is_alphabetic()).count() > 1))
                    || (last_letters.len() <= 3 && last.contains('.')));
            if unambiguous_pubmed {
                return last_alpha_word(&rest.join(" ")).into_iter().collect();
            }
            // All-caps full name (`"JOHN LI"`, `"KAIMING HE"`): last token is the surname.
            if all_words_caps {
                return last_alpha_word(last).into_iter().collect();
            }
            // 2–3 all-caps letters without periods following a mixed-case word:
            // could be PubMed `"Wilding JPH"` / `"van der Berg JM"` OR
            // `"Given FAMILY"` (`"Kaiming HE"`, `"Andrew NG"`, `"Fei-Fei LI"`).
            let ambiguous_caps_tail = !rest_all_initials
                && (2..=3).contains(&last_letters.len())
                && last_letters.iter().all(|c| c.is_uppercase())
                && !last.contains('.');
            if ambiguous_caps_tail {
                let mut out = Vec::with_capacity(2);
                if let Some(a) = last_alpha_word(&rest.join(" ")) {
                    out.push(a);
                }
                if let Some(b) = last_alpha_word(last)
                    && !out.contains(&b)
                {
                    out.push(b);
                }
                return out;
            }
            last_alpha_word(last).into_iter().collect()
        }
    }
}

/// Whether two author strings refer to the same surname (handling `"Given FAMILY"`,
/// PubMed `"Surname JPH"`, `"Family, Given"`, and generational suffixes).
pub fn surnames_match(a: &str, b: &str) -> bool {
    let ca = surname_candidates(a);
    let cb = surname_candidates(b);
    ca.iter().any(|x| cb.iter().any(|y| x == y))
}

/// `(surname, given_initial)` candidate pairs for co-author matching during merge.
pub(crate) fn author_key_pairs(name: &str) -> Vec<(String, Option<char>)> {
    let cleaned = strip_gen_suffix(name);
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        return Vec::new();
    }
    if let Some((family, given)) = cleaned.split_once(',') {
        let family = strip_gen_suffix(family);
        return last_alpha_word(&family)
            .map(|s| (s, first_alpha_char(given)))
            .into_iter()
            .collect();
    }
    let words: Vec<&str> = cleaned.split_whitespace().collect();
    match words.as_slice() {
        [] => Vec::new(),
        [one] => last_alpha_word(one)
            .map(|s| (s, None))
            .into_iter()
            .collect(),
        [rest @ .., last] => {
            let cands = surname_candidates(cleaned);
            let rest_str = rest.join(" ");
            let rest_last = last_alpha_word(&rest_str);
            let tail_last = last_alpha_word(last);
            let mut out = Vec::with_capacity(cands.len());
            for s in cands {
                let init = if tail_last.as_deref() == Some(s.as_str())
                    && rest_last.as_deref() != Some(s.as_str())
                {
                    first_alpha_char(&rest_str)
                } else if rest_last.as_deref() == Some(s.as_str()) {
                    first_alpha_char(last)
                } else {
                    first_alpha_char(&rest_str)
                };
                out.push((s, init));
            }
            out
        }
    }
}

/// A comparable key for an author's surname: the last alphabetic word of the
/// family name, folded. Handles "Ashish Vaswani", "Vaswani, Ashish",
/// "Vaswani A" (PubMed), "A. Vaswani", "John Smith Jr.", "JOHN LI" and
/// "van der Berg JM". For matching `"Given FAMILY"` against `"Given Family"`,
/// use [`surnames_match`].
pub fn surname_key(name: &str) -> Option<String> {
    surname_candidates(name).into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Port of `test_paper_graph.py::test_title_score_separates_the_paper_from_its_neighbours`.
    #[test]
    fn test_title_score_separates_the_paper_from_its_neighbours() {
        let cases = [
            (
                "attention is all you need",
                "Attention Is All You Need",
                true,
            ),
            // A caller who typed only the opening words still means this paper.
            (
                "reciprocal rank fusion outperforms condorcet",
                "Reciprocal rank fusion outperforms condorcet and individual rank learning methods",
                true,
            ),
            // Words in common, different paper.
            (
                "attention is all you need",
                "Channel Attention Is All You Need for Video Frame Interpolation",
                false,
            ),
            (
                "BERT pre-training of deep bidirectional transformers",
                "BEiT: BERT Pre-Training of Image Transformers",
                false,
            ),
            (
                "BERT pre-training of deep bidirectional transformers",
                "BioBERT: a pre-trained biomedical language representation model",
                false,
            ),
        ];
        for (q, t, confident) in cases {
            assert_eq!(title_score(q, t) >= TITLE_MIN, confident, "{q} / {t}");
        }
    }

    /// The oracle docstring's calibration numbers (70 / 100 / 75 / 100).
    #[test]
    fn scores_match_the_rapidfuzz_values_the_oracle_documents() {
        let q = default_process("reciprocal rank fusion outperforms condorcet");
        let full = default_process(
            "Reciprocal rank fusion outperforms condorcet and individual rank learning methods",
        );
        assert!((token_sort_ratio(&q, &full) - 70.4).abs() < 0.05);
        assert_eq!(
            title_score("attention is all you need", "Attention Is All You Need"),
            100.0
        );
        let channel = title_score(
            "attention is all you need",
            "Channel Attention Is All You Need for Video Frame Interpolation",
        );
        assert!((channel - 75.47).abs() < 0.05, "{channel}");
        // token_set_ratio scores every superset title 100: why it is never the gate.
        assert_eq!(
            token_set_ratio(
                &fold_title("attention is all you need"),
                &fold_title("Attention Is All You Need In Speech Separation")
            ),
            100.0
        );
        assert_eq!(
            token_set_ratio("python 3 13 released", "python 3 12 released"),
            token_set_ratio("python 3 12 released", "python 3 13 released")
        );
        assert_eq!(token_set_ratio("", "x"), 0.0);
        assert_eq!(ratio("", ""), 100.0);
    }

    /// Port of `test_paper_graph.py::test_title_match_follows_relevance_order_not_the_highest_score`.
    #[test]
    fn test_title_match_follows_relevance_order_not_the_highest_score() {
        let items = [
            "Attention Is All You Need",
            "Attention Is All You Need In Speech Separation",
        ];
        assert_eq!(
            best_title_match("attention is all you need", items),
            Some(0)
        );
        // Order decides, not score: both clear the gate as a prefix.
        let swapped = [items[1], items[0]];
        assert_eq!(
            best_title_match("attention is all you need", swapped),
            Some(0)
        );
    }

    /// Port of `test_paper_graph.py::test_no_candidate_clearing_the_gate_resolves_to_nothing`.
    #[test]
    fn test_no_candidate_clearing_the_gate_resolves_to_nothing() {
        let items = [
            "BioBERT: a pre-trained biomedical language model",
            "Sentence-BERT: Sentence Embeddings using Siamese Networks",
            "BEiT: BERT Pre-Training of Image Transformers",
        ];
        assert_eq!(
            best_title_match("BERT pre-training of deep bidirectional", items),
            None
        );
        assert_eq!(best_title_match("x", ["", "  "]), None);
    }

    /// Port of `test_paper_graph.py::test_arxiv_exact_match_normalizes_spacing_and_punctuation`
    /// (the key half: both variants fold to the same exact key).
    #[test]
    fn test_arxiv_exact_match_normalizes_spacing_and_punctuation() {
        let key = exact_key("Attention Is All You Need");
        assert_eq!(exact_key("Attention  Is  All  You  Need"), key);
        assert_eq!(exact_key("Attention — Is — All — You — Need"), key);
        assert_ne!(
            exact_key("Attention Is All You Need In Speech Separation"),
            key
        );
    }

    /// Ports of `test_merge_identity.py`'s digit-guard tests.
    #[test]
    fn test_titles_whose_numbers_differ_are_still_distinct() {
        for (a, b) in [
            ("Python 3.13 released", "Python 3.12 released"),
            ("iPhone 15 review", "iPhone 14 review"),
            (
                "Fed cuts rates by 25 basis points",
                "Fed cuts rates by 50 basis points",
            ),
        ] {
            assert!(!numbers_agree(&digit_tokens(a), &digit_tokens(b)), "{a}");
        }
    }

    #[test]
    fn test_an_arxiv_id_prefix_does_not_make_the_same_paper_distinct() {
        let a = "Deep Residual Learning for Image Recognition";
        let b = "[1512.03385] Deep Residual Learning for Image Recognition";
        assert!(numbers_agree(&digit_tokens(a), &digit_tokens(b)));
        assert!(token_set_ratio(&fold_title(a), &fold_title(b)) >= 92.0);
    }

    #[test]
    fn folding_and_surnames() {
        assert_eq!(
            fold_title("Schrödinger’s  Cat: a Re-View"),
            "schrodinger s cat a re view"
        );
        for (name, key) in [
            ("Ashish Vaswani", "vaswani"),
            ("Vaswani, Ashish", "vaswani"),
            ("Vaswani A", "vaswani"),
            ("A. Vaswani", "vaswani"),
            ("van der Berg JM", "berg"),
            ("Jan van der Berg", "berg"),
            ("Luke Melas-Kyriazi", "kyriazi"),
            ("Melas-Kyriazi L", "kyriazi"),
            ("Gupta D", "gupta"),
            ("Ng", "ng"),
            ("José Müller", "muller"),
        ] {
            assert_eq!(surname_key(name).as_deref(), Some(key), "{name}");
        }
        assert_eq!(surname_key("  "), None);
    }

    #[test]
    fn surnames_handle_generational_suffixes_and_given_family_caps() {
        assert_eq!(surname_key("John Smith Jr.").as_deref(), Some("smith"));
        assert_eq!(surname_key("John Smith, Jr.").as_deref(), Some("smith"));
        assert_eq!(surname_key("Smith, John Jr.").as_deref(), Some("smith"));
        assert_eq!(surname_key("William Gates III").as_deref(), Some("gates"));
        assert_eq!(surname_key("JOHN LI").as_deref(), Some("li"));
        // Both "Given FAMILY" and PubMed "Surname JPH" match cleanly.
        assert!(surnames_match("Kaiming HE", "Kaiming He"));
        assert!(surnames_match("Andrew NG", "Andrew Ng"));
        assert!(surnames_match("Fei-Fei LI", "Fei-Fei Li"));
        assert!(surnames_match("JOHN LI", "John Li"));
        assert!(surnames_match("Wilding JPH", "John Wilding"));
        assert!(surnames_match("John Smith Jr.", "Smith, John"));
        assert!(!surnames_match("Kaiming HE", "Kaiming Zhang"));
        assert!(!surnames_match("Wilding JPH", "Alice Smith"));
    }
}
