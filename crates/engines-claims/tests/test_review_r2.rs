//! Regression tests for every finding in `D/handoff/w1d-review2.md`
//! (B1, B2, I1, I2, M1–M3) and all 6 "Missing tests" items.

use fsm_core::fixture::{
    Exchange, Fixture, FixtureRequest, FixtureResponse, ReplayNet, canonical_json,
};
use fsm_core::{
    Config, FsmError, HttpBytes, HttpRequest, NetClient, NetCx, NullStore, Source, SourceDeps,
    SrcCx, SrcOutput, TransportProfile,
};
use fsm_engines_claims::appstore::AppStoreEngine;
use fsm_engines_claims::cfets::CfetsEngine;
use fsm_engines_claims::cisakev::CisaKevEngine;
use fsm_engines_claims::coingecko::CoinGeckoEngine;
use fsm_engines_claims::crates_io::CratesEngine;
use fsm_engines_claims::endoflife::EndOfLifeEngine;
use fsm_engines_claims::frankfurter::FrankfurterEngine;
use fsm_engines_claims::gleif::GleifEngine;
use fsm_engines_claims::holidays::HolidaysEngine;
use fsm_engines_claims::imf::ImfEngine;
use fsm_engines_claims::npm::NpmEngine;
use fsm_engines_claims::osv::OsvEngine;
use fsm_engines_claims::pypi::PypiEngine;
use fsm_engines_claims::wdi::WdiEngine;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn empty_deps() -> SourceDeps {
    let net = Arc::new(ReplayNet::from_fixtures(vec![]).unwrap());
    SourceDeps::new(net, Arc::new(NullStore::new()), Arc::new(Config::default()))
}

fn replay_deps(exchanges: Vec<Exchange>) -> (Arc<ReplayNet>, SourceDeps) {
    let mut f = Fixture::new("review_r2", "search", "2026-11-25T10:00:00Z");
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

fn get_raw_ex(url: &str, status: u16, content_type: &str, body: &[u8]) -> Exchange {
    Exchange::new(
        FixtureRequest::new("GET", url),
        FixtureResponse::from_bytes(
            status,
            vec![("content-type".into(), content_type.into())],
            body,
        ),
    )
}

fn post_json_ex(url: &str, req_body: &Value, status: u16, resp_body: &Value) -> Exchange {
    let wire = canonical_json(req_body);
    let http_req = HttpRequest::post(url, wire)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json");
    let freq = FixtureRequest::from_http(&http_req, &["accept", "content-type"]);
    Exchange::new(
        freq,
        FixtureResponse::from_bytes(
            status,
            vec![("content-type".into(), "application/json".into())],
            resp_body.to_string().as_bytes(),
        ),
    )
}

fn default_cx() -> SrcCx {
    SrcCx::new(CancellationToken::new(), Duration::from_secs(5))
}

fn assert_upstream(res: Result<SrcOutput, FsmError>, expected_source: &str, expected_status: u16) {
    match res {
        Err(FsmError::Upstream {
            source_id, status, ..
        }) => {
            assert_eq!(source_id, expected_source, "wrong source_id");
            assert_eq!(
                status,
                Some(expected_status),
                "{expected_source}: expected HTTP {expected_status}, got {status:?}"
            );
        }
        other => panic!(
            "{expected_source}: expected Err(FsmError::Upstream {{ status: Some({expected_status}) }}), got {other:?}"
        ),
    }
}

// ---------------------------------------------------------------------------
// B1 (Missing test #1): Empty 200 body errors; 204 empty body is Ok([])
// ---------------------------------------------------------------------------

#[tokio::test]
async fn regression_b1_empty_200_errors_and_204_empty_is_ok() {
    // npm receiving HTTP 200 with b"" -> Upstream(200)
    let (_, deps) = replay_deps(vec![get_raw_ex(
        "https://registry.npmjs.org/-/v1/search?text=express&size=5",
        200,
        "application/json",
        b"",
    )]);
    let res = NpmEngine::new(deps)
        .call(
            "search",
            json!({"query": "npm express latest version", "limit": 5}),
            &default_cx(),
        )
        .await;
    assert_upstream(res, "npm", 200);

    // crates receiving HTTP 200 with b"" -> Upstream(200)
    let (_, deps) = replay_deps(vec![get_raw_ex(
        "https://crates.io/api/v1/crates?q=tokio&per_page=5",
        200,
        "application/json",
        b"",
    )]);
    let res = CratesEngine::new(deps)
        .call(
            "search",
            json!({"query": "tokio crate latest stable version", "limit": 5}),
            &default_cx(),
        )
        .await;
    assert_upstream(res, "crates", 200);

    // cfets receiving HTTP 200 with b"" -> Upstream(200)
    let (_, deps) = replay_deps(vec![get_raw_ex(
        "https://www.chinamoney.com.cn/r/cms/www/chinamoney/data/fx/ccpr.json",
        200,
        "application/json",
        b"",
    )]);
    let res = CfetsEngine::new(deps)
        .call(
            "search",
            json!({"query": "人民币中间价", "limit": 5}),
            &default_cx(),
        )
        .await;
    assert_upstream(res, "cfets", 200);

    // npm receiving HTTP 204 with b"" -> Ok(Hits([]))
    let (_, deps) = replay_deps(vec![get_raw_ex(
        "https://registry.npmjs.org/-/v1/search?text=express&size=5",
        204,
        "application/json",
        b"",
    )]);
    let SrcOutput::Hits(hits) = NpmEngine::new(deps)
        .call(
            "search",
            json!({"query": "npm express latest version", "limit": 5}),
            &default_cx(),
        )
        .await
        .expect("204 empty should succeed with empty hits")
    else {
        panic!("expected Hits");
    };
    assert!(hits.is_empty());
}

// ---------------------------------------------------------------------------
// B1 (Missing test #2): Strict 404 on search/feed/catalogue vs optional 404 on lookup-by-key
// ---------------------------------------------------------------------------

#[tokio::test]
async fn regression_b1_strict_404_on_search_feed_catalogue_endpoints() {
    let not_found = json!({"error": "not found"});
    let cx = default_cx();

    // 1. npm
    let (_, deps) = replay_deps(vec![get_ex(
        "https://registry.npmjs.org/-/v1/search?text=express&size=5",
        404,
        &not_found,
    )]);
    assert_upstream(
        NpmEngine::new(deps)
            .call(
                "search",
                json!({"query": "npm express latest version", "limit": 5}),
                &cx,
            )
            .await,
        "npm",
        404,
    );

    // 2. crates
    let (_, deps) = replay_deps(vec![get_ex(
        "https://crates.io/api/v1/crates?q=tokio&per_page=5",
        404,
        &not_found,
    )]);
    assert_upstream(
        CratesEngine::new(deps)
            .call(
                "search",
                json!({"query": "tokio crate latest stable version", "limit": 5}),
                &cx,
            )
            .await,
        "crates",
        404,
    );

    // 3. cfets
    let (_, deps) = replay_deps(vec![get_ex(
        "https://www.chinamoney.com.cn/r/cms/www/chinamoney/data/fx/ccpr.json",
        404,
        &not_found,
    )]);
    assert_upstream(
        CfetsEngine::new(deps)
            .call("search", json!({"query": "人民币中间价", "limit": 5}), &cx)
            .await,
        "cfets",
        404,
    );

    // 4a. coingecko (/simple/price)
    let (_, deps) = replay_deps(vec![get_ex(
        "https://api.coingecko.com/api/v3/simple/price?ids=bitcoin&vs_currencies=usd,cny&include_24hr_change=true&include_market_cap=true&include_last_updated_at=true",
        404,
        &not_found,
    )]);
    assert_upstream(
        CoinGeckoEngine::new(deps)
            .call("search", json!({"query": "btc price", "limit": 5}), &cx)
            .await,
        "coingecko",
        404,
    );

    // 4b. coingecko (/search)
    let (_, deps) = replay_deps(vec![get_ex(
        "https://api.coingecko.com/api/v3/search?query=mysterytoken",
        404,
        &not_found,
    )]);
    assert_upstream(
        CoinGeckoEngine::new(deps)
            .call(
                "search",
                json!({"query": "mysterytoken crypto price", "limit": 5}),
                &cx,
            )
            .await,
        "coingecko",
        404,
    );

    // 5. gleif
    let (_, deps) = replay_deps(vec![get_ex(
        "https://api.gleif.org/api/v1/lei-records?filter%5Bfulltext%5D=alibaba+group&page%5Bsize%5D=5",
        404,
        &not_found,
    )]);
    assert_upstream(
        GleifEngine::new(deps)
            .call(
                "search",
                json!({"query": "alibaba group legal entity LEI", "limit": 5}),
                &cx,
            )
            .await,
        "gleif",
        404,
    );

    // 6. appstore
    let (_, deps) = replay_deps(vec![get_ex(
        "https://itunes.apple.com/search?term=Notion&country=us&entity=software&limit=5",
        404,
        &not_found,
    )]);
    assert_upstream(
        AppStoreEngine::new(deps)
            .call(
                "search",
                json!({"query": "Notion iOS app version", "limit": 5}),
                &cx,
            )
            .await,
        "appstore",
        404,
    );

    // 7. wdi
    let (_, deps) = replay_deps(vec![get_ex(
        "https://api.worldbank.org/v2/country/CHN/indicator/NY.GDP.MKTP.CD?format=json&mrv=3",
        404,
        &not_found,
    )]);
    assert_upstream(
        WdiEngine::new(deps)
            .call(
                "search",
                json!({"query": "china 2025 gdp", "limit": 5}),
                &cx,
            )
            .await,
        "wdi",
        404,
    );

    // 8. osv (POST /v1/query)
    let (_, deps) = replay_deps(vec![
        post_json_ex(
            "https://api.osv.dev/v1/query",
            &json!({"package": {"purl": "pkg:pypi/requests"}}),
            404,
            &not_found,
        ),
        post_json_ex(
            "https://api.osv.dev/v1/query",
            &json!({"package": {"purl": "pkg:npm/requests"}}),
            404,
            &not_found,
        ),
    ]);
    assert_upstream(
        OsvEngine::new(deps)
            .call(
                "search",
                json!({"query": "requests vulnerabilities", "limit": 5}),
                &cx,
            )
            .await,
        "osv",
        404,
    );

    // 9. cold-cache cisakev
    let (_, deps) = replay_deps(vec![get_ex(
        "https://www.cisa.gov/sites/default/files/feeds/known_exploited_vulnerabilities.json",
        404,
        &not_found,
    )]);
    assert_upstream(
        CisaKevEngine::new(deps)
            .call("search", json!({"query": "CVE-2024-3094", "limit": 5}), &cx)
            .await,
        "cisakev",
        404,
    );

    // 10. cold-cache endoflife (/products)
    let (_, deps) = replay_deps(vec![get_ex(
        "https://endoflife.date/api/v1/products",
        404,
        &not_found,
    )]);
    assert_upstream(
        EndOfLifeEngine::new(deps)
            .call(
                "search",
                json!({"query": "is python 3.9 still supported", "limit": 5}),
                &cx,
            )
            .await,
        "endoflife",
        404,
    );

    // 11. imf (/indicators)
    let (_, deps) = replay_deps(vec![get_ex(
        "https://www.imf.org/external/datamapper/api/v1/indicators",
        404,
        &not_found,
    )]);
    assert_upstream(
        ImfEngine::new(deps)
            .call(
                "search",
                json!({"query": "vietnam real gdp growth", "limit": 5}),
                &cx,
            )
            .await,
        "imf",
        404,
    );

    // 12. frankfurter (ECB /v1/latest)
    let (_, deps) = replay_deps(vec![get_ex(
        "https://api.frankfurter.dev/v1/latest?base=USD&symbols=CNY",
        404,
        &not_found,
    )]);
    assert_upstream(
        FrankfurterEngine::new(deps)
            .call(
                "search",
                json!({"query": "100 usd to cny", "limit": 5}),
                &cx,
            )
            .await,
        "frankfurter",
        404,
    );
}

#[tokio::test(start_paused = true)]
async fn regression_b1_lookup_by_key_404_still_skips_and_stale_endoflife_survives_404() {
    let not_found = json!({"error": "not found"});
    let cx = default_cx();

    // 1. pypi lookup-by-key 404 -> Ok([])
    let (_, deps) = replay_deps(vec![get_ex(
        "https://pypi.org/pypi/nonexistentpkg/json",
        404,
        &not_found,
    )]);
    let SrcOutput::Hits(h) = PypiEngine::new(deps)
        .call(
            "search",
            json!({"query": "nonexistentpkg latest version", "limit": 5}),
            &cx,
        )
        .await
        .unwrap()
    else {
        panic!("expected Hits");
    };
    assert!(h.is_empty());

    // 2. osv GET /v1/vulns/{id} 404 -> Ok([])
    let (_, deps) = replay_deps(vec![get_ex(
        "https://api.osv.dev/v1/vulns/CVE-2099-99999",
        404,
        &not_found,
    )]);
    let SrcOutput::Hits(h) = OsvEngine::new(deps)
        .call(
            "search",
            json!({"query": "CVE-2099-99999", "limit": 5}),
            &cx,
        )
        .await
        .unwrap()
    else {
        panic!("expected Hits");
    };
    assert!(h.is_empty());

    // 3. holidays (holiday-cn and Nager) 404 -> Ok([])
    let (_, deps) = replay_deps(vec![
        get_ex(
            "https://raw.githubusercontent.com/NateScarlet/holiday-cn/master/2099.json",
            404,
            &not_found,
        ),
        get_ex(
            "https://nagerholidays.com/api/v4/Holidays/JP/2099",
            404,
            &not_found,
        ),
    ]);
    let hol = HolidaysEngine::new(deps);
    let SrcOutput::Hits(h1) = hol
        .call(
            "search",
            json!({"query": "2099年中国放假安排", "limit": 5}),
            &cx,
        )
        .await
        .unwrap()
    else {
        panic!("expected Hits");
    };
    assert!(h1.is_empty());
    let SrcOutput::Hits(h2) = hol
        .call(
            "search",
            json!({"query": "2099 japan holidays", "limit": 5}),
            &cx,
        )
        .await
        .unwrap()
    else {
        panic!("expected Hits");
    };
    assert!(h2.is_empty());

    // 4. endoflife /products/{name} 404 -> Ok([]); warm-cache /products refresh 404 -> falls back to stale
    let products = json!({
        "result": [{"name": "python", "label": "Python", "aliases": []}]
    });
    let (_, deps) = replay_deps(vec![
        get_ex("https://endoflife.date/api/v1/products", 200, &products),
        get_ex(
            "https://endoflife.date/api/v1/products/python",
            404,
            &not_found,
        ),
        get_ex("https://endoflife.date/api/v1/products", 404, &not_found),
    ]);
    let eol = EndOfLifeEngine::new(deps);
    let SrcOutput::Hits(h) = eol
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
    assert!(h.is_empty());
    tokio::time::advance(Duration::from_secs(24 * 3600 + 5)).await;
    let SrcOutput::Hits(h_stale) = eol
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
    assert!(h_stale.is_empty());

    // 5. frankfurter open.er-api.com 404 -> Ok([])
    let (_, deps) = replay_deps(vec![get_ex(
        "https://open.er-api.com/v6/latest/TWD",
        404,
        &not_found,
    )]);
    let SrcOutput::Hits(h) = FrankfurterEngine::new(deps)
        .call(
            "search",
            json!({"query": "100 twd to usd", "limit": 5}),
            &cx,
        )
        .await
        .unwrap()
    else {
        panic!("expected Hits");
    };
    assert!(h.is_empty());
}

// ---------------------------------------------------------------------------
// B2 (Missing test #3): Multi-request fan-out surfaces errors when empty, keeps partial hits
// ---------------------------------------------------------------------------

#[tokio::test]
async fn regression_b2_fanout_errors_surface_when_empty_and_partial_hits_preserved() {
    let cx = default_cx();
    let err_body = json!({"error": "down"});
    let not_found = json!({"error": "not found"});

    // 1a. pypi: fastapi 503 + vs 404 + starlette 503 -> Err(Upstream(503))
    let (_, deps) = replay_deps(vec![
        get_ex("https://pypi.org/pypi/fastapi/json", 503, &err_body),
        get_ex("https://pypi.org/pypi/vs/json", 404, &not_found),
        get_ex("https://pypi.org/pypi/starlette/json", 503, &err_body),
    ]);
    assert_upstream(
        PypiEngine::new(deps)
            .call(
                "search",
                json!({"query": "fastapi vs starlette latest version", "limit": 5}),
                &cx,
            )
            .await,
        "pypi",
        503,
    );

    // 1b. pypi: fastapi 200 + vs 404 + starlette 503 -> partial hit [fastapi]
    let fastapi_ok = json!({
        "info": {"name": "fastapi", "version": "0.115.0"},
        "urls": [{"upload_time_iso_8601": "2026-01-01T00:00:00Z"}]
    });
    let (_, deps) = replay_deps(vec![
        get_ex("https://pypi.org/pypi/fastapi/json", 200, &fastapi_ok),
        get_ex("https://pypi.org/pypi/vs/json", 404, &not_found),
        get_ex("https://pypi.org/pypi/starlette/json", 503, &err_body),
    ]);
    let SrcOutput::Hits(partial) = PypiEngine::new(deps)
        .call(
            "search",
            json!({"query": "fastapi vs starlette latest version", "limit": 5}),
            &cx,
        )
        .await
        .expect("partial hit should succeed")
    else {
        panic!("expected Hits");
    };
    assert_eq!(partial.len(), 1);
    assert!(partial[0].title.starts_with("fastapi 0.115.0"));

    // 2a. osv package query: pkg:pypi/requests 429 + pkg:npm/requests 200 {} -> Err(RateLimited)
    let (_, deps) = replay_deps(vec![
        post_json_ex(
            "https://api.osv.dev/v1/query",
            &json!({"package": {"purl": "pkg:pypi/requests"}}),
            429,
            &err_body,
        ),
        post_json_ex(
            "https://api.osv.dev/v1/query",
            &json!({"package": {"purl": "pkg:npm/requests"}}),
            200,
            &json!({}),
        ),
    ]);
    let osv_res = OsvEngine::new(deps)
        .call(
            "search",
            json!({"query": "requests vulnerabilities", "limit": 5}),
            &cx,
        )
        .await;
    assert!(
        matches!(
            osv_res,
            Err(FsmError::RateLimited { ref source_id, .. }) if source_id == "osv"
        ),
        "expected RateLimited for osv, got {osv_res:?}"
    );

    // 2b. osv ID lookup: first 503 + second 404 -> Err(Upstream(503))
    let (_, deps) = replay_deps(vec![
        get_ex("https://api.osv.dev/v1/vulns/CVE-2024-0001", 503, &err_body),
        get_ex(
            "https://api.osv.dev/v1/vulns/CVE-2024-0002",
            404,
            &not_found,
        ),
    ]);
    assert_upstream(
        OsvEngine::new(deps)
            .call(
                "search",
                json!({"query": "CVE-2024-0001 CVE-2024-0002", "limit": 5}),
                &cx,
            )
            .await,
        "osv",
        503,
    );

    // 3. wdi: one indicator 503 + one 200 with all-null values -> Err(Upstream(503))
    let null_wdi = json!([
        {"lastupdated": "2026-01-01"},
        [{"indicator": {"id": "FP.CPI.TOTL.ZG", "value": "Inflation"}, "country": {"id": "CN", "value": "China"}, "date": "2025", "value": null}]
    ]);
    let (_, deps) = replay_deps(vec![
        get_ex(
            "https://api.worldbank.org/v2/country/CHN/indicator/NY.GDP.MKTP.CD?format=json&mrv=3",
            503,
            &err_body,
        ),
        get_ex(
            "https://api.worldbank.org/v2/country/CHN/indicator/FP.CPI.TOTL.ZG?format=json&mrv=3",
            200,
            &null_wdi,
        ),
    ]);
    assert_upstream(
        WdiEngine::new(deps)
            .call(
                "search",
                json!({"query": "china gdp inflation", "limit": 5}),
                &cx,
            )
            .await,
        "wdi",
        503,
    );

    // 4. holidays: one country 503 + one country 404 -> Err(Upstream(503))
    let (_, deps) = replay_deps(vec![
        get_ex(
            "https://raw.githubusercontent.com/NateScarlet/holiday-cn/master/2026.json",
            503,
            &err_body,
        ),
        get_ex(
            "https://nagerholidays.com/api/v4/Holidays/JP/2026",
            404,
            &not_found,
        ),
    ]);
    assert_upstream(
        HolidaysEngine::new(deps)
            .call(
                "search",
                json!({"query": "china japan 2026 holidays", "limit": 5}),
                &cx,
            )
            .await,
        "holidays",
        503,
    );

    // 5. endoflife: one product 503 + one product 404 -> Err(Upstream(503))
    let products = json!({
        "result": [
            {"name": "python", "label": "Python", "aliases": []},
            {"name": "nodejs", "label": "Node.js", "aliases": ["node"]}
        ]
    });
    let (_, deps) = replay_deps(vec![
        get_ex("https://endoflife.date/api/v1/products", 200, &products),
        get_ex(
            "https://endoflife.date/api/v1/products/python",
            503,
            &err_body,
        ),
        get_ex(
            "https://endoflife.date/api/v1/products/nodejs",
            404,
            &not_found,
        ),
    ]);
    assert_upstream(
        EndOfLifeEngine::new(deps)
            .call(
                "search",
                json!({"query": "python nodejs latest version", "limit": 5}),
                &cx,
            )
            .await,
        "endoflife",
        503,
    );

    // 6a. imf: one series 503 + another 200 missing the country -> Err(Upstream(503))
    let indicators = json!({
        "indicators": {
            "NGDP_RPCH": {"label": "Real GDP growth", "dataset": "WEO"},
            "NGDPD": {"label": "GDP growth nominal", "dataset": "WEO"}
        }
    });
    let (_, deps) = replay_deps(vec![
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
            503,
            &err_body,
        ),
        get_ex(
            "https://www.imf.org/external/datamapper/api/v1/NGDPD",
            200,
            &json!({"values": {"NGDPD": {"USA": {"2025": 2.0}}}}),
        ),
    ]);
    assert_upstream(
        ImfEngine::new(deps)
            .call(
                "search",
                json!({"query": "vietnam real gdp growth", "limit": 5}),
                &cx,
            )
            .await,
        "imf",
        503,
    );

    // 6b. imf: `/countries` 503 on a country-only query (0 indicator matches) -> Err(Upstream(503))
    let (_, deps) = replay_deps(vec![
        get_ex(
            "https://www.imf.org/external/datamapper/api/v1/indicators",
            200,
            &indicators,
        ),
        get_ex(
            "https://www.imf.org/external/datamapper/api/v1/countries",
            503,
            &err_body,
        ),
    ]);
    assert_upstream(
        ImfEngine::new(deps)
            .call("search", json!({"query": "vietnam", "limit": 5}), &cx)
            .await,
        "imf",
        503,
    );
}

// ---------------------------------------------------------------------------
// I1 (Missing test #4): `cisakev` CVE-ID lookup enforces `limit`
// ---------------------------------------------------------------------------

#[tokio::test]
async fn regression_i1_cisakev_cve_id_lookup_enforces_limit() {
    let feed = json!({
        "catalogVersion": "2026.09.21",
        "vulnerabilities": [
            {
                "cveID": "CVE-2026-7273",
                "vendorProject": "Zyxel",
                "product": "GS1900",
                "vulnerabilityName": "Zyxel OS Command Injection",
                "dateAdded": "2026-09-21"
            }
        ]
    });
    let (_, deps) = replay_deps(vec![get_ex(
        "https://www.cisa.gov/sites/default/files/feeds/known_exploited_vulnerabilities.json",
        200,
        &feed,
    )]);
    let engine = CisaKevEngine::new(deps);
    let cx = default_cx();

    // 3 absent CVEs with limit=1 -> 1 hit (not 3).
    let SrcOutput::Hits(h1) = engine
        .call(
            "search",
            json!({"query": "CVE-2024-0001 CVE-2024-0002 CVE-2024-0003", "limit": 1}),
            &cx,
        )
        .await
        .unwrap()
    else {
        panic!("expected Hits");
    };
    assert_eq!(h1.len(), 1, "expected 1 hit with limit=1, got {}", h1.len());
    assert!(h1[0].title.starts_with("CVE-2024-0001: not in CISA"));

    // 1 listed + 1 absent CVE with limit=1 -> 1 hit (the listed CVE, not 2).
    let SrcOutput::Hits(h2) = engine
        .call(
            "search",
            json!({"query": "CVE-2026-7273 CVE-2024-3094", "limit": 1}),
            &cx,
        )
        .await
        .unwrap()
    else {
        panic!("expected Hits");
    };
    assert_eq!(h2.len(), 1, "expected 1 hit with limit=1, got {}", h2.len());
    assert!(h2[0].title.starts_with("CVE-2026-7273: known exploited"));
}

// ---------------------------------------------------------------------------
// I2 (Missing test #5): Shared `CallBudget` timeout across sequential requests
// ---------------------------------------------------------------------------

struct DelayedStep {
    delay: Duration,
    status: u16,
    body: String,
}

struct DelayedNet {
    steps: Mutex<VecDeque<DelayedStep>>,
    recorded_timeouts: Mutex<Vec<(String, Duration)>>,
}

impl DelayedNet {
    fn new(steps: Vec<DelayedStep>) -> Self {
        Self {
            steps: Mutex::new(steps.into()),
            recorded_timeouts: Mutex::new(Vec::new()),
        }
    }

    fn recorded(&self) -> Vec<(String, Duration)> {
        self.recorded_timeouts.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl NetClient for DelayedNet {
    async fn send(
        &self,
        req: HttpRequest,
        _profile: TransportProfile,
        cx: &NetCx,
    ) -> Result<HttpBytes, FsmError> {
        self.recorded_timeouts
            .lock()
            .unwrap()
            .push((req.url.clone(), cx.timeout));
        let step = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected extra HTTP call");
        if !step.delay.is_zero() {
            tokio::time::sleep(step.delay).await;
        }
        Ok(HttpBytes::new(
            req.url,
            step.status,
            vec![("content-type".into(), "application/json".into())],
            step.body.into_bytes(),
        ))
    }

    fn check_url_offline(&self, _url: &str) -> Result<(), FsmError> {
        Ok(())
    }
}

fn delayed_deps(steps: Vec<DelayedStep>) -> (Arc<DelayedNet>, SourceDeps) {
    let net = Arc::new(DelayedNet::new(steps));
    let deps = SourceDeps::new(
        net.clone(),
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    );
    (net, deps)
}

#[tokio::test(start_paused = true)]
async fn regression_i2_shared_call_budget_across_sequential_requests_and_zero_fallback() {
    let indicators = json!({
        "indicators": {
            "NGDP_RPCH": {"label": "Real GDP growth", "dataset": "WEO"}
        }
    })
    .to_string();
    let countries = json!({"countries": {"VNM": {"label": "Vietnam"}}}).to_string();
    let series = json!({"values": {"NGDP_RPCH": {"VNM": {"2025": 6.5}}}}).to_string();

    // 1. IMF: 2.5s (/indicators) + 2.5s (/countries) exhausts 5s budget before /v1/NGDP_RPCH runs.
    let (net, deps) = delayed_deps(vec![
        DelayedStep {
            delay: Duration::from_millis(2500),
            status: 200,
            body: indicators.clone(),
        },
        DelayedStep {
            delay: Duration::from_millis(2500),
            status: 200,
            body: countries.clone(),
        },
        DelayedStep {
            delay: Duration::ZERO,
            status: 200,
            body: series.clone(),
        },
    ]);
    let cx = SrcCx::new(CancellationToken::new(), Duration::from_secs(5));
    let res = ImfEngine::new(deps)
        .call(
            "search",
            json!({"query": "vietnam real gdp growth", "limit": 5}),
            &cx,
        )
        .await;
    assert!(
        matches!(res, Err(FsmError::Timeout(ms)) if ms > 0),
        "expected Timeout on IMF cumulative budget expiry, got {res:?}"
    );
    let rec = net.recorded();
    assert_eq!(
        rec.len(),
        2,
        "3rd IMF call must not start after budget exhausted"
    );
    assert!(
        rec[1].1 <= Duration::from_millis(2500),
        "2nd IMF call must receive remaining budget (<=2.5s), got {:?}",
        rec[1].1
    );

    // 2. CoinGecko: /search takes 3s, /simple/price tries to take 3s -> exceeds 5s shared budget.
    let (net, deps) = delayed_deps(vec![
        DelayedStep {
            delay: Duration::from_secs(3),
            status: 200,
            body: json!({"coins": [{"id": "mysterytoken", "name": "Mystery", "symbol": "MYS"}]})
                .to_string(),
        },
        DelayedStep {
            delay: Duration::from_secs(3),
            status: 200,
            body: json!({"mysterytoken": {"usd": 1.23}}).to_string(),
        },
    ]);
    let res = CoinGeckoEngine::new(deps)
        .call(
            "search",
            json!({"query": "mysterytoken crypto price", "limit": 5}),
            &cx,
        )
        .await;
    assert!(
        matches!(res, Err(FsmError::Timeout(ms)) if ms > 0),
        "expected Timeout on CoinGecko cumulative budget expiry, got {res:?}"
    );
    let rec = net.recorded();
    assert_eq!(rec.len(), 2);
    assert!(
        rec[1].1 <= Duration::from_secs(2),
        "2nd CoinGecko call must receive remaining budget (<=2s), got {:?}",
        rec[1].1
    );

    // 3. endoflife: /products takes 3s, /products/python tries to take 3s -> exceeds 5s shared budget.
    let (net, deps) = delayed_deps(vec![
        DelayedStep {
            delay: Duration::from_secs(3),
            status: 200,
            body: json!({"result": [{"name": "python", "label": "Python", "aliases": []}]})
                .to_string(),
        },
        DelayedStep {
            delay: Duration::from_secs(3),
            status: 200,
            body: json!({"result": {"name": "python", "label": "Python", "releases": []}})
                .to_string(),
        },
    ]);
    let res = EndOfLifeEngine::new(deps)
        .call(
            "search",
            json!({"query": "python latest version", "limit": 5}),
            &cx,
        )
        .await;
    assert!(
        matches!(res, Err(FsmError::Timeout(ms)) if ms > 0),
        "expected Timeout on endoflife cumulative budget expiry, got {res:?}"
    );
    let rec = net.recorded();
    assert_eq!(rec.len(), 2);
    assert!(
        rec[1].1 <= Duration::from_secs(2),
        "2nd endoflife call must receive remaining budget (<=2s), got {:?}",
        rec[1].1
    );

    // 4. Duration::ZERO in SrcCx falls back to decl.limits.timeout_ms (not 0).
    let (net, deps) = delayed_deps(vec![DelayedStep {
        delay: Duration::ZERO,
        status: 200,
        body: json!({"objects": []}).to_string(),
    }]);
    let engine = NpmEngine::new(deps);
    let expected_ms = engine.decl().limits.timeout_ms;
    assert!(expected_ms > 0);
    let zero_cx = SrcCx::new(CancellationToken::new(), Duration::ZERO);
    engine
        .call(
            "search",
            json!({"query": "npm express latest version", "limit": 5}),
            &zero_cx,
        )
        .await
        .unwrap();
    let rec = net.recorded();
    assert_eq!(rec.len(), 1);
    assert_eq!(
        rec[0].1,
        Duration::from_millis(expected_ms),
        "Duration::ZERO must fall back to decl.limits.timeout_ms"
    );
}

// ---------------------------------------------------------------------------
// M1 (Missing test #6a): `extract_charset` skips valueless parameters
// ---------------------------------------------------------------------------

#[tokio::test]
async fn regression_m1_extract_charset_skips_valueless_parameters() {
    let utf8_json = r#"{"objects":[{"package":{"name":"demo","version":"1.0.0","description":"中文描述测试"}}]}"#;
    let (gbk_bytes, _, had_errors) = encoding_rs::GBK.encode(utf8_json);
    assert!(!had_errors);

    for ct in [
        "application/json; ; charset=gbk",
        "application/json; foo; charset=gb18030",
    ] {
        let (_, deps) = replay_deps(vec![get_raw_ex(
            "https://registry.npmjs.org/-/v1/search?text=demo&size=5",
            200,
            ct,
            &gbk_bytes,
        )]);
        let SrcOutput::Hits(hits) = NpmEngine::new(deps)
            .call(
                "search",
                json!({"query": "npm demo latest version", "limit": 5}),
                &default_cx(),
            )
            .await
            .expect("GBK body with valueless Content-Type parameter must decode")
        else {
            panic!("expected Hits");
        };
        assert_eq!(hits.len(), 1);
        assert!(
            hits[0].snippet.contains("中文描述测试"),
            "ct={ct:?}: expected decoded CJK text, got {:?}",
            hits[0].snippet
        );
        assert!(
            !hits[0].snippet.contains('\u{FFFD}'),
            "ct={ct:?}: replacement char found in {:?}",
            hits[0].snippet
        );
    }
}

// ---------------------------------------------------------------------------
// M2: `cisakev.call()` keyword search clips & deduplicates tokens
// ---------------------------------------------------------------------------

#[tokio::test]
async fn regression_m2_cisakev_keyword_search_caps_and_deduplicates_tokens() {
    let rows: Vec<Value> = (0..1_200)
        .map(|i| {
            json!({
                "cveID": format!("CVE-2026-{i:04}"),
                "vendorProject": "ExampleVendor",
                "product": format!("Gateway-{i}"),
                "vulnerabilityName": "Remote Code Execution Vulnerability",
                "dateAdded": "2026-09-21"
            })
        })
        .collect();
    let feed = json!({
        "catalogVersion": "2026.09.21",
        "vulnerabilities": rows
    });
    let (_, deps) = replay_deps(vec![get_ex(
        "https://www.cisa.gov/sites/default/files/feeds/known_exploited_vulnerabilities.json",
        200,
        &feed,
    )]);
    let engine = CisaKevEngine::new(deps);
    let cx = default_cx();
    let huge_query = "remote ".repeat(15_000);

    let start = std::time::Instant::now();
    let SrcOutput::Hits(hits) = engine
        .call("search", json!({"query": huge_query, "limit": 5}), &cx)
        .await
        .unwrap()
    else {
        panic!("expected Hits");
    };
    let elapsed = start.elapsed();
    assert_eq!(hits.len(), 5);
    assert!(
        elapsed < Duration::from_millis(150),
        "cisakev keyword search on 105KB repeated query took {elapsed:?} (limit 150ms)"
    );
}

// ---------------------------------------------------------------------------
// M3 (Missing test #6b): Non-finite string rates filtered in frankfurter & cfets
// ---------------------------------------------------------------------------

#[test]
fn regression_m3_non_finite_rates_filtered_in_frankfurter_and_cfets() {
    let fx_hits = FrankfurterEngine::new(empty_deps()).map_results(&json!({
        "base": "USD",
        "date": "2026-09-22",
        "rates": {
            "CNY": "NaN",
            "EUR": "inf",
            "JPY": "-inf",
            "GBP": "0.79"
        }
    }));
    assert_eq!(fx_hits.len(), 1, "got hits: {fx_hits:?}");
    assert!(fx_hits[0].title.starts_with("USD to GBP: 0.79"));

    let cfets_hits = CfetsEngine::new(empty_deps()).map_results(&json!({
        "data": {"lastDate": "2026-09-22 9:15"},
        "_wanted": ["USD", "EUR", "JPY", "GBP"],
        "records": [
            {"vrtEName": "EUR/CNY", "foreignCName": "EUR", "price": "NaN"},
            {"vrtEName": "JPY/CNY", "foreignCName": "JPY", "price": "inf"},
            {"vrtEName": "GBP/CNY", "foreignCName": "GBP", "price": "-Infinity"},
            {"vrtEName": "USD/CNY", "foreignCName": "USD", "price": "6.7459"}
        ]
    }));
    assert_eq!(cfets_hits.len(), 1, "got hits: {cfets_hits:?}");
    assert!(
        cfets_hits[0]
            .title
            .starts_with("USD to CNY central parity: 6.7459")
    );
}
