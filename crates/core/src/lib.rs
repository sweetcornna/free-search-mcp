//! `fsm-core`: the shared contracts of free-search-mcp 1.0
//! (`SPEC-contracts.md`): wire types, traits, errors, configuration, scope
//! vocabulary, manifest schema and small pure utilities. Every other crate
//! depends on this one; after W0a it changes only through `ext/<lane>.rs`.
//!
//! Conventions (M1, M2): public structs and enums are `#[non_exhaustive]`
//! with `Default` and/or constructors; wire types serialize camelCase and
//! omit `None` fields.

pub mod api;
pub mod config;
pub mod dates;
pub mod deps;
pub mod error;
pub mod ext;
pub mod fixture;
pub mod host;
pub mod manifest;
pub mod net;
pub mod scopes;
pub mod serde_compat;
pub mod source;
pub mod text;
pub mod types;
pub mod url_canon;

pub use api::*;
pub use config::{Config, RawHttpPolicy, SafeSearch, SsrfResolveMode};
pub use deps::*;
pub use error::*;
pub use host::*;
pub use net::*;
pub use scopes::{SCOPES, ScopeDef};
pub use source::*;
pub use types::*;

/// Re-exported so lanes can build request bodies and contexts without
/// declaring `bytes` / `tokio-util` themselves.
pub use bytes::Bytes;
pub use tokio_util::sync::CancellationToken;
