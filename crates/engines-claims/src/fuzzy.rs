//! Pure-Rust port of the `rapidfuzz` scorers (`default_process`,
//! `token_sort_ratio`, `token_set_ratio`) and word-boundary helpers used by
//! `ImfEngine`.

use std::collections::BTreeSet;

/// Port of `rapidfuzz.utils.default_process`: replace non-alphanumeric chars
/// with space, lowercase, and trim surrounding whitespace.
pub fn default_process(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch.is_alphanumeric() {
            for lc in ch.to_lowercase() {
                out.push(lc);
            }
        } else {
            out.push(' ');
        }
    }
    out.trim().to_owned()
}

/// Indel normalized similarity (`rapidfuzz.fuzz.ratio`), scaled to `0.0..=100.0`.
pub fn indel_ratio(a: &str, b: &str) -> f64 {
    let ac: Vec<char> = a.chars().collect();
    let bc: Vec<char> = b.chars().collect();
    let total = ac.len() + bc.len();
    if total == 0 {
        return 100.0;
    }
    if ac.is_empty() || bc.is_empty() {
        return 0.0;
    }
    let lcs = lcs_len(&ac, &bc);
    (2 * lcs) as f64 * 100.0 / (total as f64)
}

fn lcs_len(a: &[char], b: &[char]) -> usize {
    // Keep the shorter slice as the DP row to minimize allocations.
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let mut prev = vec![0usize; short.len() + 1];
    let mut curr = vec![0usize; short.len() + 1];
    for &lc in long {
        for (j, &sc) in short.iter().enumerate() {
            curr[j + 1] = if lc == sc {
                prev[j] + 1
            } else {
                prev[j + 1].max(curr[j])
            };
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[short.len()]
}

/// Port of `rapidfuzz.fuzz.token_sort_ratio(a, b, processor=utils.default_process)`.
pub fn token_sort_ratio(a: &str, b: &str) -> f64 {
    let pa = default_process(a);
    let pb = default_process(b);
    if pa.is_empty() || pb.is_empty() {
        return 0.0;
    }
    let mut ta: Vec<&str> = pa.split_whitespace().collect();
    let mut tb: Vec<&str> = pb.split_whitespace().collect();
    ta.sort_unstable();
    tb.sort_unstable();
    indel_ratio(&ta.join(" "), &tb.join(" "))
}

/// Port of `rapidfuzz.fuzz.token_set_ratio(a, b, processor=utils.default_process)`.
pub fn token_set_ratio(a: &str, b: &str) -> f64 {
    let pa = default_process(a);
    let pb = default_process(b);
    if pa.is_empty() || pb.is_empty() {
        return 0.0;
    }
    let set_a: BTreeSet<&str> = pa.split_whitespace().collect();
    let set_b: BTreeSet<&str> = pb.split_whitespace().collect();

    let sect: Vec<&str> = set_a.intersection(&set_b).copied().collect();
    let diff_ab: Vec<&str> = set_a.difference(&set_b).copied().collect();
    let diff_ba: Vec<&str> = set_b.difference(&set_a).copied().collect();

    if !sect.is_empty() && (diff_ab.is_empty() || diff_ba.is_empty()) {
        return 100.0;
    }

    let sect_str = sect.join(" ");
    let ab_str = join_pair(&sect_str, &diff_ab.join(" "));
    let ba_str = join_pair(&sect_str, &diff_ba.join(" "));

    let r1 = if sect_str.is_empty() {
        0.0
    } else {
        indel_ratio(&sect_str, &ab_str)
    };
    let r2 = if sect_str.is_empty() {
        0.0
    } else {
        indel_ratio(&sect_str, &ba_str)
    };
    let r3 = indel_ratio(&ab_str, &ba_str);
    r1.max(r2).max(r3)
}

fn join_pair(a: &str, b: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (true, _) => b.to_owned(),
        (_, true) => a.to_owned(),
        (false, false) => format!("{a} {b}"),
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Word-boundary substring check equivalent to `re.search(rf"\b{re.escape(needle)}\b", haystack)`
/// where both arguments are already lower-cased.
pub fn contains_word_bounded(haystack: &str, needle: &str) -> bool {
    let Some(first) = needle.chars().next() else {
        return false;
    };
    let Some(last) = needle.chars().next_back() else {
        return false;
    };
    let first_w = is_word_char(first);
    let last_w = is_word_char(last);
    let mut start = 0usize;
    while let Some(rel) = haystack[start..].find(needle) {
        let at = start + rel;
        let end = at + needle.len();
        let prev_w = haystack[..at].chars().next_back().is_some_and(is_word_char);
        let next_w = haystack[end..].chars().next().is_some_and(is_word_char);
        if (prev_w != first_w) && (last_w != next_w) {
            return true;
        }
        start = at + haystack[at..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

/// Case-insensitive word-boundary removal equivalent to
/// `re.sub(rf"\b{re.escape(needle)}\b", " ", text, flags=re.I)`.
pub fn strip_word_bounded_ci(text: &str, needle: &str) -> String {
    if needle.is_empty() {
        return text.to_owned();
    }
    // Use regex with escaped literal for Unicode case-insensitive `\b` fidelity.
    let pattern = format!(r"(?i)\b{}\b", regex::escape(needle));
    match regex::Regex::new(&pattern) {
        Ok(re) => re.replace_all(text, " ").into_owned(),
        Err(_) => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_ratios_match_imf_expectations() {
        assert_eq!(token_set_ratio("real gdp growth", "Real GDP growth"), 100.0);
        assert_eq!(
            token_sort_ratio("real gdp growth", "Real GDP growth"),
            100.0
        );
        assert_eq!(
            token_set_ratio("real gdp growth", "Real Per Capita GDP Growth"),
            100.0
        );
        assert!(token_sort_ratio("real gdp growth", "Real Per Capita GDP Growth") < 100.0);
        assert!(token_set_ratio("zzzzqqq", "Real GDP growth") < 60.0);
    }

    #[test]
    fn word_boundaries_block_substrings() {
        assert!(!contains_word_bounded("chadwick holdings", "chad"));
        assert!(!contains_word_bounded("romania gdp", "oman"));
        assert!(contains_word_bounded("china inflation", "china"));
        assert_eq!(
            strip_word_bounded_ci("Vietnam real gdp growth", "vietnam")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
            "real gdp growth"
        );
    }
}
