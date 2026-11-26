//! 30-page offline HTML extraction golden benchmark (`SPEC.md` §10.2, `SPEC-lanes.md` W4a).
//!
//! Compares Rust `extract_html` (`lol_html` -> `trafilatura` -> `dom_smoothie` -> `htmd`)
//! directly against the real Python `trafilatura 2.0.0` (`fetcher._extract`) reference
//! output stored offline in `tests/fixtures/corpus/golden.json` (30 pages across 5
//! categories: news, docs, blog, forum, CJK):
//! - Mean token F1 >= 0.85 AND every individual page >= 0.50 floor.
//! - Strict normalized equality on all 4 metadata fields (`title`, `author`,
//!   `published_date`, `sitename`), with a small documented override list where the
//!   Python 2.0.0 oracle emits demonstrably bogus metadata (e.g. Wikipedia navbox
//!   authority-control strings or date fragments as `author`).

use fsm_extract::html::extract_html;
use regex::Regex;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::LazyLock;

#[derive(Debug, Deserialize)]
struct GoldenEntry {
    id: String,
    category: String,
    url: String,
    file: String,
    py_title: String,
    py_author: Option<String>,
    py_date: Option<String>,
    py_sitename: Option<String>,
    py_markdown: String,
}

static MD_LINK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[([^\]]+)\]\([^)]+\)").expect("md link regex"));

fn is_cjk(c: char) -> bool {
    matches!(
        c as u32,
        0x3040..=0x30FF // Hiragana / Katakana
            | 0x3400..=0x4DBF // CJK Ext A
            | 0x4E00..=0x9FFF // CJK Unified
            | 0xAC00..=0xD7AF // Hangul Syllables
            | 0xF900..=0xFAFF // CJK Compatibility
    )
}

/// CJK-aware word/char tokenizer: strips Markdown link URLs, extracts lowercase
/// alphanumeric words and individual CJK characters as multiset counts.
fn tokenize_multiset(text: &str) -> (HashMap<String, usize>, usize) {
    let stripped = MD_LINK_RE.replace_all(text, "$1");
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut total = 0usize;
    let mut buf = String::new();

    let flush = |buf: &mut String, counts: &mut HashMap<String, usize>, total: &mut usize| {
        if !buf.is_empty() {
            let tok = std::mem::take(buf);
            *counts.entry(tok).or_insert(0) += 1;
            *total += 1;
        }
    };

    for ch in stripped.chars() {
        if is_cjk(ch) {
            flush(&mut buf, &mut counts, &mut total);
            *counts.entry(ch.to_string()).or_insert(0) += 1;
            total += 1;
        } else if ch.is_alphanumeric() {
            for lc in ch.to_lowercase() {
                buf.push(lc);
            }
        } else {
            flush(&mut buf, &mut counts, &mut total);
        }
    }
    flush(&mut buf, &mut counts, &mut total);
    (counts, total)
}

fn token_f1(pred: &str, reference: &str) -> (f64, f64, f64) {
    let (p_map, p_tot) = tokenize_multiset(pred);
    let (r_map, r_tot) = tokenize_multiset(reference);
    if p_tot == 0 || r_tot == 0 {
        return (0.0, 0.0, 0.0);
    }
    let mut overlap = 0usize;
    for (tok, p_cnt) in &p_map {
        if let Some(r_cnt) = r_map.get(tok) {
            overlap += (*p_cnt).min(*r_cnt);
        }
    }
    let precision = overlap as f64 / p_tot as f64;
    let recall = overlap as f64 / r_tot as f64;
    let f1 = if precision + recall > 0.0 {
        2.0 * precision * recall / (precision + recall)
    } else {
        0.0
    };
    (precision, recall, f1)
}

/// Strict canonical normalization for metadata equality (NO substring or F1 relaxation):
/// lowercase, normalize curly quotes/apostrophes & dashes, collapse whitespace.
fn normalize_meta(s: &str) -> String {
    let mapped: String = s
        .chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{02BC}' => '\'',
            '\u{201C}' | '\u{201D}' => '"',
            '\u{2013}' | '\u{2014}' => '-',
            '\u{00A0}' => ' ',
            other => other,
        })
        .collect();
    mapped
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn exact_meta_eq(got: &str, expected: &str) -> bool {
    let g = normalize_meta(got);
    let e = normalize_meta(expected);
    !g.is_empty() && g == e
}

/// Documented per-page overrides ONLY where Python `trafilatura 2.0.0` emits a
/// demonstrably bogus metadata value (e.g. a date fragment or authority-control navbox
/// as `author`, or bare hostname instead of `<meta name="og:site_name">`).
/// Each override asserts the exact better value Rust extracts instead.
fn documented_author_override(id: &str) -> Option<Option<&'static str>> {
    match id {
        // Python outputs "[Posted March" (byline date fragment, not an author).
        "01_news_lwn" => Some(None),
        // Python outputs "Sept 5" (post date fragment, not an author).
        "13_blog_rust" => Some(None),
        // Python scrapes Wikipedia's authority-control navbox as author; Rust reads JSON-LD author.
        "12_docs_wikipedia" => Some(Some("Contributors to Wikimedia projects")),
        "25_cjk_zh_wiki" => Some(Some("维基媒体项目贡献者")),
        "26_cjk_ja_wiki" => Some(Some("ウィキメディアプロジェクトへの貢献者")),
        "27_cjk_ko_wiki" => Some(Some("위키미디어 프로젝트 기여자")),
        _ => None,
    }
}

fn documented_sitename_override(id: &str) -> Option<&'static str> {
    match id {
        // MDN sets `<meta name="og:site_name" content="MDN Web Docs">`; Python falls back to hostname.
        "09_docs_mdn" => Some("MDN Web Docs"),
        _ => None,
    }
}

#[test]
fn test_30_html_corpus_golden_f1_and_metadata() {
    let corpus_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/corpus");
    let golden_raw = fs::read_to_string(corpus_dir.join("golden.json"))
        .expect("tests/fixtures/corpus/golden.json must exist");
    let entries: Vec<GoldenEntry> = serde_json::from_str(&golden_raw).expect("parse golden.json");
    assert_eq!(
        entries.len(),
        30,
        "corpus must contain exactly 30 HTML pages"
    );

    // Verify all 5 categories are represented.
    let mut cats: HashMap<String, usize> = HashMap::new();
    for e in &entries {
        *cats.entry(e.category.clone()).or_insert(0) += 1;
    }
    for required in ["news", "docs", "blog", "forum", "cjk"] {
        assert!(
            cats.get(required).copied().unwrap_or(0) >= 5,
            "missing category {required}: {cats:?}"
        );
    }

    let mut sum_f1 = 0.0f64;
    let mut min_f1 = 1.0f64;
    let mut min_f1_id = String::new();
    let mut title_ok = 0usize;
    let mut author_total = 0usize;
    let mut author_ok = 0usize;
    let mut date_total = 0usize;
    let mut date_ok = 0usize;
    let mut site_total = 0usize;
    let mut site_ok = 0usize;

    for e in &entries {
        let html = fs::read_to_string(corpus_dir.join(&e.file))
            .unwrap_or_else(|err| panic!("missing {}: {err}", e.file));
        let extracted = extract_html(&html, &e.url);

        assert!(
            !extracted.markdown.trim().is_empty(),
            "{} produced empty markdown",
            e.id
        );

        let (p, r, f1) = token_f1(&extracted.markdown, &e.py_markdown);
        sum_f1 += f1;
        if f1 < min_f1 {
            min_f1 = f1;
            min_f1_id = e.id.clone();
        }

        let t_match = exact_meta_eq(&extracted.title, &e.py_title);
        if t_match {
            title_ok += 1;
        }

        let a_status = if let Some(override_a) = documented_author_override(&e.id) {
            author_total += 1;
            let matched = match override_a {
                None => extracted.author.is_none(),
                Some(exp) => extracted
                    .author
                    .as_deref()
                    .is_some_and(|got| exact_meta_eq(got, exp)),
            };
            if matched {
                author_ok += 1;
                "OVR-OK"
            } else {
                "OVR-DIFF"
            }
        } else if let Some(ref exp_a) = e.py_author {
            author_total += 1;
            if extracted
                .author
                .as_deref()
                .is_some_and(|a| exact_meta_eq(a, exp_a))
            {
                author_ok += 1;
                "OK"
            } else {
                "DIFF"
            }
        } else {
            "-"
        };

        let d_status = if let Some(ref exp_d) = e.py_date {
            date_total += 1;
            if extracted.published_date.as_deref() == Some(exp_d.as_str()) {
                date_ok += 1;
                "OK"
            } else {
                "DIFF"
            }
        } else {
            "-"
        };

        let s_status = if let Some(override_s) = documented_sitename_override(&e.id) {
            site_total += 1;
            if extracted
                .sitename
                .as_deref()
                .is_some_and(|s| exact_meta_eq(s, override_s))
            {
                site_ok += 1;
                "OVR-OK"
            } else {
                "OVR-DIFF"
            }
        } else if let Some(ref exp_s) = e.py_sitename {
            site_total += 1;
            if extracted
                .sitename
                .as_deref()
                .is_some_and(|s| exact_meta_eq(s, exp_s))
            {
                site_ok += 1;
                "OK"
            } else {
                "DIFF"
            }
        } else {
            "-"
        };

        eprintln!(
            "{:22} cat={:5} P={:.3} R={:.3} F1={:.3} title={} ({:?} vs {:?}) author={} ({:?} vs {:?}) date={} ({:?} vs {:?}) site={} ({:?} vs {:?})",
            e.id,
            e.category,
            p,
            r,
            f1,
            if t_match { "OK" } else { "DIFF" },
            extracted.title,
            e.py_title,
            a_status,
            extracted.author,
            e.py_author,
            d_status,
            extracted.published_date,
            e.py_date,
            s_status,
            extracted.sitename,
            e.py_sitename,
        );

        // B3 regression checks on the two previously broken corpus pages (HN & Ars Technica).
        if e.id == "19_forum_hn" {
            assert!(
                f1 >= 0.90,
                "19_forum_hn must recover full discussion thread (F1 >= 0.90), got {f1:.3}"
            );
            assert!(
                !extracted.markdown.contains("submit | login"),
                "19_forum_hn must not leak HN nav bar"
            );
        }
        if e.id == "02_news_ars" {
            assert!(
                !extracted.markdown.contains("Skip to content"),
                "02_news_ars must not leak screen-reader skip link"
            );
            // Rust recovers all 3 ad-split `.post-content` chunks of Dan Goodin's article
            // (Python trafilatura 2.0.0 dropped chunks 2 & 3 and emitted comment-picks instead).
            assert!(
                extracted.markdown.contains("JiaT75")
                    && extracted.markdown.contains("Freund provided a script"),
                "02_news_ars must retain all 3 parts of the article body"
            );
        }

        assert!(
            f1 >= 0.50,
            "{} fell below per-page F1 floor 0.50: P={p:.3} R={r:.3} F1={f1:.3}",
            e.id
        );
    }

    let mean_f1 = sum_f1 / entries.len() as f64;
    let title_acc = title_ok as f64 / entries.len() as f64;
    let author_acc = if author_total > 0 {
        author_ok as f64 / author_total as f64
    } else {
        1.0
    };
    let date_acc = if date_total > 0 {
        date_ok as f64 / date_total as f64
    } else {
        1.0
    };
    let site_acc = if site_total > 0 {
        site_ok as f64 / site_total as f64
    } else {
        1.0
    };

    eprintln!(
        "\nCORPUS SUMMARY vs Python trafilatura 2.0.0 (n=30): mean_F1={mean_f1:.4} (min={min_f1:.4} on {min_f1_id}) | \
         title={title_ok}/30 ({title_acc:.2}) | \
         author={author_ok}/{author_total} ({author_acc:.2}) | \
         date={date_ok}/{date_total} ({date_acc:.2}) | \
         sitename={site_ok}/{site_total} ({site_acc:.2})"
    );

    assert!(
        mean_f1 >= 0.85,
        "mean token F1 {mean_f1:.4} fell below required 0.85 threshold"
    );
    assert!(
        title_acc >= 0.90,
        "title accuracy {title_ok}/30 ({title_acc:.2}) below 0.90"
    );
    assert!(
        author_acc >= 0.85,
        "author accuracy {author_ok}/{author_total} ({author_acc:.2}) below 0.85"
    );
    assert!(
        date_acc >= 0.95,
        "date accuracy {date_ok}/{date_total} ({date_acc:.2}) below 0.95"
    );
    assert!(
        site_acc >= 0.90,
        "sitename accuracy {site_ok}/{site_total} ({site_acc:.2}) below 0.90"
    );
}
