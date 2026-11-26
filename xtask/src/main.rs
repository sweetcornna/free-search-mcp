//! `cargo run -p xtask -- <subcommand>` — manifest validation, offline replay,
//! fixture recording, and wave probing (`SPEC-lanes.md` W2a / M11).

#![allow(clippy::print_stdout)]

mod probe;
mod record;
mod replay;
mod validate;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use fsm_core::manifest::{AuthIn, ManifestDecl};
use fsm_core::{Config, NullStore, Source, SourceDeps, SrcCx};
use fsm_sources::{ManifestSource, OAuth2TokenCache};
use jiff::Timestamp;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(name = "xtask", about = "free-search-mcp manifest tooling")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Parse, validate schema/waves/jaq/HTML selectors, and verify probe fixtures exist.
    Validate {
        /// One or more manifest/instance `.toml` files or directories to validate.
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Override wave name inferred from path (e.g. `seed`, `w5a`).
        #[arg(long)]
        wave: Option<String>,
        /// Skip checking that probe fixture files exist on disk.
        #[arg(long)]
        no_fixtures: bool,
    },
    /// Replay a manifest file against its recorded fixture(s) through the real interpreter.
    Replay {
        /// Path to a declarative manifest `.toml` file.
        file: PathBuf,
        /// Replay only this operation name (defaults to all ops with a `[op.<name>.probe]`).
        #[arg(long)]
        op: Option<String>,
        /// Explicit fixture JSON path override.
        #[arg(long)]
        fixture: Option<PathBuf>,
        /// Override wave name inferred from path.
        #[arg(long)]
        wave: Option<String>,
    },
    /// Record live HTTP replay fixtures for a manifest ID or file (with secret redaction).
    Record {
        /// Manifest ID (e.g. `arxiv`) or path to a `.toml` manifest file.
        target: String,
        /// Record only this op (defaults to all ops with a probe).
        #[arg(long)]
        op: Option<String>,
        /// Override wave name for output fixture path (`fixtures/<wave>/<id>/<op>/default.json`).
        #[arg(long)]
        wave: Option<String>,
        /// Allow writing a fixture even if `result_count < probe.min_results`.
        #[arg(long)]
        allow_short: bool,
    },
    /// Probe all keyless manifests in a wave (or `--all`) and check the green threshold (M11).
    Probe {
        /// Wave directory name under `crates/manifests/sources/<wave>` (e.g. `seed`, `w5a`).
        #[arg(long)]
        wave: Option<String>,
        /// Probe all promoted waves under `crates/manifests/sources/`.
        #[arg(long)]
        all: bool,
        /// Number of qualifying runs required (default: 2).
        #[arg(long, default_value_t = 2)]
        runs: usize,
        /// Minimum gap between qualifying runs, e.g. `1h`, `30m`, `0s` (default: `1h`).
        #[arg(long, default_value = "1h")]
        min_gap: String,
        /// Minimum green fraction in 0.0..=1.0 (default: 0.95).
        #[arg(long, default_value_t = 0.95)]
        min_green: f64,
        /// Run against recorded offline fixtures instead of live upstream hosts.
        #[arg(long)]
        replay: bool,
        /// Directory for storing probe run history across `--runs`.
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Optional comma-separated filter of source IDs or `id.op` pairs to probe.
        #[arg(long)]
        only: Option<String>,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(1)
        }
    }
}

async fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Validate {
            paths,
            wave,
            no_fixtures,
        } => {
            let summary = validate::run_validate(&paths, wave.as_deref(), no_fixtures)?;
            for (path, probs) in &summary.failures {
                println!("FAIL {}", path.display());
                for prob in probs {
                    println!("  - {prob}");
                }
            }
            println!(
                "validate: {}/{} passed ({} failed)",
                summary.files_passed,
                summary.files_checked,
                summary.failures.len()
            );
            Ok(if summary.failures.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
        Cmd::Replay {
            file,
            op,
            fixture,
            wave,
        } => {
            let reports =
                replay::run_replay(&file, op.as_deref(), fixture.as_deref(), wave.as_deref())
                    .await?;
            for rep in &reports {
                println!(
                    "=== op={} (results={}/{}, exchanges={}, fixture={}) ===",
                    rep.op,
                    rep.result_count,
                    rep.min_results,
                    rep.exchanges_matched,
                    rep.fixture.display()
                );
                println!("{}", serde_json::to_string_pretty(&rep.output_json)?);
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Record {
            target,
            op,
            wave,
            allow_short,
        } => {
            run_record(&target, op.as_deref(), wave.as_deref(), allow_short).await?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Probe {
            wave,
            all,
            runs,
            min_gap,
            min_green,
            replay,
            state_dir,
            only,
        } => {
            let gap = probe::parse_duration_str(&min_gap)?;
            let only_filter = only
                .map(|s| {
                    s.split(',')
                        .map(|t| t.trim().to_owned())
                        .filter(|t| !t.is_empty())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let args = probe::ProbeArgs {
                wave,
                all,
                runs,
                min_gap: gap,
                min_green,
                replay_only: replay,
                state_dir,
                only: only_filter,
            };
            let (passed, latest, summary) = probe::run_probe(&args).await?;
            for o in &latest.outcomes {
                if o.status == "failed" {
                    println!(
                        "FAIL {}.{}: {}",
                        o.id,
                        o.op,
                        o.error.as_deref().unwrap_or("unknown error")
                    );
                } else if !args.only.is_empty() {
                    println!(
                        "OK   {}.{}: {} ({} results)",
                        o.id, o.op, o.status, o.results
                    );
                }
            }
            println!("{summary}");
            Ok(if passed {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
    }
}

async fn run_record(
    target: &str,
    only_op: Option<&str>,
    wave_override: Option<&str>,
    allow_short: bool,
) -> Result<()> {
    let manifests_root = validate::find_manifests_root(Path::new(target));
    let manifest_path = resolve_manifest_target(&manifests_root, target)?;
    let text = fs::read_to_string(&manifest_path)
        .with_context(|| format!("cannot read {}", manifest_path.display()))?;
    let decl = ManifestDecl::parse_validated(&text)
        .with_context(|| format!("manifest {} invalid", manifest_path.display()))?;

    let wave = wave_override
        .map(str::to_owned)
        .or_else(|| validate::infer_wave(&manifest_path))
        .unwrap_or_else(|| "seed".to_owned());

    let cfg = Arc::new(Config::load().unwrap_or_default());
    let live: Arc<dyn fsm_core::NetClient> = record::build_live_net(&cfg)?;
    let redact_body_key = decl.auth.as_ref().and_then(|a| {
        if matches!(a.location, Some(AuthIn::Body)) {
            a.name.clone()
        } else {
            None
        }
    });

    let ops: Vec<(&str, &fsm_core::manifest::OpSpec)> = match only_op {
        Some(name) => {
            let op = decl
                .op
                .get(name)
                .with_context(|| format!("manifest {:?} has no op {name:?}", decl.id))?;
            vec![(name, op)]
        }
        None => decl
            .op
            .iter()
            .filter(|(_, op)| op.probe.is_some())
            .collect(),
    };
    if ops.is_empty() {
        bail!("manifest {:?} has no ops with probe", decl.id);
    }

    for (op_name, op) in ops {
        let probe = op
            .probe
            .as_ref()
            .with_context(|| format!("{}.{op_name} has no probe", decl.id))?;
        let recorder = Arc::new(record::RecordingNet::with_body_redaction(
            Arc::clone(&live),
            redact_body_key.clone(),
        ));
        let deps = SourceDeps::new(
            recorder.clone(),
            Arc::new(NullStore::new()),
            Arc::clone(&cfg),
        );
        let src = ManifestSource::new(decl.clone(), deps, Arc::new(OAuth2TokenCache::new()));

        let mut scx = SrcCx::from_config(
            &cfg,
            fsm_core::CancellationToken::new(),
            Duration::from_millis(decl.limits.timeout_ms.max(25_000)),
        );
        scx.lang = Some("en".to_owned());

        let params = Value::Object(probe.params.clone().into_iter().collect());
        let out = src
            .call(op_name, params, &scx)
            .await
            .with_context(|| format!("live call failed for {}.{op_name}", decl.id))?;
        let (count, _) = replay::serialize_src_output(&out);
        if !allow_short && count < probe.min_results {
            bail!(
                "{}.{op_name}: live call returned {count} result(s), below probe.min_results={} (pass --allow-short to record anyway)",
                decl.id,
                probe.min_results
            );
        }

        let now_iso = format_utc_rfc3339(Timestamp::now());
        let fixture = recorder.take_fixture(&decl.id, op_name, &now_iso);
        fixture
            .validate()
            .with_context(|| format!("recorded fixture invalid for {}.{op_name}", decl.id))?;

        let rel = probe.fixture_path(&wave, &decl.id, op_name);
        let out_path = manifests_root.join(&rel);
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&out_path, fixture.to_json())
            .with_context(|| format!("cannot write {}", out_path.display()))?;
        println!(
            "RECORDED {}.{} -> {} ({} exchange(s), {} result(s))",
            decl.id,
            op_name,
            out_path.display(),
            fixture.exchanges.len(),
            count
        );
    }
    Ok(())
}

fn resolve_manifest_target(manifests_root: &Path, target: &str) -> Result<PathBuf> {
    let as_path = PathBuf::from(target);
    if as_path.is_file() {
        return Ok(as_path);
    }
    let pattern = format!("{}/sources/*/{target}.toml", manifests_root.display());
    if let Ok(paths) = glob::glob(&pattern) {
        for entry in paths.flatten() {
            if entry.is_file() {
                return Ok(entry);
            }
        }
    }
    bail!("could not locate manifest {target:?} (pass a .toml path or an id under sources/)");
}

fn format_utc_rfc3339(ts: Timestamp) -> String {
    let dt = jiff::tz::TimeZone::UTC.to_datetime(ts);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        dt.year(),
        dt.month(),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second()
    )
}
