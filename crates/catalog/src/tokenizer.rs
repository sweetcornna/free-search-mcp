//! Catalog tokenizer: NFKC normalization, camelCase/identifier splitting, Snowball English
//! stemming (via `bm25::DefaultTokenizer`), and CJK character bigrams.

use bm25::{DefaultTokenizer, Language, Tokenizer};
use unicode_normalization::UnicodeNormalization;

/// Minimal stopword list that preserves meaningful 2-letter catalog tokens
/// (`ai`, `ml`, `us`, `uk`, `eu`, `cn`, `jp`, `kr`, `fr`, `de`, `oa`, `ip`, `go`, `qa`).
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "in", "into", "is", "it",
    "its", "of", "on", "or", "that", "the", "this", "to", "via", "with",
];

/// Custom tokenizer for [`bm25::SearchEngine`] supporting English stemming and CJK bigrams.
pub(crate) struct CatalogTokenizer {
    en_stemmer: DefaultTokenizer,
}

impl CatalogTokenizer {
    pub(crate) fn new() -> Self {
        let en_stemmer = DefaultTokenizer::builder()
            .language_mode(Language::English)
            .normalization(true)
            .stemming(true)
            .stopwords(false)
            .build();
        Self { en_stemmer }
    }

    /// Stem a single lowercased Latin/ASCII token using `bm25`'s Snowball English stemmer.
    pub(crate) fn stem_word(&self, word: &str) -> String {
        if word.len() <= 2 || !word.is_ascii() {
            return word.to_owned();
        }
        let mut stemmed = self.en_stemmer.tokenize(word);
        if stemmed.len() == 1 && !stemmed[0].is_empty() {
            stemmed.remove(0)
        } else {
            word.to_owned()
        }
    }

    /// Split a text into raw normalized semantic units (Latin/number words + CJK bigrams),
    /// returning `(raw_lower, stemmed)` pairs for Latin words and `(gram, gram)` for CJK.
    pub(crate) fn analyze_terms(&self, input_text: &str) -> Vec<(String, String)> {
        let norm: String = input_text.nfkc().collect();
        let mut out = Vec::new();
        let mut latin_buf = String::new();
        let mut cjk_buf: Vec<char> = Vec::new();

        let flush_latin = |buf: &mut String, out: &mut Vec<(String, String)>| {
            if buf.is_empty() {
                return;
            }
            for part in split_camel_case(buf) {
                let lower = part.to_lowercase();
                if lower.is_empty() || STOPWORDS.contains(&lower.as_str()) {
                    continue;
                }
                let stem = self.stem_word(&lower);
                out.push((lower, stem));
            }
            buf.clear();
        };

        let flush_cjk = |buf: &mut Vec<char>, out: &mut Vec<(String, String)>| {
            if buf.is_empty() {
                return;
            }
            if buf.len() == 1 {
                let s = buf[0].to_string();
                out.push((s.clone(), s));
            } else {
                for w in buf.windows(2) {
                    let mut gram = String::with_capacity(6);
                    gram.push(w[0]);
                    gram.push(w[1]);
                    out.push((gram.clone(), gram));
                }
            }
            buf.clear();
        };

        for ch in norm.chars() {
            if is_cjk(ch) {
                flush_latin(&mut latin_buf, &mut out);
                cjk_buf.push(ch);
            } else if ch.is_alphanumeric() {
                flush_cjk(&mut cjk_buf, &mut out);
                latin_buf.push(ch);
            } else {
                flush_latin(&mut latin_buf, &mut out);
                flush_cjk(&mut cjk_buf, &mut out);
            }
        }
        flush_latin(&mut latin_buf, &mut out);
        flush_cjk(&mut cjk_buf, &mut out);
        out
    }
}

impl Default for CatalogTokenizer {
    fn default() -> Self {
        Self::new()
    }
}

impl Tokenizer for CatalogTokenizer {
    fn tokenize(&self, input_text: &str) -> Vec<String> {
        let mut tokens = Vec::new();
        for (raw, stem) in self.analyze_terms(input_text) {
            tokens.push(stem);
            // Keep raw form too if distinct and short/acronym-like or identifier-helpful,
            // wait: emitting only stem keeps BM25 TF clean while exact matches share stems.
            // For raw != stem where raw is an acronym/proper term, stem is deterministic for both doc & query.
            let _ = raw;
        }
        tokens
    }
}

/// Split camelCase / PascalCase runs (`fetchBatch` -> `["fetch", "Batch"]`, `HTMLParser` -> `["HTML", "Parser"]`).
fn split_camel_case(word: &str) -> Vec<&str> {
    if word.is_empty() {
        return Vec::new();
    }
    let chars: Vec<(usize, char)> = word.char_indices().collect();
    if chars.len() <= 1 {
        return vec![word];
    }
    let mut parts = Vec::new();
    let mut start = 0;
    for i in 1..chars.len() {
        let prev = chars[i - 1].1;
        let curr = chars[i].1;
        let next = chars.get(i + 1).map(|(_, c)| *c);
        let split_here = (prev.is_lowercase() && curr.is_uppercase())
            || (prev.is_uppercase() && curr.is_uppercase() && next.is_some_and(char::is_lowercase));
        if split_here {
            parts.push(&word[start..chars[i].0]);
            start = chars[i].0;
        }
    }
    parts.push(&word[start..]);
    parts
}

/// True for CJK Unified Ideographs, Kana, and Hangul syllables.
pub(crate) fn is_cjk(ch: char) -> bool {
    matches!(
        ch as u32,
        0x3400..=0x4DBF   // CJK Extension A
        | 0x4E00..=0x9FFF // CJK Unified Ideographs
        | 0xF900..=0xFAFF // CJK Compatibility Ideographs
        | 0x3040..=0x309F // Hiragana
        | 0x30A0..=0x30FF // Katakana
        | 0xAC00..=0xD7AF // Hangul Syllables
        | 0x20000..=0x2A6DF // CJK Extension B
    )
}
