//! Resource limits and isolation. Test names follow the Spike C property
//! cases (D/spikes/core/code/ws/c1-qjs/src/main.rs).

use crate::mock::*;
use fsm_core::FsmError;
use fsm_script::{ExecValue, Limits, Sandbox};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

async fn times_out(code: &str) {
    let t = Instant::now();
    let r = exec_with(code, limits(300), call_cx(10_000)).await;
    assert!(
        matches!(r.error(), FsmError::Timeout(300)),
        "{code}: {:?}",
        r.error()
    );
    assert!(
        t.elapsed() < Duration::from_secs(5),
        "{code}: took {:?}",
        t.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_async_sleep_echo() {
    let r = exec("async () => { const v = await test.sleepEcho(20, {a: 1}); return { v }; }").await;
    assert_eq!(r.json(), json!({"v": {"a": 1}}));
    assert_eq!(r.out.host_calls, 1);
    assert_eq!(r.mock.completed.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_loop_sync() {
    times_out("async () => { while (true) {} }").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_loop_after_await() {
    times_out("async () => { await test.sleepEcho(10, 0); while (true) {} }").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_loop_try_catch_swallow() {
    times_out("async () => { for (;;) { try { while (true) {} } catch (e) {} } }").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finally_continue_swallow() {
    times_out("async () => { for (;;) { try { while (true) {} } finally { continue; } } }").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn microtask_flood() {
    times_out("async () => { const f = () => Promise.resolve().then(f); f(); await new Promise(() => {}); }").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn await_loop() {
    times_out("async () => { for (;;) await null; }").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn catastrophic_regex() {
    times_out("async () => /(a+)+$/.test('a'.repeat(40) + 'b')").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_loop_in_set_timeout_callback() {
    times_out(
        "async () => { setTimeout(() => { while (true) {} }, 1); await new Promise(() => {}); }",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hanging_await_host_future_dropped() {
    let r = exec_with(
        "async () => { await test.hang(); }",
        limits(300),
        call_cx(10_000),
    )
    .await;
    assert!(
        matches!(r.error(), FsmError::Timeout(300)),
        "{:?}",
        r.error()
    );
    // The spawned host future is aborted when the sandbox drops it.
    let until = Instant::now() + Duration::from_secs(3);
    while r.mock.dropped.load(Ordering::SeqCst) == 0 && Instant::now() < until {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(r.mock.started.load(Ordering::SeqCst), 1);
    assert_eq!(r.mock.dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deadline_from_call_cx_wins_when_sooner() {
    let r = exec_with(
        "async () => { while (true) {} }",
        limits(10_000),
        call_cx(250),
    )
    .await;
    assert!(
        matches!(r.error(), FsmError::Timeout(ms) if (1..=250).contains(ms)),
        "{:?}",
        r.error()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_token_stops_a_busy_loop() {
    let cx = call_cx(10_000);
    let cancel = cx.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
    });
    let t = Instant::now();
    let r = exec_with("async () => { while (true) {} }", limits(10_000), cx).await;
    assert!(matches!(r.error(), FsmError::Timeout(0)), "{:?}", r.error());
    assert!(t.elapsed() < Duration::from_secs(5));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_token_stops_a_hanging_host_call() {
    let cx = call_cx(10_000);
    let cancel = cx.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
    });
    let r = exec_with("async () => { await test.hang(); }", limits(10_000), cx).await;
    assert!(matches!(r.error(), FsmError::Timeout(0)), "{:?}", r.error());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_run_future_stops_the_script() {
    let mock = Arc::new(Mock::default());
    let cx = Arc::new(call_cx(10_000));
    let sb = sandbox();
    let lim = limits(10_000);
    let fut = sb.run(
        "async () => { await test.hang(); }",
        mock.clone(),
        cx.clone(),
        &lim,
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(200), fut)
            .await
            .is_err()
    );
    let until = Instant::now() + Duration::from_secs(3);
    while mock.dropped.load(Ordering::SeqCst) == 0 && Instant::now() < until {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        mock.dropped.load(Ordering::SeqCst),
        1,
        "host call aborted after the caller went away"
    );
    assert!(
        !cx.cancel.is_cancelled(),
        "the caller's own token is left alone"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn global_exec_slots_are_bounded() {
    let sb = Sandbox::new(HOST_API).unwrap().with_max_concurrent_execs(1);
    let (m1, m2) = (Arc::new(Mock::default()), Arc::new(Mock::default()));
    let lim = limits(1500);
    let first = sb.run(
        "async () => { await test.hang(); }",
        m1,
        Arc::new(call_cx(1500)),
        &lim,
    );
    let second = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        sb.run("async () => 1", m2, Arc::new(call_cx(300)), &limits(300))
            .await
    };
    let (a, b) = tokio::join!(first, second);
    assert!(matches!(a.result, Err(ref f) if matches!(f.error, FsmError::Timeout(_))));
    assert!(
        matches!(b.result, Err(ref f) if matches!(f.error, FsmError::BudgetExceeded(ref m) if m.contains("exec calls"))),
        "{:?}",
        b.result
    );
}

fn small_heap() -> Limits {
    let mut l = limits(5000);
    l.heap_bytes = 16 << 20;
    l
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_push_arrays() {
    let r = exec_with(
        "async () => { const a = []; for (;;) a.push(new Array(1e5).fill(1.5)); }",
        small_heap(),
        call_cx(5000),
    )
    .await;
    assert!(
        matches!(r.error(), FsmError::BudgetExceeded(m) if m.contains("16 MiB JS heap")),
        "{:?}",
        r.error()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_default_heap_is_96_mib() {
    let r = exec_with(
        "async () => { const a = []; for (;;) a.push(new Array(1e5).fill(1.5)); }",
        limits(10_000),
        call_cx(10_000),
    )
    .await;
    assert!(
        matches!(r.error(), FsmError::BudgetExceeded(m) if m.contains("96 MiB JS heap")),
        "{:?}",
        r.error()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_arraybuffer_2gb() {
    let r = exec_with(
        "async () => new ArrayBuffer(2 ** 31).byteLength",
        small_heap(),
        call_cx(5000),
    )
    .await;
    assert!(
        r.out.result.is_err(),
        "a 2 GiB buffer must not be allocated"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_string_doubling() {
    let r = exec_with(
        "async () => { let s = 'x'; for (;;) s += s; }",
        small_heap(),
        call_cx(5000),
    )
    .await;
    assert!(
        matches!(
            r.error(),
            FsmError::BudgetExceeded(_) | FsmError::JsRuntime(_)
        ),
        "{:?}",
        r.error()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_catch_oom_and_continue() {
    let r = exec_with(
        "async () => { try { const a = []; for (;;) a.push(new Array(1e5).fill(1.5)); } catch (e) { return 'caught: ' + e; } }",
        small_heap(),
        call_cx(5000),
    )
    .await;
    // Either the script recovers or the heap stays exhausted; never a crash.
    match &r.out.result {
        Ok(ExecValue::Text(t)) => assert!(t.starts_with("caught"), "{t}"),
        Err(f) => assert!(
            matches!(f.error, FsmError::BudgetExceeded(_)),
            "{:?}",
            f.error
        ),
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stack_recursion() {
    let r = exec("async () => { const f = (n) => f(n + 1) + 1; return f(0); }").await;
    assert!(
        matches!(r.error(), FsmError::JsRuntime(m) if m.contains("RangeError")),
        "{:?}",
        r.error()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deeply_nested_source_100k() {
    let src = format!("async () => {}{}", "[".repeat(100_000), "]".repeat(100_000));
    let r = exec(&src).await;
    assert!(r.out.result.is_err(), "the parser must refuse, not crash");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deeply_nested_return_100k() {
    let r =
        exec("async () => { let o = {}; for (let i = 0; i < 100000; i++) o = { o }; return o; }")
            .await;
    assert!(
        matches!(r.error(), FsmError::JsRuntime(m) if m.contains("not JSON-serializable")),
        "{:?}",
        r.error()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deeply_nested_return_200() {
    // JSON.stringify copes, serde_json's 128-level limit does not: an explicit error.
    let r = exec("async () => { let o = {}; for (let i = 0; i < 200; i++) o = { o }; return o; }")
        .await;
    assert!(
        matches!(r.error(), FsmError::InvalidInput(m) if m.contains("deeper than 128")),
        "{:?}",
        r.error()
    );
    let ok = exec("async () => { let o = {}; for (let i = 0; i < 100; i++) o = { o }; return o; }")
        .await;
    assert!(matches!(ok.value(), ExecValue::Json(_)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn atomics_wait() {
    let r = exec("async () => Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0)").await;
    assert!(
        matches!(r.error(), FsmError::JsRuntime(m) if m.contains("TypeError")),
        "{:?}",
        r.error()
    );
    let r = exec_with(
        "async () => Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 3000)",
        limits(500),
        call_cx(5000),
    )
    .await;
    assert!(r.out.result.is_err());
    assert!(
        r.out.elapsed < Duration::from_secs(3),
        "never blocks the thread"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ambient_probe() {
    let r = exec(
        r#"async () => ({
          require: typeof require, process: typeof process, fetch: typeof fetch, Deno: typeof Deno,
          std: typeof std, os: typeof os, XMLHttpRequest: typeof XMLHttpRequest, WebSocket: typeof WebSocket,
          WebAssembly: typeof WebAssembly, importScripts: typeof importScripts, setInterval: typeof setInterval,
          __fsm_host: typeof __fsm_host, __fsm_log: typeof __fsm_log, __fsm_url_key: typeof __fsm_url_key,
          setTimeout: typeof setTimeout, clearTimeout: typeof clearTimeout, console: typeof console,
          HostError: typeof HostError, web: typeof web, util: typeof util,
          evalRequire: eval('typeof require'), fnProcess: Function('return typeof process')(),
        })"#,
    )
    .await;
    let v = r.json();
    for k in [
        "require",
        "process",
        "fetch",
        "Deno",
        "std",
        "os",
        "XMLHttpRequest",
        "WebSocket",
        "WebAssembly",
        "importScripts",
        "setInterval",
        "__fsm_host",
        "__fsm_log",
        "__fsm_url_key",
        "evalRequire",
        "fnProcess",
    ] {
        assert_eq!(v[k], "undefined", "{k}");
    }
    for k in ["setTimeout", "clearTimeout", "HostError"] {
        assert_eq!(v[k], "function", "{k}");
    }
    for k in ["console", "web", "util"] {
        assert_eq!(v[k], "object", "{k}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dynamic_import_fs() {
    let r = exec("async () => { const m = await import('fs'); return Object.keys(m); }").await;
    assert!(
        matches!(r.error(), FsmError::JsRuntime(_)),
        "{:?}",
        r.error()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dynamic_import_data_url() {
    let r = exec("async () => { const m = await import('data:text/javascript,export default 1'); return m.default; }").await;
    assert!(
        matches!(r.error(), FsmError::JsRuntime(_)),
        "{:?}",
        r.error()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn static_import_syntax() {
    let r = exec("import fs from 'fs';\nreturn 1;").await;
    assert!(
        matches!(r.error(), FsmError::JsSyntax { .. }),
        "{:?}",
        r.error()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pollute_prototype_then_check_next_call() {
    let first = exec("async () => { Object.prototype.polluted = 1; globalThis.leak = 2; Array.prototype.push = null; return 'set'; }").await;
    assert_eq!(first.json(), json!("set"));
    let second = exec(
        "async () => ({ polluted: typeof ({}).polluted, leak: typeof leak, push: typeof [].push })",
    )
    .await;
    assert_eq!(
        second.json(),
        json!({"polluted": "undefined", "leak": "undefined", "push": "function"})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn namespaces_frozen() {
    let r = exec(
        r#"async () => {
          const out = [];
          try { web.search = () => 1; out.push('mutated'); } catch (e) { out.push(e.name); }
          try { web = 1; out.push('replaced'); } catch (e) { out.push(e.name); }
          try { console.log = null; out.push('console'); } catch (e) { out.push(e.name); }
          out.push(Object.isFrozen(web), Object.isFrozen(util), typeof util.sleep, typeof util.grep);
          return out;
        }"#,
    )
    .await;
    assert_eq!(
        r.json(),
        json!([
            "TypeError",
            "TypeError",
            "TypeError",
            true,
            true,
            "function",
            "function"
        ])
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn return_values_follow_json_stringify() {
    for (code, want) in [
        (
            "async () => ({ a: 1, b: [1, 'x', null], c: { d: true } })",
            json!({"a": 1, "b": [1, "x", null], "c": {"d": true}}),
        ),
        ("async () => [1, 2, 3]", json!([1, 2, 3])),
        ("async () => 42", json!(42)),
        (
            "async () => ({ d: new Date(0), m: new Map([[1, 2]]), f: () => 1, nan: NaN })",
            json!({"d": "1970-01-01T00:00:00.000Z", "m": {}, "nan": null}),
        ),
    ] {
        assert_eq!(exec(code).await.json(), want, "{code}");
    }
    assert!(matches!(
        exec("async () => { }").await.value(),
        ExecValue::Undefined
    ));
    assert!(matches!(
        exec("async () => Symbol('x')").await.value(),
        ExecValue::Undefined
    ));
    let circ = exec("async () => { const a = {}; a.self = a; return a; }").await;
    assert!(
        matches!(circ.error(), FsmError::JsRuntime(m) if m.contains("not JSON-serializable")),
        "{:?}",
        circ.error()
    );
    let big = exec("async () => 10n ** 30n").await;
    assert!(
        matches!(big.error(), FsmError::JsRuntime(m) if m.contains("BigInt")),
        "{:?}",
        big.error()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn console_capture() {
    let r = exec("async () => { console.log('hello', {a: 1}, [1, 2]); console.error('bad'); console.warn('careful'); console.debug(3); return 1; }").await;
    assert_eq!(
        r.out.console,
        [
            "hello {\"a\":1} [1,2]",
            "error: bad",
            "warn: careful",
            "debug: 3"
        ]
    );
    assert!(!r.out.console_truncated);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn log_flood_100k_lines() {
    let r = exec(
        "async () => { for (let i = 0; i < 100000; i++) console.log('line ' + i); return 'done'; }",
    )
    .await;
    assert_eq!(r.json(), json!("done"));
    assert!(r.out.console_truncated);
    let bytes: usize = r.out.console.iter().map(|l| l.len() + 1).sum();
    assert!(bytes <= 2048, "{bytes}");
    assert_eq!(r.out.console[0], "line 0");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn result_over_payload_cap() {
    let r = exec("async () => 'x'.repeat(5 * 1024 * 1024)").await;
    assert!(
        matches!(r.error(), FsmError::BudgetExceeded(m) if m.contains("4 MiB")),
        "{:?}",
        r.error()
    );
    let ok = exec("async () => 'x'.repeat(3 * 1024 * 1024)").await;
    assert!(matches!(ok.value(), ExecValue::Text(t) if t.len() == 3 << 20));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_payloads_over_cap() {
    let r = exec("async () => { const s = await test.big(5); return s.length; }").await;
    assert!(
        matches!(r.error(), FsmError::BudgetExceeded(m) if m.contains("test.big returned")),
        "{:?}",
        r.error()
    );
    let r = exec("async () => test.echo('x'.repeat(5 * 1024 * 1024))").await;
    assert!(
        matches!(r.error(), FsmError::InvalidInput(m) if m.contains("arguments to test.echo")),
        "{:?}",
        r.error()
    );
    assert_eq!(r.mock.started.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_code_is_invalid_input() {
    let r = exec("  \n ").await;
    assert!(
        matches!(r.error(), FsmError::InvalidInput(m) if m.contains("code is empty")),
        "{:?}",
        r.error()
    );
}
