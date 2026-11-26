//! Seed instance validation, secondary ops, dialect handling, error paths,
//! and Python oracle parity tests (`wikipedia.py`, `wikimedia.py`, `dataverse.py`,
//! `test_media_engines.py`) for all 9 `fsm-protocols-a` adapters.

use fsm_core::fixture::{
    Exchange, Fixture, FixtureRequest, FixtureResponse, ReplayNet, sha256_hex,
};
use fsm_core::manifest::InstanceFileDecl;
use fsm_core::{
    Config, Freshness, FsmError, Hit, HitKind, HttpBytes, HttpRequest, NetClient, NetCx, NullStore,
    ProtocolAdapter, Registry, ScholarlyWork, SourceDeps, SrcCx, TransportProfile,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

type ResponderFn = Box<dyn Fn(&HttpRequest, usize) -> Result<HttpBytes, FsmError> + Send + Sync>;

/// In-test request-capturing `NetClient` for asserting exact outgoing URLs,
/// query parameters, headers, and POST bodies without hitting the network.
struct CapturingNet {
    requests: Mutex<Vec<HttpRequest>>,
    responder: ResponderFn,
}

impl CapturingNet {
    fn fixed(content_type: &'static str, body: impl Into<String>) -> Arc<Self> {
        let bytes: Vec<u8> = body.into().into_bytes();
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
            responder: Box::new(move |req, _| {
                Ok(HttpBytes::new(
                    req.url.clone(),
                    200,
                    vec![("content-type".into(), content_type.into())],
                    bytes.clone(),
                ))
            }),
        })
    }

    fn with_fn(
        f: impl Fn(&HttpRequest, usize) -> Result<HttpBytes, FsmError> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
            responder: Box::new(f),
        })
    }

    fn captured(&self) -> Vec<HttpRequest> {
        self.requests.lock().expect("lock").clone()
    }

    fn full_url(req: &HttpRequest) -> url::Url {
        let mut u = url::Url::parse(&req.url).expect("valid request url");
        if !req.query.is_empty() {
            let mut qp = u.query_pairs_mut();
            for (k, v) in &req.query {
                qp.append_pair(k, v);
            }
        }
        u
    }
}

#[async_trait::async_trait]
impl NetClient for CapturingNet {
    async fn send(
        &self,
        req: HttpRequest,
        _profile: TransportProfile,
        _cx: &NetCx,
    ) -> Result<HttpBytes, FsmError> {
        let idx = {
            let mut guard = self.requests.lock().expect("lock");
            let i = guard.len();
            guard.push(req.clone());
            i
        };
        (self.responder)(&req, idx)
    }

    fn check_url_offline(&self, _url: &str) -> Result<(), FsmError> {
        Ok(())
    }
}

fn build_reg_with_net(net: Arc<dyn NetClient>) -> (Registry, SourceDeps) {
    let deps = SourceDeps::new(net, Arc::new(NullStore::new()), Arc::new(Config::default()));
    let mut reg = Registry::new();
    fsm_protocols_a::register(&mut reg, &deps).expect("register");
    (reg, deps)
}

fn seed_instances_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates dir")
        .join("manifests/instances/seed")
}

fn crate_fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn build_reg(net: Arc<ReplayNet>) -> (Registry, SourceDeps) {
    let deps = SourceDeps::new(net, Arc::new(NullStore::new()), Arc::new(Config::default()));
    let mut reg = Registry::new();
    fsm_protocols_a::register(&mut reg, &deps).expect("register");
    (reg, deps)
}

fn default_cx(deps: &SourceDeps) -> SrcCx {
    SrcCx::from_config(&deps.cfg, CancellationToken::new(), Duration::from_secs(10))
}

fn single_exchange(
    source: &str,
    op: &str,
    req: FixtureRequest,
    status: u16,
    content_type: &str,
    body: &str,
) -> Arc<ReplayNet> {
    let mut f = Fixture::new(source, op, "2026-11-25T12:00:00Z");
    let resp = FixtureResponse::from_bytes(
        status,
        vec![("content-type".into(), content_type.into())],
        body.as_bytes(),
    );
    f.exchanges.push(Exchange::new(req, resp));
    Arc::new(ReplayNet::from_fixtures(vec![f]).expect("replay net"))
}

// ---------------------------------------------------------------------------
// 1. Registration & seed instance validation
// ---------------------------------------------------------------------------

#[test]
fn register_exposes_all_nine_protocol_adapters_and_expected_ops() {
    let net = Arc::new(ReplayNet::from_fixtures(vec![]).unwrap());
    let (reg, _) = build_reg(net);

    let expected: &[(&str, &[&str])] = &[
        (
            "oai_pmh",
            &["identify", "list_sets", "list_records", "get_record"],
        ),
        ("sru", &["search", "explain"]),
        ("opensearch", &["search", "describe"]),
        ("sparql", &["query"]),
        ("dspace7", &["search", "item"]),
        ("inveniordm", &["search", "record"]),
        ("dataverse", &["search", "dataset"]),
        ("eprints", &["search", "record"]),
        ("mediawiki", &["search", "page"]),
    ];

    for &(kind, ops) in expected {
        let adapter = reg
            .protocol(kind)
            .unwrap_or_else(|| panic!("adapter {kind} not registered"));
        assert_eq!(adapter.kind(), kind);
        let got_ops: Vec<&str> = adapter.ops().iter().map(|o| o.name.as_str()).collect();
        assert_eq!(got_ops, ops, "ops mismatch for {kind}");
    }
}

#[test]
fn academic_seed_instance_tables_parse_and_validate() {
    let net = Arc::new(ReplayNet::from_fixtures(vec![]).unwrap());
    let (reg, _) = build_reg(net);

    let seed_files = [
        ("academic_mediawiki.toml", &["wikipedia", "wikimedia"][..]),
        ("academic_dataverse.toml", &["dataverse"][..]),
        ("academic_oai_pmh.toml", &["hal_oai"][..]),
    ];

    for (filename, expected_ids) in seed_files {
        let path = seed_instances_dir().join(filename);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let file = InstanceFileDecl::from_toml(&text)
            .unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
        file.validate()
            .unwrap_or_else(|e| panic!("validate {}: {e}", path.display()));

        let adapter = reg
            .protocol(&file.protocol)
            .unwrap_or_else(|| panic!("unknown protocol {}", file.protocol));

        let ids: Vec<&str> = file.instance.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, expected_ids);

        for inst in &file.instance {
            adapter
                .validate_dialect(&inst.dialect)
                .unwrap_or_else(|e| panic!("dialect for {}: {e}", inst.id));

            // A1 §D.2 #1: every default_params key must exist on at least one adapter op.
            for k in inst.default_params.keys() {
                assert!(
                    adapter.ops().iter().any(|o| o.param(k).is_some()),
                    "{}: default_params key {k:?} not declared on {}",
                    inst.id,
                    file.protocol
                );
            }

            let decl = inst.to_source_decl(adapter.ops());
            assert_eq!(decl.id, inst.id);
            assert!(
                decl.op(inst.probe.op.as_deref().unwrap())
                    .and_then(|o| o.probe.as_ref())
                    .is_some()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 2. MediaWiki: Wikipedia dynamic lang, Wikimedia Commons imageinfo, page op
// ---------------------------------------------------------------------------

#[tokio::test]
async fn wikipedia_dynamic_lang_subdomain_and_searchmatch_stripping() {
    let net = Arc::new(
        ReplayNet::from_file(crate_fixtures_dir().join("seed_wikipedia_search.json")).unwrap(),
    );
    let (reg, deps) = build_reg(net.clone());
    let adapter = reg.protocol("mediawiki").unwrap();

    // 1. Default region "us-en" -> en.wikipedia.org (matches seed probe limit = 3)
    let mut cx = default_cx(&deps);
    cx.dialect.insert("dynamic_lang_host".into(), json!(true));
    let out = adapter
        .call(
            "https://{lang_sub}.wikipedia.org/w/api.php",
            "search",
            json!({"query": "Alan Turing", "limit": 3}),
            &cx,
        )
        .await
        .unwrap();
    assert_eq!(out.items.len(), 3);
    assert!(out.items[0].get("kind").is_none());
    let h0: Hit = serde_json::from_value(out.items[0].clone()).unwrap();
    assert_eq!(h0.title, "Alan Turing");
    assert_eq!(h0.url, "https://en.wikipedia.org/wiki/Alan_Turing");
    assert!(!h0.snippet.contains("<span"));
    assert!(h0.snippet.contains("[Turing] was treated\""));
    assert_eq!(h0.date, None);
    assert_eq!(
        h0.extra
            .as_ref()
            .and_then(|e| e.get("lastEdited"))
            .and_then(Value::as_str),
        Some("2026-11-21")
    );
    assert_eq!(out.next_cursor.as_deref(), Some("3"));

    // 2. cx.region = "cn-zh" -> zh.wikipedia.org (Python wikipedia.py parity)
    let mut cx_zh = default_cx(&deps);
    cx_zh.region = "cn-zh".into();
    cx_zh
        .dialect
        .insert("dynamic_lang_host".into(), json!(true));
    let out_zh = adapter
        .call(
            "https://{lang_sub}.wikipedia.org",
            "search",
            json!({"query": "图灵", "limit": 1}),
            &cx_zh,
        )
        .await
        .unwrap();
    let hz: Hit = serde_json::from_value(out_zh.items[0].clone()).unwrap();
    assert!(hz.url.starts_with("https://zh.wikipedia.org/wiki/"));

    // 3. params.lang overrides cx.lang & cx.region; hostile lang falls back to "en"
    let mut cx_fr = default_cx(&deps);
    cx_fr.lang = Some("de-DE".into());
    cx_fr
        .dialect
        .insert("dynamic_lang_host".into(), json!(true));
    let out_fr = adapter
        .call(
            "https://{lang_sub}.wikipedia.org/w/api.php",
            "search",
            json!({"query": "Alan Turing", "limit": 1, "lang": "fr"}),
            &cx_fr,
        )
        .await
        .unwrap();
    let hf: Hit = serde_json::from_value(out_fr.items[0].clone()).unwrap();
    assert_eq!(hf.url, "https://fr.wikipedia.org/wiki/Alan_Turing");

    // Hostname-injection guard: invalid lang falls back to "en" (replays first en exchange).
    let out_safe = adapter
        .call(
            "https://{lang_sub}.wikipedia.org/w/api.php",
            "search",
            json!({"query": "Alan Turing", "limit": 3, "lang": "en.evil.com"}),
            &cx,
        )
        .await
        .unwrap();
    let hs: Hit = serde_json::from_value(out_safe.items[0].clone()).unwrap();
    assert!(hs.url.starts_with("https://en.wikipedia.org/wiki/"));
    assert!(net.unmatched().is_empty() && net.unused().is_empty());
}

#[tokio::test]
async fn wikimedia_commons_imageinfo_namespace_6_and_attribution_snippet() {
    let net = Arc::new(
        ReplayNet::from_file(crate_fixtures_dir().join("seed_wikimedia_search.json")).unwrap(),
    );
    let (reg, deps) = build_reg(net.clone());
    let adapter = reg.protocol("mediawiki").unwrap();
    let cx = default_cx(&deps);

    let out = adapter
        .call(
            "https://commons.wikimedia.org/w/api.php",
            "search",
            json!({"query": "lighthouse", "limit": 3, "namespace": 6, "prop": "imageinfo"}),
            &cx,
        )
        .await
        .unwrap();
    assert_eq!(out.items.len(), 3);
    assert!(out.items[0].get("kind").is_none());
    let h0: Hit = serde_json::from_value(out.items[0].clone()).unwrap();
    assert_eq!(h0.title, "Leuchtturm in Westerheversand crop.jpg");
    assert!(!h0.title.starts_with("File:"));
    assert!(
        h0.url.starts_with(
            "https://upload.wikimedia.org/wikipedia/commons/0/02/Leuchtturm_in_Westerheversand_crop.jpg"
        ),
        "got {}",
        h0.url
    );
    assert!(
        h0.snippet.starts_with(
            "CC BY-SA 4.0 \u{00B7} by Marco Leiter \u{00B7} 3606\u{00D7}2894px \u{2014}"
        ),
        "got {:?}",
        h0.snippet
    );
    assert!(h0.snippet.contains("Westerheversand Lighthouse"));
    assert!(!h0.snippet.contains("<a"));
    assert_eq!(h0.date.as_deref(), Some("2016-02-05"));

    let h1: Hit = serde_json::from_value(out.items[1].clone()).unwrap();
    assert!(h1.snippet.contains("6000\u{00D7}3257px"));
    assert_eq!(h1.date.as_deref(), Some("2020-09-02"));
    assert!(net.unmatched().is_empty() && net.unused().is_empty());
}

#[tokio::test]
async fn mediawiki_page_extract_skips_missing() {
    let body = json!({
        "query": {
            "pages": {
                "10": {
                    "pageid": 10,
                    "title": "Rust (programming language)",
                    "fullurl": "https://en.wikipedia.org/wiki/Rust_(programming_language)",
                    "extract": "Rust is a general-purpose programming language.",
                    "touched": "2026-11-20T09:00:00Z",
                    "length": 42000
                },
                "-1": {
                    "title": "Missing",
                    "missing": ""
                }
            }
        }
    })
    .to_string();
    let mut req = FixtureRequest::new(
        "GET",
        "https://en.wikipedia.org/w/api.php?action=query&prop=extracts%7Cinfo&inprop=url&explaintext=1&titles=Rust+%28programming+language%29&format=json",
    );
    req.headers_subset
        .insert("accept".into(), "application/json".into());
    let net = single_exchange("wikipedia", "page", req, 200, "application/json", &body);
    let (reg, deps) = build_reg(net.clone());
    let adapter = reg.protocol("mediawiki").unwrap();
    let mut cx = default_cx(&deps);
    cx.dialect.insert("dynamic_lang_host".into(), json!(true));

    let out = adapter
        .call(
            "https://{lang_sub}.wikipedia.org/w/api.php",
            "page",
            json!({"title": "Rust (programming language)"}),
            &cx,
        )
        .await
        .unwrap();
    assert_eq!(out.items.len(), 1);
    assert_eq!(out.items[0]["title"], "Rust (programming language)");
    assert_eq!(out.items[0]["lastEdited"], "2026-11-20");
    assert!(out.items[0].get("date").is_none());
    assert!(
        out.items[0]["extract"]
            .as_str()
            .unwrap()
            .contains("general-purpose")
    );
    assert!(net.unmatched().is_empty());
}

// ---------------------------------------------------------------------------
// 3. Dataverse: Harvard seed search, dataset lookup, freshness filter
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dataverse_seed_search_and_dataset_lookup() {
    let net =
        Arc::new(ReplayNet::from_file(crate_fixtures_dir().join("seed_dataverse.json")).unwrap());
    let (reg, deps) = build_reg(net.clone());
    let adapter = reg.protocol("dataverse").unwrap();
    let cx = default_cx(&deps);

    let out = adapter
        .call(
            "https://dataverse.harvard.edu",
            "search",
            json!({"query": "climate", "limit": 3}),
            &cx,
        )
        .await
        .unwrap();
    assert_eq!(out.items.len(), 3);
    assert_eq!(
        out.items[0]["title"],
        "Climatic characterization of the sites in Argentina (C\u{00F3}rdoba y Santiago del Estero)"
    );
    assert_eq!(out.items[0]["globalId"], "doi:10.7910/DVN/DUWBBU");
    assert_eq!(out.items[0]["date"], "2025-06-16");
    assert!(
        out.items[0]["authors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == "Leandro D\u{00ED}az")
    );
    assert_eq!(out.next_cursor.as_deref(), Some("3"));

    let ds = adapter
        .call(
            "https://dataverse.harvard.edu",
            "dataset",
            json!({"persistent_id": "doi:10.7910/DVN/DUWBBU"}),
            &cx,
        )
        .await
        .unwrap();
    assert_eq!(ds.items.len(), 1);
    assert_eq!(
        ds.items[0]["title"],
        "Climatic characterization of the sites in Argentina (C\u{00F3}rdoba y Santiago del Estero)"
    );
    assert_eq!(
        ds.items[0]["files"][0]["filename"],
        "IAI_Informe Clima_2021_FINAL.pdf"
    );
    assert!(net.unmatched().is_empty() && net.unused().is_empty());
}

#[tokio::test]
async fn dataverse_freshness_pushes_publication_date_fq() {
    let since = jiff::Timestamp::now()
        .to_zoned_datetime(jiff::tz::TimeZone::UTC)
        .date()
        .checked_sub(jiff::Span::new().days(7))
        .unwrap();
    let expected_fq = format!("publicationDate:[{since} TO *]");
    let url = format!(
        "https://dataverse.harvard.edu/api/search?q=ocean&type=dataset&per_page=5&fq={}&sort=date&order=desc",
        url::form_urlencoded::byte_serialize(expected_fq.as_bytes()).collect::<String>()
    );
    let body = json!({
        "status": "OK",
        "data": { "start": 0, "total_count": 1, "items": [
            { "name": "Recent Ocean Data", "type": "dataset", "url": "https://doi.org/10.7910/DVN/NEW1", "published_at": "2026-11-24T00:00:00Z" }
        ]}
    })
    .to_string();
    let net = single_exchange(
        "dataverse",
        "search",
        FixtureRequest::new("GET", url),
        200,
        "application/json",
        &body,
    );
    let (reg, deps) = build_reg(net.clone());
    let adapter = reg.protocol("dataverse").unwrap();
    let mut cx = default_cx(&deps);
    cx.freshness = Some(Freshness::Week);

    let out = adapter
        .call(
            "https://dataverse.harvard.edu",
            "search",
            json!({"query": "ocean", "limit": 5}),
            &cx,
        )
        .await
        .unwrap();
    assert_eq!(out.items.len(), 1);
    assert!(net.unmatched().is_empty());
}

// ---------------------------------------------------------------------------
// 4. OAI-PMH: HAL seed identify, list_sets, list_records, get_record, errors
// ---------------------------------------------------------------------------

#[tokio::test]
async fn oai_pmh_full_verbs_resumption_token_and_deleted_skip() {
    let net =
        Arc::new(ReplayNet::from_file(crate_fixtures_dir().join("seed_hal_oai.json")).unwrap());
    let (reg, deps) = build_reg(net.clone());
    let adapter = reg.protocol("oai_pmh").unwrap();
    let cx = default_cx(&deps);
    let base = "https://api.archives-ouvertes.fr/oai/hal/";

    let ident = adapter
        .call(base, "identify", json!({}), &cx)
        .await
        .unwrap();
    assert_eq!(ident.items.len(), 1);
    assert_eq!(ident.items[0]["repositoryName"], "HAL");

    let sets = adapter
        .call(base, "list_sets", json!({}), &cx)
        .await
        .unwrap();
    assert_eq!(sets.items.len(), 2);
    assert_eq!(sets.items[0]["setSpec"], "collection:CNRS");
    assert_eq!(sets.next_cursor.as_deref(), Some("sets_tok_2"));

    // list_records: skips deleted header, returns composite oaiskip: cursor while records remain on page 1 (B3).
    let recs1 = adapter
        .call(
            base,
            "list_records",
            json!({"set": "collection:CNRS", "from": "2026-01-01", "limit": 1}),
            &cx,
        )
        .await
        .unwrap();
    assert_eq!(recs1.items.len(), 1);
    let w: ScholarlyWork = serde_json::from_value(recs1.items[0].clone()).unwrap();
    assert_eq!(w.title, "Neural Operators in Fluid Dynamics");
    assert_eq!(w.ids.doi.as_deref(), Some("10.1234/hal.2026.001"));
    assert_eq!(w.year, Some(2026));
    let skip_cur = recs1
        .next_cursor
        .as_deref()
        .expect("expected composite oaiskip cursor");
    assert!(skip_cur.starts_with("oaiskip:"), "got {skip_cur:?}");

    // Follow oaiskip: cursor -> yields the second live record on page 1, THEN returns raw resumptionToken.
    let recs1b = adapter
        .call(
            base,
            "list_records",
            json!({"cursor": skip_cur, "limit": 1}),
            &cx,
        )
        .await
        .unwrap();
    assert_eq!(recs1b.items.len(), 1);
    let w1b: ScholarlyWork = serde_json::from_value(recs1b.items[0].clone()).unwrap();
    assert_eq!(w1b.title, "Second Record Sliced Out");
    assert_eq!(recs1b.next_cursor.as_deref(), Some("rec_tok_2"));

    // Subsequent page uses EXCLUSIVE resumptionToken (no metadataPrefix/set/from).
    let page2 = adapter
        .call(
            base,
            "list_records",
            json!({"cursor": "rec_tok_2", "limit": 5}),
            &cx,
        )
        .await
        .unwrap();
    assert_eq!(page2.items.len(), 1);
    assert!(page2.next_cursor.is_none());

    // get_record
    let single = adapter
        .call(
            base,
            "get_record",
            json!({"identifier": "oai:HAL:hal-04000001v1"}),
            &cx,
        )
        .await
        .unwrap();
    assert_eq!(single.items.len(), 1);
    let w1: ScholarlyWork = serde_json::from_value(single.items[0].clone()).unwrap();
    assert_eq!(w1.title, "Neural Operators in Fluid Dynamics");
    assert!(net.unmatched().is_empty() && net.unused().is_empty());
}

// ---------------------------------------------------------------------------
// 5. SRU: CQL template wrapping, verbatim passthrough, explain, MARCXML, diagnostics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sru_cql_wrapping_start_record_explain_and_marcxml() {
    let net = Arc::new(ReplayNet::from_file(crate_fixtures_dir().join("sru_ops.json")).unwrap());
    let (reg, deps) = build_reg(net.clone());
    let adapter = reg.protocol("sru").unwrap();

    // 1. Plain keyword wrapped with dialect.query_template (escaping quotes) + start > 1
    let mut cx = default_cx(&deps);
    cx.dialect.insert("version".into(), json!("1.2"));
    cx.dialect
        .insert("record_schema".into(), json!("dublincore"));
    cx.dialect
        .insert("query_template".into(), json!("bib.title all \"{query}\""));

    let out = adapter
        .call(
            "https://catalogue.bnf.fr/api/SRU",
            "search",
            json!({"query": "neural \"nets\"", "limit": 2, "start": 3}),
            &cx,
        )
        .await
        .unwrap();
    assert_eq!(out.items.len(), 1);
    let w: ScholarlyWork = serde_json::from_value(out.items[0].clone()).unwrap();
    assert_eq!(w.title, "Neural Networks Monograph");
    assert_eq!(out.next_cursor.as_deref(), Some("4"));

    // 2. Query with CQL operator `=` passes through verbatim (not double-wrapped) + MARCXML
    let marc = adapter
        .call(
            "https://catalogue.bnf.fr/api/SRU",
            "search",
            json!({"query": "dc.title=algorithms", "limit": 1, "record_schema": "marcxml"}),
            &cx,
        )
        .await
        .unwrap();
    let mw: ScholarlyWork = serde_json::from_value(marc.items[0].clone()).unwrap();
    assert_eq!(mw.title, "Introduction to Algorithms");
    assert_eq!(mw.authors[0].name, "Cormen, Thomas H");
    assert_eq!(mw.year, Some(2022));

    // 3. explain op
    let exp = adapter
        .call(
            "https://catalogue.bnf.fr/api/SRU",
            "explain",
            json!({}),
            &cx,
        )
        .await
        .unwrap();
    assert_eq!(exp.items.len(), 1);
    assert_eq!(exp.items[0]["title"], "BnF General Catalog");
    assert!(net.unmatched().is_empty() && net.unused().is_empty());
}

// ---------------------------------------------------------------------------
// 6. OpenSearch: describe, url_template bypass, JSON-LD feed, OSDD cache vs replay_mode
// ---------------------------------------------------------------------------

#[tokio::test]
async fn opensearch_describe_cache_behavior_and_json_template_bypass() {
    let net =
        Arc::new(ReplayNet::from_file(crate_fixtures_dir().join("opensearch_ops.json")).unwrap());
    let (reg, deps) = build_reg(net.clone());
    let adapter = reg.protocol("opensearch").unwrap();

    // 1. url_template dialect bypasses OSDD completely (1 exchange, JSON-LD response)
    let mut cx_tpl = default_cx(&deps);
    cx_tpl.dialect.insert(
        "url_template".into(),
        json!("https://cir.nii.ac.jp/opensearch/v2/all?q={searchTerms}&count={count?}&format=json"),
    );
    let out = adapter
        .call(
            "https://cir.nii.ac.jp/opensearch/description.xml",
            "search",
            json!({"query": "rust", "limit": 2}),
            &cx_tpl,
        )
        .await
        .unwrap();
    assert_eq!(out.items.len(), 1);
    let h: Hit = serde_json::from_value(out.items[0].clone()).unwrap();
    assert_eq!(h.title, "Safe Systems Programming in Rust");
    assert_eq!(h.date.as_deref(), Some("2025-04-01"));

    // 2. describe + cold cache test: replay_mode=false caches OSDD across two searches!
    let mut cx_live = default_cx(&deps);
    cx_live.replay_mode = false;
    let desc = adapter
        .call(
            "https://example.org/opensearch.xml",
            "describe",
            json!({}),
            &cx_live,
        )
        .await
        .unwrap();
    assert_eq!(desc.items[0]["shortName"], "Example Catalog");
    assert_eq!(net.hits().len(), 2); // 1 from json template + 1 OSDD describe

    // Second call with replay_mode=false reuses cached OSDD (only 1 feed HTTP exchange!)
    let s1 = adapter
        .call(
            "https://example.org/opensearch.xml",
            "search",
            json!({"query": "climate", "limit": 2}),
            &cx_live,
        )
        .await
        .unwrap();
    assert_eq!(s1.items.len(), 1);
    assert_eq!(net.hits().len(), 3); // OSDD was cached!

    // With replay_mode=true, OSDD cache MUST be bypassed (2 HTTP exchanges: OSDD + feed).
    let mut cx_replay = default_cx(&deps);
    cx_replay.replay_mode = true;
    let _s2 = adapter
        .call(
            "https://example.org/opensearch.xml",
            "search",
            json!({"query": "climate", "limit": 2}),
            &cx_replay,
        )
        .await
        .unwrap();
    assert_eq!(net.hits().len(), 5); // +2 exchanges (OSDD + feed)
    assert!(net.unmatched().is_empty());
}

// ---------------------------------------------------------------------------
// 7. SPARQL: POST method, ASK query, prefixes & automatic LIMIT (only when absent)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sparql_post_ask_prefixes_and_limit_rules() {
    let query_no_limit = "SELECT ?s WHERE { ?s a ex:Paper }";
    let expected_post_body =
        "PREFIX ex: <http://example.org/>\nSELECT ?s WHERE { ?s a ex:Paper }\nLIMIT 25";

    let mut post_req = FixtureRequest::new(
        "POST",
        "https://sparql.example.org/query?default-graph-uri=http%3A%2F%2Fexample.org%2Fg",
    );
    post_req
        .headers_subset
        .insert("content-type".into(), "application/sparql-query".into());
    post_req
        .headers_subset
        .insert("accept".into(), "application/sparql-results+json".into());
    post_req.body_sha256 = Some(sha256_hex(expected_post_body.as_bytes()));

    let post_resp = FixtureResponse::from_bytes(
        200,
        vec![("content-type".into(), "application/sparql-results+json".into())],
        br#"{"head":{"vars":["s"]},"results":{"bindings":[{"s":{"type":"uri","value":"http://example.org/p1"}}]}}"#,
    );

    let mut ask_req = FixtureRequest::new(
        "GET",
        "https://sparql.example.org/query?query=ASK+%7B+%3Fs+%3Fp+%3Fo+%7D",
    );
    ask_req
        .headers_subset
        .insert("accept".into(), "application/sparql-results+json".into());
    let ask_resp = FixtureResponse::from_bytes(
        200,
        vec![(
            "content-type".into(),
            "application/sparql-results+json".into(),
        )],
        br#"{"head":{},"boolean":true}"#,
    );

    let mut f = Fixture::new("sparql", "query", "2026-11-25T12:00:00Z");
    f.exchanges.push(Exchange::new(post_req, post_resp));
    f.exchanges.push(Exchange::new(ask_req, ask_resp));
    let net = Arc::new(ReplayNet::from_fixtures(vec![f]).unwrap());
    let (reg, deps) = build_reg(net.clone());
    let adapter = reg.protocol("sparql").unwrap();

    // POST + prefixes + default_graph_uri + auto LIMIT
    let mut cx = default_cx(&deps);
    cx.dialect.insert("method".into(), json!("POST"));
    cx.dialect
        .insert("default_graph_uri".into(), json!("http://example.org/g"));
    cx.dialect
        .insert("prefixes".into(), json!({"ex": "http://example.org/"}));
    let out = adapter
        .call(
            "https://sparql.example.org/query",
            "query",
            json!({"query": query_no_limit, "limit": 25}),
            &cx,
        )
        .await
        .unwrap();
    assert_eq!(out.items, vec![json!({"s": "http://example.org/p1"})]);

    // ASK query (never appends LIMIT)
    let cx_ask = default_cx(&deps);
    let ask_out = adapter
        .call(
            "https://sparql.example.org/query",
            "query",
            json!({"query": "ASK { ?s ?p ?o }"}),
            &cx_ask,
        )
        .await
        .unwrap();
    assert_eq!(ask_out.items, vec![json!({"boolean": true})]);
    assert!(net.unmatched().is_empty() && net.unused().is_empty());
}

// ---------------------------------------------------------------------------
// 8. DSpace 7, InvenioRDM (incl. Zenodo parity), EPrints secondary ops & errors
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dspace7_item_lookup_dialect_options_and_path_guard() {
    let body = json!({
        "id": "11111111-2222-3333-4444-555555555555",
        "uuid": "11111111-2222-3333-4444-555555555555",
        "handle": "12345/6789",
        "inArchive": true,
        "metadata": {
            "dc.title": [{"value": "Global Malaria Report 2025"}],
            "dc.contributor.author": [{"value": "World Health Organization"}],
            "dc.date.issued": [{"value": "2025-11-01"}],
            "dc.identifier.doi": [{"value": "10.1000/who.2025"}]
        }
    })
    .to_string();
    let net = single_exchange(
        "dspace7",
        "item",
        FixtureRequest::new(
            "GET",
            "https://repo.example.edu/api7/core/items/11111111-2222-3333-4444-555555555555",
        ),
        200,
        "application/hal+json",
        &body,
    );
    let (reg, deps) = build_reg(net.clone());
    let adapter = reg.protocol("dspace7").unwrap();
    let mut cx = default_cx(&deps);
    cx.dialect.insert("api_prefix".into(), json!("/api7"));

    // Path traversal rejected before network.
    let err = adapter
        .call(
            "https://repo.example.edu",
            "item",
            json!({"id": "../secret"}),
            &cx,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, FsmError::InvalidInput(_)));

    let out = adapter
        .call(
            "https://repo.example.edu",
            "item",
            json!({"id": "11111111-2222-3333-4444-555555555555"}),
            &cx,
        )
        .await
        .unwrap();
    let w: ScholarlyWork = serde_json::from_value(out.items[0].clone()).unwrap();
    assert_eq!(w.title, "Global Malaria Report 2025");
    assert_eq!(w.url, "https://repo.example.edu/handle/12345/6789");
    assert_eq!(w.ids.doi.as_deref(), Some("10.1000/who.2025"));
    // I4: inArchive without positive OA rights metadata must NOT be marked OA.
    assert!(w.oa.is_none());
    assert!(net.unmatched().is_empty());
}

#[tokio::test]
async fn inveniordm_community_record_and_zenodo_parity_tests() {
    // Ported from Python tests/test_media_engines.py (_ZENODO vectors):
    // prefers self_html, falls back to doi_url, skips untitled records.
    let search_body = json!({
        "hits": {
            "total": 2,
            "hits": [
                {
                    "doi": "10.5281/zenodo.123",
                    "links": {"self_html": "https://zenodo.org/records/123"},
                    "files": [{"key": "paper.pdf", "links": {"self": "https://zenodo.org/api/files/paper.pdf"}}],
                    "metadata": {
                        "title": "All Countries' Dataset",
                        "publication_date": "2025-09-10",
                        "resource_type": {"type": "dataset"},
                        "access_right": "open",
                        "creators": [{"name": "Ada L"}],
                        "description": "<p>Tidy country data.</p>"
                    }
                },
                {
                    "doi_url": "https://doi.org/10.5281/zenodo.456",
                    "metadata": {"title": "DOI only", "publication_date": "2024-01-01"}
                },
                {"metadata": {"title": ""}}
            ]
        }
    })
    .to_string();
    let rec_body = json!({
        "id": "abc12-xyz34",
        "pids": {"doi": {"identifier": "10.22002/abc12"}},
        "links": {"self_html": "https://data.caltech.edu/records/abc12-xyz34"},
        "metadata": {
            "title": "Single Record Lookup",
            "publication_date": "2026-02-14",
            "creators": [{"person_or_org": {"name": "Curie, Marie"}}]
        }
    })
    .to_string();

    let mut f = Fixture::new("inveniordm", "ops", "2026-11-25T12:00:00Z");
    f.exchanges.push(Exchange::new(
        FixtureRequest::new(
            "GET",
            "https://zenodo.org/api/communities/climate/records?q=country&size=5&page=2&sort=newest",
        ),
        FixtureResponse::from_bytes(
            200,
            vec![("content-type".into(), "application/json".into())],
            search_body.as_bytes(),
        ),
    ));
    f.exchanges.push(Exchange::new(
        FixtureRequest::new("GET", "https://data.caltech.edu/api/records/abc12-xyz34"),
        FixtureResponse::from_bytes(
            200,
            vec![("content-type".into(), "application/json".into())],
            rec_body.as_bytes(),
        ),
    ));

    let net = Arc::new(ReplayNet::from_fixtures(vec![f]).unwrap());
    let (reg, deps) = build_reg(net.clone());
    let adapter = reg.protocol("inveniordm").unwrap();

    let mut cx = default_cx(&deps);
    cx.dialect.insert("community".into(), json!("climate"));
    let out = adapter
        .call(
            "https://zenodo.org",
            "search",
            json!({"query": "country", "limit": 5, "page": 2, "sort": "newest"}),
            &cx,
        )
        .await
        .unwrap();
    // Skips untitled record -> 2 items.
    assert_eq!(out.items.len(), 2);
    let w0: ScholarlyWork = serde_json::from_value(out.items[0].clone()).unwrap();
    let w1: ScholarlyWork = serde_json::from_value(out.items[1].clone()).unwrap();
    assert_eq!(w0.url, "https://zenodo.org/records/123");
    assert_eq!(w0.abstract_text.as_deref(), Some("Tidy country data."));
    assert_eq!(w0.published_date.as_deref(), Some("2025-09-10"));
    assert_eq!(
        w0.oa.as_ref().and_then(|o| o.pdf_url.as_deref()),
        Some("https://zenodo.org/api/files/paper.pdf")
    );
    assert_eq!(w1.url, "https://doi.org/10.5281/zenodo.456");

    let cx_rec = default_cx(&deps);
    let rec = adapter
        .call(
            "https://data.caltech.edu",
            "record",
            json!({"id": "abc12-xyz34"}),
            &cx_rec,
        )
        .await
        .unwrap();
    let wr: ScholarlyWork = serde_json::from_value(rec.items[0].clone()).unwrap();
    assert_eq!(wr.title, "Single Record Lookup");
    assert_eq!(wr.authors[0].name, "Curie, Marie");
    assert_eq!(wr.ids.doi.as_deref(), Some("10.22002/abc12"));
    assert!(net.unmatched().is_empty() && net.unused().is_empty());
}

#[tokio::test]
async fn eprints_html_rejection_record_op_and_bounded_truncation_salvage() {
    // 1. HTML fallback page must be rejected as Upstream error
    let html_net = single_exchange(
        "eprints",
        "search",
        FixtureRequest::new(
            "GET",
            "https://whiterose.example.ac.uk/cgi/search/simple?q=neural&output=JSON",
        ),
        200,
        "text/html; charset=utf-8",
        "<!doctype html><html><body>Search Results</body></html>",
    );
    let (reg, deps) = build_reg(html_net);
    let adapter = reg.protocol("eprints").unwrap();
    let cx = default_cx(&deps);
    let err = adapter
        .call(
            "https://whiterose.example.ac.uk",
            "search",
            json!({"query": "neural"}),
            &cx,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, FsmError::Upstream { .. }), "got {err:?}");

    // 2. Bounded max_response_bytes truncates a huge EPrints array mid-second-item,
    //    yet the first complete object is salvaged cleanly!
    let pad = "a".repeat(70_000);
    let huge_json = format!(
        r#"[{{"eprintid":1,"title":"First Complete Paper","date":2025,"uri":"https://ep.example/1"}},{{"eprintid":2,"title":"Second Huge Paper","abstract":"{pad}"}}]"#
    );
    let trunc_net = single_exchange(
        "eprints",
        "search",
        FixtureRequest::new(
            "GET",
            "https://ep.example/cgi/search/simple?q=huge&output=JSON",
        ),
        200,
        "application/json",
        &huge_json,
    );
    let (reg2, deps2) = build_reg(trunc_net);
    let adapter2 = reg2.protocol("eprints").unwrap();
    let mut cx2 = default_cx(&deps2);
    cx2.dialect
        .insert("max_response_bytes".into(), json!(65_536));
    let out = adapter2
        .call(
            "https://ep.example",
            "search",
            json!({"query": "huge", "limit": 5}),
            &cx2,
        )
        .await
        .unwrap();
    assert_eq!(out.items.len(), 1);
    let w: ScholarlyWork = serde_json::from_value(out.items[0].clone()).unwrap();
    assert_eq!(w.title, "First Complete Paper");

    // 3. Record op by numeric id + rejection of non-numeric id
    let rec_net = single_exchange(
        "eprints",
        "record",
        FixtureRequest::new(
            "GET",
            "https://ep.example/cgi/export/eprint/438715/JSON/eprint-438715.js",
        ),
        200,
        "application/json",
        r#"{"eprintid":438715,"title":[{"name":"Deutscher Titel","lang":"deu"},{"name":"English Title","lang":"eng"}],"date":"2026-03","uri":"https://ep.example/id/eprint/438715"}"#,
    );
    let (reg3, deps3) = build_reg(rec_net);
    let adapter3 = reg3.protocol("eprints").unwrap();
    let cx3 = default_cx(&deps3);
    assert!(matches!(
        adapter3
            .call(
                "https://ep.example",
                "record",
                json!({"id": "../bad"}),
                &cx3
            )
            .await,
        Err(FsmError::InvalidInput(_))
    ));
    let rec_out = adapter3
        .call(
            "https://ep.example",
            "record",
            json!({"id": "438715"}),
            &cx3,
        )
        .await
        .unwrap();
    let wr: ScholarlyWork = serde_json::from_value(rec_out.items[0].clone()).unwrap();
    assert_eq!(wr.title, "English Title");
    assert_eq!(wr.published_date.as_deref(), Some("2026-03"));
}

// ---------------------------------------------------------------------------
// 9. Cross-adapter validation, HTTP 429 / 500, and cancellation checks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dialect_validation_rejects_unknown_and_invalid_values() {
    let net = Arc::new(ReplayNet::from_fixtures(vec![]).unwrap());
    let (reg, _) = build_reg(net);

    let all_kinds = [
        "oai_pmh",
        "sru",
        "opensearch",
        "sparql",
        "dspace7",
        "inveniordm",
        "dataverse",
        "eprints",
        "mediawiki",
    ];
    for kind in all_kinds {
        let adapter: Arc<dyn ProtocolAdapter> = reg.protocol(kind).unwrap();
        assert!(adapter.validate_dialect(&BTreeMap::new()).is_ok());

        let mut bad = BTreeMap::new();
        bad.insert("nonexistent_key".into(), json!(true));
        assert!(
            matches!(
                adapter.validate_dialect(&bad),
                Err(FsmError::InvalidInput(_))
            ),
            "{kind} accepted unknown dialect key"
        );
    }

    // Specific range/enum checks
    let mut bad_sru = BTreeMap::new();
    bad_sru.insert("version".into(), json!("2.0"));
    assert!(
        reg.protocol("sru")
            .unwrap()
            .validate_dialect(&bad_sru)
            .is_err()
    );

    let mut bad_eprints = BTreeMap::new();
    bad_eprints.insert("max_response_bytes".into(), json!(1024));
    assert!(
        reg.protocol("eprints")
            .unwrap()
            .validate_dialect(&bad_eprints)
            .is_err()
    );

    let mut bad_sparql = BTreeMap::new();
    bad_sparql.insert("method".into(), json!("DELETE"));
    assert!(
        reg.protocol("sparql")
            .unwrap()
            .validate_dialect(&bad_sparql)
            .is_err()
    );
}

#[tokio::test]
async fn rate_limit_429_and_cancellation_map_to_expected_fsm_errors() {
    let mut f = Fixture::new("dataverse", "search", "2026-11-25T12:00:00Z");
    let resp = FixtureResponse::from_bytes(
        429,
        vec![
            ("content-type".into(), "application/json".into()),
            ("retry-after".into(), "42".into()),
        ],
        br#"{"status":"ERROR","message":"Too many requests"}"#,
    );
    f.exchanges.push(Exchange::new(
        FixtureRequest::new(
            "GET",
            "https://dataverse.harvard.edu/api/search?q=x&type=dataset&per_page=10",
        ),
        resp,
    ));
    let net = Arc::new(ReplayNet::from_fixtures(vec![f]).unwrap());
    let (reg, deps) = build_reg(net);
    let adapter = reg.protocol("dataverse").unwrap();

    // 429 -> RateLimited with retry_after_s = Some(42)
    let cx = default_cx(&deps);
    let err = adapter
        .call(
            "https://dataverse.harvard.edu",
            "search",
            json!({"query": "x"}),
            &cx,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            FsmError::RateLimited {
                retry_after_s: Some(42),
                ..
            }
        ),
        "expected RateLimited(42), got {err:?}"
    );

    // Cancelled token -> Timeout(0)
    let cx_cancel = default_cx(&deps);
    cx_cancel.cancel.cancel();
    let err_cancel = adapter
        .call(
            "https://dataverse.harvard.edu",
            "search",
            json!({"query": "x"}),
            &cx_cancel,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err_cancel, FsmError::Timeout(0)),
        "got {err_cancel:?}"
    );
}

// ---------------------------------------------------------------------------
// 10. Python oracle verbatim parity tests (test_reference_engines.py,
//     test_source_enhancement.py, test_media_engines.py)
// ---------------------------------------------------------------------------

const ORACLE_WIKI_BODY: &str = r#"{"query":{"search":[{"title":"Model Context Protocol","pageid":79706999,"snippet":"The <span class=\"searchmatch\">Model</span> Context Protocol (MCP) is an open standard &amp; framework"},{"title":"","snippet":"no title"}]}}"#;

const ORACLE_WIKIMEDIA_BODY: &str = r#"{"continue":{"gsroffset":2,"continue":"gsroffset||"},"query":{"pages":{"101799395":{"pageid":101799395,"title":"File:Roscoe Wind Farm in West Texas.jpg","imageinfo":[{"url":"https://upload.wikimedia.org/wikipedia/commons/d/d4/Roscoe_Wind_Farm_in_West_Texas.jpg?utm_source=commons.wikimedia.org&utm_campaign=imageinfo&utm_content=original","timestamp":"2021-03-19T02:23:31Z","width":2871,"height":1914,"extmetadata":{"ImageDescription":{"value":"Wind Turbines and an old windmill at the Roscoe Wind Farm in West Texas"},"LicenseShortName":{"value":"CC BY-SA 4.0"},"Artist":{"value":"<a rel=\"nofollow\" class=\"external text\" href=\"https://matthewtrader.com\">Matthew T Rader</a>"}}}]}}}}"#;

const ORACLE_DATAVERSE_BODY: &str = r#"{"status":"OK","data":{"start":0,"total_count":5374,"items":[{"name":"Texas Traffic Accidents 2003-9 (CRIS)","type":"dataset","url":"https://doi.org/10.7910/DVN/GGLKEM","global_id":"doi:10.7910/DVN/GGLKEM","description":"Traffic accident (CRIS) files from 2003-2009 obtained from the Center for Transportation Research at the University of Texas at Austin that cover accidents across the state of Texas.","published_at":"2017-09-01T21:53:19Z","authors":["Fisher, Paul J.","Gallagher, Justin"]}]}}"#;

async fn run_oracle_wiki_parse() -> Vec<Hit> {
    let net = single_exchange(
        "wikipedia",
        "search",
        FixtureRequest::new(
            "GET",
            "https://en.wikipedia.org/w/api.php?action=query&list=search&srsearch=Model+Context+Protocol&srlimit=10&format=json",
        ),
        200,
        "application/json",
        ORACLE_WIKI_BODY,
    );
    let (reg, deps) = build_reg(net);
    let adapter = reg.protocol("mediawiki").unwrap();
    let mut cx = default_cx(&deps);
    cx.dialect.insert("dynamic_lang_host".into(), json!(true));
    let out = adapter
        .call(
            "https://{lang_sub}.wikipedia.org/w/api.php",
            "search",
            json!({"query": "Model Context Protocol"}),
            &cx,
        )
        .await
        .unwrap();
    out.items
        .into_iter()
        .map(|v| serde_json::from_value(v).unwrap())
        .collect()
}

#[tokio::test]
async fn test_wikipedia_strips_search_highlight_markup() {
    let hits = run_oracle_wiki_parse().await;
    assert!(!hits[0].snippet.contains("<span"));
    assert!(
        hits[0]
            .snippet
            .starts_with("The Model Context Protocol (MCP) is an open standard")
    );
}

#[tokio::test]
async fn test_wikipedia_unescapes_entities() {
    let hits = run_oracle_wiki_parse().await;
    assert!(
        hits[0].snippet.contains("open standard & framework"),
        "got {:?}",
        hits[0].snippet
    );
}

#[tokio::test]
async fn test_wikipedia_builds_article_urls_from_titles() {
    let hits = run_oracle_wiki_parse().await;
    assert_eq!(
        hits[0].url,
        "https://en.wikipedia.org/wiki/Model_Context_Protocol"
    );
}

#[tokio::test]
async fn test_wikipedia_skips_untitled_entries() {
    let hits = run_oracle_wiki_parse().await;
    assert_eq!(hits.len(), 1);
}

#[tokio::test]
async fn test_wikipedia_language_follows_region() {
    let net = CapturingNet::fixed("application/json", r#"{"query":{"search":[]}}"#);
    let (reg, deps) = build_reg_with_net(net.clone());
    let adapter = reg.protocol("mediawiki").unwrap();

    for (region, expected_host) in [("cn-zh", "zh.wikipedia.org"), ("us-en", "en.wikipedia.org")] {
        let mut cx = default_cx(&deps);
        cx.region = region.into();
        cx.dialect.insert("dynamic_lang_host".into(), json!(true));
        adapter
            .call(
                "https://{lang_sub}.wikipedia.org/w/api.php",
                "search",
                json!({"query": "test"}),
                &cx,
            )
            .await
            .unwrap();
        let reqs = net.captured();
        let u = CapturingNet::full_url(reqs.last().unwrap());
        assert_eq!(u.host_str(), Some(expected_host), "region {region:?}");
    }
}

#[tokio::test]
async fn test_wikipedia_bad_region_falls_back_to_english() {
    let net = CapturingNet::fixed("application/json", r#"{"query":{"search":[]}}"#);
    let (reg, deps) = build_reg_with_net(net.clone());
    let adapter = reg.protocol("mediawiki").unwrap();

    for region in ["", "garbage", "us-", "us-e/../n", "us-toolongtag"] {
        let mut cx = default_cx(&deps);
        cx.region = region.into();
        cx.dialect.insert("dynamic_lang_host".into(), json!(true));
        adapter
            .call(
                "https://{lang_sub}.wikipedia.org/w/api.php",
                "search",
                json!({"query": "test"}),
                &cx,
            )
            .await
            .unwrap();
        let reqs = net.captured();
        let u = CapturingNet::full_url(reqs.last().unwrap());
        assert_eq!(u.host_str(), Some("en.wikipedia.org"), "region {region:?}");
    }
}

#[tokio::test]
async fn test_wikimedia_build_url_requests_files_and_licence_metadata() {
    // Regression test for B1 (A1 erratum #1): must request size|mime|timestamp in iiprop.
    let net = CapturingNet::fixed("application/json", r#"{"query":{"pages":{}}}"#);
    let (reg, deps) = build_reg_with_net(net.clone());
    let adapter = reg.protocol("mediawiki").unwrap();
    let cx = default_cx(&deps);

    adapter
        .call(
            "https://commons.wikimedia.org/w/api.php",
            "search",
            json!({"query": "wind farm", "limit": 5, "namespace": 6, "prop": "imageinfo"}),
            &cx,
        )
        .await
        .unwrap();

    let reqs = net.captured();
    assert_eq!(reqs.len(), 1);
    let u = CapturingNet::full_url(&reqs[0]);
    let qp: BTreeMap<String, String> = u.query_pairs().into_owned().collect();
    assert_eq!(qp.get("generator").map(String::as_str), Some("search"));
    assert_eq!(qp.get("gsrnamespace").map(String::as_str), Some("6"));
    assert_eq!(qp.get("prop").map(String::as_str), Some("imageinfo"));
    assert_eq!(
        qp.get("iiprop").map(String::as_str),
        Some("url|extmetadata|size|mime|timestamp")
    );
}

#[tokio::test]
async fn test_wikimedia_maps_page_object_values_to_direct_image_urls() {
    // Regression test for B1 using Python test_source_enhancement.py `_WIKIMEDIA` verbatim.
    let net = single_exchange(
        "wikimedia",
        "search",
        FixtureRequest::new(
            "GET",
            "https://commons.wikimedia.org/w/api.php?action=query&generator=search&gsrsearch=wind+farm&gsrlimit=10&gsrnamespace=6&prop=imageinfo&iiprop=url%7Cextmetadata%7Csize%7Cmime%7Ctimestamp&format=json",
        ),
        200,
        "application/json",
        ORACLE_WIKIMEDIA_BODY,
    );
    let (reg, deps) = build_reg(net);
    let adapter = reg.protocol("mediawiki").unwrap();
    let cx = default_cx(&deps);

    let out = adapter
        .call(
            "https://commons.wikimedia.org/w/api.php",
            "search",
            json!({"query": "wind farm", "namespace": 6, "prop": "imageinfo"}),
            &cx,
        )
        .await
        .unwrap();
    assert_eq!(out.items.len(), 1);
    let hit: Hit = serde_json::from_value(out.items[0].clone()).unwrap();
    assert_eq!(hit.title, "Roscoe Wind Farm in West Texas.jpg");
    assert!(hit.url.starts_with(
        "https://upload.wikimedia.org/wikipedia/commons/d/d4/Roscoe_Wind_Farm_in_West_Texas.jpg"
    ));
    assert!(
        hit.snippet.starts_with(
            "CC BY-SA 4.0 \u{00B7} by Matthew T Rader \u{00B7} 2871\u{00D7}1914px \u{2014}"
        ),
        "got {:?}",
        hit.snippet
    );
    assert_eq!(hit.date.as_deref(), Some("2021-03-19"));
    assert_eq!(out.next_cursor.as_deref(), Some("2"));
}

#[tokio::test]
async fn test_dataverse_maps_the_public_doi_record() {
    // Ported from Python test_source_enhancement.py `_DATAVERSE` verbatim.
    let net = single_exchange(
        "dataverse",
        "search",
        FixtureRequest::new(
            "GET",
            "https://dataverse.harvard.edu/api/search?q=texas+traffic&type=dataset&per_page=10",
        ),
        200,
        "application/json",
        ORACLE_DATAVERSE_BODY,
    );
    let (reg, deps) = build_reg(net);
    let adapter = reg.protocol("dataverse").unwrap();
    let cx = default_cx(&deps);

    let out = adapter
        .call(
            "https://dataverse.harvard.edu",
            "search",
            json!({"query": "texas traffic"}),
            &cx,
        )
        .await
        .unwrap();
    assert_eq!(out.items.len(), 1);
    assert_eq!(
        out.items[0]["title"],
        "Texas Traffic Accidents 2003-9 (CRIS)"
    );
    assert_eq!(out.items[0]["url"], "https://doi.org/10.7910/DVN/GGLKEM");
    assert!(
        out.items[0]["snippet"]
            .as_str()
            .unwrap()
            .contains("Center for Transportation Research")
    );
    assert_eq!(out.items[0]["date"], "2017-09-01");
}

#[test]
fn test_registered_with_its_category() {
    // Ported from Python tests/test_media_engines.py: verify seed scopes for wikimedia and dataverse.
    let mw_text =
        std::fs::read_to_string(seed_instances_dir().join("academic_mediawiki.toml")).unwrap();
    let mw = InstanceFileDecl::from_toml(&mw_text).unwrap();
    let wikimedia = mw.instance.iter().find(|i| i.id == "wikimedia").unwrap();
    assert!(
        wikimedia.scopes.iter().any(|s| s == "media"),
        "wikimedia scopes: {:?}",
        wikimedia.scopes
    );

    let dv_text =
        std::fs::read_to_string(seed_instances_dir().join("academic_dataverse.toml")).unwrap();
    let dv = InstanceFileDecl::from_toml(&dv_text).unwrap();
    let dataverse = dv.instance.iter().find(|i| i.id == "dataverse").unwrap();
    assert!(
        dataverse.scopes.iter().any(|s| s == "dataset"),
        "dataverse scopes: {:?}",
        dataverse.scopes
    );
}

// ---------------------------------------------------------------------------
// 11. Review Round 1 regression tests (B2, B3, I1–I7, M1–M8)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mediawiki_non_w_api_uses_index_php_article_url() {
    // Regression test for B2.
    let body =
        json!({"query":{"search":[{"title":"Systemd","snippet":"Init system"}]}}).to_string();
    let net = CapturingNet::fixed("application/json", body);
    let (reg, deps) = build_reg_with_net(net);
    let adapter = reg.protocol("mediawiki").unwrap();
    let cx = default_cx(&deps);

    let out_root = adapter
        .call(
            "https://wiki.archlinux.org/api.php",
            "search",
            json!({"query": "systemd"}),
            &cx,
        )
        .await
        .unwrap();
    let h_root: Hit = serde_json::from_value(out_root.items[0].clone()).unwrap();
    assert_eq!(
        h_root.url,
        "https://wiki.archlinux.org/index.php?title=Systemd"
    );

    let mut cx_sub = default_cx(&deps);
    cx_sub
        .dialect
        .insert("api_path".into(), json!("/docs/api.php"));
    let out_sub = adapter
        .call(
            "https://wiki.example.org",
            "search",
            json!({"query": "systemd"}),
            &cx_sub,
        )
        .await
        .unwrap();
    let h_sub: Hit = serde_json::from_value(out_sub.items[0].clone()).unwrap();
    assert_eq!(
        h_sub.url,
        "https://wiki.example.org/docs/index.php?title=Systemd"
    );
}

#[tokio::test]
async fn oai_pmh_slice_paging_does_not_skip_records() {
    // Regression test for B3 (A1 erratum #2): 3 live records on page 1 + T-PAGE2 + 1 record on page 2.
    let page1 = r#"<?xml version="1.0" encoding="UTF-8"?><OAI-PMH xmlns="http://www.openarchives.org/OAI/2.0/"><ListRecords>
        <record><header><identifier>oai:ex:1</identifier><datestamp>2026-01-01</datestamp></header><metadata><oai_dc:dc xmlns:oai_dc="http://www.openarchives.org/OAI/2.0/oai_dc/" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>Paper 1</dc:title><dc:identifier>https://repo.example/1</dc:identifier></oai_dc:dc></metadata></record>
        <record><header><identifier>oai:ex:2</identifier><datestamp>2026-01-02</datestamp></header><metadata><oai_dc:dc xmlns:oai_dc="http://www.openarchives.org/OAI/2.0/oai_dc/" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>Paper 2</dc:title><dc:identifier>https://repo.example/2</dc:identifier></oai_dc:dc></metadata></record>
        <record><header><identifier>oai:ex:3</identifier><datestamp>2026-01-03</datestamp></header><metadata><oai_dc:dc xmlns:oai_dc="http://www.openarchives.org/OAI/2.0/oai_dc/" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>Paper 3</dc:title><dc:identifier>https://repo.example/3</dc:identifier></oai_dc:dc></metadata></record>
        <resumptionToken>T-PAGE2</resumptionToken>
    </ListRecords></OAI-PMH>"#;
    let page2 = r#"<?xml version="1.0" encoding="UTF-8"?><OAI-PMH xmlns="http://www.openarchives.org/OAI/2.0/"><ListRecords>
        <record><header><identifier>oai:ex:4</identifier><datestamp>2026-01-04</datestamp></header><metadata><oai_dc:dc xmlns:oai_dc="http://www.openarchives.org/OAI/2.0/oai_dc/" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>Paper 4</dc:title><dc:identifier>https://repo.example/4</dc:identifier></oai_dc:dc></metadata></record>
        <record><header><identifier>oai:ex:5</identifier><datestamp>2026-01-05</datestamp></header><metadata><oai_dc:dc xmlns:oai_dc="http://www.openarchives.org/OAI/2.0/oai_dc/" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>Paper 5</dc:title><dc:identifier>https://repo.example/5</dc:identifier></oai_dc:dc></metadata></record>
        <resumptionToken/>
    </ListRecords></OAI-PMH>"#;

    let net = CapturingNet::with_fn(move |req, _| {
        let u = CapturingNet::full_url(req);
        let qp: BTreeMap<String, String> = u.query_pairs().into_owned().collect();
        let body = if qp.get("resumptionToken").map(String::as_str) == Some("T-PAGE2") {
            page2
        } else {
            page1
        };
        Ok(HttpBytes::new(
            req.url.clone(),
            200,
            vec![("content-type".into(), "text/xml; charset=utf-8".into())],
            body.as_bytes().to_vec(),
        ))
    });
    let (reg, deps) = build_reg_with_net(net);
    let adapter = reg.protocol("oai_pmh").unwrap();
    let cx = default_cx(&deps);

    let mut titles = Vec::new();
    let mut cursor: Option<String> = None;
    for step in 0..5 {
        let mut params = serde_json::Map::new();
        params.insert("limit".into(), json!(1));
        if let Some(c) = &cursor {
            params.insert("cursor".into(), Value::String(c.clone()));
        }
        let out = adapter
            .call(
                "https://repo.example/oai",
                "list_records",
                Value::Object(params),
                &cx,
            )
            .await
            .unwrap();
        assert_eq!(out.items.len(), 1, "step {step}");
        let w: ScholarlyWork = serde_json::from_value(out.items[0].clone()).unwrap();
        titles.push(w.title);
        cursor = out.next_cursor;
        if matches!(step, 0 | 1 | 3) {
            assert!(
                cursor.as_deref().is_some_and(|c| c.starts_with("oaiskip:")),
                "step {step} expected oaiskip cursor, got {cursor:?}"
            );
        } else if step == 2 {
            assert_eq!(cursor.as_deref(), Some("T-PAGE2"));
        } else {
            assert_eq!(cursor, None);
        }
    }
    assert_eq!(
        titles,
        vec!["Paper 1", "Paper 2", "Paper 3", "Paper 4", "Paper 5"]
    );
}

#[tokio::test]
async fn oai_pmh_real_hal_list_records_and_get_record() {
    // Regression test for B3, I3, I8: real live-recorded HAL Identify + 100-record ListRecords + GetRecord.
    let net = Arc::new(
        ReplayNet::from_file(crate_fixtures_dir().join("seed_hal_oai_real.json")).unwrap(),
    );
    let (reg, deps) = build_reg(net.clone());
    let adapter = reg.protocol("oai_pmh").unwrap();
    let cx = default_cx(&deps);
    let base = "https://api.archives-ouvertes.fr/oai/hal/";

    let ident = adapter
        .call(base, "identify", json!({}), &cx)
        .await
        .unwrap();
    assert_eq!(ident.items[0]["repositoryName"], "HAL");

    // Slice paging across the real 100-record HAL page: 20 + 20 yields distinct contiguous works.
    let slice1 = adapter
        .call(
            base,
            "list_records",
            json!({"from": "2025-01-01", "until": "2025-01-02", "limit": 20}),
            &cx,
        )
        .await
        .unwrap();
    assert_eq!(slice1.items.len(), 20);
    let skip_cur = slice1.next_cursor.as_deref().expect("oaiskip cursor");
    assert!(skip_cur.starts_with("oaiskip:"));

    let slice2 = adapter
        .call(
            base,
            "list_records",
            json!({"cursor": skip_cur, "limit": 20}),
            &cx,
        )
        .await
        .unwrap();
    assert_eq!(slice2.items.len(), 20);
    let s1_t0 = slice1.items[0]["title"].as_str().unwrap();
    let s2_t0 = slice2.items[0]["title"].as_str().unwrap();
    assert_ne!(s1_t0, s2_t0);

    // Full 100-record page returns all 100 works + HAL's raw resumptionToken.
    let full = adapter
        .call(
            base,
            "list_records",
            json!({"from": "2025-01-01", "until": "2025-01-02", "limit": 100}),
            &cx,
        )
        .await
        .unwrap();
    assert_eq!(full.items.len(), 100);
    assert_eq!(full.next_cursor.as_deref(), Some("AoEnNDg1MTk1OA=="));

    // I3 & Round 2 M1: zero works out of 100 real HAL records may have ISSN/EISSN/ISBN, URL, or bare 4-digit year as venue.
    for (i, item) in full.items.iter().enumerate() {
        let w: ScholarlyWork = serde_json::from_value(item.clone()).unwrap();
        if let Some(v) = &w.venue {
            let low = v.to_ascii_lowercase();
            assert!(
                !low.starts_with("issn:")
                    && !low.starts_with("eissn:")
                    && !low.starts_with("isbn:")
                    && !low.starts_with("http://")
                    && !low.starts_with("https://")
                    && !v.chars().all(|c| c.is_ascii_digit()),
                "HAL record {i} leaked invalid venue: {v:?}"
            );
        }
    }

    // Record 0 & GetRecord verify non-ISSN venue, DOI from "DOI: 10.…", and work_type from info:eu-repo/semantics/*.
    let w0: ScholarlyWork = serde_json::from_value(full.items[0].clone()).unwrap();
    assert_eq!(w0.title, "Generalized universal series");
    assert_eq!(
        w0.venue.as_deref(),
        Some("Monatshefte f\u{00FC}r Mathematik")
    );
    assert_eq!(w0.ids.doi.as_deref(), Some("10.1007/s00605-015-0764-1"));
    assert_eq!(w0.work_type, "article");
    assert!(w0.oa.is_some());

    let get_rec = adapter
        .call(
            base,
            "get_record",
            json!({"identifier": "oai:HAL:hal-00925108v1"}),
            &cx,
        )
        .await
        .unwrap();
    assert_eq!(get_rec.items.len(), 1);
    let wg: ScholarlyWork = serde_json::from_value(get_rec.items[0].clone()).unwrap();
    assert_eq!(
        wg.venue.as_deref(),
        Some("Monatshefte f\u{00FC}r Mathematik")
    );
    assert_eq!(wg.ids.doi.as_deref(), Some("10.1007/s00605-015-0764-1"));
    assert_eq!(wg.work_type, "article");
    assert!(net.unmatched().is_empty() && net.unused().is_empty());
}

#[tokio::test]
async fn mediawiki_snippet_punctuation_matches_real_w5g_vectors() {
    // Regression test for I1.
    let body = json!({
        "query": {
            "search": [
                {
                    "title": "Algorithm",
                    "snippet": "memetic algorithm (plural memetic <span class=\"searchmatch\">algorithms</span>) An x; <span class=\"searchmatch\">highway</span>=crossing; <span class=\"searchmatch\">Constitution</span>. [[de:<span class=\"searchmatch\">Systemd</span>]] <span class=\"searchmatch\">systemd</span>-tmpfiles",
                    "timestamp": "2026-11-15T00:00:00Z"
                }
            ]
        }
    })
    .to_string();
    let net = CapturingNet::fixed("application/json", body);
    let (reg, deps) = build_reg_with_net(net);
    let adapter = reg.protocol("mediawiki").unwrap();
    let cx = default_cx(&deps);

    let out = adapter
        .call(
            "https://en.wikipedia.org/w/api.php",
            "search",
            json!({"query": "algorithm"}),
            &cx,
        )
        .await
        .unwrap();
    let hit: Hit = serde_json::from_value(out.items[0].clone()).unwrap();
    assert_eq!(
        hit.snippet,
        "memetic algorithm (plural memetic algorithms) An x; highway=crossing; Constitution. [[de:Systemd]] systemd-tmpfiles"
    );
}

#[tokio::test]
async fn mediawiki_search_omits_revision_timestamp_from_date() {
    // Regression test for I2: wikinews COP15 (2009) edited in 2024 must have date == None.
    let body = json!({
        "query": {
            "search": [
                {
                    "title": "Copenhagen climate conference opens",
                    "snippet": "COP15 has opened on Monday",
                    "timestamp": "2024-12-18T11:22:33Z"
                }
            ]
        }
    })
    .to_string();
    let net = CapturingNet::fixed("application/json", body);
    let (reg, deps) = build_reg_with_net(net);
    let adapter = reg.protocol("mediawiki").unwrap();
    let cx = default_cx(&deps);

    let out = adapter
        .call(
            "https://en.wikinews.org/w/api.php",
            "search",
            json!({"query": "climate"}),
            &cx,
        )
        .await
        .unwrap();
    let hit: Hit = serde_json::from_value(out.items[0].clone()).unwrap();
    assert_eq!(hit.date, None);
    assert_eq!(hit.date_source, None);
    assert_eq!(
        hit.extra
            .as_ref()
            .and_then(|e| e.get("lastEdited"))
            .and_then(Value::as_str),
        Some("2024-12-18")
    );
}

#[tokio::test]
async fn dspace7_in_archive_without_oa_rights_is_not_marked_oa() {
    // Regression test for I4.
    let body = json!({
        "_embedded": {
            "searchResult": {
                "_embedded": {
                    "objects": [
                        {
                            "_embedded": {
                                "indexableObject": {
                                    "type": "item",
                                    "inArchive": true,
                                    "handle": "123/1",
                                    "metadata": {
                                        "dc.title": [{"value": "Embargoed Thesis"}]
                                    }
                                }
                            }
                        },
                        {
                            "_embedded": {
                                "indexableObject": {
                                    "type": "item",
                                    "inArchive": true,
                                    "handle": "123/2",
                                    "metadata": {
                                        "dc.title": [{"value": "Restricted Report"}],
                                        "datacite.rights": [{"value": "restricted"}]
                                    }
                                }
                            }
                        },
                        {
                            "_embedded": {
                                "indexableObject": {
                                    "type": "item",
                                    "inArchive": true,
                                    "handle": "123/3",
                                    "metadata": {
                                        "dc.title": [{"value": "Open CC Paper"}],
                                        "dc.rights.uri": [{"value": "https://creativecommons.org/licenses/by/4.0/"}]
                                    }
                                }
                            }
                        }
                    ]
                }
            }
        }
    })
    .to_string();
    let net = CapturingNet::fixed("application/hal+json", body);
    let (reg, deps) = build_reg_with_net(net);
    let adapter = reg.protocol("dspace7").unwrap();
    let cx = default_cx(&deps);

    let out = adapter
        .call(
            "https://repo.example.edu",
            "search",
            json!({"query": "thesis"}),
            &cx,
        )
        .await
        .unwrap();
    assert_eq!(out.items.len(), 3);
    let w0: ScholarlyWork = serde_json::from_value(out.items[0].clone()).unwrap();
    let w1: ScholarlyWork = serde_json::from_value(out.items[1].clone()).unwrap();
    let w2: ScholarlyWork = serde_json::from_value(out.items[2].clone()).unwrap();
    assert!(w0.oa.is_none());
    assert!(w1.oa.is_none());
    assert_eq!(w2.oa.as_ref().map(|o| o.status.as_str()), Some("green"));
    assert_eq!(
        w2.oa.as_ref().and_then(|o| o.license.as_deref()),
        Some("https://creativecommons.org/licenses/by/4.0/")
    );
}

#[tokio::test]
async fn opensearch_url_template_preserves_encoded_literals() {
    // Regression test for I5: `%3A` and `+` in url_template must not be double-encoded.
    let atom = r#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom"><entry><title>Hit 1</title><link rel="alternate" href="https://example.org/1"/></entry></feed>"#;
    let net = CapturingNet::fixed("application/atom+xml", atom);
    let (reg, deps) = build_reg_with_net(net.clone());
    let adapter = reg.protocol("opensearch").unwrap();
    let mut cx = default_cx(&deps);
    cx.dialect.insert(
        "url_template".into(),
        json!(
            "https://example.org/os?q={searchTerms}&fq=type%3Aarticle&sort=date+desc&count={count}"
        ),
    );

    let out = adapter
        .call(
            "https://example.org/osdd.xml",
            "search",
            json!({"query": "a b", "limit": 10}),
            &cx,
        )
        .await
        .unwrap();
    assert_eq!(out.items.len(), 1);
    let reqs = net.captured();
    assert_eq!(
        reqs[0].url,
        "https://example.org/os?q=a+b&fq=type%3Aarticle&sort=date+desc&count=10"
    );
}

#[tokio::test]
async fn sparql_trailing_comment_does_not_swallow_auto_limit() {
    // Regression test for I6.
    let net = CapturingNet::fixed(
        "application/sparql-results+json",
        r#"{"head":{"vars":["item"]},"results":{"bindings":[]}}"#,
    );
    let (reg, deps) = build_reg_with_net(net.clone());
    let adapter = reg.protocol("sparql").unwrap();
    let cx = default_cx(&deps);

    adapter
        .call(
            "https://sparql.example.org/query",
            "query",
            json!({"query": "SELECT ?item WHERE { ?item wdt:P31 wd:Q5 } # all humans"}),
            &cx,
        )
        .await
        .unwrap();

    // Also ensure a `# LIMIT 10` comment does not fool HAS_LIMIT_RE, while `<http://ex.org/#a>` is intact.
    adapter
        .call(
            "https://sparql.example.org/query",
            "query",
            json!({"query": "SELECT ?item WHERE { ?item <http://ex.org/#a> 1 } # LIMIT 10"}),
            &cx,
        )
        .await
        .unwrap();

    let reqs = net.captured();
    assert_eq!(reqs.len(), 2);
    let q0 = reqs[0]
        .query
        .iter()
        .find(|(k, _)| k == "query")
        .map(|(_, v)| v.as_str())
        .unwrap();
    assert_eq!(
        q0,
        "SELECT ?item WHERE { ?item wdt:P31 wd:Q5 } # all humans\nLIMIT 50"
    );
    let q1 = reqs[1]
        .query
        .iter()
        .find(|(k, _)| k == "query")
        .map(|(_, v)| v.as_str())
        .unwrap();
    assert_eq!(
        q1,
        "SELECT ?item WHERE { ?item <http://ex.org/#a> 1 } # LIMIT 10\nLIMIT 50"
    );
}

#[tokio::test]
async fn limit_above_max_clamps_instead_of_erroring() {
    // Regression test for I7.
    let mw_net = CapturingNet::fixed("application/json", r#"{"query":{"search":[]}}"#);
    let (reg_mw, deps_mw) = build_reg_with_net(mw_net.clone());
    let mw = reg_mw.protocol("mediawiki").unwrap();
    let cx_mw = default_cx(&deps_mw);

    mw.call(
        "https://en.wikipedia.org/w/api.php",
        "search",
        json!({"query": "rust", "limit": 100}),
        &cx_mw,
    )
    .await
    .unwrap();
    let u = CapturingNet::full_url(&mw_net.captured()[0]);
    let qp: BTreeMap<String, String> = u.query_pairs().into_owned().collect();
    assert_eq!(qp.get("srlimit").map(String::as_str), Some("50"));

    assert!(matches!(
        mw.call(
            "https://en.wikipedia.org/w/api.php",
            "search",
            json!({"query": "rust", "limit": 0}),
            &cx_mw,
        )
        .await,
        Err(FsmError::InvalidInput(_))
    ));

    let dv_net = CapturingNet::fixed(
        "application/json",
        r#"{"status":"OK","data":{"start":0,"total_count":0,"items":[]}}"#,
    );
    let (reg_dv, deps_dv) = build_reg_with_net(dv_net.clone());
    let dv = reg_dv.protocol("dataverse").unwrap();
    let cx_dv = default_cx(&deps_dv);

    dv.call(
        "https://dataverse.harvard.edu",
        "search",
        json!({"query": "climate", "limit": 100}),
        &cx_dv,
    )
    .await
    .unwrap();
    let u_dv = CapturingNet::full_url(&dv_net.captured()[0]);
    let qp_dv: BTreeMap<String, String> = u_dv.query_pairs().into_owned().collect();
    assert_eq!(qp_dv.get("per_page").map(String::as_str), Some("50"));
}

#[tokio::test]
async fn hit_adapters_omit_kind_so_academic_scope_projects_academic() {
    // Regression test for M1: adapters omit "kind" so InstanceSource (A1 §D.3) applies HitKind::Academic.
    let atom = r#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom"><entry><title>CiNii Paper</title><link rel="alternate" href="https://cir.nii.ac.jp/crid/1"/></entry></feed>"#;
    let os_net = CapturingNet::fixed("application/atom+xml", atom);
    let (reg_os, deps_os) = build_reg_with_net(os_net);
    let os = reg_os.protocol("opensearch").unwrap();
    let mut cx_os = default_cx(&deps_os);
    cx_os.dialect.insert(
        "url_template".into(),
        json!("https://cir.nii.ac.jp/opensearch/v2/all?q={searchTerms}&count={count}"),
    );

    let out_os = os
        .call(
            "https://cir.nii.ac.jp/opensearch/description.xml",
            "search",
            json!({"query": "neural"}),
            &cx_os,
        )
        .await
        .unwrap();
    let raw_item = &out_os.items[0];
    assert!(
        raw_item.get("kind").is_none(),
        "opensearch must not serialize kind: {raw_item}"
    );
    // Exact InstanceSource A1 §D.3 projection check (`crates/sources/src/instance_source.rs:126-145`):
    let has_kind = raw_item.get("kind").is_some_and(|k| !k.is_null());
    let mut projected: Hit = serde_json::from_value(raw_item.clone()).unwrap();
    if !has_kind {
        projected.kind = HitKind::Academic;
    }
    assert_eq!(projected.kind, HitKind::Academic);

    // Same check for mediawiki (e.g. academic-scoped wikisource/wikispecies).
    let mw_net = CapturingNet::fixed(
        "application/json",
        r#"{"query":{"search":[{"title":"Homo sapiens","snippet":"Species"}]}}"#,
    );
    let (reg_mw, deps_mw) = build_reg_with_net(mw_net);
    let mw = reg_mw.protocol("mediawiki").unwrap();
    let out_mw = mw
        .call(
            "https://species.wikimedia.org/w/api.php",
            "search",
            json!({"query": "Homo sapiens"}),
            &default_cx(&deps_mw),
        )
        .await
        .unwrap();
    assert!(out_mw.items[0].get("kind").is_none());
}

#[tokio::test]
async fn sru_cql_clause_detection_and_unquoted_template_quoting() {
    // Regression test for M2 (A1 erratum #3).
    let empty_sru = r#"<?xml version="1.0"?><zs:searchRetrieveResponse xmlns:zs="http://www.loc.gov/zing/srw/"><zs:version>1.1</zs:version><zs:numberOfRecords>0</zs:numberOfRecords><zs:records/></zs:searchRetrieveResponse>"#;
    let net = CapturingNet::fixed("application/xml", empty_sru);
    let (reg, deps) = build_reg_with_net(net.clone());
    let adapter = reg.protocol("sru").unwrap();

    // 1. "Attention is all you need" must be wrapped (not mistreated as CQL `all`).
    let mut cx_bnf = default_cx(&deps);
    cx_bnf
        .dialect
        .insert("query_template".into(), json!("bib.title all \"{query}\""));
    adapter
        .call(
            "https://catalogue.bnf.fr/api/SRU",
            "search",
            json!({"query": "Attention is all you need"}),
            &cx_bnf,
        )
        .await
        .unwrap();

    // 2. Unquoted template `dc.title={query}` quotes multi-word plain query.
    let mut cx_loc = default_cx(&deps);
    cx_loc
        .dialect
        .insert("query_template".into(), json!("dc.title={query}"));
    adapter
        .call(
            "https://lx2.loc.gov/sru/lcdb",
            "search",
            json!({"query": "machine learning"}),
            &cx_loc,
        )
        .await
        .unwrap();

    // 3. Real CQL clause passes verbatim.
    adapter
        .call(
            "https://lx2.loc.gov/sru/lcdb",
            "search",
            json!({"query": "dc.title=machine learning"}),
            &cx_loc,
        )
        .await
        .unwrap();

    let reqs = net.captured();
    let get_q = |idx: usize| -> String {
        CapturingNet::full_url(&reqs[idx])
            .query_pairs()
            .find(|(k, _)| k == "query")
            .map(|(_, v)| v.into_owned())
            .unwrap()
    };
    assert_eq!(get_q(0), r#"bib.title all "Attention is all you need""#);
    assert_eq!(get_q(1), r#"dc.title="machine learning""#);
    assert_eq!(get_q(2), "dc.title=machine learning");
}

#[tokio::test]
async fn upstream_integer_overflow_guard_never_panics() {
    // Regression test for M3: huge upstream integers in OpenSearch, DSpace7, Dataverse.
    // 1. OpenSearch OSDD indexOffset = u64::MAX with page = 2.
    let osdd = r#"<?xml version="1.0"?><OpenSearchDescription xmlns="http://a9.com/-/spec/opensearch/1.1/"><ShortName>Big</ShortName><Url type="application/atom+xml" indexOffset="18446744073709551615" pageOffset="18446744073709551615" template="https://example.org/search?q={searchTerms}&amp;start={startIndex?}&amp;count={count?}"/></OpenSearchDescription>"#;
    let atom = r#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom" xmlns:os="http://a9.com/-/spec/opensearch/1.1/"><os:totalResults>100</os:totalResults><os:startIndex>18446744073709551615</os:startIndex><entry><title>Item</title><link rel="alternate" href="https://example.org/1"/></entry></feed>"#;
    let os_net = CapturingNet::with_fn(move |req, idx| {
        let (ct, body) = if idx == 0 {
            ("application/opensearchdescription+xml", osdd)
        } else {
            ("application/atom+xml", atom)
        };
        Ok(HttpBytes::new(
            req.url.clone(),
            200,
            vec![("content-type".into(), ct.into())],
            body.as_bytes().to_vec(),
        ))
    });
    let (reg_os, deps_os) = build_reg_with_net(os_net);
    let mut cx_os = default_cx(&deps_os);
    cx_os.replay_mode = true;
    let os_out = reg_os
        .protocol("opensearch")
        .unwrap()
        .call(
            "https://example.org/osdd.xml",
            "search",
            json!({"query": "test", "page": 2}),
            &cx_os,
        )
        .await
        .unwrap();
    assert_eq!(os_out.items.len(), 1);

    // 2. DSpace 7 page.number = u64::MAX
    let ds_body = json!({
        "_embedded": {
            "searchResult": {
                "page": {"number": u64::MAX, "totalPages": u64::MAX},
                "_embedded": {"objects": []}
            }
        }
    })
    .to_string();
    let ds_net = CapturingNet::fixed("application/hal+json", ds_body);
    let (reg_ds, deps_ds) = build_reg_with_net(ds_net);
    let ds_out = reg_ds
        .protocol("dspace7")
        .unwrap()
        .call(
            "https://repo.example.edu",
            "search",
            json!({"query": "x"}),
            &default_cx(&deps_ds),
        )
        .await
        .unwrap();
    assert_eq!(ds_out.next_cursor, None);

    // 3. Dataverse data.start = u64::MAX
    let dv_body = json!({
        "status": "OK",
        "data": {
            "start": u64::MAX,
            "total_count": u64::MAX,
            "items": [{"name": "A", "url": "https://example.org/a"}]
        }
    })
    .to_string();
    let dv_net = CapturingNet::fixed("application/json", dv_body);
    let (reg_dv, deps_dv) = build_reg_with_net(dv_net);
    let dv_out = reg_dv
        .protocol("dataverse")
        .unwrap()
        .call(
            "https://dataverse.harvard.edu",
            "search",
            json!({"query": "x"}),
            &default_cx(&deps_dv),
        )
        .await
        .unwrap();
    assert_eq!(dv_out.items.len(), 1);
    assert_eq!(dv_out.next_cursor, None);
}

#[tokio::test]
async fn dspace7_skips_community_and_collection_objects() {
    // Regression test for M4.
    let body = json!({
        "_embedded": {
            "searchResult": {
                "_embedded": {
                    "objects": [
                        {
                            "_embedded": {
                                "indexableObject": {
                                    "type": "community",
                                    "name": "Faculty of Medicine",
                                    "handle": "123/1"
                                }
                            }
                        },
                        {
                            "_embedded": {
                                "indexableObject": {
                                    "type": "collection",
                                    "name": "Department Theses",
                                    "handle": "123/2"
                                }
                            }
                        },
                        {
                            "_embedded": {
                                "indexableObject": {
                                    "type": "item",
                                    "handle": "123/3",
                                    "metadata": {
                                        "dc.title": [{"value": "Clinical Trial Results"}]
                                    }
                                }
                            }
                        }
                    ]
                }
            }
        }
    })
    .to_string();
    let net = CapturingNet::fixed("application/hal+json", body);
    let (reg, deps) = build_reg_with_net(net);
    let out = reg
        .protocol("dspace7")
        .unwrap()
        .call(
            "https://repo.example.edu",
            "search",
            json!({"query": "medicine"}),
            &default_cx(&deps),
        )
        .await
        .unwrap();
    assert_eq!(out.items.len(), 1);
    let w: ScholarlyWork = serde_json::from_value(out.items[0].clone()).unwrap();
    assert_eq!(w.title, "Clinical Trial Results");
}

#[tokio::test]
async fn sparql_user_supplied_limit_not_truncated_to_default_50() {
    // Regression test for M5: user's LIMIT 500 returning 60 bindings must not be truncated to 50.
    let bindings: Vec<Value> = (0..60)
        .map(|i| json!({"s": {"type": "uri", "value": format!("http://example.org/{i}")}}))
        .collect();
    let body = json!({"head": {"vars": ["s"]}, "results": {"bindings": bindings}}).to_string();
    let net = CapturingNet::fixed("application/sparql-results+json", body);
    let (reg, deps) = build_reg_with_net(net);
    let out = reg
        .protocol("sparql")
        .unwrap()
        .call(
            "https://sparql.example.org/query",
            "query",
            json!({"query": "SELECT ?s WHERE { ?s a <http://ex.org/X> } LIMIT 500"}),
            &default_cx(&deps),
        )
        .await
        .unwrap();
    assert_eq!(out.items.len(), 60);
}

#[tokio::test]
async fn dialect_overridable_params_declare_no_hardcoded_default() {
    // Regression test for M6: metadata_prefix and dataverse.type must not shadow dialect defaults.
    let oai_net = CapturingNet::fixed(
        "text/xml",
        r#"<?xml version="1.0"?><OAI-PMH xmlns="http://www.openarchives.org/OAI/2.0/"><ListRecords/></OAI-PMH>"#,
    );
    let (reg_oai, deps_oai) = build_reg_with_net(oai_net.clone());
    let oai = reg_oai.protocol("oai_pmh").unwrap();
    for op_name in ["list_records", "get_record"] {
        let p = oai
            .ops()
            .iter()
            .find(|o| o.name == op_name)
            .unwrap()
            .param("metadata_prefix")
            .unwrap();
        assert_eq!(
            p.default, None,
            "oai_pmh.{op_name}.metadata_prefix must not declare hardcoded default"
        );
    }
    let mut cx_oai = default_cx(&deps_oai);
    cx_oai
        .dialect
        .insert("metadata_prefix".into(), json!("jpcoar_1.0"));
    oai.call(
        "https://repo.example/oai",
        "list_records",
        json!({}),
        &cx_oai,
    )
    .await
    .unwrap();
    let oai_q: BTreeMap<String, String> = CapturingNet::full_url(&oai_net.captured()[0])
        .query_pairs()
        .into_owned()
        .collect();
    assert_eq!(
        oai_q.get("metadataPrefix").map(String::as_str),
        Some("jpcoar_1.0")
    );

    let dv_net = CapturingNet::fixed(
        "application/json",
        r#"{"status":"OK","data":{"start":0,"total_count":0,"items":[]}}"#,
    );
    let (reg_dv, deps_dv) = build_reg_with_net(dv_net.clone());
    let dv = reg_dv.protocol("dataverse").unwrap();
    let dv_type = dv
        .ops()
        .iter()
        .find(|o| o.name == "search")
        .unwrap()
        .param("type")
        .unwrap();
    assert_eq!(dv_type.default, None);
    let mut cx_dv = default_cx(&deps_dv);
    cx_dv.dialect.insert("default_type".into(), json!("file"));
    dv.call(
        "https://dataverse.harvard.edu",
        "search",
        json!({"query": "climate"}),
        &cx_dv,
    )
    .await
    .unwrap();
    let dv_q: BTreeMap<String, String> = CapturingNet::full_url(&dv_net.captured()[0])
        .query_pairs()
        .into_owned()
        .collect();
    assert_eq!(dv_q.get("type").map(String::as_str), Some("file"));
}

#[tokio::test]
async fn dataverse_next_cursor_counts_skipped_raw_items() {
    // Regression test for M7: 3 raw items (1 untitled skipped, 2 valid) with start=0, total=10 -> cursor "3".
    let body = json!({
        "status": "OK",
        "data": {
            "start": 0,
            "total_count": 10,
            "items": [
                {"name": "", "url": "https://example.org/skip"},
                {"name": "Valid One", "url": "https://example.org/1"},
                {"name": "Valid Two", "url": "https://example.org/2"}
            ]
        }
    })
    .to_string();
    let net = CapturingNet::fixed("application/json", body);
    let (reg, deps) = build_reg_with_net(net);
    let out = reg
        .protocol("dataverse")
        .unwrap()
        .call(
            "https://dataverse.harvard.edu",
            "search",
            json!({"query": "x", "limit": 10}),
            &default_cx(&deps),
        )
        .await
        .unwrap();
    assert_eq!(out.items.len(), 2);
    assert_eq!(out.next_cursor.as_deref(), Some("3"));
}

#[tokio::test]
async fn sru_dc_catalog_dates_extract_year() {
    // Regression test for M8: catalogue-style DC dates "1994.", "[1996]", "c2014.", "1994-" yield years.
    let sru_xml = r#"<?xml version="1.0"?><zs:searchRetrieveResponse xmlns:zs="http://www.loc.gov/zing/srw/"><zs:version>1.1</zs:version><zs:numberOfRecords>4</zs:numberOfRecords><zs:records>
        <zs:record><zs:recordPacking>xml</zs:recordPacking><zs:recordData><srw_dc:dc xmlns:srw_dc="info:srw/schema/1/dc-schema" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>Book A</dc:title><dc:date>1994.</dc:date></srw_dc:dc></zs:recordData></zs:record>
        <zs:record><zs:recordPacking>xml</zs:recordPacking><zs:recordData><srw_dc:dc xmlns:srw_dc="info:srw/schema/1/dc-schema" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>Book B</dc:title><dc:date>[1996]</dc:date></srw_dc:dc></zs:recordData></zs:record>
        <zs:record><zs:recordPacking>xml</zs:recordPacking><zs:recordData><srw_dc:dc xmlns:srw_dc="info:srw/schema/1/dc-schema" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>Book C</dc:title><dc:date>c2014.</dc:date></srw_dc:dc></zs:recordData></zs:record>
        <zs:record><zs:recordPacking>xml</zs:recordPacking><zs:recordData><srw_dc:dc xmlns:srw_dc="info:srw/schema/1/dc-schema" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>Book D</dc:title><dc:date>1994-</dc:date></srw_dc:dc></zs:recordData></zs:record>
    </zs:records></zs:searchRetrieveResponse>"#;
    let net = CapturingNet::fixed("application/xml", sru_xml);
    let (reg, deps) = build_reg_with_net(net);
    let out = reg
        .protocol("sru")
        .unwrap()
        .call(
            "https://lx2.loc.gov/sru/lcdb",
            "search",
            json!({"query": "neural", "limit": 10}),
            &default_cx(&deps),
        )
        .await
        .unwrap();
    let years: Vec<Option<u16>> = out
        .items
        .into_iter()
        .map(|v| serde_json::from_value::<ScholarlyWork>(v).unwrap().year)
        .collect();
    assert_eq!(years, vec![Some(1994), Some(1996), Some(2014), Some(1994)]);
}

// ---------------------------------------------------------------------------
// 12. Review Round 2 regression tests (B1–B3, I1–I6, M1–M4; Rulings R2 & R3)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn xml_r3_rejects_entity_blowup_node_limit_and_oversized_text_or_attrs() {
    // Regression test for Round 2 B1 (Ruling R3).
    // 1. Single non-nested quadratic <!ENTITY> blowup (10KB x 500 = 5MB).
    let payload_10k = "A".repeat(10_000);
    let refs_500 = "&a;".repeat(500);
    let oai_bomb = format!(
        r#"<?xml version="1.0"?><!DOCTYPE OAI-PMH [<!ENTITY a "{payload_10k}">]><OAI-PMH xmlns="http://www.openarchives.org/OAI/2.0/"><Identify><repositoryName>{refs_500}</repositoryName></Identify></OAI-PMH>"#
    );
    let net_oai = CapturingNet::fixed("text/xml", oai_bomb);
    let (reg_oai, deps_oai) = build_reg_with_net(net_oai);
    let err_oai = reg_oai
        .protocol("oai_pmh")
        .unwrap()
        .call(
            "https://repo.example/oai",
            "identify",
            json!({}),
            &default_cx(&deps_oai),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err_oai, FsmError::Upstream { .. }),
        "oai_pmh entity bomb must fail Upstream: {err_oai:?}"
    );

    let sru_bomb = format!(
        r#"<?xml version="1.0"?><!DOCTYPE searchRetrieveResponse [<!ENTITY a "{payload_10k}">]><searchRetrieveResponse xmlns="http://www.loc.gov/zing/srw/"><records><record><recordData><dc><title>{refs_500}</title></dc></recordData></record></records></searchRetrieveResponse>"#
    );
    let net_sru = CapturingNet::fixed("application/xml", sru_bomb);
    let (reg_sru, deps_sru) = build_reg_with_net(net_sru);
    let err_sru = reg_sru
        .protocol("sru")
        .unwrap()
        .call(
            "https://sru.example/sru",
            "search",
            json!({"query": "x"}),
            &default_cx(&deps_sru),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err_sru, FsmError::Upstream { .. }),
        "sru entity bomb must fail Upstream: {err_sru:?}"
    );

    let osdd_bomb = format!(
        r#"<?xml version="1.0"?><!DOCTYPE OpenSearchDescription [<!ENTITY a "{payload_10k}">]><OpenSearchDescription xmlns="http://a9.com/-/spec/opensearch/1.1/"><ShortName>{refs_500}</ShortName></OpenSearchDescription>"#
    );
    let net_os = CapturingNet::fixed("application/opensearchdescription+xml", osdd_bomb);
    let (reg_os, deps_os) = build_reg_with_net(net_os);
    let err_os = reg_os
        .protocol("opensearch")
        .unwrap()
        .call(
            "https://ex.example/osdd.xml",
            "describe",
            json!({}),
            &default_cx(&deps_os),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err_os, FsmError::Upstream { .. }),
        "opensearch entity bomb must fail Upstream: {err_os:?}"
    );

    // 2. SRU recordPacking="string" double-escaped &lt;!ENTITY inside inner XML must error, not return Ok([]).
    let inner_escaped = format!(
        "&lt;?xml version=&quot;1.0&quot;?&gt;&lt;!DOCTYPE dc [&lt;!ENTITY a &quot;{payload_10k}&quot;&gt;]&gt;&lt;dc&gt;&lt;title&gt;{}&lt;/title&gt;&lt;/dc&gt;",
        "&amp;a;".repeat(500)
    );
    let sru_string_bomb = format!(
        r#"<?xml version="1.0"?><searchRetrieveResponse xmlns="http://www.loc.gov/zing/srw/"><records><record><recordPacking>string</recordPacking><recordData>{inner_escaped}</recordData></record></records></searchRetrieveResponse>"#
    );
    let net_str = CapturingNet::fixed("application/xml", sru_string_bomb);
    let (reg_str, deps_str) = build_reg_with_net(net_str);
    let err_str = reg_str
        .protocol("sru")
        .unwrap()
        .call(
            "https://sru.example/sru",
            "search",
            json!({"query": "x"}),
            &default_cx(&deps_str),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err_str, FsmError::Upstream { .. }),
        "sru string-packed inner entity bomb must fail Upstream: {err_str:?}"
    );

    // 3. Node-limit overflow (> 200_000 nodes).
    let many_nodes = format!(
        r#"<?xml version="1.0"?><OAI-PMH xmlns="http://www.openarchives.org/OAI/2.0/"><Identify>{}</Identify></OAI-PMH>"#,
        "<x/>".repeat(200_050)
    );
    let net_nodes = CapturingNet::fixed("text/xml", many_nodes);
    let (reg_nodes, deps_nodes) = build_reg_with_net(net_nodes);
    let err_nodes = reg_nodes
        .protocol("oai_pmh")
        .unwrap()
        .call(
            "https://repo.example/oai",
            "identify",
            json!({}),
            &default_cx(&deps_nodes),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err_nodes, FsmError::Upstream { .. }),
        "node limit overflow must fail Upstream: {err_nodes:?}"
    );

    // 4. Oversized text node (> 1_048_576 bytes).
    let big_text = "T".repeat(1_048_600);
    let text_doc = format!(
        r#"<?xml version="1.0"?><OAI-PMH xmlns="http://www.openarchives.org/OAI/2.0/"><Identify><repositoryName>{big_text}</repositoryName></Identify></OAI-PMH>"#
    );
    let net_text = CapturingNet::fixed("text/xml", text_doc);
    let (reg_text, deps_text) = build_reg_with_net(net_text);
    let err_text = reg_text
        .protocol("oai_pmh")
        .unwrap()
        .call(
            "https://repo.example/oai",
            "identify",
            json!({}),
            &default_cx(&deps_text),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err_text, FsmError::Upstream { .. }),
        "oversized XML text must fail Upstream: {err_text:?}"
    );

    // 5. Oversized attribute (> 65_536 bytes).
    let big_attr = "V".repeat(65_600);
    let attr_doc = format!(
        r#"<?xml version="1.0"?><OAI-PMH xmlns="http://www.openarchives.org/OAI/2.0/"><Identify extra="{big_attr}"><repositoryName>OK</repositoryName></Identify></OAI-PMH>"#
    );
    let net_attr = CapturingNet::fixed("text/xml", attr_doc);
    let (reg_attr, deps_attr) = build_reg_with_net(net_attr);
    let err_attr = reg_attr
        .protocol("oai_pmh")
        .unwrap()
        .call(
            "https://repo.example/oai",
            "identify",
            json!({}),
            &default_cx(&deps_attr),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err_attr, FsmError::Upstream { .. }),
        "oversized XML attribute must fail Upstream: {err_attr:?}"
    );
}

#[test]
fn sru_declares_record_schema_alias_for_instance_source_wiring() {
    // Regression test for Round 2 B2: SruAdapter::ops() must accept both record_schema and recordSchema.
    let net = Arc::new(ReplayNet::from_fixtures(vec![]).unwrap());
    let (reg, _) = build_reg(net);
    let sru = reg.protocol("sru").unwrap();
    let search_op = sru.ops().iter().find(|o| o.name == "search").unwrap();
    assert!(search_op.param("record_schema").is_some());
    assert!(search_op.param("recordSchema").is_some());

    let manifests = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("manifests/instances");
    for wave in ["w5e", "w5o"] {
        let text = std::fs::read_to_string(manifests.join(wave).join("sru.toml")).unwrap();
        let file = InstanceFileDecl::from_toml(&text).unwrap();
        for inst in &file.instance {
            for k in inst.default_params.keys() {
                assert!(
                    sru.ops().iter().any(|o| o.param(k).is_some()),
                    "{wave}/{}: default_params key {k:?} missing from sru ops",
                    inst.id
                );
            }
        }
    }
}

#[tokio::test]
async fn http_200_html_or_error_envelopes_return_upstream_error_r2() {
    // Regression test for Round 2 B3 (Ruling R2): 200 XHTML maintenance pages and JSON error envelopes must fail Upstream.
    let xhtml_maint = "<html><body><h1>Service Unavailable</h1></body></html>";

    // OAI-PMH list_sets, list_records, get_record
    let oai_net = CapturingNet::fixed("text/html; charset=utf-8", xhtml_maint);
    let (reg_oai, deps_oai) = build_reg_with_net(oai_net);
    let oai = reg_oai.protocol("oai_pmh").unwrap();
    let cx_oai = default_cx(&deps_oai);
    for (op, params) in [
        ("list_sets", json!({})),
        ("list_records", json!({})),
        ("get_record", json!({"identifier": "oai:ex:1"})),
    ] {
        let err = oai
            .call("https://repo.example/oai", op, params, &cx_oai)
            .await
            .unwrap_err();
        assert!(
            matches!(err, FsmError::Upstream { .. }),
            "oai_pmh.{op} returned non-Upstream on XHTML maintenance page: {err:?}"
        );
    }

    // SRU search & explain (must never fabricate an explain record)
    let sru_net = CapturingNet::fixed("text/html; charset=utf-8", xhtml_maint);
    let (reg_sru, deps_sru) = build_reg_with_net(sru_net);
    let sru = reg_sru.protocol("sru").unwrap();
    let cx_sru = default_cx(&deps_sru);
    for (op, params) in [("search", json!({"query": "x"})), ("explain", json!({}))] {
        let err = sru
            .call("https://sru.example/sru", op, params, &cx_sru)
            .await
            .unwrap_err();
        assert!(
            matches!(err, FsmError::Upstream { .. }),
            "sru.{op} returned non-Upstream on XHTML maintenance page: {err:?}"
        );
    }

    // OpenSearch XML feed with XHTML maintenance page
    let os_xml_net = CapturingNet::fixed("application/xhtml+xml", xhtml_maint);
    let (reg_os_xml, deps_os_xml) = build_reg_with_net(os_xml_net);
    let mut cx_os_xml = default_cx(&deps_os_xml);
    cx_os_xml.dialect.insert(
        "url_template".into(),
        json!("https://example.org/search?q={searchTerms}&count={count}"),
    );
    let err_os_xml = reg_os_xml
        .protocol("opensearch")
        .unwrap()
        .call(
            "https://example.org/osdd.xml",
            "search",
            json!({"query": "x"}),
            &cx_os_xml,
        )
        .await
        .unwrap_err();
    assert!(matches!(err_os_xml, FsmError::Upstream { .. }));

    // JSON error envelope across dspace7, inveniordm, dataverse, and opensearch JSON
    let json_err = r#"{"error":"maintenance","message":"down"}"#;

    let ds_net = CapturingNet::fixed("application/json", json_err);
    let (reg_ds, deps_ds) = build_reg_with_net(ds_net);
    let ds = reg_ds.protocol("dspace7").unwrap();
    let cx_ds = default_cx(&deps_ds);
    for (op, params) in [
        ("search", json!({"query": "x"})),
        ("item", json!({"id": "1111"})),
    ] {
        let err = ds
            .call("https://repo.example.edu", op, params, &cx_ds)
            .await
            .unwrap_err();
        assert!(
            matches!(err, FsmError::Upstream { .. }),
            "dspace7.{op}: {err:?}"
        );
    }

    let inv_net = CapturingNet::fixed("application/json", json_err);
    let (reg_inv, deps_inv) = build_reg_with_net(inv_net);
    let inv = reg_inv.protocol("inveniordm").unwrap();
    let cx_inv = default_cx(&deps_inv);
    for (op, params) in [
        ("search", json!({"query": "x"})),
        ("record", json!({"id": "123"})),
    ] {
        let err = inv
            .call("https://zenodo.org", op, params, &cx_inv)
            .await
            .unwrap_err();
        assert!(
            matches!(err, FsmError::Upstream { .. }),
            "inveniordm.{op}: {err:?}"
        );
    }

    let dv_net = CapturingNet::fixed("application/json", json_err);
    let (reg_dv, deps_dv) = build_reg_with_net(dv_net);
    let dv = reg_dv.protocol("dataverse").unwrap();
    let cx_dv = default_cx(&deps_dv);
    for (op, params) in [
        ("search", json!({"query": "x"})),
        ("dataset", json!({"persistent_id": "doi:10.7910/DVN/1"})),
    ] {
        let err = dv
            .call("https://dataverse.harvard.edu", op, params, &cx_dv)
            .await
            .unwrap_err();
        assert!(
            matches!(err, FsmError::Upstream { .. }),
            "dataverse.{op}: {err:?}"
        );
    }

    let os_json_net = CapturingNet::fixed("application/json", json_err);
    let (reg_os_json, deps_os_json) = build_reg_with_net(os_json_net);
    let mut cx_os_json = default_cx(&deps_os_json);
    cx_os_json.dialect.insert(
        "url_template".into(),
        json!("https://example.org/search?q={searchTerms}&count={count}&format=json"),
    );
    let err_os_json = reg_os_json
        .protocol("opensearch")
        .unwrap()
        .call(
            "https://example.org/osdd.xml",
            "search",
            json!({"query": "x"}),
            &cx_os_json,
        )
        .await
        .unwrap_err();
    assert!(matches!(err_os_json, FsmError::Upstream { .. }));
}

#[tokio::test]
async fn inveniordm_metadata_only_record_is_not_marked_oa() {
    // Regression test for Round 2 I1.
    let body = json!({
        "hits": {
            "total": 1,
            "hits": [{
                "links": {"self_html": "https://archive.nyu.edu/records/1"},
                "access": {"record": "public", "files": "public", "status": "metadata-only"},
                "files": {"enabled": false, "count": 0},
                "metadata": {
                    "title": "L2 Political Academic Voter File",
                    "publication_date": "2024-01-01",
                    "rights": [{"id": "prop-license"}]
                }
            }]
        }
    })
    .to_string();
    let net = CapturingNet::fixed("application/json", body);
    let (reg, deps) = build_reg_with_net(net);
    let out = reg
        .protocol("inveniordm")
        .unwrap()
        .call(
            "https://archive.nyu.edu",
            "search",
            json!({"query": "voter"}),
            &default_cx(&deps),
        )
        .await
        .unwrap();
    let w: ScholarlyWork = serde_json::from_value(out.items[0].clone()).unwrap();
    assert!(w.oa.is_none(), "metadata-only record must not be OA");
}

#[tokio::test]
async fn dspace7_unrestricted_cc_by_article_is_marked_oa() {
    // Regression test for Round 2 I2.
    let body = json!({
        "type": "item",
        "inArchive": true,
        "handle": "123/42",
        "metadata": {
            "dc.title": [{"value": "Unrestricted Open Access Study"}],
            "dc.rights": [{"value": "This is an open access article distributed under the Creative Commons Attribution 4.0 License, which permits unrestricted use, distribution, and reproduction."}],
            "dc.rights.uri": [{"value": "https://creativecommons.org/licenses/by/4.0/"}]
        }
    })
    .to_string();
    let net = CapturingNet::fixed("application/hal+json", body);
    let (reg, deps) = build_reg_with_net(net);
    let out = reg
        .protocol("dspace7")
        .unwrap()
        .call(
            "https://repo.example.edu",
            "item",
            json!({"id": "42"}),
            &default_cx(&deps),
        )
        .await
        .unwrap();
    let w: ScholarlyWork = serde_json::from_value(out.items[0].clone()).unwrap();
    assert_eq!(w.oa.as_ref().map(|o| o.status.as_str()), Some("green"));
}

#[tokio::test]
async fn eprints_detects_format_pdf_url_and_rejects_closed_openaccess_no() {
    // Regression test for Round 2 I3.
    let body = json!([
        {
            "eprintid": 100,
            "title": "Format PDF Paper",
            "uri": "https://ep.example/100/",
            "full_text_status": "public",
            "documents": [{
                "security": "public",
                "format": "application/pdf",
                "main": "paper.pdf",
                "files": [{"mime_type": "application/pdf", "uri": "https://ep.example/id/file/100"}]
            }]
        },
        {
            "eprintid": 101,
            "title": "Closed Access Paper",
            "uri": "https://ep.example/101/",
            "openaccess": "no",
            "full_text_status": "restricted"
        }
    ])
    .to_string();
    let net = CapturingNet::fixed("application/json", body);
    let (reg, deps) = build_reg_with_net(net);
    let out = reg
        .protocol("eprints")
        .unwrap()
        .call(
            "https://ep.example",
            "search",
            json!({"query": "pdf", "limit": 5}),
            &default_cx(&deps),
        )
        .await
        .unwrap();
    assert_eq!(out.items.len(), 2);
    let w0: ScholarlyWork = serde_json::from_value(out.items[0].clone()).unwrap();
    assert_eq!(
        w0.oa.as_ref().and_then(|o| o.pdf_url.as_deref()),
        Some("https://ep.example/id/file/100")
    );
    let w1: ScholarlyWork = serde_json::from_value(out.items[1].clone()).unwrap();
    assert!(
        w1.oa.is_none(),
        "openaccess=no must not produce OA, got {:?}",
        w1.oa
    );
}

#[tokio::test]
async fn mediawiki_imageinfo_skips_pages_without_imageinfo() {
    // Regression test for Round 2 I4: prop=imageinfo skips redirects/deleted files lacking imageinfo[0].
    let body = json!({
        "query": {
            "pages": {
                "1": {
                    "index": 1,
                    "title": "File:DeletedOrRedirect.jpg"
                },
                "2": {
                    "index": 2,
                    "title": "File:EmptyImageInfo.jpg",
                    "imageinfo": []
                },
                "3": {
                    "index": 3,
                    "title": "File:RealPhoto.jpg",
                    "imageinfo": [{
                        "url": "https://upload.wikimedia.org/wikipedia/commons/a/ab/RealPhoto.jpg",
                        "timestamp": "2025-05-01T00:00:00Z",
                        "width": 800,
                        "height": 600
                    }]
                }
            }
        }
    })
    .to_string();
    let net = CapturingNet::fixed("application/json", body);
    let (reg, deps) = build_reg_with_net(net);
    let out = reg
        .protocol("mediawiki")
        .unwrap()
        .call(
            "https://commons.wikimedia.org/w/api.php",
            "search",
            json!({"query": "photo", "namespace": 6, "prop": "imageinfo"}),
            &default_cx(&deps),
        )
        .await
        .unwrap();
    assert_eq!(out.items.len(), 1);
    let hit: Hit = serde_json::from_value(out.items[0].clone()).unwrap();
    assert_eq!(hit.title, "RealPhoto.jpg");
    assert_eq!(
        hit.url,
        "https://upload.wikimedia.org/wikipedia/commons/a/ab/RealPhoto.jpg"
    );
}

#[tokio::test]
async fn opensearch_language_path_template_blocks_injection() {
    // Regression test for Round 2 I5.
    let atom = r#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom"><entry><title>OK</title><link rel="alternate" href="https://example.org/1"/></entry></feed>"#;
    let net = CapturingNet::fixed("application/atom+xml", atom);
    let (reg, deps) = build_reg_with_net(net.clone());
    let adapter = reg.protocol("opensearch").unwrap();

    let mut cx_bad = default_cx(&deps);
    cx_bad.lang = Some("evil.com/pwn#".into());
    cx_bad.dialect.insert(
        "url_template".into(),
        json!("https://example.org/{language}/search?q={searchTerms}&count={count}"),
    );
    let err = adapter
        .call(
            "https://example.org/osdd.xml",
            "search",
            json!({"query": "x"}),
            &cx_bad,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, FsmError::InvalidInput(_)), "got {err:?}");
    assert!(
        net.captured().is_empty(),
        "unsafe lang must fail before sending HTTP"
    );

    let mut cx_ok = default_cx(&deps);
    cx_ok.lang = Some("en-US".into());
    cx_ok.dialect.insert(
        "url_template".into(),
        json!("https://example.org/{language}/search?q={searchTerms}&count={count}"),
    );
    adapter
        .call(
            "https://example.org/osdd.xml",
            "search",
            json!({"query": "x"}),
            &cx_ok,
        )
        .await
        .unwrap();
    assert_eq!(
        net.captured()[0].url,
        "https://example.org/en-US/search?q=x&count=10"
    );
}

#[tokio::test]
async fn sru_cql_wraps_natural_language_questions_and_escapes_trailing_backslash() {
    // Regression test for Round 2 I6.
    let empty_sru = r#"<?xml version="1.0"?><zs:searchRetrieveResponse xmlns:zs="http://www.loc.gov/zing/srw/"><zs:version>1.1</zs:version><zs:numberOfRecords>0</zs:numberOfRecords><zs:records/></zs:searchRetrieveResponse>"#;
    let net = CapturingNet::fixed("application/xml", empty_sru);
    let (reg, deps) = build_reg_with_net(net.clone());
    let adapter = reg.protocol("sru").unwrap();
    let mut cx = default_cx(&deps);
    cx.dialect
        .insert("query_template".into(), json!("bib.title all \"{query}\""));

    let cases = [
        (
            "Are all swans white",
            r#"bib.title all "Are all swans white""#,
        ),
        ("Is any AI safe", r#"bib.title all "Is any AI safe""#),
        ("Find exact match", r#"bib.title all "Find exact match""#),
        (r"trailing\", r#"bib.title all "trailing\\""#),
        (r#"title all "neural""#, r#"title all "neural""#),
        (r#"bib.title all "neural""#, r#"bib.title all "neural""#),
    ];

    for (input, _) in cases {
        adapter
            .call(
                "https://catalogue.bnf.fr/api/SRU",
                "search",
                json!({"query": input}),
                &cx,
            )
            .await
            .unwrap();
    }

    let reqs = net.captured();
    for (idx, (_, expected)) in cases.iter().enumerate() {
        let sent = CapturingNet::full_url(&reqs[idx])
            .query_pairs()
            .find(|(k, _)| k == "query")
            .map(|(_, v)| v.into_owned())
            .unwrap();
        assert_eq!(sent, *expected, "case {idx}");
    }
}

#[tokio::test]
async fn dc_bare_year_venue_and_icc_rights_not_misclassified() {
    // Regression test for Round 2 M1.
    let oai_xml = r#"<?xml version="1.0"?><OAI-PMH xmlns="http://www.openarchives.org/OAI/2.0/"><GetRecord><record><header><identifier>oai:ex:1</identifier></header><metadata><oai_dc:dc xmlns:oai_dc="http://www.openarchives.org/OAI/2.0/oai_dc/" xmlns:dc="http://purl.org/dc/elements/1.1/">
        <dc:title>ICC Trade Report</dc:title>
        <dc:source>2013</dc:source>
        <dc:publisher>International Press</dc:publisher>
        <dc:rights>Copyright ICC Paris - All rights reserved</dc:rights>
    </oai_dc:dc></metadata></record></GetRecord></OAI-PMH>"#;
    let net = CapturingNet::fixed("text/xml", oai_xml);
    let (reg, deps) = build_reg_with_net(net);
    let out = reg
        .protocol("oai_pmh")
        .unwrap()
        .call(
            "https://repo.example/oai",
            "get_record",
            json!({"identifier": "oai:ex:1"}),
            &default_cx(&deps),
        )
        .await
        .unwrap();
    let w: ScholarlyWork = serde_json::from_value(out.items[0].clone()).unwrap();
    assert_eq!(w.venue.as_deref(), Some("International Press"));
    assert!(
        w.oa.is_none(),
        "\"Copyright ICC Paris\" must not trigger CC OA: {:?}",
        w.oa
    );
}

#[tokio::test]
async fn sparql_string_literal_limit_does_not_suppress_auto_limit() {
    // Regression test for Round 2 M2.
    let net = CapturingNet::fixed(
        "application/sparql-results+json",
        r#"{"head":{"vars":["x"]},"results":{"bindings":[]}}"#,
    );
    let (reg, deps) = build_reg_with_net(net.clone());
    let q = r#"SELECT ?x WHERE { ?x <http://ex.org/label> "speed LIMIT 10" }"#;
    reg.protocol("sparql")
        .unwrap()
        .call(
            "https://sparql.example.org/query",
            "query",
            json!({"query": q}),
            &default_cx(&deps),
        )
        .await
        .unwrap();
    let sent = net.captured()[0]
        .query
        .iter()
        .find(|(k, _)| k == "query")
        .map(|(_, v)| v.as_str())
        .unwrap()
        .to_owned();
    assert_eq!(sent, format!("{q}\nLIMIT 50"));
}

#[tokio::test]
async fn mediawiki_myapi_php_article_url_not_corrupted() {
    // Regression test for Round 2 M3.
    let body = json!({"query":{"search":[{"title":"Page","snippet":"Body"}]}}).to_string();
    let net = CapturingNet::fixed("application/json", body);
    let (reg, deps) = build_reg_with_net(net);
    let out = reg
        .protocol("mediawiki")
        .unwrap()
        .call(
            "https://wiki.example.org/myapi.php",
            "search",
            json!({"query": "page"}),
            &default_cx(&deps),
        )
        .await
        .unwrap();
    let hit: Hit = serde_json::from_value(out.items[0].clone()).unwrap();
    assert_eq!(hit.url, "https://wiki.example.org/index.php?title=Page");
}

#[tokio::test]
async fn dataverse_fallback_url_percent_encodes_persistent_id() {
    // Regression test for Round 2 M4.
    let search_body = json!({
        "status": "OK",
        "data": {
            "start": 0,
            "total_count": 1,
            "items": [{
                "name": "Special ID Dataset",
                "global_id": "doi:10.7910/DVN/A&B#1"
            }]
        }
    })
    .to_string();
    let ds_body = json!({
        "status": "OK",
        "data": {
            "id": 99,
            "latestVersion": {
                "metadataBlocks": {
                    "citation": {
                        "fields": [{"typeName": "title", "value": "Special ID Dataset"}]
                    }
                }
            }
        }
    })
    .to_string();

    let net_s = CapturingNet::fixed("application/json", search_body);
    let (reg_s, deps_s) = build_reg_with_net(net_s);
    let out_s = reg_s
        .protocol("dataverse")
        .unwrap()
        .call(
            "https://dataverse.harvard.edu",
            "search",
            json!({"query": "special"}),
            &default_cx(&deps_s),
        )
        .await
        .unwrap();
    assert_eq!(
        out_s.items[0]["url"],
        "https://dataverse.harvard.edu/dataset.xhtml?persistentId=doi%3A10.7910%2FDVN%2FA%26B%231"
    );

    let net_d = CapturingNet::fixed("application/json", ds_body);
    let (reg_d, deps_d) = build_reg_with_net(net_d);
    let out_d = reg_d
        .protocol("dataverse")
        .unwrap()
        .call(
            "https://dataverse.harvard.edu",
            "dataset",
            json!({"persistent_id": "doi:10.7910/DVN/A&B#1"}),
            &default_cx(&deps_d),
        )
        .await
        .unwrap();
    assert_eq!(
        out_d.items[0]["url"],
        "https://dataverse.harvard.edu/dataset.xhtml?persistentId=doi%3A10.7910%2FDVN%2FA%26B%231"
    );
}
