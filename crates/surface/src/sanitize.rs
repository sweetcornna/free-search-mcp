//! Untrusted-text sanitization, anti-spoofing delimiter escaping, date/age crumbs,
//! FTS5 syntax hints, and hard token-budget capping.

use fsm_core::text::{clip_chars, estimate_tokens, truncate_to_tokens};
use regex::Regex;
use std::sync::LazyLock;

static NONCED_DELIMITER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(--- (?:console|TRUNCATED) |Full result (?:not )?saved )\[([0-9A-Za-z]{4})\]")
        .expect("nonced delimiter regex")
});

static BARE_TRUNCATION_NOTICE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"Full result saved:\s*cache\.get\("exec:"#).expect("bare notice regex")
});

/// Neutralize host-delimiter patterns in untrusted text (`SPEC.md` §2.5.2, I14.3).
///
/// Replaces `[xxxx]` on `--- console [xxxx] ---`, `--- TRUNCATED [xxxx] ---`, and
/// `Full result (not )?saved [xxxx]` with fullwidth brackets `［xxxx］` (preserving valid
/// JSON when applied to JSON strings), and breaks bare `Full result saved: cache.get("exec:`
/// spoofs.
pub fn escape_delimiters(text: &str) -> String {
    let first = NONCED_DELIMITER.replace_all(text, "${1}［${2}］");
    BARE_TRUNCATION_NOTICE
        .replace_all(&first, "Full result saved：cache.get(\"exec:")
        .into_owned()
}

fn is_dangerous_bidi(c: char) -> bool {
    matches!(
        c,
        '\u{202A}'..='\u{202E}' // LRE, RLE, PDF, LRO, RLO
            | '\u{2066}'..='\u{2069}' // LRI, RLI, FSI, PDI
            | '\u{061C}' // ALM
    )
}

/// Sanitize untrusted single-line text (titles, authors, venues, dates, hostnames):
/// strips control & bidi-override characters, collapses whitespace/newlines to a single space,
/// and neutralizes host delimiters.
pub(crate) fn sanitize_inline(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_space = true;
    for ch in text.chars() {
        if is_dangerous_bidi(ch) || ch == '\0' {
            continue;
        }
        if ch.is_whitespace() || ch.is_control() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    escape_delimiters(&out)
}

/// Sanitize untrusted multi-line content (page bodies, snippets, abstracts, sections):
/// normalizes line endings, strips NUL/C0/C1 controls (keeping `\n` and `\t`) and
/// bidi-override characters, preserves CJK and natural RTL scripts, and neutralizes
/// host delimiters.
pub(crate) fn sanitize_block(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
            continue;
        }
        if ch == '\n' || ch == '\t' {
            out.push(ch);
            continue;
        }
        if ch.is_control() || is_dangerous_bidi(ch) {
            continue;
        }
        out.push(ch);
    }
    escape_delimiters(out.trim_end())
}

/// Sanitize untrusted single-line text and cap to `max_chars` (with `…` if cut) so
/// pathological titles/authors/venues cannot crowd out URLs, dates, or IDs.
pub(crate) fn sanitize_inline_capped(text: &str, max_chars: usize) -> String {
    let clean = sanitize_inline(text);
    if clean.chars().count() <= max_chars {
        clean
    } else {
        format!("{}…", clip_chars(&clean, max_chars).trim_end())
    }
}

/// Sanitize a URL for `<url>` Markdown autolink display without panicking.
pub(crate) fn sanitize_url(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let cleaned: String = trimmed
        .chars()
        .filter(|&c| !c.is_control() && !c.is_whitespace() && !is_dangerous_bidi(c))
        .map(|c| match c {
            '<' => ' ',
            '>' => ' ',
            _ => c,
        })
        .collect();
    let safe = escape_delimiters(&cleaned.replace(' ', ""));
    if safe.chars().count() > 1024 {
        format!("{}…", clip_chars(&safe, 1024))
    } else {
        safe
    }
}

/// Hard token cap: guarantees `estimate_tokens(&result) <= max_tokens` for every
/// `max_tokens` (including tiny values `< 8` where `truncate_to_tokens` markers alone
/// would exceed the budget).
pub(crate) fn cap_to_tokens(text: &str, max_tokens: usize) -> String {
    if max_tokens == 0 {
        return String::new();
    }
    if estimate_tokens(text) <= max_tokens {
        return text.to_owned();
    }
    let cut = truncate_to_tokens(text, max_tokens);
    if cut.tokens <= max_tokens {
        return cut.text;
    }
    // Tiny budget fallback (< 8 tokens): fit the longest char prefix + "…" (cost 1 token).
    let marker = "…";
    let marker_tokens = estimate_tokens(marker);
    if max_tokens <= marker_tokens {
        return marker.to_owned();
    }
    let total = text.chars().count();
    let (mut lo, mut hi) = (0usize, total);
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let candidate = format!("{}{marker}", clip_chars(text, mid));
        if estimate_tokens(&candidate) <= max_tokens {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    format!("{}{marker}", clip_chars(text, lo).trim_end())
}

/// Human-friendly coarse cache age phrase (`formatting.py::_age_phrase`).
pub(crate) fn age_phrase(seconds: u64) -> String {
    if seconds < 60 {
        return "under a minute".to_owned();
    }
    for (unit, size) in [("day", 86_400u64), ("hour", 3_600), ("minute", 60)] {
        if seconds >= size {
            let n = seconds / size;
            let s = if n == 1 { "" } else { "s" };
            return format!("{n} {unit}{s}");
        }
    }
    "under a minute".to_owned()
}

/// Format retrieval/cache provenance (`formatting.py::_retrieved_crumb`).
pub(crate) fn retrieved_crumb(retrieved_at: Option<&str>, cache_age_s: Option<u64>) -> String {
    let stamp = retrieved_at.map(sanitize_inline).filter(|s| !s.is_empty());
    match (cache_age_s, stamp) {
        (Some(age), Some(ts)) => format!("cached {} ago · retrieved {ts}", age_phrase(age)),
        (Some(age), None) => format!("cached {} ago", age_phrase(age)),
        (None, Some(ts)) => format!("retrieved {ts}"),
        (None, None) => String::new(),
    }
}

/// Format hit date provenance (`formatting.py::_date_crumb`).
pub(crate) fn date_crumb(date: Option<&str>, date_source: Option<&str>) -> String {
    let d = date.map(sanitize_inline).filter(|s| !s.is_empty());
    let src = date_source.map(str::trim);
    match d {
        None => {
            if src == Some("none") {
                "undated".to_owned()
            } else {
                String::new()
            }
        }
        Some(d) => match src {
            Some("snippet") => format!("{d} (from snippet text)"),
            Some("structured" | "api") => format!("dated {d}"),
            _ => d,
        },
    }
}

/// Format a millisecond elapsed duration like `1.2s` or `45ms`.
pub(crate) fn elapsed_crumb(elapsed_ms: u64) -> String {
    if elapsed_ms < 1000 {
        format!("{elapsed_ms}ms")
    } else {
        format!("{:.2}s", (elapsed_ms as f64) / 1000.0)
    }
}

/// Heuristic FTS5 syntax check (`server.py::_invalid_fts_hint`).
///
/// Returns an actionable hint when `query` has unbalanced quotes/parentheses or
/// misplaced boolean operators (`AND`/`OR`/`NOT`/`NEAR`), else `None`.
pub fn invalid_fts_hint(query: &str) -> Option<&'static str> {
    let q = query.trim();
    if q.is_empty() {
        return None;
    }
    if q.chars().filter(|&c| c == '"').count() % 2 == 1 {
        return Some(
            "Your query has an unterminated quote. FTS5 phrases need matching double quotes, e.g. `\"exact phrase\"`.",
        );
    }
    let open = q.chars().filter(|&c| c == '(').count();
    let close = q.chars().filter(|&c| c == ')').count();
    if open != close {
        return Some(
            "Your query has unbalanced parentheses. Group sub-expressions like `(a OR b) c`.",
        );
    }
    let ops = ["AND", "OR", "NOT", "NEAR"];
    let tokens: Vec<String> = q
        .split_whitespace()
        .map(|t| t.to_ascii_uppercase())
        .collect();
    if let (Some(first), Some(last)) = (tokens.first(), tokens.last())
        && (ops.contains(&first.as_str()) || ops.contains(&last.as_str()))
    {
        return Some(
            "Your query starts or ends with a boolean operator (AND/OR/NOT/NEAR). These join two terms, e.g. `cats AND dogs`, not `cats AND`.",
        );
    }
    for pair in tokens.windows(2) {
        if ops.contains(&pair[0].as_str()) && ops.contains(&pair[1].as_str()) {
            return Some(
                "Your query has two boolean operators in a row. Put a term between them, e.g. `a AND b OR c`.",
            );
        }
    }
    None
}
