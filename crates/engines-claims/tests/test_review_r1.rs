//! Regression tests for every finding in `D/handoff/w1d-review.md` (B1, I2, M1–M8)
//! and all 8 "Missing tests" items.

use fsm_core::fixture::{Exchange, Fixture, FixtureRequest, FixtureResponse, ReplayNet};
use fsm_core::{
    Config, NullStore, PoolRole, Registry, Source, SourceDeps, SrcCx, SrcOutput, TransportProfile,
};
use fsm_engines_claims::cfets::CfetsEngine;
use fsm_engines_claims::cisakev::CisaKevEngine;
use fsm_engines_claims::coingecko::{CoinGeckoEngine, coins_in};
use fsm_engines_claims::endoflife::EndOfLifeEngine;
use fsm_engines_claims::frankfurter::parse_pair;
use fsm_engines_claims::imf::ImfEngine;
use fsm_engines_claims::osv::OsvEngine;
use fsm_engines_claims::pypi::PypiEngine;
use serde_json::{Map, Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn empty_deps() -> SourceDeps {
    let net = Arc::new(ReplayNet::from_fixtures(vec![]).unwrap());
    SourceDeps::new(net, Arc::new(NullStore::new()), Arc::new(Config::default()))
}

fn replay_deps(exchanges: Vec<Exchange>) -> (Arc<ReplayNet>, SourceDeps) {
    let mut f = Fixture::new("review_r1", "search", "2026-11-25T10:00:00Z");
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

fn default_cx() -> SrcCx {
    SrcCx::new(CancellationToken::new(), Duration::from_secs(5))
}

// ---------------------------------------------------------------------------
// B1: Non-ASCII digits (full-width, Arabic-Indic, Extended Arabic-Indic, Devanagari)
// ---------------------------------------------------------------------------

#[test]
fn regression_b1_non_ascii_digits_in_parse_pair_and_claims() {
    assert_eq!(
        parse_pair("１万日元等于多少人民币"),
        Some(("JPY".into(), "CNY".into(), 10_000.0))
    );
    assert_eq!(
        parse_pair("１００美元兑人民币"),
        Some(("USD".into(), "CNY".into(), 100.0))
    );
    assert_eq!(
        parse_pair("\u{0661}\u{0660}\u{0660} usd to eur"),
        Some(("USD".into(), "EUR".into(), 100.0))
    );
    assert_eq!(
        parse_pair("\u{06F2}\u{06F5}\u{06F0} usd to eur"),
        Some(("USD".into(), "EUR".into(), 250.0))
    );
    assert_eq!(
        parse_pair("\u{0967}\u{0966}\u{0966} usd to inr"),
        Some(("USD".into(), "INR".into(), 100.0))
    );
    // Full-width letters or punctuation must NOT be NFKC-folded (Python parity).
    assert_eq!(parse_pair("100 ＵＳＤ to ＥＵＲ"), None);

    let deps = empty_deps();
    let mut reg = Registry::new();
    fsm_engines_claims::register(&mut reg, &deps).unwrap();
    let claimants: Vec<&str> = reg
        .all()
        .filter(|s| s.claims("１万日元等于多少人民币", None) > 0.0)
        .map(|s| s.decl().id.as_str())
        .collect();
    assert_eq!(claimants, vec!["frankfurter", "cfets"]);
}

#[tokio::test]
async fn regression_b1_cfets_fullwidth_amount_converts_hundred_not_one() {
    let cfets = json!({
        "data": {"lastDate": "2026-09-22 9:15"},
        "records": [
            {"vrtEName": "USD/CNY", "foreignCName": "USD", "price": "6.7459"}
        ]
    });
    let (_net, deps) = replay_deps(vec![get_ex(
        "https://www.chinamoney.com.cn/r/cms/www/chinamoney/data/fx/ccpr.json",
        200,
        &cfets,
    )]);
    let engine = CfetsEngine::new(deps);
    let SrcOutput::Hits(hits) = engine
        .call(
            "search",
            json!({"query": "１００美元兑人民币", "limit": 5}),
            &default_cx(),
        )
        .await
        .unwrap()
    else {
        panic!("expected Hits");
    };
    assert_eq!(hits.len(), 1);
    assert!(
        hits[0].snippet.starts_with("100 USD = 674.5900 CNY"),
        "got snippet: {}",
        hits[0].snippet
    );
}

// ---------------------------------------------------------------------------
// I2: Unified claim-score scale (orchestrator ruling: >=0.8 direct, 0.5 relevant, 0.0 none)
// ---------------------------------------------------------------------------

#[test]
fn regression_i2_test_every_fact_engine_is_honest_about_which_results_are_records() {
    let deps = empty_deps();
    let mut reg = Registry::new();
    fsm_engines_claims::register(&mut reg, &deps).unwrap();

    // Direct-answer engines must score >= 0.8 (1.0) on their direct queries.
    let direct_cases: &[(&str, &str)] = &[
        ("endoflife", "is python 3.9 still supported"),
        ("pypi", "latest fastapi version"),
        ("npm", "npm express latest version"),
        ("crates", "tokio crate latest stable version"),
        ("appstore", "微信 iOS 版最新版本"),
        ("osv", "requests vulnerabilities"),
        ("osv", "CVE-2024-3094"),
        ("cisakev", "CVE-2024-3094"),
        ("wdi", "中国 2025 GDP"),
        ("holidays", "2026年中国放假安排"),
        ("frankfurter", "100 usd to cny"),
        ("cfets", "人民币中间价"),
        ("gleif", "alibaba group legal entity LEI"),
        ("coingecko", "btc price"),
    ];
    for &(id, query) in direct_cases {
        let src = reg.get(id).expect("registered");
        let score = src.claims(query, None);
        assert!(
            (0.8..=1.0).contains(&score),
            "{id}.claims({query:?}) = {score}, expected >= 0.8 (direct answer)"
        );
    }

    // `cisakev` keyword mode is relevant (0 < score < 0.8), NOT a direct answer.
    let kev = reg.get("cisakev").unwrap();
    assert_eq!(kev.claims("CVE-2024-3094", None), 1.0);
    assert_eq!(kev.claims("zyxel exploited in the wild", None), 0.5);
    assert_eq!(kev.claims("is log4j actively exploited", None), 0.5);
    assert_eq!(kev.claims("how to cook rice", None), 0.0);

    // `imf` never claims general queries (`ScopeOnly` under `"finance"`).
    let imf = reg.get("imf").unwrap();
    assert_eq!(imf.decl().pool_role, PoolRole::ScopeOnly);
    assert!(imf.decl().scopes.iter().any(|s| s == "finance"));
    assert_eq!(imf.claims("vietnam real gdp growth", None), 0.0);
    assert_eq!(imf.claims("中国 2025 GDP", None), 0.0);
}

// ---------------------------------------------------------------------------
// M1: `shiba` in CoinGecko table and label
// ---------------------------------------------------------------------------

#[test]
fn regression_m1_coingecko_shiba_key_and_label() {
    assert_eq!(coins_in("shiba coin price", 3), vec!["shiba-inu"]);
    let hits = CoinGeckoEngine::new(empty_deps())
        .map_results(&json!({"shiba-inu": {"usd": 0.000012, "cny": 0.000084}}));
    assert_eq!(hits.len(), 1);
    assert!(
        hits[0].title.starts_with("Shiba (SHIB)"),
        "got title: {}",
        hits[0].title
    );
}

// ---------------------------------------------------------------------------
// M2: Python `a or b` truthiness fallbacks in PyPI and OSV
// ---------------------------------------------------------------------------

#[test]
fn regression_m2_pypi_and_osv_null_and_empty_fallbacks() {
    let pypi_payload = json!({
        "info": {
            "name": "demo",
            "version": "1.0",
            "summary": "s",
            "project_urls": {"Homepage": ""},
            "home_page": "https://h.example"
        },
        "urls": [{
            "upload_time_iso_8601": null,
            "upload_time": "2026-01-02T00:00:00"
        }]
    });
    let pypi_hits = PypiEngine::new(empty_deps()).map_results(&pypi_payload);
    assert_eq!(pypi_hits.len(), 1);
    assert_eq!(
        pypi_hits[0].snippet,
        "Latest release 1.0, uploaded 2026-01-02 · s · homepage https://h.example"
    );
    assert_eq!(pypi_hits[0].date.as_deref(), Some("2026-01-02"));

    let osv_payload = json!({
        "vulns": [{
            "id": "GHSA-x",
            "published": null,
            "modified": "2026-01-02T00:00:00Z",
            "summary": "   ",
            "details": "d"
        }]
    });
    let osv_hits = OsvEngine::new(empty_deps()).map_results(&osv_payload);
    assert_eq!(osv_hits.len(), 1);
    assert_eq!(osv_hits[0].title, "GHSA-x");
    assert_eq!(osv_hits[0].date.as_deref(), Some("2026-01-02"));
    assert!(
        !osv_hits[0].snippet.contains('d') || osv_hits[0].snippet.contains("published 2026-01-02"),
        "unexpected snippet: {}",
        osv_hits[0].snippet
    );
    assert_eq!(osv_hits[0].snippet, "published 2026-01-02");
}

// ---------------------------------------------------------------------------
// M4 & Missing tests #4–#5: Cache reuse, TTL expiry, stale-on-error, IMF countries fallback
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn regression_m4_cisakev_cache_reuse_and_stale_on_error() {
    let feed = json!({
        "catalogVersion": "2026.09.21",
        "vulnerabilities": [
            {"cveID": "CVE-2026-7273", "vendorProject": "Zyxel", "product": "GS1900", "dateAdded": "2026-09-21"}
        ]
    });
    let (net, deps) = replay_deps(vec![
        get_ex(
            "https://www.cisa.gov/sites/default/files/feeds/known_exploited_vulnerabilities.json",
            200,
            &feed,
        ),
        get_ex(
            "https://www.cisa.gov/sites/default/files/feeds/known_exploited_vulnerabilities.json",
            503,
            &json!({"error": "upstream down"}),
        ),
    ]);
    let engine = CisaKevEngine::new(deps);
    let cx = default_cx();

    // Two calls within TTL -> only 1 network hit.
    let SrcOutput::Hits(h1) = engine
        .call("search", json!({"query": "CVE-2026-7273", "limit": 5}), &cx)
        .await
        .unwrap()
    else {
        panic!("expected Hits");
    };
    assert_eq!(h1.len(), 1);
    let SrcOutput::Hits(h2) = engine
        .call("search", json!({"query": "CVE-2026-7273", "limit": 5}), &cx)
        .await
        .unwrap()
    else {
        panic!("expected Hits");
    };
    assert_eq!(h2.len(), 1);
    assert_eq!(net.hits().len(), 1, "within TTL must reuse cache");

    // Advance past 6h TTL -> refresh triggers (503) -> serves stale cache instead of failing.
    tokio::time::advance(Duration::from_secs(6 * 3600 + 5)).await;
    let SrcOutput::Hits(h3) = engine
        .call("search", json!({"query": "CVE-2026-7273", "limit": 5}), &cx)
        .await
        .unwrap()
    else {
        panic!("expected Hits from stale cache");
    };
    assert_eq!(net.hits().len(), 2, "expired TTL must attempt refresh");
    assert_eq!(h3.len(), 1);
    assert!(h3[0].title.starts_with("CVE-2026-7273: known exploited"));
}

#[tokio::test(start_paused = true)]
async fn regression_m4_endoflife_cache_reuse_and_stale_on_error() {
    let products = json!({
        "result": [{"name": "python", "label": "Python", "aliases": []}]
    });
    let py = json!({
        "result": {
            "name": "python",
            "label": "Python",
            "releases": [{"name": "3.14", "isEol": false, "eolFrom": "2030-10-31", "latest": {"name": "3.14.7", "date": "2026-08-05"}}]
        }
    });
    let (net, deps) = replay_deps(vec![
        get_ex("https://endoflife.date/api/v1/products", 200, &products),
        get_ex("https://endoflife.date/api/v1/products/python", 200, &py),
        get_ex(
            "https://endoflife.date/api/v1/products",
            503,
            &json!({"error": "down"}),
        ),
    ]);
    let engine = EndOfLifeEngine::new(deps);
    let cx = default_cx();

    // Two calls within 24h TTL -> `/products` fetched once, `/products/python` twice.
    engine
        .call(
            "search",
            json!({"query": "python latest version", "limit": 5}),
            &cx,
        )
        .await
        .unwrap();
    engine
        .call(
            "search",
            json!({"query": "python latest version", "limit": 5}),
            &cx,
        )
        .await
        .unwrap();
    let prod_hits = net
        .hits()
        .iter()
        .filter(|h| h.url == "https://endoflife.date/api/v1/products")
        .count();
    assert_eq!(prod_hits, 1, "catalogue must be cached across calls");

    // Advance past 24h TTL -> `/products` refresh returns 503 -> stale index used.
    tokio::time::advance(Duration::from_secs(24 * 3600 + 5)).await;
    let SrcOutput::Hits(h3) = engine
        .call(
            "search",
            json!({"query": "python latest version", "limit": 5}),
            &cx,
        )
        .await
        .unwrap()
    else {
        panic!("expected Hits");
    };
    let prod_hits_after = net
        .hits()
        .iter()
        .filter(|h| h.url == "https://endoflife.date/api/v1/products")
        .count();
    assert_eq!(prod_hits_after, 2, "expired catalogue must attempt refresh");
    assert_eq!(h3.len(), 1);
    assert_eq!(h3[0].url, "https://endoflife.date/python");
}

#[tokio::test]
async fn regression_m4_imf_cache_reuse_and_countries_failure_still_describes_indicator() {
    let indicators = json!({
        "indicators": {
            "NGDP_RPCH": {
                "label": "Real GDP growth",
                "description": "Gross domestic product at constant prices.",
                "unit": "Annual percent change",
                "dataset": "WEO"
            }
        }
    });
    let (net, deps) = replay_deps(vec![
        get_ex(
            "https://www.imf.org/external/datamapper/api/v1/indicators",
            200,
            &indicators,
        ),
        get_ex(
            "https://www.imf.org/external/datamapper/api/v1/countries",
            503,
            &json!({"error": "countries temporarily down"}),
        ),
    ]);
    let engine = ImfEngine::new(deps);
    let cx = default_cx();

    // First call: `/countries` fails with 503 -> engine degrades to empty countries map and still describes indicator.
    let SrcOutput::Hits(h1) = engine
        .call(
            "search",
            json!({"query": "real gdp growth", "limit": 5}),
            &cx,
        )
        .await
        .unwrap()
    else {
        panic!("expected Hits");
    };
    assert_eq!(h1.len(), 1);
    assert_eq!(h1[0].title, "Real GDP growth (Annual percent change)");

    // Second call with replay_mode=false: `/indicators` is served from cache (1 fetch total).
    engine
        .call(
            "search",
            json!({"query": "real gdp growth", "limit": 5}),
            &cx,
        )
        .await
        .unwrap();
    let ind_hits = net
        .hits()
        .iter()
        .filter(|h| h.url.ends_with("/indicators"))
        .count();
    assert_eq!(ind_hits, 1, "IMF indicator catalogue must be cached");

    // Separate engine with healthy `/indicators` + `/countries`: both catalogues cached across calls.
    let (net_ok, deps_ok) = replay_deps(vec![
        get_ex(
            "https://www.imf.org/external/datamapper/api/v1/indicators",
            200,
            &indicators,
        ),
        get_ex(
            "https://www.imf.org/external/datamapper/api/v1/countries",
            200,
            &json!({"countries": {"VNM": {"label": "Vietnam"}}}),
        ),
        get_ex(
            "https://www.imf.org/external/datamapper/api/v1/NGDP_RPCH",
            200,
            &json!({"values": {"NGDP_RPCH": {"VNM": {"2025": 6.5}}}}),
        ),
    ]);
    let engine_ok = ImfEngine::new(deps_ok);
    engine_ok
        .call(
            "search",
            json!({"query": "vietnam real gdp growth", "limit": 5}),
            &cx,
        )
        .await
        .unwrap();
    engine_ok
        .call(
            "search",
            json!({"query": "vietnam real gdp growth", "limit": 5}),
            &cx,
        )
        .await
        .unwrap();
    assert_eq!(
        net_ok
            .hits()
            .iter()
            .filter(|h| h.url.ends_with("/indicators"))
            .count(),
        1,
        "IMF indicators fetched once"
    );
    assert_eq!(
        net_ok
            .hits()
            .iter()
            .filter(|h| h.url.ends_with("/countries"))
            .count(),
        1,
        "IMF countries fetched once"
    );
}

// ---------------------------------------------------------------------------
// M6: Long-query CPU budget
// ---------------------------------------------------------------------------

#[test]
fn regression_m6_long_query_claims_and_imf_bounded_cpu() {
    let deps = empty_deps();
    let mut reg = Registry::new();
    fsm_engines_claims::register(&mut reg, &deps).unwrap();

    // 100 KB repetitive query through `coins_in` directly (tests O(N) blanking) and every `claims()`.
    let big_btc = "btc ".repeat(25_000);
    let start_coins = std::time::Instant::now();
    assert_eq!(coins_in(&big_btc, 3), vec!["bitcoin"]);
    let coins_elapsed = start_coins.elapsed();
    assert!(
        coins_elapsed < Duration::from_millis(250),
        "coins_in() on 100KB query took {coins_elapsed:?} (limit 250ms)"
    );

    let start = std::time::Instant::now();
    for src in reg.all() {
        let _ = src.claims(&big_btc, None);
    }
    let claims_elapsed = start.elapsed();
    assert!(
        claims_elapsed < Duration::from_millis(250),
        "claims() on 100KB query took {claims_elapsed:?} (limit 250ms)"
    );

    // 50 KB query through IMF's indicator matcher against 132 synthetic indicators.
    let mut catalogue = Map::new();
    for i in 0..132 {
        catalogue.insert(
            format!("IND_{i:03}"),
            json!({
                "label": format!("Macroeconomic indicator series number {i} annual percent change"),
                "description": format!("Detailed description for indicator {i} covering GDP growth inflation debt and employment.")
            }),
        );
    }
    let big_query = "real gdp growth inflation forecast ".repeat(1_500);
    let start_imf = std::time::Instant::now();
    let _ = ImfEngine::match_indicators(&big_query, &catalogue);
    let imf_elapsed = start_imf.elapsed();
    assert!(
        imf_elapsed < Duration::from_millis(250),
        "ImfEngine::match_indicators on 50KB query took {imf_elapsed:?} (limit 250ms)"
    );
}

// ---------------------------------------------------------------------------
// M7: IMF uses Chrome transport (oracle parity)
// ---------------------------------------------------------------------------

#[test]
fn regression_m7_imf_uses_chrome_transport() {
    let imf = ImfEngine::new(empty_deps());
    assert_eq!(imf.decl().transport, TransportProfile::Chrome);
}

// ---------------------------------------------------------------------------
// M8: IMF country tie-breaking preserves API order
// ---------------------------------------------------------------------------

#[test]
fn regression_m8_imf_country_equal_length_ties_preserve_api_order() {
    // Both aliases have length 4 ("chad" and "peru"); ZZZ appears first in API order.
    let api_order = vec![
        ("ZZZ".to_owned(), "Peru".to_owned()),
        ("AAA".to_owned(), "Chad".to_owned()),
    ];
    let matched = ImfEngine::match_countries_ordered("peru and chad gdp", &api_order);
    assert_eq!(matched, vec!["ZZZ", "AAA"]);
}

// ---------------------------------------------------------------------------
// Missing test #8: Per-engine `cache_ttl_s` policy pins
// ---------------------------------------------------------------------------

#[test]
fn test_record_categories_cap_the_cache_on_their_own_clock() {
    let deps = empty_deps();
    let mut reg = Registry::new();
    fsm_engines_claims::register(&mut reg, &deps).unwrap();

    // FX & crypto cap at 1h (3600s); software/security/reference/macro cap at 6h (21600s).
    let expected_ttl: &[(&str, u64)] = &[
        ("frankfurter", 3_600),
        ("cfets", 3_600),
        ("coingecko", 3_600),
        ("pypi", 21_600),
        ("npm", 21_600),
        ("crates", 21_600),
        ("endoflife", 21_600),
        ("appstore", 21_600),
        ("osv", 21_600),
        ("cisakev", 21_600),
        ("wdi", 21_600),
        ("holidays", 21_600),
        ("gleif", 21_600),
        ("imf", 21_600),
    ];
    for &(id, ttl) in expected_ttl {
        let src = reg.get(id).expect("registered");
        assert_eq!(src.decl().limits.cache_ttl_s, ttl, "{id} cache_ttl_s");
    }
}
