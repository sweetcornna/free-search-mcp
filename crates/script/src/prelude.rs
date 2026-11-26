//! Prelude generation from the `HOST_API` table (`SPEC.md` §3.1).
//!
//! `fsm-surface` owns the table; the sandbox only needs each row's name and
//! whether it is a prelude-sync helper. Async rows become
//! `(...args) => __host(name, args)` bindings; sync rows must name one of the
//! helpers implemented in `prelude.js` ([`PRELUDE_SYNC_FNS`]).

use fsm_core::{FsmError, HostFn};
use serde_json::{Value, json};

/// Helpers implemented in the JS prelude (`is_prelude_sync = true` rows).
pub const PRELUDE_SYNC_FNS: [&str; 4] = [
    "util.dedupe",
    "util.grep",
    "util.pick",
    "util.estimateTokens",
];

/// Async host functions the sandbox serves itself, never forwarding them to
/// `HostServices`. `util.sleep` is a timer clamped to the remaining wall
/// clock (the `setTimeout` shim uses it whether or not the table lists it).
pub const SANDBOX_NATIVE_FNS: [&str; 1] = ["util.sleep"];

const PRELUDE_JS: &str = include_str!("prelude.js");

/// File name of the prelude in stack traces (filtered from `HostError` stacks).
pub(crate) const PRELUDE_NAME: &str = "fsm-prelude.js";

fn valid_ident(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Prelude source for `host_api`: the IIFE plus its binding table.
pub(crate) fn build(host_api: &[HostFn]) -> Result<String, FsmError> {
    let mut rows: Vec<Value> = Vec::with_capacity(host_api.len());
    let mut seen = std::collections::HashSet::new();
    for f in host_api {
        let Some((ns, name)) = f.name.split_once('.') else {
            return Err(FsmError::internal(
                "host_api",
                format!("host function {:?} is not <namespace>.<name>", f.name),
            ));
        };
        if !valid_ident(ns) || !valid_ident(name) {
            return Err(FsmError::internal(
                "host_api",
                format!("host function {:?} has an invalid name", f.name),
            ));
        }
        if !seen.insert(f.name) {
            return Err(FsmError::internal(
                "host_api",
                format!("host function {:?} is listed twice", f.name),
            ));
        }
        if f.is_prelude_sync && !PRELUDE_SYNC_FNS.contains(&f.name) {
            return Err(FsmError::internal(
                "host_api",
                format!(
                    "{:?} is marked prelude-sync but the prelude implements only {PRELUDE_SYNC_FNS:?}",
                    f.name
                ),
            ));
        }
        if f.is_prelude_sync && f.egress {
            return Err(FsmError::internal(
                "host_api",
                format!("prelude-sync {:?} cannot be egress", f.name),
            ));
        }
        rows.push(json!([f.name, ns, name, f.is_prelude_sync]));
    }
    Ok(format!("{PRELUDE_JS}({});\n", Value::Array(rows)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_become_bindings_and_bad_rows_are_rejected() {
        const OK: &[HostFn] = &[
            HostFn::new("web.fetch", "", "", "").with_egress(true),
            HostFn::new("util.dedupe", "", "", "").with_prelude_sync(true),
        ];
        let src = build(OK).unwrap();
        assert!(src.ends_with("([[\"web.fetch\",\"web\",\"fetch\",false],[\"util.dedupe\",\"util\",\"dedupe\",true]]);\n"));
        let bad: [&[HostFn]; 5] = [
            &[HostFn::new("fetch", "", "", "")],
            &[HostFn::new("web.fe-tch", "", "", "")],
            &[
                HostFn::new("web.a", "", "", ""),
                HostFn::new("web.a", "", "", ""),
            ],
            &[HostFn::new("util.nope", "", "", "").with_prelude_sync(true)],
            &[HostFn::new("util.grep", "", "", "")
                .with_prelude_sync(true)
                .with_egress(true)],
        ];
        for rows in bad {
            assert!(
                matches!(build(rows), Err(FsmError::Internal { .. })),
                "{rows:?}"
            );
        }
    }
}
