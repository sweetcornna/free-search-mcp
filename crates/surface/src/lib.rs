//! `fsm-surface` — everything a model sees (`SPEC.md` §2–§3, lane `W6s`).
//!
//! Pure functions over `fsm-core` types (no I/O, no network, no async):
//! - [`HOST_API`] single-source host function table and [`dts`] TypeScript declarations;
//! - [`tool_defs`] (`search`, `exec`) and [`instructions`] within strict MCP byte budgets;
//! - [`render`], [`render_json`], [`render_error`], and [`truncation_notice`] for
//!   budget-bounded, delimiter-safe Markdown and JSON output.

mod error;
mod host_api;
mod render;
mod sanitize;
mod tools;

pub use error::{
    render_error, tool_budget_notice, truncation_notice, truncation_notice_with_nonce,
};
pub use host_api::{HOST_API, dts, host_fn, render_kind_for_host_fn};
pub use render::{
    DEFAULT_MAX_TOKENS, RenderOpts, render, render_cache_hits, render_integrity_report,
    render_json, render_untagged,
};
pub use sanitize::{escape_delimiters, invalid_fts_hint};
pub use tools::{ToolAnnotations, ToolDef, instructions, tool_defs, tools_wire_2024_11_05};
