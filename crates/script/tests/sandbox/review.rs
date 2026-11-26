//! Review round 1 regressions (D/handoff/w0d-review.md). Each test is named
//! after its finding, uses the reviewer's failing input, and failed before
//! the fix (see the handoff's "Review round 1" section).

use crate::mock::*;
use fsm_core::text::estimate_tokens;
use fsm_core::url_canon::url_key;
use fsm_core::{FsmError, NullStore, RenderKind};
use fsm_script::{ExecValue, JsonRenderer, Limits, ReplyOptions, Sandbox, compose_reply};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn heap(mib: usize, timeout_ms: u64) -> Limits {
    let mut l = limits(timeout_ms);
    l.heap_bytes = mib << 20;
    l
}

fn kind(r: &Run) -> Option<RenderKind> {
    match r.value() {
        ExecValue::Tagged { kind, .. } => Some(*kind),
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b2_oom_while_delivering_a_host_result_fails_fast() {
    let t = Instant::now();
    let r = exec_with(
        "const hold = new Uint8Array(14 << 20);\nreturn (await test.big(3)).length + hold.length;",
        heap(16, 5000),
        call_cx(5000),
    )
    .await;
    assert!(
        matches!(r.error(), FsmError::BudgetExceeded(m) if m.contains("16 MiB JS heap")),
        "{:?}",
        r.error()
    );
    assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b4_wrapper_breakout_runs_once_as_runtime_code() {
    let r = exec(r#"1)); await test.echo("ran"); throw new SyntaxError("boom"); ((1"#).await;
    assert!(
        matches!(r.error(), FsmError::JsRuntime(m) if m.starts_with("SyntaxError: boom")),
        "{:?}",
        r.error()
    );
    assert_eq!(r.mock.calls.lock().unwrap().len(), 1);
    assert_eq!(r.out.host_calls, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b4_breakout_tampering_with_then_is_not_a_server_bug() {
    let r = exec("}); Promise.prototype.then = null; (() => {").await;
    assert_eq!(r.value(), &ExecValue::Undefined);
    let r = exec("}); Promise.prototype.then = null; await test.echo(1); (() => {").await;
    assert_eq!(r.value(), &ExecValue::Undefined);
    assert_eq!(r.out.host_calls, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b4_breakout_host_error_keeps_its_variant() {
    let r = exec("}); await test.rateLimited(); (() => {").await;
    assert!(
        matches!(r.error(), FsmError::RateLimited { .. }),
        "{:?}",
        r.error()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b4_ts_retry_reports_out_of_memory_not_a_stale_syntax_error() {
    // Both JS candidates stop at the annotation; the stripped source parses
    // but its bytecode does not fit the heap.
    let code = format!(
        "const a: number[] = [{}];\nreturn a.length;",
        "1,".repeat(600_000)
    );
    let r = exec_with(&code, heap(4, 30_000), call_cx(30_000)).await;
    assert!(
        matches!(r.error(), FsmError::BudgetExceeded(m) if m.contains("JS heap")),
        "{:?}",
        r.error()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn i1_custom_null_keys_fall_back_to_json() {
    let fetch = "const p = await web.fetch('https://example.com/a');\n";
    let r = exec(&format!("{fetch}p.myNote = null;\nreturn p;")).await;
    assert_eq!(kind(&r), None);
    assert!(r.json().as_object().unwrap().contains_key("myNote"));
    let r =
        exec("const b = await web.research('q');\nb.pages[0].extracted = null;\nreturn b;").await;
    assert_eq!(kind(&r), None);
    assert_eq!(r.json()["pages"][0]["extracted"], Value::Null);
    let r = exec(&format!("{fetch}p.author = null;\nreturn p;")).await;
    assert_eq!(kind(&r), Some(RenderKind::PageResult), "known Option field");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn i2_cancel_while_waiting_for_an_exec_slot() {
    let sb = Sandbox::new(HOST_API).unwrap().with_max_concurrent_execs(1);
    let busy_cx = Arc::new(call_cx(3000));
    let (busy_limits, wait_limits) = (limits(3000), limits(10_000));
    let busy = sb.run(
        "while (true) {}",
        Arc::new(Mock::default()),
        busy_cx.clone(),
        &busy_limits,
    );
    let waiting = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let cx = Arc::new(call_cx(10_000));
        let cancel = cx.cancel.clone();
        let canceller = async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel.cancel();
        };
        let t = Instant::now();
        let (out, ()) = tokio::join!(
            sb.run("return 1;", Arc::new(Mock::default()), cx, &wait_limits),
            canceller
        );
        busy_cx.cancel.cancel();
        (out, t.elapsed())
    };
    let (_, (out, waited)) = tokio::join!(busy, waiting);
    assert!(
        matches!(out.result, Err(ref f) if matches!(f.error, FsmError::Timeout(0))),
        "{:?}",
        out.result
    );
    assert!(waited < Duration::from_secs(1), "{waited:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn i2_a_pre_cancelled_call_never_runs() {
    let cx = call_cx(5000);
    cx.cancel.cancel();
    let r = exec_with("await test.echo(1);\nreturn 1;", limits(5000), cx).await;
    assert!(matches!(r.error(), FsmError::Timeout(0)), "{:?}", r.error());
    assert!(r.mock.calls.lock().unwrap().is_empty());
    assert_eq!(r.out.host_calls, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn i4_spoofed_interrupt_and_oom_messages_stay_script_errors() {
    for msg in ["interrupted", "out of memory"] {
        let r = exec(&format!("throw new Error({msg:?});")).await;
        assert!(
            matches!(r.error(), FsmError::JsRuntime(m) if m.starts_with(&format!("Error: {msg}"))),
            "{:?}",
            r.error()
        );
        let r = exec(&format!("await test.failWith({msg:?});")).await;
        assert!(
            matches!(r.error(), FsmError::InvalidInput(m) if m.contains(msg)),
            "{:?}",
            r.error()
        );
    }
    let r = exec(r#"const e = new Error("interrupted"); e.name = "InternalError"; throw e;"#).await;
    assert!(
        matches!(r.error(), FsmError::JsRuntime(m) if m.starts_with("InternalError: interrupted")),
        "{:?}",
        r.error()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn i4_errors_inside_to_json_are_runtime_errors() {
    let r = exec(r#"return { toJSON() { JSON.parse("{"); } };"#).await;
    assert!(
        matches!(r.error(), FsmError::JsRuntime(m) if m.contains("SyntaxError")),
        "{:?}",
        r.error()
    );
    let r = exec(
        "let e;\ntry { await test.rateLimited(); } catch (x) { e = x; }\nreturn { toJSON() { throw e; } };",
    )
    .await;
    assert!(
        matches!(r.error(), FsmError::RateLimited { .. }),
        "{:?}",
        r.error()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn i4_thrown_value_with_a_looping_to_json_times_out() {
    let r = exec_with(
        "const o = { toJSON() { while (true) {} } };\nthrow o;",
        limits(200),
        call_cx(5000),
    )
    .await;
    assert!(
        matches!(r.error(), FsmError::Timeout(200)),
        "{:?}",
        r.error()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn i4_an_expired_deadline_is_a_timeout_not_a_cancel() {
    let r = exec_with("return 1;", limits(5000), call_cx(0)).await;
    assert!(
        matches!(r.error(), FsmError::Timeout(ms) if *ms >= 1),
        "{:?}",
        r.error()
    );
    let rep = compose_reply(
        &r.out,
        &JsonRenderer,
        &NullStore::new(),
        &ReplyOptions::new(None, "k7m9"),
    )
    .await;
    assert!(rep.text().starts_with("Timed out"), "{}", rep.text());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn i5_trailing_semicolon_and_comment_keep_the_expression_form() {
    assert_eq!(
        exec("async () => { return 42; }; // done").await.json(),
        json!(42)
    );
    assert_eq!(
        exec("await test.echo(1); // fetch page").await.json(),
        json!([1])
    );
    assert_eq!(
        exec("test.echo('https://example.com/a;b'); /* c */ // d")
            .await
            .json(),
        json!(["https://example.com/a;b"])
    );
    assert_eq!(exec("test.echo(1); // a; b").await.json(), json!([1]));
    assert_eq!(exec("test.echo(1); /* a;\nb */").await.json(), json!([1]));
    assert_eq!(
        exec(r#"test.echo("a; // b"); // trailing comment"#)
            .await
            .json(),
        json!(["a; // b"])
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m1_nul_byte_is_invalid_input() {
    let r = exec("return 'a\u{0}b';").await;
    assert!(
        matches!(r.error(), FsmError::InvalidInput(m) if m.contains("NUL")),
        "{:?}",
        r.error()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m2_dedupe_scheme_relative_padded_and_tampered_regexp() {
    let base = url_key("https://example.com/b");
    assert_eq!(url_key("//example.com/b"), base);
    assert_eq!(url_key(" https://example.com/b "), base);
    let r = exec(
        r#"return util.dedupe(["//example.com/b", "https://example.com/b", " https://example.com/b "]);"#,
    )
    .await;
    assert_eq!(r.json(), json!(["//example.com/b"]));
    let r = exec(
        "RegExp.prototype.test = () => false;\nreturn util.dedupe(['https://Example.com/a/', 'http://www.example.com/a']);",
    )
    .await;
    assert_eq!(r.json(), json!(["https://Example.com/a/"]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_snapshot_ignores_array_prototype_accessors() {
    let r = exec(
        r#"const real = await web.fetch('https://example.com/a');
const forged = { ...real, title: 'FORGED' };
Object.defineProperty(Array.prototype, '0', { set() {}, get() { return forged; }, configurable: true });
const a = [];
Object.defineProperty(a, '0', { value: real, enumerable: true, writable: true, configurable: true });
return a;"#,
    )
    .await;
    assert_eq!(kind(&r), Some(RenderKind::PageList));
    let v = r.json();
    assert_eq!(
        v[0]["title"],
        json!("Title of https://example.com/a"),
        "{v}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_pick_copies_a_proto_key_as_data() {
    let r =
        exec(r#"return util.pick(JSON.parse('{"__proto__":{"a":1},"b":2}'), ["__proto__", "b"]);"#)
            .await;
    assert_eq!(r.json(), json!({"__proto__": {"a": 1}, "b": 2}));
}

// ---- "Missing tests" from the review ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fulltext_and_citation_graph_render_end_to_end() {
    for (code, want) in [
        (
            "return await test.fulltext();",
            Some(RenderKind::FulltextResult),
        ),
        (
            "return await test.graph();",
            Some(RenderKind::CitationGraph),
        ),
        (
            "return (await test.graph()).paper;",
            Some(RenderKind::WorkList),
        ),
        (
            "return (await test.graph()).references;",
            Some(RenderKind::WorkList),
        ),
        (
            "return (await test.graph()).citations.filter((w) => w.year === 2020);",
            Some(RenderKind::WorkList),
        ),
        (
            "const g = await test.graph();\ng.paper.myNote = 1;\nreturn g;",
            None,
        ),
        (
            "const f = await test.fulltext();\nf.myNote = null;\nreturn f;",
            None,
        ),
    ] {
        assert_eq!(kind(&exec(code).await), want, "{code}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grep_caps_at_500_snippets() {
    let r = exec(r#"return util.grep("a\n".repeat(1000), "a").length;"#).await;
    assert_eq!(r.json(), json!(500));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_panic_is_an_internal_host_panic() {
    let r = exec("await test.panic();").await;
    assert!(
        matches!(
            r.error(),
            FsmError::Internal {
                kind: "host_panic",
                ..
            }
        ),
        "{:?}",
        r.error()
    );
    let r = exec("try { await test.panic(); } catch (e) { return [e.name, e.kind]; }").await;
    assert_eq!(r.json(), json!(["HostError", "Internal"]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b3_huge_thrown_errors_are_clipped() {
    for code in [
        "throw new Error('x'.repeat(10 << 20));",
        "const e = new Error('a');\ne.name = 'x'.repeat(10 << 20);\nthrow e;",
    ] {
        let r = exec(code).await;
        assert!(
            matches!(r.error(), FsmError::JsRuntime(m) if m.chars().count() < 1200),
            "{} chars",
            r.error().to_string().len()
        );
        let rep = compose_reply(
            &r.out,
            &JsonRenderer,
            &NullStore::new(),
            &ReplyOptions::new(Some(500), "k7m9"),
        )
        .await;
        assert!(estimate_tokens(&rep.text()) <= 500);
    }
}
