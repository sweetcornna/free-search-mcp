//! `SourceDispatcher` — frozen §5.2 source and ad-hoc protocol dispatcher.

use fsm_core::{
    CallCx, FsmError, ProtocolOutput, Registry, SourceDeps, SourceOutput, SrcCx, SrcOutput,
};
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Source/protocol call dispatcher (delegated to by `fsm-server`'s `SourcesApi`).
pub struct SourceDispatcher {
    reg: Arc<Registry>,
    deps: SourceDeps,
}

impl std::fmt::Debug for SourceDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceDispatcher").finish_non_exhaustive()
    }
}

impl SourceDispatcher {
    pub fn new(reg: Arc<Registry>, deps: SourceDeps) -> Self {
        Self { reg, deps }
    }

    pub async fn call(
        &self,
        op_id: &str,
        params: Value,
        cx: &CallCx,
    ) -> Result<SourceOutput, FsmError> {
        if cx.cancel.is_cancelled() || cx.remaining().is_zero() {
            return Err(FsmError::Timeout(0));
        }

        let (src, op) = self.reg.resolve_op(op_id)?;
        let decl = src.decl();
        let started = Instant::now();

        let timeout = compute_timeout(
            cx.deadline,
            decl.limits.timeout_ms,
            self.deps.cfg.request_timeout_s,
        );
        let scx = SrcCx::from_config(&self.deps.cfg, cx.cancel.child_token(), timeout);

        let output = src.call(op, params, &scx).await?;
        let elapsed_ms = started.elapsed().as_millis() as u64;

        let mut res = SourceOutput::new(&decl.id, op);
        res.elapsed_ms = elapsed_ms;

        match output {
            SrcOutput::Hits(hits) => {
                res.items = hits
                    .into_iter()
                    .map(serde_json::to_value)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| FsmError::internal("serialize_hits", e.to_string()))?;
            }
            SrcOutput::Works(works) => {
                res.items = works
                    .into_iter()
                    .map(serde_json::to_value)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| FsmError::internal("serialize_works", e.to_string()))?;
            }
            SrcOutput::Records {
                items, next_cursor, ..
            } => {
                res.items = items;
                res.next_cursor = next_cursor;
            }
            SrcOutput::Text(t) => {
                res.text = Some(t);
            }
            SrcOutput::Raw(v) => {
                res.raw = Some(v);
            }
            _ => {}
        }

        Ok(res)
    }

    pub async fn protocol(
        &self,
        kind: &str,
        base_url: &str,
        op: &str,
        params: Value,
        cx: &CallCx,
    ) -> Result<ProtocolOutput, FsmError> {
        if cx.cancel.is_cancelled() || cx.remaining().is_zero() {
            return Err(FsmError::Timeout(0));
        }

        self.deps.net.check_url_offline(base_url)?;

        let adapter = self.reg.protocol(kind).ok_or_else(|| {
            let known: Vec<&str> = self.reg.protocols().map(|p| p.kind()).collect();
            FsmError::invalid(format!(
                "unknown protocol {kind:?}; available protocols: {}",
                known.join(", ")
            ))
        })?;

        let ops = adapter.ops();
        if !ops.is_empty() && !ops.iter().any(|o| o.name == op) {
            let names: Vec<&str> = ops.iter().map(|o| o.name.as_str()).collect();
            return Err(FsmError::invalid(format!(
                "protocol {kind:?} has no op {op:?}; available ops: {}",
                names.join(", ")
            )));
        }

        let timeout = compute_timeout(cx.deadline, 0, self.deps.cfg.request_timeout_s);
        let scx = SrcCx::from_config(&self.deps.cfg, cx.cancel.child_token(), timeout);
        adapter.call(base_url, op, params, &scx).await
    }
}

fn compute_timeout(deadline: Instant, limits_timeout_ms: u64, cfg_timeout_s: f64) -> Duration {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let base = if limits_timeout_ms > 0 {
        Duration::from_millis(limits_timeout_ms)
    } else if cfg_timeout_s > 0.0 && cfg_timeout_s.is_finite() {
        Duration::from_secs_f64(cfg_timeout_s)
    } else {
        Duration::from_secs(8)
    };
    base.min(remaining)
}
