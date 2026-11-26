//! Host bridge: Promise.all concurrency, budgets, setTimeout shim, host errors
//! and line numbers.

use crate::mock::*;
use fsm_core::FsmError;
use serde_json::json;
use std::sync::atomic::Ordering;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrency_promise_all() {
    // Every call waits until 10 are in flight at once: passes only if they overlap.
    let r = exec("async () => Promise.all(Array.from({length: 10}, () => test.barrier(10, 4000)))")
        .await;
    assert_eq!(r.json(), json!([10, 10, 10, 10, 10, 10, 10, 10, 10, 10]));
    assert_eq!(r.mock.high_water.load(Ordering::SeqCst), 10);
    assert!(r.out.elapsed.as_millis() < 4000, "{:?}", r.out.elapsed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrency_cap_queues_extra_calls() {
    // Each call returns once 12 were in flight together; a broken cap would
    // let all 20 overlap.
    let r = exec("async () => (await Promise.all(Array.from({length: 20}, () => test.barrier(12, 4000)))).length").await;
    assert_eq!(r.json(), json!(20));
    assert!(r.out.elapsed.as_millis() < 4000, "{:?}", r.out.elapsed);
    assert_eq!(
        r.mock.high_water.load(Ordering::SeqCst),
        12,
        "12 in flight, the rest queued"
    );
    assert_eq!(r.out.host_calls, 20);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_call_budget() {
    let mut cx = call_cx(5000);
    cx.max_host_calls = 3;
    let r = exec_with(
        "async () => { for (let i = 0; i < 5; i++) await test.echo(i); }",
        limits(5000),
        cx,
    )
    .await;
    assert!(
        matches!(r.error(), FsmError::BudgetExceeded(m) if m.contains("budget of 3")),
        "{:?}",
        r.error()
    );
    assert_eq!(r.mock.started.load(Ordering::SeqCst), 3);
    let mut cx = call_cx(5000);
    cx.max_host_calls = 1;
    let r = exec_with(
        "async () => { await test.echo(1); try { await test.echo(2); } catch (e) { return [e.name, e.kind]; } }",
        limits(5000),
        cx,
    )
    .await;
    assert_eq!(r.json(), json!(["HostError", "BudgetExceeded"]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_timeout_shim_counts_and_fires() {
    let r = exec(
        r#"async () => {
          const order = [];
          await new Promise((resolve) => { setTimeout((x) => { order.push(x); resolve(); }, 20, 'fired'); });
          const id = setTimeout(() => order.push('cleared'), 5);
          clearTimeout(id);
          await util.sleep(40);
          return order;
        }"#,
    )
    .await;
    assert_eq!(r.json(), json!(["fired"]));
    assert_eq!(
        r.out.host_calls, 3,
        "two timers + one util.sleep, all charged"
    );
    assert_eq!(
        r.mock.started.load(Ordering::SeqCst),
        0,
        "util.sleep never reaches HostServices"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_timeout_is_bounded_by_the_deadline() {
    // A huge delay neither panics nor outlives the wall clock.
    let r = exec_with(
        "async () => { await new Promise((r) => setTimeout(r, 1e300)); return 1; }",
        limits(300),
        call_cx(5000),
    )
    .await;
    assert!(
        matches!(r.error(), FsmError::Timeout(300)),
        "{:?}",
        r.error()
    );
    let r = exec("async () => { setTimeout(() => {}, 1e12); await util.sleep(-5); await util.sleep('x'); return 'done'; }").await;
    assert_eq!(r.json(), json!("done"));
    let r =
        exec("async () => { try { setTimeout('code', 1); } catch (e) { return e.name; } }").await;
    assert_eq!(r.json(), json!("TypeError"));
    let mut cx = call_cx(5000);
    cx.max_host_calls = 0;
    let r = exec_with(
        "async () => { setTimeout(() => {}, 1); await new Promise((r) => r()); return 1; }",
        limits(5000),
        cx,
    )
    .await;
    assert_eq!(r.json(), json!(1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_timeout_callback_errors_go_to_console() {
    let r = exec("async () => { setTimeout(() => { throw new Error('cb boom'); }, 1); await util.sleep(30); return 1; }").await;
    assert_eq!(r.json(), json!(1));
    assert!(
        r.out.console.iter().any(|l| l.contains("cb boom")),
        "{:?}",
        r.out.console
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_args_are_positional_json() {
    let r = exec("async () => { await test.echo('q', {limit: 3, f: () => 1}, undefined, [1n].length); return 1; }").await;
    assert_eq!(r.json(), json!(1));
    let calls = r.mock.calls.lock().unwrap().clone();
    assert_eq!(
        calls,
        [("test.echo".to_owned(), json!(["q", {"limit": 3}, null, 1]))]
    );
    // SPEC.md §2.5.3: unserializable values (cycles, BigInt) raise InvalidInput.
    let r = exec(
        "async () => { try { await test.echo(10n); } catch (e) { return [e.name, e.kind, e.message]; } }",
    )
    .await;
    assert_eq!(
        r.json(),
        json!([
            "HostError",
            "InvalidInput",
            "the arguments to test.echo are not JSON-serializable (BigInt are forbidden in JSON.stringify); pass plain data without cycles or BigInt"
        ])
    );
    assert_eq!(r.out.host_calls, 0);
    let r = exec("const v = {};\nv.self = v;\nawait cache.put('k', v);").await;
    assert!(
        matches!(r.error(), FsmError::InvalidInput(m) if m.contains("arguments to cache.put are not JSON-serializable (circular reference)")),
        "{:?}",
        r.error()
    );
    assert_eq!(r.line(), Some(3));
    assert_eq!(r.mock.started.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_timeout_works_without_a_util_sleep_row() {
    const TABLE: &[fsm_core::HostFn] = &[fsm_core::HostFn::new("test.echo", "", "", "")];
    let sb = fsm_script::Sandbox::new(TABLE).unwrap();
    let mock = std::sync::Arc::new(Mock::default());
    let out = sb
        .run(
            "await new Promise((r) => setTimeout(r, 5, 'ok'));\nreturn typeof util;",
            mock,
            std::sync::Arc::new(call_cx(5000)),
            &limits(5000),
        )
        .await;
    assert_eq!(out.result.unwrap().raw_json(), json!("undefined"));
    assert_eq!(out.host_calls, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prelude_ignores_tampered_globals() {
    let r = exec(
        r#"async () => {
          JSON.parse = () => ({ ok: true, value: 'forged' });
          JSON.stringify = () => '["forged"]';
          Function.prototype.call = null;
          Promise.prototype.then = null;
          return await test.echo(1, 'two');
        }"#,
    )
    .await;
    assert_eq!(r.json(), json!([1, "two"]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exception_line_numbers() {
    let r =
        exec("async () => {\n  const x = 1;\n  throw new TypeError('boom on line 3');\n}").await;
    assert!(
        matches!(r.error(), FsmError::JsRuntime(m) if m.starts_with("TypeError: boom on line 3 (at line 3:")),
        "{:?}",
        r.error()
    );
    assert_eq!(r.line(), Some(3));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exception_throw_in_inner_fn() {
    let r = exec("async () => {\n  function inner() {\n    throw new Error('line 3');\n  }\n  return inner();\n}").await;
    assert_eq!(r.line(), Some(3));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exception_nested_fn_line_is_best_effort() {
    // QuickJS-NG reports engine errors at the enclosing function (S9).
    let r = exec(
        "async () => {\n  function inner() {\n    return null.prop;\n  }\n  return inner();\n}",
    )
    .await;
    assert!(
        matches!(r.error(), FsmError::JsRuntime(m) if m.starts_with("TypeError")),
        "{:?}",
        r.error()
    );
    assert!(matches!(r.line(), Some(2 | 3)), "{:?}", r.line());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exception_from_host_keeps_its_variant_and_line() {
    let r = exec("async () => {\n  const a = 1;\n  await test.fail('upstream 503');\n}").await;
    assert!(
        matches!(r.error(), FsmError::Upstream { source_id, status: Some(503), message } if source_id == "mock" && message == "upstream 503"),
        "{:?}",
        r.error()
    );
    assert_eq!(r.line(), Some(3), "the calling line");
    let r = exec("await test.rateLimited()").await;
    assert!(
        matches!(
            r.error(),
            FsmError::RateLimited {
                retry_after_s: Some(5),
                ..
            }
        ),
        "{:?}",
        r.error()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exception_host_caught_by_script() {
    let r = exec(
        "async () => { try { await test.fail('upstream 503'); } catch (e) { return { name: e.name, kind: e.kind, message: e.message, isError: e instanceof Error, isHost: e instanceof HostError, stack: e.stack.includes('exec.js:1'), prelude: e.stack.includes('fsm-prelude') }; } }",
    )
    .await;
    assert_eq!(
        r.json(),
        json!({"name": "HostError", "kind": "Upstream", "message": "Upstream error (mock): upstream 503", "isError": true, "isHost": true, "stack": true, "prelude": false})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rethrown_host_error_keeps_variant_but_wrapped_does_not() {
    let r = exec("async () => { try { await test.rateLimited(); } catch (e) { throw e; } }").await;
    assert!(matches!(r.error(), FsmError::RateLimited { .. }));
    let r = exec("async () => { try { await test.rateLimited(); } catch (e) { throw new Error('wrapped: ' + e.kind); } }").await;
    assert!(
        matches!(r.error(), FsmError::JsRuntime(m) if m.starts_with("Error: wrapped: RateLimited")),
        "{:?}",
        r.error()
    );
    // A forged HostError look-alike is just a JS error.
    let r = exec("async () => { const e = new Error('x'); e.name = 'HostError'; e.kind = 'RateLimited'; throw e; }").await;
    assert!(
        matches!(r.error(), FsmError::JsRuntime(m) if m.starts_with("HostError: x")),
        "{:?}",
        r.error()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn throw_non_error() {
    let r = exec("async () => { throw 'plain string'; }").await;
    assert!(
        matches!(r.error(), FsmError::JsRuntime(m) if m == "uncaught non-Error value thrown: \"plain string\""),
        "{:?}",
        r.error()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn syntax_error() {
    let r = exec("async () => {\n  let = ;\n}").await;
    match r.error() {
        FsmError::JsSyntax {
            line,
            col,
            message,
            snippet,
        } => {
            assert_eq!((*line, *col), (2, 7), "{message}");
            assert_eq!(snippet, "let = ;");
            assert!(!message.is_empty());
        }
        other => panic!("{other:?}"),
    }
    // Line 1 columns are mapped back past the wrapper.
    let r = exec("let = ;").await;
    assert!(
        matches!(
            r.error(),
            FsmError::JsSyntax {
                line: 1,
                col: 5,
                ..
            }
        ),
        "{:?}",
        r.error()
    );
    assert_eq!(r.out.host_calls, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_syntax_error_never_reruns_the_script() {
    let r = exec("async () => {\n  await test.echo(1);\n  return JSON.parse('{');\n}").await;
    assert!(
        matches!(r.error(), FsmError::JsRuntime(m) if m.starts_with("SyntaxError")),
        "{:?}",
        r.error()
    );
    assert_eq!(r.mock.started.load(Ordering::SeqCst), 1);
    assert!(!r.out.ts_stripped);
    let r = exec("await test.echo(1);\nnew Function('let = ;');").await;
    assert!(
        matches!(r.error(), FsmError::JsRuntime(m) if m.starts_with("SyntaxError")),
        "{:?}",
        r.error()
    );
    assert_eq!(r.mock.started.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unawaited_host_calls_are_reported() {
    // sleepEcho(1000) cannot settle before the script returns: the count is exact.
    let r = exec(
        "test.sleepEcho(1000, 0);\ntest.sleepEcho(1000, 1);\nconst t = setTimeout(() => {}, 1000);\nclearTimeout(t);\nsetTimeout(() => {}, 1000);\nreturn 1;",
    )
    .await;
    assert_eq!(r.json(), json!(1));
    assert_eq!(
        r.out.console,
        [
            "[exec] 2 host call(s) were still pending when the script returned and may not have completed; await them (e.g. with Promise.all)"
        ]
    );
    // Awaited calls and timers, fired, cleared or pending, add no note.
    let r = exec(
        "await cache.put('k', 1);\nawait new Promise((r) => setTimeout(r, 1));\nconst t = setTimeout(() => {}, 1000);\nclearTimeout(t);\nPromise.race([Promise.resolve(1), new Promise((r) => setTimeout(r, 1000))]);\nreturn 1;",
    )
    .await;
    assert_eq!(r.json(), json!(1));
    assert!(r.out.console.is_empty(), "{:?}", r.out.console);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_results_racing_the_deadline() {
    // The script reaches a busy loop while host results land (150..325 ms)
    // around the 300 ms deadline: they are resolved, and their reactions run,
    // after the interrupt handler has started firing. The run reports Timeout
    // and nothing panics.
    for _ in 0..3 {
        let r = exec_with(
            "const late = Array.from({length: 8}, (_, i) => test.sleepEcho(150 + 25 * i, i));\nlate.forEach((p) => p.then((v) => console.log('late ' + v)));\nawait test.sleepEcho(20, 'x');\nconsole.log('looping');\nwhile (true) {}",
            limits(300),
            call_cx(5000),
        )
        .await;
        assert!(
            matches!(r.error(), FsmError::Timeout(300)),
            "{:?}",
            r.error()
        );
        assert_eq!(r.out.console.first().map(String::as_str), Some("looping"));
        assert!(r.mock.completed.load(Ordering::SeqCst) >= 1);
    }
}
