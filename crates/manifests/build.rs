//! Build script for `fsm-manifests`.
//!
//! Globs ONLY `sources/**/*.toml` and `instances/**/*.toml` (NEVER `staging/**`),
//! validates every file against `ManifestDecl::validate()` / `InstanceFileDecl::validate()`,
//! `RESERVED_IDS`, `WAVES.toml`, `core::SCOPES`, and `jaq` compilation, and embeds the
//! validated bundle via `postcard` into `$OUT_DIR/embedded_manifests.bin`.

#![allow(clippy::print_stdout)]

#[allow(dead_code)]
#[path = "src/jaq_check.rs"]
mod jaq_check;
#[allow(dead_code)]
#[path = "src/waves.rs"]
mod waves;

use fsm_core::manifest::{InstanceFileDecl, ManifestDecl};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EmbeddedManifest {
    wave: String,
    path: String,
    manifest: ManifestDecl,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EmbeddedInstanceFile {
    wave: String,
    path: String,
    file: InstanceFileDecl,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct EmbeddedBundle {
    manifests: Vec<EmbeddedManifest>,
    instances: Vec<EmbeddedInstanceFile>,
}

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=RESERVED_IDS");
    println!("cargo:rerun-if-changed=WAVES.toml");
    println!("cargo:rerun-if-changed=sources");
    println!("cargo:rerun-if-changed=instances");

    let reserved_text = fs::read_to_string(manifest_dir.join("RESERVED_IDS"))
        .expect("crates/manifests/RESERVED_IDS must exist");
    let reserved = waves::ReservedIds::parse(&reserved_text)
        .unwrap_or_else(|e| panic!("crates/manifests/RESERVED_IDS invalid: {e}"));

    let waves_text = fs::read_to_string(manifest_dir.join("WAVES.toml"))
        .expect("crates/manifests/WAVES.toml must exist");
    let alloc = waves::WaveAllocation::parse(&waves_text)
        .unwrap_or_else(|e| panic!("crates/manifests/WAVES.toml invalid: {e}"));

    let mut seen_ids: BTreeMap<String, String> = BTreeMap::new();
    let mut bundle = EmbeddedBundle::default();

    // 1. Glob sources/**/*.toml (NEVER staging/**)
    let sources_root = manifest_dir.join("sources");
    if sources_root.is_dir() {
        emit_dir_rerun(&sources_root, &manifest_dir);
        let pattern = format!("{}/**/*.toml", sources_root.display());
        let mut paths = collect_glob(&pattern);
        paths.sort();

        for path in paths {
            let rel = rel_path(&path, &manifest_dir);
            println!("cargo:rerun-if-changed={rel}");
            let wave = extract_wave(&sources_root, &path).unwrap_or_else(|| {
                panic!("manifest {rel}: must live directly under sources/<wave>/<id>.toml")
            });
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("manifest {rel}: cannot read: {e}"));
            let decl = ManifestDecl::from_toml(&text)
                .unwrap_or_else(|e| panic!("manifest {rel}: parse error: {e}"));
            decl.validate()
                .unwrap_or_else(|e| panic!("manifest {rel}: validation failed: {e}"));

            // File stem should match manifest id.
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if stem != decl.id {
                panic!(
                    "manifest {rel}: filename stem {stem:?} does not match id {:?}",
                    decl.id
                );
            }

            // RESERVED_IDS & WAVES.toml enforcement.
            if let Err(e) = alloc.check_placement(&reserved, &decl.id, &wave, false) {
                panic!("manifest {rel}: {e}");
            }

            // Duplicate ID check across all sources & instances.
            if let Some(prev) = seen_ids.insert(decl.id.clone(), rel.clone()) {
                panic!(
                    "manifest {rel}: duplicate source id {:?} (already defined in {prev})",
                    decl.id
                );
            }

            // Compile every jaq filter at build time (S14).
            let jaq_errs = jaq_check::check_manifest_jaq(&decl);
            if !jaq_errs.is_empty() {
                panic!(
                    "manifest {rel}: jaq compile error(s): {}",
                    jaq_errs.join("; ")
                );
            }

            bundle.manifests.push(EmbeddedManifest {
                wave,
                path: rel,
                manifest: decl,
            });
        }
    }

    // 2. Glob instances/**/*.toml (NEVER staging/**)
    let instances_root = manifest_dir.join("instances");
    if instances_root.is_dir() {
        emit_dir_rerun(&instances_root, &manifest_dir);
        let pattern = format!("{}/**/*.toml", instances_root.display());
        let mut paths = collect_glob(&pattern);
        paths.sort();

        for path in paths {
            let rel = rel_path(&path, &manifest_dir);
            println!("cargo:rerun-if-changed={rel}");
            let wave = extract_wave(&instances_root, &path).unwrap_or_else(|| {
                panic!("instance file {rel}: must live under instances/<wave>/*.toml")
            });
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("instance file {rel}: cannot read: {e}"));
            let file = InstanceFileDecl::from_toml(&text)
                .unwrap_or_else(|e| panic!("instance file {rel}: parse error: {e}"));
            file.validate()
                .unwrap_or_else(|e| panic!("instance file {rel}: validation failed: {e}"));

            for inst in &file.instance {
                if let Err(e) = alloc.check_placement(&reserved, &inst.id, &wave, true) {
                    panic!("instance file {rel}: {e}");
                }
                if let Some(prev) = seen_ids.insert(inst.id.clone(), rel.clone()) {
                    panic!(
                        "instance file {rel}: duplicate source id {:?} (already defined in {prev})",
                        inst.id
                    );
                }
            }

            bundle.instances.push(EmbeddedInstanceFile {
                wave,
                path: rel,
                file,
            });
        }
    }

    let bytes = postcard::to_allocvec(&bundle)
        .unwrap_or_else(|e| panic!("failed to postcard-encode embedded manifests: {e}"));
    fs::write(out_dir.join("embedded_manifests.bin"), bytes)
        .expect("failed to write embedded_manifests.bin");
}

fn collect_glob(pattern: &str) -> Vec<PathBuf> {
    glob::glob(pattern)
        .unwrap_or_else(|e| panic!("bad glob {pattern:?}: {e}"))
        .map(|entry| entry.unwrap_or_else(|e| panic!("glob read error: {e}")))
        .filter(|p| p.is_file())
        .collect()
}

fn emit_dir_rerun(dir: &Path, root: &Path) {
    if let Ok(rd) = fs::read_dir(dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                println!("cargo:rerun-if-changed={}", rel_path(&p, root));
                emit_dir_rerun(&p, root);
            }
        }
    }
}

fn rel_path(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn extract_wave(root: &Path, file: &Path) -> Option<String> {
    waves::extract_wave(root, file)
}
