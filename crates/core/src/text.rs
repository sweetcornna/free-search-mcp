//! Token estimation and truncation (`SPEC.md` §2.5.3).
//!
//! The estimator is the contract shared with the sandbox's
//! `util.estimateTokens`, so it is defined exactly, over Unicode scalar
//! values (JS must iterate code points, `for (const ch of s)`, not UTF-16 units):
//!
//! ```text
//! tokens = ceil(ascii / 4) + ceil(non_ascii / 1.5)
//!        = (ascii + 3) / 4  +  (2 * non_ascii + 2) / 3      (integer division)
//! ```

use serde_json::{Map, Value};

/// Estimated LLM tokens of `text` (see module docs for the exact formula).
pub fn estimate_tokens(text: &str) -> usize {
    let (mut ascii, mut other) = (0usize, 0usize);
    for c in text.chars() {
        if c.is_ascii() {
            ascii += 1;
        } else {
            other += 1;
        }
    }
    ascii.div_ceil(4) + (2 * other).div_ceil(3)
}

/// Estimated tokens of a value's compact JSON serialization.
pub fn estimate_json_tokens(value: &Value) -> usize {
    estimate_tokens(&serde_json::to_string(value).unwrap_or_default())
}

/// The longest prefix of `s` holding at most `max_chars` characters.
pub fn clip_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// Marker appended when a cut lands on a natural boundary.
pub const BOUNDARY_MARKER: &str = "\n\n[…truncated]";
/// Marker appended on a hard cut.
pub const HARD_MARKER: &str = " …";

const BOUNDARIES: [&str; 8] = ["\n\n", "\n", "。", ". ", "！", "! ", "？", "? "];

/// Port of Python `formatting.smart_truncate`: cut `text` to `max_chars`
/// characters at the latest paragraph / line / sentence boundary that lies in
/// the last 30% of the budget, else hard-cut. Returns `(text, truncated)`.
pub fn smart_truncate(text: &str, max_chars: usize) -> (String, bool) {
    if text.chars().count() <= max_chars {
        return (text.to_owned(), false);
    }
    (
        cut_at_boundary(clip_chars(text, max_chars), max_chars * 7 / 10),
        true,
    )
}

fn cut_at_boundary(head: &str, floor_chars: usize) -> String {
    let mut best: Option<usize> = None; // byte end of the kept part
    let mut best_chars = 0usize;
    for sep in BOUNDARIES {
        if let Some(idx) = head.rfind(sep) {
            let idx_chars = head[..idx].chars().count();
            let end_chars = idx_chars + sep.chars().count();
            if idx_chars >= floor_chars && end_chars > best_chars {
                best_chars = end_chars;
                best = Some(idx + sep.len());
            }
        }
    }
    match best {
        Some(end) if best_chars > 0 => format!("{}{BOUNDARY_MARKER}", head[..end].trim_end()),
        _ => format!("{}{HARD_MARKER}", head.trim_end()),
    }
}

/// Outcome of a token-budget cut.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct TextCut {
    pub text: String,
    pub truncated: bool,
    pub tokens: usize,
}

/// Cut plain text to at most `max_tokens` (markers included, for
/// `max_tokens >= 8`), preferring paragraph, then line, then sentence breaks.
pub fn truncate_to_tokens(text: &str, max_tokens: usize) -> TextCut {
    let tokens = estimate_tokens(text);
    if tokens <= max_tokens {
        return TextCut {
            text: text.to_owned(),
            truncated: false,
            tokens,
        };
    }
    let budget = max_tokens.saturating_sub(estimate_tokens(BOUNDARY_MARKER));
    // Largest char prefix within budget (the estimate is monotone in prefix length).
    let total = text.chars().count();
    let (mut lo, mut hi) = (0usize, total);
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if estimate_tokens(clip_chars(text, mid)) <= budget {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let out = cut_at_boundary(clip_chars(text, lo), lo * 7 / 10);
    let tokens = estimate_tokens(&out);
    TextCut {
        text: out,
        truncated: true,
        tokens,
    }
}

/// Outcome of [`shrink_json`].
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct JsonShrink {
    pub value: Value,
    pub truncated: bool,
    pub tokens: usize,
    /// Water level applied to string values (chars), if any.
    pub string_cap: Option<usize>,
    /// Water level applied to array/object lengths, if any.
    pub item_cap: Option<usize>,
}

/// Shortest string cap tried before the shrinker starts dropping items.
pub const MIN_STRING_CAP: usize = 40;

/// Structural water-filling shrinker: returns valid JSON whose compact
/// serialization fits `max_tokens`.
///
/// 1. Strings are capped at one common level `L` (short strings untouched,
///    long ones cut to `L` chars + `…`), with `L` as high as fits.
/// 2. If `L = MIN_STRING_CAP` is not enough, arrays and objects keep their
///    first `K` items, with `K` as high as fits; dropped items are reported by
///    a trailing `"…(+N more)"` element / a `"…": "+N more keys"` entry.
/// 3. Last resort: `L = 0`, `K = 1`. Only pathological nesting depth can still exceed
///    the budget; the result is then the smallest shape and `tokens > max_tokens`.
///
/// Object keys are never altered; key order is preserved.
pub fn shrink_json(value: &Value, max_tokens: usize) -> JsonShrink {
    let tokens = estimate_json_tokens(value);
    if tokens <= max_tokens {
        return JsonShrink {
            value: value.clone(),
            truncated: false,
            tokens,
            string_cap: None,
            item_cap: None,
        };
    }
    let (max_str, max_items) = extents(value);
    let fits = |l: usize, k: usize| {
        let v = cap(value, l, k);
        let t = estimate_json_tokens(&v);
        (t <= max_tokens, v, t)
    };
    // 1. string water level
    if max_str > MIN_STRING_CAP
        && let Some(l) = largest(MIN_STRING_CAP, max_str, |l| fits(l, usize::MAX).0)
    {
        let (_, v, t) = fits(l, usize::MAX);
        return JsonShrink {
            value: v,
            truncated: true,
            tokens: t,
            string_cap: Some(l),
            item_cap: None,
        };
    }
    let l = MIN_STRING_CAP.min(max_str);
    // 2. item water level
    if max_items >= 1
        && let Some(k) = largest(1, max_items, |k| fits(l, k).0)
    {
        let (_, v, t) = fits(l, k);
        return JsonShrink {
            value: v,
            truncated: true,
            tokens: t,
            string_cap: Some(l),
            item_cap: Some(k),
        };
    }
    // 3. last resort
    let (_, v, t) = fits(0, 1);
    JsonShrink {
        value: v,
        truncated: true,
        tokens: t,
        string_cap: Some(0),
        item_cap: Some(1),
    }
}

/// Largest `x` in `lo..=hi` with `ok(x)` (monotone: true up to some point).
fn largest(lo: usize, hi: usize, ok: impl Fn(usize) -> bool) -> Option<usize> {
    if !ok(lo) {
        return None;
    }
    let (mut lo, mut hi) = (lo, hi);
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if ok(mid) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    Some(lo)
}

fn extents(v: &Value) -> (usize, usize) {
    match v {
        Value::String(s) => (s.chars().count(), 0),
        Value::Array(a) => a
            .iter()
            .map(extents)
            .fold((0, a.len()), |(s, n), (s2, n2)| (s.max(s2), n.max(n2))),
        Value::Object(o) => o
            .values()
            .map(extents)
            .fold((0, o.len()), |(s, n), (s2, n2)| (s.max(s2), n.max(n2))),
        _ => (0, 0),
    }
}

fn cap(v: &Value, max_chars: usize, max_items: usize) -> Value {
    match v {
        Value::String(s) if s.chars().count() > max_chars => {
            Value::String(format!("{}…", clip_chars(s, max_chars)))
        }
        Value::Array(a) => {
            let mut out: Vec<Value> = a
                .iter()
                .take(max_items)
                .map(|x| cap(x, max_chars, max_items))
                .collect();
            if a.len() > max_items {
                out.push(Value::String(format!("…(+{} more)", a.len() - max_items)));
            }
            Value::Array(out)
        }
        Value::Object(o) => {
            let mut out = Map::new();
            for (k, x) in o.iter().take(max_items) {
                out.insert(k.clone(), cap(x, max_chars, max_items));
            }
            if o.len() > max_items {
                out.insert(
                    "…".to_owned(),
                    Value::String(format!("+{} more keys", o.len() - max_items)),
                );
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    #[test]
    fn estimator_formula() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("a"), 1);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
        assert_eq!(estimate_tokens(&"a".repeat(400)), 100);
        assert_eq!(estimate_tokens("中"), 1);
        assert_eq!(estimate_tokens("中文"), 2);
        assert_eq!(estimate_tokens("中文字"), 2);
        assert_eq!(estimate_tokens(&"中".repeat(100)), 67);
        assert_eq!(estimate_tokens(&"中".repeat(150)), 100);
        // mixed: 11 ascii -> 3, 2 non-ascii -> 2
        assert_eq!(estimate_tokens("hello world中文"), 5);
        // astral char counts once (code point, not UTF-16 unit)
        assert_eq!(estimate_tokens("😀"), 1);
    }

    #[test]
    fn smart_truncate_matches_python_tests() {
        assert_eq!(smart_truncate("short", 100), ("short".to_owned(), false));
        let mut text = "paragraph one filler text content here. ".repeat(5);
        text.push_str("\n\n");
        text.push_str(&"paragraph two starts here.".repeat(8));
        let (out, t) = smart_truncate(&text, 250);
        assert!(
            t && out.ends_with("[…truncated]") && !out.contains("paragraph two"),
            "{out}"
        );
        let (out, t) = smart_truncate(&"no_boundaries_at_all_just_a_giant_word".repeat(5), 30);
        assert!(
            t && out.chars().count() <= 40 && out.ends_with(" …"),
            "{out}"
        );
        // CJK sentence boundary
        let cjk = format!("{}。{}", "中".repeat(80), "文".repeat(50));
        let (out, t) = smart_truncate(&cjk, 100);
        assert!(
            t && out.starts_with(&"中".repeat(80)) && !out.contains('文'),
            "{out}"
        );
    }

    #[test]
    fn token_cut_prefers_breaks_and_fits() {
        let text = format!(
            "{}\n\n{}\n{}",
            "alpha ".repeat(60),
            "beta ".repeat(60),
            "gamma ".repeat(60)
        );
        let cut = truncate_to_tokens(&text, 120);
        assert!(cut.truncated && cut.tokens <= 120, "{cut:?}");
        assert!(cut.text.ends_with(BOUNDARY_MARKER) || cut.text.ends_with(HARD_MARKER));
        assert!(!cut.text.contains("gamma"));
        let same = truncate_to_tokens("tiny", 10);
        assert_eq!((same.text.as_str(), same.truncated), ("tiny", false));
        let cjk = truncate_to_tokens(&"漢字".repeat(500), 100);
        assert!(cjk.tokens <= 100);
    }

    #[test]
    fn shrink_json_water_fills_strings_first() {
        let v = json!({
            "query": "rust",
            "results": (0..10).map(|i| json!({"title": format!("T{i}"), "snippet": "x".repeat(2000)})).collect::<Vec<_>>()
        });
        let s = shrink_json(&v, 1500);
        assert!(s.truncated && s.tokens <= 1500, "{}", s.tokens);
        assert!(s.item_cap.is_none(), "strings alone were enough");
        let l = s.string_cap.unwrap();
        assert!((MIN_STRING_CAP..2000).contains(&l));
        assert_eq!(s.value["results"].as_array().unwrap().len(), 10);
        assert_eq!(s.value["query"], "rust", "short strings untouched");
        assert_eq!(s.value["results"][3]["title"], "T3");
        // keys and order preserved
        let keys: Vec<&String> = s.value.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["query", "results"]);
    }

    #[test]
    fn shrink_json_drops_items_when_needed() {
        let v = Value::Array(
            (0..2000)
                .map(|i| json!({"id": i, "name": format!("item number {i}")}))
                .collect(),
        );
        let s = shrink_json(&v, 500);
        assert!(s.truncated && s.tokens <= 500, "{}", s.tokens);
        let arr = s.value.as_array().unwrap();
        let k = s.item_cap.unwrap();
        assert_eq!(arr.len(), k + 1);
        assert_eq!(arr[k], json!(format!("…(+{} more)", 2000 - k)));
        assert_eq!(arr[0]["id"], 0);
    }

    #[test]
    fn shrink_json_noop_under_budget() {
        let v = json!({"a": [1, 2, 3], "b": "c"});
        let s = shrink_json(&v, 100);
        assert!(!s.truncated);
        assert_eq!(s.value, v);
    }

    fn arb_json() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            any::<i32>().prop_map(|n| json!(n)),
            "[a-z 中文。\\n]{0,300}".prop_map(Value::String),
        ];
        leaf.prop_recursive(4, 200, 12, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..12).prop_map(Value::Array),
                prop::collection::btree_map("[a-z]{1,6}", inner, 0..8)
                    .prop_map(|m| Value::Object(m.into_iter().collect())),
            ]
        })
    }

    proptest! {
        #[test]
        fn shrink_json_fits_and_stays_valid(v in arb_json(), budget in 60usize..2000) {
            let s = shrink_json(&v, budget);
            prop_assert!(s.tokens <= budget, "tokens {} > budget {}", s.tokens, budget);
            prop_assert_eq!(s.tokens, estimate_json_tokens(&s.value));
            let text = serde_json::to_string(&s.value).unwrap();
            prop_assert!(serde_json::from_str::<Value>(&text).is_ok());
            if !s.truncated { prop_assert_eq!(&s.value, &v); }
        }

        #[test]
        fn token_cut_is_bounded_prefix(text in "[a-z 中。\\n.]{0,3000}", budget in 8usize..800) {
            let cut = truncate_to_tokens(&text, budget);
            prop_assert!(cut.tokens <= budget);
            let body = cut.text.trim_end_matches(BOUNDARY_MARKER).trim_end_matches(HARD_MARKER);
            prop_assert!(text.starts_with(body.trim_end()) || !cut.truncated);
        }

        #[test]
        fn estimator_is_monotone_in_prefixes(text in "\\PC{0,200}") {
            let mut last = 0;
            for n in 0..=text.chars().count() {
                let t = estimate_tokens(clip_chars(&text, n));
                prop_assert!(t >= last);
                last = t;
            }
        }
    }
}
