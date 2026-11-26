//! Lane `W7` extension helpers (ruling R5: agent search lane coordination).
//!
//! Shared between `fsm-agent` (codex/antigravity engines) and `fsm-search`
//! (agent lane orchestration) without adding crate cycles or new `FsmError`
//! variants (`SPEC-contracts.md` M8).

use crate::error::FsmError;
use std::future::Future;

tokio::task_local! {
    static IN_AGENT_LANE: bool;
}

/// Run `fut` marked as executing inside `fsm-search`'s automatic agent lane.
///
/// Inside this scope, `codex` never auto-opens a browser sign-in window.
pub async fn scope_agent_lane<F: Future>(fut: F) -> F::Output {
    IN_AGENT_LANE.scope(true, fut).await
}

/// True when the current async task is executing inside the automatic agent lane.
pub fn in_agent_lane() -> bool {
    IN_AGENT_LANE.try_with(|&v| v).unwrap_or(false)
}

/// Prefix tag placed in `FsmError::Upstream.message` for lasting agent-engine
/// failures (usage limit, revoked sign-in, pinned model refused, 403 license)
/// that should bench the lane engine on 1 strike without shortening cache TTL.
pub const AGENT_LASTING_TAG: &str = "[agent:lasting] ";

/// Build an upstream error marked as a lasting (1-strike) agent-engine failure.
pub fn agent_lasting_error(
    source_id: impl Into<String>,
    status: Option<u16>,
    message: impl AsRef<str>,
) -> FsmError {
    FsmError::upstream(
        source_id,
        status,
        format!("{AGENT_LASTING_TAG}{}", message.as_ref()),
    )
}

/// Strip the internal lasting marker before presenting a message to callers.
pub fn strip_agent_lasting_tag(msg: &str) -> &str {
    msg.strip_prefix(AGENT_LASTING_TAG).unwrap_or(msg)
}

/// True when `err` represents a lasting agent-engine failure (usage limit,
/// revoked/missing sign-in, pinned model refused, 403 license) that should
/// bench on the first strike rather than waiting for 2 transient strikes.
pub fn is_lasting_agent_failure(err: &FsmError) -> bool {
    match err {
        FsmError::AuthRequired { .. } => true,
        FsmError::Upstream { message, .. } => message.starts_with(AGENT_LASTING_TAG),
        _ => false,
    }
}

/// Built-in agent-search engine ids and whether they participate in `"auto"`.
pub const BUILTIN_AGENT_ENGINES: &[(&str, bool)] = &[("codex", true), ("antigravity", false)];

/// True if `id` is a known built-in agent-search engine (`codex` or `antigravity`).
pub fn is_builtin_agent_engine(id: &str) -> bool {
    BUILTIN_AGENT_ENGINES.iter().any(|(name, _)| *name == id)
}

/// True if `id` is enabled when `SEARCH_MCP_AGENT_ENGINES=auto` (codex only).
pub fn is_builtin_agent_lane_auto(id: &str) -> bool {
    BUILTIN_AGENT_ENGINES
        .iter()
        .any(|(name, auto)| *name == id && *auto)
}
