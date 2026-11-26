//! normalize_code forms (SPEC.md §3.3) and the TypeScript strip retry (S10).

use crate::mock::*;
use fsm_core::FsmError;
use fsm_script::ExecValue;
use serde_json::json;
use std::sync::atomic::Ordering;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepted_forms() {
    for (code, want) in [
        ("1 + 1", json!(2)),
        ("async () => 41 + 1", json!(42)),
        ("() => 41 + 1", json!(42)),
        ("async () => { return 'block'; }", json!("block")),
        ("async function main() { return 'm'; }", json!("m")),
        ("function main() { return 'sync'; }", json!("sync")),
        ("await test.echo('x')", json!(["x"])),
        ("await test.echo(1);  ", json!([1])),
        ("const a = await test.echo(1);\nreturn a[0] + 1;", json!(2)),
        ("({a: 1})", json!({"a": 1})),
        ("{a: 1}", json!({"a": 1})),
        ("(async () => 3)()", json!(3)),
        ("test.echo('promise')", json!(["promise"])),
        ("```js\nconst v = 5;\nreturn v;\n```", json!(5)),
        ("```javascript\nasync () => 6\n```", json!(6)),
        (
            "web.fetch('https://e.com') // trailing comment",
            json!("https://e.com"),
        ),
    ] {
        let r = exec(code).await;
        let got = match r.value() {
            ExecValue::Tagged { value, .. } => value["url"].clone(),
            v => v.raw_json(),
        };
        assert_eq!(got, want, "{code}");
        assert!(!r.out.ts_stripped, "{code}");
    }
    let r = exec("const a = 1;\na + 1").await;
    assert!(
        matches!(r.value(), ExecValue::Undefined),
        "statements without return give undefined"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fenced_code_keeps_line_numbers() {
    let r = exec("```js\nconst a = 1;\nthrow new Error('line 3');\n```").await;
    assert_eq!(r.line(), Some(3));
}

const TS_SRC: &str = "async () => {
  interface Hit { title: string; url: string }
  const hits: Hit[] = (await test.echo({ title: 'a', url: 'u' })) as Hit[];
  const top = (hits as Array<Hit>).map((h: Hit): string => h.title);
  type R = { n: number; top: string[] };
  const out: R = { n: hits.length, top };
  if (out.n > 0) throw new Error('line 7 on purpose');
  return out satisfies R;
}";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ts_strip_preserves_line_numbers_in_sandbox() {
    // SPEC.md §12 item 5 (S10): the error raised on line 7 of TS source
    // reports line 7 after stripping.
    let r = exec(TS_SRC).await;
    assert!(r.out.ts_stripped);
    assert!(
        matches!(r.error(), FsmError::JsRuntime(m) if m.starts_with("Error: line 7 on purpose (at line 7:")),
        "{:?}",
        r.error()
    );
    assert_eq!(r.line(), Some(7));
    assert_eq!(r.mock.started.load(Ordering::SeqCst), 1, "ran exactly once");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ts_strip_runs_valid_typescript() {
    let ok = TS_SRC.replace("if (out.n > 0) throw new Error('line 7 on purpose');", "");
    let r = exec(&ok).await;
    assert!(r.out.ts_stripped);
    assert_eq!(r.json(), json!({"n": 1, "top": ["a"]}));
    let r = exec("const n: number = 2;\nreturn (n as number) * 2;").await;
    assert!(r.out.ts_stripped);
    assert_eq!(r.json(), json!(4));
    let r = exec("(x: number): number => x ?? 9").await;
    assert_eq!(r.json(), json!(9));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ts_strip_failure_reports_the_js_error() {
    let r = exec("const x: number = ;\nreturn x;").await;
    assert!(
        matches!(r.error(), FsmError::JsSyntax { line: 1, .. }),
        "{:?}",
        r.error()
    );
    assert!(!r.out.ts_stripped);
    // enum needs a transform, which strip-only refuses.
    let r = exec("enum E { A }\nreturn E.A;").await;
    assert!(
        matches!(r.error(), FsmError::JsSyntax { line: 1, .. }),
        "{:?}",
        r.error()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ts_runtime_syntax_error_runs_once() {
    let r = exec("const n: number = (await test.echo(1))[0];\nreturn JSON.parse('{' + n);").await;
    assert!(r.out.ts_stripped);
    assert!(
        matches!(r.error(), FsmError::JsRuntime(m) if m.starts_with("SyntaxError")),
        "{:?}",
        r.error()
    );
    assert_eq!(r.line(), Some(2));
    assert_eq!(r.mock.started.load(Ordering::SeqCst), 1);
}
