//! Extension point of lane `W3c` — Scholar 11-Step Lawful Full-Text Chain (`fsm-scholar-fulltext`).
//!
//! Only that lane edits this file.

use crate::types::FulltextResult;
use serde::{Deserialize, Serialize};

/// Diagnostic note emitted when Step 7 (Unpaywall) is skipped because
/// `SEARCH_MCP_CONTACT_EMAIL` is unset (`SPEC.md` §6, I16).
pub const UNPAYWALL_SKIP_NOTE: &str =
    "Unpaywall skipped: set SEARCH_MCP_CONTACT_EMAIL to enable Unpaywall open-access resolution.";

/// Extended full-text outcome carrying diagnostic `notes` (such as the I16
/// Unpaywall skip hint) and the ordered trace of chain steps attempted.
///
/// `FulltextResult` in `fsm_core::types` is frozen at W0a without a `notes`
/// field; callers that want structured diagnostics alongside the wire
/// [`FulltextResult`] can call `ScholarFulltext::fulltext_report`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct FulltextReport {
    pub result: FulltextResult,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps_tried: Vec<String>,
}

impl FulltextReport {
    pub fn new(result: FulltextResult) -> Self {
        Self {
            result,
            notes: Vec::new(),
            steps_tried: Vec::new(),
        }
    }
}
