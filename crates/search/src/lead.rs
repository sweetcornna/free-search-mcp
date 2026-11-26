//! The extractive lead (`aggregator._lead_snippet`): never an LLM answer.

use crate::coherence::lead_query_terms;
use fsm_core::Hit;
use fsm_core::url_canon::host_of;
use std::collections::HashSet;

/// A lead snippet must be at least this long.
pub const LEAD_MIN_CHARS: usize = 80;

/// The trailing "(Outlet)" of a Google News title, or "".
pub fn outlet_from_gnews_title(title: &str) -> &str {
    let t = title.trim_end();
    let Some(body) = t.strip_suffix(')') else {
        return "";
    };
    match body.rfind(['(', ')']) {
        Some(i) if body[i..].starts_with('(') => body[i + 1..].trim(),
        _ => "",
    }
}

fn host_label(url: &str) -> String {
    let host = host_of(url);
    host.strip_prefix("www.").unwrap_or(&host).to_owned()
}

/// "According to {host}: {snippet}" from the top three results.
///
/// A first result that a direct claimant looked up IS the answer and rarely
/// echoes the question's words, so it is taken without the term test.
/// Otherwise the snippet needs ≥ 80 characters and ≥ min(2, #terms) query
/// terms. A Google News redirect is attributed to the outlet in its title.
pub fn lead_snippet(query: &str, results: &[Hit], direct: &HashSet<String>) -> Option<String> {
    if let Some(first) = results.first()
        && first.sources.iter().any(|s| direct.contains(s))
    {
        let sn = first.snippet.trim();
        if !sn.is_empty() {
            return Some(format!("According to {}: {sn}", host_label(&first.url)));
        }
    }
    let terms = lead_query_terms(query);
    if terms.is_empty() {
        return None;
    }
    let need = terms.len().min(2);
    for r in results.iter().take(3) {
        let sn = r.snippet.trim();
        if sn.chars().count() < LEAD_MIN_CHARS {
            continue;
        }
        let low = sn.to_lowercase();
        if terms.iter().filter(|t| low.contains(t.as_str())).count() < need {
            continue;
        }
        let mut host = host_label(&r.url);
        if host == "news.google.com" {
            let outlet = outlet_from_gnews_title(&r.title);
            if !outlet.is_empty() {
                host = outlet.to_owned();
            }
        }
        return Some(format!("According to {host}: {sn}"));
    }
    None
}

#[cfg(test)]
mod tests {
    //! Ported verbatim from `tests/test_dedup_and_lead.py` (lead cases).
    use super::*;

    fn r(title: &str, url: &str, snippet: &str) -> Hit {
        let mut h = Hit::new(title, url);
        h.snippet = snippet.to_owned();
        h
    }

    fn none() -> HashSet<String> {
        HashSet::new()
    }

    fn set(v: Vec<String>) -> HashSet<String> {
        v.into_iter().collect()
    }

    #[test]
    fn lead_snippet_picks_top3_with_two_query_terms_and_long_enough() {
        let results = [
            r("x", "https://a.com", "tiny"),
            r(
                "y",
                "https://realpython.com/async-tutorial",
                "This guide walks through python async programming, \
                 covering coroutines and the event loop in detail.",
            ),
            r(
                "z",
                "https://docs.python.org",
                "The python docs explain async/await syntax with examples \
                 and runtime behavior for the curious reader.",
            ),
        ];
        let out = lead_snippet("python async tutorial guide", &results, &none()).unwrap();
        assert!(out.starts_with("According to realpython.com:"));
        assert!(out.contains("python async programming"));
    }

    #[test]
    fn lead_snippet_attributes_googlenews_to_real_outlet() {
        let results = [r(
            "OpenAI ships a new python async runtime (Reuters)",
            "https://news.google.com/rss/articles/CBMiABC123",
            "The new python async runtime overhauls coroutine scheduling \
             and the event loop for high-throughput inference workloads.",
        )];
        let out = lead_snippet("python async runtime", &results, &none()).unwrap();
        assert!(out.starts_with("According to Reuters:"));
        assert!(!out.contains("news.google.com"));
    }

    #[test]
    fn lead_snippet_returns_none_when_no_snippet_qualifies() {
        let results = [
            r("x", "https://a.com", "too short"),
            r("y", "https://b.com", "another short one"),
            r("z", "https://c.com", ""),
        ];
        assert_eq!(
            lead_snippet("python async tutorial guide", &results, &none()),
            None
        );
    }

    #[test]
    fn lead_snippet_requires_two_terms_not_one() {
        let results = [r(
            "x",
            "https://a.com",
            "This is a long enough snippet that mentions python only once \
             but never the other terms used in the query at all here.",
        )];
        assert_eq!(
            lead_snippet("python async tutorial guide", &results, &none()),
            None
        );
    }

    #[test]
    fn lead_snippet_ignores_short_query_terms() {
        let results = [r(
            "x",
            "https://a.com",
            "a long snippet that is in fact eighty plus chars long for the test to be valid input here.",
        )];
        assert_eq!(lead_snippet("a is in", &results, &none()), None);
    }

    #[test]
    fn lead_snippet_only_inspects_top_3() {
        let short = r("x", "https://a.com", "short");
        let results = [
            short.clone(),
            short.clone(),
            short,
            r(
                "y",
                "https://b.com",
                "This long snippet about python async programming would qualify, \
                 but it's the 4th result so we should never even look at it.",
            ),
        ];
        assert_eq!(lead_snippet("python async", &results, &none()), None);
    }

    #[test]
    fn lead_snippet_strips_www_prefix_from_host() {
        let results = [r(
            "x",
            "https://www.example.com/post",
            "A long-form article about python async programming with \
             concrete examples and benchmarks for various scenarios.",
        )];
        let out = lead_snippet("python async tutorial", &results, &none()).unwrap();
        assert!(out.starts_with("According to example.com:"));
    }

    #[test]
    fn lead_query_terms_cjk_emits_bigrams() {
        let terms = set(lead_query_terms("DeepSeek MoE 模型架构"));
        assert!(terms.contains("deepseek"));
        assert!(!terms.contains("moe"));
        for bg in ["模型", "型架", "架构"] {
            assert!(terms.contains(bg), "{bg}");
        }
    }

    #[test]
    fn lead_query_terms_pure_ascii_unchanged() {
        let terms = set(lead_query_terms("python async tutorial guide"));
        assert_eq!(
            terms,
            set(["python", "async", "tutorial", "guide"]
                .map(String::from)
                .to_vec())
        );
    }

    #[test]
    fn lead_query_terms_skips_solo_cjk_char() {
        let terms = set(lead_query_terms("python 是 best"));
        assert!(!terms.contains("是"));
        assert_eq!(terms, set(["python", "best"].map(String::from).to_vec()));
    }

    #[test]
    fn lead_snippet_picks_chinese_query_when_bigrams_match() {
        let results = [r(
            "DeepSeek 解析",
            "https://zhuanlan.zhihu.com/p/123",
            "DeepSeek 是一个基于稀疏 MoE 模型 的开源大模型，其 架构 设计在\
             多个维度上都有显著创新。本文从专家路由、负载均衡、训练稳定性、\
             以及推理时延等多个角度系统拆解 DeepSeek-V3 的关键设计权衡。",
        )];
        let out = lead_snippet("DeepSeek MoE 模型架构", &results, &none()).unwrap();
        assert!(out.contains("zhihu.com"));
        assert!(out.contains("DeepSeek"));
    }

    // --- 1.0 additions ---

    #[test]
    fn a_direct_record_is_the_lead_without_the_term_test() {
        let mut rec = r(
            "JPY to CNY",
            "https://www.frankfurter.app/latest",
            "10000 JPY = 425.70 CNY",
        );
        rec.sources = vec!["frankfurter".into()];
        let direct: HashSet<String> = ["frankfurter".to_owned()].into();
        let out = lead_snippet("1万日元等于多少人民币", std::slice::from_ref(&rec), &direct);
        assert_eq!(
            out.unwrap(),
            "According to frankfurter.app: 10000 JPY = 425.70 CNY"
        );
        assert_eq!(lead_snippet("1万日元等于多少人民币", &[rec], &none()), None);
        assert_eq!(lead_snippet("python async", &[], &none()), None);
    }

    #[test]
    fn the_outlet_is_the_trailing_parenthesis_only() {
        assert_eq!(
            outlet_from_gnews_title("Headline (The Verge) "),
            "The Verge"
        );
        assert_eq!(outlet_from_gnews_title("No outlet"), "");
        // Python's `\(([^()]+)\)\s*$` finds nothing in a nested tail.
        assert_eq!(outlet_from_gnews_title("Bad (nest (x))"), "");
    }
}
