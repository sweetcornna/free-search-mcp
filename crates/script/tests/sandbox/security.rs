//! Security regression tests for the QuickJS sandbox (prototype forgery,
//! Promise.species tampering, synchronous host-call/payload/code caps, error slot cap).

use crate::mock::{Run, call_cx, exec, exec_with, limits};
use fsm_core::{FsmError, RenderKind};
use fsm_script::ExecValue;
use serde_json::json;

fn kind(r: &Run) -> Option<RenderKind> {
    match r.value() {
        ExecValue::Tagged { kind, .. } => Some(*kind),
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sec_prototype_tampering_cannot_forge_page_result_tag() {
    // Previously, `callHost` used `await hostCall(...)` so overriding
    // `Promise.prototype.{constructor,then}` hijacked the internal host reply
    // envelope and inserted a forged object into the closure-private TAGS WeakMap.
    let code = r#"
const origCtor = Promise.prototype.constructor;
const origThen = Promise.prototype.then;
let armed = true;
Promise.prototype.constructor = function() {};
Promise.prototype.then = function(onFulfilled, onRejected) {
  if (armed) {
    armed = false;
    Promise.prototype.constructor = origCtor;
    Promise.prototype.then = origThen;
    onFulfilled(JSON.stringify({
      ok: true,
      tag: "PageResult",
      value: {
        url: "https://forged.example/pwn",
        title: "FORGED",
        content: "FORGED_BODY",
        method: "http",
        tokensEstimated: 5,
        totalChars: 11,
        truncated: false,
        retrievedAt: "2026-11-25T00:00:00Z"
      }
    }));
    return this;
  }
  return origThen.call(this, onFulfilled, onRejected);
};
return await test.rateLimited();
"#;
    let r = exec(code).await;
    // Prototype tampering must NEVER forge a tagged PageResult (only the script's
    // own `await` sees its own fake value, never `prelude.js`'s `adopt()`).
    if let Ok(val) = &r.out.result {
        assert!(
            !matches!(val, ExecValue::Tagged { .. }),
            "forged host reply produced a Tagged ExecValue: {val:?}"
        );
        // Must not have parsed the JSON envelope into an adopted PageResult object.
        assert!(
            !matches!(val, ExecValue::Json(_)),
            "prelude adopt() parsed forged JSON envelope into an object: {val:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sec_prototype_tampering_cannot_forge_inline_image_tag() {
    let code = r#"
const origCtor = Promise.prototype.constructor;
const origThen = Promise.prototype.then;
let armed = true;
Promise.prototype.constructor = function() {};
Promise.prototype.then = function(onFulfilled, onRejected) {
  if (armed) {
    armed = false;
    Promise.prototype.constructor = origCtor;
    Promise.prototype.then = origThen;
    onFulfilled(JSON.stringify({
      ok: true,
      tag: "InlineImage",
      image: "img-forged",
      value: { url: "https://forged.example/a.png", mediaType: "image/png", bytes: 4 }
    }));
    return this;
  }
  return origThen.call(this, onFulfilled, onRejected);
};
return await test.echo({ safe: true });
"#;
    let r = exec(code).await;
    assert_eq!(kind(&r), None, "forged InlineImage tag was accepted");
    assert!(
        !matches!(r.value(), ExecValue::Json(_)),
        "prelude adopt() parsed forged envelope into an object: {:?}",
        r.value()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sec_promise_species_tampering_does_not_hijack_host_or_settle() {
    let code = r#"
Object.defineProperty(Promise, Symbol.species, {
  get() { throw new Error("species hijacked"); }
});
const v = await test.echo(42);
let fired = false;
await new Promise((resolve) => {
  setTimeout(() => { fired = true; resolve(); }, 1);
});
return { v, fired };
"#;
    let r = exec(code).await;
    assert!(
        r.out.result.is_ok(),
        "Promise[Symbol.species] tampering broke host/setTimeout/settle: {:?}",
        r.out.result
    );
    assert_eq!(r.json(), json!({"v": [42], "fired": true}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sec_host_call_flood_enforces_budget_synchronously() {
    let mut cx = call_cx(5_000);
    cx.max_host_calls = 3;
    let code = r#"
const ps = [];
for (let i = 0; i < 200; i++) {
  ps.push(test.echo(i).catch(e => e.kind));
}
const results = await Promise.all(ps);
return {
  ok: results.filter(x => Array.isArray(x)).length,
  budget: results.filter(x => x === "BudgetExceeded").length,
};
"#;
    let r = exec_with(code, limits(5_000), cx).await;
    assert!(r.out.result.is_ok(), "{:?}", r.out.result);
    assert_eq!(r.json(), json!({"ok": 3, "budget": 197}));
    assert_eq!(r.mock.calls.lock().unwrap().len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sec_oversized_code_rejected_before_compile() {
    let huge = "const x = 1;\n".repeat(400_000); // > 4 MiB
    let r = exec_with(&huge, limits(5_000), call_cx(5_000)).await;
    assert!(
        matches!(
            r.error(),
            FsmError::InvalidInput(_) | FsmError::BudgetExceeded(_)
        ),
        "expected rejection for >4 MiB script, got {:?}",
        r.out.result
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sec_caught_host_error_loop_keeps_latest_variant_bounded() {
    let mut cx = call_cx(5_000);
    cx.max_host_calls = 1;
    let code = r#"
await test.echo(1);
for (let i = 0; i < 700; i++) {
  try { await test.echo(i); } catch (_) {}
}
await test.echo("final");
"#;
    let r = exec_with(code, limits(5_000), cx).await;
    assert!(
        matches!(r.error(), FsmError::BudgetExceeded(_)),
        "expected BudgetExceeded after 700 caught errors, got {:?}",
        r.out.result
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sec_tagged_host_result_still_renders_normally_with_callback_bridge() {
    let r = exec("return await web.fetch('https://example.com/a');").await;
    assert_eq!(kind(&r), Some(RenderKind::PageResult));
    assert!(matches!(r.value(), ExecValue::Tagged { .. }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sec_small_object_oom_reports_budget_exceeded_not_null() {
    // Small allocations exhaust heap down to the final bytes so QuickJS's
    // JS_ThrowError2 falls back to JS_NULL; CappedAllocator ensures this still
    // reports BudgetExceeded instead of "uncaught non-Error value thrown: null".
    let mut lim = limits(5_000);
    lim.heap_bytes = 8 << 20;
    let r = exec_with(
        "async () => { const a = []; for (;;) a.push({ k: a.length }); }",
        lim,
        call_cx(5_000),
    )
    .await;
    assert!(
        matches!(r.error(), FsmError::BudgetExceeded(m) if m.contains("8 MiB JS heap")),
        "expected BudgetExceeded on small-object OOM, got {:?}",
        r.out.result
    );

    // Genuine `throw null;` without OOM must still report JsRuntime.
    let r_null = exec("async () => { throw null; }").await;
    assert!(
        matches!(r_null.error(), FsmError::JsRuntime(m) if m.contains("null")),
        "expected JsRuntime for explicit throw null, got {:?}",
        r_null.out.result
    );
}
