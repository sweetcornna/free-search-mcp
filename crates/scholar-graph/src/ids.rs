//! Identifier classification, normalization, and title-similarity gating.
//!
//! Preserves Python `paper_graph.py` classification and title-gating semantics
//! while adding DOI, arXiv, PMID, PMCID, OpenAlex, ORCID, ROR, and ISSN helpers
//! needed across `graph`, `integrity`, `cite`, `rw_sync`, and `entity`.

use regex::Regex;
use std::sync::LazyLock;

/// Minimum `title_score` (0..=100) for a free-text title lookup to clear the gate.
pub const TITLE_MATCH_MIN: f64 = 85.0;

static DOI_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(10\.\d{4,9}/\S+)").expect("regex"));
static DOI_EXACT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^10\.\d{4,9}/\S+$").expect("regex"));
static OPENALEX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(W\d{4,12})\b").expect("regex"));
static OPENALEX_FULL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(W\d{4,12})$").expect("regex"));
static OPENALEX_SHORT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(W\d+)$").expect("regex"));
static ARXIV_DOI_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^10\.48550/arxiv\.([a-z-]+(?:\.[a-z]{2})?/\d{7}|\d{4}\.\d{4,5})(?:v\d+)?$")
        .expect("regex")
});
static ARXIV_REF_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:arxiv\.org/(?:abs|pdf|html)/|\barxiv:\s*)([a-z-]+(?:\.[a-z]{2})?/\d{7}|\d{4}\.\d{4,5})(?:v\d+)?",
    )
    .expect("regex")
});
static ARXIV_BARE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^([a-z-]+(?:\.[a-z]{2})?/\d{7}|\d{4}\.\d{4,5})(?:v\d+)?$").expect("regex")
});
static PMCID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:pmcid:\s*)?(PMC\d{4,12})$").expect("regex"));
static PMID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:pmid:\s*|https?://pubmed\.ncbi\.nlm\.nih\.gov/)(\d{4,12})/?$")
        .expect("regex")
});
static ORCID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:https?://orcid\.org/|\borcid:\s*)?(\d{4}-\d{4}-\d{4}-\d{3}[\dX])\b")
        .expect("regex")
});
static ROR_URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)https?://ror\.org/(0[a-z0-9]{6}\d{2})\b").expect("regex"));
static ROR_BARE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(0[a-z0-9]{6}\d{2})$").expect("regex"));
static ISSN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:issn:\s*)?(\d{4})-?(\d{3}[\dX])$").expect("regex"));

/// Classification of a paper query passed to `graph`, `integrity`, or `cite`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaperInput {
    Doi(String),
    OpenAlex(String),
    Arxiv(String),
    Pmid(String),
    Pmcid(String),
    Title(String),
}

impl PaperInput {
    /// Wire tag for `CitationGraph.resolved_as`.
    pub fn kind_str(&self) -> &'static str {
        match self {
            Self::Doi(_) => "doi",
            Self::OpenAlex(_) => "openalex",
            Self::Arxiv(_) => "arxiv",
            Self::Pmid(_) => "pmid",
            Self::Pmcid(_) => "pmcid",
            Self::Title(_) => "title",
        }
    }
}

/// Strip trailing prose punctuation from a candidate DOI while preserving
/// balanced parentheses inside DOIs such as `10.1016/s0140-6736(20)31180-6`.
fn trim_doi_punctuation(raw: &str) -> &str {
    let mut s = raw.trim();
    loop {
        if let Some(stripped) = s.strip_suffix(['.', ',', ';', ']', '}', '>', '"', '\'']) {
            s = stripped;
            continue;
        }
        if let Some(stripped) = s.strip_suffix(')') {
            let opens = s.bytes().filter(|&b| b == b'(').count();
            let closes = s.bytes().filter(|&b| b == b')').count();
            if closes > opens {
                s = stripped;
                continue;
            }
        }
        break;
    }
    s
}

/// Normalize a DOI string (strip `https://doi.org/`, `doi:`, trailing punctuation,
/// ASCII-lowercase, validate `^10\.\d{4,9}/\S+$`).
pub fn normalize_doi(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let lower_head = s.to_ascii_lowercase();
    let stripped = if let Some(idx) = lower_head.find("doi.org/") {
        &s[idx + "doi.org/".len()..]
    } else if lower_head.starts_with("doi:") {
        s[4..].trim_start()
    } else {
        s
    };
    let candidate = trim_doi_punctuation(stripped);
    if DOI_EXACT_RE.is_match(candidate) {
        Some(candidate.to_ascii_lowercase())
    } else {
        None
    }
}

/// Extract a bare normalized DOI from either a bare DOI, a `doi.org` URL, or prose.
pub fn extract_doi(raw: &str) -> Option<String> {
    if let Some(d) = normalize_doi(raw) {
        return Some(d);
    }
    let caps = DOI_RE.captures(raw)?;
    let cleaned = trim_doi_punctuation(&caps[1]);
    if DOI_EXACT_RE.is_match(cleaned) {
        Some(cleaned.to_ascii_lowercase())
    } else {
        None
    }
}

/// Short OpenAlex ID (`https://openalex.org/W2148972377` -> `W2148972377`).
pub fn short_openalex_id(raw: &str) -> Option<String> {
    let tail = raw.trim().rsplit('/').next()?.trim();
    OPENALEX_SHORT_RE
        .captures(tail)
        .map(|c| c[1].to_ascii_uppercase())
}

/// Classify a paper identifier or title, matching Python `paper_graph._normalize`
/// with added PMID/PMCID support.
pub fn classify_paper_input(raw: &str) -> PaperInput {
    let text = raw.trim();
    if text.is_empty() {
        return PaperInput::Title(String::new());
    }
    if text.to_ascii_lowercase().contains("openalex.org")
        && let Some(caps) = OPENALEX_RE.captures(text)
    {
        return PaperInput::OpenAlex(caps[1].to_ascii_uppercase());
    }
    if let Some(caps) = ARXIV_REF_RE
        .captures(text)
        .or_else(|| ARXIV_BARE_RE.captures(text))
    {
        return PaperInput::Arxiv(caps[1].to_ascii_lowercase());
    }
    if let Some(caps) = DOI_RE.captures(text) {
        let doi = trim_doi_punctuation(&caps[1]);
        if let Some(arxiv) = ARXIV_DOI_RE.captures(doi) {
            return PaperInput::Arxiv(arxiv[1].to_ascii_lowercase());
        }
        return PaperInput::Doi(doi.to_ascii_lowercase());
    }
    if let Some(caps) = OPENALEX_FULL_RE.captures(text) {
        return PaperInput::OpenAlex(caps[1].to_ascii_uppercase());
    }
    if let Some(caps) = PMCID_RE.captures(text) {
        return PaperInput::Pmcid(caps[1].to_ascii_uppercase());
    }
    if let Some(caps) = PMID_RE.captures(text) {
        return PaperInput::Pmid(caps[1].to_owned());
    }
    PaperInput::Title(text.to_owned())
}

/// Extract an ORCID iD (`0000-0002-1825-0097`) when the input is an ORCID URL,
/// `orcid:` prefixed string, or bare ORCID iD.
pub fn extract_orcid(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let caps = ORCID_RE.captures(trimmed)?;
    let full = caps.get(0)?;
    if full.start() == 0 && full.end() == trimmed.len() {
        Some(caps[1].to_ascii_uppercase())
    } else {
        None
    }
}

/// Extract a 9-character ROR identifier (`03cve4549`) from a `ror.org` URL or bare ID.
pub fn extract_ror_id(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if let Some(caps) = ROR_URL_RE.captures(trimmed) {
        return Some(caps[1].to_ascii_lowercase());
    }
    ROR_BARE_RE
        .captures(trimmed)
        .map(|c| c[1].to_ascii_lowercase())
}

/// Extract a canonical hyphenated ISSN (`0028-0836`) when input is an ISSN.
pub fn extract_issn(raw: &str) -> Option<String> {
    let caps = ISSN_RE.captures(raw.trim())?;
    Some(format!("{}-{}", &caps[1], caps[2].to_ascii_uppercase()))
}

/// Equivalent of `rapidfuzz.utils.default_process`: replace non-alphanumeric
/// characters with spaces, lowercase, collapse runs of whitespace, and trim.
pub fn default_process(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut prev_space = true;
    for ch in input.chars() {
        if ch.is_alphanumeric() {
            for lc in ch.to_lowercase() {
                out.push(lc);
            }
            prev_space = false;
        } else if !prev_space {
            out.push(' ');
            prev_space = true;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Maximum Unicode scalar values processed in fuzzy similarity DP (I5 DoS bound:
/// caps `indel_ratio` to <= 262k iterations on untrusted strings while preserving
/// exact scores for all real scholarly titles and entity names).
pub const MAX_FUZZY_CHARS: usize = 512;

/// Normalized Indel similarity `200 * LCS(a, b) / (len(a) + len(b))`, matching
/// `rapidfuzz.fuzz.ratio` on Unicode scalar sequences (capped at [`MAX_FUZZY_CHARS`]).
pub fn indel_ratio(a: &str, b: &str) -> f64 {
    let ac: Vec<char> = a.chars().take(MAX_FUZZY_CHARS).collect();
    let bc: Vec<char> = b.chars().take(MAX_FUZZY_CHARS).collect();
    let total = ac.len() + bc.len();
    if total == 0 {
        return 100.0;
    }
    if ac.is_empty() || bc.is_empty() {
        return 0.0;
    }
    let (short, long) = if ac.len() <= bc.len() {
        (&ac, &bc)
    } else {
        (&bc, &ac)
    };
    let mut prev = vec![0usize; short.len() + 1];
    let mut curr = vec![0usize; short.len() + 1];
    for &lc in long {
        curr[0] = 0;
        for (j, &sc) in short.iter().enumerate() {
            curr[j + 1] = if lc == sc {
                prev[j] + 1
            } else {
                prev[j + 1].max(curr[j])
            };
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    let lcs = prev[short.len()];
    (200.0 * lcs as f64) / total as f64
}

/// Equivalent of `rapidfuzz.fuzz.token_sort_ratio` on already-processed strings.
pub fn token_sort_ratio(a: &str, b: &str) -> f64 {
    let mut ta: Vec<&str> = a.split_whitespace().collect();
    let mut tb: Vec<&str> = b.split_whitespace().collect();
    ta.sort_unstable();
    tb.sort_unstable();
    indel_ratio(&ta.join(" "), &tb.join(" "))
}

/// Faithfulness score (0..=100) of `query` against candidate `title`, matching
/// Python `paper_graph._title_score` (max of whole-string `token_sort_ratio`
/// and prefix-truncated `token_sort_ratio`, bounded to [`MAX_FUZZY_CHARS`]).
pub fn title_score(query: &str, title: &str) -> f64 {
    let nq: String = default_process(fsm_core::text::clip_chars(query, MAX_FUZZY_CHARS * 2))
        .chars()
        .take(MAX_FUZZY_CHARS)
        .collect();
    let nt: String = default_process(fsm_core::text::clip_chars(title, MAX_FUZZY_CHARS * 2))
        .chars()
        .take(MAX_FUZZY_CHARS)
        .collect();
    if nq.is_empty() || nt.is_empty() {
        return 0.0;
    }
    let whole = token_sort_ratio(&nq, &nt);
    let q_words = nq.split_whitespace().count();
    let prefix_title: String = nt
        .split_whitespace()
        .take(q_words)
        .collect::<Vec<_>>()
        .join(" ");
    let prefix = token_sort_ratio(&nq, &prefix_title);
    whole.max(prefix)
}

/// Severity rank for post-publication notices (lower = more severe).
/// Matches Python `paper_graph._NOTICE_RANK`, extended for Retraction Watch vocabulary.
pub fn notice_rank(kind: &str) -> usize {
    let lower = kind.trim().to_ascii_lowercase().replace([' ', '-'], "_");
    match lower.as_str() {
        "retraction" => 0,
        "withdrawal" | "removal" => 1,
        "expression_of_concern" => 2,
        "correction" | "corrigendum" => 3,
        "erratum" => 4,
        "addendum" => 5,
        "reinstatement" => 6,
        _ if lower.contains("retract") => 0,
        _ if lower.contains("withdraw") || lower.contains("remov") => 1,
        _ if lower.contains("concern") => 2,
        _ if lower.contains("correct") || lower.contains("corrigend") => 3,
        _ => 7,
    }
}

/// True when a notice kind/nature indicates the work is no longer part of the
/// standing scholarly record.
///
/// Deliberate 1.0 policy (M7): unlike Python `paper_graph.py` (which matched only
/// `type == "retraction"`), we also treat `"withdrawal"` and `"removal"` notices
/// (used by Elsevier / COPE / Crossref when withdrawing articles from the record)
/// as retracted, while keeping `"expression_of_concern"`, `"correction"`,
/// `"corrigendum"`, `"erratum"`, `"addendum"`, and `"reinstatement"` non-retracted.
pub fn is_retraction_nature(kind: &str) -> bool {
    let lower = kind.trim().to_ascii_lowercase();
    if lower.contains("reinstat") {
        return false;
    }
    lower.contains("retract") || lower.contains("withdraw") || lower.contains("remov")
}

/// Format Crossref `{"date-parts": [[2020, 6, 5]]}` -> `Some("2020-06-05")`,
/// truncating at the first non-integer gap (`[[2020, null, 5]]` -> `Some("2020")`).
pub fn crossref_date(updated: Option<&serde_json::Value>) -> Option<String> {
    let parts = updated?
        .get("date-parts")?
        .as_array()?
        .first()?
        .as_array()?;
    let mut nums = Vec::with_capacity(3);
    for p in parts.iter().take(3) {
        let Some(n) = p.as_i64() else { break };
        nums.push(n);
    }
    match nums.as_slice() {
        [] => None,
        [y] => Some(format!("{y}")),
        [y, m] => Some(format!("{y}-{m:02}")),
        [y, m, d, ..] => Some(format!("{y}-{m:02}-{d:02}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn doi_normalization_and_balanced_parens() {
        assert_eq!(
            normalize_doi("https://doi.org/10.1016/S0140-6736(20)31180-6").as_deref(),
            Some("10.1016/s0140-6736(20)31180-6")
        );
        assert_eq!(
            extract_doi("(10.1145/1571941.1572114).").as_deref(),
            Some("10.1145/1571941.1572114")
        );
        assert_eq!(normalize_doi("unavailable"), None);
        assert_eq!(normalize_doi("0"), None);
    }

    #[test]
    fn entity_id_extractors() {
        assert_eq!(
            extract_orcid("https://orcid.org/0000-0002-1825-0097").as_deref(),
            Some("0000-0002-1825-0097")
        );
        assert_eq!(
            extract_orcid("0000-0001-2345-678x").as_deref(),
            Some("0000-0001-2345-678X")
        );
        assert_eq!(extract_orcid("Geoffrey Hinton"), None);
        assert_eq!(
            extract_ror_id("https://ror.org/03cve4549").as_deref(),
            Some("03cve4549")
        );
        assert_eq!(extract_ror_id("03cve4549").as_deref(), Some("03cve4549"));
        assert_eq!(extract_ror_id("Tsinghua University"), None);
        assert_eq!(extract_issn("0028-0836").as_deref(), Some("0028-0836"));
        assert_eq!(extract_issn("0028083x").as_deref(), Some("0028-083X"));
        assert_eq!(extract_issn("Nature"), None);
    }

    #[test]
    fn crossref_date_truncates_at_gap() {
        assert_eq!(
            crossref_date(Some(&json!({"date-parts": [[2020, 6, 5]]}))).as_deref(),
            Some("2020-06-05")
        );
        assert_eq!(
            crossref_date(Some(&json!({"date-parts": [[2020, 6]]}))).as_deref(),
            Some("2020-06")
        );
        assert_eq!(
            crossref_date(Some(&json!({"date-parts": [[2020]]}))).as_deref(),
            Some("2020")
        );
        assert_eq!(
            crossref_date(Some(&json!({"date-parts": [[2020, null, 5]]}))).as_deref(),
            Some("2020")
        );
        assert_eq!(crossref_date(Some(&json!({}))), None);
        assert_eq!(crossref_date(None), None);
    }
}
