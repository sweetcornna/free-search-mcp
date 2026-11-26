//! The one error type every crate returns (`SPEC-contracts.md` §1.6, frozen in W0a).
//!
//! No variant may be added after W0a (`#[non_exhaustive]` keeps downstream
//! `match`es future-proof anyway). No field is named `source`: thiserror
//! reserves that name for the error-chain source.

/// Every failure a facade, source, sandbox or host call can report.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FsmError {
    /// The caller's arguments are wrong; the message names the fix.
    #[error("{0}")]
    InvalidInput(String),
    /// SSRF guard refused the URL.
    #[error("Unsafe URL blocked: {0}")]
    UnsafeUrl(String),
    /// A response body (wire or decompressed) passed the byte cap.
    #[error("Response exceeded {0} byte limit")]
    MaxBytesExceeded(u64),
    /// A keyed source was called without its key.
    #[error("Authentication required for {source_id}: set {env_var}")]
    AuthRequired { source_id: String, env_var: String },
    /// Upstream said 429 (or equivalent).
    #[error("Rate limited by {source_id}; retry after {retry_after_s:?}s")]
    RateLimited {
        source_id: String,
        retry_after_s: Option<u64>,
    },
    /// A deadline passed.
    #[error("Timed out after {0}ms")]
    Timeout(u64),
    /// A sandbox or per-call budget ran out (heap, host calls, downloads, PDF queue, ...).
    #[error("Sandbox limit exceeded: {0}")]
    BudgetExceeded(String),
    /// The exec script does not parse.
    #[error("JS syntax error at {line}:{col}: {message}\n  {snippet}")]
    JsSyntax {
        line: u32,
        col: u32,
        message: String,
        snippet: String,
    },
    /// The exec script threw.
    #[error("JS runtime error: {0}")]
    JsRuntime(String),
    /// Policy refused the call (taint rule, raw-HTTP kill switch, DOCUMENT_ROOT, ...).
    #[error("Permission denied: {0}")]
    PermissionDenied(String),
    /// Browser rendering requested but unavailable (1.0 ships the stub only).
    #[error("Browser unavailable: {0}")]
    BrowserUnavailable(String),
    /// Upstream returned an error page, an unparsable body or an error status.
    #[error("Upstream error ({source_id}): {message}")]
    Upstream {
        source_id: String,
        status: Option<u16>,
        message: String,
    },
    /// A server bug, never the caller's fault.
    #[error("Internal error ({kind}): {message}")]
    Internal { kind: &'static str, message: String },
}

impl FsmError {
    /// `FsmError::InvalidInput(msg)`.
    pub fn invalid(msg: impl Into<String>) -> Self {
        Self::InvalidInput(msg.into())
    }

    /// `FsmError::Upstream { .. }`.
    pub fn upstream(
        source_id: impl Into<String>,
        status: Option<u16>,
        message: impl Into<String>,
    ) -> Self {
        Self::Upstream {
            source_id: source_id.into(),
            status,
            message: message.into(),
        }
    }

    /// `FsmError::Internal { .. }`.
    pub fn internal(kind: &'static str, message: impl Into<String>) -> Self {
        Self::Internal {
            kind,
            message: message.into(),
        }
    }

    /// Stable variant name, for logs and the server's
    /// `internal error (<Kind>)` rendering.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::InvalidInput(_) => "InvalidInput",
            Self::UnsafeUrl(_) => "UnsafeUrl",
            Self::MaxBytesExceeded(_) => "MaxBytesExceeded",
            Self::AuthRequired { .. } => "AuthRequired",
            Self::RateLimited { .. } => "RateLimited",
            Self::Timeout(_) => "Timeout",
            Self::BudgetExceeded(_) => "BudgetExceeded",
            Self::JsSyntax { .. } => "JsSyntax",
            Self::JsRuntime(_) => "JsRuntime",
            Self::PermissionDenied(_) => "PermissionDenied",
            Self::BrowserUnavailable(_) => "BrowserUnavailable",
            Self::Upstream { .. } => "Upstream",
            Self::Internal { .. } => "Internal",
        }
    }

    /// True for the one variant that is a server bug rather than an
    /// anticipated, actionable condition (`SPEC.md` §2.5.4).
    pub fn is_internal(&self) -> bool {
        matches!(self, Self::Internal { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_strings_match_spec() {
        assert_eq!(FsmError::invalid("bad").to_string(), "bad");
        assert_eq!(
            FsmError::AuthRequired {
                source_id: "nasa_ads".into(),
                env_var: "SEARCH_MCP_ADS_API_KEY".into()
            }
            .to_string(),
            "Authentication required for nasa_ads: set SEARCH_MCP_ADS_API_KEY"
        );
        assert_eq!(
            FsmError::RateLimited {
                source_id: "s2".into(),
                retry_after_s: Some(5)
            }
            .to_string(),
            "Rate limited by s2; retry after Some(5)s"
        );
        assert_eq!(
            FsmError::Timeout(1500).to_string(),
            "Timed out after 1500ms"
        );
        assert_eq!(
            FsmError::JsSyntax {
                line: 2,
                col: 7,
                message: "variable name expected".into(),
                snippet: "let = ;".into()
            }
            .to_string(),
            "JS syntax error at 2:7: variable name expected\n  let = ;"
        );
        assert_eq!(
            FsmError::upstream("arxiv", Some(503), "busy").to_string(),
            "Upstream error (arxiv): busy"
        );
        assert_eq!(
            FsmError::internal("registry", "dup").to_string(),
            "Internal error (registry): dup"
        );
    }

    #[test]
    fn kind_names_are_stable() {
        assert_eq!(FsmError::MaxBytesExceeded(1).kind(), "MaxBytesExceeded");
        assert_eq!(FsmError::internal("x", "y").kind(), "Internal");
        assert!(FsmError::internal("x", "y").is_internal());
        assert!(!FsmError::Timeout(1).is_internal());
    }
}
