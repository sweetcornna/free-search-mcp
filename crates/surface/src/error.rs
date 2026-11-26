//! Error formatting and truncation/budget notices (`SPEC.md` §2.5.3–§2.5.4).

use crate::sanitize::escape_delimiters;
use fsm_core::FsmError;

/// Render an [`FsmError`] per the `SPEC.md` §2.5.4 error contract:
/// - Anticipated errors name what failed, why, and what to try next.
/// - [`FsmError::AuthRequired`] points away from asking the user for keys.
/// - [`FsmError::Internal`] emits the fixed non-leaking bug message.
pub fn render_error(err: &FsmError) -> String {
    match err {
        FsmError::InvalidInput(msg) => {
            let clean = escape_delimiters(msg.trim());
            if clean.is_empty() {
                "Invalid input. Check the required parameter names and types via `catalog.describe` or tool schema.".to_owned()
            } else {
                format!("Invalid input: {clean}")
            }
        }
        FsmError::UnsafeUrl(msg) => {
            let clean = escape_delimiters(msg.trim());
            format!(
                "Unsafe URL blocked ({clean}). Only public http(s) URLs are allowed; verify the URL or pick a public source."
            )
        }
        FsmError::MaxBytesExceeded(limit) => format!(
            "Response exceeded the {limit} byte limit. Use `doc.read` with start/length pagination, narrow the query, or fetch a smaller resource."
        ),
        FsmError::AuthRequired { source_id, env_var } => {
            let src = escape_delimiters(source_id.trim());
            let env = escape_delimiters(env_var.trim());
            format!(
                "{src} not configured (optional keyed source). Nothing is wrong with the search: omit `sources` or pick keyless alternatives via `search({{query, scope:\"catalog\"}})`. Do not ask the user for an API key. (Operator note: set {env} to enable {src}.)"
            )
        }
        FsmError::RateLimited {
            source_id,
            retry_after_s,
        } => {
            let src = escape_delimiters(source_id.trim());
            match retry_after_s {
                Some(secs) => format!(
                    "Rate limited by {src} (retry after {secs}s). Wait briefly, retry with other sources, or find alternatives via `search({{query, scope:\"catalog\"}})`."
                ),
                None => format!(
                    "Rate limited by {src}. Retry later, switch to other sources, or find alternatives via `search({{query, scope:\"catalog\"}})`."
                ),
            }
        }
        FsmError::Timeout(0) => {
            "Cancelled before completion. Re-run the call if you still need the result.".to_owned()
        }
        FsmError::Timeout(ms) => format!(
            "Timed out after {ms}ms. Batch independent calls with `Promise.all`, do less per call, or raise `timeout_s` (max 120)."
        ),
        FsmError::BudgetExceeded(msg) => {
            let clean = escape_delimiters(msg.trim());
            format!(
                "Limit exceeded: {clean}. Reduce work per call, batch calls with `Promise.all`, or return a smaller summary."
            )
        }
        FsmError::JsSyntax {
            line,
            col,
            message,
            snippet,
        } => {
            let msg = escape_delimiters(message.trim());
            let snip = escape_delimiters(snippet);
            format!(
                "JS syntax error at {line}:{col}: {msg}\n  {snip}\nFix the syntax: pass `async () => {{ ... }}`, statements ending in `return`, or a single expression."
            )
        }
        FsmError::JsRuntime(msg) => {
            let clean = escape_delimiters(msg.trim());
            format!(
                "JS runtime error: {clean}. Check the failing line, inspect values with `console.log`, or verify shapes via `catalog.describe`."
            )
        }
        FsmError::PermissionDenied(msg) => {
            let clean = escape_delimiters(msg.trim());
            format!(
                "Permission denied: {clean}. Remove the disallowed call, or run outbound requests in a separate `exec` that does not read local documents."
            )
        }
        FsmError::BrowserUnavailable(msg) => {
            let clean = escape_delimiters(msg.trim());
            format!(
                "Browser rendering unavailable: {clean}. Retry with `render: \"http\"` or fetch an alternative source."
            )
        }
        FsmError::Upstream {
            source_id,
            status,
            message,
        } => {
            let src = escape_delimiters(source_id.trim());
            let msg = escape_delimiters(message.trim());
            match status {
                Some(code) => format!(
                    "Upstream error from {src} (HTTP {code}): {msg}. Retry with other sources or rephrase the query."
                ),
                None => format!(
                    "Upstream error from {src}: {msg}. Retry with other sources or rephrase the query."
                ),
            }
        }
        FsmError::Internal { kind, .. } => format!(
            "internal error ({kind}) - a server bug, not a problem with your arguments; retrying the same call will not help. Details are in the server log."
        ),
        other => {
            let clean = escape_delimiters(&other.to_string());
            format!("{clean}. Adjust the arguments or check `catalog.search` for alternatives.")
        }
    }
}

/// Canonical overflow notice line for an `exec:<id>` handle (`SPEC.md` §2.5.3).
pub fn truncation_notice(handle: &str) -> String {
    let key = normalize_exec_handle(handle);
    format!("Full result saved: cache.get(\"{key}\")")
}

/// Nonced overflow notice line (`SPEC.md` §2.5.2, I14.3).
pub fn truncation_notice_with_nonce(handle: &str, nonce: &str) -> String {
    let key = normalize_exec_handle(handle);
    let n = nonce.trim();
    if n.is_empty() {
        truncation_notice(&key)
    } else {
        format!("Full result saved [{n}]: cache.get(\"{key}\")")
    }
}

fn normalize_exec_handle(handle: &str) -> String {
    let trimmed = handle.trim();
    if trimmed.starts_with("exec:") {
        trimmed.to_owned()
    } else {
        format!("exec:{trimmed}")
    }
}

/// Non-error reply when `SEARCH_MCP_TOOL_CALL_BUDGET` is exhausted (`SPEC.md` §2.5.4).
pub fn tool_budget_notice(budget: u32) -> String {
    let plural = if budget == 1 { "" } else { "s" };
    format!(
        "Tool budget used up ({budget} call{plural}). Do not call another tool. Answer now from the pages you already have, and list what you could not confirm."
    )
}
