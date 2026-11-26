//! Offline replay tests across ALL 98 promoted real upstream fixtures for the 9
//! `fsm-protocols-a` adapters (`w5e`, `w5f`, `w5g`, `w5o` — read-only inputs).

use fsm_core::fixture::ReplayNet;
use fsm_core::manifest::InstanceFileDecl;
use fsm_core::{Config, Hit, NullStore, Registry, ReturnKind, ScholarlyWork, SourceDeps, SrcCx};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn manifests_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates dir")
        .join("manifests")
}

fn merge_params(defaults: &BTreeMap<String, Value>, probe: &BTreeMap<String, Value>) -> Value {
    let mut merged = Map::new();
    for (k, v) in defaults {
        merged.insert(k.clone(), v.clone());
    }
    for (k, v) in probe {
        merged.insert(k.clone(), v.clone());
    }
    Value::Object(merged)
}

/// Replay every promoted instance for `protocol` across `w5e`, `w5f`, `w5g`, `w5o`,
/// verifying zero unmatched/unused exchanges, dialect validity, and typed output shapes.
async fn replay_staged_protocol(protocol: &str) -> BTreeMap<String, Vec<Value>> {
    let root = manifests_root();
    let waves = ["w5e", "w5f", "w5g", "w5o"];
    let mut results = BTreeMap::new();

    for wave in waves {
        let instances_dir = root.join("instances").join(wave);
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&instances_dir)
            .unwrap_or_else(|e| panic!("read {}: {e}", instances_dir.display()))
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "toml"))
            .collect();
        entries.sort();

        for toml_path in entries {
            let text = std::fs::read_to_string(&toml_path)
                .unwrap_or_else(|e| panic!("read {}: {e}", toml_path.display()));
            let file = InstanceFileDecl::from_toml(&text)
                .unwrap_or_else(|e| panic!("parse {}: {e}", toml_path.display()));
            if file.protocol != protocol {
                continue;
            }
            file.validate()
                .unwrap_or_else(|e| panic!("validate {}: {e}", toml_path.display()));

            for inst in &file.instance {
                let op = inst
                    .probe
                    .op
                    .as_deref()
                    .expect("validated instance probe has op");
                let fx_path = root
                    .join("fixtures")
                    .join(wave)
                    .join(&inst.id)
                    .join(op)
                    .join("default.json");
                let net = Arc::new(
                    ReplayNet::from_file(&fx_path)
                        .unwrap_or_else(|e| panic!("fixture {}: {e}", fx_path.display())),
                );
                let deps = SourceDeps::new(
                    net.clone(),
                    Arc::new(NullStore::new()),
                    Arc::new(Config::default()),
                );
                let mut reg = Registry::new();
                fsm_protocols_a::register(&mut reg, &deps).expect("register protocols-a");

                let adapter = reg
                    .protocol(&file.protocol)
                    .unwrap_or_else(|| panic!("missing adapter {}", file.protocol));

                adapter.validate_dialect(&inst.dialect).unwrap_or_else(|e| {
                    panic!("validate_dialect {wave}/{protocol}/{}: {e}", inst.id)
                });

                // A1 §D.2 #1 (Round 2 B2): every default_params key must be declared on at least one adapter op.
                for k in inst.default_params.keys() {
                    assert!(
                        adapter.ops().iter().any(|o| o.param(k).is_some()),
                        "{wave}/{protocol} [{}]: default_params key {k:?} not declared on adapter ops",
                        inst.id
                    );
                }

                let op_decl = adapter
                    .ops()
                    .iter()
                    .find(|o| o.name == op)
                    .unwrap_or_else(|| panic!("adapter {protocol} missing op {op}"));

                let params = merge_params(&inst.default_params, &inst.probe.params);
                let mut cx = SrcCx::from_config(
                    &deps.cfg,
                    CancellationToken::new(),
                    Duration::from_secs(10),
                );
                cx.dialect = inst.dialect.clone();
                cx.replay_mode = true;

                let out = adapter
                    .call(&inst.base_url, op, params, &cx)
                    .await
                    .unwrap_or_else(|e| {
                        panic!(
                            "replay failed for {wave}/{protocol} [{}] ({op}): {e}",
                            inst.id
                        )
                    });

                assert_eq!(out.kind, protocol);
                assert_eq!(out.op, op);
                assert!(
                    net.unmatched().is_empty(),
                    "{wave}/{protocol} [{}]: unmatched {:?}",
                    inst.id,
                    net.unmatched()
                );
                assert!(
                    net.unused().is_empty(),
                    "{wave}/{protocol} [{}]: unused {:?}",
                    inst.id,
                    net.unused()
                );
                assert!(
                    out.items.len() >= inst.probe.min_results,
                    "{wave}/{protocol} [{}]: got {} items, need >= {}",
                    inst.id,
                    out.items.len(),
                    inst.probe.min_results
                );

                for item in &out.items {
                    match op_decl.returns {
                        ReturnKind::Works => {
                            let work: ScholarlyWork = serde_json::from_value(item.clone())
                                .unwrap_or_else(|e| {
                                    panic!("{wave}/{protocol} [{}]: bad Work: {e}\n{item}", inst.id)
                                });
                            assert!(!work.title.trim().is_empty());
                        }
                        ReturnKind::Hits => {
                            // M1: adapters must omit "kind" so InstanceSource applies scope defaults.
                            assert!(
                                item.get("kind").is_none(),
                                "{wave}/{protocol} [{}]: unexpected serialized kind field in {item}",
                                inst.id
                            );
                            let hit: Hit =
                                serde_json::from_value(item.clone()).unwrap_or_else(|e| {
                                    panic!("{wave}/{protocol} [{}]: bad Hit: {e}\n{item}", inst.id)
                                });
                            assert!(!hit.title.trim().is_empty());
                            assert!(
                                hit.url.starts_with("http://") || hit.url.starts_with("https://"),
                                "{wave}/{protocol} [{}]: bad url {:?}",
                                inst.id,
                                hit.url
                            );
                            if protocol == "mediawiki" {
                                assert!(!hit.snippet.contains("<span"));
                                // I2: list=search revision timestamps must not populate Hit.date.
                                assert_eq!(
                                    hit.date, None,
                                    "{wave}/{protocol} [{}]: revision timestamp leaked into hit.date",
                                    inst.id
                                );
                                assert!(
                                    hit.extra
                                        .as_ref()
                                        .and_then(|e| e.get("lastEdited"))
                                        .is_some(),
                                    "{wave}/{protocol} [{}]: expected extra.lastEdited",
                                    inst.id
                                );
                            }
                        }
                        ReturnKind::Records => {
                            let obj = item.as_object().expect("record object");
                            assert!(!obj.is_empty());
                        }
                        _ => {}
                    }
                }

                results.insert(inst.id.clone(), out.items);
            }
        }
    }

    assert!(
        results.len() >= 3,
        "protocol {protocol} replayed {} instances (need >= 3)",
        results.len()
    );
    results
}

#[tokio::test]
async fn replay_staged_oai_pmh_24_instances() {
    let res = replay_staged_protocol("oai_pmh").await;
    assert_eq!(res.len(), 24);
    assert_eq!(res["doab_oai"][0]["repositoryName"], "DOAB PROD Instance");
}

#[tokio::test]
async fn replay_staged_sru_7_instances_including_ndl_string_packed_dcndl() {
    let res = replay_staged_protocol("sru").await;
    assert_eq!(res.len(), 7);
    let ndl_work: ScholarlyWork = serde_json::from_value(res["ndl_sru"][0].clone()).unwrap();
    assert!(!ndl_work.title.is_empty());
    assert!(ndl_work.ids.doi.is_some());
    // ANU sentinel date "9999" must be rejected as None, and ISBD place stubs ending in ":" skipped (I3).
    let anu_work: ScholarlyWork = serde_json::from_value(res["anu_sru"][0].clone()).unwrap();
    assert_eq!(anu_work.year, None);
    assert_eq!(
        anu_work.venue.as_deref(),
        Some("Dept. of Mines and Energy.")
    );
    for raw in &res["anu_sru"] {
        let w: ScholarlyWork = serde_json::from_value(raw.clone()).unwrap();
        if let Some(v) = &w.venue {
            assert!(
                !v.ends_with(':'),
                "ISBD place stub leaked into venue: {v:?}"
            );
        }
    }
    // M8: LoC catalog date "1994." parses to year 1994.
    let loc_work2: ScholarlyWork = serde_json::from_value(res["loc_sru"][2].clone()).unwrap();
    assert_eq!(loc_work2.year, Some(1994));
    assert_eq!(loc_work2.published_date.as_deref(), Some("1994"));
}

#[tokio::test]
async fn replay_staged_opensearch_7_two_exchange_instances() {
    let res = replay_staged_protocol("opensearch").await;
    assert_eq!(res.len(), 7);
    let aodn_hit: Hit = serde_json::from_value(res["aodn_imos"][0].clone()).unwrap();
    assert_eq!(aodn_hit.date.as_deref(), Some("2026-11-25"));
}

#[tokio::test]
async fn replay_staged_sparql_9_instances() {
    let res = replay_staged_protocol("sparql").await;
    assert_eq!(res.len(), 9);
    assert_eq!(
        res["wikidata_scholarly"][0]["item"],
        "http://www.wikidata.org/entity/Q34460861"
    );
}

#[tokio::test]
async fn replay_staged_dspace7_21_instances() {
    let res = replay_staged_protocol("dspace7").await;
    assert_eq!(res.len(), 21);
    let who: ScholarlyWork = serde_json::from_value(res["who_iris"][0].clone()).unwrap();
    assert!(who.title.contains("Malaria"));
    assert_eq!(who.year, Some(2007));

    // I4: positive OA metadata yields oa=Some("green"); restricted or rights-less archived items yield oa=None.
    let ntu0: ScholarlyWork = serde_json::from_value(res["dr_ntu_sg"][0].clone()).unwrap();
    assert!(ntu0.oa.is_some(), "open.access item should be OA");
    let ntu1: ScholarlyWork = serde_json::from_value(res["dr_ntu_sg"][1].clone()).unwrap();
    assert!(ntu1.oa.is_none(), "restricted archived item must not be OA");
    let uct0: ScholarlyWork = serde_json::from_value(res["open_uct"][0].clone()).unwrap();
    assert!(
        uct0.oa.is_none(),
        "rights-less archived item must not be OA"
    );
    let wb0: ScholarlyWork = serde_json::from_value(res["worldbank_okr"][0].clone()).unwrap();
    assert!(wb0.oa.is_some(), "CC BY 3.0 IGO item should be OA");
}

#[tokio::test]
async fn replay_staged_inveniordm_6_instances_including_aperta_zenodo_compat() {
    let res = replay_staged_protocol("inveniordm").await;
    assert_eq!(res.len(), 6);
    let caltech: ScholarlyWork = serde_json::from_value(res["caltechdata"][0].clone()).unwrap();
    assert!(caltech.ids.doi.is_some());
    let aperta: ScholarlyWork =
        serde_json::from_value(res["aperta_tubitak_tr"][0].clone()).unwrap();
    assert_eq!(
        aperta.ids.doi.as_deref(),
        Some("10.1016/j.tsep.2024.102971")
    );
    assert!(!aperta.authors.is_empty());

    // Round 2 I1: nyu_ultraviolet[0] (status=open) is OA; [1] (status=metadata-only, files=public) is NOT OA.
    let nyu0: ScholarlyWork = serde_json::from_value(res["nyu_ultraviolet"][0].clone()).unwrap();
    assert!(nyu0.oa.is_some(), "open nyu_ultraviolet[0] should be OA");
    let nyu1: ScholarlyWork = serde_json::from_value(res["nyu_ultraviolet"][1].clone()).unwrap();
    assert!(
        nyu1.oa.is_none(),
        "metadata-only nyu_ultraviolet[1] must not be OA, got {:?}",
        nyu1.oa
    );
}

#[tokio::test]
async fn replay_staged_dataverse_12_instances() {
    let res = replay_staged_protocol("dataverse").await;
    assert_eq!(res.len(), 12);
    assert_eq!(res["borealis_ca"][0]["globalId"], "doi:10.5683/SP/MW6OKX");
}

#[tokio::test]
async fn replay_staged_eprints_5_instances_including_lmu_multilingual_title() {
    let res = replay_staged_protocol("eprints").await;
    assert_eq!(res.len(), 5);
    let lmu: ScholarlyWork = serde_json::from_value(res["eprints_lmu"][0].clone()).unwrap();
    assert!(lmu.title.contains("multiterawatt fibre lasers"));
    assert_eq!(lmu.year, Some(2014));
    assert_eq!(lmu.ids.doi.as_deref(), Some("10.1038/lsa.2014.92"));
    // Round 2 I3: format="application/pdf" repos populate oa.pdf_url.
    assert!(
        lmu.oa.as_ref().and_then(|o| o.pdf_url.as_deref()).is_some(),
        "eprints_lmu[0] should populate oa.pdf_url"
    );
    let utas: ScholarlyWork = serde_json::from_value(res["eprints_utas"][0].clone()).unwrap();
    assert!(
        utas.oa
            .as_ref()
            .and_then(|o| o.pdf_url.as_deref())
            .is_some(),
        "eprints_utas[0] should populate oa.pdf_url"
    );
}

#[tokio::test]
async fn replay_staged_mediawiki_7_instances_including_archwiki_api_php() {
    let res = replay_staged_protocol("mediawiki").await;
    assert_eq!(res.len(), 7);

    // B2: non-/w/api.php wikis use index.php?title=...; /w/api.php wikis keep /wiki/...
    let arch0: Hit = serde_json::from_value(res["archwiki"][0].clone()).unwrap();
    assert_eq!(arch0.title, "Systemd");
    assert_eq!(
        arch0.url,
        "https://wiki.archlinux.org/index.php?title=Systemd"
    );
    // I1: no spurious spaces injected around inline <span class="searchmatch"> tags.
    assert!(
        arch0.snippet.contains("[[de:Systemd]]"),
        "got {:?}",
        arch0.snippet
    );
    let arch2: Hit = serde_json::from_value(res["archwiki"][2].clone()).unwrap();
    assert!(
        arch2.snippet.contains("systemd-tmpfiles"),
        "got {:?}",
        arch2.snippet
    );

    let osm0: Hit = serde_json::from_value(res["osm_wiki"][0].clone()).unwrap();
    assert_eq!(
        osm0.url,
        "https://wiki.openstreetmap.org/wiki/Key%3Ahighway"
    );
    let osm2: Hit = serde_json::from_value(res["osm_wiki"][2].clone()).unwrap();
    assert!(
        osm2.snippet.contains("highway=crossing"),
        "got {:?}",
        osm2.snippet
    );

    let wikt1: Hit = serde_json::from_value(res["wiktionary"][1].clone()).unwrap();
    assert!(
        wikt1.snippet.contains("algorithms)"),
        "got {:?}",
        wikt1.snippet
    );

    let ws2: Hit = serde_json::from_value(res["wikisource"][2].clone()).unwrap();
    assert!(
        ws2.snippet.contains("Constitution."),
        "got {:?}",
        ws2.snippet
    );

    // I2: wikinews COP15 (2009) must not receive its 2024 revision timestamp as publication date.
    let wn0: Hit = serde_json::from_value(res["wikinews"][0].clone()).unwrap();
    assert_eq!(wn0.date, None);
    assert_eq!(
        wn0.extra
            .as_ref()
            .and_then(|e| e.get("lastEdited"))
            .and_then(Value::as_str),
        Some("2024-12-18")
    );
}
