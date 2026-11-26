//! `xtask probe --wave <w> --runs 2 --min-gap 1h --min-green 0.95` implementation (M11 / B9).
//!
//! Exits 0 iff the **latest** run AND at least `runs` total qualifying runs separated by
//! at least `min_gap` each achieve `rate >= min_green`, else exits 1.
//! Scans BOTH `sources/<wave>/*.toml` (declarative manifests) AND `instances/<wave>/*.toml`
//! (protocol instances via real `fsm-protocols-{a,b}` adapters).
//! In live mode, skips individual ops whose effective auth requires credentials (or
//! `route_requires_key = true` on `search`) when credentials are absent.
//! Under `--replay`, always uses `Config::default()` so host env vars never perturb replay.

use crate::record::build_live_http_client;
use crate::replay::serialize_src_output;
use crate::validate::{build_protocol_registry, find_manifests_root};
use anyhow::{Context, Result, bail};
use fsm_core::fixture::ReplayNet;
use fsm_core::manifest::{
    AuthModeManifest, InstanceFileDecl, ManifestDecl, OpAuthOverride, OpSpec,
};
use fsm_core::{Config, NetClient, NullStore, Source, SourceDeps, SrcCx};
use fsm_sources::{InstanceSource, ManifestSource, OAuth2TokenCache};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeSourceOutcome {
    pub id: String,
    pub op: String,
    pub status: String, // "green" | "failed" | "skipped_needs_key"
    pub results: usize,
    pub min_results: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeRunRecord {
    pub timestamp_s: i64,
    pub wave: String,
    pub green: usize,
    pub failed: usize,
    pub skipped: usize,
    pub rate: f64,
    pub outcomes: Vec<ProbeSourceOutcome>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProbeHistory {
    pub runs: Vec<ProbeRunRecord>,
}

/// Parse a human duration string such as `"1h"`, `"30m"`, `"45s"`, `"0s"`, `"1h30m"`.
pub fn parse_duration_str(raw: &str) -> Result<Duration> {
    let s = raw.trim();
    if s.is_empty() {
        bail!("empty duration");
    }
    if let Ok(secs) = s.parse::<u64>() {
        return Ok(Duration::from_secs(secs));
    }
    let mut total_s: u64 = 0;
    let mut num = String::new();
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            num.push(ch);
        } else {
            let val: u64 = num
                .parse()
                .with_context(|| format!("invalid duration {raw:?}"))?;
            num.clear();
            let mult = match ch {
                's' | 'S' => 1,
                'm' | 'M' => 60,
                'h' | 'H' => 3600,
                'd' | 'D' => 86_400,
                _ => bail!("unknown duration unit {ch:?} in {raw:?}"),
            };
            total_s = total_s.saturating_add(val.saturating_mul(mult));
        }
    }
    if !num.is_empty() {
        bail!("trailing digits without unit in duration {raw:?}");
    }
    Ok(Duration::from_secs(total_s))
}

#[derive(Debug)]
pub struct ProbeArgs {
    pub wave: Option<String>,
    pub all: bool,
    pub runs: usize,
    pub min_gap: Duration,
    pub min_green: f64,
    pub replay_only: bool,
    pub state_dir: Option<PathBuf>,
    pub only: Vec<String>,
}

fn matches_only(only: &[String], id: &str, op: &str) -> bool {
    if only.is_empty() {
        return true;
    }
    let full = format!("{id}.{op}");
    only.iter().any(|f| f == id || f == &full)
}

/// Execute `xtask probe` and return `(passed, latest_run, history_summary)`.
pub async fn run_probe(args: &ProbeArgs) -> Result<(bool, ProbeRunRecord, String)> {
    let manifests_root = find_manifests_root(Path::new("."));
    let wave_label = if args.all {
        "all".to_owned()
    } else {
        args.wave
            .clone()
            .ok_or_else(|| anyhow::anyhow!("specify either --wave <w> or --all"))?
    };

    let manifest_files =
        collect_wave_files(&manifests_root, "sources", args.wave.as_deref(), args.all)?;
    let instance_files =
        collect_wave_files(&manifests_root, "instances", args.wave.as_deref(), args.all)?;
    if manifest_files.is_empty() && instance_files.is_empty() {
        bail!(
            "no manifests or protocol instance tables found for wave {wave_label:?} under {}/{{sources,instances}}",
            manifests_root.display()
        );
    }

    // B9: Under --replay, ALWAYS use clean Config::default() so host env vars (e.g.
    // SEARCH_MCP_CONTACT_EMAIL) never perturb recorded fixture URLs.
    let cfg = Arc::new(if args.replay_only {
        Config::default()
    } else {
        Config::load().unwrap_or_default()
    });

    let state_path = args
        .state_dir
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join("fsm-xtask-probe"))
        .join(format!("{wave_label}.json"));

    let mut history: ProbeHistory = fs::read_to_string(&state_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();

    let run = execute_single_probe_run(
        &manifests_root,
        &wave_label,
        &manifest_files,
        &instance_files,
        &cfg,
        args.replay_only,
        &args.only,
    )
    .await?;

    history.runs.push(run.clone());
    if let Some(parent) = state_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(
        &state_path,
        serde_json::to_string_pretty(&history).unwrap_or_default(),
    );

    // B9: Latest run MUST pass `min_green`, and we select `required_runs` spaced green runs
    // backwards from the latest run so stale historical passes can never mask a current failure.
    let required_runs = args.runs.max(1);
    let min_gap_s = args.min_gap.as_secs() as i64;
    let qualifying =
        select_qualifying_runs(&history.runs, required_runs, min_gap_s, args.min_green);

    let passed = run.rate + f64::EPSILON >= args.min_green && qualifying.len() >= required_runs;
    let summary = format!(
        "wave={wave_label} latest={}/{} ({:.1}%, skipped={}) qualifying_runs={}/{} (min_gap={}s, min_green={:.1}%)",
        run.green,
        run.green + run.failed,
        run.rate * 100.0,
        run.skipped,
        qualifying.len(),
        required_runs,
        min_gap_s,
        args.min_green * 100.0
    );

    Ok((passed, run, summary))
}

/// Select up to `needed` qualifying runs spaced by `>= min_gap_s` walking **backwards**
/// from the latest run. Returns empty if the latest run itself fails `min_green` (B9).
fn select_qualifying_runs(
    runs: &[ProbeRunRecord],
    needed: usize,
    min_gap_s: i64,
    min_green: f64,
) -> Vec<&ProbeRunRecord> {
    let Some(latest) = runs.last() else {
        return Vec::new();
    };
    if latest.rate + f64::EPSILON < min_green {
        return Vec::new();
    }
    let mut picked: Vec<&ProbeRunRecord> = vec![latest];
    if needed <= 1 {
        return picked;
    }
    for r in runs[..runs.len() - 1].iter().rev() {
        if r.rate + f64::EPSILON < min_green {
            continue;
        }
        let anchor = picked.last().expect("non-empty");
        if anchor.timestamp_s - r.timestamp_s < min_gap_s {
            continue;
        }
        picked.push(r);
        if picked.len() >= needed {
            break;
        }
    }
    picked.reverse();
    picked
}

/// True when a manifest op should be skipped in **live** probing due to missing credentials
/// (handles mixed-auth manifests and `route_requires_key = true` optional-key sources, B9).
fn op_should_skip_live_auth(decl: &ManifestDecl, op_name: &str, op: &OpSpec, cfg: &Config) -> bool {
    let Some(auth) = &decl.auth else {
        return false;
    };
    let creds = auth.credential_envs();
    let has_creds = !creds.is_empty()
        && creds
            .iter()
            .all(|e| cfg.api_keys.get(*e).is_some_and(|v| !v.is_empty()));
    if has_creds {
        return false;
    }
    match op.auth {
        OpAuthOverride::None => false,
        OpAuthOverride::Required => true,
        OpAuthOverride::Inherit | _ => {
            auth.mode == AuthModeManifest::Required
                || (auth.route_requires_key && op_name == "search")
        }
    }
}

/// True when a manifest op requires `contact_email` (e.g. `unpaywall`) and neither
/// `cfg.contact_email` nor `probe.params.email` is configured in live mode.
fn op_should_skip_live_email(decl: &ManifestDecl, op: &OpSpec, cfg: &Config) -> bool {
    if cfg
        .contact_email
        .as_deref()
        .is_some_and(|s| !s.trim().is_empty())
    {
        return false;
    }
    let has_probe_email = op.probe.as_ref().is_some_and(|pr| {
        pr.params
            .get("email")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty())
    });
    if has_probe_email {
        return false;
    }
    // Check if the op declares an `email` param whose doc explicitly states it is required
    // and reads `$cfg.contact_email` in `query_jaq`.
    let uses_contact_email = op
        .query_jaq
        .as_deref()
        .is_some_and(|q| q.contains("$cfg.contact_email"));
    let email_required = op.params.get("email").is_some_and(|p| {
        p.doc.to_ascii_lowercase().contains("required")
            || decl
                .summary
                .to_ascii_lowercase()
                .contains("requires contact email")
    });
    uses_contact_email && email_required
}

/// Compute polite inter-request spacing for repeated live probe calls to the same bucket.
fn polite_bucket_gap(rps: f64) -> Duration {
    let min_gap = Duration::from_secs(1);
    if !rps.is_finite() || rps <= 0.0 {
        return min_gap;
    }
    let secs = (1.0 / rps).ceil().clamp(1.0, 22.0);
    Duration::from_secs_f64(secs)
}

async fn execute_single_probe_run(
    manifests_root: &Path,
    wave_label: &str,
    manifest_files: &[(String, PathBuf)],
    instance_files: &[(String, PathBuf)],
    cfg: &Arc<Config>,
    replay_only: bool,
    only: &[String],
) -> Result<ProbeRunRecord> {
    let mut outcomes = Vec::new();
    let mut green = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;

    let live_http: Option<Arc<fsm_net::HttpClient>> = if replay_only {
        None
    } else {
        Some(build_live_http_client(cfg)?)
    };
    let live_net: Option<Arc<dyn NetClient>> = live_http
        .as_ref()
        .map(|h| Arc::clone(h) as Arc<dyn NetClient>);
    let mut last_bucket_hit: HashMap<String, Instant> = HashMap::new();

    // 1. Declarative manifests under sources/<wave>/*.toml
    for (wave, path) in manifest_files {
        let text =
            fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
        let decl = ManifestDecl::from_toml(&text)
            .with_context(|| format!("cannot parse {}", path.display()))?;

        let bucket = decl
            .limits
            .host_bucket
            .clone()
            .unwrap_or_else(|| decl.id.clone());
        if let Some(http) = &live_http {
            http.rate_limits()
                .configure(&bucket, decl.limits.rps, decl.limits.burst.max(1));
        }

        for (op_name, op) in decl.op.iter() {
            let Some(probe) = &op.probe else {
                continue;
            };
            if !matches_only(only, &decl.id, op_name) {
                continue;
            }

            if !replay_only && op_should_skip_live_auth(&decl, op_name, op, cfg) {
                skipped += 1;
                outcomes.push(ProbeSourceOutcome {
                    id: decl.id.clone(),
                    op: op_name.to_owned(),
                    status: "skipped_needs_key".to_owned(),
                    results: 0,
                    min_results: probe.min_results,
                    error: None,
                });
                continue;
            }

            if !replay_only && op_should_skip_live_email(&decl, op, cfg) {
                skipped += 1;
                outcomes.push(ProbeSourceOutcome {
                    id: decl.id.clone(),
                    op: op_name.to_owned(),
                    status: "skipped_needs_email".to_owned(),
                    results: 0,
                    min_results: probe.min_results,
                    error: None,
                });
                continue;
            }

            if !replay_only {
                let gap = polite_bucket_gap(decl.limits.rps);
                if let Some(prev) = last_bucket_hit.get(&bucket) {
                    let elapsed = prev.elapsed();
                    if elapsed < gap {
                        tokio::time::sleep(gap - elapsed).await;
                    }
                }
                last_bucket_hit.insert(bucket.clone(), Instant::now());
            }

            let net: Arc<dyn NetClient> = if replay_only {
                let fixture_path = manifests_root.join(probe.fixture_path(wave, &decl.id, op_name));
                match ReplayNet::from_file(&fixture_path) {
                    Ok(rn) => Arc::new(rn),
                    Err(e) => {
                        failed += 1;
                        outcomes.push(ProbeSourceOutcome {
                            id: decl.id.clone(),
                            op: op_name.to_owned(),
                            status: "failed".to_owned(),
                            results: 0,
                            min_results: probe.min_results,
                            error: Some(format!("fixture load error: {e}")),
                        });
                        continue;
                    }
                }
            } else {
                Arc::clone(live_net.as_ref().expect("live_net built"))
            };

            let deps = SourceDeps::new(net, Arc::new(NullStore::new()), Arc::clone(cfg));
            let src = ManifestSource::new(decl.clone(), deps, Arc::new(OAuth2TokenCache::new()));

            let mut scx = SrcCx::from_config(
                cfg,
                fsm_core::CancellationToken::new(),
                Duration::from_millis(decl.limits.timeout_ms.max(8000)),
            );
            scx.replay_mode = replay_only;
            scx.lang = Some("en".to_owned());

            let params = Value::Object(probe.params.clone().into_iter().collect());
            record_call_outcome(
                &decl.id,
                op_name,
                probe.min_results,
                src.call(op_name, params, &scx).await,
                &mut green,
                &mut failed,
                &mut outcomes,
            );
        }
    }

    // 2. Protocol instance tables under instances/<wave>/*.toml (B9 / I11)
    for (wave, path) in instance_files {
        let text =
            fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
        let inst_file = InstanceFileDecl::from_toml(&text)
            .with_context(|| format!("cannot parse {}", path.display()))?;

        for inst in &inst_file.instance {
            let Some(op_name) = inst.probe.op.as_deref() else {
                continue;
            };
            if !matches_only(only, &inst.id, op_name) {
                continue;
            }

            let net: Arc<dyn NetClient> = if replay_only {
                let fixture_path =
                    manifests_root.join(inst.probe.fixture_path(wave, &inst.id, op_name));
                match ReplayNet::from_file(&fixture_path) {
                    Ok(rn) => Arc::new(rn),
                    Err(e) => {
                        failed += 1;
                        outcomes.push(ProbeSourceOutcome {
                            id: inst.id.clone(),
                            op: op_name.to_owned(),
                            status: "failed".to_owned(),
                            results: 0,
                            min_results: inst.probe.min_results,
                            error: Some(format!("fixture load error: {e}")),
                        });
                        continue;
                    }
                }
            } else {
                Arc::clone(live_net.as_ref().expect("live_net built"))
            };

            let reg = build_protocol_registry(net, Arc::clone(cfg))?;
            let Some(adapter) = reg.protocol(&inst_file.protocol) else {
                failed += 1;
                outcomes.push(ProbeSourceOutcome {
                    id: inst.id.clone(),
                    op: op_name.to_owned(),
                    status: "failed".to_owned(),
                    results: 0,
                    min_results: inst.probe.min_results,
                    error: Some(format!("unknown protocol {:?}", inst_file.protocol)),
                });
                continue;
            };

            let src = match InstanceSource::new(inst.clone(), adapter) {
                Ok(s) => s,
                Err(e) => {
                    failed += 1;
                    outcomes.push(ProbeSourceOutcome {
                        id: inst.id.clone(),
                        op: op_name.to_owned(),
                        status: "failed".to_owned(),
                        results: 0,
                        min_results: inst.probe.min_results,
                        error: Some(e.to_string()),
                    });
                    continue;
                }
            };

            let timeout_ms = inst
                .limits
                .as_ref()
                .map_or(8000, |l| l.timeout_ms)
                .max(8000);
            let mut scx = SrcCx::from_config(
                cfg,
                fsm_core::CancellationToken::new(),
                Duration::from_millis(timeout_ms),
            );
            scx.replay_mode = replay_only;
            scx.lang = Some("en".to_owned());

            let params = Value::Object(inst.probe.params.clone().into_iter().collect());
            record_call_outcome(
                &inst.id,
                op_name,
                inst.probe.min_results,
                src.call(op_name, params, &scx).await,
                &mut green,
                &mut failed,
                &mut outcomes,
            );
        }
    }

    let tested = green + failed;
    let rate = if tested == 0 {
        1.0
    } else {
        green as f64 / tested as f64
    };

    Ok(ProbeRunRecord {
        timestamp_s: Timestamp::now().as_second(),
        wave: wave_label.to_owned(),
        green,
        failed,
        skipped,
        rate,
        outcomes,
    })
}

fn record_call_outcome(
    id: &str,
    op_name: &str,
    min_results: usize,
    res: Result<fsm_core::SrcOutput, fsm_core::FsmError>,
    green: &mut usize,
    failed: &mut usize,
    outcomes: &mut Vec<ProbeSourceOutcome>,
) {
    match res {
        Ok(out) => {
            let (count, _) = serialize_src_output(&out);
            if count >= min_results {
                *green += 1;
                outcomes.push(ProbeSourceOutcome {
                    id: id.to_owned(),
                    op: op_name.to_owned(),
                    status: "green".to_owned(),
                    results: count,
                    min_results,
                    error: None,
                });
            } else {
                *failed += 1;
                outcomes.push(ProbeSourceOutcome {
                    id: id.to_owned(),
                    op: op_name.to_owned(),
                    status: "failed".to_owned(),
                    results: count,
                    min_results,
                    error: Some(format!("got {count} results < min_results {min_results}")),
                });
            }
        }
        Err(e) => {
            *failed += 1;
            outcomes.push(ProbeSourceOutcome {
                id: id.to_owned(),
                op: op_name.to_owned(),
                status: "failed".to_owned(),
                results: 0,
                min_results,
                error: Some(e.to_string()),
            });
        }
    }
}

fn collect_wave_files(
    manifests_root: &Path,
    subdir: &str,
    wave: Option<&str>,
    all: bool,
) -> Result<Vec<(String, PathBuf)>> {
    let base_dir = manifests_root.join(subdir);
    let mut out = Vec::new();
    if all {
        if base_dir.is_dir() {
            for entry in fs::read_dir(&base_dir)? {
                let p = entry?.path();
                if p.is_dir()
                    && let Some(w) = p.file_name().and_then(|n| n.to_str())
                {
                    collect_toml_in_wave(&p, w, &mut out)?;
                }
            }
        }
    } else if let Some(w) = wave {
        let wave_dir = base_dir.join(w);
        if wave_dir.is_dir() {
            collect_toml_in_wave(&wave_dir, w, &mut out)?;
        }
    }
    out.sort();
    Ok(out)
}

fn collect_toml_in_wave(dir: &Path, wave: &str, out: &mut Vec<(String, PathBuf)>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let p = entry?.path();
        if p.is_file() && p.extension().is_some_and(|e| e == "toml") {
            out.push((wave.to_owned(), p));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_run(ts: i64, rate: f64) -> ProbeRunRecord {
        ProbeRunRecord {
            timestamp_s: ts,
            wave: "seed".into(),
            green: (rate * 100.0) as usize,
            failed: 100 - (rate * 100.0) as usize,
            skipped: 0,
            rate,
            outcomes: vec![],
        }
    }

    #[test]
    fn duration_parser_handles_units_and_rejects_bad() {
        assert_eq!(parse_duration_str("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(
            parse_duration_str("1h30m").unwrap(),
            Duration::from_secs(5400)
        );
        assert_eq!(parse_duration_str("45s").unwrap(), Duration::from_secs(45));
        assert_eq!(parse_duration_str("0s").unwrap(), Duration::from_secs(0));
        assert_eq!(parse_duration_str("120").unwrap(), Duration::from_secs(120));
        assert!(parse_duration_str("").is_err());
        assert!(parse_duration_str("10x").is_err());
        assert!(parse_duration_str("1h30").is_err());
    }

    #[test]
    fn qualifying_runs_enforces_min_green_min_gap_and_latest_must_pass_b9() {
        // Two green runs separated by >= 3600s, latest is green -> qualifies (2/2).
        let runs = vec![
            mk_run(1000, 0.90), // below 0.95 -> ignored
            mk_run(2000, 0.96), // qualifies (#1)
            mk_run(4000, 0.98), // only 1600s before latest (< 3600s) -> skipped
            mk_run(5600, 0.95), // latest >= 0.95 -> qualifies (#2, 3600s after 2000)
        ];
        let picked = select_qualifying_runs(&runs, 2, 3600, 0.95);
        assert_eq!(picked.len(), 2);
        assert_eq!(picked[0].timestamp_s, 2000);
        assert_eq!(picked[1].timestamp_s, 5600);

        // B9 regression: 2 older green runs + failing latest run MUST NOT pass!
        let stale_pass = vec![
            mk_run(1000, 1.00),
            mk_run(5000, 1.00),
            mk_run(9000, 0.00), // latest failed!
        ];
        assert!(
            select_qualifying_runs(&stale_pass, 2, 3600, 0.95).is_empty(),
            "stale historical green runs must not mask a failing latest run"
        );
    }

    #[test]
    fn mixed_auth_and_route_requires_key_skip_logic_b9() {
        let toml_src = r#"
id = "mixed_probe"
name = "Mixed Probe"
summary = "Mixed auth manifest."
tier = 2
scopes = ["gov"]
caps = ["search", "lookup"]
transport = "plain"
pool_role = "scope_only"
limits = { rps = 1.0, burst = 2, concurrency = 1, timeout_ms = 5000, cache_ttl_s = 60 }

[auth]
mode = "required"
env = "SEARCH_MCP_MIXED_PROBE_API_KEY"
in = "query"
name = "key"

[op.search]
summary = "Keyed search."
method = "GET"
url = "https://example.test/search"
response = "json"
returns = "hits"
[op.search.params]
query = { type = "string", required = true }
[op.search.map]
items = ".[]"
hit = '{ title: .t, url: .u }'

[op.datasets]
summary = "Keyless metadata."
method = "GET"
url = "https://example.test/datasets"
auth = "none"
response = "json"
returns = "records"
[op.datasets.map]
items = ".[]"
record = "."
"#;
        let decl = ManifestDecl::from_toml(toml_src).unwrap();
        let empty_cfg = Config::default();
        assert!(op_should_skip_live_auth(
            &decl,
            "search",
            decl.op.get("search").unwrap(),
            &empty_cfg
        ));
        assert!(!op_should_skip_live_auth(
            &decl,
            "datasets",
            decl.op.get("datasets").unwrap(),
            &empty_cfg
        ));

        // Optional-key route_requires_key = true source skips `search` when unkeyed.
        let oa = fsm_manifests::embedded_bundle()
            .manifests
            .iter()
            .find(|m| m.manifest.id == "openalex")
            .unwrap()
            .manifest
            .clone();
        assert!(op_should_skip_live_auth(
            &oa,
            "search",
            oa.op.get("search").unwrap(),
            &empty_cfg
        ));
        assert!(!op_should_skip_live_auth(
            &oa,
            "work",
            oa.op.get("work").unwrap(),
            &empty_cfg
        ));
    }

    #[test]
    fn unpaywall_skips_live_when_contact_email_unset_and_polite_bucket_gap_respects_rps() {
        let unpaywall = fsm_manifests::embedded_bundle()
            .manifests
            .iter()
            .find(|m| m.manifest.id == "unpaywall")
            .unwrap()
            .manifest
            .clone();
        let work_op = unpaywall.op.get("work").unwrap();
        let empty_cfg = Config::default();
        assert!(op_should_skip_live_email(&unpaywall, work_op, &empty_cfg));

        let mut with_email = Config::default();
        with_email.contact_email = Some("research@example.org".to_owned());
        assert!(!op_should_skip_live_email(&unpaywall, work_op, &with_email));

        assert_eq!(polite_bucket_gap(0.05), Duration::from_secs(20));
        assert_eq!(polite_bucket_gap(0.8), Duration::from_secs(2));
        assert_eq!(polite_bucket_gap(5.0), Duration::from_secs(1));
    }

    #[tokio::test]
    async fn probe_replay_ignores_host_contact_email_and_includes_instances_b9() {
        let dir = std::env::temp_dir().join(format!("fsm-probe-test-seed-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let args = ProbeArgs {
            wave: Some("seed".into()),
            all: false,
            runs: 1,
            min_gap: Duration::from_secs(0),
            min_green: 0.95,
            replay_only: true,
            state_dir: Some(dir.clone()),
            only: Vec::new(),
        };
        let (passed, run, _summary) = run_probe(&args).await.expect("probe replay seed");
        let _ = fs::remove_dir_all(&dir);
        assert!(passed, "seed replay probe must pass: {run:?}");
        // 26 declarative seed ops + 4 promoted seed protocol instances (dataverse, wikipedia, wikimedia, hal_oai).
        assert!(
            run.green >= 30,
            "expected >= 30 green outcomes (26 seed ops + 4 seed instances), got {}",
            run.green
        );

        // Also probe protocol-instance-only wave `w5e` (B9 / I11): must discover and replay instances!
        let dir_w5e =
            std::env::temp_dir().join(format!("fsm-probe-test-w5e-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir_w5e);
        let w5e_args = ProbeArgs {
            wave: Some("w5e".into()),
            all: false,
            runs: 1,
            min_gap: Duration::from_secs(0),
            min_green: 0.95,
            replay_only: true,
            state_dir: Some(dir_w5e.clone()),
            only: Vec::new(),
        };
        let (w5e_passed, w5e_run, _w5e_summary) =
            run_probe(&w5e_args).await.expect("probe replay w5e");
        let _ = fs::remove_dir_all(&dir_w5e);
        assert!(
            w5e_passed,
            "w5e instance wave replay probe must pass: {w5e_run:?}"
        );
        assert!(
            w5e_run.green >= 30,
            "expected w5e protocol instances to be probed, got {} green",
            w5e_run.green
        );
    }
}
