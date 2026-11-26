//! `xtask validate <file-or-dir>...` implementation.
//!
//! Delegates protocol instance dialect/op/param validation directly to the real
//! `fsm-protocols-a` and `fsm-protocols-b` `ProtocolAdapter` implementations (A1 §D.2 #5 / I11).

use anyhow::{Context, Result, bail};
use fsm_core::fixture::{Fixture, ReplayNet};
use fsm_core::manifest::{InstanceFileDecl, ManifestDecl};
use fsm_core::{Config, NullStore, Registry, SourceDeps};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Summary of validating one or more manifest/instance files.
pub struct ValidateSummary {
    pub files_checked: usize,
    pub files_passed: usize,
    pub failures: Vec<(PathBuf, Vec<String>)>,
}

/// Build an offline `Registry` with all 18 real `fsm-protocols-{a,b}` adapters registered.
pub fn build_protocol_registry(
    net: Arc<dyn fsm_core::NetClient>,
    cfg: Arc<Config>,
) -> Result<Registry> {
    let mut reg = Registry::new();
    let deps = SourceDeps::new(net, Arc::new(NullStore::new()), cfg);
    fsm_protocols_a::register(&mut reg, &deps)
        .map_err(|e| anyhow::anyhow!("failed to register fsm-protocols-a: {e}"))?;
    fsm_protocols_b::register(&mut reg, &deps)
        .map_err(|e| anyhow::anyhow!("failed to register fsm-protocols-b: {e}"))?;
    Ok(reg)
}

/// Validate a protocol instance table directly against the registered real `ProtocolAdapter`
/// (`validate_dialect`, `ops()`, `default_params`, and `probe.op`/`probe.params`).
pub fn validate_instance_with_registry(file: &InstanceFileDecl, reg: &Registry) -> Vec<String> {
    let mut problems = Vec::new();
    let Some(adapter) = reg.protocol(&file.protocol) else {
        let mut known: Vec<&str> = reg.protocols().map(|p| p.kind()).collect();
        known.sort_unstable();
        problems.push(format!(
            "unknown protocol {:?} (expected one of: {})",
            file.protocol,
            known.join(", ")
        ));
        return problems;
    };

    let ops = adapter.ops();
    let all_param_names: BTreeSet<&str> = ops
        .iter()
        .flat_map(|o| o.params.iter().map(|p| p.name.as_str()))
        .collect();

    for inst in &file.instance {
        let at = format!("instance {:?}", inst.id);

        // A1 §D.2 #5: delegate dialect validation directly to the real ProtocolAdapter!
        if let Err(e) = adapter.validate_dialect(&inst.dialect) {
            problems.push(format!("{at}: {e}"));
        }

        // A1 §D.2 #1: every default_params key must belong to at least one adapter op.
        for k in inst.default_params.keys() {
            if !all_param_names.contains(k.as_str()) {
                problems.push(format!(
                    "{at}: default_params key {k:?} is not a valid parameter of protocol {:?}",
                    file.protocol
                ));
            }
        }

        // Probe op & params validation.
        if let Some(op_name) = inst.probe.op.as_deref() {
            match ops.iter().find(|o| o.name == op_name) {
                None => {
                    let avail: Vec<&str> = ops.iter().map(|o| o.name.as_str()).collect();
                    problems.push(format!(
                        "{at}: probe.op {op_name:?} not supported by protocol {:?} (valid: {})",
                        file.protocol,
                        avail.join(", ")
                    ));
                }
                Some(op_decl) => {
                    let op_params: BTreeSet<&str> =
                        op_decl.params.iter().map(|p| p.name.as_str()).collect();
                    for k in inst.probe.params.keys() {
                        if !op_params.contains(k.as_str()) {
                            problems.push(format!(
                                "{at}: probe.params key {k:?} is not valid for {}.{}",
                                file.protocol, op_name
                            ));
                        }
                    }
                    for req in op_decl.params.iter().filter(|p| p.required) {
                        if !inst.probe.params.contains_key(&req.name)
                            && !inst.default_params.contains_key(&req.name)
                        {
                            problems.push(format!(
                                "{at}: probe.params missing required parameter {:?} for {}.{}",
                                req.name, file.protocol, op_name
                            ));
                        }
                    }
                }
            }
        }
    }

    problems
}

/// Locate `crates/manifests` relative to a target file or the workspace root.
pub fn find_manifests_root(hint: &Path) -> PathBuf {
    let abs = hint.canonicalize().unwrap_or_else(|_| hint.to_path_buf());
    for anc in abs.ancestors() {
        if anc.file_name().is_some_and(|n| n == "manifests")
            && anc.join("manifest.schema.json").exists()
        {
            return anc.to_path_buf();
        }
        let candidate = anc.join("crates").join("manifests");
        if candidate.join("manifest.schema.json").exists() {
            return candidate;
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap_or(Path::new("."))
        .join("crates")
        .join("manifests")
}

/// Infer the `<wave>` directory name from a path under `sources/<w>/` or `instances/<w>/`.
pub fn infer_wave(path: &Path) -> Option<String> {
    let comps: Vec<String> = path
        .components()
        .filter_map(|c| c.as_os_str().to_str().map(str::to_owned))
        .collect();
    for i in 0..comps.len().saturating_sub(2) {
        if matches!(comps[i].as_str(), "sources" | "instances") {
            return Some(comps[i + 1].clone());
        }
    }
    None
}

/// Collect all `*.toml` files under `paths` (recursing into directories).
pub fn collect_toml_files(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for p in paths {
        if !p.exists() {
            bail!("path does not exist: {}", p.display());
        }
        if p.is_file() {
            out.push(p.clone());
        } else if p.is_dir() {
            walk_toml(p, &mut out)?;
        }
    }
    out.sort();
    Ok(out)
}

fn walk_toml(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("cannot read dir {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            walk_toml(&p, out)?;
        } else if p.extension().is_some_and(|e| e == "toml") {
            out.push(p);
        }
    }
    Ok(())
}

/// Run full validation over all TOML files under `paths`.
pub fn run_validate(
    paths: &[PathBuf],
    wave_override: Option<&str>,
    skip_fixtures: bool,
) -> Result<ValidateSummary> {
    let files = collect_toml_files(paths)?;
    if files.is_empty() {
        bail!("no *.toml files found in the given path(s)");
    }

    let empty_net = Arc::new(
        ReplayNet::from_fixtures(Vec::new())
            .map_err(|e| anyhow::anyhow!("empty ReplayNet: {e}"))?,
    );
    let proto_reg = build_protocol_registry(empty_net, Arc::new(Config::default()))?;

    let reserved = fsm_manifests::reserved_ids();
    let alloc = fsm_manifests::wave_allocation();
    let mut seen_ids: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut failures = Vec::new();
    let mut passed = 0usize;

    for file in &files {
        let manifests_root = find_manifests_root(file);
        let wave = wave_override
            .map(str::to_owned)
            .or_else(|| infer_wave(file));
        let mut probs = Vec::new();

        let text = match fs::read_to_string(file) {
            Ok(t) => t,
            Err(e) => {
                failures.push((file.clone(), vec![format!("read error: {e}")]));
                continue;
            }
        };

        let raw: toml::Value = match toml::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                failures.push((file.clone(), vec![format!("TOML syntax error: {e}")]));
                continue;
            }
        };

        let is_instance = raw.as_table().is_some_and(|t| t.contains_key("protocol"));

        if is_instance {
            match InstanceFileDecl::from_toml(&text) {
                Err(e) => probs.push(format!("schema error: {e}")),
                Ok(inst_file) => {
                    probs.extend(inst_file.problems());
                    probs.extend(validate_instance_with_registry(&inst_file, &proto_reg));

                    for inst in &inst_file.instance {
                        if let Some(w) = wave.as_deref()
                            && let Err(e) = alloc.check_placement(reserved, &inst.id, w, true)
                        {
                            probs.push(e);
                        }
                        if let Some(prev) = seen_ids.insert(inst.id.clone(), file.clone()) {
                            probs.push(format!(
                                "duplicate id {:?} (already seen in {})",
                                inst.id,
                                prev.display()
                            ));
                        }
                        if !skip_fixtures && let Some(op_name) = inst.probe.op.as_deref() {
                            check_probe_fixture(
                                &manifests_root,
                                wave.as_deref(),
                                &inst.id,
                                op_name,
                                inst.probe.fixture.as_deref(),
                                &mut probs,
                            );
                        }
                    }
                }
            }
        } else {
            match ManifestDecl::from_toml(&text) {
                Err(e) => probs.push(format!("schema error: {e}")),
                Ok(decl) => {
                    probs.extend(decl.problems());

                    // Filename stem check when under sources/.
                    if wave.is_some()
                        && let Some(stem) = file.file_stem().and_then(|s| s.to_str())
                        && stem != decl.id
                    {
                        probs.push(format!(
                            "filename stem {stem:?} does not match manifest id {:?}",
                            decl.id
                        ));
                    }

                    // RESERVED_IDS & WAVES.toml placement check.
                    if let Some(w) = wave.as_deref()
                        && let Err(e) = alloc.check_placement(reserved, &decl.id, w, false)
                    {
                        probs.push(e);
                    }

                    if let Some(prev) = seen_ids.insert(decl.id.clone(), file.clone()) {
                        probs.push(format!(
                            "duplicate id {:?} (already seen in {})",
                            decl.id,
                            prev.display()
                        ));
                    }

                    // Compile every jaq expression with the REAL 13-function prelude!
                    probs.extend(fsm_sources::jaq_rt::validate_manifest_jaq(&decl));

                    // Validate CSS selectors in every [op.<name>.html].
                    for (op_name, op) in decl.op.iter() {
                        if let Some(html) = &op.html
                            && let Err(e) = fsm_sources::html::validate_html_spec(html)
                        {
                            probs.push(format!("op.{op_name}: {e}"));
                        }
                    }

                    // Verify probe fixtures exist & parse cleanly.
                    if !skip_fixtures {
                        for (op_name, op) in decl.op.iter() {
                            if let Some(probe) = &op.probe {
                                check_probe_fixture(
                                    &manifests_root,
                                    wave.as_deref(),
                                    &decl.id,
                                    op_name,
                                    probe.fixture.as_deref(),
                                    &mut probs,
                                );
                            }
                        }
                    }
                }
            }
        }

        if probs.is_empty() {
            passed += 1;
        } else {
            failures.push((file.clone(), probs));
        }
    }

    Ok(ValidateSummary {
        files_checked: files.len(),
        files_passed: passed,
        failures,
    })
}

fn check_probe_fixture(
    manifests_root: &Path,
    wave: Option<&str>,
    id: &str,
    op_name: &str,
    explicit: Option<&str>,
    probs: &mut Vec<String>,
) {
    let rel = match (explicit, wave) {
        (Some(f), _) => f.to_owned(),
        (None, Some(w)) => format!("fixtures/{w}/{id}/{op_name}/default.json"),
        (None, None) => {
            probs.push(format!(
                "op.{op_name}.probe: no explicit fixture path and wave cannot be inferred from file path"
            ));
            return;
        }
    };
    let full = manifests_root.join(&rel);
    if !full.is_file() {
        probs.push(format!(
            "op.{op_name}.probe: missing fixture file {}",
            full.display()
        ));
        return;
    }
    match fs::read_to_string(&full) {
        Ok(text) => {
            if let Err(e) = Fixture::from_json(&text) {
                probs.push(format!(
                    "op.{op_name}.probe: invalid fixture {}: {e}",
                    full.display()
                ));
            }
        }
        Err(e) => {
            probs.push(format!(
                "op.{op_name}.probe: cannot read fixture {}: {e}",
                full.display()
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_delegates_dialect_type_checks_to_real_protocol_adapters() {
        let empty_net = Arc::new(ReplayNet::from_fixtures(Vec::new()).unwrap());
        let reg = build_protocol_registry(empty_net, Arc::new(Config::default())).unwrap();

        // Bad dialect type on mediawiki (`default_namespace = "abc"` instead of int) MUST fail!
        let bad_mw = r#"
protocol = "mediawiki"

[[instance]]
id = "bad_mw_inst"
name = "Bad MW"
summary = "Invalid dialect type."
base_url = "https://en.wikipedia.org/w/api.php"
scopes = ["reference"]
caps = ["search"]
tier = 1
dialect = { default_namespace = "abc" }
probe = { op = "search", params = { query = "rust" }, min_results = 1 }
"#;
        let file = InstanceFileDecl::from_toml(bad_mw).unwrap();
        let probs = validate_instance_with_registry(&file, &reg);
        assert!(
            !probs.is_empty(),
            "expected real MediaWikiAdapter::validate_dialect to reject string default_namespace"
        );

        // Bad dialect type on opensearch (`upgrade_https = "yes"` instead of bool) MUST fail!
        let bad_os = r#"
protocol = "opensearch"

[[instance]]
id = "bad_os_inst"
name = "Bad OS"
summary = "Invalid dialect type."
base_url = "https://example.org/osdd.xml"
scopes = ["reference"]
caps = ["search"]
tier = 2
dialect = { upgrade_https = "yes" }
probe = { op = "search", params = { query = "rust" }, min_results = 1 }
"#;
        let file_os = InstanceFileDecl::from_toml(bad_os).unwrap();
        let probs_os = validate_instance_with_registry(&file_os, &reg);
        assert!(
            !probs_os.is_empty(),
            "expected real OpenSearchAdapter::validate_dialect to reject string upgrade_https"
        );
    }
}
