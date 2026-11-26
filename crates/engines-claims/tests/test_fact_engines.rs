//! Port of applicable tests from `tests/test_fact_engines.py` and
//! `tests/test_finance_engines.py` for the 14 engines in `fsm-engines-claims`.

use fsm_core::fixture::{
    Exchange, Fixture, FixtureRequest, FixtureResponse, ReplayNet, canonical_json, sha256_hex,
};
use fsm_core::{
    AuthMode, Availability, Config, FsmError, NullStore, PoolRole, RegisterFn, Registry, Source,
    SourceDeps, SrcCx, SrcOutput, TransportProfile,
};
use fsm_engines_claims::crates_io::CratesEngine;
use fsm_engines_claims::endoflife::{EndOfLifeEngine, build_index, match_products};
use fsm_engines_claims::facts::{candidate_names, compact_number, iso_day};
use fsm_engines_claims::frankfurter::{FrankfurterEngine, parse_pair};
use fsm_engines_claims::imf::ImfEngine;
use fsm_engines_claims::npm::NpmEngine;
use fsm_engines_claims::osv::{OsvEngine, canonical_id, ecosystems_for};
use fsm_engines_claims::pypi::PypiEngine;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn empty_deps() -> SourceDeps {
    let net = Arc::new(ReplayNet::from_fixtures(vec![]).unwrap());
    SourceDeps::new(net, Arc::new(NullStore::new()), Arc::new(Config::default()))
}

fn replay_deps(exchanges: Vec<Exchange>) -> (Arc<ReplayNet>, SourceDeps) {
    let mut f = Fixture::new("test", "search", "2026-11-25T10:00:00Z");
    f.exchanges = exchanges;
    let net = Arc::new(ReplayNet::from_fixtures(vec![f]).unwrap());
    let deps = SourceDeps::new(
        net.clone(),
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    );
    (net, deps)
}

fn get_ex(url: &str, status: u16, body: &Value) -> Exchange {
    Exchange::new(
        FixtureRequest::new("GET", url),
        FixtureResponse::from_bytes(
            status,
            vec![("content-type".into(), "application/json".into())],
            body.to_string().as_bytes(),
        ),
    )
}

fn post_ex(url: &str, req_body: &Value, resp_body: &Value) -> Exchange {
    let mut req = FixtureRequest::new("POST", url);
    let wire = canonical_json(req_body);
    req.body_sha256 = Some(sha256_hex(wire.as_bytes()));
    Exchange::new(
        req,
        FixtureResponse::from_bytes(
            200,
            vec![("content-type".into(), "application/json".into())],
            resp_body.to_string().as_bytes(),
        ),
    )
}

fn default_cx() -> SrcCx {
    SrcCx::new(CancellationToken::new(), Duration::from_secs(5))
}

// ---------------------------------------------------------------------------
// Registry contract & M16 scope declarations
// ---------------------------------------------------------------------------

#[test]
fn test_registered_all_14_engines_with_valid_scopes_and_roles() {
    // Verify `register` matches the frozen `RegisterFn` signature verbatim.
    let reg_fn: RegisterFn = fsm_engines_claims::register;
    let deps = empty_deps();
    let cfg = Config::default();
    let mut reg = Registry::new();
    reg_fn(&mut reg, &deps).unwrap();

    assert_eq!(reg.len(), 14);
    let ids: Vec<&str> = reg.all().map(|s| s.decl().id.as_str()).collect();
    assert_eq!(ids, fsm_engines_claims::ENGINE_IDS);

    let expected_scopes: &[(&str, &str, PoolRole)] = &[
        ("imf", "finance", PoolRole::ScopeOnly),
        ("endoflife", "code", PoolRole::DirectClaimant),
        ("pypi", "code", PoolRole::DirectClaimant),
        ("npm", "code", PoolRole::DirectClaimant),
        ("crates", "code", PoolRole::DirectClaimant),
        ("appstore", "code", PoolRole::DirectClaimant),
        ("osv", "security", PoolRole::DirectClaimant),
        ("cisakev", "security", PoolRole::DirectClaimant),
        ("wdi", "reference", PoolRole::DirectClaimant),
        ("holidays", "reference", PoolRole::DirectClaimant),
        ("frankfurter", "finance", PoolRole::DirectClaimant),
        ("cfets", "finance", PoolRole::DirectClaimant),
        ("gleif", "finance", PoolRole::DirectClaimant),
        ("coingecko", "finance", PoolRole::DirectClaimant),
    ];

    for (id, scope, role) in expected_scopes {
        let src = reg.get(id).expect("registered");
        let decl = src.decl();
        assert!(decl.scopes.iter().any(|s| s == scope), "{id} scope {scope}");
        assert_eq!(decl.pool_role, *role, "{id} pool_role");
        let expected_transport = if *id == "imf" {
            TransportProfile::Chrome
        } else {
            TransportProfile::Plain
        };
        assert_eq!(decl.transport, expected_transport, "{id} transport");
        assert_eq!(decl.auth, AuthMode::None, "{id} auth");
        assert_eq!(
            src.availability(&cfg),
            Availability::Available,
            "{id} avail"
        );
        assert!(decl.op("search").is_some(), "{id} has op.search");
        assert!(reg.resolve_op(&format!("{id}.search")).is_ok());
        assert!(reg.resolve_op(&format!("{id}.nonexistent")).is_err());
    }

    // M16: `pypi`, `npm`, `crates` are returned by `Registry::by_scope("code")`
    // and by the legacy synonym `"software.rust"` / `"software.python"` / `"software.node"`.
    let code_ids: Vec<String> = reg
        .by_scope("code")
        .into_iter()
        .map(|s| s.decl().id.clone())
        .collect();
    for must in ["pypi", "npm", "crates", "endoflife", "appstore"] {
        assert!(
            code_ids.iter().any(|id| id == must),
            "code scope missing {must}"
        );
    }
    assert_eq!(
        reg.by_scope("software.rust")
            .into_iter()
            .map(|s| s.decl().id.clone())
            .collect::<Vec<_>>(),
        code_ids
    );
}

#[tokio::test]
async fn test_unknown_op_or_missing_query_returns_invalid_input() {
    let engine = CratesEngine::new(empty_deps());
    let cx = default_cx();
    let err = engine.call("bogus", json!({"query": "tokio"}), &cx).await;
    assert!(matches!(err, Err(FsmError::InvalidInput(_))));
    let err2 = engine.call("search", json!({}), &cx).await;
    assert!(matches!(err2, Err(FsmError::InvalidInput(_))));
}

// ---------------------------------------------------------------------------
// Shared query helpers
// ---------------------------------------------------------------------------

#[test]
fn test_candidate_names_drop_question_words_and_keep_scoped_names() {
    assert_eq!(
        candidate_names("latest fastapi version", 3),
        vec!["fastapi"]
    );
    assert_eq!(candidate_names("requests 库最新版本", 3), vec!["requests"]);
    assert_eq!(
        candidate_names("@types/node and astral-sh/uv release", 3),
        vec!["@types/node", "astral-sh/uv"]
    );
    assert!(candidate_names("is python 3.9 still supported", 3).is_empty());
    assert_eq!(
        candidate_names("requests 2.32 changelog", 3),
        vec!["requests"]
    );
    assert!(candidate_names("what is the latest version", 3).is_empty());
    assert!(candidate_names("What year was Rust 1.0 released?", 3).is_empty());
}

#[test]
fn test_iso_day_and_compact_number() {
    assert_eq!(
        iso_day(Some(&json!("2026-07-29T14:02:11.123456Z"))),
        "2026-07-29"
    );
    assert_eq!(iso_day(Some(&json!(1_700_000_000))), "2023-11-14");
    assert_eq!(iso_day(None), "");
    assert_eq!(compact_number("24870895"), "24,870,895");
    assert_eq!(compact_number("6341"), "6,341");
}

// ---------------------------------------------------------------------------
// PyPI
// ---------------------------------------------------------------------------

fn pypi_fastapi_payload() -> Value {
    json!({
        "info": {
            "name": "fastapi",
            "version": "0.141.1",
            "summary": "FastAPI framework, high performance, easy to learn",
            "requires_python": ">=3.10",
            "project_urls": {"Homepage": "https://github.com/fastapi/fastapi"}
        },
        "urls": [{"upload_time_iso_8601": "2026-07-29T10:11:12.000000Z"}]
    })
}

#[test]
fn test_pypi_maps_the_current_release_with_its_upload_date() {
    let hits = PypiEngine::new(empty_deps()).map_results(&pypi_fastapi_payload());
    assert_eq!(hits.len(), 1);
    let r = &hits[0];
    assert_eq!(r.title, "fastapi 0.141.1 on PyPI");
    assert_eq!(r.url, "https://pypi.org/project/fastapi/0.141.1/");
    assert!(
        r.snippet
            .contains("Latest release 0.141.1, uploaded 2026-07-29")
    );
    assert!(r.snippet.contains("requires Python >=3.10"));
    assert_eq!(r.date.as_deref(), Some("2026-07-29"));
    assert_eq!(r.date_source.as_deref(), Some("api"));
}

#[tokio::test]
async fn test_pypi_asks_for_each_candidate_name_and_skips_404s() {
    let (net, deps) = replay_deps(vec![
        get_ex(
            "https://pypi.org/pypi/fastapi/json",
            200,
            &pypi_fastapi_payload(),
        ),
        get_ex(
            "https://pypi.org/pypi/vs/json",
            404,
            &json!({"message": "Not Found"}),
        ),
        get_ex(
            "https://pypi.org/pypi/starlette/json",
            404,
            &json!({"message": "Not Found"}),
        ),
    ]);
    let engine = PypiEngine::new(deps);
    let SrcOutput::Hits(out) = engine
        .call(
            "search",
            json!({"query": "fastapi vs starlette latest version", "limit": 5}),
            &default_cx(),
        )
        .await
        .unwrap()
    else {
        panic!("expected Hits");
    };
    let asked: Vec<String> = net.hits().into_iter().map(|h| h.url).collect();
    assert_eq!(
        asked,
        vec![
            "https://pypi.org/pypi/fastapi/json",
            "https://pypi.org/pypi/vs/json",
            "https://pypi.org/pypi/starlette/json",
        ]
    );
    assert_eq!(
        out.iter().map(|r| r.title.as_str()).collect::<Vec<_>>(),
        vec!["fastapi 0.141.1 on PyPI"]
    );
}

#[test]
fn test_pypi_rejects_payloads_without_a_version() {
    let engine = PypiEngine::new(empty_deps());
    assert!(
        engine
            .map_results(&json!({"info": {"name": "x"}}))
            .is_empty()
    );
    assert!(
        engine
            .map_results(&json!({"message": "Not Found"}))
            .is_empty()
    );
    assert!(engine.map_results(&Value::Null).is_empty());
}

// ---------------------------------------------------------------------------
// npm and crates.io
// ---------------------------------------------------------------------------

#[test]
fn test_npm_searches_for_the_package_name_not_the_question() {
    let url = NpmEngine::build_url("latest express version", 5);
    assert!(url.contains("text=express&") && !url.contains("latest"));
}

#[test]
fn test_npm_maps_search_objects() {
    let payload = json!({
        "objects": [
            {
                "package": {
                    "name": "express",
                    "version": "5.2.1",
                    "date": "2025-12-01T20:00:00.000Z",
                    "description": "Fast, unopinionated, minimalist web framework",
                    "links": {"homepage": "https://expressjs.com/"}
                }
            },
            {"package": {"name": "broken"}}
        ]
    });
    let hits = NpmEngine::new(empty_deps()).map_results(&payload);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].url, "https://www.npmjs.com/package/express/v/5.2.1");
    assert!(
        hits[0]
            .snippet
            .contains("Version 5.2.1, published 2025-12-01")
    );
    assert_eq!(hits[0].date.as_deref(), Some("2025-12-01"));
}

#[test]
fn test_crates_uses_the_stable_version_and_an_honest_user_agent() {
    let engine = CratesEngine::new(empty_deps());
    assert_eq!(engine.decl().transport, TransportProfile::Plain);
    let payload = json!({
        "crates": [
            {
                "name": "tokio",
                "max_stable_version": "1.53.1",
                "newest_version": "1.54.0-alpha.1",
                "updated_at": "2026-07-20T09:00:00.000000+00:00",
                "description": "An event-driven, non-blocking I/O platform",
                "repository": "https://github.com/tokio-rs/tokio"
            }
        ]
    });
    let hits = engine.map_results(&payload);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].title, "tokio 1.53.1 on crates.io");
    assert_eq!(hits[0].url, "https://crates.io/crates/tokio/1.53.1");
    assert_eq!(hits[0].date.as_deref(), Some("2026-07-20"));
}

// ---------------------------------------------------------------------------
// endoflife.date
// ---------------------------------------------------------------------------

fn eol_products() -> Value {
    json!({
        "result": [
            {"name": "python", "label": "Python", "aliases": []},
            {"name": "ubuntu", "label": "Ubuntu", "aliases": ["ubuntu-linux"]},
            {"name": "redhat", "label": "Red Hat Enterprise Linux", "aliases": ["rhel", "red hat"]},
            {"name": "go", "label": "Go", "aliases": ["golang"]}
        ]
    })
}

fn eol_python() -> Value {
    json!({
        "result": {
            "name": "python",
            "label": "Python",
            "releases": [
                {
                    "name": "3.14",
                    "isEol": false,
                    "eolFrom": "2030-10-31",
                    "latest": {"name": "3.14.7", "date": "2026-08-05"}
                },
                {"name": "3.13", "isEol": false, "eolFrom": "2029-10-31"},
                {"name": "3.12", "isEol": false, "eolFrom": "2028-10-31"},
                {"name": "3.11", "isEol": false, "eolFrom": "2027-10-31"},
                {"name": "3.10", "isEol": false, "eolFrom": "2026-10-31"},
                {"name": "3.9", "isEol": true, "eolFrom": "2025-10-31"}
            ]
        }
    })
}

#[test]
fn test_endoflife_index_maps_every_spelling_to_the_api_name() {
    let prods = eol_products();
    let index = build_index(prods["result"].as_array().unwrap());
    assert_eq!(index.get("python").map(String::as_str), Some("python"));
    assert_eq!(index.get("rhel").map(String::as_str), Some("redhat"));
    assert_eq!(index.get("redhat").map(String::as_str), Some("redhat"));
    assert_eq!(index.get("golang").map(String::as_str), Some("go"));
}

#[test]
fn test_endoflife_matches_products_named_in_the_question_including_go_and_python() {
    let prods = eol_products();
    let index = build_index(prods["result"].as_array().unwrap());
    assert_eq!(
        match_products("is python 3.9 still supported", &index),
        vec!["python"]
    );
    assert_eq!(
        match_products("red hat 9 end of life", &index),
        vec!["redhat"]
    );
    assert_eq!(
        match_products("when does go 1.22 stop getting fixes", &index),
        vec!["go"]
    );
    assert!(match_products("latest kubernetes release", &index).is_empty());
}

#[tokio::test]
async fn test_endoflife_puts_the_asked_cycle_first_and_dates_the_result() {
    let (_net, deps) = replay_deps(vec![
        get_ex(
            "https://endoflife.date/api/v1/products",
            200,
            &eol_products(),
        ),
        get_ex(
            "https://endoflife.date/api/v1/products/python",
            200,
            &eol_python(),
        ),
    ]);
    let engine = EndOfLifeEngine::new(deps);
    let SrcOutput::Hits(hits) = engine
        .call(
            "search",
            json!({"query": "is python 3.9 still supported", "limit": 5}),
            &default_cx(),
        )
        .await
        .unwrap()
    else {
        panic!("expected Hits");
    };
    assert_eq!(hits.len(), 1);
    let r = &hits[0];
    assert_eq!(r.url, "https://endoflife.date/python");
    assert!(
        r.snippet
            .starts_with("Latest release 3.14.7 on 2026-08-05 (cycle 3.14)")
    );
    assert!(
        r.snippet.contains(
            "Cycles: 3.9: end of life since 2025-10-31; 3.14: supported until 2030-10-31"
        )
    );
    assert_eq!(r.date.as_deref(), Some("2026-08-05"));
    assert_eq!(r.date_source.as_deref(), Some("api"));
}

// ---------------------------------------------------------------------------
// OSV
// ---------------------------------------------------------------------------

fn osv_vuln() -> Value {
    json!({
        "id": "PYSEC-2026-1872",
        "summary": "Requests vulnerable to .netrc credentials leak via malicious URLs",
        "aliases": ["CVE-2024-47081", "GHSA-9hjg-9r4m-mvj7"],
        "published": "2026-07-07T00:00:00Z",
        "affected": [
            {
                "package": {"ecosystem": "PyPI", "name": "requests", "purl": "pkg:pypi/requests"},
                "ranges": [{"type": "ECOSYSTEM", "events": [{"introduced": "0"}, {"fixed": "2.32.4"}]}]
            }
        ]
    })
}

#[test]
fn test_osv_guesses_the_ecosystem_and_keeps_ghsa_ids_case_sensitive() {
    assert_eq!(ecosystems_for("requests pip vulnerabilities"), vec!["pypi"]);
    assert_eq!(ecosystems_for("lodash npm"), vec!["npm"]);
    assert_eq!(ecosystems_for("tokio"), vec!["pypi", "npm"]);
    assert_eq!(canonical_id("ghsa-9wx4-h78v-vm56"), "GHSA-9wx4-h78v-vm56");
    assert_eq!(canonical_id("cve-2024-3094"), "CVE-2024-3094");
}

#[tokio::test]
async fn test_osv_posts_a_purl_per_ecosystem_and_leads_with_the_newest_advisory() {
    let mut older = osv_vuln();
    older["id"] = json!("PYSEC-2020-1");
    older["published"] = json!("2020-01-01T00:00:00Z");

    let (net, deps) = replay_deps(vec![
        post_ex(
            "https://api.osv.dev/v1/query",
            &json!({"package": {"purl": "pkg:pypi/requests"}}),
            &json!({"vulns": [older, osv_vuln()]}),
        ),
        post_ex(
            "https://api.osv.dev/v1/query",
            &json!({"package": {"purl": "pkg:npm/requests"}}),
            &json!({}),
        ),
    ]);
    let engine = OsvEngine::new(deps);
    let SrcOutput::Hits(out) = engine
        .call(
            "search",
            json!({"query": "requests vulnerabilities", "limit": 5}),
            &default_cx(),
        )
        .await
        .unwrap()
    else {
        panic!("expected Hits");
    };
    assert_eq!(net.hits().len(), 2);
    assert!(net.unmatched().is_empty());
    assert_eq!(
        out.iter()
            .map(|r| r.title.split(':').next().unwrap())
            .collect::<Vec<_>>(),
        vec!["PYSEC-2026-1872", "PYSEC-2020-1"]
    );
    let top = &out[0];
    assert_eq!(top.url, "https://osv.dev/vulnerability/PYSEC-2026-1872");
    assert!(top.snippet.contains("PyPI requests fixed in 2.32.4"));
    assert!(
        top.snippet
            .contains("also CVE-2024-47081, GHSA-9hjg-9r4m-mvj7")
    );
    assert_eq!(top.date.as_deref(), Some("2026-07-07"));
}

#[tokio::test]
async fn test_osv_looks_an_advisory_id_up_directly() {
    let (net, deps) = replay_deps(vec![get_ex(
        "https://api.osv.dev/v1/vulns/GHSA-9hjg-9r4m-mvj7",
        200,
        &osv_vuln(),
    )]);
    let engine = OsvEngine::new(deps);
    let SrcOutput::Hits(hits) = engine
        .call(
            "search",
            json!({"query": "GHSA-9hjg-9r4m-mvj7", "limit": 5}),
            &default_cx(),
        )
        .await
        .unwrap()
    else {
        panic!("expected Hits");
    };
    assert_eq!(
        net.hits()[0].url,
        "https://api.osv.dev/v1/vulns/GHSA-9hjg-9r4m-mvj7"
    );
    assert!(hits[0].title.starts_with("PYSEC-2026-1872"));
}

// ---------------------------------------------------------------------------
// Frankfurter (ECB + ExchangeRate-API fallback)
// ---------------------------------------------------------------------------

#[test]
fn test_parse_pair_reads_codes_names_and_amounts_in_question_order() {
    assert_eq!(
        parse_pair("100 usd to cny"),
        Some(("USD".into(), "CNY".into(), 100.0))
    );
    assert_eq!(
        parse_pair("1万日元等于多少人民币"),
        Some(("JPY".into(), "CNY".into(), 10_000.0))
    );
    assert_eq!(
        parse_pair("euro to hong kong dollar"),
        Some(("EUR".into(), "HKD".into(), 1.0))
    );
    assert_eq!(
        parse_pair("2.5k GBP in USD"),
        Some(("GBP".into(), "USD".into(), 2_500.0))
    );
    assert_eq!(parse_pair("usd exchange rate"), None);
    assert_eq!(
        parse_pair("新台币 兑 美元"),
        Some(("TWD".into(), "USD".into(), 1.0))
    );
    assert_eq!(parse_pair("the cat sat"), None);
}

#[tokio::test]
async fn test_frankfurter_converts_and_labels_the_rate_as_a_daily_reference() {
    let (net, deps) = replay_deps(vec![get_ex(
        "https://api.frankfurter.dev/v1/latest?base=USD&symbols=CNY",
        200,
        &json!({"amount": 1.0, "base": "USD", "date": "2026-09-21", "rates": {"CNY": 6.6954}}),
    )]);
    let engine = FrankfurterEngine::new(deps);
    let SrcOutput::Hits(hits) = engine
        .call(
            "search",
            json!({"query": "100 usd to cny", "limit": 5}),
            &default_cx(),
        )
        .await
        .unwrap()
    else {
        panic!("expected Hits");
    };
    assert_eq!(
        net.hits()[0].url,
        "https://api.frankfurter.dev/v1/latest?base=USD&symbols=CNY"
    );
    let r = &hits[0];
    assert_eq!(r.title, "USD to CNY: 6.6954 (ECB, 2026-09-21)");
    assert!(r.snippet.starts_with(
        "100 USD = 669.5400 CNY · 1 USD = 6.6954 CNY · ECB reference rate for 2026-09-21"
    ));
    assert!(r.snippet.contains("not a live market quote"));
    assert_eq!(r.date.as_deref(), Some("2026-09-21"));
}

#[tokio::test]
async fn test_frankfurter_does_nothing_without_two_currencies() {
    // Empty ReplayNet would error if any HTTP request were made.
    let engine = FrankfurterEngine::new(empty_deps());
    let SrcOutput::Hits(hits) = engine
        .call(
            "search",
            json!({"query": "dollar exchange rate today", "limit": 5}),
            &default_cx(),
        )
        .await
        .unwrap()
    else {
        panic!("expected Hits");
    };
    assert!(hits.is_empty());
}

#[tokio::test]
async fn test_frankfurter_falls_back_to_the_open_endpoint_for_non_ecb_currencies() {
    let (net, deps) = replay_deps(vec![get_ex(
        "https://open.er-api.com/v6/latest/TWD",
        200,
        &json!({
            "result": "success",
            "base_code": "TWD",
            "time_last_update_unix": 1790035351,
            "time_last_update_utc": "Tue, 22 Sep 2026 00:02:31 +0000",
            "rates": {"USD": 0.031495, "CNY": 0.2105}
        }),
    )]);
    let engine = FrankfurterEngine::new(deps);
    let SrcOutput::Hits(hits) = engine
        .call(
            "search",
            json!({"query": "1000 TWD to CNY", "limit": 5}),
            &default_cx(),
        )
        .await
        .unwrap()
    else {
        panic!("expected Hits");
    };
    assert_eq!(net.hits()[0].url, "https://open.er-api.com/v6/latest/TWD");
    let r = &hits[0];
    assert_eq!(r.title, "TWD to CNY: 0.2105 (ExchangeRate-API, 2026-09-22)");
    assert!(
        r.snippet.starts_with(
            "1000 TWD = 210.5000 CNY · 1 TWD = 0.2105 CNY · daily rate for 2026-09-22"
        )
    );
    assert!(
        r.snippet
            .contains("Rates By Exchange Rate API https://www.exchangerate-api.com")
    );
    assert_eq!(r.date.as_deref(), Some("2026-09-22"));
}

// ---------------------------------------------------------------------------
// IMF (ported from `tests/test_finance_engines.py`)
// ---------------------------------------------------------------------------

fn imf_indicators() -> Map<String, Value> {
    json!({
        "NGDP_RPCH": {
            "label": "Real GDP growth",
            "description": "Gross domestic product at constant prices.",
            "unit": "Annual percent change",
            "dataset": "WEO",
            "source": "World Economic Outlook (April 2026)"
        },
        "PCPIPCH": {
            "label": "Inflation rate, average consumer prices",
            "description": "Consumer price inflation.",
            "unit": "Annual percent change",
            "dataset": "WEO",
            "source": "World Economic Outlook (April 2026)"
        },
        "NGDPRPC_PCH": {
            "label": "Real Per Capita GDP Growth",
            "description": "Real per capita GDP growth.",
            "unit": "Annual percent change",
            "dataset": "WEO",
            "source": "World Economic Outlook (April 2026)"
        },
        "GDP": {
            "label": "Nominal GDP",
            "description": "Gross domestic product at current prices.",
            "unit": "Millions of US Dollars",
            "dataset": "CF",
            "source": "Capital Flows"
        }
    })
    .as_object()
    .unwrap()
    .clone()
}

fn imf_countries() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("VNM".to_owned(), "Vietnam".to_owned()),
        ("CHN".to_owned(), "China, People's Republic of".to_owned()),
        ("USA".to_owned(), "United States".to_owned()),
        ("TCD".to_owned(), "Chad".to_owned()),
    ])
}

#[test]
fn test_imf_matches_a_country_by_its_short_name() {
    assert_eq!(
        ImfEngine::match_countries("china inflation", &imf_countries()),
        vec!["CHN"]
    );
}

#[test]
fn test_imf_matches_a_country_by_iso3_code() {
    assert_eq!(
        ImfEngine::match_countries("VNM gdp", &imf_countries()),
        vec!["VNM"]
    );
}

#[test]
fn test_imf_country_matching_respects_word_boundaries() {
    assert!(ImfEngine::match_countries("chadwick holdings", &imf_countries()).is_empty());
}

#[test]
fn test_imf_strips_the_country_before_scoring_indicators() {
    let countries = imf_countries();
    let codes = ImfEngine::match_countries("vietnam real gdp growth", &countries);
    let metric = ImfEngine::strip_countries("vietnam real gdp growth", &codes, &countries);
    assert_eq!(metric, "real gdp growth");
    assert_eq!(
        ImfEngine::match_indicators(&metric, &imf_indicators())[0],
        "NGDP_RPCH"
    );
}

#[test]
fn test_imf_code_shortcut_needs_the_code_typed_as_a_code() {
    let ind = imf_indicators();
    assert_eq!(
        ImfEngine::match_indicators("real gdp growth", &ind)[0],
        "NGDP_RPCH"
    );
    assert_eq!(ImfEngine::match_indicators("GDP", &ind)[0], "GDP");
    assert_eq!(
        ImfEngine::match_indicators("NGDP_RPCH", &ind),
        vec!["NGDP_RPCH"]
    );
}

#[test]
fn test_imf_scoring_ignores_capitalization() {
    assert_eq!(
        ImfEngine::ratio("real gdp growth", "Real GDP growth", false),
        100.0
    );
}

#[test]
fn test_imf_exact_label_outranks_a_label_that_merely_contains_the_query() {
    assert_eq!(
        ImfEngine::match_indicators("real gdp growth", &imf_indicators()),
        vec!["NGDP_RPCH", "NGDPRPC_PCH"]
    );
}

#[test]
fn test_imf_returns_nothing_rather_than_an_unrelated_series() {
    assert!(ImfEngine::match_indicators("zzzzqqq", &imf_indicators()).is_empty());
}

#[test]
fn test_imf_recent_keeps_the_last_observations_including_forecasts() {
    let values: Map<String, Value> = (1990..2032)
        .map(|y| (y.to_string(), json!(f64::from(y))))
        .collect();
    let recent = ImfEngine::recent(&values);
    assert_eq!(recent.len(), 8);
    assert_eq!(recent.last().unwrap().0, "2031");
}

#[tokio::test]
async fn test_imf_describes_indicator_without_country() {
    let (_net, deps) = replay_deps(vec![
        get_ex(
            "https://www.imf.org/external/datamapper/api/v1/indicators",
            200,
            &json!({"indicators": imf_indicators()}),
        ),
        get_ex(
            "https://www.imf.org/external/datamapper/api/v1/countries",
            200,
            &json!({"countries": {"VNM": {"label": "Vietnam"}}}),
        ),
    ]);
    let engine = ImfEngine::new(deps);
    let SrcOutput::Hits(hits) = engine
        .call(
            "search",
            json!({"query": "real gdp growth", "limit": 5}),
            &default_cx(),
        )
        .await
        .unwrap()
    else {
        panic!("expected Hits");
    };
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].title, "Real GDP growth (Annual percent change)");
    assert_eq!(
        hits[0].url,
        "https://www.imf.org/external/datamapper/NGDP_RPCH@WEO"
    );
}

// ---------------------------------------------------------------------------
// Deep edge cases across all claim engines
// ---------------------------------------------------------------------------

#[test]
fn test_edge_cases_unicode_countries_and_bilingual_holidays() {
    use fsm_engines_claims::facts::countries_in;
    use fsm_engines_claims::holidays::HolidaysEngine;

    assert_eq!(
        countries_in("Türkiye GDP 2025", 2),
        vec![("TUR", "TR", "Turkey")]
    );
    assert_eq!(
        countries_in("🇹🇷 Turkey and Việt Nam Vietnam", 2),
        vec![("TUR", "TR", "Turkey"), ("VNM", "VN", "Vietnam")]
    );

    let engine = HolidaysEngine::new(empty_deps());
    let cn_payload = json!({
        "year": 2026,
        "days": [
            {"name": "元旦", "date": "2026-01-01", "isOffDay": true},
            {"name": "元旦", "date": "2026-01-04", "isOffDay": false}
        ]
    });
    let en_cn = engine.map_cn(&cn_payload, 2026, false);
    assert_eq!(
        en_cn[0].title,
        "China public holidays 2026 (State Council notice)"
    );
    assert_eq!(en_cn[0].url, "https://www.gov.cn/zhengce/");
    assert_eq!(en_cn[0].snippet, "元旦 01-01 (make-up working days 01-04)");

    let de_payload = json!([
        {"date": "2027-01-01", "localName": "Neujahr", "name": "New Year's Day", "countryCode": "DE", "global": true}
    ]);
    let zh_de = engine.map_nager(&de_payload, "DE", "Germany", 2027, true);
    assert_eq!(zh_de[0].title, "2027年德国公众假期");
    assert_eq!(zh_de[0].snippet, "01-01 Neujahr");
}

#[test]
fn test_edge_cases_osv_severity_introduced_no_fix_and_details_fallback() {
    let engine = OsvEngine::new(empty_deps());
    let payload = json!({
        "vulns": [
            {
                "id": "GHSA-aaaa-bbbb-cccc",
                "details": "Detailed advisory body without summary.",
                "modified": "2026-05-01T12:00:00Z",
                "database_specific": {"severity": "CRITICAL"},
                "affected": [
                    {
                        "package": {"ecosystem": "npm", "name": "demo", "purl": "pkg:npm/demo"},
                        "ranges": [{"type": "SEMVER", "events": [{"introduced": "1.2.0"}]}]
                    }
                ]
            },
            {
                "id": "OSV-2026-9",
                "summary": "CVSS fallback test",
                "published": "2026-04-01T00:00:00Z",
                "severity": [{"type": "CVSS_V3", "score": "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H"}]
            }
        ]
    });
    let hits = engine.map_results(&payload);
    assert_eq!(hits.len(), 2);
    assert!(hits[0].snippet.contains("Critical severity"));
    assert!(
        hits[0]
            .snippet
            .contains("npm demo from 1.2.0 no fix listed")
    );
    assert!(
        hits[0]
            .snippet
            .contains("Detailed advisory body without summary.")
    );
    assert_eq!(hits[0].date.as_deref(), Some("2026-05-01"));
    assert!(
        hits[1]
            .snippet
            .contains("CVSS_V3 CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H")
    );
}

#[test]
fn test_edge_cases_endoflife_lts_and_unknown_eol_states() {
    let engine = EndOfLifeEngine::new(empty_deps());
    let payload = json!({
        "result": {
            "name": "nodejs",
            "label": "Node.js",
            "releases": [
                {"name": "24", "isEol": false, "isLts": true, "eolFrom": "2028-04-30", "latest": {"name": "24.1.0", "date": "2026-05-10"}},
                {"name": "25", "isEol": false},
                {"name": "26"},
                {"name": "18", "isEol": true}
            ]
        }
    });
    let hits = engine.map_results(&payload, "node 18 lts");
    assert_eq!(hits.len(), 1);
    assert!(hits[0].snippet.contains("Cycles: 18: end of life; 24: LTS, supported until 2028-04-30; 25: supported; 26: support end not published"));
}

#[test]
fn test_edge_cases_pypi_yanked_crates_prerelease_fallback_and_npm_repo() {
    let pypi_hits = PypiEngine::new(empty_deps()).map_results(&json!({
        "info": {
            "name": "demo",
            "version": "1.0.0",
            "yanked": true,
            "home_page": "https://example.org/demo"
        }
    }));
    assert!(pypi_hits[0].snippet.contains("this release is yanked"));
    assert!(
        pypi_hits[0]
            .snippet
            .contains("homepage https://example.org/demo")
    );
    assert_eq!(pypi_hits[0].date, None);

    let crates_hits = CratesEngine::new(empty_deps()).map_results(&json!({
        "crates": [{
            "name": "alpha_crate",
            "max_stable_version": null,
            "newest_version": "0.1.0-alpha.1",
            "documentation": "https://docs.rs/alpha_crate"
        }]
    }));
    assert_eq!(
        crates_hits[0].title,
        "alpha_crate 0.1.0-alpha.1 on crates.io"
    );
    assert!(
        crates_hits[0]
            .snippet
            .contains("documentation https://docs.rs/alpha_crate")
    );

    let npm_hits = NpmEngine::new(empty_deps()).map_results(&json!({
        "objects": [{
            "package": {
                "name": "@scope/pkg",
                "version": "2.0.0",
                "links": {"repository": "https://github.com/scope/pkg"}
            }
        }]
    }));
    assert_eq!(
        npm_hits[0].url,
        "https://www.npmjs.com/package/@scope/pkg/v/2.0.0"
    );
    assert!(
        npm_hits[0]
            .snippet
            .contains("repository https://github.com/scope/pkg")
    );
}

#[tokio::test]
async fn test_edge_cases_crates_contact_email_ua_and_replay_mode_cache_bypass() {
    let mut req = FixtureRequest::new("GET", "https://crates.io/api/v1/crates?q=serde&per_page=3");
    req.headers_subset.insert(
        "user-agent".into(),
        "free-search-mcp/1.0 (ops@example.com)".into(),
    );
    let ex = Exchange::new(
        req,
        FixtureResponse::from_bytes(
            200,
            vec![("content-type".into(), "application/json".into())],
            br#"{"crates":[{"name":"serde","max_stable_version":"1.0.219"}]}"#,
        ),
    );
    let (net, deps) = replay_deps(vec![ex]);
    let engine = CratesEngine::new(deps);
    let mut cx = default_cx();
    cx.contact_email = Some("ops@example.com".into());
    let SrcOutput::Hits(hits) = engine
        .call(
            "search",
            json!({"query": "serde crate latest version", "limit": 3}),
            &cx,
        )
        .await
        .unwrap()
    else {
        panic!("expected Hits");
    };
    assert_eq!(hits[0].title, "serde 1.0.219 on crates.io");
    assert!(net.unmatched().is_empty());

    // Verify `replay_mode = true` bypasses CISA KEV's in-memory cache while `false` reuses it.
    let kev = json!({
        "catalogVersion": "2026.09.21",
        "vulnerabilities": [{"cveID": "CVE-2026-7273", "dateAdded": "2026-09-21"}]
    });
    let (net2, deps2) = replay_deps(vec![get_ex(
        "https://www.cisa.gov/sites/default/files/feeds/known_exploited_vulnerabilities.json",
        200,
        &kev,
    )]);
    let kev_engine = fsm_engines_claims::cisakev::CisaKevEngine::new(deps2);
    let mut cx_replay = default_cx();
    cx_replay.replay_mode = true;
    kev_engine
        .call(
            "search",
            json!({"query": "CVE-2026-7273", "limit": 3}),
            &cx_replay,
        )
        .await
        .unwrap();
    kev_engine
        .call(
            "search",
            json!({"query": "CVE-2026-7273", "limit": 3}),
            &cx_replay,
        )
        .await
        .unwrap();
    assert_eq!(
        net2.hits().len(),
        2,
        "replay_mode=true skips in-memory cache"
    );
}

#[tokio::test]
async fn test_edge_cases_http_500_and_malformed_json_return_upstream_error() {
    let (_net, deps) = replay_deps(vec![get_ex(
        "https://registry.npmjs.org/-/v1/search?text=express&size=5",
        503,
        &json!({"error": "service unavailable"}),
    )]);
    let err = NpmEngine::new(deps)
        .call(
            "search",
            json!({"query": "npm express latest version", "limit": 5}),
            &default_cx(),
        )
        .await;
    assert!(matches!(
        err,
        Err(FsmError::Upstream { ref source_id, status: Some(503), .. }) if source_id == "npm"
    ));

    let bad_ex = Exchange::new(
        FixtureRequest::new(
            "GET",
            "https://registry.npmjs.org/-/v1/search?text=express&size=5",
        ),
        FixtureResponse::from_bytes(
            200,
            vec![("content-type".into(), "text/html".into())],
            b"<html>not json</html>",
        ),
    );
    let (_net2, deps2) = replay_deps(vec![bad_ex]);
    let err2 = NpmEngine::new(deps2)
        .call(
            "search",
            json!({"query": "npm express latest version", "limit": 5}),
            &default_cx(),
        )
        .await;
    assert!(matches!(
        err2,
        Err(FsmError::Upstream { ref source_id, status: Some(200), .. }) if source_id == "npm"
    ));
}
