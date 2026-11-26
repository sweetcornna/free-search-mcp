//! Prelude-sync helpers, cross-checked against Rust (SPEC.md §3.1).

use crate::mock::*;
use fsm_core::text::{estimate_json_tokens, estimate_tokens};
use fsm_core::url_canon::url_key;
use rand::{RngExt, SeedableRng};
use serde_json::{Value, json};

fn palette() -> Vec<char> {
    let mut p: Vec<char> = "abcXYZ019 .,;:-_\n\t\"'\\/".chars().collect();
    p.extend("中文字漢字。日本語한국어éüñçΩЖ€😀🎉𝄞\u{0301}\u{200b}\u{feff}".chars());
    p
}

fn random_strings(n: usize, seed: u64) -> Vec<String> {
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let pal = palette();
    (0..n)
        .map(|_| {
            let len = rng.random_range(0..120);
            (0..len)
                .map(|_| pal[rng.random_range(0..pal.len())])
                .collect()
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn estimate_tokens_matches_rust() {
    let mut inputs: Vec<String> = [
        "",
        "a",
        "abcd",
        "abcde",
        "中",
        "中文",
        "中文字",
        "hello world中文",
        "😀",
        "😀😀😀",
        "e\u{0301}",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    inputs.push("a".repeat(400));
    inputs.push("中".repeat(150));
    inputs.extend(random_strings(400, 7));
    let code = format!(
        "const xs = {};\nreturn xs.map((s) => util.estimateTokens(s));",
        json!(inputs)
    );
    let got = exec(&code).await.json();
    let want: Vec<usize> = inputs.iter().map(|s| estimate_tokens(s)).collect();
    assert_eq!(got, json!(want));
    // Non-strings are measured as compact JSON, like estimate_json_tokens.
    let obj = json!({"title": "中文 title", "n": [1, 2, 3], "nested": {"k": "v"}});
    let r = exec(&format!("return [util.estimateTokens({obj}), util.estimateTokens(null), util.estimateTokens(12345)];")).await;
    assert_eq!(r.json(), json!([estimate_json_tokens(&obj), 0, 2]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dedupe_uses_rust_url_key() {
    let urls = [
        "https://Example.com/a/",
        "http://www.example.com/a",
        "https://example.com/a#frag",
        "https://example.com:443/a",
        "https://example.com/a?x=1",
        "https://example.com/a?x=1#y",
        "https://example.com:8080/a",
        "//example.com/b",
        "https://example.com/b",
        "not a url",
        "not a url",
        "https://例子.测试/路径",
        "https://xn--fsqu00a.xn--0zwm56d/%E8%B7%AF%E5%BE%84",
    ];
    let r = exec(&format!("return util.dedupe({});", json!(urls))).await;
    let mut seen = std::collections::HashSet::new();
    let want: Vec<&str> = urls
        .iter()
        .copied()
        .filter(|u| {
            let t = u.trim_start().to_ascii_lowercase();
            let key =
                if t.starts_with("http://") || t.starts_with("https://") || t.starts_with("//") {
                    format!("u:{}", url_key(u))
                } else {
                    format!("s:{u}")
                };
            seen.insert(key)
        })
        .collect();
    assert_eq!(r.json(), json!(want));
    // Objects dedupe on url_key(url), others on JSON; keyFn overrides; order kept.
    let r = exec(
        r#"return [
          util.dedupe([{url: 'https://a.com/x/', n: 1}, {url: 'https://www.a.com/x', n: 2}, {n: 3}, {n: 3}, {n: 4}]),
          util.dedupe([3, 1, 3, 2, 1]),
          util.dedupe(['b', 'a', 'B'], (s) => s.toLowerCase()),
          util.dedupe([{id: 1}, {id: 2}, {id: 1}], (x) => x.id),
        ];"#,
    )
    .await;
    assert_eq!(
        r.json(),
        json!([[{"url": "https://a.com/x/", "n": 1}, {"n": 3}, {"n": 4}], [3, 1, 2], ["b", "a"], [{"id": 1}, {"id": 2}]])
    );
    let r =
        exec("try { util.dedupe('abc'); } catch (e) { return e.name + ': ' + e.message; }").await;
    assert_eq!(
        r.json(),
        json!("TypeError: util.dedupe(items, keyFn?): items must be an array")
    );
}

/// Rust reference of util.grep for literal patterns.
fn grep_ref(text: &str, needle: &str, ctx: usize) -> Vec<String> {
    let lines: Vec<&str> = text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let needle = needle.to_lowercase();
    let mut out = Vec::new();
    let mut win: Option<(usize, usize)> = None;
    for (i, line) in lines.iter().enumerate() {
        if !line.to_lowercase().contains(&needle) {
            continue;
        }
        let (s, e) = (i.saturating_sub(ctx), (i + ctx).min(lines.len() - 1));
        match win {
            Some((ws, we)) if s <= we + usize::from(ctx > 0) => win = Some((ws, we.max(e))),
            _ => {
                if let Some((ws, we)) = win {
                    out.push(lines[ws..=we].join("\n"));
                }
                win = Some((s, e));
            }
        }
    }
    if let Some((ws, we)) = win {
        out.push(lines[ws..=we].join("\n"));
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grep_matches_rust_reference() {
    let mut rng = rand::rngs::StdRng::seed_from_u64(11);
    let words = [
        "alpha", "Beta", "PRICE", "price", "中文", "价格", "gamma", "", "x y z",
    ];
    let mut cases = Vec::new();
    for _ in 0..60 {
        let n = rng.random_range(1..40);
        let text: Vec<String> = (0..n)
            .map(|_| {
                let k = rng.random_range(0..4);
                (0..k)
                    .map(|_| words[rng.random_range(0..words.len())])
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect();
        let sep = if rng.random_bool(0.3) { "\r\n" } else { "\n" };
        let needle = ["price", "中文", "beta", "a"][rng.random_range(0..4)];
        cases.push((text.join(sep), needle, rng.random_range(0..3usize)));
    }
    let js_cases: Vec<Value> = cases.iter().map(|(t, n, c)| json!([t, n, c])).collect();
    let r = exec(&format!(
        "return {}.map(([t, n, c]) => util.grep(t, n, c));",
        json!(js_cases)
    ))
    .await;
    let want: Vec<Vec<String>> = cases.iter().map(|(t, n, c)| grep_ref(t, n, *c)).collect();
    assert_eq!(r.json(), json!(want));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grep_regex_objects_and_errors() {
    let r = exec(
        r#"const t = 'one\nTwo price\nthree\nfour Price\nfive';
        const g = /price/gi;
        return [
          util.grep(t, /price/i),
          util.grep(t, g), util.grep(t, g),
          util.grep(t, 'PRICE', 1),
          util.grep(await web.fetch('https://example.com/a'), 'price'),
          util.grep({a: 'x', b: 'price here'}, 'price'),
          util.grep(t, 'zzz'),
        ];"#,
    )
    .await;
    assert_eq!(
        r.json(),
        json!([
            ["Two price", "four Price"],
            ["Two price", "four Price"],
            ["Two price", "four Price"],
            ["one\nTwo price\nthree\nfour Price\nfive"],
            ["Second line mentions Price: 42."],
            [" \"b\": \"price here\""],
            []
        ])
    );
    let r = exec("try { util.grep('x', ''); } catch (e) { return e.name; }").await;
    assert_eq!(r.json(), json!("TypeError"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pick_projects_own_keys() {
    let r = exec(
        r#"const base = { inherited: 1 };
        const o = Object.create(base); o.a = 1; o.b = 2; o.c = 3;
        return [
          util.pick(o, ['a', 'c', 'missing', 'inherited']),
          util.pick([{ a: 1, b: 2 }, { a: 3 }], ['a']),
          util.pick({ x: 1, y: 2 }, 'y'),
        ];"#,
    )
    .await;
    assert_eq!(
        r.json(),
        json!([{"a": 1, "c": 3}, [{"a": 1}, {"a": 3}], {"y": 2}])
    );
    let r = exec("try { util.pick(null, ['a']); } catch (e) { return e.name; }").await;
    assert_eq!(r.json(), json!("TypeError"));
}
