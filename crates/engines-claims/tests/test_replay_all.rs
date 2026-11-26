//! Offline `ReplayNet` tests covering all 14 engines via §4 fixtures in
//! `tests/fixtures/`, including the M16 replay test for
//! `search({query: "tokio crate latest stable version", scope: "code"})`.

use fsm_core::fixture::{Exchange, Fixture, FixtureRequest, FixtureResponse, ReplayNet};
use fsm_core::{
    Availability, Config, FsmError, HitKind, NullStore, Registry, SourceDeps, SrcCx, SrcOutput,
    scopes,
};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn replay_cx(replay_mode: bool) -> SrcCx {
    let mut cx = SrcCx::new(CancellationToken::new(), Duration::from_secs(5));
    cx.replay_mode = replay_mode;
    cx
}

/// M16 replay test (W1d half): `crates` declares `"code"` in `scopes`, claims
/// `"tokio crate latest stable version"` at direct-answer strength (`>= 0.8`),
/// and replays the crates.io fixture offline via `ReplayNet`.
///
/// The kernel ranking rule is verified by W1a
/// (`crates/search/tests/kernel.rs::a_direct_claimant_is_seated_on_web_and_code_with_the_record_on_top`),
/// and end-to-end `SearchService::search` ranking against the real `crates`
/// engine is handed off to W6a (`fsm-server`).
#[tokio::test]
async fn m16_code_scope_crates_tokio_ranks_first() {
    let net = Arc::new(ReplayNet::from_dir(fixtures_dir().join("crates")).unwrap());
    let cfg = Arc::new(Config::default());
    let deps = SourceDeps::new(net.clone(), Arc::new(NullStore::new()), cfg.clone());
    let mut reg = Registry::new();
    fsm_engines_claims::register(&mut reg, &deps).unwrap();

    // 1. Scope resolution & M16 `"code"` scope membership.
    let scope = scopes::resolve("code").expect("code scope");
    assert_eq!(scope.id, "code");
    let code_sources = reg.by_scope("code");
    let code_ids: Vec<&str> = code_sources.iter().map(|s| s.decl().id.as_str()).collect();
    assert!(code_ids.contains(&"crates"));
    assert!(code_ids.contains(&"pypi"));
    assert!(code_ids.contains(&"npm"));

    // 2. Claim evaluation across the registry on `scope="code"` (>= 0.8 direct-answer scale).
    let query = "tokio crate latest stable version";
    let claimants: Vec<_> = reg
        .all()
        .filter(|s| s.claims(query, None) > 0.0 && s.availability(&cfg) == Availability::Available)
        .collect();
    assert_eq!(claimants.len(), 1);
    assert_eq!(claimants[0].decl().id, "crates");
    assert!(claimants[0].claims(query, None) >= 0.8);

    // 3. Offline replay `call()`.
    let (src, op) = reg.resolve_op("crates.search").unwrap();
    let SrcOutput::Hits(crates_hits) = src
        .call(op, json!({"query": query, "limit": 5}), &replay_cx(true))
        .await
        .unwrap()
    else {
        panic!("expected SrcOutput::Hits");
    };
    assert!(
        net.unmatched().is_empty(),
        "unmatched: {:?}",
        net.unmatched()
    );
    assert!(net.unused().is_empty(), "unused: {:?}", net.unused());
    assert_eq!(crates_hits.len(), 1);
    let hit = &crates_hits[0];
    assert_eq!(hit.title, "tokio 1.53.1 on crates.io");
    assert_eq!(hit.url, "https://crates.io/crates/tokio/1.53.1");
    assert_eq!(hit.date.as_deref(), Some("2026-07-20"));
    assert_eq!(hit.source, "crates");
    assert_eq!(hit.kind, HitKind::Record);
}

/// Replay every single one of the 14 native claim engines against `tests/fixtures/`
/// and assert zero unmatched and zero unused exchanges.
#[tokio::test]
async fn replay_all_14_claim_engines_offline() {
    let net = Arc::new(ReplayNet::from_dir(fixtures_dir()).unwrap());
    let cfg = Arc::new(Config::default());
    let deps = SourceDeps::new(net.clone(), Arc::new(NullStore::new()), cfg);
    let mut reg = Registry::new();
    fsm_engines_claims::register(&mut reg, &deps).unwrap();
    let cx = replay_cx(true);

    // 1. crates
    let (src, op) = reg.resolve_op("crates.search").unwrap();
    let SrcOutput::Hits(h) = src
        .call(
            op,
            json!({"query": "tokio crate latest stable version", "limit": 5}),
            &cx,
        )
        .await
        .unwrap()
    else {
        panic!("crates");
    };
    assert_eq!(h[0].title, "tokio 1.53.1 on crates.io");

    // 2. pypi
    let (src, op) = reg.resolve_op("pypi.search").unwrap();
    let SrcOutput::Hits(h) = src
        .call(
            op,
            json!({"query": "fastapi vs starlette latest version", "limit": 5}),
            &cx,
        )
        .await
        .unwrap()
    else {
        panic!("pypi");
    };
    assert_eq!(h[0].title, "fastapi 0.141.1 on PyPI");

    // 3. npm
    let (src, op) = reg.resolve_op("npm.search").unwrap();
    let SrcOutput::Hits(h) = src
        .call(
            op,
            json!({"query": "npm express latest version", "limit": 5}),
            &cx,
        )
        .await
        .unwrap()
    else {
        panic!("npm");
    };
    assert_eq!(h[0].title, "express 5.2.1 on npm");

    // 4. appstore
    let (src, op) = reg.resolve_op("appstore.search").unwrap();
    let SrcOutput::Hits(h) = src
        .call(op, json!({"query": "微信 iOS 版最新版本", "limit": 5}), &cx)
        .await
        .unwrap()
    else {
        panic!("appstore");
    };
    assert_eq!(h[0].title, "微信 8.0.78 on the App Store");

    // 5. endoflife
    let (src, op) = reg.resolve_op("endoflife.search").unwrap();
    let SrcOutput::Hits(h) = src
        .call(
            op,
            json!({"query": "is python 3.9 still supported", "limit": 5}),
            &cx,
        )
        .await
        .unwrap()
    else {
        panic!("endoflife");
    };
    assert_eq!(h[0].url, "https://endoflife.date/python");

    // 6. cisakev
    let (src, op) = reg.resolve_op("cisakev.search").unwrap();
    let SrcOutput::Hits(h) = src
        .call(
            op,
            json!({"query": "is CVE-2026-7273 exploited", "limit": 5}),
            &cx,
        )
        .await
        .unwrap()
    else {
        panic!("cisakev");
    };
    assert!(h[0].title.starts_with("CVE-2026-7273: known exploited"));

    // 7. osv (both POST package lookup and GET advisory lookup)
    let (src, op) = reg.resolve_op("osv.search").unwrap();
    let SrcOutput::Hits(h) = src
        .call(
            op,
            json!({"query": "requests vulnerabilities", "limit": 5}),
            &cx,
        )
        .await
        .unwrap()
    else {
        panic!("osv pkg");
    };
    assert!(h[0].title.starts_with("PYSEC-2026-1872"));
    let SrcOutput::Hits(h) = src
        .call(op, json!({"query": "GHSA-9hjg-9r4m-mvj7", "limit": 5}), &cx)
        .await
        .unwrap()
    else {
        panic!("osv id");
    };
    assert!(h[0].title.starts_with("PYSEC-2026-1872"));

    // 8. frankfurter (ECB + ExchangeRate-API fallback)
    let (src, op) = reg.resolve_op("frankfurter.search").unwrap();
    let SrcOutput::Hits(h) = src
        .call(op, json!({"query": "100 usd to cny", "limit": 5}), &cx)
        .await
        .unwrap()
    else {
        panic!("frankfurter ecb");
    };
    assert_eq!(h[0].title, "USD to CNY: 6.6954 (ECB, 2026-09-21)");
    let SrcOutput::Hits(h) = src
        .call(op, json!({"query": "1000 TWD to CNY", "limit": 5}), &cx)
        .await
        .unwrap()
    else {
        panic!("frankfurter open");
    };
    assert_eq!(
        h[0].title,
        "TWD to CNY: 0.2105 (ExchangeRate-API, 2026-09-22)"
    );

    // 9. cfets
    let (src, op) = reg.resolve_op("cfets.search").unwrap();
    let SrcOutput::Hits(h) = src
        .call(
            op,
            json!({"query": "1万日元等于多少人民币", "limit": 5}),
            &cx,
        )
        .await
        .unwrap()
    else {
        panic!("cfets");
    };
    assert!(h[0].snippet.starts_with("10000 JPY = 427.9400 CNY"));

    // 10. coingecko (unknown coin + known coin)
    let (src, op) = reg.resolve_op("coingecko.search").unwrap();
    let SrcOutput::Hits(h) = src
        .call(
            op,
            json!({"query": "worldcoin token price", "limit": 5}),
            &cx,
        )
        .await
        .unwrap()
    else {
        panic!("coingecko unknown");
    };
    assert_eq!(
        h[0].title,
        "Worldcoin (WLD): 0.92 USD (2026-09-22 03:33 UTC)"
    );
    let SrcOutput::Hits(h) = src
        .call(op, json!({"query": "btc price", "limit": 5}), &cx)
        .await
        .unwrap()
    else {
        panic!("coingecko btc");
    };
    assert!(h[0].title.starts_with("Bitcoin (BTC): 85,485 USD"));

    // 11. gleif
    let (src, op) = reg.resolve_op("gleif.search").unwrap();
    let SrcOutput::Hits(h) = src
        .call(
            op,
            json!({"query": "alibaba group legal entity LEI", "limit": 3}),
            &cx,
        )
        .await
        .unwrap()
    else {
        panic!("gleif");
    };
    assert_eq!(
        h[0].title,
        "Alibaba Group Services Limited (254900KD4P900ITKSU65)"
    );

    // 12. wdi
    let (src, op) = reg.resolve_op("wdi.search").unwrap();
    let SrcOutput::Hits(h) = src
        .call(op, json!({"query": "中国 2025 GDP", "limit": 5}), &cx)
        .await
        .unwrap()
    else {
        panic!("wdi");
    };
    assert_eq!(h[0].title, "China GDP (current US$) 2025: 19.5 trillion");

    // 13. holidays (CN + DE)
    let (src, op) = reg.resolve_op("holidays.search").unwrap();
    let SrcOutput::Hits(h) = src
        .call(op, json!({"query": "2026年中国放假安排", "limit": 5}), &cx)
        .await
        .unwrap()
    else {
        panic!("holidays cn");
    };
    assert_eq!(h[0].title, "2026年中国法定节假日安排（国务院通知）");
    let SrcOutput::Hits(h) = src
        .call(
            op,
            json!({"query": "germany public holidays 2027", "limit": 5}),
            &cx,
        )
        .await
        .unwrap()
    else {
        panic!("holidays de");
    };
    assert_eq!(h[0].title, "Germany public holidays 2027");

    // 14. imf
    let (src, op) = reg.resolve_op("imf.search").unwrap();
    let SrcOutput::Hits(h) = src
        .call(
            op,
            json!({"query": "vietnam real gdp growth", "limit": 5}),
            &cx,
        )
        .await
        .unwrap()
    else {
        panic!("imf");
    };
    assert_eq!(h.len(), 2);
    assert_eq!(
        h[0].title,
        "Real GDP growth — Vietnam (Annual percent change)"
    );
    assert!(h[0].snippet.contains("2024: 6.1 · 2025: 6.5 · 2026: 6.7"));
    assert_eq!(
        h[0].url,
        "https://www.imf.org/external/datamapper/NGDP_RPCH@WEO/VNM"
    );

    assert!(
        net.unmatched().is_empty(),
        "unmatched requests: {:?}",
        net.unmatched()
    );
    assert!(
        net.unused().is_empty(),
        "unused fixture exchanges: {:?}",
        net.unused()
    );
}

#[tokio::test]
async fn replay_cancellation_propagates_timeout() {
    let net = Arc::new(ReplayNet::from_dir(fixtures_dir().join("crates")).unwrap());
    let deps = SourceDeps::new(net, Arc::new(NullStore::new()), Arc::new(Config::default()));
    let mut reg = Registry::new();
    fsm_engines_claims::register(&mut reg, &deps).unwrap();

    let (src, op) = reg.resolve_op("crates.search").unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let cx = SrcCx::new(cancel, Duration::from_secs(5));
    let err = src
        .call(
            op,
            json!({"query": "tokio crate latest version", "limit": 5}),
            &cx,
        )
        .await;
    assert!(matches!(err, Err(FsmError::Timeout(0))));
}

#[tokio::test]
async fn replay_rate_limit_429_propagates() {
    let mut f = Fixture::new("coingecko", "search", "2026-11-25T10:00:00Z");
    f.exchanges.push(Exchange::new(
        FixtureRequest::new(
            "GET",
            "https://api.coingecko.com/api/v3/simple/price?ids=bitcoin&vs_currencies=usd,cny&include_24hr_change=true&include_market_cap=true&include_last_updated_at=true",
        ),
        FixtureResponse::from_bytes(
            429,
            vec![("retry-after".into(), "42".into())],
            b"rate limited",
        ),
    ));
    let net = Arc::new(ReplayNet::from_fixtures(vec![f]).unwrap());
    let deps = SourceDeps::new(net, Arc::new(NullStore::new()), Arc::new(Config::default()));
    let mut reg = Registry::new();
    fsm_engines_claims::register(&mut reg, &deps).unwrap();

    let (src, op) = reg.resolve_op("coingecko.search").unwrap();
    let err = src
        .call(
            op,
            json!({"query": "btc price", "limit": 5}),
            &replay_cx(false),
        )
        .await;
    assert!(matches!(
        err,
        Err(FsmError::RateLimited {
            ref source_id,
            retry_after_s: Some(42)
        }) if source_id == "coingecko"
    ));
}
