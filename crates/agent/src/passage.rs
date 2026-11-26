//! CJK-aware sentence-window passage selection using `bm25` + `rapidfuzz` (ruling R5 Deliverable 1).
//!
//! Splits cleaned Markdown pages and search snippets into overlapping 1–3 sentence windows,
//! scores them against the question using Okapi BM25 (with Snowball English stemming + CJK
//! unigrams/bigrams + preserved version/date tokens) blended with `rapidfuzz` similarity and
//! answer-type cues (dates, versions, prices), and deduplicates near-identical passages while
//! enforcing source diversity.

use bm25::{DefaultTokenizer, Document, Language, SearchEngineBuilder, Tokenizer};
use fsm_core::AskPassage;
use fsm_core::url_canon::{host_of, normalize_url};
use rapidfuzz::fuzz;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use unicode_normalization::UnicodeNormalization;

/// Input document (fetched page or search snippet) for passage ranking.
#[derive(Debug, Clone)]
pub struct CandidateDoc {
    pub title: String,
    pub url: String,
    pub date: Option<String>,
    pub source: Option<String>,
    pub cache_age_s: Option<u64>,
    pub rank: usize,
    pub is_full_page: bool,
    pub text: String,
}

const EN_STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "been", "being", "by", "can", "could", "did", "do",
    "does", "for", "from", "had", "has", "have", "how", "i", "in", "into", "is", "it", "its",
    "may", "might", "much", "must", "my", "of", "on", "or", "our", "should", "so", "that", "the",
    "their", "them", "there", "these", "they", "this", "to", "was", "we", "were", "what", "when",
    "where", "which", "who", "whom", "why", "will", "with", "would", "you", "your",
];

const MAX_WINDOW_CHARS: usize = 460;
const MIN_WINDOW_CHARS: usize = 20;
const MAX_PASSAGES_PER_URL: usize = 1;

/// Generic question/cue words excluded when identifying the question's core entity terms.
const ENTITY_IGNORE: &[&str] = &[
    "when",
    "what",
    "which",
    "where",
    "who",
    "whom",
    "whose",
    "why",
    "how",
    "release",
    "released",
    "releases",
    "releas",
    "version",
    "versions",
    "latest",
    "newest",
    "current",
    "stable",
    "date",
    "year",
    "month",
    "day",
    "time",
    "first",
    "initial",
    "initially",
    "originally",
    "default",
    "price",
    "cost",
    "created",
    "creator",
    "author",
    "founded",
    "invented",
    "made",
    "built",
    "crate",
    "crates",
    "package",
    "packages",
    "library",
    "module",
    "framework",
    "language",
    "programming",
    "software",
    "tool",
    "edition",
    "update",
    "updated",
    "support",
    "supported",
    "still",
    "main",
    "major",
    "new",
    "features",
    "highlights",
];

/// CJK-aware tokenizer compatible with `bm25::SearchEngine`.
pub struct PassageTokenizer {
    en_stemmer: DefaultTokenizer,
}

impl PassageTokenizer {
    pub fn new() -> Self {
        let en_stemmer = DefaultTokenizer::builder()
            .language_mode(Language::English)
            .normalization(true)
            .stemming(true)
            .stopwords(false)
            .build();
        Self { en_stemmer }
    }

    fn stem_ascii(&self, word: &str) -> String {
        if word.len() <= 2 || !word.is_ascii() || word.chars().any(|c| c.is_ascii_digit()) {
            return word.to_owned();
        }
        let mut out = self.en_stemmer.tokenize(word);
        if out.len() == 1 && !out[0].is_empty() {
            out.remove(0)
        } else {
            word.to_owned()
        }
    }

    /// Tokenize text into normalized Latin stems, preserved version/numeric tokens, and CJK unigrams + bigrams.
    pub fn tokenize_terms(&self, input: &str, drop_stopwords: bool) -> Vec<String> {
        let norm: String = input.nfkc().collect();
        let mut tokens = Vec::new();
        let mut latin = String::new();
        let mut cjk: Vec<char> = Vec::new();

        let flush_latin = |buf: &mut String, out: &mut Vec<String>| {
            if buf.is_empty() {
                return;
            }
            let lower = buf.to_lowercase();
            // Keep dotted version numbers intact (e.g. "1.85.0", "3.13") AND emit split parts.
            if lower.contains('.') && lower.chars().any(|c| c.is_ascii_digit()) {
                let trimmed = lower.trim_matches('.');
                if !trimmed.is_empty() {
                    out.push(trimmed.to_owned());
                }
            }
            for piece in lower.split(|c: char| !c.is_alphanumeric()) {
                if piece.is_empty() {
                    continue;
                }
                if drop_stopwords && EN_STOPWORDS.contains(&piece) {
                    continue;
                }
                out.push(self.stem_ascii(piece));
            }
            buf.clear();
        };

        let flush_cjk = |buf: &mut Vec<char>, out: &mut Vec<String>| {
            if buf.is_empty() {
                return;
            }
            // Emit CJK bigrams for phrase precision AND unigrams for single-character facts / short queries.
            for &ch in buf.iter() {
                out.push(ch.to_string());
            }
            for w in buf.windows(2) {
                let mut gram = String::with_capacity(6);
                gram.push(w[0]);
                gram.push(w[1]);
                out.push(gram);
            }
            buf.clear();
        };

        for ch in norm.chars() {
            if is_cjk(ch) {
                flush_latin(&mut latin, &mut tokens);
                cjk.push(ch);
            } else if ch.is_alphanumeric() || (ch == '.' && !latin.is_empty()) {
                flush_cjk(&mut cjk, &mut tokens);
                latin.push(ch);
            } else {
                flush_latin(&mut latin, &mut tokens);
                flush_cjk(&mut cjk, &mut tokens);
            }
        }
        flush_latin(&mut latin, &mut tokens);
        flush_cjk(&mut cjk, &mut tokens);
        tokens
    }
}

impl Default for PassageTokenizer {
    fn default() -> Self {
        Self::new()
    }
}

impl Tokenizer for PassageTokenizer {
    fn tokenize(&self, input_text: &str) -> Vec<String> {
        let mut toks = self.tokenize_terms(input_text, true);
        if toks.is_empty() {
            toks = self.tokenize_terms(input_text, false);
        }
        toks
    }
}

fn is_cjk(ch: char) -> bool {
    matches!(
        ch as u32,
        0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xF900..=0xFAFF
            | 0x3040..=0x309F
            | 0x30A0..=0x30FF
            | 0xAC00..=0xD7AF
            | 0x20000..=0x2A6DF
    )
}

#[derive(Debug, Clone)]
struct Window<'a> {
    doc: &'a CandidateDoc,
    canon_url: String,
    host: String,
    text: String,
    lower_text: String,
    lower_chars: Vec<char>,
    tokens: HashSet<String>,
}

/// Extract the question's core entity tokens (both stemmed and raw lowercase forms)
/// plus contiguous 2-token phrases (e.g. `"rust 1.0"`).
fn extract_entity_signals(
    question: &str,
    tok: &PassageTokenizer,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let norm: String = question.nfkc().collect::<String>().to_lowercase();
    let mut raw_words: Vec<String> = Vec::new();
    let mut cur = String::new();
    for ch in norm.chars() {
        if ch.is_alphanumeric() || (ch == '.' && !cur.is_empty()) || (ch == '-' && !cur.is_empty())
        {
            cur.push(ch);
        } else if !cur.is_empty() {
            let trimmed = cur.trim_matches(|c| c == '.' || c == '-').to_owned();
            if !trimmed.is_empty() {
                raw_words.push(trimmed);
            }
            cur.clear();
        }
    }
    if !cur.is_empty() {
        let trimmed = cur.trim_matches(|c| c == '.' || c == '-').to_owned();
        if !trimmed.is_empty() {
            raw_words.push(trimmed);
        }
    }

    let mut entity_raw: Vec<String> = Vec::new();
    let mut entity_stemmed: Vec<String> = Vec::new();
    let mut phrases: Vec<String> = Vec::new();

    let is_entity_word = |w: &str| -> bool {
        if w.is_empty() || EN_STOPWORDS.contains(&w) || ENTITY_IGNORE.contains(&w) {
            return false;
        }
        // Keep dotted versions (`1.0`, `1.85.0`) and alphanumeric entities (>= 2 chars or CJK).
        w.chars().count() >= 2 || w.chars().any(is_cjk)
    };

    for w in &raw_words {
        if is_entity_word(w) {
            if !entity_raw.contains(w) {
                entity_raw.push(w.clone());
            }
            for st in tok.tokenize_terms(w, true) {
                if !ENTITY_IGNORE.contains(&st.as_str()) && !entity_stemmed.contains(&st) {
                    entity_stemmed.push(st);
                }
            }
        }
    }
    for pair in raw_words.windows(2) {
        if is_entity_word(&pair[0]) && is_entity_word(&pair[1]) {
            let phrase = format!("{} {}", pair[0], pair[1]);
            if !phrases.contains(&phrase) {
                phrases.push(phrase);
            }
        }
    }
    (entity_raw, entity_stemmed, phrases)
}

/// Authority prior for official/primary/reference domains vs arbitrary third-party blogs.
/// Uses exact domain-label / hyphen-segment matching so `rust-lang.org` matches entity
/// `"rust"` while `rustly.com` does NOT.
fn domain_authority_bonus(host: &str, entity_raw: &[String]) -> f64 {
    if host.is_empty() {
        return 0.0;
    }
    let mut bonus = 0.0;

    // 1. Check whether any entity word (>= 3 chars, non-numeric) matches a domain segment
    //    (e.g. `blog.rust-lang.org` -> `["blog", "rust", "lang", "org"]` matches `"rust"`).
    let segments: Vec<&str> = host.split(['.', '-']).filter(|s| !s.is_empty()).collect();
    let matches_entity_host = entity_raw.iter().any(|e| {
        e.len() >= 3
            && e.chars().any(|c| c.is_ascii_alphabetic())
            && !e.contains('.')
            && segments.contains(&e.as_str())
    });
    if matches_entity_host {
        bonus += 0.16;
    }

    // 2. Reference, official, standards, academic, and package-registry domains.
    let is_ref_or_official = host == "wikipedia.org"
        || host.ends_with(".wikipedia.org")
        || host.ends_with(".gov")
        || host.ends_with(".edu")
        || host.ends_with(".org")
        || host.starts_with("docs.")
        || host.starts_with("developer.")
        || matches!(
            host,
            "github.com"
                | "crates.io"
                | "pypi.org"
                | "npmjs.com"
                | "www.npmjs.com"
                | "pkg.go.dev"
                | "go.dev"
                | "docs.rs"
                | "tokio.rs"
        );
    if is_ref_or_official {
        bonus += 0.08;
    }

    bonus
}

/// Collect factual anchors (4-digit years for date questions, dotted versions for version
/// questions) that are corroborated across >= 2 distinct hosts (excluding tokens already
/// present in the question itself).
fn collect_corroborated_facts(
    docs: &[CandidateDoc],
    q_lower: &str,
    intent: &QueryIntent,
) -> HashMap<String, usize> {
    static YEAR_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\b((?:19|20)\d{2})\b").expect("year re"));
    static VER_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\b(\d+\.\d+(?:\.\d+)?)\b").expect("ver re"));

    let mut host_sets: HashMap<String, HashSet<String>> = HashMap::new();
    for doc in docs {
        let host = host_of(&doc.url);
        if host.is_empty() {
            continue;
        }
        if intent.wants_date {
            for cap in YEAR_RE.captures_iter(&doc.text) {
                let y = &cap[1];
                if !q_lower.contains(y) {
                    host_sets
                        .entry(y.to_owned())
                        .or_default()
                        .insert(host.clone());
                }
            }
        }
        if intent.wants_version {
            for cap in VER_RE.captures_iter(&doc.text) {
                let v = &cap[1];
                if !q_lower.contains(v) {
                    host_sets
                        .entry(v.to_owned())
                        .or_default()
                        .insert(host.clone());
                }
            }
        }
    }
    host_sets
        .into_iter()
        .filter_map(|(fact, hosts)| (hosts.len() >= 2).then_some((fact, hosts.len())))
        .collect()
}

fn consensus_bonus(text: &str, corroborated: &HashMap<String, usize>) -> f64 {
    if corroborated.is_empty() {
        return 0.0;
    }
    let mut best_hosts = 0usize;
    for (fact, &count) in corroborated {
        if text.contains(fact.as_str()) && count > best_hosts {
            best_hosts = count;
        }
    }
    if best_hosts >= 3 {
        0.14
    } else if best_hosts >= 2 {
        0.10
    } else {
        0.0
    }
}

/// Select the top `max_passages` diverse passages answering `question`.
pub fn select_passages(
    question: &str,
    docs: &[CandidateDoc],
    max_passages: usize,
) -> Vec<AskPassage> {
    if question.trim().is_empty() || docs.is_empty() || max_passages == 0 {
        return Vec::new();
    }

    let tok = PassageTokenizer::new();
    let mut q_tokens = tok.tokenize_terms(question, true);
    if q_tokens.is_empty() {
        q_tokens = tok.tokenize_terms(question, false);
    }
    let q_set: HashSet<String> = q_tokens.iter().cloned().collect();
    let q_norm: String = question.nfkc().collect::<String>().to_lowercase();
    let q_chars: Vec<char> = q_norm.chars().collect();
    let q_intent = QueryIntent::from_question(&q_norm);
    let (entity_raw, entity_stemmed, entity_phrases) = extract_entity_signals(question, &tok);
    let corroborated = collect_corroborated_facts(docs, &q_norm, &q_intent);

    let mut windows: Vec<Window<'_>> = Vec::new();
    for doc in docs {
        let canon = normalize_url(&doc.url);
        let canon_url = if canon.is_empty() {
            doc.url.trim().to_owned()
        } else {
            canon
        };
        let host = host_of(&doc.url);
        let chunks = extract_sentence_windows(&doc.text, doc.is_full_page);
        for text in chunks {
            let norm: String = text.nfkc().collect::<String>().to_lowercase();
            let toks: HashSet<String> = tok.tokenize_terms(&text, true).into_iter().collect();
            let lower_chars = norm.chars().collect();
            windows.push(Window {
                doc,
                canon_url: canon_url.clone(),
                host: host.clone(),
                text,
                lower_text: norm,
                lower_chars,
                tokens: toks,
            });
        }
    }

    if windows.is_empty() {
        return Vec::new();
    }

    // 1. Score all windows with BM25 (using our CJK-aware PassageTokenizer).
    let bm25_docs: Vec<Document<usize>> = windows
        .iter()
        .enumerate()
        .map(|(i, w)| {
            // Include doc title in BM25 text lightly so title-anchored facts match cleanly.
            let combined = if w.doc.title.is_empty() {
                w.text.clone()
            } else {
                format!("{} — {}", w.doc.title, w.text)
            };
            Document::new(i, combined)
        })
        .collect();

    let engine = SearchEngineBuilder::<usize, u32, PassageTokenizer>::with_tokenizer_and_documents(
        PassageTokenizer::new(),
        bm25_docs,
    )
    .k1(1.4)
    .b(0.5)
    .build();

    let mut bm25_scores: HashMap<usize, f64> = HashMap::new();
    let mut max_bm25 = 0.0_f64;
    for res in engine.search(question, windows.len()) {
        let s = f64::from(res.score);
        if s > max_bm25 {
            max_bm25 = s;
        }
        bm25_scores.insert(res.document.id, s);
    }

    // 2. Blend BM25 + rapidfuzz + coverage + entity-mention + phrase + authority + consensus + search-rank prior.
    let mut scored: Vec<(usize, f64)> = Vec::with_capacity(windows.len());
    for (idx, w) in windows.iter().enumerate() {
        let bm25_norm = if max_bm25 > 0.0 {
            bm25_scores.get(&idx).copied().unwrap_or(0.0) / max_bm25
        } else {
            0.0
        };

        let fuzzy = if q_chars.len() >= 2 && !w.lower_chars.is_empty() {
            let full_r = fuzz::ratio(q_chars.iter().copied(), w.lower_chars.iter().copied());
            let ts_r = token_set_ratio(&q_tokens, &w.tokens);
            full_r.max(ts_r)
        } else {
            0.0
        };

        let coverage = if !q_set.is_empty() {
            let matched = q_set.iter().filter(|t| w.tokens.contains(*t)).count();
            (matched as f64) / (q_set.len() as f64)
        } else {
            0.0
        };

        // Prefer passages whose BODY mentions the question's core entity tokens.
        let (entity_bonus, entity_mult) = if entity_stemmed.is_empty() {
            (0.0, 1.0)
        } else {
            let matched = entity_stemmed
                .iter()
                .filter(|t| w.tokens.contains(t.as_str()))
                .count();
            let ratio = (matched as f64) / (entity_stemmed.len() as f64);
            if matched == 0 {
                (0.0, 0.65)
            } else {
                (0.12 * ratio, 1.0)
            }
        };

        // Bonus for exact entity/version phrase match in body (or title+body).
        let phrase_bonus = if entity_phrases.is_empty() {
            0.0
        } else if entity_phrases
            .iter()
            .any(|p| w.lower_text.contains(p.as_str()))
        {
            0.12
        } else {
            let title_lower = w.doc.title.to_lowercase();
            if entity_phrases
                .iter()
                .any(|p| title_lower.contains(p.as_str()))
            {
                0.05
            } else {
                0.0
            }
        };

        let authority_bonus = domain_authority_bonus(&w.host, &entity_raw);
        let fact_consensus = consensus_bonus(&w.text, &corroborated);
        let cue_bonus = q_intent.bonus(&w.text);
        let page_bonus = if w.doc.is_full_page { 0.06 } else { 0.0 };
        let rank_prior = 1.0 / (1.0 + 0.10 * (w.doc.rank as f64));

        let raw = (0.45 * bm25_norm
            + 0.22 * fuzzy
            + 0.18 * coverage
            + entity_bonus
            + phrase_bonus
            + authority_bonus
            + fact_consensus
            + cue_bonus
            + page_bonus)
            * entity_mult;
        let final_score = raw * (0.85 + 0.15 * rank_prior);
        if final_score > 0.01 {
            scored.push((idx, final_score));
        }
    }

    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    // 3. Greedy selection with canonical-URL dedup (max 1 per URL) and strong near-duplicate suppression.
    let mut selected: Vec<AskPassage> = Vec::new();
    let mut selected_meta: Vec<(&str, &Vec<char>, &HashSet<String>)> = Vec::new();
    let mut url_counts: HashMap<&str, usize> = HashMap::new();

    for (idx, score) in scored {
        if selected.len() >= max_passages {
            break;
        }
        let w = &windows[idx];
        let count = url_counts.get(w.canon_url.as_str()).copied().unwrap_or(0);
        if count >= MAX_PASSAGES_PER_URL {
            continue;
        }
        let duplicate = selected_meta
            .iter()
            .any(|(prev_text, prev_chars, prev_toks)| {
                is_near_duplicate(
                    prev_text,
                    prev_chars,
                    prev_toks,
                    &w.lower_text,
                    &w.lower_chars,
                    &w.tokens,
                )
            });
        if duplicate {
            continue;
        }

        *url_counts.entry(w.canon_url.as_str()).or_insert(0) += 1;
        selected_meta.push((&w.lower_text, &w.lower_chars, &w.tokens));

        let mut passage = AskPassage::new(&w.text, &w.doc.title, &w.doc.url);
        passage.date = w.doc.date.clone();
        passage.source = w.doc.source.clone();
        passage.cache_age_s = w.doc.cache_age_s;
        passage.score = (score * 1000.0).round() / 1000.0;
        selected.push(passage);
    }

    selected
}

/// True when two candidate passages are near-duplicates by edit similarity, substring
/// containment, or token-set overlap.
pub(crate) fn is_near_duplicate(
    a_text: &str,
    a_chars: &[char],
    a_toks: &HashSet<String>,
    b_text: &str,
    b_chars: &[char],
    b_toks: &HashSet<String>,
) -> bool {
    if fuzz::ratio(a_chars.iter().copied(), b_chars.iter().copied()) > 0.72 {
        return true;
    }
    let min_chars = a_chars.len().min(b_chars.len());
    if min_chars >= 24 && (a_text.contains(b_text) || b_text.contains(a_text)) {
        return true;
    }
    let min_toks = a_toks.len().min(b_toks.len());
    if min_toks >= 4 {
        let overlap = a_toks.iter().filter(|t| b_toks.contains(*t)).count();
        if (overlap as f64) / (min_toks as f64) >= 0.70 {
            return true;
        }
    }
    false
}

/// Token-set similarity in `[0.0, 1.0]` built on `rapidfuzz::distance::indel::distance`
/// over our CJK-aware normalized token sets.
fn token_set_ratio(q_tokens: &[String], doc_tokens: &HashSet<String>) -> f64 {
    if q_tokens.is_empty() || doc_tokens.is_empty() {
        return 0.0;
    }
    let mut qa: Vec<&str> = q_tokens.iter().map(String::as_str).collect();
    qa.sort_unstable();
    qa.dedup();
    let mut db: Vec<&str> = doc_tokens.iter().map(String::as_str).collect();
    db.sort_unstable();

    let inter: Vec<&str> = qa
        .iter()
        .filter(|t| doc_tokens.contains(**t))
        .copied()
        .collect();
    if inter.len() == qa.len() {
        return 1.0;
    }
    let diff_q: Vec<&str> = qa
        .iter()
        .filter(|t| !doc_tokens.contains(**t))
        .copied()
        .collect();
    let diff_d: Vec<&str> = db
        .iter()
        .filter(|t| !qa.contains(t))
        .take(32)
        .copied()
        .collect();
    let sect = inter.join(" ");
    let s1 = if sect.is_empty() {
        diff_q.join(" ")
    } else {
        format!("{sect} {}", diff_q.join(" "))
    };
    let s2 = if sect.is_empty() {
        diff_d.join(" ")
    } else {
        format!("{sect} {}", diff_d.join(" "))
    };
    let norm = |a: &str, b: &str| -> f64 {
        let len_sum = a.chars().count() + b.chars().count();
        if len_sum == 0 {
            return 0.0;
        }
        let dist = rapidfuzz::distance::indel::distance(a.chars(), b.chars());
        (1.0 - (dist as f64) / (len_sum as f64)).clamp(0.0, 1.0)
    };
    let r1 = norm(&s1, &s2);
    if sect.is_empty() {
        r1
    } else {
        r1.max(norm(&sect, &s1)).max(norm(&sect, &s2))
    }
}

struct QueryIntent {
    wants_date: bool,
    wants_version: bool,
    wants_number: bool,
    wants_person: bool,
}

impl QueryIntent {
    fn from_question(q_lower: &str) -> Self {
        let wants_date = [
            "when",
            "date",
            "release",
            "released",
            "year",
            "何时",
            "什么时候",
            "发布日期",
            "哪一年",
            "日期",
            "发布",
        ]
        .iter()
        .any(|k| q_lower.contains(k));
        let wants_version = ["version", "latest", "release", "v1", "v2", "版本", "最新"]
            .iter()
            .any(|k| q_lower.contains(k));
        let wants_number = [
            "price",
            "cost",
            "how much",
            "how many",
            "population",
            "port",
            "default",
            "价格",
            "多少",
            "费用",
            "默认",
        ]
        .iter()
        .any(|k| q_lower.contains(k));
        let wants_person = [
            "who ",
            "author",
            "creator",
            "created",
            "designed",
            "founded",
            "invented",
            "作者",
            "创始人",
            "发明",
            "是谁",
        ]
        .iter()
        .any(|k| q_lower.contains(k));
        Self {
            wants_date,
            wants_version,
            wants_number,
            wants_person,
        }
    }

    fn bonus(&self, text: &str) -> f64 {
        static DATE_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"(?:\b20\d{2}-\d{2}-\d{2}\b|\b(?:19|20)\d{2}\b|\d{4}年\d{1,2}月)")
                .expect("date re")
        });
        static VER_RE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"\bv?\d+\.\d+(?:\.\d+)?\b").expect("ver re"));
        static NUM_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"(?:[$€£¥]\s*\d|\b\d[\d,.]*(?:\s*%|\s*(?:USD|EUR|CNY|GB|MB|ms|s))?)")
                .expect("num re")
        });
        static PERSON_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"(?i)\b(?:created|designed|written|developed|founded|invented|authored)\s+by\b|\b(?:creator|author|founder)\b|作者|创始人|发明|开发").expect("person re")
        });

        let mut b = 0.0;
        if self.wants_date && DATE_RE.is_match(text) {
            b += 0.08;
        }
        if self.wants_version && VER_RE.is_match(text) {
            b += 0.10;
        }
        if self.wants_number && NUM_RE.is_match(text) {
            b += 0.06;
        }
        if self.wants_person && PERSON_RE.is_match(text) {
            b += 0.12;
        }
        b
    }
}

static MD_IMG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"!\[[^\]]*\]\([^)]*\)").expect("md img re"));
static MD_LINK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[([^\]]+)\]\([^)]*\)").expect("md link re"));

/// Clean Markdown noise and split into overlapping 1–3 sentence windows.
pub fn extract_sentence_windows(raw: &str, is_full_page: bool) -> Vec<String> {
    if raw.trim().is_empty() {
        return Vec::new();
    }
    if !is_full_page {
        let cleaned = clean_markdown_line(raw);
        if cleaned.chars().count() >= 6 {
            return vec![clip_window(&cleaned)];
        }
        return Vec::new();
    }

    // Strip fenced code blocks and collect clean paragraphs.
    let mut paragraphs: Vec<String> = Vec::new();
    let mut in_fence = false;
    let mut cur = Vec::new();

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if trimmed.is_empty() {
            if !cur.is_empty() {
                paragraphs.push(cur.join(" "));
                cur.clear();
            }
            continue;
        }
        let cleaned = clean_markdown_line(trimmed);
        if cleaned.is_empty() || is_boilerplate_line(&cleaned) {
            continue;
        }
        // Headings and bullet points start fresh logical units.
        if trimmed.starts_with('#') || trimmed.starts_with("- ") || trimmed.starts_with("* ") {
            if !cur.is_empty() {
                paragraphs.push(cur.join(" "));
                cur.clear();
            }
            paragraphs.push(cleaned);
        } else {
            cur.push(cleaned);
        }
    }
    if !cur.is_empty() {
        paragraphs.push(cur.join(" "));
    }

    let mut sentences: Vec<String> = Vec::new();
    for para in paragraphs {
        for s in split_sentences(&para) {
            if s.chars().count() >= MIN_WINDOW_CHARS
                || s.chars().any(is_cjk) && s.chars().count() >= 8
            {
                sentences.push(s);
            }
        }
        if sentences.len() >= 180 {
            break;
        }
    }

    if sentences.is_empty() {
        let fallback = clean_markdown_line(raw);
        if !fallback.is_empty() {
            return vec![clip_window(&fallback)];
        }
        return Vec::new();
    }

    // Build sliding 1..=3 sentence windows capped around MAX_WINDOW_CHARS.
    let mut windows = Vec::new();
    let n = sentences.len();
    for i in 0..n {
        let mut combined = sentences[i].clone();
        let mut count = combined.chars().count();
        windows.push(clip_window(&combined));
        for next in sentences.iter().take((i + 3).min(n)).skip(i + 1) {
            let next_len = next.chars().count();
            if count + 1 + next_len > MAX_WINDOW_CHARS {
                break;
            }
            combined.push(' ');
            combined.push_str(next);
            count += 1 + next_len;
            windows.push(clip_window(&combined));
        }
    }
    windows
}

/// Unescape Markdown backslash-escaped punctuation (e.g. `\&`, `\-`, `\_`, `\[`, `\]`, `\*`)
/// so quoted prose passages never show raw Markdown escape artifacts.
pub(crate) fn unescape_markdown_punctuation(text: &str) -> String {
    if !text.contains('\\') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\'
            && let Some(&next) = chars.peek()
            && matches!(
                next,
                '\\' | '`'
                    | '*'
                    | '_'
                    | '{'
                    | '}'
                    | '['
                    | ']'
                    | '('
                    | ')'
                    | '#'
                    | '+'
                    | '-'
                    | '.'
                    | '!'
                    | '&'
                    | '|'
                    | '<'
                    | '>'
                    | '~'
            )
        {
            out.push(next);
            chars.next();
            continue;
        }
        out.push(ch);
    }
    out
}

fn clean_markdown_line(line: &str) -> String {
    let s1 = MD_IMG_RE.replace_all(line, "");
    let s2 = MD_LINK_RE.replace_all(&s1, "$1");
    let unescaped = unescape_markdown_punctuation(&s2);
    let stripped = unescaped
        .trim()
        .trim_start_matches('#')
        .trim_start_matches(['>', '-', '*', '•', ' '])
        .replace("**", "")
        .replace("__", "")
        .replace('`', "");
    stripped.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_boilerplate_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.len() < 8
        || lower.starts_with("skip to ")
        || lower.starts_with("accept all cookies")
        || lower.starts_with("cookie policy")
        || lower.starts_with("privacy policy")
        || lower.starts_with("terms of service")
        || lower.starts_with("sign in")
        || lower.starts_with("log in")
        || lower == "table of contents"
}

/// Split text into sentences respecting CJK terminators (`。！？；`) and English `.!?`
/// without breaking decimal/version numbers like `1.85.0`.
pub fn split_sentences(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut buf = String::new();

    for i in 0..chars.len() {
        let ch = chars[i];
        buf.push(ch);

        let is_cjk_end = matches!(ch, '。' | '！' | '？' | '；');
        let is_ascii_end = matches!(ch, '!' | '?')
            || (ch == '.' && {
                let prev_digit = i > 0 && chars[i - 1].is_ascii_digit();
                let next_digit = i + 1 < chars.len() && chars[i + 1].is_ascii_digit();
                let next_space_or_end = i + 1 == chars.len() || chars[i + 1].is_whitespace();
                // Don't break inside version numbers (1.85.0) or abbreviations (U.S.)
                !(prev_digit && next_digit) && next_space_or_end
            });

        if is_cjk_end || is_ascii_end {
            let trimmed = buf.trim();
            if !trimmed.is_empty() {
                out.push(trimmed.to_owned());
            }
            buf.clear();
        }
    }
    let tail = buf.trim();
    if !tail.is_empty() {
        out.push(tail.to_owned());
    }
    out
}

fn clip_window(text: &str) -> String {
    if text.chars().count() <= MAX_WINDOW_CHARS {
        return text.to_owned();
    }
    let clipped: String = text.chars().take(MAX_WINDOW_CHARS).collect();
    format!("{}…", clipped.trim_end())
}
