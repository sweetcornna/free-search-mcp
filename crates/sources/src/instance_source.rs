//! Protocol instance wrapper (`InstanceSource`: adapts a `ProtocolAdapter` + `InstanceDecl`
//! into a registered `dyn Source` per A1 §D.2 & §D.3).

use fsm_core::manifest::InstanceDecl;
use fsm_core::{
    FsmError, Hit, HitKind, ProtocolAdapter, ReturnKind, ScholarlyWork, Source, SourceDecl, SrcCx,
    SrcOutput,
};
use serde_json::{Map, Value};
use std::sync::Arc;

/// Wraps one `[[instance]]` entry over its registered [`ProtocolAdapter`].
pub struct InstanceSource {
    inst: InstanceDecl,
    decl: SourceDecl,
    adapter: Arc<dyn ProtocolAdapter>,
}

impl std::fmt::Debug for InstanceSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstanceSource")
            .field("id", &self.decl.id)
            .field("protocol", &self.adapter.kind())
            .finish_non_exhaustive()
    }
}

impl InstanceSource {
    /// Validate `inst` against `adapter` (`validate_dialect`, `probe.op`, `default_params` keys)
    /// and build the [`InstanceSource`].
    pub fn new(inst: InstanceDecl, adapter: Arc<dyn ProtocolAdapter>) -> Result<Self, FsmError> {
        adapter.validate_dialect(&inst.dialect)?;

        let adapter_ops = adapter.ops();
        if !adapter_ops.is_empty() {
            if let Some(probe_op) = inst.probe.op.as_deref()
                && !adapter_ops.iter().any(|o| o.name == probe_op)
            {
                return Err(FsmError::invalid(format!(
                    "instance {:?} ({}) probe.op {:?} is not declared by adapter",
                    inst.id,
                    adapter.kind(),
                    probe_op
                )));
            }
            for k in inst.default_params.keys() {
                let valid = adapter_ops
                    .iter()
                    .any(|o| o.params.iter().any(|p| p.name == *k));
                if !valid {
                    return Err(FsmError::invalid(format!(
                        "instance {:?} ({}) default_params key {k:?} is not a param of any adapter op",
                        inst.id,
                        adapter.kind()
                    )));
                }
            }
        }

        let decl = inst.to_source_decl(adapter_ops);
        Ok(Self {
            inst,
            decl,
            adapter,
        })
    }

    fn default_hit_kind(&self) -> HitKind {
        if self
            .inst
            .scopes
            .iter()
            .any(|s| s == "academic" || s.starts_with("academic."))
        {
            HitKind::Academic
        } else {
            HitKind::Record
        }
    }
}

#[async_trait::async_trait]
impl Source for InstanceSource {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    async fn call(&self, op: &str, params: Value, cx: &SrcCx) -> Result<SrcOutput, FsmError> {
        // A1 §D.2 #1: shallow-merge default_params underneath caller params (caller wins).
        let mut merged = Map::new();
        for (k, v) in &self.inst.default_params {
            merged.insert(k.clone(), v.clone());
        }
        match params {
            Value::Object(caller) => {
                for (k, v) in caller {
                    merged.insert(k, v);
                }
            }
            Value::Null => {}
            _ => {
                return Err(FsmError::invalid(format!(
                    "{}.{op}: params must be a JSON object",
                    self.inst.id
                )));
            }
        }

        // A1 §D.2 #5: clone instance dialect into SrcCx.
        let mut inst_cx = cx.clone();
        inst_cx.dialect = self.inst.dialect.clone();

        let out = self
            .adapter
            .call(&self.inst.base_url, op, Value::Object(merged), &inst_cx)
            .await
            .map_err(|err| remap_protocol_source_id(err, self.adapter.kind(), &self.inst.id))?;

        let returns = self.decl.op(op).map_or(ReturnKind::Records, |o| o.returns);
        let default_kind = self.default_hit_kind();

        // A1 §D.3: output projection.
        match returns {
            ReturnKind::Hits => {
                let mut hits = Vec::with_capacity(out.items.len());
                for item in out.items {
                    let has_kind = item.get("kind").is_some_and(|k| !k.is_null());
                    let mut h: Hit = serde_json::from_value(item).map_err(|e| {
                        FsmError::upstream(
                            &self.inst.id,
                            None,
                            format!(
                                "protocol {} op {op} item not a valid Hit: {e}",
                                self.adapter.kind()
                            ),
                        )
                    })?;
                    if h.source.is_empty() {
                        h.source = self.inst.id.clone();
                    }
                    if h.sources.is_empty() {
                        h.sources = vec![self.inst.id.clone()];
                    }
                    if !has_kind {
                        h.kind = default_kind;
                    }
                    hits.push(h);
                }
                Ok(SrcOutput::Hits(hits))
            }
            ReturnKind::Works => {
                let mut works = Vec::with_capacity(out.items.len());
                for item in out.items {
                    let mut w: ScholarlyWork = serde_json::from_value(item).map_err(|e| {
                        FsmError::upstream(
                            &self.inst.id,
                            None,
                            format!(
                                "protocol {} op {op} item not a valid ScholarlyWork: {e}",
                                self.adapter.kind()
                            ),
                        )
                    })?;
                    if !w.sources.iter().any(|s| s == &self.inst.id) {
                        w.sources.push(self.inst.id.clone());
                    }
                    works.push(w);
                }
                Ok(SrcOutput::Works(works))
            }
            ReturnKind::Records => {
                let hits = if op == "search" {
                    let synthesized: Vec<Hit> = out
                        .items
                        .iter()
                        .filter_map(|rec| synthesize_hit(rec, &self.inst.id, default_kind))
                        .collect();
                    Some(synthesized)
                } else {
                    None
                };
                Ok(SrcOutput::Records {
                    items: out.items,
                    hits,
                    next_cursor: out.next_cursor,
                })
            }
            ReturnKind::Text => {
                let text = out
                    .items
                    .into_iter()
                    .map(|v| match v {
                        Value::String(s) => s,
                        other => other.to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(SrcOutput::Text(text))
            }
            ReturnKind::Raw | _ => Ok(SrcOutput::Raw(Value::Array(out.items))),
        }
    }
}

fn remap_protocol_source_id(err: FsmError, adapter_kind: &str, inst_id: &str) -> FsmError {
    match err {
        FsmError::RateLimited {
            source_id,
            retry_after_s,
        } if source_id == adapter_kind => FsmError::RateLimited {
            source_id: inst_id.to_owned(),
            retry_after_s,
        },
        FsmError::Upstream {
            source_id,
            status,
            message,
        } if source_id == adapter_kind => FsmError::Upstream {
            source_id: inst_id.to_owned(),
            status,
            message,
        },
        other => other,
    }
}

fn synthesize_hit(rec: &Value, source_id: &str, kind: HitKind) -> Option<Hit> {
    let obj = rec.as_object()?;
    let title = obj
        .get("title")
        .or_else(|| obj.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let url = obj
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    if title.is_empty() && url.is_empty() {
        return None;
    }
    let snippet = obj
        .get("snippet")
        .or_else(|| obj.get("description"))
        .or_else(|| obj.get("summary"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let date = obj
        .get("date")
        .or_else(|| obj.get("publishedDate"))
        .or_else(|| obj.get("modified"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let mut h = Hit::new(title, url);
    h.snippet = snippet;
    h.date = date;
    h.date_source = h.date.as_ref().map(|_| "api".to_owned());
    h.source = source_id.to_owned();
    h.sources = vec![source_id.to_owned()];
    h.kind = kind;
    Some(h)
}
