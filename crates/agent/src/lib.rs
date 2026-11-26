//! `fsm-agent` — Quick `ask` tool (fast + agent modes), answer agent, OAuth PKCE /
//! keystore / CLI login, and `codex` / `antigravity` search engines (lane `W7`, ruling R5).

pub mod agent;
pub mod engines;
pub mod fast;
pub mod keystore;
pub mod login;
pub mod oauth;
pub mod passage;

use async_trait::async_trait;
use engines::{AntigravityEngine, CodexEngine};
use fast::FastConfig;
use fsm_core::{
    AskApi, AskMode, AskOpts, AskResponse, CallCx, Config, DocApi, FetchApi, FsmError, Registry,
    SearchAgentSummary, SearchApi, SearchResponse, SourceDeps,
};
use oauth::OAuthManager;
use std::sync::Arc;

/// Register `codex` and `antigravity` OAuth-backed agent-search engines in `registry`.
pub fn register_agent_engines(
    reg: &mut Registry,
    deps: &SourceDeps,
    is_http_transport: bool,
) -> Result<(), FsmError> {
    let oauth = Arc::new(OAuthManager::from_config(&deps.cfg));
    reg.register(Arc::new(CodexEngine::new(
        Arc::clone(&deps.cfg),
        Arc::clone(&oauth),
        is_http_transport,
    )))?;
    reg.register(Arc::new(AntigravityEngine::new(
        Arc::clone(&deps.cfg),
        oauth,
    )))?;
    Ok(())
}

/// Service implementing [`fsm_core::AskApi`] for `ask`, `web.ask`, CLI `ask`,
/// HTTP `/v1/ask`, and `agent_in_search`.
pub struct AskService {
    search: Arc<dyn SearchApi>,
    fetch: Arc<dyn FetchApi>,
    doc: Arc<dyn DocApi>,
    registry: Arc<Registry>,
    cfg: Arc<Config>,
    fast_cfg: FastConfig,
}

impl AskService {
    pub fn new(
        search: Arc<dyn SearchApi>,
        fetch: Arc<dyn FetchApi>,
        doc: Arc<dyn DocApi>,
        registry: Arc<Registry>,
        cfg: Arc<Config>,
    ) -> Self {
        Self {
            search,
            fetch,
            doc,
            registry,
            cfg,
            fast_cfg: FastConfig::default(),
        }
    }

    pub fn with_fast_config(mut self, fast_cfg: FastConfig) -> Self {
        self.fast_cfg = fast_cfg;
        self
    }
}

#[async_trait]
impl AskApi for AskService {
    async fn ask(
        &self,
        question: &str,
        opts: AskOpts,
        cx: &CallCx,
    ) -> Result<AskResponse, FsmError> {
        let q = question.trim();
        if q.is_empty() {
            return Err(FsmError::invalid("question must not be empty"));
        }

        let effective_mode = match opts.mode {
            AskMode::Fast => AskMode::Fast,
            AskMode::Agent => AskMode::Agent,
            AskMode::Auto => {
                if agent::is_agent_configured(&self.cfg) {
                    AskMode::Agent
                } else {
                    AskMode::Fast
                }
            }
            _ => AskMode::Fast,
        };

        match effective_mode {
            AskMode::Fast => {
                let ev = fast::run_fast_ask(
                    q,
                    &opts,
                    &self.search,
                    &self.fetch,
                    &self.registry,
                    &self.cfg,
                    &self.fast_cfg,
                    cx,
                )
                .await?;
                Ok(ev.response)
            }
            AskMode::Agent => {
                agent::run_agent_ask(
                    q,
                    &opts,
                    &self.search,
                    &self.fetch,
                    &self.doc,
                    &self.registry,
                    &self.cfg,
                    &self.fast_cfg,
                    cx,
                )
                .await
            }
            _ => unreachable!(),
        }
    }

    async fn answer_search(
        &self,
        question: &str,
        search: &SearchResponse,
        cx: &CallCx,
    ) -> Option<SearchAgentSummary> {
        agent::answer_alongside_search(
            question,
            search,
            &self.search,
            &self.fetch,
            &self.doc,
            &self.cfg,
            cx,
        )
        .await
    }
}
