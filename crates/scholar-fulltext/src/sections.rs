//! Structured document representation, academic section matching, text decoding,
//! paywall/stub detection, canonical SPDX license normalization, and `max_chars` budgeting.

use chardetng::{EncodingDetector, Iso2022JpDetection, Utf8Detection};
use fsm_core::text::smart_truncate;
use fsm_core::{FulltextOpts, FulltextSection, HttpBytes};
use regex::Regex;
use std::collections::HashSet;
use std::sync::LazyLock;

/// A structured scholarly section parsed from JATS XML, BioC JSON, HTML5, or text.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedSection {
    pub heading: String,
    pub text: String,
    /// Nested subsections kept for fine-grained `opts.sections` queries when
    /// no top-level heading matches.
    pub subsections: Vec<FulltextSection>,
}

/// Output of the JATS, BioC, HTML5, or plain-text parsers prior to section filtering.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedDocument {
    pub title: Option<String>,
    pub license: Option<String>,
    pub sections: Vec<ParsedSection>,
}

impl ParsedDocument {
    /// True if no section contains meaningful non-whitespace text.
    pub fn is_empty(&self) -> bool {
        self.sections.iter().all(|s| s.text.trim().is_empty())
    }

    /// Total character length across all section bodies.
    pub fn total_text_chars(&self) -> usize {
        self.sections.iter().map(|s| s.text.chars().count()).sum()
    }
}

static LEADING_NUM_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:(?:section|sec\.?|chapter|ch\.?)\s+)?(?:[ivxlcdm]+\.|[0-9]+(?:\.[0-9]+)*\.?|[a-z]\))\s*",
    )
    .expect("leading num regex")
});

/// Matches Markdown headings or canonical academic section headings in any case
/// (including ALL-CAPS PDF headings like `ABSTRACT`, `1 INTRODUCTION`, `2 METHODS` — M2).
static HEADING_LINE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:#{1,4}\s+(.+)|(?i)(?:(?:\d+(?:\.\d+)*|[IVX]+)\.?\s+)?(Abstract|Summary|Introduction|Background|Related Work|Methods?|Methodology|Materials and Methods|Materials & Methods|Experimental(?: Setup| Procedures)?|Results?(?: and Discussion)?|Experiments?|Evaluation|Discussion|Analysis|Limitations|Conclusions?|Concluding Remarks|Future Work|Acknowledgments?|Acknowledgements?|References|Bibliography|Appendix(?:\s+[A-Z0-9]+)?)\s*:?\s*)$",
    )
    .expect("heading line regex")
});

static CC_VERSION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b([1-4]\.0)\b").expect("cc version regex"));

/// Canonical academic section synonym groups for forgiving LLM section queries.
const SECTION_GROUPS: &[&[&str]] = &[
    &["abstract", "summary", "author summary", "synopsis"],
    &["introduction", "intro", "background", "overview"],
    &["related work", "prior work", "literature review"],
    &[
        "methods",
        "method",
        "methodology",
        "materials and methods",
        "materials & methods",
        "experimental",
        "experimental setup",
        "experimental procedures",
        "experimental design",
        "online methods",
        "star methods",
        "approach",
        "model",
        "architecture",
        "model architecture",
        "study design",
        "procedures",
    ],
    &[
        "results",
        "result",
        "findings",
        "experiments",
        "experimental results",
        "evaluation",
    ],
    &["discussion", "analysis", "limitations"],
    &[
        "conclusion",
        "conclusions",
        "concluding remarks",
        "summary and conclusions",
        "future work",
    ],
    &[
        "acknowledgments",
        "acknowledgements",
        "acknowledgment",
        "acknowledgement",
        "funding",
    ],
    &["references", "bibliography", "works cited"],
    &[
        "appendix",
        "appendices",
        "supplementary",
        "supplementary material",
        "supplementary information",
        "supporting information",
    ],
];

/// Canonical body section groups used to distinguish real full-text HTML articles
/// from repository abstract/handle or paywalled landing pages (I3).
const BODY_HEADING_QUERIES: &[&str] = &[
    "introduction",
    "related work",
    "methods",
    "results",
    "discussion",
    "conclusion",
    "appendix",
];

/// Normalize a section heading or query token for matching.
pub fn normalize_heading(s: &str) -> String {
    let lower = s.trim().to_ascii_lowercase().replace('&', " and ");
    let stripped = LEADING_NUM_RE.replace(&lower, "");
    let cleaned: String = stripped
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c.is_whitespace() {
                c
            } else {
                ' '
            }
        })
        .collect();
    collapse_ws(&cleaned)
}

/// True if `heading` matches a requested section query `wanted`.
pub fn heading_matches(heading: &str, wanted: &str) -> bool {
    let nh = normalize_heading(heading);
    let nw = normalize_heading(wanted);
    if nh.is_empty() || nw.is_empty() {
        return false;
    }
    if nh == nw || nh.contains(&nw) || (nh.len() >= 4 && nw.contains(&nh)) {
        return true;
    }
    for group in SECTION_GROUPS {
        let wanted_in_group = group.iter().any(|&g| nw == g || nw.contains(g));
        if wanted_in_group {
            let heading_in_group = group
                .iter()
                .any(|&g| nh == g || nh.contains(g) || (nh.len() >= 4 && g.contains(nh.as_str())));
            if heading_in_group {
                return true;
            }
        }
    }
    false
}

/// True when a parsed HTML document looks like a genuine full-text article rather
/// than an abstract-only repository handle page or paywalled landing page (I3).
pub fn has_fulltext_article_structure(doc: &ParsedDocument) -> bool {
    let mut canonical_body_hits = 0usize;
    let mut abstract_chars = 0usize;
    let mut body_chars = 0usize;

    for sec in &doc.sections {
        let len = sec.text.trim().chars().count();
        if len == 0 {
            continue;
        }
        if heading_matches(&sec.heading, "abstract") {
            abstract_chars += len;
            continue;
        }
        if heading_matches(&sec.heading, "references") {
            continue;
        }
        body_chars += len;
        if BODY_HEADING_QUERIES
            .iter()
            .any(|q| heading_matches(&sec.heading, q))
        {
            canonical_body_hits += 1;
        }
    }

    (canonical_body_hits >= 2 && body_chars >= 400)
        || (body_chars >= 6_000 && body_chars >= abstract_chars.saturating_mul(3))
}

/// Apply `opts.sections` filtering and `opts.max_chars` budgeting to a [`ParsedDocument`].
///
/// Jointly budgets `sections` and `content` (I11):
/// - Default `cap` is `default_max_chars` (typically `cfg.max_content_chars`, 50k).
/// - When `opts.sections` is `None` and total document text exceeds `cap / 2`,
///   `content` receives the full `cap` while `sections` is compacted to headings-only
///   outline entries so the serialized payload never duplicates multi-MB bodies.
///
/// Returns `(sections, rendered_markdown_content, matched_any_or_unfiltered)`.
pub fn filter_and_budget(
    doc: &ParsedDocument,
    opts: &FulltextOpts,
    default_max_chars: usize,
) -> (Vec<FulltextSection>, String, bool) {
    let cap = opts.max_chars.unwrap_or(default_max_chars).max(1);
    let explicit_sections = opts
        .sections
        .as_deref()
        .is_some_and(|wanted| !wanted.is_empty());

    let (selected, matched_filter): (Vec<FulltextSection>, bool) = match opts.sections.as_deref() {
        Some(wanted) if !wanted.is_empty() => {
            let mut out = Vec::new();
            let mut seen_top: HashSet<usize> = HashSet::new();

            // Pass 1: top-level section matches in document order.
            for (idx, sec) in doc.sections.iter().enumerate() {
                if wanted.iter().any(|w| heading_matches(&sec.heading, w)) {
                    seen_top.insert(idx);
                    out.push(FulltextSection::new(sec.heading.clone(), sec.text.clone()));
                }
            }

            // Pass 2: for any wanted query that matched NO top-level section, check subsections
            // of unmatched top-level sections so we never duplicate content.
            let unmatched_queries: Vec<&String> = wanted
                .iter()
                .filter(|w| !doc.sections.iter().any(|s| heading_matches(&s.heading, w)))
                .collect();
            if !unmatched_queries.is_empty() {
                for (idx, sec) in doc.sections.iter().enumerate() {
                    if seen_top.contains(&idx) {
                        continue;
                    }
                    for sub in &sec.subsections {
                        if unmatched_queries
                            .iter()
                            .any(|w| heading_matches(&sub.heading, w))
                        {
                            out.push(sub.clone());
                        }
                    }
                }
            }

            let matched = !out.is_empty();
            (out, matched)
        }
        _ => (
            doc.sections
                .iter()
                .map(|s| FulltextSection::new(s.heading.clone(), s.text.clone()))
                .collect(),
            true,
        ),
    };

    if !matched_filter {
        let available: Vec<&str> = doc.sections.iter().map(|s| s.heading.as_str()).collect();
        let wanted_list = opts.sections.as_deref().unwrap_or(&[]);
        let msg = format!(
            "[No sections matched {:?}; available sections: {}]",
            wanted_list,
            if available.is_empty() {
                "(none)".to_owned()
            } else {
                available.join(", ")
            }
        );
        let (clipped, _) = smart_truncate(&msg, cap);
        return (Vec::new(), clipped, false);
    }

    let budgeted_sections = budget_sections(selected, cap);
    let rendered = render_sections_markdown(doc.title.as_deref(), &budgeted_sections);
    let (content, _) = smart_truncate(&rendered, cap);

    // I11: Jointly budget `sections` + `content` when `opts.sections` was not explicitly
    // requested so large papers never double the serialized wire payload.
    let final_sections = if !explicit_sections {
        let sec_chars: usize = budgeted_sections
            .iter()
            .map(|s| s.text.chars().count())
            .sum();
        if sec_chars > cap / 2 {
            doc.sections
                .iter()
                .map(|s| FulltextSection::new(s.heading.clone(), String::new()))
                .collect()
        } else {
            budgeted_sections
        }
    } else {
        budgeted_sections
    };

    (final_sections, content, true)
}

fn budget_sections(sections: Vec<FulltextSection>, max_chars: usize) -> Vec<FulltextSection> {
    let mut remaining = max_chars;
    let mut out = Vec::with_capacity(sections.len());
    for sec in sections {
        if remaining == 0 {
            break;
        }
        let heading_overhead = sec.heading.chars().count() + 6; // "## " + "\n\n"
        let text_len = sec.text.chars().count();
        if text_len + heading_overhead <= remaining {
            remaining = remaining.saturating_sub(text_len + heading_overhead);
            out.push(sec);
        } else {
            let allow_text = remaining
                .saturating_sub(heading_overhead)
                .max(32.min(remaining));
            if allow_text > 0 {
                let (clipped, _) = smart_truncate(&sec.text, allow_text);
                out.push(FulltextSection::new(sec.heading, clipped));
            }
            break;
        }
    }
    out
}

/// Render structured sections as clean Markdown.
pub fn render_sections_markdown(title: Option<&str>, sections: &[FulltextSection]) -> String {
    let mut parts = Vec::with_capacity(sections.len() + 1);
    if let Some(t) = title.map(str::trim).filter(|s| !s.is_empty()) {
        parts.push(format!("# {t}"));
    }
    for sec in sections {
        let heading = sec.heading.trim();
        let body = sec.text.trim();
        if heading.is_empty() {
            if !body.is_empty() {
                parts.push(body.to_owned());
            }
        } else if body.is_empty() {
            parts.push(format!("## {heading}"));
        } else {
            parts.push(format!("## {heading}\n\n{body}"));
        }
    }
    parts.join("\n\n")
}

/// Heuristically split plain text (CORE v3 `fullText` or PDF extracted text) into sections.
pub fn extract_text_sections(
    text: &str,
    title: Option<String>,
    license: Option<String>,
) -> ParsedDocument {
    let mut sections: Vec<ParsedSection> = Vec::new();
    let mut current_heading = String::from("Body");
    let mut current_lines: Vec<&str> = Vec::new();
    let mut found_heading = false;

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.len() <= 120
            && let Some(caps) = HEADING_LINE_RE.captures(line)
        {
            let heading = caps
                .get(1)
                .or_else(|| caps.get(2))
                .map(|m| m.as_str().trim().trim_end_matches(':').to_owned())
                .unwrap_or_default();
            if !heading.is_empty() {
                let body = current_lines.join("\n").trim().to_owned();
                if !body.is_empty() {
                    sections.push(ParsedSection {
                        heading: current_heading,
                        text: body,
                        subsections: Vec::new(),
                    });
                }
                current_heading = heading;
                current_lines.clear();
                found_heading = true;
                continue;
            }
        }
        current_lines.push(raw_line);
    }

    let tail = current_lines.join("\n").trim().to_owned();
    if !tail.is_empty() {
        sections.push(ParsedSection {
            heading: current_heading,
            text: tail,
            subsections: Vec::new(),
        });
    }

    if !found_heading && sections.is_empty() {
        let cleaned = text.trim();
        if !cleaned.is_empty() {
            sections.push(ParsedSection {
                heading: "Full Text".to_owned(),
                text: cleaned.to_owned(),
                subsections: Vec::new(),
            });
        }
    }

    ParsedDocument {
        title,
        license,
        sections,
    }
}

/// Collapse runs of Unicode whitespace into a single ASCII space and trim.
pub fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !in_ws && !out.is_empty() {
                out.push(' ');
                in_ws = true;
            }
        } else {
            out.push(c);
            in_ws = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Decode HTTP response bytes honoring `Content-Type` `charset=`, UTF-8 fast-path,
/// and `chardetng` + `encoding_rs` fallback.
pub fn decode_response_text(resp: &HttpBytes) -> String {
    let bytes = resp.body.as_ref();
    if bytes.is_empty() {
        return String::new();
    }
    if let Some(ct) = resp.header("content-type") {
        for part in ct.split(';').skip(1) {
            let trimmed = part.trim();
            if let Some((k, v)) = trimmed.split_once('=')
                && k.trim().eq_ignore_ascii_case("charset")
            {
                let label = v.trim().trim_matches('"').trim_matches('\'');
                if let Some(enc) = encoding_rs::Encoding::for_label(label.as_bytes()) {
                    let (decoded, _, _) = enc.decode(bytes);
                    return decoded.into_owned();
                }
            }
        }
    }
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.strip_prefix('\u{feff}').unwrap_or(s).to_owned();
    }
    let mut detector = EncodingDetector::new(Iso2022JpDetection::Allow);
    detector.feed(bytes, true);
    let enc = detector.guess(None, Utf8Detection::Allow);
    let (decoded, _, _) = enc.decode(bytes);
    decoded.into_owned()
}

/// Detect known API placeholder strings or publisher paywall/bot-wall HTML stubs.
/// Strong paywall phrases are rejected at **any** page length (I3).
pub fn looks_like_paywall_or_stub(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();

    // Strong paywall markers apply regardless of page length (e.g. a 6k-char landing page
    // with abstract + 59 references + "Buy this article" / "Access through your institution").
    const STRONG_PAYWALL_MARKERS: &[&str] = &[
        "access through your institution",
        "buy this article",
        "purchase pdf",
        "rent this article",
        "sign in to access full text",
        "subscribe to access",
        "you do not have access to this content",
        "get access to the full version",
        "log in via an institution",
        "purchase instant access",
    ];
    if STRONG_PAYWALL_MARKERS.iter().any(|m| lower.contains(m)) {
        return true;
    }

    let char_len = trimmed.chars().count();
    if char_len < 160 {
        const SHORT_STUB_MARKERS: &[&str] = &[
            "not available for public api",
            "full text not available",
            "access denied",
            "subscribe to read",
        ];
        if SHORT_STUB_MARKERS.iter().any(|m| lower.contains(m)) {
            return true;
        }
    }
    if char_len < 2_500 {
        const CHALLENGE_MARKERS: &[&str] = &[
            "just a moment...",
            "enable javascript and cookies to continue",
            "institutional access",
            "html is not available for the source",
        ];
        if CHALLENGE_MARKERS.iter().any(|m| lower.contains(m)) {
            return true;
        }
    }
    false
}

/// Normalize a Creative Commons or standard license URL/prose/label to a canonical
/// SPDX-style identifier (`CC-BY-4.0`, `CC-BY`, `CC-BY-NC-ND-4.0`, `CC0-1.0`, etc. — M8).
pub fn normalize_license(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();

    // 1. Creative Commons URLs
    if lower.contains("creativecommons.org/publicdomain/zero/") {
        return Some("CC0-1.0".to_owned());
    }
    if let Some(rest) = lower.split("creativecommons.org/licenses/").nth(1) {
        let parts: Vec<&str> = rest
            .split('/')
            .filter(|p| !p.is_empty() && !p.starts_with("legalcode") && !p.starts_with("deed"))
            .collect();
        if let Some(&variant) = parts.first() {
            let v_upper = variant.to_ascii_uppercase();
            if let Some(&ver) = parts.get(1)
                && ver.chars().next().is_some_and(|c| c.is_ascii_digit())
            {
                return Some(format!("CC-{v_upper}-{ver}"));
            }
            return Some(format!("CC-{v_upper}"));
        }
    }

    // 2. CC0 / Public Domain
    if lower == "cc0"
        || lower == "cc0-1.0"
        || lower == "cc0 1.0"
        || lower == "cc_0"
        || lower.contains("creative commons zero")
        || lower.contains("public domain dedication")
    {
        return Some("CC0-1.0".to_owned());
    }

    // 3. Creative Commons prose ("Creative Commons Attribution-NonCommercial 4.0 License", etc.)
    if lower.contains("creative commons") {
        let words: Vec<&str> = lower
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|w| !w.is_empty())
            .collect();
        let has_word = |w: &str| words.contains(&w);
        let mut quals = vec!["BY"];
        if lower.contains("noncommercial") || lower.contains("non-commercial") || has_word("nc") {
            quals.push("NC");
        }
        if lower.contains("noderiv") || lower.contains("no-deriv") || has_word("nd") {
            quals.push("ND");
        } else if lower.contains("sharealike") || lower.contains("share-alike") || has_word("sa") {
            quals.push("SA");
        }
        let base = format!("CC-{}", quals.join("-"));
        if let Some(ver) = CC_VERSION_RE.captures(&lower).and_then(|c| c.get(1)) {
            return Some(format!("{base}-{}", ver.as_str()));
        }
        return Some(base);
    }

    // 4. Compact CC tokens (`CC BY`, `cc-by-4.0`, `cc_by_nc_nd`, `CC BY-NC-ND 4.0`, `cc-by-sa-3.0`)
    let compact = lower.replace(['_', ' '], "-");
    let compact = compact
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if compact == "cc-by" || compact.starts_with("cc-by-") {
        let parts: Vec<&str> = compact.split('-').collect();
        let mut out = vec!["CC".to_owned(), "BY".to_owned()];
        for &p in parts.iter().skip(2) {
            if matches!(p, "nc" | "nd" | "sa") {
                out.push(p.to_ascii_uppercase());
            } else if CC_VERSION_RE.is_match(p) {
                out.push(p.to_owned());
            }
        }
        return Some(out.join("-"));
    }

    // 5. Common non-CC open licenses
    if lower == "mit" {
        return Some("MIT".to_owned());
    }
    if lower == "apache-2.0" || lower == "apache 2.0" {
        return Some("Apache-2.0".to_owned());
    }

    Some(trimmed.to_owned())
}
