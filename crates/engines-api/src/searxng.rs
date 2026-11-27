//! Explicit, operator-configured SearXNG JSON search.

use crate::instance_search::InstanceSearch;
use async_trait::async_trait;
use fsm_core::{FsmError, Source, SourceDecl, SourceDeps, SrcCx, SrcOutput};
use serde_json::Value;

pub struct SearxngEngine(InstanceSearch);

impl SearxngEngine {
    /// `base_url` must come from trusted operator configuration, never request parameters.
    pub fn new(deps: SourceDeps, base_url: String) -> Self {
        Self(InstanceSearch::new(deps, base_url, false))
    }
}

#[async_trait]
impl Source for SearxngEngine {
    fn decl(&self) -> &SourceDecl {
        &self.0.decl
    }

    async fn call(&self, op: &str, params: Value, cx: &SrcCx) -> Result<SrcOutput, FsmError> {
        self.0.call(op, params, cx).await
    }
}
