//! `xtask replay <file> [--op <op>] [--fixture <path>]` implementation.
//!
//! Runs each op of a declarative manifest OR protocol instance table against its recorded
//! fixture through the real `ManifestSource` / `InstanceSource` interpreter and prints the
//! mapped output as JSON.

use crate::validate::{build_protocol_registry, find_manifests_root, infer_wave};
use anyhow::{Context, Result, bail};
use fsm_core::fixture::ReplayNet;
use fsm_core::manifest::{InstanceFileDecl, ManifestDecl};
use fsm_core::{Config, NullStore, Source, SourceDeps, SrcCx, SrcOutput};
use fsm_sources::{InstanceSource, ManifestSource, OAuth2TokenCache};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug)]
pub struct ReplayOpReport {
    pub op: String,
    pub fixture: PathBuf,
    pub result_count: usize,
    pub min_results: usize,
    pub exchanges_matched: usize,
    pub output_json: Value,
}

/// Replay one or all probed ops of a declarative manifest or protocol instance table.
pub async fn run_replay(
    manifest_path: &Path,
    only_op: Option<&str>,
    fixture_override: Option<&Path>,
    wave_override: Option<&str>,
) -> Result<Vec<ReplayOpReport>> {
    let text = fs::read_to_string(manifest_path)
        .with_context(|| format!("cannot read {}", manifest_path.display()))?;
    let manifests_root = find_manifests_root(manifest_path);
    let wave = wave_override
        .map(str::to_owned)
        .or_else(|| infer_wave(manifest_path))
        .unwrap_or_else(|| "seed".to_owned());

    if let Ok(raw) = toml::from_str::<toml::Value>(&text)
        && raw
            .as_table()
            .and_then(|t| t.get("protocol"))
            .and_then(toml::Value::as_str)
            .is_some()
    {
        return replay_instance_file(
            manifest_path,
            &text,
            &manifests_root,
            &wave,
            only_op,
            fixture_override,
        )
        .await;
    }

    let decl = ManifestDecl::from_toml(&text)
        .with_context(|| format!("failed to parse manifest {}", manifest_path.display()))?;

    let ops_to_run: Vec<(&str, &fsm_core::manifest::OpSpec)> = match only_op {
        Some(name) => {
            let op = decl.op.get(name).with_context(|| {
                let avail: Vec<&str> = decl.op.keys().collect();
                format!(
                    "manifest {:?} has no op {name:?} (available: {})",
                    decl.id,
                    avail.join(", ")
                )
            })?;
            vec![(name, op)]
        }
        None => decl
            .op
            .iter()
            .filter(|(_, op)| op.probe.is_some())
            .collect(),
    };

    if ops_to_run.is_empty() {
        bail!("manifest {:?} has no ops with [op.<name>.probe]", decl.id);
    }

    let mut reports = Vec::new();
    let mut errors = Vec::new();

    for (op_name, op) in ops_to_run {
        let probe = op.probe.as_ref();
        let fixture_path = match fixture_override {
            Some(p) => p.to_path_buf(),
            None => {
                let rel = match probe {
                    Some(pr) => pr.fixture_path(&wave, &decl.id, op_name),
                    None => format!("fixtures/{wave}/{}/{op_name}/default.json", decl.id),
                };
                manifests_root.join(rel)
            }
        };

        let net = match ReplayNet::from_file(&fixture_path) {
            Ok(n) => Arc::new(n),
            Err(e) => {
                errors.push(format!(
                    "{}.{op_name}: failed to load fixture {}: {e}",
                    decl.id,
                    fixture_path.display()
                ));
                continue;
            }
        };

        let cfg = Arc::new(Config::default());
        let deps = SourceDeps::new(net.clone(), Arc::new(NullStore::new()), cfg.clone());
        let src = ManifestSource::new(decl.clone(), deps, Arc::new(OAuth2TokenCache::new()));

        let mut scx = SrcCx::from_config(
            &cfg,
            fsm_core::CancellationToken::new(),
            Duration::from_secs(15),
        );
        scx.replay_mode = true;
        scx.lang = Some("en".to_owned());

        let params = match probe {
            Some(pr) => Value::Object(pr.params.clone().into_iter().collect()),
            None => Value::Object(Default::default()),
        };
        let min_results = probe.map_or(1, |pr| pr.min_results);

        match src.call(op_name, params, &scx).await {
            Ok(output) => {
                let unmatched = net.unmatched();
                if !unmatched.is_empty() {
                    errors.push(format!(
                        "{}.{op_name}: unmatched requests: {}",
                        decl.id,
                        unmatched.join(", ")
                    ));
                    continue;
                }
                let (count, output_json) = serialize_src_output(&output);
                if count < min_results {
                    errors.push(format!(
                        "{}.{op_name}: got {count} results, expected min_results = {min_results}",
                        decl.id
                    ));
                    continue;
                }
                reports.push(ReplayOpReport {
                    op: op_name.to_owned(),
                    fixture: fixture_path,
                    result_count: count,
                    min_results,
                    exchanges_matched: net.hits().len(),
                    output_json,
                });
            }
            Err(e) => {
                errors.push(format!("{}.{op_name}: {e}", decl.id));
            }
        }
    }

    if !errors.is_empty() {
        bail!("{}", errors.join("\n"));
    }

    Ok(reports)
}

async fn replay_instance_file(
    manifest_path: &Path,
    text: &str,
    manifests_root: &Path,
    wave: &str,
    only_op: Option<&str>,
    fixture_override: Option<&Path>,
) -> Result<Vec<ReplayOpReport>> {
    let inst_file = InstanceFileDecl::from_toml(text)
        .with_context(|| format!("failed to parse instance table {}", manifest_path.display()))?;
    let mut reports = Vec::new();
    let mut errors = Vec::new();
    let cfg = Arc::new(Config::default());

    for inst in &inst_file.instance {
        let op_name = match only_op {
            Some(o) => o,
            None => match inst.probe.op.as_deref() {
                Some(o) => o,
                None => continue,
            },
        };
        let fixture_path = match fixture_override {
            Some(p) => p.to_path_buf(),
            None => manifests_root.join(inst.probe.fixture_path(wave, &inst.id, op_name)),
        };
        let net = match ReplayNet::from_file(&fixture_path) {
            Ok(n) => Arc::new(n),
            Err(e) => {
                errors.push(format!(
                    "{}.{op_name}: failed to load fixture {}: {e}",
                    inst.id,
                    fixture_path.display()
                ));
                continue;
            }
        };
        let reg = build_protocol_registry(net.clone(), Arc::clone(&cfg))?;
        let Some(adapter) = reg.protocol(&inst_file.protocol) else {
            errors.push(format!(
                "{}: unregistered protocol {:?}",
                inst.id, inst_file.protocol
            ));
            continue;
        };
        let src = match InstanceSource::new(inst.clone(), adapter) {
            Ok(s) => s,
            Err(e) => {
                errors.push(format!("{}: {e}", inst.id));
                continue;
            }
        };
        let mut scx = SrcCx::from_config(
            &cfg,
            fsm_core::CancellationToken::new(),
            Duration::from_secs(15),
        );
        scx.replay_mode = true;
        scx.lang = Some("en".to_owned());

        let params = Value::Object(inst.probe.params.clone().into_iter().collect());
        match src.call(op_name, params, &scx).await {
            Ok(output) => {
                let unmatched = net.unmatched();
                if !unmatched.is_empty() {
                    errors.push(format!(
                        "{}.{op_name}: unmatched requests: {}",
                        inst.id,
                        unmatched.join(", ")
                    ));
                    continue;
                }
                let (count, output_json) = serialize_src_output(&output);
                if count < inst.probe.min_results {
                    errors.push(format!(
                        "{}.{op_name}: got {count} results, expected min_results = {}",
                        inst.id, inst.probe.min_results
                    ));
                    continue;
                }
                reports.push(ReplayOpReport {
                    op: format!("{}.{op_name}", inst.id),
                    fixture: fixture_path,
                    result_count: count,
                    min_results: inst.probe.min_results,
                    exchanges_matched: net.hits().len(),
                    output_json,
                });
            }
            Err(e) => errors.push(format!("{}.{op_name}: {e}", inst.id)),
        }
    }

    if !errors.is_empty() {
        bail!("{}", errors.join("\n"));
    }
    Ok(reports)
}

pub fn serialize_src_output(output: &SrcOutput) -> (usize, Value) {
    match output {
        SrcOutput::Hits(hits) => (
            hits.len(),
            json!({ "kind": "hits", "count": hits.len(), "items": hits }),
        ),
        SrcOutput::Works(works) => (
            works.len(),
            json!({ "kind": "works", "count": works.len(), "items": works }),
        ),
        SrcOutput::Records {
            items,
            hits,
            next_cursor,
        } => (
            items.len(),
            json!({
                "kind": "records",
                "count": items.len(),
                "nextCursor": next_cursor,
                "items": items,
                "hits": hits,
            }),
        ),
        SrcOutput::Text(t) => {
            let count = usize::from(!t.trim().is_empty());
            (
                count,
                json!({ "kind": "text", "chars": t.len(), "text": t }),
            )
        }
        SrcOutput::Raw(v) => {
            let count = match v {
                Value::Null => 0,
                Value::Array(a) => a.len(),
                _ => 1,
            };
            (count, json!({ "kind": "raw", "raw": v }))
        }
        _ => (0, Value::Null),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn replay_instance_table_executes_real_protocol_adapter_i11() {
        let root = find_manifests_root(Path::new("."));
        let mw_path = root.join("instances/seed/academic_mediawiki.toml");
        let reports = run_replay(&mw_path, None, None, Some("seed"))
            .await
            .expect("replay academic_mediawiki.toml instances");
        assert_eq!(reports.len(), 2, "expected wikipedia + wikimedia reports");
        assert!(
            reports
                .iter()
                .any(|r| r.op == "wikipedia.search" && r.result_count >= 1)
        );
        assert!(
            reports
                .iter()
                .any(|r| r.op == "wikimedia.search" && r.result_count >= 1)
        );

        let dv_path = root.join("instances/seed/academic_dataverse.toml");
        let dv_reports = run_replay(&dv_path, None, None, Some("seed"))
            .await
            .expect("replay academic_dataverse.toml instance");
        assert_eq!(dv_reports.len(), 1);
        assert_eq!(dv_reports[0].op, "dataverse.search");
        assert!(dv_reports[0].result_count >= 1);
    }
}
