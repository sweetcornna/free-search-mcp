//! Offline manifest replay acceptance suite (`SPEC-lanes.md` W2a / M11).
//!
//! Run for the 12 seed manifests:
//!   WAVE=seed cargo test -p fsm-sources --test manifest_replay --locked --offline
//! Or across all promoted waves:
//!   cargo test -p fsm-sources --test manifest_replay --locked --offline

use fsm_core::fixture::{Fixture, ReplayNet};
use fsm_core::manifest::ManifestDecl;
use fsm_core::{
    CancellationToken, Config, NullStore, PoolRole, ReturnKind, Source, SourceDeps, SrcCx,
    SrcOutput, VenueProfile,
};
use fsm_sources::{ManifestSource, OAuth2TokenCache};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

fn manifests_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("manifests")
}

fn collect_wave_manifests(root: &Path, wave_filter: Option<&str>) -> Vec<(String, PathBuf)> {
    let sources_dir = root.join("sources");
    let mut wave_dirs: Vec<PathBuf> = fs::read_dir(&sources_dir)
        .expect("crates/manifests/sources directory must exist")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    wave_dirs.sort();

    let mut out = Vec::new();
    for dir in wave_dirs {
        let wave = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_owned();
        if let Some(want) = wave_filter
            && wave != want
        {
            continue;
        }
        let mut files: Vec<PathBuf> = fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_file() && p.extension().is_some_and(|ext| ext == "toml"))
            .collect();
        files.sort();
        for f in files {
            out.push((wave.clone(), f));
        }
    }
    out
}

#[tokio::test]
async fn replay_promoted_manifests() {
    let root = manifests_root();
    let wave_filter = std::env::var("WAVE")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());

    let files = collect_wave_manifests(&root, wave_filter.as_deref());
    assert!(
        !files.is_empty(),
        "no promoted manifests found under {}/sources (WAVE={wave_filter:?})",
        root.display()
    );
    if wave_filter.as_deref() == Some("seed") {
        assert_eq!(
            files.len(),
            12,
            "expected exactly 12 seed manifests under sources/seed"
        );
    }

    let embedded_ids: BTreeSet<String> = fsm_manifests::embedded_bundle()
        .manifests
        .iter()
        .map(|m| m.manifest.id.clone())
        .collect();

    let mut total_ops = 0usize;
    for (wave, path) in &files {
        let text = fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let decl = ManifestDecl::parse_validated(&text)
            .unwrap_or_else(|e| panic!("invalid manifest {}: {e}", path.display()));
        assert!(
            embedded_ids.contains(&decl.id),
            "manifest {} (from {}) missing from build.rs embedded_manifests()",
            decl.id,
            path.display()
        );

        let probed_ops: Vec<(&str, &fsm_core::manifest::OpSpec)> = decl
            .op
            .iter()
            .filter(|(_, op)| op.probe.is_some())
            .collect();
        assert!(
            !probed_ops.is_empty(),
            "manifest {} has no probed ops",
            decl.id
        );

        for (op_name, op) in probed_ops {
            total_ops += 1;
            let probe = op.probe.as_ref().expect("checked is_some");
            let fixture_rel = probe.fixture_path(wave, &decl.id, op_name);
            let fixture_path = root.join(&fixture_rel);
            assert!(
                fixture_path.is_file(),
                "{}.{op_name}: missing fixture {}",
                decl.id,
                fixture_path.display()
            );

            let net = Arc::new(ReplayNet::from_file(&fixture_path).unwrap_or_else(|e| {
                panic!(
                    "{}.{op_name}: failed to load fixture {}: {e}",
                    decl.id,
                    fixture_path.display()
                )
            }));
            let cfg = Arc::new(Config::default());
            let deps = SourceDeps::new(net.clone(), Arc::new(NullStore::new()), Arc::clone(&cfg));
            let src = ManifestSource::new(decl.clone(), deps, Arc::new(OAuth2TokenCache::new()));

            let mut scx =
                SrcCx::from_config(&cfg, CancellationToken::new(), Duration::from_secs(15));
            scx.replay_mode = true;
            scx.lang = Some("en".to_owned());

            let params = Value::Object(probe.params.clone().into_iter().collect());
            let output = src.call(op_name, params, &scx).await.unwrap_or_else(|e| {
                panic!(
                    "{}.{op_name} (fixture {}): replay failed: {e}",
                    decl.id,
                    fixture_path.display()
                )
            });

            assert!(
                net.unmatched().is_empty(),
                "{}.{op_name}: unmatched replay requests: {:?}",
                decl.id,
                net.unmatched()
            );
            assert!(
                !net.hits().is_empty(),
                "{}.{op_name}: ReplayNet recorded zero matched exchanges",
                decl.id
            );

            match &output {
                SrcOutput::Hits(hits) => {
                    assert!(
                        hits.len() >= probe.min_results,
                        "{}.{op_name}: got {} hits, expected >= {}",
                        decl.id,
                        hits.len(),
                        probe.min_results
                    );
                    for h in hits {
                        assert!(
                            !h.title.is_empty(),
                            "{}.{op_name}: empty hit title",
                            decl.id
                        );
                        assert!(!h.url.is_empty(), "{}.{op_name}: empty hit url", decl.id);
                        assert_eq!(h.source, decl.id);
                    }
                    // M3 semantic check: mwmbl.search decodes HTML entities (`&#62;` -> `>`).
                    if decl.id == "mwmbl" && op_name == "search" {
                        assert!(
                            hits.iter().all(|h| !h.title.contains("&#62;")),
                            "mwmbl.search must decode HTML entities in titles"
                        );
                    }
                }
                SrcOutput::Works(works) => {
                    assert!(
                        works.len() >= probe.min_results,
                        "{}.{op_name}: got {} works, expected >= {}",
                        decl.id,
                        works.len(),
                        probe.min_results
                    );
                    for w in works {
                        assert!(
                            !w.title.is_empty(),
                            "{}.{op_name}: empty work title",
                            decl.id
                        );
                        assert!(
                            w.sources.contains(&decl.id),
                            "{}.{op_name}: work.sources {:?} missing {:?}",
                            decl.id,
                            w.sources,
                            decl.id
                        );
                    }
                    // B6 semantic check: arxiv.search uses phrase+AND query (no stopword-OR noise),
                    // so all top hits genuinely match "Attention Is All You Need".
                    if decl.id == "arxiv" && op_name == "search" {
                        assert!(
                            works.iter().all(|w| w
                                .title
                                .to_ascii_lowercase()
                                .contains("attention is all you need")),
                            "arxiv.search probe must return on-topic 'Attention Is All You Need' works (not stopword noise), got: {:?}",
                            works.iter().map(|w| &w.title).collect::<Vec<_>>()
                        );
                    }
                    // I4 semantic check: openaire.work extracts open Harvard PDF, not closed Springer TDM.
                    if decl.id == "openaire" && op_name == "work" {
                        let oa = works[0]
                            .oa
                            .as_ref()
                            .expect("openaire.work must have OA info");
                        assert!(
                            oa.pdf_url.as_deref().is_some_and(
                                |u| u.contains("dash.harvard.edu") && u.ends_with(".pdf")
                            ),
                            "openaire.work expected Harvard PDF URL, got {:?}",
                            oa.pdf_url
                        );
                        assert!(
                            oa.license.as_deref() != Some("http://www.springer.com/tdm"),
                            "openaire.work must not emit closed Springer TDM license"
                        );
                    }
                    // I8 semantic check: semanticscholar.search handles empty openAccessPdf.url & arXiv DOI preprint.
                    if decl.id == "semanticscholar" && op_name == "search" {
                        let vit = works
                            .iter()
                            .find(|w| w.title.contains("16x16"))
                            .expect("expected ViT paper in semanticscholar.search fixture");
                        assert_eq!(vit.work_type, "preprint");
                        let oa = vit.oa.as_ref().expect("ViT OA metadata");
                        assert!(
                            oa.url.is_none(),
                            "empty openAccessPdf.url must map to None, got {:?}",
                            oa.url
                        );
                        assert!(
                            oa.pdf_url.is_none(),
                            "empty openAccessPdf.url must map to None, got {:?}",
                            oa.pdf_url
                        );
                    }
                }
                SrcOutput::Records { items, hits, .. } => {
                    assert!(
                        items.len() >= probe.min_results,
                        "{}.{op_name}: got {} records, expected >= {}",
                        decl.id,
                        items.len(),
                        probe.min_results
                    );
                    if op_name == "search" && op.returns == ReturnKind::Records {
                        let synth_hits = hits.as_ref().unwrap_or_else(|| {
                            panic!(
                                "{}.{op_name}: search+records missing mapped hits (M3)",
                                decl.id
                            )
                        });
                        assert!(
                            synth_hits.len() >= probe.min_results,
                            "{}.{op_name}: got {} hits for search+records, expected >= {}",
                            decl.id,
                            synth_hits.len(),
                            probe.min_results
                        );
                    }
                }
                SrcOutput::Text(t) => {
                    assert!(!t.trim().is_empty(), "{}.{op_name}: empty text", decl.id);
                }
                SrcOutput::Raw(v) => {
                    assert!(!v.is_null(), "{}.{op_name}: null raw output", decl.id);
                }
                _ => {}
            }
        }
    }

    assert!(total_ops > 0, "expected at least one replayed op");
}

#[tokio::test]
async fn seed_specific_contracts_and_oracle_regressions() {
    if let Ok(w) = std::env::var("WAVE")
        && !w.trim().is_empty()
        && w.trim() != "seed"
    {
        return;
    }

    let embedded = &fsm_manifests::embedded_bundle().manifests;
    let expected_seeds = [
        "arxiv",
        "crossref",
        "openaire",
        "europepmc",
        "openalex",
        "semanticscholar",
        "unpaywall",
        "pmc_idconv",
        "doaj",
        "datacite",
        "opencitations",
        "mwmbl",
    ];
    for id in expected_seeds {
        assert!(
            embedded.iter().any(|m| m.manifest.id == id),
            "seed manifest {id:?} missing from embedded_bundle()"
        );
    }

    let get_manifest = |id: &str| -> ManifestDecl {
        embedded
            .iter()
            .find(|m| m.manifest.id == id)
            .map(|m| m.manifest.clone())
            .unwrap_or_else(|| panic!("missing manifest {id}"))
    };

    // SPEC §6: OpenAIRE MUST use /graph/v3/research-products.
    let openaire = get_manifest("openaire");
    assert!(
        openaire
            .op
            .get("search")
            .unwrap()
            .url
            .contains("/graph/v3/research-products"),
        "openaire must use /graph/v3/research-products"
    );
    assert_eq!(openaire.pool_role, PoolRole::Default);

    // SPEC §6: OpenAlex & Semantic Scholar route_requires_key = true, ScopeOnly.
    let oa = get_manifest("openalex");
    assert_eq!(oa.pool_role, PoolRole::ScopeOnly);
    assert!(oa.auth.as_ref().is_some_and(|a| a.route_requires_key));
    let s2 = get_manifest("semanticscholar");
    assert_eq!(s2.pool_role, PoolRole::ScopeOnly);
    assert!(s2.auth.as_ref().is_some_and(|a| a.route_requires_key));

    // S13: mwmbl is reserve pool, web scope.
    let mwmbl = get_manifest("mwmbl");
    assert_eq!(mwmbl.pool_role, PoolRole::Reserve);
    assert_eq!(mwmbl.scopes, vec!["web"]);

    // S5 & S6: arXiv legacy ID regex (`hep-th/9901001v2` -> `hep-th/9901001`) + sortOrder=descending.
    let arxiv = get_manifest("arxiv");
    assert_eq!(
        arxiv
            .op
            .get("search")
            .unwrap()
            .const_query
            .get("sortOrder")
            .and_then(|v| v.values().first().copied()),
        Some("descending")
    );
    let legacy_feed = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom" xmlns:arxiv="http://arxiv.org/schemas/atom">
  <entry>
    <id>http://arxiv.org/abs/hep-th/9901001v2</id>
    <published>1999-01-01T12:00:00Z</published>
    <title>Legacy String Theory Paper</title>
    <summary>Abstract text here.</summary>
    <author><name>Jane Doe</name></author>
    <arxiv:doi>10.1000/legacy.1</arxiv:doi>
  </entry>
</feed>"#;
    let fx_arxiv = Fixture::from_json(
        &json!({
            "version": 1,
            "source": "arxiv",
            "op": "work",
            "recordedAt": "2026-11-25T00:00:00Z",
            "exchanges": [{
                "request": {
                    "method": "GET",
                    "url": "https://export.arxiv.org/api/query?id_list=hep-th%2F9901001",
                    "headersSubset": {},
                    "bodySha256": null
                },
                "response": {
                    "status": 200,
                    "headers": [["content-type", "application/atom+xml"]],
                    "bodyText": legacy_feed,
                    "bodyB64": null
                }
            }]
        })
        .to_string(),
    )
    .unwrap();
    let cfg = Arc::new(Config::default());
    let net = Arc::new(ReplayNet::from_fixtures(vec![fx_arxiv]).unwrap());
    let src = ManifestSource::new(
        arxiv,
        SourceDeps::new(net, Arc::new(NullStore::new()), Arc::clone(&cfg)),
        Arc::new(OAuth2TokenCache::new()),
    );
    let mut scx = SrcCx::from_config(&cfg, CancellationToken::new(), Duration::from_secs(5));
    scx.replay_mode = true;
    let out = src
        .call("work", json!({ "id": "hep-th/9901001" }), &scx)
        .await
        .expect("legacy arXiv lookup should succeed");
    match out {
        SrcOutput::Works(ws) => {
            assert_eq!(ws.len(), 1);
            assert_eq!(ws[0].ids.arxiv.as_deref(), Some("hep-th/9901001"));
            assert_eq!(ws[0].ids.doi.as_deref(), Some("10.1000/legacy.1"));
            assert_eq!(ws[0].url, "https://arxiv.org/abs/hep-th/9901001");
        }
        other => panic!("expected Works, got {other:?}"),
    }

    // Oracle regression (crossref.py): date-parts truncates at first null & never pads year-only to -01-01.
    let crossref = get_manifest("crossref");
    let cr_body = json!({
        "message": {
            "items": [
                {
                    "DOI": "10.1000/year-only",
                    "title": ["Year Only Paper"],
                    "URL": "https://doi.org/10.1000/year-only",
                    "type": "journal-article",
                    "issued": { "date-parts": [[2024]] },
                    "abstract": "<jats:p>Clean <b>abstract</b> text.</jats:p>"
                },
                {
                    "DOI": "10.1000/null-gap",
                    "title": ["Null Gap Paper"],
                    "URL": "https://doi.org/10.1000/null-gap",
                    "type": "journal-article",
                    "issued": { "date-parts": [[2024, null, 5]] }
                }
            ]
        }
    })
    .to_string();
    let fx_cr = Fixture::from_json(
        &json!({
            "version": 1,
            "source": "crossref",
            "op": "search",
            "recordedAt": "2026-11-25T00:00:00Z",
            "exchanges": [{
                "request": {
                    "method": "GET",
                    "url": "https://api.crossref.org/works?filter=type%3Ajournal-article%2Ctype%3Aproceedings-article%2Ctype%3Aposted-content%2Ctype%3Abook-chapter&query=test&rows=2",
                    "headersSubset": { "accept": "application/json" },
                    "bodySha256": null
                },
                "response": {
                    "status": 200,
                    "headers": [["content-type", "application/json"]],
                    "bodyText": cr_body,
                    "bodyB64": null
                }
            }]
        })
        .to_string(),
    )
    .unwrap();
    let net_cr = Arc::new(ReplayNet::from_fixtures(vec![fx_cr]).unwrap());
    let src_cr = ManifestSource::new(
        crossref,
        SourceDeps::new(net_cr, Arc::new(NullStore::new()), Arc::clone(&cfg)),
        Arc::new(OAuth2TokenCache::new()),
    );
    let out_cr = src_cr
        .call("search", json!({ "query": "test", "limit": 2 }), &scx)
        .await
        .expect("crossref search");
    match out_cr {
        SrcOutput::Works(ws) => {
            assert_eq!(ws.len(), 2);
            assert_eq!(ws[0].published_date.as_deref(), Some("2024"));
            assert_eq!(ws[0].abstract_text.as_deref(), Some("Clean abstract text."));
            assert_eq!(ws[1].published_date.as_deref(), Some("2024"));
        }
        other => panic!("expected Works, got {other:?}"),
    }

    // M13: doaj.journals output deserializes cleanly into core::VenueProfile.
    let doaj = get_manifest("doaj");
    let doaj_fx = manifests_root().join("fixtures/seed/doaj/journals/default.json");
    let net_doaj = Arc::new(ReplayNet::from_file(&doaj_fx).expect("load doaj journals fixture"));
    let src_doaj = ManifestSource::new(
        doaj,
        SourceDeps::new(net_doaj, Arc::new(NullStore::new()), Arc::clone(&cfg)),
        Arc::new(OAuth2TokenCache::new()),
    );
    let out_doaj = src_doaj
        .call(
            "journals",
            json!({ "query": "issn:1932-6203", "limit": 3 }),
            &scx,
        )
        .await
        .expect("doaj.journals replay");
    match out_doaj {
        SrcOutput::Records { items, .. } => {
            assert!(!items.is_empty());
            let vp: VenueProfile =
                serde_json::from_value(items[0].clone()).expect("M13 VenueProfile deserialize");
            assert_eq!(vp.doaj_vetted, Some(true));
            assert!(vp.issn.iter().any(|s| s == "1932-6203"));
            assert!(vp.sources.contains(&"doaj".to_owned()));
        }
        other => panic!("expected Records, got {other:?}"),
    }
}

// ===========================================================================
// Ported Python oracle tests (from tests/test_academic_engines.py) & Round 1
// seed manifest regressions (B6, B7, I3, I4, I8, I9, M3, M4)
// ===========================================================================

struct InspectNet {
    body: String,
    content_type: String,
    status: u16,
    captured: tokio::sync::Mutex<Vec<fsm_core::HttpRequest>>,
}

impl InspectNet {
    fn json(body: Value) -> Self {
        Self {
            body: body.to_string(),
            content_type: "application/json".to_owned(),
            status: 200,
            captured: tokio::sync::Mutex::new(Vec::new()),
        }
    }

    fn xml(body: impl Into<String>) -> Self {
        Self {
            body: body.into(),
            content_type: "application/atom+xml".to_owned(),
            status: 200,
            captured: tokio::sync::Mutex::new(Vec::new()),
        }
    }

    async fn last_req(&self) -> fsm_core::HttpRequest {
        self.captured
            .lock()
            .await
            .last()
            .cloned()
            .expect("expected at least one HTTP request")
    }
}

#[async_trait::async_trait]
impl fsm_core::NetClient for InspectNet {
    async fn send(
        &self,
        req: fsm_core::HttpRequest,
        _profile: fsm_core::TransportProfile,
        _cx: &fsm_core::NetCx,
    ) -> Result<fsm_core::HttpBytes, fsm_core::FsmError> {
        let url = req.url.clone();
        self.captured.lock().await.push(req);
        Ok(fsm_core::HttpBytes::new(
            url,
            self.status,
            vec![("content-type".into(), self.content_type.clone())],
            bytes::Bytes::from(self.body.clone()),
        ))
    }

    fn check_url_offline(&self, _url: &str) -> Result<(), fsm_core::FsmError> {
        Ok(())
    }
}

fn seed_manifest(id: &str) -> ManifestDecl {
    fsm_manifests::embedded_bundle()
        .manifests
        .iter()
        .find(|m| m.manifest.id == id)
        .map(|m| m.manifest.clone())
        .unwrap_or_else(|| panic!("missing seed manifest {id}"))
}

fn query_val<'a>(req: &'a fsm_core::HttpRequest, key: &str) -> Option<&'a str> {
    req.query
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

#[tokio::test]
async fn test_arxiv_build_url_encodes_query_and_clamps_and_query_shapes_b6_m4() {
    let decl = seed_manifest("arxiv");
    let cfg = Arc::new(Config::default());
    let sample_feed = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <entry>
    <id>
      http://arxiv.org/abs/1706.03762v7
    </id>
    <title>Attention Is All You Need</title>
    <summary>Dominant sequence transduction models.</summary>
    <published>2017-06-12T18:00:00Z</published>
    <author><name>Ashish Vaswani</name></author>
  </entry>
</feed>"#;
    let net = Arc::new(InspectNet::xml(sample_feed));
    let src = ManifestSource::new(
        decl,
        SourceDeps::new(net.clone(), Arc::new(NullStore::new()), Arc::clone(&cfg)),
        Arc::new(OAuth2TokenCache::new()),
    );
    let scx = SrcCx::from_config(&cfg, CancellationToken::new(), Duration::from_secs(5));

    // 1. Phrase OR AND-ed non-stopwords (Vaswani regression B6) + whitespace-padded <id> (M4)
    match src
        .call(
            "search",
            json!({ "query": "attention is all you need", "limit": 5 }),
            &scx,
        )
        .await
        .unwrap()
    {
        SrcOutput::Works(ws) => {
            assert_eq!(ws.len(), 1);
            assert_eq!(ws[0].ids.arxiv.as_deref(), Some("1706.03762"));
            assert_eq!(ws[0].published_date.as_deref(), Some("2017-06-12"));
        }
        other => panic!("unexpected {other:?}"),
    }
    let req = net.last_req().await;
    assert_eq!(
        query_val(&req, "search_query"),
        Some(r#"all:"attention is all you need" OR (all:attention AND all:need)"#)
    );
    assert_eq!(query_val(&req, "max_results"), Some("5"));
    assert_eq!(query_val(&req, "sortBy"), Some("relevance"));
    assert_eq!(query_val(&req, "sortOrder"), Some("descending"));

    // 2. Query shapes (oracle test_arxiv_query_shapes)
    let cases = [
        (
            "reciprocal rank fusion",
            r#"all:"reciprocal rank fusion" OR (all:reciprocal AND all:rank AND all:fusion)"#,
        ),
        ("GAN vs LLM", r#"all:"GAN vs LLM" OR (all:GAN AND all:LLM)"#),
        ("transformers", "all:transformers"),
        (
            r#""residual learning" imagenet"#,
            r#"all:"residual learning" imagenet"#,
        ),
        ("au:Vaswani", "au:Vaswani"),
        ("to be or not", "all:to be or not"),
    ];
    for (q, want) in cases {
        src.call("search", json!({ "query": q, "limit": 2 }), &scx)
            .await
            .unwrap();
        let r = net.last_req().await;
        assert_eq!(query_val(&r, "search_query"), Some(want), "query={q:?}");
    }

    // 8-term cap -> 7 " AND " operators
    src.call(
        "search",
        json!({ "query": "one two three four five six seven eight nine ten", "limit": 2 }),
        &scx,
    )
    .await
    .unwrap();
    let long_sq = query_val(&net.last_req().await, "search_query")
        .unwrap()
        .to_owned();
    assert_eq!(long_sq.matches(" AND ").count(), 7, "long_sq={long_sq}");

    // 3. Freshness switches sortBy to submittedDate (oracle test_arxiv_freshness_switches_to_newest_first)
    let mut fresh_cx = scx.clone();
    fresh_cx.freshness = Some(fsm_core::Freshness::Week);
    src.call(
        "search",
        json!({ "query": "transformers", "limit": 5 }),
        &fresh_cx,
    )
    .await
    .unwrap();
    let fresh_req = net.last_req().await;
    assert_eq!(query_val(&fresh_req, "sortBy"), Some("submittedDate"));
    assert_eq!(query_val(&fresh_req, "sortOrder"), Some("descending"));

    // 4. Malformed & empty XML return FsmError::Upstream under R2 (oracle test_arxiv_malformed_xml_returns_empty)
    for bad_xml in ["<feed><entry>", ""] {
        let bad_net = Arc::new(InspectNet::xml(bad_xml));
        let bad_src = ManifestSource::new(
            seed_manifest("arxiv"),
            SourceDeps::new(bad_net, Arc::new(NullStore::new()), Arc::clone(&cfg)),
            Arc::new(OAuth2TokenCache::new()),
        );
        let err = bad_src
            .call("search", json!({ "query": "x", "limit": 1 }), &scx)
            .await
            .unwrap_err();
        assert!(
            matches!(err, fsm_core::FsmError::Upstream { .. }),
            "expected Upstream error for malformed XML {bad_xml:?}, got {err:?}"
        );
    }
}

#[tokio::test]
async fn test_crossref_oracle_dates_mailto_freshness_retractions_and_empty_title_b7_i9_m4() {
    let body = json!({
        "message": {
            "items": [
                {
                    "DOI": "10.1000/null-date",
                    "title": ["No Date At All"],
                    "URL": "https://doi.org/10.1000/null-date",
                    "issued": { "date-parts": [[null]] }
                },
                {
                    "DOI": "10.1000/oob-zero",
                    "title": ["Zero Month Day"],
                    "URL": "https://doi.org/10.1000/oob-zero",
                    "issued": { "date-parts": [[2024, 0, 0]] }
                },
                {
                    "DOI": "10.1000/oob-high",
                    "title": ["Invalid Month 13"],
                    "URL": "https://doi.org/10.1000/oob-high",
                    "issued": { "date-parts": [[2024, 13, 40]] }
                },
                {
                    "DOI": "10.1000/retracted-paper",
                    "title": ["Retracted Study"],
                    "URL": "https://doi.org/10.1000/retracted-paper",
                    "issued": { "date-parts": [[2023, 5, 10]] },
                    "updated-by": [{ "type": "retraction", "DOI": "10.1000/ret-notice" }]
                },
                {
                    "DOI": "10.1000/ret-notice",
                    "title": ["Retraction Notice: Retracted Study"],
                    "URL": "https://doi.org/10.1000/ret-notice",
                    "issued": { "date-parts": [[2024, 1, 2]] },
                    "update-to": [{ "type": "retraction", "DOI": "10.1000/retracted-paper" }]
                },
                {
                    "DOI": "10.1000/empty-title",
                    "title": [""],
                    "URL": "https://doi.org/10.1000/empty-title"
                }
            ]
        }
    });
    let mut cfg = Config::default();
    cfg.contact_email = Some("polite@example.org".to_owned());
    let cfg = Arc::new(cfg);
    let net = Arc::new(InspectNet::json(body));
    let src = ManifestSource::new(
        seed_manifest("crossref"),
        SourceDeps::new(net.clone(), Arc::new(NullStore::new()), Arc::clone(&cfg)),
        Arc::new(OAuth2TokenCache::new()),
    );
    let mut scx = SrcCx::from_config(&cfg, CancellationToken::new(), Duration::from_secs(5));
    scx.freshness = Some(fsm_core::Freshness::Month);

    let out = src
        .call("search", json!({ "query": "test", "limit": 10 }), &scx)
        .await
        .unwrap();
    let req = net.last_req().await;
    assert_eq!(query_val(&req, "mailto"), Some("polite@example.org"));
    assert_eq!(query_val(&req, "sort"), Some("published"));
    assert_eq!(query_val(&req, "order"), Some("desc"));

    match out {
        SrcOutput::Works(ws) => {
            // M4: title: [""] must be skipped (5 valid items left).
            assert_eq!(ws.len(), 5);
            // [[null]] -> None
            assert_eq!(ws[0].published_date, None);
            assert_eq!(ws[0].year, None);
            // [[2024, 0, 0]] -> year-only "2024" (month/day rejected)
            assert_eq!(ws[1].published_date.as_deref(), Some("2024"));
            // [[2024, 13, 40]] -> year-only "2024" (month 13 rejected)
            assert_eq!(ws[2].published_date.as_deref(), Some("2024"));
            // B7: updated-by retraction marks the retracted paper as retracted with reason
            assert!(ws[3].retracted);
            assert!(
                ws[3]
                    .retraction_reason
                    .as_deref()
                    .is_some_and(|r| r.contains("10.1000/ret-notice")),
                "got {:?}",
                ws[3].retraction_reason
            );
            // B7: update-to sits on the retraction notice itself -> retracted MUST be false!
            assert!(!ws[4].retracted);
            assert_eq!(ws[4].retraction_reason, None);
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn test_openalex_mailto_doi_fallback_freshness_and_structural_surprises_i9() {
    let payload = json!({
        "results": [
            {
                "display_name": "Empty Landing Page",
                "publication_date": "2020-01-01",
                "doi": "https://doi.org/10.1234/abcd",
                "primary_location": { "landing_page_url": "" }
            },
            {
                "display_name": "Missing Landing Page",
                "publication_date": "2021-01-01",
                "doi": "https://doi.org/10.1234/efgh",
                "primary_location": {}
            },
            {
                "display_name": "",
                "primary_location": { "landing_page_url": "https://skip.example" }
            }
        ]
    });

    // Without contact_email -> no mailto
    let cfg_no_mail = Arc::new(Config::default());
    let net1 = Arc::new(InspectNet::json(payload.clone()));
    let src1 = ManifestSource::new(
        seed_manifest("openalex"),
        SourceDeps::new(
            net1.clone(),
            Arc::new(NullStore::new()),
            Arc::clone(&cfg_no_mail),
        ),
        Arc::new(OAuth2TokenCache::new()),
    );
    let scx1 = SrcCx::from_config(
        &cfg_no_mail,
        CancellationToken::new(),
        Duration::from_secs(5),
    );
    match src1
        .call("search", json!({ "query": "x", "limit": 5 }), &scx1)
        .await
        .unwrap()
    {
        SrcOutput::Works(ws) => {
            assert_eq!(ws.len(), 2);
            assert_eq!(ws[0].url, "https://doi.org/10.1234/abcd");
            assert_eq!(ws[1].url, "https://doi.org/10.1234/efgh");
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(query_val(&net1.last_req().await, "mailto"), None);

    // With contact_email + freshness -> sends mailto and sort=publication_date:desc
    let mut cfg_mail = Config::default();
    cfg_mail.contact_email = Some("a@b.com".to_owned());
    let cfg_mail = Arc::new(cfg_mail);
    let net2 = Arc::new(InspectNet::json(payload));
    let src2 = ManifestSource::new(
        seed_manifest("openalex"),
        SourceDeps::new(
            net2.clone(),
            Arc::new(NullStore::new()),
            Arc::clone(&cfg_mail),
        ),
        Arc::new(OAuth2TokenCache::new()),
    );
    let mut scx2 = SrcCx::from_config(&cfg_mail, CancellationToken::new(), Duration::from_secs(5));
    scx2.freshness = Some(fsm_core::Freshness::Week);
    src2.call("search", json!({ "query": "x", "limit": 5 }), &scx2)
        .await
        .unwrap();
    let r2 = net2.last_req().await;
    assert_eq!(query_val(&r2, "mailto"), Some("a@b.com"));
    assert_eq!(query_val(&r2, "sort"), Some("publication_date:desc"));

    // Structural surprises: null, {}, {"results": "nope"}, [], "garbage" -> empty Works (no panic/error)
    for bad in [
        Value::Null,
        json!({}),
        json!({"results": "nope"}),
        json!([]),
        json!("garbage"),
    ] {
        let net_bad = Arc::new(InspectNet::json(bad.clone()));
        let src_bad = ManifestSource::new(
            seed_manifest("openalex"),
            SourceDeps::new(
                net_bad,
                Arc::new(NullStore::new()),
                Arc::clone(&cfg_no_mail),
            ),
            Arc::new(OAuth2TokenCache::new()),
        );
        match src_bad
            .call("search", json!({ "query": "x", "limit": 5 }), &scx1)
            .await
            .unwrap()
        {
            SrcOutput::Works(ws) => assert!(ws.is_empty(), "payload={bad:?} got {ws:?}"),
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[tokio::test]
async fn test_europepmc_html_vs_pdf_record_fallback_freshness_and_collective_authors_i3_i9() {
    let payload = json!({
        "hitCount": 2,
        "resultList": {
            "result": [
                {
                    "id": "38270601",
                    "source": "MED",
                    "pmid": "38270601",
                    "pmcid": "PMC10871810",
                    "doi": "10.1093/nar/gkae023",
                    "title": "CRISPR-Cas9 screening of proteins.",
                    "authorList": {
                        "author": [
                            { "collectiveName": "WHO Working Group" },
                            { "firstName": "Y", "lastName": "Zhang" }
                        ]
                    },
                    "journalInfo": { "journal": { "title": "Nucleic Acids Res" } },
                    "firstPublicationDate": "2024-01-25",
                    "citedByCount": 42,
                    "isOpenAccess": "Y",
                    "fullTextUrlList": {
                        "fullTextUrl": [
                            { "availabilityCode": "OA", "documentStyle": "pdf", "url": "https://europepmc.org/articles/PMC10871810?pdf=render" },
                            { "availabilityCode": "OA", "documentStyle": "html", "url": "https://europepmc.org/articles/PMC10871810" }
                        ]
                    }
                },
                {
                    "id": "PPR812345",
                    "source": "PPR",
                    "doi": "10.1101/2026.01.01.000000",
                    "title": "A preprint about base editors",
                    "authorString": "Doe J.",
                    "firstPublicationDate": "2026-01-01"
                }
            ]
        }
    });
    let cfg = Arc::new(Config::default());
    let net = Arc::new(InspectNet::json(payload));
    let src = ManifestSource::new(
        seed_manifest("europepmc"),
        SourceDeps::new(net.clone(), Arc::new(NullStore::new()), Arc::clone(&cfg)),
        Arc::new(OAuth2TokenCache::new()),
    );
    let mut scx = SrcCx::from_config(&cfg, CancellationToken::new(), Duration::from_secs(5));
    scx.freshness = Some(fsm_core::Freshness::Week);

    match src
        .call("search", json!({ "query": "crispr", "limit": 10 }), &scx)
        .await
        .unwrap()
    {
        SrcOutput::Works(ws) => {
            assert_eq!(ws.len(), 2);
            // Prefers OA HTML over PDF
            assert_eq!(ws[0].url, "https://europepmc.org/articles/PMC10871810");
            // I3: collectiveName consortium author preserved!
            assert_eq!(ws[0].authors.len(), 2);
            assert_eq!(ws[0].authors[0].name, "WHO Working Group");
            assert_eq!(ws[0].authors[1].name, "Y Zhang");
            // Falls back to Europe PMC record page
            assert_eq!(ws[1].url, "https://europepmc.org/article/PPR/PPR812345");
            assert_eq!(ws[1].work_type, "preprint");
        }
        other => panic!("unexpected {other:?}"),
    }
    // Freshness becomes FIRST_PDATE date range clause
    let q = query_val(&net.last_req().await, "query")
        .unwrap()
        .to_owned();
    assert!(
        q.contains("FIRST_PDATE:["),
        "expected FIRST_PDATE clause in Europe PMC query, got {q:?}"
    );
}

#[tokio::test]
async fn test_semanticscholar_header_key_oa_pdf_empty_url_preprint_doi_and_freshness_i8_i9() {
    let payload = json!({
        "total": 3,
        "data": [
            {
                "paperId": "p1",
                "title": "Attention Is All You Need",
                "year": 2017,
                "publicationDate": "2017-06-12",
                "isOpenAccess": true,
                "openAccessPdf": { "url": "https://arxiv.org/pdf/1706.03762", "status": "GREEN" },
                "externalIds": { "DOI": "10.5555/3295222.3295349" },
                "authors": [{ "name": "Ashish Vaswani" }]
            },
            {
                "paperId": "p2",
                "title": "Empty OA PDF With ArXiv DOI",
                "year": 2020,
                "isOpenAccess": true,
                "openAccessPdf": { "url": "", "status": "GREEN", "disclaimer": "Notice" },
                "externalIds": { "DOI": "10.48550/arXiv.2010.11929", "ArXiv": "2010.11929" },
                "authors": [{ "name": "Alexey Dosovitskiy" }]
            }
        ]
    });
    let mut cfg = Config::default();
    cfg.api_keys
        .insert("SEARCH_MCP_SEMANTICSCHOLAR_API_KEY".into(), "sekrit".into());
    let cfg = Arc::new(cfg);
    let net = Arc::new(InspectNet::json(payload));
    let src = ManifestSource::new(
        seed_manifest("semanticscholar"),
        SourceDeps::new(net.clone(), Arc::new(NullStore::new()), Arc::clone(&cfg)),
        Arc::new(OAuth2TokenCache::new()),
    );
    let mut scx = SrcCx::from_config(&cfg, CancellationToken::new(), Duration::from_secs(5));
    scx.freshness = Some(fsm_core::Freshness::Month);

    match src
        .call("search", json!({ "query": "attention", "limit": 2 }), &scx)
        .await
        .unwrap()
    {
        SrcOutput::Works(ws) => {
            assert_eq!(ws.len(), 2);
            assert_eq!(ws[0].url, "https://arxiv.org/pdf/1706.03762");
            // I8: empty openAccessPdf.url becomes None and url falls back to DOI, workType = Preprint!
            assert_eq!(ws[1].url, "https://doi.org/10.48550/arxiv.2010.11929");
            assert_eq!(ws[1].work_type, "preprint");
            let oa = ws[1].oa.as_ref().unwrap();
            assert_eq!(oa.url, None);
            assert_eq!(oa.pdf_url, None);
        }
        other => panic!("unexpected {other:?}"),
    }
    let req = net.last_req().await;
    assert!(
        req.headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("x-api-key") && v == "sekrit"),
        "expected x-api-key header, got {:?}",
        req.headers
    );
    assert_eq!(query_val(&req, "sort"), Some("publicationDate:desc"));
}

#[tokio::test]
async fn test_doaj_path_encoding_links_year_month_license_and_freshness_i9() {
    let payload = json!({
        "total": 2,
        "results": [
            {
                "id": "0a1b2c3d",
                "bibjson": {
                    "title": "Machine learning for\n  soil mapping",
                    "year": "2023",
                    "month": "4",
                    "license": [{ "type": "CC BY" }],
                    "journal": { "title": "Open Soil Science" },
                    "identifier": [{ "type": "doi", "id": "10.1234/oss.2023.1" }],
                    "link": [{ "type": "fulltext", "url": "https://journal.example/article/1" }]
                }
            },
            {
                "id": "deadbeef",
                "bibjson": {
                    "title": "No full text link",
                    "year": "2020",
                    "identifier": [{ "type": "doi", "id": "10.1234/x" }]
                }
            }
        ]
    });
    let cfg = Arc::new(Config::default());
    let net = Arc::new(InspectNet::json(payload));
    let src = ManifestSource::new(
        seed_manifest("doaj"),
        SourceDeps::new(net.clone(), Arc::new(NullStore::new()), Arc::clone(&cfg)),
        Arc::new(OAuth2TokenCache::new()),
    );
    let mut scx = SrcCx::from_config(&cfg, CancellationToken::new(), Duration::from_secs(5));
    scx.freshness = Some(fsm_core::Freshness::Week);

    match src
        .call("search", json!({ "query": "a/b c", "limit": 5 }), &scx)
        .await
        .unwrap()
    {
        SrcOutput::Works(ws) => {
            assert_eq!(ws.len(), 2);
            assert_eq!(ws[0].title, "Machine learning for soil mapping");
            assert_eq!(ws[0].url, "https://journal.example/article/1");
            assert_eq!(ws[0].published_date.as_deref(), Some("2023-04"));
            assert_eq!(
                ws[0].oa.as_ref().and_then(|o| o.license.as_deref()),
                Some("CC BY")
            );
            assert_eq!(ws[1].url, "https://doi.org/10.1234/x");
            assert_eq!(ws[1].published_date.as_deref(), Some("2020"));
        }
        other => panic!("unexpected {other:?}"),
    }
    let req = net.last_req().await;
    assert_eq!(req.url, "https://doaj.org/api/search/articles/a%2Fb%20c");
    assert_eq!(query_val(&req, "sort"), Some("created_date:desc"));
}

#[tokio::test]
async fn test_mwmbl_unescapes_html_entities_m3() {
    let fx_path = manifests_root().join("fixtures/seed/mwmbl/search/default.json");
    let net = Arc::new(ReplayNet::from_file(&fx_path).expect("load mwmbl fixture"));
    let cfg = Arc::new(Config::default());
    let src = ManifestSource::new(
        seed_manifest("mwmbl"),
        SourceDeps::new(net, Arc::new(NullStore::new()), Arc::clone(&cfg)),
        Arc::new(OAuth2TokenCache::new()),
    );
    let mut scx = SrcCx::from_config(&cfg, CancellationToken::new(), Duration::from_secs(5));
    scx.replay_mode = true;
    match src
        .call("search", json!({ "query": "python", "limit": 50 }), &scx)
        .await
        .unwrap()
    {
        SrcOutput::Hits(hits) => {
            assert!(
                hits.iter().all(|h| !h.title.contains("&#62;")),
                "raw HTML entity &#62; leaked in mwmbl titles"
            );
            assert!(
                hits.iter()
                    .any(|h| h.title.starts_with("> As a Pythonista")),
                "expected decoded '> As a Pythonista' hit in top 50 mwmbl results"
            );
        }
        other => panic!("unexpected {other:?}"),
    }
}

fn manifest_by_id(id: &str) -> ManifestDecl {
    fsm_manifests::embedded_bundle()
        .manifests
        .iter()
        .find(|m| m.manifest.id == id)
        .map(|m| m.manifest.clone())
        .unwrap_or_else(|| panic!("missing embedded manifest {id}"))
}

#[tokio::test]
async fn test_cordis_new_and_legacy_envelope_regression() {
    let decl = manifest_by_id("cordis");
    let cfg = Arc::new(Config::default());
    let scx = SrcCx::from_config(&cfg, CancellationToken::new(), Duration::from_secs(5));

    // New live CORDIS envelope: .content.hits[].source + array relations.associations.
    let new_body = json!({
        "encoding": "UTF-8",
        "content": {
            "header": { "total": 42 },
            "hits": [{
                "score": 6.1,
                "source": {
                    "id": "101201649",
                    "acronym": "CCnoisyQC",
                    "title": "Computational Complexity of Quantum Systems",
                    "ecMaxContribution": "2498125",
                    "startDate": "2025-08-01",
                    "endDate": "2030-07-31",
                    "status": "SIGNED",
                    "teaser": "Quantum teaser.",
                    "relations": {
                        "associations": [{
                            "contenttype": "organization",
                            "attributes": { "type": "coordinator" },
                            "legalName": "THE HEBREW UNIVERSITY OF JERUSALEM"
                        }]
                    }
                }
            }]
        }
    });
    let net_new = Arc::new(InspectNet::json(new_body));
    let src_new = ManifestSource::new(
        decl.clone(),
        SourceDeps::new(net_new, Arc::new(NullStore::new()), Arc::clone(&cfg)),
        Arc::new(OAuth2TokenCache::new()),
    );
    match src_new
        .call("search", json!({ "query": "quantum", "limit": 1 }), &scx)
        .await
        .expect("cordis new schema")
    {
        SrcOutput::Records { items, .. } => {
            assert_eq!(items.len(), 1);
            assert_eq!(items[0]["id"], "101201649");
            assert_eq!(items[0]["org"], "THE HEBREW UNIVERSITY OF JERUSALEM");
            assert_eq!(items[0]["amount"], 2498125);
            assert_eq!(items[0]["startDate"], "2025-08-01");
        }
        other => panic!("unexpected {other:?}"),
    }

    // Legacy CORDIS envelope: .hits.hit[].project + object relations.associations.organization.
    let legacy_body = json!({
        "result": { "header": { "totalHits": "1" } },
        "hits": {
            "hit": [{
                "project": {
                    "id": "999001",
                    "title": "Legacy CORDIS Project",
                    "ecMaxContribution": "100000",
                    "startDate": "2024-01-01",
                    "relations": {
                        "associations": {
                            "organization": {
                                "@attributes": { "type": "coordinator" },
                                "legalName": "LEGACY UNIV"
                            }
                        }
                    }
                }
            }]
        }
    });
    let net_old = Arc::new(InspectNet::json(legacy_body));
    let src_old = ManifestSource::new(
        decl,
        SourceDeps::new(net_old, Arc::new(NullStore::new()), Arc::clone(&cfg)),
        Arc::new(OAuth2TokenCache::new()),
    );
    match src_old
        .call("search", json!({ "query": "legacy", "limit": 1 }), &scx)
        .await
        .expect("cordis legacy schema")
    {
        SrcOutput::Records { items, .. } => {
            assert_eq!(items.len(), 1);
            assert_eq!(items[0]["org"], "LEGACY UNIV");
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn test_episciences_array_program_and_year_only_date_regression() {
    let decl = manifest_by_id("episciences_oa");
    let cfg = Arc::new(Config::default());
    let scx = SrcCx::from_config(&cfg, CancellationToken::new(), Duration::from_secs(5));

    let body = json!({
        "paperid": 16852,
        "document": {
            "journal": {
                "journal_metadata": { "full_title": "DMTCS" },
                "journal_issue": { "publication_date": { "year": "2026" } },
                "journal_article": {
                    "titles": { "title": "Array Program Regression" },
                    "doi_data": { "doi": "10.46298/dmtcs.16852", "resource": "https://dmtcs.episciences.org/16852" },
                    "program": [
                        { "@name": "AccessIndicators", "free_to_read": { "value": "" } },
                        {
                            "related_item": [
                                { "intra_work_relation": { "value": "arXiv:2511.00650", "@identifier-type": "arxiv", "@relationship-type": "isSameAs" } },
                                { "intra_work_relation": { "value": "https://arxiv.org/abs/2511.00650v2", "@identifier-type": "uri", "@relationship-type": "hasPreprint" } }
                            ]
                        }
                    ]
                }
            }
        }
    });
    let net = Arc::new(InspectNet::json(body));
    let src = ManifestSource::new(
        decl,
        SourceDeps::new(net, Arc::new(NullStore::new()), Arc::clone(&cfg)),
        Arc::new(OAuth2TokenCache::new()),
    );
    match src
        .call("paper", json!({ "docid": "20214" }), &scx)
        .await
        .expect("episciences array program")
    {
        SrcOutput::Works(ws) => {
            assert_eq!(ws.len(), 1);
            assert_eq!(ws[0].ids.arxiv.as_deref(), Some("2511.00650"));
            assert_eq!(ws[0].published_date.as_deref(), Some("2026"));
            assert_eq!(ws[0].versions.len(), 1);
            assert_eq!(ws[0].versions[0].url, "https://arxiv.org/abs/2511.00650v2");
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn test_quality_fixes_entities_ws_dates_and_hit_hygiene() {
    let cfg = Arc::new(Config::default());
    let mut scx = SrcCx::from_config(&cfg, CancellationToken::new(), Duration::from_secs(5));
    scx.replay_mode = true;

    // 1. Crossref double-encoded `&amp;nbsp;` cleaned in abstractText.
    let cr_fx = manifests_root().join("fixtures/seed/crossref/search/default.json");
    let cr_src = ManifestSource::new(
        seed_manifest("crossref"),
        SourceDeps::new(
            Arc::new(ReplayNet::from_file(&cr_fx).unwrap()),
            Arc::new(NullStore::new()),
            Arc::clone(&cfg),
        ),
        Arc::new(OAuth2TokenCache::new()),
    );
    if let SrcOutput::Works(ws) = cr_src
        .call(
            "search",
            json!({ "query": "attention is all you need", "limit": 3 }),
            &scx,
        )
        .await
        .unwrap()
    {
        for w in &ws {
            if let Some(ab) = &w.abstract_text {
                assert!(
                    !ab.contains("&nbsp;"),
                    "raw &nbsp; leaked in crossref: {ab}"
                );
            }
        }
    }

    // 2. DOAJ author name & affiliation collapse whitespace (no `\t` or double spaces).
    let doaj_fx = manifests_root().join("fixtures/seed/doaj/search/default.json");
    let doaj_src = ManifestSource::new(
        seed_manifest("doaj"),
        SourceDeps::new(
            Arc::new(ReplayNet::from_file(&doaj_fx).unwrap()),
            Arc::new(NullStore::new()),
            Arc::clone(&cfg),
        ),
        Arc::new(OAuth2TokenCache::new()),
    );
    if let SrcOutput::Works(ws) = doaj_src
        .call(
            "search",
            json!({ "query": "machine learning", "limit": 3 }),
            &scx,
        )
        .await
        .unwrap()
    {
        for w in &ws {
            for a in &w.authors {
                assert!(
                    !a.name.contains("  "),
                    "uncollapsed author name: {:?}",
                    a.name
                );
                if let Some(aff) = &a.affiliation {
                    assert!(!aff.contains('\t'), "raw tab in affiliation: {aff:?}");
                }
            }
        }
    }

    // 3. eprint_iacr.search extracts ISO publishedDate from HTML updated field.
    let ep_fx = manifests_root().join("fixtures/w5a/eprint_iacr/search/default.json");
    let ep_src = ManifestSource::new(
        manifest_by_id("eprint_iacr"),
        SourceDeps::new(
            Arc::new(ReplayNet::from_file(&ep_fx).unwrap()),
            Arc::new(NullStore::new()),
            Arc::clone(&cfg),
        ),
        Arc::new(OAuth2TokenCache::new()),
    );
    if let SrcOutput::Works(ws) = ep_src
        .call("search", json!({ "query": "ML-KEM Kyber lattice" }), &scx)
        .await
        .unwrap()
    {
        assert_eq!(ws[0].published_date.as_deref(), Some("2026-11-05"));
    }

    // 4. jalc_doi.doi preserves year-only publishedDate ("2008").
    let jalc_fx = manifests_root().join("fixtures/w5h/jalc_doi/doi/default.json");
    let jalc_src = ManifestSource::new(
        manifest_by_id("jalc_doi"),
        SourceDeps::new(
            Arc::new(ReplayNet::from_file(&jalc_fx).unwrap()),
            Arc::new(NullStore::new()),
            Arc::clone(&cfg),
        ),
        Arc::new(OAuth2TokenCache::new()),
    );
    if let SrcOutput::Works(ws) = jalc_src
        .call(
            "doi",
            json!({ "doi": "10.11514/infopro.2008.0.138.0" }),
            &scx,
        )
        .await
        .unwrap()
    {
        assert_eq!(ws[0].published_date.as_deref(), Some("2008"));
    }

    // 5. artic_museum.search normalizes date to ISO and preserves dateDisplay.
    let artic_fx = manifests_root().join("fixtures/w5n/artic_museum/search/default.json");
    let artic_src = ManifestSource::new(
        manifest_by_id("artic_museum"),
        SourceDeps::new(
            Arc::new(ReplayNet::from_file(&artic_fx).unwrap()),
            Arc::new(NullStore::new()),
            Arc::clone(&cfg),
        ),
        Arc::new(OAuth2TokenCache::new()),
    );
    if let SrcOutput::Records { items, .. } = artic_src
        .call("search", json!({ "query": "monet", "limit": 3 }), &scx)
        .await
        .unwrap()
    {
        assert_eq!(items[0]["date"], "1906");
        assert!(
            items[2]["date"].is_null(),
            "non-ISO span 1890–91 must normalize to null"
        );
        assert_eq!(items[2]["dateDisplay"], "1890–91");
    }

    // 6. abusech_urlhaus normalizes space+UTC timestamps to ISO-8601 Z.
    let uh_fx = manifests_root().join("fixtures/w5l/abusech_urlhaus/host/default.json");
    let uh_src = ManifestSource::new(
        manifest_by_id("abusech_urlhaus"),
        SourceDeps::new(
            Arc::new(ReplayNet::from_file(&uh_fx).unwrap()),
            Arc::new(NullStore::new()),
            Arc::clone(&cfg),
        ),
        Arc::new(OAuth2TokenCache::new()),
    );
    if let SrcOutput::Records { items, .. } = uh_src
        .call("host", json!({ "host": "vektorex.com" }), &scx)
        .await
        .unwrap()
    {
        assert_eq!(items[0]["firstSeen"], "2023-05-12T14:22:10Z");
        assert_eq!(items[0]["urls"][0]["dateAdded"], "2023-05-12T14:22:10Z");
    }

    // 7. remotive_jobs.search respects limit parameter.
    let rem_fx = manifests_root().join("fixtures/w5n/remotive_jobs/search/default.json");
    let rem_src = ManifestSource::new(
        manifest_by_id("remotive_jobs"),
        SourceDeps::new(
            Arc::new(ReplayNet::from_file(&rem_fx).unwrap()),
            Arc::new(NullStore::new()),
            Arc::clone(&cfg),
        ),
        Arc::new(OAuth2TokenCache::new()),
    );
    if let SrcOutput::Records { items, .. } = rem_src
        .call("search", json!({ "query": "golang", "limit": 3 }), &scx)
        .await
        .unwrap()
    {
        assert_eq!(items.len(), 3, "remotive_jobs must slice to limit=3");
    }

    // 8. value_to_hit clears date_source when date is None & trims snippet trailing spaces.
    let lmfdb_fx = manifests_root().join("fixtures/w5j/lmfdb_math/search/default.json");
    let lmfdb_src = ManifestSource::new(
        manifest_by_id("lmfdb_math"),
        SourceDeps::new(
            Arc::new(ReplayNet::from_file(&lmfdb_fx).unwrap()),
            Arc::new(NullStore::new()),
            Arc::clone(&cfg),
        ),
        Arc::new(OAuth2TokenCache::new()),
    );
    if let SrcOutput::Records { hits: Some(hs), .. } = lmfdb_src
        .call("search", json!({ "query": "11", "limit": 3 }), &scx)
        .await
        .unwrap()
    {
        assert!(
            hs.iter()
                .all(|h| h.date.is_none() && h.date_source.is_none())
        );
    }

    let ct_fx = manifests_root().join("fixtures/w5b/clinicaltrials/search/default.json");
    let ct_src = ManifestSource::new(
        manifest_by_id("clinicaltrials"),
        SourceDeps::new(
            Arc::new(ReplayNet::from_file(&ct_fx).unwrap()),
            Arc::new(NullStore::new()),
            Arc::clone(&cfg),
        ),
        Arc::new(OAuth2TokenCache::new()),
    );
    if let SrcOutput::Records { hits: Some(hs), .. } = ct_src
        .call("search", json!({ "query": "crispr", "limit": 3 }), &scx)
        .await
        .unwrap()
    {
        for h in &hs {
            assert_eq!(
                h.snippet,
                h.snippet.trim(),
                "trailing whitespace in hit snippet"
            );
        }
    }
}

#[test]
fn test_claims_word_boundaries_and_routing_host_bucket_hygiene() {
    let cfg = Arc::new(Config::default());
    let net = Arc::new(InspectNet::json(json!({})));
    let deps = SourceDeps::new(net, Arc::new(NullStore::new()), cfg);
    let mdn = ManifestSource::new(
        manifest_by_id("mdn"),
        deps,
        Arc::new(OAuth2TokenCache::new()),
    );

    // Must match real web/DOM/CJK queries.
    assert_eq!(mdn.claims("Array.prototype.map", None), 1.0);
    assert_eq!(mdn.claims("DOM manipulation", None), 0.5);
    assert_eq!(mdn.claims("前端 性能优化", None), 0.5);
    // Must NOT substring-claim unrelated English words containing "dom".
    for benign in [
        "freedom of speech",
        "random number generator",
        "domain name system",
        "domestic economic policy",
        "united kingdom parliament",
    ] {
        assert_eq!(
            mdn.claims(benign, None),
            0.0,
            "mdn over-claimed benign query {benign:?}"
        );
    }

    // Routing safety: zero manifests or instances in PoolRole::Default for "web" scope.
    let bundle = fsm_manifests::embedded_bundle();
    for m in &bundle.manifests {
        if m.manifest.scopes.iter().any(|s| s == "web") {
            assert_ne!(
                m.manifest.pool_role,
                PoolRole::Default,
                "niche manifest {} must not be in default web pool",
                m.manifest.id
            );
        }
    }
    for f in &bundle.instances {
        for inst in &f.file.instance {
            if inst.scopes.iter().any(|s| s == "web") {
                assert_ne!(
                    inst.pool_role,
                    Some(PoolRole::Default),
                    "instance {} must not be in default web pool",
                    inst.id
                );
            }
        }
    }

    // Shared host buckets verified.
    assert_eq!(
        manifest_by_id("wayback_cdx").limits.host_bucket.as_deref(),
        Some("archive.org")
    );
    assert_eq!(
        manifest_by_id("archive_org").limits.host_bucket.as_deref(),
        Some("archive.org")
    );
    for (id, want_bucket) in [
        ("cinii_research", "cir.nii.ac.jp"),
        ("cinii_kaken", "cir.nii.ac.jp"),
        ("wiktionary", "wikimedia.org"),
        ("wikispecies", "wikimedia.org"),
        ("hal_oai", "api.archives-ouvertes.fr"),
    ] {
        let inst = bundle
            .instances
            .iter()
            .flat_map(|f| f.file.instance.iter())
            .find(|i| i.id == id)
            .unwrap_or_else(|| panic!("missing instance {id}"));
        assert_eq!(
            inst.limits.as_ref().and_then(|l| l.host_bucket.as_deref()),
            Some(want_bucket),
            "instance {id} missing shared host_bucket {want_bucket}"
        );
    }
}
