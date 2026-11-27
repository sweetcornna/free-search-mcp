//! Explicit, operator-configured Degoog JSON search.

use crate::instance_search::InstanceSearch;
use async_trait::async_trait;
use fsm_core::{FsmError, Source, SourceDecl, SourceDeps, SrcCx, SrcOutput};
use serde_json::Value;

pub struct DegoogEngine(InstanceSearch);

impl DegoogEngine {
    /// `base_url` must come from trusted operator configuration, never request parameters.
    pub fn new(deps: SourceDeps, base_url: String) -> Self {
        Self(InstanceSearch::new(deps, base_url, true))
    }
}

#[async_trait]
impl Source for DegoogEngine {
    fn decl(&self) -> &SourceDecl {
        &self.0.decl
    }

    async fn call(&self, op: &str, params: Value, cx: &SrcCx) -> Result<SrcOutput, FsmError> {
        self.0.call(op, params, cx).await
    }
}
