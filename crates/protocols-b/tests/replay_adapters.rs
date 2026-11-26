//! Offline replay and edge-case tests for all 9 `fsm-protocols-b` adapters.
//!
//! Uses `fsm_core::fixture::ReplayNet` exclusively (zero network access).

use flate2::{Compression, write::GzEncoder};
use fsm_core::fixture::{Exchange, Fixture, FixtureRequest, FixtureResponse, ReplayNet};
use fsm_core::{
    Bytes, CancellationToken, Config, FsmError, Hit, HttpBytes, HttpRequest, NetClient, NetCx,
    NullStore, Registry, SourceDeps, SrcCx, TransportProfile,
};
use fsm_net::{HttpClient, StaticDns};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn fixture_path(wave: &str, id: &str, op: &str) -> PathBuf {
    workspace_root()
        .join("crates/manifests/fixtures")
        .join(wave)
        .join(id)
        .join(op)
        .join("default.json")
}

fn build_registry(net: Arc<ReplayNet>) -> Registry {
    let deps = SourceDeps::new(net, Arc::new(NullStore::new()), Arc::new(Config::default()));
    let mut reg = Registry::new();
    fsm_protocols_b::register(&mut reg, &deps).expect("register protocols-b");
    reg
}

fn default_cx() -> SrcCx {
    SrcCx::new(CancellationToken::new(), Duration::from_secs(5))
}

fn cx_with_dialect(pairs: &[(&str, Value)]) -> SrcCx {
    let mut cx = default_cx();
    for (k, v) in pairs {
        cx.dialect.insert((*k).to_owned(), v.clone());
    }
    cx
}

fn make_fixture(source: &str, op: &str, exchanges: Vec<Exchange>) -> Fixture {
    let mut f = Fixture::new(source, op, "2026-11-25T10:00:00Z");
    f.exchanges = exchanges;
    f
}

fn json_exchange(url: &str, body: Value) -> Exchange {
    let mut req = FixtureRequest::new("GET", url);
    req.headers_subset
        .insert("accept".into(), "application/json".into());
    let resp = FixtureResponse::from_bytes(
        200,
        vec![(
            "content-type".into(),
            "application/json; charset=utf-8".into(),
        )],
        body.to_string().as_bytes(),
    );
    Exchange::new(req, resp)
}

#[test]
fn register_exposes_all_nine_adapters_and_rejects_duplicates() {
    let net = Arc::new(ReplayNet::from_fixtures(vec![]).unwrap());
    let deps = SourceDeps::new(net, Arc::new(NullStore::new()), Arc::new(Config::default()));
    let mut reg = Registry::new();
    fsm_protocols_b::register(&mut reg, &deps).expect("first register");

    let expected = [
        ("ckan", &["search", "package"][..]),
        ("socrata", &["search", "rows"]),
        ("opendatasoft", &["search", "records"]),
        ("discourse", &["search", "topic", "latest"]),
        ("wordpress_rest", &["search", "post"]),
        ("rss_atom", &["search", "latest"]),
        ("activitypub_fediverse", &["search", "nodeinfo"]),
        ("shopify_json", &["search", "products"]),
        ("sitemap_llms", &["search", "list"]),
    ];

    for (kind, ops) in expected {
        let adapter = reg
            .protocol(kind)
            .unwrap_or_else(|| panic!("missing adapter {kind}"));
        assert_eq!(adapter.kind(), kind);
        let got_ops: Vec<&str> = adapter.ops().iter().map(|o| o.name.as_str()).collect();
        assert_eq!(got_ops, ops, "ops mismatch for {kind}");
    }

    assert!(fsm_protocols_b::register(&mut reg, &deps).is_err());
}

#[tokio::test]
async fn ckan_replays_all_w5f_and_w5p_fixtures() {
    let cases = [
        (
            "w5f",
            "open_canada",
            "https://open.canada.ca/data",
            "climate",
            3,
        ),
        ("w5f", "hdx_ocha", "https://data.humdata.org", "malaria", 2),
        (
            "w5f",
            "data_qld_au",
            "https://www.data.qld.gov.au",
            "water",
            2,
        ),
        (
            "w5f",
            "opendata_swiss",
            "https://ckan.opendata.swiss",
            "climate",
            2,
        ),
        ("w5f", "data_gov_ie", "https://data.gov.ie", "transport", 3),
        (
            "w5f",
            "data_egov_jp",
            "https://data.e-gov.go.jp/data",
            "人口",
            3,
        ),
        (
            "w5f",
            "energydata_info",
            "https://energydata.info",
            "solar",
            3,
        ),
        ("w5f", "data_ca_gov", "https://data.ca.gov", "air", 2),
        (
            "w5p",
            "data_nsw_au",
            "https://data.nsw.gov.au/data",
            "transport",
            3,
        ),
        (
            "w5p",
            "depositar_tw",
            "https://data.depositar.io",
            "climate",
            3,
        ),
        ("w5p", "datos_gob_cl", "https://datos.gob.cl", "salud", 3),
        (
            "w5p",
            "uruguay_datos",
            "https://catalogodatos.gub.uy",
            "salud",
            3,
        ),
        (
            "w5p",
            "toronto_open_data",
            "https://ckan0.cf.opendata.inter.prod-toronto.ca",
            "transit",
            3,
        ),
        (
            "w5p",
            "boston_open_data",
            "https://data.boston.gov",
            "housing",
            3,
        ),
        (
            "w5p",
            "munich_open_data",
            "https://opendata.muenchen.de",
            "verkehr",
            3,
        ),
        (
            "w5p",
            "open_africa_ckan",
            "https://open.africa",
            "health",
            3,
        ),
    ];

    for (wave, id, base_url, query, limit) in cases {
        let net = Arc::new(ReplayNet::from_file(fixture_path(wave, id, "search")).unwrap());
        let reg = build_registry(Arc::clone(&net));
        let adapter = reg.protocol("ckan").unwrap();
        let out = adapter
            .call(
                base_url,
                "search",
                json!({"query": query, "limit": limit}),
                &default_cx(),
            )
            .await
            .unwrap_or_else(|e| panic!("ckan {wave}/{id} failed: {e}"));

        assert_eq!(out.kind, "ckan");
        assert!(!out.items.is_empty(), "{wave}/{id} returned 0 items");
        assert!(out.items.len() <= limit);
        let expected_prefix = format!("{}/dataset/", base_url.trim_end_matches('/'));
        let mut seen_urls = HashSet::new();
        for item in &out.items {
            let title = item["title"].as_str().unwrap_or("");
            let url = item["url"].as_str().unwrap_or("");
            assert!(!title.is_empty(), "{wave}/{id} empty title: {item}");
            assert!(
                url.starts_with(&expected_prefix),
                "{wave}/{id} expected portal dataset url starting with {expected_prefix:?}, got {url:?}"
            );
            assert!(
                seen_urls.insert(url.to_owned()),
                "{wave}/{id} duplicate dataset url: {url:?}"
            );
            assert!(item["resources"].is_array());
        }
        // I1 regression checks on real portals whose upstream pkg.url pointed off-portal:
        if id == "munich_open_data" {
            assert_eq!(out.items.len(), 3);
            for item in &out.items {
                assert_eq!(
                    item["sourceUrl"].as_str(),
                    Some("https://www.mstatistik-muenchen.de/indikatorenatlas/export/export.php")
                );
            }
        }
        if id == "data_gov_ie" {
            assert_eq!(
                out.items[0]["url"],
                "https://data.gov.ie/dataset/community-transport1"
            );
            assert_eq!(
                out.items[0]["sourceUrl"],
                "https://data.smartdublin.ie/dataset/community-transport1"
            );
            assert_eq!(
                out.items[2]["url"],
                "https://data.gov.ie/dataset/eta06-transport-taxes"
            );
            assert_eq!(
                out.items[2]["sourceUrl"],
                "https://ws.cso.ie/public/api.restful/PxStat.Data.Cube_API.ReadDataset/ETA06/JSON-stat/2.0/en"
            );
        }
        assert!(net.unmatched().is_empty() && net.hits().len() == 1);
    }
}

#[tokio::test]
async fn ckan_package_show_and_error_envelope() {
    let f_ok = make_fixture(
        "ckan_pkg",
        "package",
        vec![json_exchange(
            "https://data.example.org/ckan/api/3/action/package_show?id=air-quality",
            json!({
                "success": true,
                "result": {
                    "id": "uuid-1",
                    "name": "air-quality",
                    "title": {"en": "Air Quality Index", "de": "Luftqualität"},
                    "notes": "<p>Hourly <b>PM2.5</b> measurements.</p>",
                    "metadata_modified": "2026-10-01T12:00:00Z",
                    "organization": {"name": "env", "title": {"en": "Env Agency"}},
                    "license_title": "CC-BY-4.0",
                    "resources": [{"id": "r1", "name": "CSV", "format": "CSV", "url": "https://data.example.org/a.csv"}]
                }
            }),
        )],
    );
    let f_err = make_fixture(
        "ckan_err",
        "package",
        vec![json_exchange(
            "https://data.example.org/api/3/action/package_show?id=missing",
            json!({"success": false, "error": {"message": "Not found"}}),
        )],
    );
    let net = Arc::new(ReplayNet::from_fixtures(vec![f_ok, f_err]).unwrap());
    let reg = build_registry(Arc::clone(&net));
    let adapter = reg.protocol("ckan").unwrap();

    let cx = cx_with_dialect(&[("api_prefix", json!("/ckan/api/3/action"))]);
    let out = adapter
        .call(
            "https://data.example.org",
            "package",
            json!({"id": "air-quality"}),
            &cx,
        )
        .await
        .unwrap();
    assert_eq!(out.items.len(), 1);
    assert_eq!(out.items[0]["title"], "Air Quality Index");
    assert_eq!(out.items[0]["snippet"], "Hourly PM2.5 measurements.");
    assert_eq!(out.items[0]["organization"], "Env Agency");
    assert_eq!(out.items[0]["date"], "2026-10-01");
    assert_eq!(
        out.items[0]["url"],
        "https://data.example.org/dataset/air-quality"
    );

    let err = adapter
        .call(
            "https://data.example.org",
            "package",
            json!({"id": "missing"}),
            &default_cx(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, FsmError::Upstream { .. }), "got {err:?}");
}

#[tokio::test]
async fn socrata_replays_all_w5f_and_w5p_fixtures_including_auto_domain_scoping() {
    let cases = [
        (
            "w5f",
            "socrata_global",
            "https://api.us.socrata.com",
            "health",
            3,
            false,
        ),
        (
            "w5f",
            "cdc_open_data",
            "https://data.cdc.gov",
            "influenza",
            3,
            true,
        ),
        (
            "w5f",
            "nyc_open_data",
            "https://data.cityofnewyork.us",
            "311",
            3,
            true,
        ),
        (
            "w5f",
            "chicago_open_data",
            "https://data.cityofchicago.org",
            "crime",
            3,
            true,
        ),
        (
            "w5f",
            "ny_state_data",
            "https://data.ny.gov",
            "energy",
            3,
            true,
        ),
        (
            "w5f",
            "texas_open_data",
            "https://data.texas.gov",
            "water",
            3,
            true,
        ),
        (
            "w5f",
            "calgary_open_data",
            "https://data.calgary.ca",
            "transit",
            3,
            true,
        ),
        (
            "w5p",
            "la_city_data",
            "https://data.lacity.org",
            "crime",
            3,
            true,
        ),
        (
            "w5p",
            "wa_state_data",
            "https://data.wa.gov",
            "energy",
            3,
            true,
        ),
        (
            "w5p",
            "maryland_open_data",
            "https://opendata.maryland.gov",
            "health",
            3,
            true,
        ),
        (
            "w5p",
            "colombia_datos",
            "https://www.datos.gov.co",
            "salud",
            3,
            true,
        ),
        (
            "w5p",
            "edmonton_open_data",
            "https://data.edmonton.ca",
            "transit",
            3,
            true,
        ),
    ];

    for (wave, id, base_url, query, limit, _scoped) in cases {
        let net = Arc::new(ReplayNet::from_file(fixture_path(wave, id, "search")).unwrap());
        let reg = build_registry(Arc::clone(&net));
        let adapter = reg.protocol("socrata").unwrap();
        // Deliberately omit `domains` in params: auto_scope_domain=true must infer host
        // for portals and omit for api.us.socrata.com!
        let out = adapter
            .call(
                base_url,
                "search",
                json!({"query": query, "limit": limit}),
                &default_cx(),
            )
            .await
            .unwrap_or_else(|e| panic!("socrata {wave}/{id} failed: {e}"));

        assert!(!out.items.is_empty(), "{wave}/{id} returned 0 items");
        assert!(out.items.len() <= limit);
        for item in &out.items {
            assert!(!item["id"].as_str().unwrap_or("").is_empty());
            assert!(!item["title"].as_str().unwrap_or("").is_empty());
            assert!(item["url"].as_str().unwrap_or("").starts_with("http"));
            assert!(item["columns"].is_array());
        }
        assert!(net.unmatched().is_empty() && net.hits().len() == 1);
    }
}

#[tokio::test]
async fn socrata_rows_op_and_traversal_guard() {
    let f = make_fixture(
        "socrata_rows",
        "rows",
        vec![json_exchange(
            "https://data.cdc.gov/resource/ynw2-4viq.json?$limit=5&$offset=10&$where=state='CA'&$q=flu",
            json!([{"state": "CA", "deaths": "42"}, {"state": "CA", "deaths": "17"}]),
        )],
    );
    let net = Arc::new(ReplayNet::from_fixtures(vec![f]).unwrap());
    let reg = build_registry(Arc::clone(&net));
    let adapter = reg.protocol("socrata").unwrap();

    let out = adapter
        .call(
            "https://data.cdc.gov",
            "rows",
            json!({"dataset_id": "ynw2-4viq", "limit": 5, "offset": 10, "where": "state='CA'", "q": "flu"}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(out.items.len(), 2);
    assert_eq!(out.items[0]["state"], "CA");

    let bad = adapter
        .call(
            "https://data.cdc.gov",
            "rows",
            json!({"dataset_id": "../secret"}),
            &default_cx(),
        )
        .await
        .unwrap_err();
    assert!(matches!(bad, FsmError::InvalidInput(_)));
}

#[tokio::test]
async fn opendatasoft_replays_all_w5f_and_w5p_fixtures() {
    let cases = [
        (
            "w5f",
            "ods_public",
            "https://public.opendatasoft.com",
            "population",
            2,
        ),
        (
            "w5f",
            "france_economie",
            "https://data.economie.gouv.fr",
            "energie",
            3,
        ),
        (
            "w5f",
            "france_esr",
            "https://data.enseignementsup-recherche.gouv.fr",
            "recherche",
            3,
        ),
        (
            "w5f",
            "france_odre",
            "https://odre.opendatasoft.com",
            "energie",
            3,
        ),
        (
            "w5f",
            "banque_de_france",
            "https://webstat.banque-france.fr",
            "taux",
            3,
        ),
        ("w5f", "sbb_swiss_rail", "https://data.sbb.ch", "bahnhof", 3),
        (
            "w5f",
            "vancouver_open_data",
            "https://opendata.vancouver.ca",
            "park",
            3,
        ),
        (
            "w5p",
            "paris_open_data",
            "https://opendata.paris.fr",
            "velo",
            3,
        ),
        (
            "w5p",
            "brussels_open_data",
            "https://opendata.brussels.be",
            "transport",
            3,
        ),
        ("w5p", "basel_open_data", "https://data.bs.ch", "verkehr", 3),
        (
            "w5p",
            "uk_power_networks",
            "https://ukpowernetworks.opendatasoft.com",
            "solar",
            3,
        ),
        ("w5p", "sncf_rail_fr", "https://data.sncf.com", "gare", 3),
        (
            "w5p",
            "leicester_uk_ods",
            "https://data.leicester.gov.uk",
            "air",
            3,
        ),
    ];

    for (wave, id, base_url, query, limit) in cases {
        let net = Arc::new(ReplayNet::from_file(fixture_path(wave, id, "search")).unwrap());
        let reg = build_registry(Arc::clone(&net));
        let adapter = reg.protocol("opendatasoft").unwrap();
        let out = adapter
            .call(
                base_url,
                "search",
                json!({"query": query, "limit": limit}),
                &default_cx(),
            )
            .await
            .unwrap_or_else(|e| panic!("ods {wave}/{id} failed: {e}"));

        assert!(!out.items.is_empty(), "{wave}/{id} returned 0 items");
        assert!(out.items.len() <= limit);
        for item in &out.items {
            assert!(!item["datasetId"].as_str().unwrap_or("").is_empty());
            assert!(!item["title"].as_str().unwrap_or("").is_empty());
            assert!(
                item["url"]
                    .as_str()
                    .unwrap_or("")
                    .contains("/explore/dataset/")
            );
            let snippet = item["snippet"].as_str().unwrap_or("");
            assert!(
                !snippet.contains("<span"),
                "{wave}/{id} unstripped HTML: {snippet}"
            );
        }
        assert!(net.unmatched().is_empty() && net.hits().len() == 1);
    }
}

#[tokio::test]
async fn opendatasoft_records_op_and_query_escaping() {
    let f = make_fixture(
        "ods_rec",
        "records",
        vec![json_exchange(
            "https://public.opendatasoft.com/api/explore/v2.1/catalog/datasets/my-ds/records?limit=2&where=search%28%2A%2C%22foo+%5C%22bar%5C%22%22%29",
            json!({"total_count": 5, "results": [{"city": "Paris"}, {"city": "Lyon"}]}),
        )],
    );
    let net = Arc::new(ReplayNet::from_fixtures(vec![f]).unwrap());
    let reg = build_registry(Arc::clone(&net));
    let adapter = reg.protocol("opendatasoft").unwrap();

    let out = adapter
        .call(
            "https://public.opendatasoft.com",
            "records",
            json!({"dataset_id": "my-ds", "query": "foo \"bar\"", "limit": 2}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(out.items.len(), 2);
    assert_eq!(out.next_cursor.as_deref(), Some("2"));
}

#[tokio::test]
async fn discourse_replays_all_w5g_fixtures_plus_topic_and_latest() {
    let cases = [
        (
            "rust_users",
            "https://users.rust-lang.org",
            "tokio-console deadlock",
            3,
        ),
        (
            "python_discuss",
            "https://discuss.python.org",
            "PEP 703 nogil",
            3,
        ),
        (
            "julia_discourse",
            "https://discourse.julialang.org",
            "Enzyme.jl reverse mode shadow",
            3,
        ),
        (
            "pytorch_discuss",
            "https://discuss.pytorch.org",
            "flex_attention compile",
            3,
        ),
        (
            "llvm_discourse",
            "https://discourse.llvm.org",
            "TableGen OpInterface verify",
            3,
        ),
    ];

    for (id, base_url, query, limit) in cases {
        let net = Arc::new(ReplayNet::from_file(fixture_path("w5g", id, "search")).unwrap());
        let reg = build_registry(Arc::clone(&net));
        let adapter = reg.protocol("discourse").unwrap();
        let out = adapter
            .call(
                base_url,
                "search",
                json!({"query": query, "limit": limit}),
                &default_cx(),
            )
            .await
            .unwrap_or_else(|e| panic!("discourse {id} failed: {e}"));

        assert!(
            out.items.len() >= 2 && out.items.len() <= limit,
            "{id} len={}",
            out.items.len()
        );
        for raw in &out.items {
            assert!(
                raw.get("kind").is_none(),
                "{id} Hits item must omit kind for A1 §D.3 injection: {raw}"
            );
            let hit: Hit = serde_json::from_value(raw.clone()).expect("valid Hit");
            assert!(!hit.title.is_empty(), "{id} empty title");
            assert!(
                hit.url.starts_with(base_url) && hit.url.contains("/t/"),
                "{id} bad url: {}",
                hit.url
            );
            assert!(hit.date.is_some(), "{id} missing date");
        }
        assert!(net.unmatched().is_empty() && net.hits().len() == 1);
    }

    // Topic & latest with dialect.base_path
    let f_topic = make_fixture(
        "disc_topic",
        "topic",
        vec![json_exchange(
            "https://example.org/forum/t/42.json",
            json!({
                "id": 42,
                "title": "Async &amp; Pinning",
                "slug": "async-pinning",
                "posts_count": 2,
                "views": 100,
                "created_at": "2026-10-10T01:02:03Z",
                "post_stream": {"posts": [{"id": 1, "post_number": 1, "username": "alice", "cooked": "<p>Hello <code>Pin</code></p>", "created_at": "2026-10-10T01:02:03Z"}]}
            }),
        )],
    );
    let f_latest = make_fixture(
        "disc_latest",
        "latest",
        vec![json_exchange(
            "https://example.org/forum/c/help.json",
            json!({
                "topic_list": {"topics": [{"id": 99, "title": "Need help", "slug": "need-help", "last_posted_at": "2026-11-01T00:00:00Z", "excerpt": "<b>Question</b>"}]}
            }),
        )],
    );
    let net = Arc::new(ReplayNet::from_fixtures(vec![f_topic, f_latest]).unwrap());
    let reg = build_registry(Arc::clone(&net));
    let adapter = reg.protocol("discourse").unwrap();
    let cx = cx_with_dialect(&[("base_path", json!("/forum"))]);

    let t_out = adapter
        .call("https://example.org", "topic", json!({"id": 42}), &cx)
        .await
        .unwrap();
    assert_eq!(t_out.items[0]["title"], "Async & Pinning");
    assert_eq!(
        t_out.items[0]["url"],
        "https://example.org/forum/t/async-pinning/42"
    );
    assert_eq!(t_out.items[0]["posts"][0]["text"], "Hello Pin");

    let l_out = adapter
        .call(
            "https://example.org",
            "latest",
            json!({"limit": 5, "category": "help"}),
            &cx,
        )
        .await
        .unwrap();
    assert_eq!(l_out.items.len(), 1);
    assert!(l_out.items[0].get("kind").is_none());
    let hit: Hit = serde_json::from_value(l_out.items[0].clone()).unwrap();
    assert_eq!(hit.title, "Need help");
    assert_eq!(hit.snippet, "Question");
}

#[tokio::test]
async fn wordpress_rest_replays_all_w5g_and_w5p_fixtures_and_post_op() {
    let cases = [
        (
            "w5g",
            "quantamagazine",
            "https://www.quantamagazine.org",
            "quantum",
            3,
        ),
        (
            "w5g",
            "harvard_gazette",
            "https://news.harvard.edu/gazette",
            "climate",
            3,
        ),
        ("w5g", "github_blog", "https://github.blog", "security", 3),
        (
            "w5g",
            "schneier_security",
            "https://www.schneier.com",
            "cryptography",
            3,
        ),
        (
            "w5g",
            "mozilla_blog",
            "https://blog.mozilla.org",
            "privacy",
            3,
        ),
        (
            "w5p",
            "cncf_blog_wp",
            "https://www.cncf.io",
            "kubernetes",
            3,
        ),
        (
            "w5p",
            "isoc_blog_wp",
            "https://www.internetsociety.org",
            "routing",
            3,
        ),
        (
            "w5p",
            "oasis_open_wp",
            "https://www.oasis-open.org",
            "standard",
            3,
        ),
        (
            "w5p",
            "fnal_news_wp",
            "https://news.fnal.gov",
            "neutrino",
            3,
        ),
        (
            "w5p",
            "embl_news_wp",
            "https://www.embl.org/news",
            "genome",
            3,
        ),
    ];

    for (wave, id, base_url, query, limit) in cases {
        let net = Arc::new(ReplayNet::from_file(fixture_path(wave, id, "search")).unwrap());
        let reg = build_registry(Arc::clone(&net));
        let adapter = reg.protocol("wordpress_rest").unwrap();
        let out = adapter
            .call(
                base_url,
                "search",
                json!({"query": query, "limit": limit}),
                &default_cx(),
            )
            .await
            .unwrap_or_else(|e| panic!("wp {wave}/{id} failed: {e}"));

        assert!(out.items.len() >= 2 && out.items.len() <= limit);
        for raw in &out.items {
            assert!(
                raw.get("kind").is_none(),
                "{wave}/{id} Hits item must omit kind: {raw}"
            );
            let hit: Hit = serde_json::from_value(raw.clone()).expect("valid Hit");
            assert!(!hit.title.is_empty());
            assert!(
                !hit.title.contains("&#8211;"),
                "{wave}/{id} unescaped entity: {}",
                hit.title
            );
            assert!(hit.url.starts_with("http"));
            // M2: WP /search returns no excerpt; snippet must be empty (not "post").
            assert!(
                hit.snippet.is_empty(),
                "{wave}/{id} expected empty WP /search snippet, got {:?}",
                hit.snippet
            );
        }
        if id == "oasis_open_wp" {
            let title = out.items[0]["title"].as_str().unwrap();
            assert!(title.contains('\u{2013}'), "expected en-dash in {title:?}");
        }
        assert!(net.unmatched().is_empty() && net.hits().len() == 1);
    }

    // post op
    let f_post = make_fixture(
        "wp_post",
        "post",
        vec![json_exchange(
            "https://blog.example.org/wp-json/wp/v2/posts/123",
            json!({
                "id": 123,
                "slug": "hello-world",
                "status": "publish",
                "date_gmt": "2026-11-12T08:00:00",
                "link": "https://blog.example.org/hello-world/",
                "title": {"rendered": "Hello &#8212; World"},
                "excerpt": {"rendered": "<p>Short <em>summary</em>.</p>"},
                "content": {"rendered": "<p>Full article body.</p>"}
            }),
        )],
    );
    let net = Arc::new(ReplayNet::from_fixtures(vec![f_post]).unwrap());
    let reg = build_registry(Arc::clone(&net));
    let out = reg
        .protocol("wordpress_rest")
        .unwrap()
        .call(
            "https://blog.example.org",
            "post",
            json!({"id": 123}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(out.items[0]["title"], "Hello \u{2014} World");
    assert_eq!(out.items[0]["date"], "2026-11-12");
    assert_eq!(out.items[0]["snippet"], "Short summary.");
}

#[tokio::test]
async fn rss_atom_replays_all_w5g_fixtures_verbatim() {
    let w5g_cases = [
        ("nber_papers_rss", "https://back.nber.org/rss/new.xml"),
        ("w3c_news_rss", "https://www.w3.org/news/feed/"),
        (
            "nist_news_rss",
            "https://www.nist.gov/news-events/news/rss.xml",
        ),
        (
            "fed_press_rss",
            "https://www.federalreserve.gov/feeds/press_all.xml",
        ),
        (
            "kernel_releases_rss",
            "https://www.kernel.org/feeds/kdist.xml",
        ),
        (
            "bbc_world_rss",
            "https://feeds.bbci.co.uk/news/world/rss.xml",
        ),
        (
            "eli_bendersky_atom",
            "https://eli.thegreenplace.net/feeds/all.atom.xml",
        ),
    ];

    for (id, base_url) in w5g_cases {
        let net = Arc::new(ReplayNet::from_file(fixture_path("w5g", id, "search")).unwrap());
        let reg = build_registry(Arc::clone(&net));
        let adapter = reg.protocol("rss_atom").unwrap();

        // 1. search without query (probe shape)
        let out = adapter
            .call(base_url, "search", json!({"limit": 5}), &default_cx())
            .await
            .unwrap_or_else(|e| panic!("rss_atom w5g/{id} failed: {e}"));
        assert!(
            out.items.len() >= 2 && out.items.len() <= 5,
            "{id} len={}",
            out.items.len()
        );
        for raw in &out.items {
            assert!(
                raw.get("kind").is_none(),
                "rss_atom {id} Hits item must omit kind: {raw}"
            );
        }
        let first_hit: Hit = serde_json::from_value(out.items[0].clone()).unwrap();
        assert!(!first_hit.title.is_empty());
        assert!(first_hit.url.starts_with("http"));
        if id == "eli_bendersky_atom" {
            assert_eq!(
                first_hit.title,
                "Notes on the discrete Fourier transform (DFT)"
            );
            assert_eq!(
                first_hit.url,
                "https://eli.thegreenplace.net/2026/notes-on-the-discrete-fourier-transform-dft/"
            );
            assert_eq!(first_hit.date.as_deref(), Some("2026-11-21"));
        }

        // 2. latest op
        let latest = adapter
            .call(base_url, "latest", json!({"limit": 2}), &default_cx())
            .await
            .unwrap();
        assert_eq!(latest.items.len(), 2);

        // 3. search WITH keyword taken from first item's title
        let kw = first_hit
            .title
            .split(|c: char| !c.is_alphanumeric())
            .find(|w| w.len() >= 4)
            .unwrap_or("the");
        let filtered = adapter
            .call(
                base_url,
                "search",
                json!({"query": kw, "limit": 5}),
                &default_cx(),
            )
            .await
            .unwrap();
        assert!(
            !filtered.items.is_empty(),
            "{id} query={kw:?} returned 0 items"
        );
        assert!(net.unmatched().is_empty());
    }

    // Format mismatch check: Atom feed rejected when dialect.format = "rss"
    let net = Arc::new(
        ReplayNet::from_file(fixture_path("w5g", "eli_bendersky_atom", "search")).unwrap(),
    );
    let reg = build_registry(Arc::clone(&net));
    let mut cx = cx_with_dialect(&[("format", json!("atom"))]);
    assert!(
        reg.protocol("rss_atom")
            .unwrap()
            .call(
                "https://eli.thegreenplace.net/feeds/all.atom.xml",
                "search",
                json!({"limit": 2}),
                &cx
            )
            .await
            .is_ok()
    );
    // Feed with rss dialect rejects atom root
    let raw = std::fs::read_to_string(fixture_path("w5g", "eli_bendersky_atom", "search")).unwrap();
    let mut fx = Fixture::from_json(&raw).unwrap();
    fx.exchanges[0].request.headers_subset.clear();
    let net2 = Arc::new(ReplayNet::from_fixtures(vec![fx]).unwrap());
    let reg2 = build_registry(net2);
    cx.dialect.insert("format".into(), json!("rss"));
    let err = reg2
        .protocol("rss_atom")
        .unwrap()
        .call(
            "https://eli.thegreenplace.net/feeds/all.atom.xml",
            "search",
            json!({"limit": 2}),
            &cx,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, FsmError::Upstream { .. }));
}

/// M8: Verifies verbatim replay and XML body parsing across all 12 `w5p` RSS/Atom fixtures
/// using canonical A1 §E #15 `Accept` headers.
#[tokio::test]
async fn rss_atom_w5p_xml_body_parse_pending_fixture_rerecord() {
    let w5p_ids = [
        (
            "unicode_blog_rss",
            "https://blog.unicode.org/feeds/posts/summary?max-results=10",
        ),
        (
            "linux_foundation_rss",
            "https://www.linuxfoundation.org/blog/rss.xml",
        ),
        (
            "pyfound_blog_rss",
            "https://pyfound.blogspot.com/feeds/posts/summary?max-results=10",
        ),
        (
            "esa_science_rss",
            "https://www.esa.int/rss/latest-news-homepage.rss",
        ),
        ("mpg_research_rss", "https://www.mpg.de/en/research.rss"),
        ("cnrs_news_rss", "https://news.cnrs.fr/rss"),
        ("nsf_news_rss", "https://www.nsf.gov/rss/rss_www_news.xml"),
        (
            "fda_press_rss",
            "https://www.fda.gov/about-fda/contact-fda/stay-informed/rss-feeds/press-releases/rss.xml",
        ),
        ("ema_news_rss", "https://www.ema.europa.eu/en/news.xml"),
        (
            "ecdc_news_rss",
            "https://www.ecdc.europa.eu/en/taxonomy/term/1310/feed",
        ),
        ("yale_news_rss", "https://news.yale.edu/news-rss"),
        ("princeton_news_rss", "https://www.princeton.edu/feed"),
    ];
    for (id, base_url) in w5p_ids {
        let net = Arc::new(ReplayNet::from_file(fixture_path("w5p", id, "search")).unwrap());
        let reg = build_registry(Arc::clone(&net));
        let out = reg
            .protocol("rss_atom")
            .unwrap()
            .call(base_url, "search", json!({"limit": 5}), &default_cx())
            .await
            .unwrap_or_else(|e| panic!("w5p/{id} failed: {e}"));
        assert!(
            out.items.len() >= 2,
            "w5p/{id} got {} items",
            out.items.len()
        );
        assert!(out.items[0].get("kind").is_none());
        assert!(net.unmatched().is_empty() && net.hits().len() == 1);
    }
}

#[tokio::test]
async fn activitypub_replays_all_w5g_fixtures_plus_auto_nodeinfo_and_peertube() {
    let w5g_cases = [
        ("lemmy_world", "https://lemmy.world", "lemmy", "rust", 3),
        ("lemmy_ml", "https://lemmy.ml", "lemmy", "linux", 3),
        (
            "mastodon_social",
            "https://mastodon.social",
            "mastodon",
            "#rust",
            3,
        ),
        ("hachyderm", "https://hachyderm.io", "mastodon", "rust", 3),
        (
            "fediscience",
            "https://fediscience.org",
            "mastodon",
            "science",
            3,
        ),
    ];

    for (id, base_url, flavor, query, limit) in w5g_cases {
        let net = Arc::new(ReplayNet::from_file(fixture_path("w5g", id, "search")).unwrap());
        let reg = build_registry(Arc::clone(&net));
        let adapter = reg.protocol("activitypub_fediverse").unwrap();
        let cx = cx_with_dialect(&[("flavor", json!(flavor))]);
        let out = adapter
            .call(
                base_url,
                "search",
                json!({"query": query, "limit": limit}),
                &cx,
            )
            .await
            .unwrap_or_else(|e| panic!("activitypub {id} failed: {e}"));

        assert!(out.items.len() >= 2 && out.items.len() <= limit);
        for raw in &out.items {
            assert!(
                raw.get("kind").is_none(),
                "activitypub {id} Hits item must omit kind: {raw}"
            );
            let hit: Hit = serde_json::from_value(raw.clone()).expect("valid Hit");
            assert!(!hit.title.is_empty(), "{id} empty title");
            assert!(hit.url.starts_with("http"), "{id} bad url: {}", hit.url);
            assert!(hit.date.is_some());
        }
        // M3: Lemmy link posts must point to the Fediverse forum post (ap_id / /post/{id}),
        // carrying the external article URL in extra.linkUrl.
        if id == "lemmy_world" {
            assert_eq!(out.items[0]["url"], "https://jlai.lu/post/46639822");
            assert_eq!(
                out.items[1]["url"],
                "https://kbin.earth/m/health@lemmy.world/t/3413664"
            );
            assert!(
                out.items[1]["extra"]["linkUrl"]
                    .as_str()
                    .is_some_and(|u| u.contains("sciencealert.com")),
                "expected sciencealert linkUrl in {}",
                out.items[1]["extra"]
            );
            assert_eq!(
                out.items[2]["url"],
                "https://news.abolish.capital/post/95254"
            );
            assert!(
                out.items[2]["extra"]["linkUrl"]
                    .as_str()
                    .is_some_and(|u| u.contains("news.mongabay.com")),
                "expected mongabay linkUrl in {}",
                out.items[2]["extra"]
            );
        }
        assert!(net.unmatched().is_empty() && net.hits().len() == 1);
    }

    // 3-exchange auto NodeInfo -> PeerTube search + nodeinfo op + cache check
    let f_auto = make_fixture(
        "peertube_auto",
        "search",
        vec![
            json_exchange(
                "https://framatube.org/.well-known/nodeinfo",
                json!({"links": [{"rel": "http://nodeinfo.diaspora.software/ns/schema/2.1", "href": "https://framatube.org/nodeinfo/2.1"}]}),
            ),
            json_exchange(
                "https://framatube.org/nodeinfo/2.1",
                json!({"version": "2.1", "software": {"name": "peertube", "version": "6.3.0"}, "protocols": ["activitypub"], "openRegistrations": false}),
            ),
            json_exchange(
                "https://framatube.org/api/v1/search/videos?search=rust&count=2",
                json!({"total": 1, "data": [{"uuid": "u1", "name": "Rust Conf", "url": "https://framatube.org/w/u1", "description": "Talk", "publishedAt": "2026-09-01T00:00:00Z"}]}),
            ),
        ],
    );
    let net = Arc::new(ReplayNet::from_fixtures(vec![f_auto]).unwrap());
    let reg = build_registry(Arc::clone(&net));
    let adapter = reg.protocol("activitypub_fediverse").unwrap();

    // First auto call performs 3 exchanges and caches nodeinfo
    let out1 = adapter
        .call(
            "https://framatube.org",
            "search",
            json!({"query": "rust", "limit": 2}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(out1.items.len(), 1);
    assert_eq!(out1.items[0]["title"], "Rust Conf");
    assert_eq!(net.hits().len(), 3);

    // Second call (replay_mode=false) reuses cached flavor -> only 1 additional exchange!
    let out2 = adapter
        .call(
            "https://framatube.org",
            "search",
            json!({"query": "rust", "limit": 2}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(out2.items.len(), 1);
    assert_eq!(net.hits().len(), 4);

    // Third call with replay_mode=true bypasses cache -> 3 additional exchanges!
    let mut replay_cx = default_cx();
    replay_cx.replay_mode = true;
    let _ = adapter
        .call(
            "https://framatube.org",
            "search",
            json!({"query": "rust", "limit": 2}),
            &replay_cx,
        )
        .await
        .unwrap();
    assert_eq!(net.hits().len(), 7);

    // Explicit nodeinfo op
    let ni = adapter
        .call(
            "https://framatube.org",
            "nodeinfo",
            json!({}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(ni.items[0]["software"], "peertube");
    assert_eq!(ni.items[0]["version"], "6.3.0");
}

#[tokio::test]
async fn shopify_json_replays_all_w5g_fixtures_and_products_op() {
    let cases = [
        ("arduino_store", "https://store-usa.arduino.cc", "uno", 3),
        ("pimoroni", "https://shop.pimoroni.com", "pico", 3),
        ("m5stack", "https://shop.m5stack.com", "esp32", 3),
    ];

    for (id, base_url, query, limit) in cases {
        let net = Arc::new(ReplayNet::from_file(fixture_path("w5g", id, "search")).unwrap());
        let reg = build_registry(Arc::clone(&net));
        let adapter = reg.protocol("shopify_json").unwrap();
        let cx = cx_with_dialect(&[("currency", json!("USD"))]);
        let out = adapter
            .call(
                base_url,
                "search",
                json!({"query": query, "limit": limit}),
                &cx,
            )
            .await
            .unwrap_or_else(|e| panic!("shopify {id} failed: {e}"));

        assert!(out.items.len() >= 2 && out.items.len() <= limit);
        for item in &out.items {
            assert!(!item["title"].as_str().unwrap_or("").is_empty());
            let url = item["url"].as_str().unwrap_or("");
            assert!(
                url.starts_with(base_url),
                "{id} relative url not resolved: {url}"
            );
            assert_eq!(item["currency"], "USD");
            let snippet = item["snippet"].as_str().unwrap_or("");
            assert!(
                !snippet.contains(".product-description") && !snippet.contains("<style"),
                "{id} leaked CSS into snippet: {snippet}"
            );
        }
        assert!(net.unmatched().is_empty() && net.hits().len() == 1);
    }

    // products op
    let f = make_fixture(
        "shop_prods",
        "products",
        vec![json_exchange(
            "https://shop.example.com/products.json?limit=2&page=1",
            json!({
                "products": [
                    {"id": 1, "title": "Board A", "handle": "board-a", "vendor": "ACME", "product_type": "MCU", "body_html": "<p>Great board</p>", "updated_at": "2026-10-01T00:00:00Z", "variants": [{"price": "19.99"}], "images": [{"src": "https://cdn.example.com/a.jpg"}]},
                    {"id": 2, "title": "Board B", "handle": "board-b", "vendor": "ACME", "product_type": "MCU", "body_html": "<p>Better board</p>", "updated_at": "2026-10-02T00:00:00Z", "variants": [{"price": "29.99"}], "images": []}
                ]
            }),
        )],
    );
    let net = Arc::new(ReplayNet::from_fixtures(vec![f]).unwrap());
    let reg = build_registry(Arc::clone(&net));
    let out = reg
        .protocol("shopify_json")
        .unwrap()
        .call(
            "https://shop.example.com",
            "products",
            json!({"limit": 2, "page": 1}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(out.items.len(), 2);
    assert_eq!(
        out.items[0]["url"],
        "https://shop.example.com/products/board-a"
    );
    assert_eq!(out.items[0]["price"], "19.99");
    assert_eq!(out.next_cursor.as_deref(), Some("2"));
}

#[tokio::test]
async fn sitemap_llms_replays_all_w5g_and_w5p_fixtures_for_search_and_list() {
    let cases = [
        (
            "w5g",
            "owasp_cheatsheets",
            "https://cheatsheetseries.owasp.org/sitemap.xml",
            "authentication",
            5,
        ),
        (
            "w5g",
            "fastapi_docs",
            "https://fastapi.tiangolo.com/sitemap.xml",
            "tutorial",
            5,
        ),
        (
            "w5g",
            "prometheus_docs",
            "https://prometheus.io/sitemap.xml",
            "alerting",
            5,
        ),
        (
            "w5g",
            "mcp_llms",
            "https://modelcontextprotocol.io/llms.txt",
            "tools",
            5,
        ),
        (
            "w5g",
            "cloudflare_llms",
            "https://developers.cloudflare.com/llms.txt",
            "workers",
            5,
        ),
        (
            "w5g",
            "pydantic_llms",
            "https://pydantic.dev/docs/validation/latest/llms.txt",
            "validators",
            5,
        ),
        (
            "w5p",
            "spec_openapis_sitemap",
            "https://spec.openapis.org/sitemap.xml",
            "schema",
            5,
        ),
    ];

    for (wave, id, base_url, query, limit) in cases {
        let net = Arc::new(ReplayNet::from_file(fixture_path(wave, id, "search")).unwrap());
        let reg = build_registry(Arc::clone(&net));
        let adapter = reg.protocol("sitemap_llms").unwrap();

        let out = adapter
            .call(
                base_url,
                "search",
                json!({"query": query, "limit": limit}),
                &default_cx(),
            )
            .await
            .unwrap_or_else(|e| panic!("sitemap_llms {wave}/{id} failed: {e}"));
        assert!(
            out.items.len() >= 2 && out.items.len() <= limit,
            "{wave}/{id} got {}",
            out.items.len()
        );
        for raw in &out.items {
            assert!(
                raw.get("kind").is_none(),
                "sitemap_llms {wave}/{id} Hits item must omit kind: {raw}"
            );
            let hit: Hit = serde_json::from_value(raw.clone()).expect("valid Hit");
            assert!(!hit.title.is_empty());
            assert!(hit.url.starts_with("http"));
        }

        let listed = adapter
            .call(base_url, "list", json!({"limit": 10}), &default_cx())
            .await
            .unwrap();
        assert_eq!(listed.items.len(), 10);
        assert!(listed.items[0]["url"].as_str().unwrap().starts_with("http"));
        assert!(net.unmatched().is_empty());
    }
}

#[test]
fn dialect_validation_accepts_valid_and_rejects_invalid_keys_and_values() {
    let net = Arc::new(ReplayNet::from_fixtures(vec![]).unwrap());
    let reg = build_registry(net);

    let check_bad = |kind: &str, k: &str, v: Value| {
        let mut d = BTreeMap::new();
        d.insert(k.to_owned(), v);
        assert!(
            reg.protocol(kind).unwrap().validate_dialect(&d).is_err(),
            "{kind} should reject {k}"
        );
    };
    let check_ok = |kind: &str, pairs: &[(&str, Value)]| {
        let mut d = BTreeMap::new();
        for (k, v) in pairs {
            d.insert((*k).to_owned(), v.clone());
        }
        reg.protocol(kind)
            .unwrap()
            .validate_dialect(&d)
            .unwrap_or_else(|e| panic!("{kind} should accept {d:?}: {e}"));
    };

    check_ok("ckan", &[("api_prefix", json!("/api/3/action"))]);
    check_bad("ckan", "unknown_key", json!(1));
    check_bad("ckan", "api_prefix", json!(""));

    check_ok("socrata", &[("auto_scope_domain", json!(false))]);
    check_bad("socrata", "auto_scope_domain", json!("false"));
    check_bad("socrata", "bogus", json!(true));

    check_ok("opendatasoft", &[("api_version", json!("v2.1"))]);
    check_bad("opendatasoft", "api_version", json!("../v1"));

    check_ok("discourse", &[("base_path", json!("/community"))]);
    check_bad("discourse", "base_path", json!("../etc"));

    check_ok("wordpress_rest", &[("rest_base", json!("wp-json"))]);
    check_bad("wordpress_rest", "rest_base", json!(123));

    check_ok(
        "rss_atom",
        &[
            ("format", json!("atom")),
            ("max_response_bytes", json!(524_288)),
        ],
    );
    check_bad("rss_atom", "format", json!("json"));
    check_bad("rss_atom", "max_response_bytes", json!(100));

    check_ok(
        "activitypub_fediverse",
        &[
            ("flavor", json!("lemmy")),
            ("lemmy_type", json!("Comments")),
        ],
    );
    check_bad("activitypub_fediverse", "flavor", json!("bluesky"));
    check_bad("activitypub_fediverse", "lemmy_type", json!("Users"));

    check_ok(
        "shopify_json",
        &[
            ("suggest_types", json!("product,article")),
            ("currency", json!("GBP")),
        ],
    );
    check_bad("shopify_json", "currency", json!(""));

    check_ok(
        "sitemap_llms",
        &[
            ("mode", json!("llms_txt")),
            ("max_response_bytes", json!(1_048_576)),
        ],
    );
    check_bad("sitemap_llms", "mode", json!("html"));
    check_bad("sitemap_llms", "max_response_bytes", json!(999_999_999));
}

#[tokio::test]
async fn error_handling_rate_limit_cancel_and_latin1_charset() {
    let mut req_429 = FixtureRequest::new(
        "GET",
        "https://rate.example.org/api/3/action/package_search?q=x&rows=10",
    );
    req_429
        .headers_subset
        .insert("accept".into(), "application/json".into());
    let resp_429 = FixtureResponse::from_bytes(
        429,
        vec![("retry-after".into(), "45".into())],
        b"Too many requests",
    );

    // Latin-1 encoded WordPress JSON title: "Café" (0xe9)
    let mut req_latin = FixtureRequest::new(
        "GET",
        "https://latin.example.org/wp-json/wp/v2/search?search=cafe&per_page=1",
    );
    req_latin
        .headers_subset
        .insert("accept".into(), "application/json".into());
    let mut latin_bytes = br#"[{"id":1,"title":"Caf"#.to_vec();
    latin_bytes.push(0xe9);
    latin_bytes.extend_from_slice(
        br#"","url":"https://latin.example.org/1","type":"post","subtype":"post"}]"#,
    );
    let resp_latin = FixtureResponse::from_bytes(
        200,
        vec![(
            "content-type".into(),
            "application/json; charset=iso-8859-1".into(),
        )],
        &latin_bytes,
    );

    // 429 with Retry-After HTTP-date (M4)
    let mut req_429_date = FixtureRequest::new(
        "GET",
        "https://rate-date.example.org/api/3/action/package_search?q=x&rows=10",
    );
    req_429_date
        .headers_subset
        .insert("accept".into(), "application/json".into());
    let resp_429_date = FixtureResponse::from_bytes(
        429,
        vec![("retry-after".into(), "Mon, 01 Jan 2035 00:00:00 GMT".into())],
        b"Slow down",
    );

    let f = make_fixture(
        "errors",
        "search",
        vec![
            Exchange::new(req_429, resp_429),
            Exchange::new(req_429_date, resp_429_date),
            Exchange::new(req_latin, resp_latin),
        ],
    );
    let net = Arc::new(ReplayNet::from_fixtures(vec![f]).unwrap());
    let reg = build_registry(Arc::clone(&net));

    // 429 -> RateLimited with Retry-After
    let err = reg
        .protocol("ckan")
        .unwrap()
        .call(
            "https://rate.example.org",
            "search",
            json!({"query": "x"}),
            &default_cx(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            FsmError::RateLimited {
                retry_after_s: Some(45),
                ..
            }
        ),
        "got {err:?}"
    );

    // 429 -> RateLimited with Retry-After HTTP-date (M4)
    let err_date = reg
        .protocol("ckan")
        .unwrap()
        .call(
            "https://rate-date.example.org",
            "search",
            json!({"query": "x"}),
            &default_cx(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            err_date,
            FsmError::RateLimited {
                retry_after_s: Some(s),
                ..
            } if s > 0
        ),
        "expected HTTP-date Retry-After > 0, got {err_date:?}"
    );

    // Cancelled token -> Timeout(0)
    let cx_cancel = default_cx();
    cx_cancel.cancel.cancel();
    let err = reg
        .protocol("ckan")
        .unwrap()
        .call(
            "https://rate.example.org",
            "search",
            json!({"query": "x"}),
            &cx_cancel,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, FsmError::Timeout(0)), "got {err:?}");

    // Latin-1 charset decoding
    let out = reg
        .protocol("wordpress_rest")
        .unwrap()
        .call(
            "https://latin.example.org",
            "search",
            json!({"query": "cafe", "limit": 1}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(out.items[0]["title"], "Caf\u{00e9}");

    // Unknown op & missing required param
    assert!(matches!(
        reg.protocol("ckan")
            .unwrap()
            .call("https://rate.example.org", "nope", json!({}), &default_cx())
            .await,
        Err(FsmError::InvalidInput(_))
    ));
    assert!(matches!(
        reg.protocol("ckan")
            .unwrap()
            .call(
                "https://rate.example.org",
                "search",
                json!({}),
                &default_cx()
            )
            .await,
        Err(FsmError::InvalidInput(_))
    ));
    // Non-http(s) base_url rejected
    assert!(
        reg.protocol("ckan")
            .unwrap()
            .call(
                "file:///etc/passwd",
                "search",
                json!({"query": "x"}),
                &default_cx()
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn edge_cases_sitemapindex_llms_fences_socrata_unscoped_wp_pagination_and_lemmy_comments() {
    // 1. sitemapindex XML
    let mut sm_req = FixtureRequest::new("GET", "https://docs.example.org/sitemap_index.xml");
    sm_req
        .headers_subset
        .insert("accept".into(), "application/xml, text/xml".into());
    let sm_resp = FixtureResponse::from_bytes(
        200,
        vec![("content-type".into(), "application/xml".into())],
        br#"<?xml version="1.0" encoding="UTF-8"?>
<sitemapindex xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <sitemap><loc>https://docs.example.org/sitemap-auth.xml</loc><lastmod>2026-10-15</lastmod></sitemap>
  <sitemap><loc>https://docs.example.org/sitemap-billing.xml</loc><lastmod>2026-10-16</lastmod></sitemap>
</sitemapindex>"#,
    );

    // 2. llms.txt with fenced code block (must ignore link inside fence) and relative link
    let mut llms_req = FixtureRequest::new("GET", "https://docs.example.org/llms.txt");
    llms_req
        .headers_subset
        .insert("accept".into(), "text/plain, text/markdown".into());
    let llms_resp = FixtureResponse::from_bytes(
        200,
        vec![("content-type".into(), "text/plain".into())],
        b"# Core Guides\n\n```markdown\n- [Fake Link](https://docs.example.org/fake.md): ignore me\n```\n\n- [Auth Guide](/docs/auth.md): OAuth2 and API tokens\n- [Mail Link](mailto:test@example.org)\n",
    );

    // 3. Socrata with auto_scope_domain=false omits domains on portal host
    let soc_ex = json_exchange(
        "https://data.cdc.gov/api/catalog/v1?q=flu&limit=2",
        json!({"resultSetSize": 1, "results": [{"resource": {"id": "abcd-1234", "name": "Flu Data", "description": "Weekly counts", "type": "dataset"}, "metadata": {"domain": "data.cdc.gov"}, "permalink": "https://data.cdc.gov/d/abcd-1234"}]}),
    );

    // 4. WordPress REST with X-WP-TotalPages pagination & subtype
    let mut wp_req = FixtureRequest::new(
        "GET",
        "https://wp.example.org/wp-json/wp/v2/search?search=rust&per_page=1&page=1&subtype=page",
    );
    wp_req
        .headers_subset
        .insert("accept".into(), "application/json".into());
    let wp_resp = FixtureResponse::from_bytes(
        200,
        vec![
            ("content-type".into(), "application/json".into()),
            ("x-wp-totalpages".into(), "3".into()),
        ],
        br#"[{"id":7,"title":"Rust &amp; Docs","url":"https://wp.example.org/rust","type":"post","subtype":"page"}]"#,
    );

    // 5. Lemmy Comments search
    let lemmy_ex = json_exchange(
        "https://lemmy.example.org/api/v3/search?q=async&type_=Comments&limit=2",
        json!({
            "type_": "Comments",
            "posts": [],
            "communities": [],
            "comments": [{
                "comment": {"id": 501, "content": "Try tokio::select!", "published": "2026-11-01T00:00:00Z", "ap_id": "https://lemmy.example.org/comment/501"},
                "post": {"id": 10, "name": "Async patterns"}
            }]
        }),
    );

    // 6. RSS with xml:base, relative link, and multi-word ranking (title beats description)
    let mut rss_req = FixtureRequest::new("GET", "https://news.example.org/feed.xml");
    rss_req.headers_subset.insert(
        "accept".into(),
        "application/rss+xml, application/xml, text/xml".into(),
    );
    let rss_resp = FixtureResponse::from_bytes(
        200,
        vec![("content-type".into(), "application/rss+xml".into())],
        br#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xml:base="https://news.example.org/">
  <channel>
    <item><title>General update</title><link>/posts/1</link><description>Mentions rust compiler once.</description><pubDate>Tue, 01 Oct 2026 10:00:00 GMT</pubDate></item>
    <item><title>Rust compiler 1.98 released</title><guid isPermaLink="true">https://news.example.org/posts/2</guid><description>Release notes.</description><pubDate>Wed, 02 Oct 2026 10:00:00 GMT</pubDate></item>
  </channel>
</rss>"#,
    );

    let f = make_fixture(
        "edges",
        "mixed",
        vec![
            Exchange::new(sm_req, sm_resp),
            Exchange::new(llms_req, llms_resp),
            soc_ex,
            Exchange::new(wp_req, wp_resp),
            lemmy_ex,
            Exchange::new(rss_req, rss_resp),
        ],
    );
    let net = Arc::new(ReplayNet::from_fixtures(vec![f]).unwrap());
    let reg = build_registry(Arc::clone(&net));

    // Check sitemapindex
    let sm_out = reg
        .protocol("sitemap_llms")
        .unwrap()
        .call(
            "https://docs.example.org/sitemap_index.xml",
            "search",
            json!({"query": "auth", "limit": 5}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(sm_out.items.len(), 1);
    assert_eq!(
        sm_out.items[0]["url"],
        "https://docs.example.org/sitemap-auth.xml"
    );
    assert_eq!(sm_out.items[0]["date"], "2026-10-15");

    // Check llms.txt fenced block ignored & relative link resolved
    let llms_out = reg
        .protocol("sitemap_llms")
        .unwrap()
        .call(
            "https://docs.example.org/llms.txt",
            "list",
            json!({"limit": 10}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(llms_out.items.len(), 1);
    assert_eq!(
        llms_out.items[0]["url"],
        "https://docs.example.org/docs/auth.md"
    );
    assert_eq!(llms_out.items[0]["title"], "Auth Guide");

    // Check Socrata unscoped dialect
    let soc_cx = cx_with_dialect(&[("auto_scope_domain", json!(false))]);
    let soc_out = reg
        .protocol("socrata")
        .unwrap()
        .call(
            "https://data.cdc.gov",
            "search",
            json!({"query": "flu", "limit": 2}),
            &soc_cx,
        )
        .await
        .unwrap();
    assert_eq!(soc_out.items.len(), 1);

    // Check WordPress pagination cursor
    let wp_out = reg
        .protocol("wordpress_rest")
        .unwrap()
        .call(
            "https://wp.example.org",
            "search",
            json!({"query": "rust", "limit": 1, "page": 1, "subtype": "page"}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(wp_out.items[0]["title"], "Rust & Docs");
    assert_eq!(wp_out.next_cursor.as_deref(), Some("2"));

    // Check Lemmy Comments override
    let lemmy_cx = cx_with_dialect(&[("flavor", json!("lemmy"))]);
    let lemmy_out = reg
        .protocol("activitypub_fediverse")
        .unwrap()
        .call(
            "https://lemmy.example.org",
            "search",
            json!({"query": "async", "type": "Comments", "limit": 2}),
            &lemmy_cx,
        )
        .await
        .unwrap();
    assert_eq!(lemmy_out.items.len(), 1);
    assert_eq!(
        lemmy_out.items[0]["url"],
        "https://lemmy.example.org/comment/501"
    );

    // Check RSS xml:base + guid fallback + title-beats-description ranking + no doubled text
    let rss_out = reg
        .protocol("rss_atom")
        .unwrap()
        .call(
            "https://news.example.org/feed.xml",
            "search",
            json!({"query": "rust compiler", "limit": 5}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(rss_out.items.len(), 2);
    assert_eq!(rss_out.items[0]["title"], "Rust compiler 1.98 released");
    assert_eq!(rss_out.items[0]["url"], "https://news.example.org/posts/2");
    assert_eq!(rss_out.items[0]["snippet"], "Release notes.");
    assert_eq!(rss_out.items[0]["date"], "2026-10-02");
    assert_eq!(rss_out.items[1]["title"], "General update");
    assert_eq!(rss_out.items[1]["url"], "https://news.example.org/posts/1");
    assert_eq!(rss_out.items[1]["snippet"], "Mentions rust compiler once.");
    assert_eq!(rss_out.items[1]["date"], "2026-10-01");

    assert!(net.unmatched().is_empty());
}

// ---------------------------------------------------------------------------
// Review Round 1 regression tests (B1, I1-I6, M1-M7) & Missing Tests #1-#12
// ---------------------------------------------------------------------------

#[tokio::test]
async fn b1_xml_entity_bomb_refused_and_title_and_url_capped() {
    // Exact reviewer quadratic entity-expansion payload (~19 KiB -> 30 MB if expanded)
    let entity_val = "A".repeat(10_000);
    let refs = "&a;".repeat(3_000);
    let rss_bomb = format!(
        r#"<?xml version="1.0"?><!DOCTYPE rss [<!ENTITY a "{entity_val}">]><rss><channel><item><title>{refs}</title><link>https://evil.example.org/1</link></item></channel></rss>"#
    );
    let sm_bomb = format!(
        r#"<?xml version="1.0"?><!DOCTYPE urlset [<!ENTITY a "{entity_val}">]><urlset><url><loc>https://evil.example.org/{refs}</loc></url></urlset>"#
    );
    let lower_entity = r#"<?xml version="1.0"?><!DOCTYPE rss [<!entity x "boom">]><rss><channel><item><title>&x;</title></item></channel></rss>"#;

    // Benign feed with huge title (500 chars) and huge URL (2,500 bytes)
    let long_title = "T".repeat(500);
    let long_url = format!("https://caps.example.org/{}", "x".repeat(2_500));
    let capped_rss = format!(
        r#"<?xml version="1.0"?><rss version="2.0"><channel><item><title>{long_title}</title><link>{long_url}</link></item></channel></rss>"#
    );
    let capped_sm = format!(
        r#"<?xml version="1.0"?><urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9"><url><loc>{long_url}</loc></url><url><loc>https://caps.example.org/ok-page</loc></url></urlset>"#
    );

    let xml_ex = |url: &str, accept: &str, body: &str| {
        let mut req = FixtureRequest::new("GET", url);
        req.headers_subset.insert("accept".into(), accept.into());
        Exchange::new(
            req,
            FixtureResponse::from_bytes(
                200,
                vec![("content-type".into(), "application/xml".into())],
                body.as_bytes(),
            ),
        )
    };

    let rss_accept = "application/rss+xml, application/xml, text/xml";
    let sm_accept = "application/xml, text/xml";
    let f = make_fixture(
        "b1",
        "search",
        vec![
            xml_ex("https://evil.example.org/bomb.xml", rss_accept, &rss_bomb),
            xml_ex("https://evil.example.org/sitemap.xml", sm_accept, &sm_bomb),
            xml_ex(
                "https://evil.example.org/lower.xml",
                rss_accept,
                lower_entity,
            ),
            xml_ex("https://caps.example.org/feed.xml", rss_accept, &capped_rss),
            xml_ex(
                "https://caps.example.org/sitemap.xml",
                sm_accept,
                &capped_sm,
            ),
        ],
    );
    let net = Arc::new(ReplayNet::from_fixtures(vec![f]).unwrap());
    let reg = build_registry(Arc::clone(&net));

    for (proto, url) in [
        ("rss_atom", "https://evil.example.org/bomb.xml"),
        ("rss_atom", "https://evil.example.org/lower.xml"),
        ("sitemap_llms", "https://evil.example.org/sitemap.xml"),
    ] {
        let t0 = std::time::Instant::now();
        let err = reg
            .protocol(proto)
            .unwrap()
            .call(url, "search", json!({"limit": 5}), &default_cx())
            .await
            .unwrap_err();
        assert!(
            matches!(err, FsmError::Upstream { ref message, .. } if message.contains("DTD")),
            "{proto} {url} expected DTD refusal, got {err:?}"
        );
        assert!(
            t0.elapsed() < Duration::from_millis(250),
            "{proto} bomb took {:?}",
            t0.elapsed()
        );
    }

    // Title capped at <= 300 chars (+ ellipsis), URL > 2048 bytes dropped
    let rss_out = reg
        .protocol("rss_atom")
        .unwrap()
        .call(
            "https://caps.example.org/feed.xml",
            "search",
            json!({"limit": 5}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(rss_out.items.len(), 1);
    let got_title = rss_out.items[0]["title"].as_str().unwrap();
    assert!(
        got_title.chars().count() <= 302 && got_title.ends_with(" …"),
        "title not capped: {} chars ({got_title:?})",
        got_title.chars().count()
    );
    assert_eq!(rss_out.items[0]["url"], "");

    let sm_out = reg
        .protocol("sitemap_llms")
        .unwrap()
        .call(
            "https://caps.example.org/sitemap.xml",
            "list",
            json!({"limit": 5}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(sm_out.items.len(), 1);
    assert_eq!(sm_out.items[0]["url"], "https://caps.example.org/ok-page");
}

/// I3 (Missing Test #2 — XML-prolog/BOM analogue of Python
/// `test_decode_sniffs_meta_charset_when_header_lacks_one` / `test_decode_honors_header_charset_gbk`).
#[tokio::test]
async fn test_decode_sniffs_xml_prolog_and_bom_charset() {
    let rss_accept = "application/rss+xml, application/xml, text/xml";
    let sm_accept = "application/xml, text/xml";

    // 1. ISO-8859-1 prolog without header charset
    let mut iso_bytes =
        br#"<?xml version="1.0" encoding="ISO-8859-1"?><rss><channel><item><title>Caf"#.to_vec();
    iso_bytes.push(0xe9);
    iso_bytes.extend_from_slice(b" r");
    iso_bytes.push(0xe9);
    iso_bytes.extend_from_slice(b"sum");
    iso_bytes.push(0xe9);
    iso_bytes.extend_from_slice(
        b"</title><link>https://enc.example.org/iso</link></item></channel></rss>",
    );

    // 2. GBK prolog without header charset ("中文" = D6 D0 CE C4)
    let mut gbk_bytes =
        br#"<?xml version="1.0" encoding="GBK"?><rss><channel><item><title>"#.to_vec();
    gbk_bytes.extend_from_slice(&[0xD6, 0xD0, 0xCE, 0xC4]);
    gbk_bytes.extend_from_slice(
        b"</title><link>https://enc.example.org/gbk</link></item></channel></rss>",
    );

    // 3. UTF-16LE BOM without header charset
    let utf16_xml = r#"<?xml version="1.0"?><rss><channel><item><title>Café UTF16</title><link>https://enc.example.org/u16</link></item></channel></rss>"#;
    let mut utf16_bytes = vec![0xFF, 0xFE];
    for u in utf16_xml.encode_utf16() {
        utf16_bytes.extend_from_slice(&u.to_le_bytes());
    }

    // 4. Header charset=gbk honored even without prolog encoding (test_decode_honors_header_charset_gbk)
    let mut hdr_gbk = br#"<?xml version="1.0"?><rss><channel><item><title>"#.to_vec();
    hdr_gbk.extend_from_slice(&[0xD6, 0xD0, 0xCE, 0xC4]);
    hdr_gbk.extend_from_slice(
        b"</title><link>https://enc.example.org/hdr</link></item></channel></rss>",
    );

    // 5. ISO-8859-1 sitemap XML without header charset
    let mut sm_iso = br#"<?xml version="1.0" encoding="ISO-8859-1"?><urlset><url><loc>https://enc.example.org/caf"#.to_vec();
    sm_iso.push(0xe9);
    sm_iso.extend_from_slice(b"</loc></url></urlset>");

    let raw_ex = |url: &str, accept: &str, ct: &str, body: &[u8]| {
        let mut req = FixtureRequest::new("GET", url);
        req.headers_subset.insert("accept".into(), accept.into());
        Exchange::new(
            req,
            FixtureResponse::from_bytes(200, vec![("content-type".into(), ct.into())], body),
        )
    };

    let f = make_fixture(
        "i3_charset",
        "search",
        vec![
            raw_ex(
                "https://enc.example.org/iso.xml",
                rss_accept,
                "application/rss+xml",
                &iso_bytes,
            ),
            raw_ex(
                "https://enc.example.org/gbk.xml",
                rss_accept,
                "text/xml",
                &gbk_bytes,
            ),
            raw_ex(
                "https://enc.example.org/u16.xml",
                rss_accept,
                "text/xml",
                &utf16_bytes,
            ),
            raw_ex(
                "https://enc.example.org/hdr.xml",
                rss_accept,
                "text/xml; charset=gbk",
                &hdr_gbk,
            ),
            raw_ex(
                "https://enc.example.org/sitemap.xml",
                sm_accept,
                "application/xml",
                &sm_iso,
            ),
        ],
    );
    let net = Arc::new(ReplayNet::from_fixtures(vec![f]).unwrap());
    let reg = build_registry(Arc::clone(&net));
    let rss = reg.protocol("rss_atom").unwrap();

    let o_iso = rss
        .call(
            "https://enc.example.org/iso.xml",
            "latest",
            json!({"limit": 5}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(o_iso.items[0]["title"], "Café résumé");

    let o_gbk = rss
        .call(
            "https://enc.example.org/gbk.xml",
            "latest",
            json!({"limit": 5}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(o_gbk.items[0]["title"], "中文");

    let o_u16 = rss
        .call(
            "https://enc.example.org/u16.xml",
            "latest",
            json!({"limit": 5}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(o_u16.items[0]["title"], "Café UTF16");

    let o_hdr = rss
        .call(
            "https://enc.example.org/hdr.xml",
            "latest",
            json!({"limit": 5}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(o_hdr.items[0]["title"], "中文");

    let o_sm = reg
        .protocol("sitemap_llms")
        .unwrap()
        .call(
            "https://enc.example.org/sitemap.xml",
            "list",
            json!({"limit": 5}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(o_sm.items[0]["url"], "https://enc.example.org/café");
}

/// I4 (Missing Test #3): Gzipped `.xml.gz` sitemap in auto & explicit modes + decompressed size cap.
#[tokio::test]
async fn i4_gzip_sitemap_auto_explicit_and_decompressed_cap() {
    let sm_xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <url><loc>https://docs.example.org/guides/oauth-setup</loc><lastmod>2026-10-20</lastmod></url>
  <url><loc>https://docs.example.org/guides/rate-limits</loc><lastmod>2026-10-21</lastmod></url>
</urlset>"#;
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(sm_xml).unwrap();
    let gz_small = enc.finish().unwrap();

    // Compressed payload is tiny (< 1 KiB) but decompressed exceeds 65,536 bytes!
    let mut big_xml = String::from(
        r#"<?xml version="1.0"?><urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">"#,
    );
    for i in 0..1_200 {
        big_xml.push_str(&format!(
            "<url><loc>https://docs.example.org/page/{i:06}-padding-padding-padding</loc></url>"
        ));
    }
    big_xml.push_str("</urlset>");
    assert!(big_xml.len() > 70_000);
    let mut enc2 = GzEncoder::new(Vec::new(), Compression::best());
    enc2.write_all(big_xml.as_bytes()).unwrap();
    let gz_bomb = enc2.finish().unwrap();
    assert!(gz_bomb.len() < 10_000);

    let make_gz_ex = |url: &str, bytes: &[u8]| {
        let mut req = FixtureRequest::new("GET", url);
        req.headers_subset
            .insert("accept".into(), "application/xml, text/xml".into());
        Exchange::new(
            req,
            FixtureResponse::from_bytes(
                200,
                vec![("content-type".into(), "application/x-gzip".into())],
                bytes,
            ),
        )
    };

    let f = make_fixture(
        "i4_gz",
        "search",
        vec![
            make_gz_ex("https://docs.example.org/sitemap.xml.gz", &gz_small),
            make_gz_ex("https://docs.example.org/bomb.xml.gz", &gz_bomb),
        ],
    );
    let net = Arc::new(ReplayNet::from_fixtures(vec![f]).unwrap());
    let reg = build_registry(Arc::clone(&net));
    let adapter = reg.protocol("sitemap_llms").unwrap();

    // Auto mode
    let out_auto = adapter
        .call(
            "https://docs.example.org/sitemap.xml.gz",
            "search",
            json!({"query": "oauth", "limit": 5}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(out_auto.items.len(), 1);
    assert_eq!(
        out_auto.items[0]["url"],
        "https://docs.example.org/guides/oauth-setup"
    );
    assert_eq!(out_auto.items[0]["date"], "2026-10-20");

    // Explicit sitemap_xml mode
    let cx_explicit = cx_with_dialect(&[("mode", json!("sitemap_xml"))]);
    let out_exp = adapter
        .call(
            "https://docs.example.org/sitemap.xml.gz",
            "list",
            json!({"limit": 5}),
            &cx_explicit,
        )
        .await
        .unwrap();
    assert_eq!(out_exp.items.len(), 2);

    // Decompressed size exceeding dialect.max_response_bytes -> MaxBytesExceeded
    let cx_cap = cx_with_dialect(&[("max_response_bytes", json!(65_536))]);
    let err = adapter
        .call(
            "https://docs.example.org/bomb.xml.gz",
            "list",
            json!({"limit": 5}),
            &cx_cap,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, FsmError::MaxBytesExceeded(65_536)),
        "expected MaxBytesExceeded(65536), got {err:?}"
    );
}

/// I2 (Missing Test #5): Hits items omit `"kind"` so `InstanceSource` injects Academic/Record per A1 §D.3.
#[tokio::test]
async fn i2_hits_omit_kind_for_instance_source_projection() {
    let nber_net =
        Arc::new(ReplayNet::from_file(fixture_path("w5g", "nber_papers_rss", "search")).unwrap());
    let nber_reg = build_registry(nber_net);
    let nber_out = nber_reg
        .protocol("rss_atom")
        .unwrap()
        .call(
            "https://back.nber.org/rss/new.xml",
            "search",
            json!({"limit": 3}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert!(!nber_out.items.is_empty());
    for item in &nber_out.items {
        assert!(
            item.get("kind").is_none(),
            "nber_papers_rss must omit kind so InstanceSource injects Academic: {item}"
        );
        let _: Hit = serde_json::from_value(item.clone()).expect("Hit deserializes with default");
    }

    let disc_net =
        Arc::new(ReplayNet::from_file(fixture_path("w5g", "rust_users", "search")).unwrap());
    let disc_reg = build_registry(disc_net);
    let disc_out = disc_reg
        .protocol("discourse")
        .unwrap()
        .call(
            "https://users.rust-lang.org",
            "search",
            json!({"query": "tokio-console deadlock", "limit": 3}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert!(!disc_out.items.is_empty());
    for item in &disc_out.items {
        assert!(
            item.get("kind").is_none(),
            "discourse must omit kind so InstanceSource injects Record: {item}"
        );
    }
}

/// I5 (Missing Test #6): Preserve `<T>`, `<10`, `>5` in plain-text titles & descriptions;
/// strip only real HTML tags (including Atom `type="html"`).
#[tokio::test]
async fn i5_angle_brackets_preserved_in_plain_titles_and_snippets() {
    let rss_xml = r#"<?xml version="1.0"?><rss version="2.0"><channel><item>
        <title>Understanding Box&lt;dyn Error&gt; and Vec&lt;T&gt; in Rust</title>
        <link>https://rust.example.org/box</link>
        <description>&lt;p&gt;Values &lt;10 and &gt;5 are safe.&lt;/p&gt;</description>
    </item></channel></rss>"#;

    let atom_xml = r#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom">
        <entry>
            <title type="text">Why HashMap&lt;K, V&gt; is slow</title>
            <link href="https://rust.example.org/hashmap"/>
        </entry>
        <entry>
            <title>Default Vec&lt;T&gt; title</title>
            <link href="https://rust.example.org/vec"/>
        </entry>
        <entry>
            <title type="html">&lt;b&gt;Bold&lt;/b&gt; &amp; &lt;em&gt;Italic&lt;/em&gt;</title>
            <link href="https://rust.example.org/html-title"/>
        </entry>
        <entry>
            <title type="xhtml"><div xmlns="http://www.w3.org/1999/xhtml">XHTML &lt;T&gt; <b>Title</b></div></title>
            <link href="https://rust.example.org/xhtml-title"/>
        </entry>
    </feed>"#;

    let mut rss_req = FixtureRequest::new("GET", "https://rust.example.org/rss.xml");
    rss_req.headers_subset.insert(
        "accept".into(),
        "application/rss+xml, application/xml, text/xml".into(),
    );
    let mut atom_req = FixtureRequest::new("GET", "https://rust.example.org/atom.xml");
    atom_req.headers_subset.insert(
        "accept".into(),
        "application/atom+xml, application/xml, text/xml".into(),
    );

    let ckan_ex = json_exchange(
        "https://data.example.org/api/3/action/package_search?q=pm&rows=2",
        json!({
            "success": true,
            "result": {
                "count": 1,
                "results": [{
                    "id": "1",
                    "name": "pm25",
                    "title": "PM2.5 Air Quality",
                    "notes": "Days with PM2.5 <10 µg/m3 and >5 µg/m3 are counted."
                }]
            }
        }),
    );
    let soc_ex = json_exchange(
        "https://data.example.org/api/catalog/v1?q=box&limit=2&domains=data.example.org",
        json!({
            "resultSetSize": 1,
            "results": [{
                "resource": {"id": "a1b2-c3d4", "name": "Metrics", "description": "Values <10 and Box<T> types"},
                "metadata": {"domain": "data.example.org"},
                "permalink": "https://data.example.org/d/a1b2-c3d4"
            }]
        }),
    );
    let disc_ex = json_exchange(
        "https://forum.example.org/search.json?q=result",
        json!({
            "topics": [{"id": 7, "title": "Returning Result<T, E>", "slug": "returning-result"}],
            "posts": [{"id": 70, "topic_id": 7, "post_number": 1, "blurb": "Use Result<T, E> when count <10 and >5"}]
        }),
    );
    let wp_ex = json_exchange(
        "https://blog.example.org/wp-json/wp/v2/search?search=vec&per_page=2",
        json!([{"id": 1, "title": "Using Vec&lt;T&gt; in Rust &#8211; Guide", "url": "https://blog.example.org/vec", "type": "post", "subtype": "post"}]),
    );

    let f = make_fixture(
        "i5",
        "search",
        vec![
            Exchange::new(
                rss_req,
                FixtureResponse::from_bytes(
                    200,
                    vec![("content-type".into(), "application/rss+xml".into())],
                    rss_xml.as_bytes(),
                ),
            ),
            Exchange::new(
                atom_req,
                FixtureResponse::from_bytes(
                    200,
                    vec![("content-type".into(), "application/atom+xml".into())],
                    atom_xml.as_bytes(),
                ),
            ),
            ckan_ex,
            soc_ex,
            disc_ex,
            wp_ex,
        ],
    );
    let net = Arc::new(ReplayNet::from_fixtures(vec![f]).unwrap());
    let reg = build_registry(Arc::clone(&net));

    let rss_out = reg
        .protocol("rss_atom")
        .unwrap()
        .call(
            "https://rust.example.org/rss.xml",
            "latest",
            json!({"limit": 5}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(
        rss_out.items[0]["title"],
        "Understanding Box<dyn Error> and Vec<T> in Rust"
    );
    assert_eq!(rss_out.items[0]["snippet"], "Values <10 and >5 are safe.");

    let atom_out = reg
        .protocol("rss_atom")
        .unwrap()
        .call(
            "https://rust.example.org/atom.xml",
            "latest",
            json!({"limit": 5}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(atom_out.items[0]["title"], "Why HashMap<K, V> is slow");
    assert_eq!(atom_out.items[1]["title"], "Default Vec<T> title");
    assert_eq!(atom_out.items[2]["title"], "Bold & Italic");
    assert_eq!(atom_out.items[3]["title"], "XHTML <T> Title");

    let ckan_out = reg
        .protocol("ckan")
        .unwrap()
        .call(
            "https://data.example.org",
            "search",
            json!({"query": "pm", "limit": 2}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(
        ckan_out.items[0]["snippet"],
        "Days with PM2.5 <10 µg/m3 and >5 µg/m3 are counted."
    );

    let soc_out = reg
        .protocol("socrata")
        .unwrap()
        .call(
            "https://data.example.org",
            "search",
            json!({"query": "box", "limit": 2}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(soc_out.items[0]["snippet"], "Values <10 and Box<T> types");

    let disc_out = reg
        .protocol("discourse")
        .unwrap()
        .call(
            "https://forum.example.org",
            "search",
            json!({"query": "result", "limit": 2}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(disc_out.items[0]["title"], "Returning Result<T, E>");
    assert_eq!(
        disc_out.items[0]["snippet"],
        "Use Result<T, E> when count <10 and >5"
    );

    let wp_out = reg
        .protocol("wordpress_rest")
        .unwrap()
        .call(
            "https://blog.example.org",
            "search",
            json!({"query": "vec", "limit": 2}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(
        wp_out.items[0]["title"],
        "Using Vec<T> in Rust \u{2013} Guide"
    );
    assert_eq!(wp_out.items[0]["snippet"], "");
}

/// I6 (Missing Test #7): RSS filters over untruncated summary (>400 chars), matches whole Latin
/// tokens (`ai` does not match "Brain"/"said"), and preserves CJK substring matching (both RSS & sitemap).
#[tokio::test]
async fn i6_rss_and_sitemap_full_summary_and_word_boundary_matching() {
    let long_prefix = "background sentence ".repeat(30); // 600 chars
    let rss_xml = format!(
        r#"<?xml version="1.0"?><rss version="2.0"><channel>
        <item><title>Long Paper</title><link>https://f.example.org/q</link><description>{long_prefix}quantum entanglement discovery.</description></item>
        <item><title>Brain training said to help</title><link>https://f.example.org/brain</link><description>Researchers said daily brain puzzles remain popular.</description></item>
        <item><title>Open AI Systems</title><link>https://f.example.org/ai</link><description>Evaluating open AI benchmarks.</description></item>
        <item><title>中文新闻</title><link>https://f.example.org/zh</link><description>最新量子计算新突破发表。</description></item>
    </channel></rss>"#
    );
    let llms_txt = "# Docs\n- [Brain training said to help](https://f.example.org/brain-said)\n- [Open AI Overview](https://f.example.org/ai-overview): Guide to AI agents\n- [量子计算指南](https://f.example.org/zh-guide): 量子算法入门\n";

    let mut rss_req = FixtureRequest::new("GET", "https://f.example.org/feed.xml");
    rss_req.headers_subset.insert(
        "accept".into(),
        "application/rss+xml, application/xml, text/xml".into(),
    );
    let mut llms_req = FixtureRequest::new("GET", "https://f.example.org/llms.txt");
    llms_req
        .headers_subset
        .insert("accept".into(), "text/plain, text/markdown".into());

    let f = make_fixture(
        "i6",
        "search",
        vec![
            Exchange::new(
                rss_req,
                FixtureResponse::from_bytes(
                    200,
                    vec![("content-type".into(), "application/rss+xml".into())],
                    rss_xml.as_bytes(),
                ),
            ),
            Exchange::new(
                llms_req,
                FixtureResponse::from_bytes(
                    200,
                    vec![("content-type".into(), "text/plain".into())],
                    llms_txt.as_bytes(),
                ),
            ),
        ],
    );
    let net = Arc::new(ReplayNet::from_fixtures(vec![f]).unwrap());
    let reg = build_registry(Arc::clone(&net));
    let rss = reg.protocol("rss_atom").unwrap();

    // 1. Term beyond 400 chars matches!
    let q_out = rss
        .call(
            "https://f.example.org/feed.xml",
            "search",
            json!({"query": "quantum", "limit": 5}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(q_out.items.len(), 1);
    assert_eq!(q_out.items[0]["title"], "Long Paper");
    assert!(q_out.items[0]["snippet"].as_str().unwrap().len() <= 410);

    // 2. "ai" must NOT match "Brain" / "said", ONLY "Open AI Systems"
    let ai_out = rss
        .call(
            "https://f.example.org/feed.xml",
            "search",
            json!({"query": "ai", "limit": 5}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(ai_out.items.len(), 1);
    assert_eq!(ai_out.items[0]["title"], "Open AI Systems");

    // 3. CJK substring query still matches
    let zh_out = rss
        .call(
            "https://f.example.org/feed.xml",
            "search",
            json!({"query": "量子", "limit": 5}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(zh_out.items.len(), 1);
    assert_eq!(zh_out.items[0]["title"], "中文新闻");

    // 4. Same whole-word & CJK rules in sitemap_llms
    let sm = reg.protocol("sitemap_llms").unwrap();
    let sm_ai = sm
        .call(
            "https://f.example.org/llms.txt",
            "search",
            json!({"query": "ai", "limit": 5}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(sm_ai.items.len(), 1);
    assert_eq!(sm_ai.items[0]["title"], "Open AI Overview");

    let sm_zh = sm
        .call(
            "https://f.example.org/llms.txt",
            "search",
            json!({"query": "量子", "limit": 5}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(sm_zh.items.len(), 1);
    assert_eq!(sm_zh.items[0]["title"], "量子计算指南");

    // 5. N2: Six real-fixture singular/plural & prefix pairs must all return hits
    let n2_pairs = [
        (
            "pydantic_llms",
            "https://pydantic.dev/docs/validation/latest/llms.txt",
            "validator",
            "validators",
        ),
        (
            "fastapi_docs",
            "https://fastapi.tiangolo.com/sitemap.xml",
            "dependency",
            "dependencies",
        ),
        (
            "prometheus_docs",
            "https://prometheus.io/sitemap.xml",
            "alert",
            "alerting",
        ),
        (
            "owasp_cheatsheets",
            "https://cheatsheetseries.owasp.org/sitemap.xml",
            "auth",
            "authentication",
        ),
        (
            "owasp_cheatsheets",
            "https://cheatsheetseries.owasp.org/sitemap.xml",
            "passwords",
            "password",
        ),
        (
            "cloudflare_llms",
            "https://developers.cloudflare.com/llms.txt",
            "queue",
            "queues",
        ),
    ];
    for (id, base_url, q1, q2) in n2_pairs {
        let fx_net = Arc::new(ReplayNet::from_file(fixture_path("w5g", id, "search")).unwrap());
        let fx_reg = build_registry(fx_net);
        let fx_sm = fx_reg.protocol("sitemap_llms").unwrap();
        for q in [q1, q2] {
            let out = fx_sm
                .call(
                    base_url,
                    "search",
                    json!({"query": q, "limit": 50}),
                    &default_cx(),
                )
                .await
                .unwrap();
            assert!(
                !out.items.is_empty(),
                "N2: {id} query {q:?} returned 0 items (expected >= 1)"
            );
        }
    }
}

/// M1 + Missing Test #8: Pagination cursors (`next_cursor` vs `None` on last page) and max page /
/// `i64::MAX` overflow protection across ckan, socrata, opendatasoft, discourse, wordpress, shopify.
#[tokio::test]
async fn m1_pagination_cursors_and_max_page_no_overflow() {
    let f = make_fixture(
        "m1_page",
        "search",
        vec![
            // CKAN page 1 (more) & last page (none)
            json_exchange(
                "https://p.example.org/api/3/action/package_search?q=a&rows=2&start=0",
                json!({"success": true, "result": {"count": 5, "results": [{"id": "1", "name": "d1", "title": "D1"}, {"id": "2", "name": "d2", "title": "D2"}]}}),
            ),
            json_exchange(
                "https://p.example.org/api/3/action/package_search?q=a&rows=2&start=4",
                json!({"success": true, "result": {"count": 5, "results": [{"id": "5", "name": "d5", "title": "D5"}]}}),
            ),
            // Socrata page 1 (more) & last page (none)
            json_exchange(
                "https://p.example.org/api/catalog/v1?q=a&limit=2&domains=p.example.org&offset=0",
                json!({"resultSetSize": 3, "results": [{"resource": {"id": "a1", "name": "A1"}}, {"resource": {"id": "a2", "name": "A2"}}]}),
            ),
            json_exchange(
                "https://p.example.org/api/catalog/v1?q=a&limit=2&domains=p.example.org&offset=2",
                json!({"resultSetSize": 3, "results": [{"resource": {"id": "a3", "name": "A3"}}]}),
            ),
            // OpenDataSoft page 1 (more) & last page (none)
            json_exchange(
                "https://p.example.org/api/explore/v2.1/catalog/datasets?where=search%28%2A%2C%22a%22%29&limit=2&offset=0",
                json!({"total_count": 3, "results": [{"dataset_id": "d1"}, {"dataset_id": "d2"}]}),
            ),
            json_exchange(
                "https://p.example.org/api/explore/v2.1/catalog/datasets?where=search%28%2A%2C%22a%22%29&limit=2&offset=2",
                json!({"total_count": 3, "results": [{"dataset_id": "d3"}]}),
            ),
            // Discourse page 1 (more -> "2") & max page 100 (more -> None, no overflow)
            json_exchange(
                "https://p.example.org/search.json?q=a&page=1",
                json!({"topics": [{"id": 1, "title": "T1"}], "posts": [{"id": 1, "topic_id": 1, "post_number": 1}], "grouped_search_result": {"more_full_page_results": true}}),
            ),
            json_exchange(
                "https://p.example.org/search.json?q=a&page=100",
                json!({"topics": [{"id": 2, "title": "T2"}], "posts": [{"id": 2, "topic_id": 2, "post_number": 1}], "grouped_search_result": {"more_full_page_results": true}}),
            ),
            // Shopify products max page 1000 (full page -> None, no overflow)
            json_exchange(
                "https://p.example.org/products.json?limit=1&page=1000",
                json!({"products": [{"id": 1, "title": "P1", "handle": "p1"}]}),
            ),
        ],
    );
    let net = Arc::new(ReplayNet::from_fixtures(vec![f]).unwrap());
    let reg = build_registry(Arc::clone(&net));

    // CKAN
    let c1 = reg
        .protocol("ckan")
        .unwrap()
        .call(
            "https://p.example.org",
            "search",
            json!({"query": "a", "limit": 2, "offset": 0}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(c1.next_cursor.as_deref(), Some("2"));
    let c2 = reg
        .protocol("ckan")
        .unwrap()
        .call(
            "https://p.example.org",
            "search",
            json!({"query": "a", "limit": 2, "offset": 4}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(c2.next_cursor, None);

    // Socrata
    let s1 = reg
        .protocol("socrata")
        .unwrap()
        .call(
            "https://p.example.org",
            "search",
            json!({"query": "a", "limit": 2, "offset": 0}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(s1.next_cursor.as_deref(), Some("2"));
    let s2 = reg
        .protocol("socrata")
        .unwrap()
        .call(
            "https://p.example.org",
            "search",
            json!({"query": "a", "limit": 2, "offset": 2}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(s2.next_cursor, None);

    // OpenDataSoft
    let o1 = reg
        .protocol("opendatasoft")
        .unwrap()
        .call(
            "https://p.example.org",
            "search",
            json!({"query": "a", "limit": 2, "offset": 0}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(o1.next_cursor.as_deref(), Some("2"));
    let o2 = reg
        .protocol("opendatasoft")
        .unwrap()
        .call(
            "https://p.example.org",
            "search",
            json!({"query": "a", "limit": 2, "offset": 2}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(o2.next_cursor, None);

    // Discourse more_full_page_results + max page 100 + i64::MAX rejected without panic
    let d1 = reg
        .protocol("discourse")
        .unwrap()
        .call(
            "https://p.example.org",
            "search",
            json!({"query": "a", "page": 1}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(d1.next_cursor.as_deref(), Some("2"));
    let d100 = reg
        .protocol("discourse")
        .unwrap()
        .call(
            "https://p.example.org",
            "search",
            json!({"query": "a", "page": 100}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(d100.next_cursor, None);
    assert!(matches!(
        reg.protocol("discourse")
            .unwrap()
            .call(
                "https://p.example.org",
                "search",
                json!({"query": "a", "page": i64::MAX}),
                &default_cx(),
            )
            .await,
        Err(FsmError::InvalidInput(_))
    ));

    // WordPress i64::MAX page rejected
    assert!(matches!(
        reg.protocol("wordpress_rest")
            .unwrap()
            .call(
                "https://p.example.org",
                "search",
                json!({"query": "a", "page": i64::MAX}),
                &default_cx(),
            )
            .await,
        Err(FsmError::InvalidInput(_))
    ));

    // Shopify products max page 1000 & i64::MAX rejected
    let sh1000 = reg
        .protocol("shopify_json")
        .unwrap()
        .call(
            "https://p.example.org",
            "products",
            json!({"limit": 1, "page": 1000}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(sh1000.next_cursor, None);
    assert!(matches!(
        reg.protocol("shopify_json")
            .unwrap()
            .call(
                "https://p.example.org",
                "products",
                json!({"limit": 1, "page": i64::MAX}),
                &default_cx(),
            )
            .await,
        Err(FsmError::InvalidInput(_))
    ));

    // Declared offset max enforced
    for (proto, op, max_over) in [
        ("ckan", "search", 10_001),
        ("socrata", "search", 10_001),
        ("opendatasoft", "search", 10_001),
    ] {
        assert!(matches!(
            reg.protocol(proto)
                .unwrap()
                .call(
                    "https://p.example.org",
                    op,
                    json!({"query": "a", "offset": max_over}),
                    &default_cx(),
                )
                .await,
            Err(FsmError::InvalidInput(_))
        ));
    }
}

/// M6: `llms.txt` skips Markdown image links `![alt](url)` while indexing normal links.
#[tokio::test]
async fn m6_llms_txt_skips_markdown_image_links() {
    let mut req = FixtureRequest::new("GET", "https://docs.example.org/llms.txt");
    req.headers_subset
        .insert("accept".into(), "text/plain, text/markdown".into());
    let resp = FixtureResponse::from_bytes(
        200,
        vec![("content-type".into(), "text/plain".into())],
        b"# Architecture\n![diagram](https://docs.example.org/arch.png)\n- ![icon](/img/icon.svg) [Real Guide](/docs/guide.md): Actual documentation\n",
    );
    let f = make_fixture("m6", "list", vec![Exchange::new(req, resp)]);
    let net = Arc::new(ReplayNet::from_fixtures(vec![f]).unwrap());
    let reg = build_registry(Arc::clone(&net));
    let out = reg
        .protocol("sitemap_llms")
        .unwrap()
        .call(
            "https://docs.example.org/llms.txt",
            "list",
            json!({"limit": 10}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(out.items.len(), 1);
    assert_eq!(out.items[0]["title"], "Real Guide");
    assert_eq!(
        out.items[0]["url"],
        "https://docs.example.org/docs/guide.md"
    );
}

/// M7 + Missing Test #9: Mastodon tag normalization & NodeInfo 2.0 fallback, relative href,
/// foreign-host href rejection, and unsupported software error.
#[tokio::test]
async fn m7_mastodon_tag_normalization_and_nodeinfo_edges() {
    let status = json!([{
        "id": "1",
        "created_at": "2026-11-20T00:00:00Z",
        "url": "https://masto.example.org/@u/1",
        "content": "<p>Hello #Rust</p>",
        "account": {"acct": "u"}
    }]);

    let f = make_fixture(
        "m7_ap",
        "search",
        vec![
            // Mastodon "#Rust async" -> /tag/Rust
            json_exchange(
                "https://masto.example.org/api/v1/timelines/tag/Rust?limit=2",
                status.clone(),
            ),
            // Mastodon Unicode tag "#日本語" -> percent-encoded
            json_exchange(
                "https://masto.example.org/api/v1/timelines/tag/%E6%97%A5%E6%9C%AC%E8%AA%9E?limit=2",
                status.clone(),
            ),
            // NodeInfo rel 2.0 fallback + relative href "/nodeinfo/2.0"
            json_exchange(
                "https://rel20.example.org/.well-known/nodeinfo",
                json!({"links": [{"rel": "http://nodeinfo.diaspora.software/ns/schema/2.0", "href": "/nodeinfo/2.0"}]}),
            ),
            json_exchange(
                "https://rel20.example.org/nodeinfo/2.0",
                json!({"version": "2.0", "software": {"name": "mastodon", "version": "4.2.0"}}),
            ),
            json_exchange(
                "https://rel20.example.org/api/v1/timelines/tag/rust?limit=2",
                status,
            ),
            // NodeInfo foreign-host href -> UnsafeUrl
            json_exchange(
                "https://foreign.example.org/.well-known/nodeinfo",
                json!({"links": [{"rel": "http://nodeinfo.diaspora.software/ns/schema/2.1", "href": "https://evil.other.net/nodeinfo/2.1"}]}),
            ),
            // NodeInfo unsupported software "misskey" -> Upstream
            json_exchange(
                "https://misskey.example.org/.well-known/nodeinfo",
                json!({"links": [{"rel": "http://nodeinfo.diaspora.software/ns/schema/2.1", "href": "https://misskey.example.org/nodeinfo/2.1"}]}),
            ),
            json_exchange(
                "https://misskey.example.org/nodeinfo/2.1",
                json!({"version": "2.1", "software": {"name": "misskey", "version": "2026.1"}}),
            ),
        ],
    );
    let net = Arc::new(ReplayNet::from_fixtures(vec![f]).unwrap());
    let reg = build_registry(Arc::clone(&net));
    let adapter = reg.protocol("activitypub_fediverse").unwrap();

    let masto_cx = cx_with_dialect(&[("flavor", json!("mastodon"))]);
    // "#Rust async" -> /tag/Rust
    let r1 = adapter
        .call(
            "https://masto.example.org",
            "search",
            json!({"query": "#Rust async", "limit": 2}),
            &masto_cx,
        )
        .await
        .unwrap();
    assert_eq!(r1.items.len(), 1);

    // Unicode tag "#日本語"
    let r2 = adapter
        .call(
            "https://masto.example.org",
            "search",
            json!({"query": "#日本語", "limit": 2}),
            &masto_cx,
        )
        .await
        .unwrap();
    assert_eq!(r2.items.len(), 1);

    // "#" -> InvalidInput
    assert!(matches!(
        adapter
            .call(
                "https://masto.example.org",
                "search",
                json!({"query": "#", "limit": 2}),
                &masto_cx,
            )
            .await,
        Err(FsmError::InvalidInput(_))
    ));

    // NodeInfo rel 2.0 fallback + relative href
    let r3 = adapter
        .call(
            "https://rel20.example.org",
            "search",
            json!({"query": "rust", "limit": 2}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(r3.items.len(), 1);

    // Foreign-host NodeInfo href -> UnsafeUrl
    let err_foreign = adapter
        .call(
            "https://foreign.example.org",
            "nodeinfo",
            json!({}),
            &default_cx(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err_foreign, FsmError::UnsafeUrl(_)),
        "expected UnsafeUrl for cross-domain nodeinfo href, got {err_foreign:?}"
    );

    // Unsupported software ("misskey") -> Upstream
    let err_sw = adapter
        .call(
            "https://misskey.example.org",
            "search",
            json!({"query": "rust", "limit": 2}),
            &default_cx(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err_sw, FsmError::Upstream { ref message, .. } if message.contains("misskey")),
        "expected Upstream for misskey, got {err_sw:?}"
    );
}

/// Missing Test #10: Atom link selection ignores `rel="self"`/`"replies"` and falls back to http(s) `<id>`.
#[tokio::test]
async fn atom_link_selection_ignores_self_and_replies_and_falls_back_to_http_id() {
    let atom = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <entry>
    <title>Has Alternate</title>
    <link rel="self" href="https://blog.example.org/feed/entry1"/>
    <link rel="replies" href="https://blog.example.org/comments/1"/>
    <link rel="alternate" href="https://blog.example.org/posts/1"/>
    <id>urn:uuid:entry-1</id>
  </entry>
  <entry>
    <title>Fallback To Http Id</title>
    <link rel="self" href="https://blog.example.org/feed/entry2"/>
    <link rel="replies" href="https://blog.example.org/comments/2"/>
    <id>https://blog.example.org/posts/2</id>
  </entry>
  <entry>
    <title>Non-Http Id Only</title>
    <link rel="self" href="https://blog.example.org/feed/entry3"/>
    <id>urn:uuid:entry-3</id>
  </entry>
</feed>"#;
    let mut req = FixtureRequest::new("GET", "https://blog.example.org/atom.xml");
    req.headers_subset.insert(
        "accept".into(),
        "application/atom+xml, application/xml, text/xml".into(),
    );
    let f = make_fixture(
        "atom_links",
        "latest",
        vec![Exchange::new(
            req,
            FixtureResponse::from_bytes(
                200,
                vec![("content-type".into(), "application/atom+xml".into())],
                atom.as_bytes(),
            ),
        )],
    );
    let net = Arc::new(ReplayNet::from_fixtures(vec![f]).unwrap());
    let reg = build_registry(Arc::clone(&net));
    let out = reg
        .protocol("rss_atom")
        .unwrap()
        .call(
            "https://blog.example.org/atom.xml",
            "latest",
            json!({"limit": 5}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(out.items.len(), 3);
    assert_eq!(out.items[0]["url"], "https://blog.example.org/posts/1");
    assert_eq!(out.items[1]["url"], "https://blog.example.org/posts/2");
    assert_eq!(out.items[2]["url"], "");
}

/// Missing Test #11: Pin exact golden first-item values (title/url/date/snippet) on one real
/// staged fixture for every single adapter (plus both XML & llms.txt modes, Lemmy & Mastodon).
#[tokio::test]
async fn golden_first_item_values_across_all_nine_adapters() {
    // 1. CKAN (data_gov_ie)
    {
        let net =
            Arc::new(ReplayNet::from_file(fixture_path("w5f", "data_gov_ie", "search")).unwrap());
        let reg = build_registry(net);
        let out = reg
            .protocol("ckan")
            .unwrap()
            .call(
                "https://data.gov.ie",
                "search",
                json!({"query": "transport", "limit": 3}),
                &default_cx(),
            )
            .await
            .unwrap();
        assert_eq!(out.items[0]["title"], "Community Transport");
        assert_eq!(
            out.items[0]["url"],
            "https://data.gov.ie/dataset/community-transport1"
        );
        assert_eq!(
            out.items[0]["sourceUrl"],
            "https://data.smartdublin.ie/dataset/community-transport1"
        );
        assert_eq!(out.items[0]["date"], "2026-11-25");
        assert_eq!(out.items[0]["snippet"], "New Group Layer");
        assert_eq!(out.items[2]["title"], "ETA06 - Transport Taxes");
        assert_eq!(
            out.items[2]["url"],
            "https://data.gov.ie/dataset/eta06-transport-taxes"
        );
        assert_eq!(
            out.items[2]["sourceUrl"],
            "https://ws.cso.ie/public/api.restful/PxStat.Data.Cube_API.ReadDataset/ETA06/JSON-stat/2.0/en"
        );
    }

    // 2. Socrata (cdc_open_data)
    {
        let net =
            Arc::new(ReplayNet::from_file(fixture_path("w5f", "cdc_open_data", "search")).unwrap());
        let reg = build_registry(net);
        let out = reg
            .protocol("socrata")
            .unwrap()
            .call(
                "https://data.cdc.gov",
                "search",
                json!({"query": "influenza", "limit": 3}),
                &default_cx(),
            )
            .await
            .unwrap();
        assert_eq!(out.items[0]["id"], "ynw2-4viq");
        assert_eq!(
            out.items[0]["title"],
            "Provisional Death Counts for Influenza, Pneumonia, and COVID-19"
        );
        assert_eq!(out.items[0]["url"], "https://data.cdc.gov/d/ynw2-4viq");
        assert_eq!(out.items[0]["date"], "2026-09-22");
        assert!(!out.items[0]["snippet"].as_str().unwrap().is_empty());
    }

    // 3. OpenDataSoft (ods_public)
    {
        let net =
            Arc::new(ReplayNet::from_file(fixture_path("w5f", "ods_public", "search")).unwrap());
        let reg = build_registry(net);
        let out = reg
            .protocol("opendatasoft")
            .unwrap()
            .call(
                "https://public.opendatasoft.com",
                "search",
                json!({"query": "population", "limit": 2}),
                &default_cx(),
            )
            .await
            .unwrap();
        assert_eq!(
            out.items[0]["datasetId"],
            "population-francaise-donnees-carroyees-a-200-metres-2015"
        );
        assert_eq!(
            out.items[0]["title"],
            "Population française : Données Carroyées à 200 mètres - 2015"
        );
        assert_eq!(
            out.items[0]["url"],
            "https://public.opendatasoft.com/explore/dataset/population-francaise-donnees-carroyees-a-200-metres-2015/"
        );
        assert_eq!(out.items[0]["date"], "2018-03-30");
    }

    // 4. Discourse (rust_users)
    {
        let net =
            Arc::new(ReplayNet::from_file(fixture_path("w5g", "rust_users", "search")).unwrap());
        let reg = build_registry(net);
        let out = reg
            .protocol("discourse")
            .unwrap()
            .call(
                "https://users.rust-lang.org",
                "search",
                json!({"query": "tokio-console deadlock", "limit": 3}),
                &default_cx(),
            )
            .await
            .unwrap();
        assert_eq!(
            out.items[0]["title"],
            "Deadlock using Thrussh to build server"
        );
        assert_eq!(
            out.items[0]["url"],
            "https://users.rust-lang.org/t/deadlock-using-thrussh-to-build-server/70029/8"
        );
        assert_eq!(out.items[0]["date"], "2022-01-10");
        assert!(!out.items[0]["snippet"].as_str().unwrap().is_empty());
    }

    // 5. WordPress REST (quantamagazine)
    {
        let net = Arc::new(
            ReplayNet::from_file(fixture_path("w5g", "quantamagazine", "search")).unwrap(),
        );
        let reg = build_registry(net);
        let out = reg
            .protocol("wordpress_rest")
            .unwrap()
            .call(
                "https://www.quantamagazine.org",
                "search",
                json!({"query": "quantum", "limit": 3}),
                &default_cx(),
            )
            .await
            .unwrap();
        assert_eq!(
            out.items[0]["title"],
            "A Strange Quantum Theorem Has Killed Einstein\u{2019}s Reality"
        );
        assert_eq!(
            out.items[0]["url"],
            "https://www.quantamagazine.org/a-strange-quantum-theorem-has-killed-einsteins-reality-20261106/"
        );
        assert_eq!(out.items[0]["snippet"], "");
    }

    // 6. RSS/Atom (eli_bendersky_atom)
    {
        let net = Arc::new(
            ReplayNet::from_file(fixture_path("w5g", "eli_bendersky_atom", "search")).unwrap(),
        );
        let reg = build_registry(net);
        let out = reg
            .protocol("rss_atom")
            .unwrap()
            .call(
                "https://eli.thegreenplace.net/feeds/all.atom.xml",
                "search",
                json!({"limit": 3}),
                &default_cx(),
            )
            .await
            .unwrap();
        assert_eq!(
            out.items[0]["title"],
            "Notes on the discrete Fourier transform (DFT)"
        );
        assert_eq!(
            out.items[0]["url"],
            "https://eli.thegreenplace.net/2026/notes-on-the-discrete-fourier-transform-dft/"
        );
        assert_eq!(out.items[0]["date"], "2026-11-21");
        assert!(!out.items[0]["snippet"].as_str().unwrap().is_empty());
    }

    // 7a. ActivityPub Lemmy (lemmy_world)
    {
        let net =
            Arc::new(ReplayNet::from_file(fixture_path("w5g", "lemmy_world", "search")).unwrap());
        let reg = build_registry(net);
        let out = reg
            .protocol("activitypub_fediverse")
            .unwrap()
            .call(
                "https://lemmy.world",
                "search",
                json!({"query": "rust", "limit": 3}),
                &cx_with_dialect(&[("flavor", json!("lemmy"))]),
            )
            .await
            .unwrap();
        assert_eq!(out.items[0]["url"], "https://jlai.lu/post/46639822");
        assert!(!out.items[0]["title"].as_str().unwrap().is_empty());
        assert!(out.items[0]["date"].as_str().is_some());
    }

    // 7b. ActivityPub Mastodon (mastodon_social)
    {
        let net = Arc::new(
            ReplayNet::from_file(fixture_path("w5g", "mastodon_social", "search")).unwrap(),
        );
        let reg = build_registry(net);
        let out = reg
            .protocol("activitypub_fediverse")
            .unwrap()
            .call(
                "https://mastodon.social",
                "search",
                json!({"query": "#rust", "limit": 3}),
                &cx_with_dialect(&[("flavor", json!("mastodon"))]),
            )
            .await
            .unwrap();
        assert_eq!(
            out.items[0]["url"],
            "https://social.rcelko.ch/objects/d79302fc-943b-44e2-b531-e222af95aeae"
        );
        assert_eq!(out.items[0]["date"], "2026-11-25");
        assert!(!out.items[0]["snippet"].as_str().unwrap().is_empty());
    }

    // 8. Shopify JSON (arduino_store)
    {
        let net =
            Arc::new(ReplayNet::from_file(fixture_path("w5g", "arduino_store", "search")).unwrap());
        let reg = build_registry(net);
        let out = reg
            .protocol("shopify_json")
            .unwrap()
            .call(
                "https://store-usa.arduino.cc",
                "search",
                json!({"query": "uno", "limit": 3}),
                &cx_with_dialect(&[("currency", json!("USD"))]),
            )
            .await
            .unwrap();
        assert_eq!(out.items[0]["title"], "Arduino Uno Rev3");
        assert_eq!(
            out.items[0]["url"],
            "https://store-usa.arduino.cc/products/arduino-uno-rev3?_pos=1&_psq=uno&_psid=53eb9bdbb&_ss=e"
        );
        assert_eq!(out.items[0]["price"], "19.32");
        assert_eq!(out.items[0]["vendor"], "Arduino");
    }

    // 9a. Sitemap XML (owasp_cheatsheets)
    {
        let net = Arc::new(
            ReplayNet::from_file(fixture_path("w5g", "owasp_cheatsheets", "search")).unwrap(),
        );
        let reg = build_registry(net);
        let out = reg
            .protocol("sitemap_llms")
            .unwrap()
            .call(
                "https://cheatsheetseries.owasp.org/sitemap.xml",
                "search",
                json!({"query": "authentication", "limit": 5}),
                &default_cx(),
            )
            .await
            .unwrap();
        assert_eq!(out.items[0]["title"], "Authentication Cheat Sheet");
        assert_eq!(
            out.items[0]["url"],
            "https://cheatsheetseries.owasp.org/cheatsheets/Authentication_Cheat_Sheet.html"
        );
        assert_eq!(out.items[0]["date"], "2026-11-25");
        assert_eq!(out.items[0]["snippet"], "");
    }

    // 9b. llms.txt (mcp_llms)
    {
        let net =
            Arc::new(ReplayNet::from_file(fixture_path("w5g", "mcp_llms", "search")).unwrap());
        let reg = build_registry(net);
        let out = reg
            .protocol("sitemap_llms")
            .unwrap()
            .call(
                "https://modelcontextprotocol.io/llms.txt",
                "search",
                json!({"query": "tools", "limit": 5}),
                &default_cx(),
            )
            .await
            .unwrap();
        assert_eq!(out.items[0]["title"], "SEP-1577: Sampling With Tools");
        assert_eq!(
            out.items[0]["url"],
            "https://modelcontextprotocol.io/seps/1577--sampling-with-tools.md"
        );
        assert_eq!(out.items[0]["snippet"], "Sampling With Tools");
    }
}

/// Missing Test #12: `limit: 0` -> `InvalidInput`, `offset: -1` / `page: 0` -> `InvalidInput`,
/// and `limit` above declared max clamps to max across ALL 9 adapters.
#[tokio::test]
async fn limits_and_offsets_validated_and_clamped_across_all_nine_adapters() {
    let rss_body = r#"<?xml version="1.0"?><rss><channel><item><title>A</title><link>https://lim.example.org/1</link></item></channel></rss>"#;
    let sm_body =
        r#"<?xml version="1.0"?><urlset><url><loc>https://lim.example.org/p1</loc></url></urlset>"#;

    let mut rss_req = FixtureRequest::new("GET", "https://lim.example.org/feed.xml");
    rss_req.headers_subset.insert(
        "accept".into(),
        "application/rss+xml, application/xml, text/xml".into(),
    );
    let mut sm_req = FixtureRequest::new("GET", "https://lim.example.org/sitemap.xml");
    sm_req
        .headers_subset
        .insert("accept".into(), "application/xml, text/xml".into());

    // ReplayNet URL matching proves each adapter clamped `limit: 99999` to its exact declared max!
    let f = make_fixture(
        "limits",
        "search",
        vec![
            json_exchange(
                "https://lim.example.org/api/3/action/package_search?q=x&rows=50",
                json!({"success": true, "result": {"count": 0, "results": []}}),
            ),
            json_exchange(
                "https://lim.example.org/api/catalog/v1?q=x&limit=50&domains=lim.example.org",
                json!({"resultSetSize": 0, "results": []}),
            ),
            json_exchange(
                "https://lim.example.org/api/explore/v2.1/catalog/datasets?where=search%28%2A%2C%22x%22%29&limit=50",
                json!({"total_count": 0, "results": []}),
            ),
            json_exchange(
                "https://lim.example.org/search.json?q=x",
                json!({"topics": [], "posts": []}),
            ),
            json_exchange(
                "https://lim.example.org/wp-json/wp/v2/search?search=x&per_page=50",
                json!([]),
            ),
            Exchange::new(
                rss_req,
                FixtureResponse::from_bytes(
                    200,
                    vec![("content-type".into(), "application/rss+xml".into())],
                    rss_body.as_bytes(),
                ),
            ),
            json_exchange(
                "https://lim.example.org/api/v3/search?q=x&type_=Posts&limit=40",
                json!({"posts": [], "comments": [], "communities": []}),
            ),
            json_exchange(
                "https://lim.example.org/search/suggest.json?q=x&resources%5Btype%5D=product&resources%5Blimit%5D=10",
                json!({"resources": {"results": {"products": []}}}),
            ),
            Exchange::new(
                sm_req,
                FixtureResponse::from_bytes(
                    200,
                    vec![("content-type".into(), "application/xml".into())],
                    sm_body.as_bytes(),
                ),
            ),
        ],
    );
    let net = Arc::new(ReplayNet::from_fixtures(vec![f]).unwrap());
    let reg = build_registry(Arc::clone(&net));

    let lemmy_cx = cx_with_dialect(&[("flavor", json!("lemmy"))]);
    let all_nine: [(&str, &str, &str, SrcCx); 9] = [
        ("ckan", "https://lim.example.org", "search", default_cx()),
        ("socrata", "https://lim.example.org", "search", default_cx()),
        (
            "opendatasoft",
            "https://lim.example.org",
            "search",
            default_cx(),
        ),
        (
            "discourse",
            "https://lim.example.org",
            "search",
            default_cx(),
        ),
        (
            "wordpress_rest",
            "https://lim.example.org",
            "search",
            default_cx(),
        ),
        (
            "rss_atom",
            "https://lim.example.org/feed.xml",
            "search",
            default_cx(),
        ),
        (
            "activitypub_fediverse",
            "https://lim.example.org",
            "search",
            lemmy_cx,
        ),
        (
            "shopify_json",
            "https://lim.example.org",
            "search",
            default_cx(),
        ),
        (
            "sitemap_llms",
            "https://lim.example.org/sitemap.xml",
            "search",
            default_cx(),
        ),
    ];

    for (proto, url, op, cx) in &all_nine {
        let adapter = reg.protocol(proto).unwrap();
        // limit: 0 -> InvalidInput
        assert!(
            matches!(
                adapter
                    .call(url, op, json!({"query": "x", "limit": 0}), cx)
                    .await,
                Err(FsmError::InvalidInput(_))
            ),
            "{proto} limit:0 must fail"
        );
        // limit: -3 -> InvalidInput
        assert!(
            matches!(
                adapter
                    .call(url, op, json!({"query": "x", "limit": -3}), cx)
                    .await,
                Err(FsmError::InvalidInput(_))
            ),
            "{proto} limit:-3 must fail"
        );
        // limit: 99999 -> clamps to max and matches ReplayNet URL!
        adapter
            .call(url, op, json!({"query": "x", "limit": 99_999}), cx)
            .await
            .unwrap_or_else(|e| panic!("{proto} limit clamp failed: {e}"));
    }

    // offset: -1 -> InvalidInput across all offset-bearing ops
    for (proto, op, params) in [
        ("ckan", "search", json!({"query": "x", "offset": -1})),
        ("socrata", "search", json!({"query": "x", "offset": -1})),
        (
            "socrata",
            "rows",
            json!({"dataset_id": "abcd-1234", "offset": -1}),
        ),
        (
            "opendatasoft",
            "search",
            json!({"query": "x", "offset": -1}),
        ),
        (
            "opendatasoft",
            "records",
            json!({"dataset_id": "ds1", "offset": -1}),
        ),
    ] {
        assert!(
            matches!(
                reg.protocol(proto)
                    .unwrap()
                    .call("https://lim.example.org", op, params, &default_cx())
                    .await,
                Err(FsmError::InvalidInput(_))
            ),
            "{proto}.{op} offset:-1 must fail"
        );
    }

    // page: 0 -> InvalidInput across all page-bearing ops
    for (proto, op, params) in [
        ("discourse", "search", json!({"query": "x", "page": 0})),
        ("wordpress_rest", "search", json!({"query": "x", "page": 0})),
        ("shopify_json", "products", json!({"page": 0})),
    ] {
        assert!(
            matches!(
                reg.protocol(proto)
                    .unwrap()
                    .call("https://lim.example.org", op, params, &default_cx())
                    .await,
                Err(FsmError::InvalidInput(_))
            ),
            "{proto}.{op} page:0 must fail"
        );
    }

    assert!(net.unmatched().is_empty());
}

// ---------------------------------------------------------------------------
// Review Round 2 regression tests (N1, N3, N4; N2 is verified in i6_... above)
// ---------------------------------------------------------------------------

struct RecordingNet {
    recorded_source_ids: Mutex<Vec<Option<String>>>,
}

#[async_trait::async_trait]
impl NetClient for RecordingNet {
    async fn send(
        &self,
        req: HttpRequest,
        _profile: TransportProfile,
        _cx: &NetCx,
    ) -> Result<HttpBytes, FsmError> {
        self.recorded_source_ids
            .lock()
            .unwrap()
            .push(req.source_id.clone());
        Ok(HttpBytes::new(
            req.url,
            200,
            vec![("content-type".into(), "application/json".into())],
            Bytes::from_static(br#"{"success":true,"result":{"count":0,"results":[]}}"#),
        ))
    }

    fn check_url_offline(&self, _url: &str) -> Result<(), FsmError> {
        Ok(())
    }
}

async fn spawn_local_http(
    response: String,
    hits: Arc<AtomicUsize>,
) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            hits.fetch_add(1, Ordering::SeqCst);
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    });
    (port, handle)
}

/// N1: `send_request` must NOT populate `req.source_id` with protocol `kind`, so a 429 +
/// `Retry-After` from one portal does NOT penalize/block a healthy sibling portal using the
/// same protocol adapter through real `fsm_net::HttpClient`.
#[tokio::test]
async fn n1_rate_limit_on_one_portal_does_not_block_sibling_portal_and_keeps_source_id_none() {
    // 1. Recording stub verifies `req.source_id` stays `None`
    let rec = Arc::new(RecordingNet {
        recorded_source_ids: Mutex::new(Vec::new()),
    });
    let deps_rec = SourceDeps::new(
        Arc::clone(&rec) as Arc<dyn NetClient>,
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    );
    let mut reg_rec = Registry::new();
    fsm_protocols_b::register(&mut reg_rec, &deps_rec).unwrap();
    reg_rec
        .protocol("ckan")
        .unwrap()
        .call(
            "https://portal.example.org",
            "search",
            json!({"query": "x"}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(
        *rec.recorded_source_ids.lock().unwrap(),
        vec![None],
        "send_request must leave req.source_id as None"
    );

    // 2. Real fsm_net::HttpClient + two local portals (A returns 429 + Retry-After: 120; B returns 200)
    let hits_a = Arc::new(AtomicUsize::new(0));
    let hits_b = Arc::new(AtomicUsize::new(0));
    let resp_a = "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 120\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbusy".to_owned();
    let body_b = r#"{"success":true,"result":{"count":1,"results":[{"id":"1","name":"ok-ds","title":"Healthy Portal B"}]}}"#;
    let resp_b = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body_b}",
        body_b.len()
    );
    let (port_a, task_a) = spawn_local_http(resp_a, Arc::clone(&hits_a)).await;
    let (port_b, task_b) = spawn_local_http(resp_b, Arc::clone(&hits_b)).await;

    let mut cfg = Config::default();
    cfg.allow_private_hosts = true;
    let dns = StaticDns::new()
        .with("portal-a.test", &[IpAddr::V4(Ipv4Addr::LOCALHOST)])
        .with("portal-b.test", &[IpAddr::V4(Ipv4Addr::LOCALHOST)]);
    let client = Arc::new(
        HttpClient::builder(&cfg)
            .dns(Arc::new(dns))
            .build()
            .expect("build HttpClient"),
    );
    let deps = SourceDeps::new(client, Arc::new(NullStore::new()), Arc::new(cfg));
    let mut reg = Registry::new();
    fsm_protocols_b::register(&mut reg, &deps).unwrap();
    let ckan = reg.protocol("ckan").unwrap();

    // Portal A -> 429 RateLimited
    let err_a = ckan
        .call(
            &format!("http://portal-a.test:{port_a}"),
            "search",
            json!({"query": "climate", "limit": 1}),
            &default_cx(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err_a, FsmError::RateLimited { retry_after_s: Some(s), .. } if s >= 115),
        "expected RateLimited(~120s) from portal A, got {err_a:?}"
    );
    assert_eq!(hits_a.load(Ordering::SeqCst), 1);

    // Portal B (same ckan adapter, different host) MUST succeed!
    let out_b = ckan
        .call(
            &format!("http://portal-b.test:{port_b}"),
            "search",
            json!({"query": "climate", "limit": 1}),
            &default_cx(),
        )
        .await
        .expect("healthy portal B must not be blocked by portal A's 429");
    assert_eq!(out_b.items.len(), 1);
    assert_eq!(out_b.items[0]["title"], "Healthy Portal B");
    assert_eq!(hits_b.load(Ordering::SeqCst), 1);

    // Second request to Portal A is still rate-limited locally without dialing A again
    let err_a2 = ckan
        .call(
            &format!("http://portal-a.test:{port_a}"),
            "search",
            json!({"query": "climate", "limit": 1}),
            &default_cx(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err_a2, FsmError::RateLimited { .. }),
        "portal A should remain penalized locally, got {err_a2:?}"
    );
    assert_eq!(hits_a.load(Ordering::SeqCst), 1);

    task_a.abort();
    task_b.abort();
}

/// N3: RSS/Atom plain-text titles decode double-escaped HTML entities (`&quot;`, `&#039;`,
/// `&#39;`, `&nbsp;`) without breaking I5 preservation of `<T>` generics.
#[tokio::test]
async fn n3_rss_atom_decodes_double_escaped_entities_without_stripping_generics() {
    // 1. Real fixtures where round-1 leaked raw entities
    let real_cases = [
        (
            "w5g",
            "nist_news_rss",
            "https://www.nist.gov/news-events/news/rss.xml",
            false,
            "\"AI Agent Standards Initiative\"",
        ),
        (
            "w5p",
            "esa_science_rss",
            "https://www.esa.int/rss/latest-news-homepage.rss",
            true,
            "El Niño's",
        ),
        (
            "w5p",
            "mpg_research_rss",
            "https://www.mpg.de/en/research.rss",
            true,
            "Borneo's",
        ),
    ];
    for (wave, id, url, clear_accept, expected_substr) in real_cases {
        let raw = std::fs::read_to_string(fixture_path(wave, id, "search")).unwrap();
        let mut fixture = Fixture::from_json(&raw).unwrap();
        if clear_accept {
            fixture.exchanges[0].request.headers_subset.clear();
        }
        let net = Arc::new(ReplayNet::from_fixtures(vec![fixture]).unwrap());
        let reg = build_registry(net);
        let out = reg
            .protocol("rss_atom")
            .unwrap()
            .call(url, "latest", json!({"limit": 50}), &default_cx())
            .await
            .unwrap();
        let titles: Vec<&str> = out
            .items
            .iter()
            .filter_map(|i| i["title"].as_str())
            .collect();
        for t in &titles {
            for raw_ent in ["&quot;", "&#039;", "&#39;", "&nbsp;"] {
                assert!(
                    !t.contains(raw_ent),
                    "{wave}/{id} leaked raw entity {raw_ent} in title {t:?}"
                );
            }
        }
        assert!(
            titles.iter().any(|t| t.contains(expected_substr)),
            "{wave}/{id} expected decoded title containing {expected_substr:?}, got {titles:?}"
        );
    }

    // 2. Synthetic mixed feed: double-escaped entities AND `<dyn Error>` generics coexist cleanly
    let rss = r#"<?xml version="1.0"?><rss version="2.0"><channel><item>
        <title>Announcing &amp;quot;Box&lt;dyn Error&gt;&amp;quot; &amp;#039;v2&amp;#39;&amp;nbsp;</title>
        <link>https://ent.example.org/1</link>
    </item></channel></rss>"#;
    let atom = r#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom"><entry>
        <title type="text">El Ni&#241;o&amp;#039;s &amp;quot;Vec&lt;T&gt;&amp;quot;</title>
        <link href="https://ent.example.org/2"/>
    </entry></feed>"#;

    let mut rss_req = FixtureRequest::new("GET", "https://ent.example.org/rss.xml");
    rss_req.headers_subset.insert(
        "accept".into(),
        "application/rss+xml, application/xml, text/xml".into(),
    );
    let mut atom_req = FixtureRequest::new("GET", "https://ent.example.org/atom.xml");
    atom_req.headers_subset.insert(
        "accept".into(),
        "application/atom+xml, application/xml, text/xml".into(),
    );
    let f = make_fixture(
        "n3",
        "latest",
        vec![
            Exchange::new(
                rss_req,
                FixtureResponse::from_bytes(
                    200,
                    vec![("content-type".into(), "application/rss+xml".into())],
                    rss.as_bytes(),
                ),
            ),
            Exchange::new(
                atom_req,
                FixtureResponse::from_bytes(
                    200,
                    vec![("content-type".into(), "application/atom+xml".into())],
                    atom.as_bytes(),
                ),
            ),
        ],
    );
    let net = Arc::new(ReplayNet::from_fixtures(vec![f]).unwrap());
    let reg = build_registry(net);
    let adapter = reg.protocol("rss_atom").unwrap();

    let o1 = adapter
        .call(
            "https://ent.example.org/rss.xml",
            "latest",
            json!({"limit": 5}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(o1.items[0]["title"], "Announcing \"Box<dyn Error>\" 'v2'");

    let o2 = adapter
        .call(
            "https://ent.example.org/atom.xml",
            "latest",
            json!({"limit": 5}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(o2.items[0]["title"], "El Niño's \"Vec<T>\"");
}

/// N4: UTF-8 XML feeds/sitemaps declaring `encoding="utf-16"` (classic .NET `StringWriter`
/// pitfall) must decode as UTF-8 and parse cleanly when no HTTP header charset is sent.
#[tokio::test]
async fn n4_utf8_feed_declaring_utf16_prolog_parses_cleanly() {
    let rss_u16 = r#"<?xml version="1.0" encoding="utf-16"?><rss version="2.0"><channel><item><title>DotNet Feed OK</title><link>https://dotnet.example.org/1</link></item></channel></rss>"#;
    let rss_u16be = r#"<?xml version="1.0" encoding="UTF-16BE"?><rss version="2.0"><channel><item><title>DotNet BE OK</title><link>https://dotnet.example.org/2</link></item></channel></rss>"#;
    let sm_u16 = r#"<?xml version="1.0" encoding="utf-16"?><urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9"><url><loc>https://dotnet.example.org/docs/intro</loc></url></urlset>"#;

    let make_xml_ex = |url: &str, accept: &str, ct: &str, body: &str| {
        let mut req = FixtureRequest::new("GET", url);
        req.headers_subset.insert("accept".into(), accept.into());
        Exchange::new(
            req,
            FixtureResponse::from_bytes(
                200,
                vec![("content-type".into(), ct.into())],
                body.as_bytes(),
            ),
        )
    };
    let rss_accept = "application/rss+xml, application/xml, text/xml";
    let sm_accept = "application/xml, text/xml";

    let f = make_fixture(
        "n4",
        "latest",
        vec![
            make_xml_ex(
                "https://dotnet.example.org/feed.xml",
                rss_accept,
                "application/rss+xml",
                rss_u16,
            ),
            make_xml_ex(
                "https://dotnet.example.org/feed-be.xml",
                rss_accept,
                "application/rss+xml",
                rss_u16be,
            ),
            make_xml_ex(
                "https://dotnet.example.org/sitemap.xml",
                sm_accept,
                "application/xml",
                sm_u16,
            ),
        ],
    );
    let net = Arc::new(ReplayNet::from_fixtures(vec![f]).unwrap());
    let reg = build_registry(net);

    let o1 = reg
        .protocol("rss_atom")
        .unwrap()
        .call(
            "https://dotnet.example.org/feed.xml",
            "latest",
            json!({"limit": 5}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(o1.items[0]["title"], "DotNet Feed OK");

    let o2 = reg
        .protocol("rss_atom")
        .unwrap()
        .call(
            "https://dotnet.example.org/feed-be.xml",
            "latest",
            json!({"limit": 5}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(o2.items[0]["title"], "DotNet BE OK");

    let o3 = reg
        .protocol("sitemap_llms")
        .unwrap()
        .call(
            "https://dotnet.example.org/sitemap.xml",
            "list",
            json!({"limit": 5}),
            &default_cx(),
        )
        .await
        .unwrap();
    assert_eq!(o3.items[0]["url"], "https://dotnet.example.org/docs/intro");
}
