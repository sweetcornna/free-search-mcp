//! OAuth-backed agent-search engines (`codex` and `antigravity`, ruling R5).

pub mod antigravity;
pub mod codex;

pub use antigravity::AntigravityEngine;
pub use codex::CodexEngine;
