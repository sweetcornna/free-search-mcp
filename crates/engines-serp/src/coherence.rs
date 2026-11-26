//! Local port of `search_mcp/coherence.py` for Bing's decoy-page detection.
//!
//! `fsm-engines-serp` cannot depend on `fsm-search` (review note 6), so this
//! compact local copy lets `BingEngine` detect single-word decoy buckets and
//! re-mint its cookie jar once.

use fsm_core::types::Hit;
use regex::Regex;
use std::collections::HashSet;
use std::sync::LazyLock;

pub const COHERENCE_MIN: f64 = 0.3;
const MIN_RESULTS: usize = 4;
const MIN_REST_TERMS: usize = 2;

static OPERATOR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^[+-]?(?:site|filetype|ext|intitle|allintitle|inurl|allinurl|intext|inbody|inanchor|lang|language|loc|location|before|after|feed|contains|ip|prefer):",
    )
    .expect("regex")
});

static WORD_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\p{L}\p{N}]+(?:\.\d+)+|[\p{L}\p{N}]+").expect("regex"));

const STOPWORDS: &[&str] = &[
    "a", "about", "an", "and", "are", "as", "at", "be", "been", "best", "by", "can", "could",
    "did", "do", "does", "for", "from", "get", "had", "has", "have", "how", "i", "if", "in",
    "into", "is", "it", "its", "me", "my", "new", "no", "not", "of", "on", "or", "our", "should",
    "so", "than", "that", "the", "their", "then", "there", "these", "they", "this", "to", "top",
    "up", "us", "use", "using", "vs", "was", "we", "were", "what", "when", "where", "which", "who",
    "why", "will", "with", "would", "you", "your",
];

const SUFFIXES: &[&str] = &["ing", "ed", "es", "s"];

fn is_cjk(c: char) -> bool {
    let o = c as u32;
    (0x4E00..=0x9FFF).contains(&o)
        || (0x3040..=0x30FF).contains(&o)
        || (0xAC00..=0xD7A3).contains(&o)
}

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

fn bigrams(run: &str, start_char: usize) -> Vec<String> {
    let chars: Vec<char> = run.chars().collect();
    if chars.len() <= start_char + 1 {
        return Vec::new();
    }
    (start_char..chars.len() - 1)
        .map(|i| {
            let mut s = String::new();
            s.push(chars[i]);
            s.push(chars[i + 1]);
            s
        })
        .collect()
}

fn stem(word: &str) -> String {
    if !word.is_ascii() || !word.bytes().all(|b| b.is_ascii_alphabetic()) || word.ends_with("ss") {
        return word.to_owned();
    }
    for suffix in SUFFIXES {
        if word.ends_with(suffix) && word.len() >= suffix.len() + 4 {
            return word[..word.len() - suffix.len()].to_owned();
        }
    }
    word.to_owned()
}

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
    for m in WORD_RE.find_iter(&latin) {
        let w = m.as_str();
        if w.chars().count() < 2 || STOPWORDS.contains(&w) {
            continue;
        }
        terms.push(stem(w));
    }
    terms
}

fn content_tokens(query: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut in_excluded_phrase = false;
    for tok in query.split_whitespace() {
        if in_excluded_phrase {
            in_excluded_phrase = !tok.ends_with('"');
            continue;
        }
        if tok.starts_with("-\"") {
            in_excluded_phrase = !(tok.len() > 2 && tok.ends_with('"'));
            continue;
        }
        if matches!(tok, "OR" | "AND" | "NOT" | "|" | "&")
            || tok.starts_with('-')
            || OPERATOR_RE.is_match(tok)
        {
            continue;
        }
        tokens.push(tok);
    }
    tokens
}

/// Split `query` into `(anchor, rest)` terms (exact port of `coherence.py`).
pub fn coherence_terms(query: &str) -> (Vec<String>, Vec<String>) {
    let tokens = content_tokens(query);
    let mut anchor: Vec<String> = Vec::new();
    let mut rest: Vec<String> = Vec::new();

    if tokens.len() == 1 {
        let runs = cjk_runs(tokens[0]);
        let stripped = tokens[0]
            .trim_matches(|c| matches!(c, '"' | '\'' | '“' | '”' | '《' | '》' | '「' | '」'));
        if runs.len() == 1 && runs[0] == stripped && runs[0].chars().count() >= 4 {
            let chars: Vec<char> = runs[0].chars().collect();
            anchor.push(chars[..2].iter().collect());
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

    let mut seen: HashSet<String> = HashSet::new();
    let mut unique_rest = Vec::new();
    for term in rest {
        if seen.contains(&term)
            || anchor
                .iter()
                .any(|a| term.contains(a.as_str()) || a.contains(term.as_str()))
        {
            continue;
        }
        seen.insert(term.clone());
        unique_rest.push(term);
    }
    (anchor, unique_rest)
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn matches_term(haystack: &str, term: &str) -> bool {
    let is_short_non_cjk = term.chars().count() <= 3 && !term.chars().all(is_cjk);
    if !is_short_non_cjk {
        return haystack.contains(term);
    }
    // Emulate `(?<!\w)term(?!\w)` without look-around.
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(term) {
        let abs = start + pos;
        let end = abs + term.len();
        let before_ok = haystack[..abs]
            .chars()
            .next_back()
            .is_none_or(|c| !is_word_char(c));
        let after_ok = haystack[end..]
            .chars()
            .next()
            .is_none_or(|c| !is_word_char(c));
        if before_ok && after_ok {
            return true;
        }
        start = abs + term.chars().next().map_or(1, char::len_utf8);
    }
    false
}

/// Share of `hits` mentioning any query term beyond the first content token.
///
/// Returns `None` when the bucket is too small (`< 4`) or the query has too
/// few rest terms (`< 2`).
pub fn bucket_coherence(query: &str, hits: &[Hit]) -> Option<f64> {
    if hits.len() < MIN_RESULTS {
        return None;
    }
    let (anchor, rest) = coherence_terms(query);
    if anchor.is_empty() || rest.len() < MIN_REST_TERMS {
        return None;
    }
    let matched = hits
        .iter()
        .filter(|h| {
            let haystack = format!("{} {} {}", h.title, h.snippet, h.url).to_lowercase();
            rest.iter().any(|t| matches_term(&haystack, t))
        })
        .count();
    Some((matched as f64) / (hits.len() as f64))
}

/// True when `hits` is evaluable and almost none of it mentions terms past the
/// query's anchor word (Bing's cookieless decoy signature).
pub fn looks_like_decoy(query: &str, hits: &[Hit]) -> bool {
    bucket_coherence(query, hits).is_some_and(|score| score < COHERENCE_MIN)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(title: &str, snippet: &str, url: &str) -> Hit {
        let mut h = Hit::new(title, url);
        h.snippet = snippet.to_owned();
        h
    }

    #[test]
    fn decoy_and_real_buckets() {
        let decoy: Vec<Hit> = (0..8)
            .map(|i| {
                mk(
                    "Rust on Steam",
                    "The only aim in Rust is to survive.",
                    &format!("https://decoy.example/{i}"),
                )
            })
            .collect();
        let real: Vec<Hit> = (0..8)
            .map(|i| {
                mk(
                    "Understanding Ownership - Rust",
                    "Ownership and borrowing in Rust.",
                    &format!("https://r.example/{i}"),
                )
            })
            .collect();
        assert!(looks_like_decoy("rust ownership borrowing", &decoy));
        assert!(!looks_like_decoy("rust ownership borrowing", &real));
        // Short queries cannot be judged.
        assert!(!looks_like_decoy("rust", &decoy));
        assert!(!looks_like_decoy("rust ownership", &decoy));
    }

    #[test]
    fn short_terms_match_word_boundaries_only() {
        assert!(!matches_term("google search again", "go"));
        assert!(!matches_term("google search again", "ai"));
        assert!(matches_term("using go and ai tools", "go"));
        assert!(matches_term("using go and ai tools", "ai"));
    }
}
