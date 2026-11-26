//! Port of `tests/test_record_routing.py` for the 14 claim engines in
//! `fsm-engines-claims`.

use fsm_core::fixture::{Exchange, Fixture, FixtureRequest, FixtureResponse, ReplayNet};
use fsm_core::{Config, NullStore, Registry, Source, SourceDeps, SrcCx, SrcOutput};
use fsm_engines_claims::appstore::{AppStoreEngine, app_name};
use fsm_engines_claims::cfets::CfetsEngine;
use fsm_engines_claims::cisakev::CisaKevEngine;
use fsm_engines_claims::coingecko::{CoinGeckoEngine, coins_in};
use fsm_engines_claims::facts::{countries_in, ecosystem_hints, version_question, year_in};
use fsm_engines_claims::gleif::{GleifEngine, entity_name};
use fsm_engines_claims::holidays::HolidaysEngine;
use fsm_engines_claims::wdi::{WdiEngine, indicators_in};
use serde_json::{Value, json};
use std::collections::HashSet;
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

fn json_exchange(url: &str, body: &Value) -> Exchange {
    Exchange::new(
        FixtureRequest::new("GET", url),
        FixtureResponse::from_bytes(
            200,
            vec![("content-type".into(), "application/json".into())],
            body.to_string().as_bytes(),
        ),
    )
}

fn default_cx() -> SrcCx {
    SrcCx::new(CancellationToken::new(), Duration::from_secs(5))
}

fn claimants_in_registry(reg: &Registry, cfg: &Config, query: &str) -> Vec<String> {
    if query.trim().is_empty() {
        return Vec::new();
    }
    reg.all()
        .filter(|src| {
            src.claims(query, None) > 0.0
                && matches!(src.availability(cfg), fsm_core::Availability::Available)
        })
        .map(|src| src.decl().id.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// Claims: who answers what, without a category
// ---------------------------------------------------------------------------

#[test]
fn test_claimants_by_question_shape() {
    let deps = empty_deps();
    let cfg = Config::default();
    let mut reg = Registry::new();
    fsm_engines_claims::register(&mut reg, &deps).unwrap();

    // Every query from `tests/test_record_routing.py`, projected onto the 14
    // engines owned by `fsm-engines-claims` (engines in other crates — e.g.
    // `nvd`, `wikidata`, `openmeteo`, `rdap`, `mdn`, `ietf`, `worldclock`,
    // `registries`, `github_releases`, `federalregister`, `govuk` — are not in
    // this registry).
    let cases: &[(&str, &[&str])] = &[
        ("latest fastapi version", &["endoflife", "pypi"]),
        ("npm express latest version", &["endoflife", "npm"]),
        ("tokio crate latest version", &["crates"]),
        ("tokio crate latest stable version", &["crates"]),
        ("guava maven latest version", &[]),
        ("微信 iOS 版最新版本", &["appstore"]),
        ("astral-sh/uv latest release", &["endoflife", "pypi"]),
        ("CVE-2024-3094", &["osv", "cisakev"]),
        ("requests vulnerabilities", &["osv"]),
        ("zyxel exploited in the wild", &["cisakev"]),
        ("上海明天天气", &[]),
        ("Shanghai population", &[]),
        ("japan population", &["wdi"]),
        ("example.com domain expiry", &[]),
        ("Array.prototype.toSorted", &[]),
        ("RFC 9110", &[]),
        ("中国 2025 GDP", &["wdi"]),
        ("2026年中国放假安排", &["holidays"]),
        ("现在北京时间几点", &[]),
        ("100 usd to cny", &["frankfurter"]),
        ("1万日元等于多少人民币", &["frankfurter", "cfets"]),
        ("人民币中间价", &["cfets"]),
        ("alibaba group legal entity LEI", &["gleif"]),
        ("btc price", &["coingecko"]),
        ("executive order on AI", &[]),
        ("UK passport renewal fee", &[]),
        ("how to cook rice", &[]),
        ("python asyncio tutorial", &[]),
        ("what is fastapi", &[]),
        ("", &[]),
    ];

    for &(query, expected) in cases {
        let got = claimants_in_registry(&reg, &cfg, query);
        let want: Vec<String> = expected.iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(got, want, "claimants mismatch for query {query:?}");
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

#[test]
fn test_question_shapes() {
    assert!(version_question("latest fastapi version"));
    assert!(version_question("python 3.9 还在支持吗"));
    assert!(!version_question("what is fastapi"));
    assert_eq!(
        ecosystem_hints("go module gin latest"),
        HashSet::from(["go"])
    );
    assert_eq!(
        ecosystem_hints("docker nginx image"),
        HashSet::from(["docker"])
    );
    assert_eq!(ecosystem_hints("微信 iOS 版"), HashSet::from(["app"]));
    assert_eq!(
        countries_in("中国和美国的GDP", 2),
        vec![("CHN", "CN", "China"), ("USA", "US", "United States")]
    );
    assert_eq!(countries_in("uk vs ukraine", 2)[0].0, "GBR");
    assert_eq!(
        countries_in("south korea", 2),
        vec![("KOR", "KR", "South Korea")]
    );
    assert_eq!(year_in("明年放假安排", 2026), 2027);
    assert_eq!(year_in("2025年假期", 2026), 2025);
    assert_eq!(year_in("假期", 2026), 2026);
    assert_eq!(year_in("last year holidays", 2026), 2025);
}

// ---------------------------------------------------------------------------
// App Store
// ---------------------------------------------------------------------------

#[test]
fn test_app_store_extracts_the_app_name_and_puts_it_first() {
    assert_eq!(app_name("微信 iOS 版最新版本"), "微信");
    assert_eq!(app_name("latest version of the Notion iOS app"), "Notion");
    assert!(AppStoreEngine::build_url("微信 iOS 版最新版本", 5).contains("country=cn"));
    assert!(AppStoreEngine::build_url("Notion iOS app version", 5).contains("country=us"));

    let engine = AppStoreEngine::new(empty_deps());
    let payload = json!({
        "_term": "微信",
        "results": [
            {
                "trackName": "秒剪",
                "version": "4.0.15",
                "trackViewUrl": "https://apps.apple.com/cn/app/id1",
                "currentVersionReleaseDate": "2026-09-18T00:00:00Z",
                "sellerName": "Tencent"
            },
            {
                "trackName": "微信",
                "version": "8.0.78",
                "trackViewUrl": "https://apps.apple.com/cn/app/id414478124?uo=4",
                "currentVersionReleaseDate": "2026-09-08T00:00:00Z",
                "sellerName": "Tencent",
                "minimumOsVersion": "15.0",
                "formattedPrice": "免费",
                "averageUserRating": 4.1,
                "releaseNotes": "本次更新：\n- 解决了一些已知问题。"
            }
        ]
    });
    let hits = engine.map_results(&payload);
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].title, "微信 8.0.78 on the App Store");
    assert_eq!(hits[0].url, "https://apps.apple.com/cn/app/id414478124");
    assert!(
        hits[0].snippet.contains(
            "Version 8.0.78, released 2026-09-08 · by Tencent · requires iOS 15.0 · 免费"
        )
    );
    assert!(hits[1].title.starts_with("秒剪"));
}

// ---------------------------------------------------------------------------
// CISA KEV
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_kev_looks_ids_up_reports_absence_and_searches_words() {
    let kev = json!({
        "catalogVersion": "2026.09.21",
        "vulnerabilities": [
            {
                "cveID": "CVE-2026-7273",
                "vendorProject": "Zyxel",
                "product": "GS1900 Series Switches",
                "vulnerabilityName": "Zyxel GS1900 Series Switches Stack-Based Buffer Overflow Vulnerability",
                "dateAdded": "2026-09-21",
                "dueDate": "2026-09-24",
                "knownRansomwareCampaignUse": "Unknown",
                "requiredAction": "Apply mitigations per vendor instructions.",
                "shortDescription": "Zyxel GS1900 switches contain a stack-based buffer overflow."
            },
            {
                "cveID": "CVE-2024-40891",
                "vendorProject": "Zyxel",
                "product": "DSL CPE Devices",
                "vulnerabilityName": "Zyxel DSL CPE OS Command Injection Vulnerability",
                "dateAdded": "2025-02-11",
                "dueDate": "2025-03-04",
                "knownRansomwareCampaignUse": "Unknown"
            }
        ]
    });
    let (net, deps) = replay_deps(vec![json_exchange(
        "https://www.cisa.gov/sites/default/files/feeds/known_exploited_vulnerabilities.json",
        &kev,
    )]);
    let engine = CisaKevEngine::new(deps);
    let cx = default_cx();

    let SrcOutput::Hits(hits1) = engine
        .call(
            "search",
            json!({"query": "is CVE-2026-7273 exploited", "limit": 5}),
            &cx,
        )
        .await
        .unwrap()
    else {
        panic!("expected Hits");
    };
    assert_eq!(hits1.len(), 1);
    assert_eq!(
        hits1[0].title,
        "CVE-2026-7273: known exploited in the wild (CISA KEV, added 2026-09-21)"
    );
    assert!(
        hits1[0]
            .snippet
            .contains("US federal remediation due 2026-09-24")
    );

    let SrcOutput::Hits(hits2) = engine
        .call("search", json!({"query": "CVE-2024-3094", "limit": 5}), &cx)
        .await
        .unwrap()
    else {
        panic!("expected Hits");
    };
    assert_eq!(hits2.len(), 1);
    assert_eq!(
        hits2[0].title,
        "CVE-2024-3094: not in CISA's Known Exploited Vulnerabilities catalogue (as of 2026-09-21)"
    );
    assert!(hits2[0].snippet.contains("not that the flaw is harmless"));

    let SrcOutput::Hits(hits3) = engine
        .call(
            "search",
            json!({"query": "zyxel exploited in the wild", "limit": 5}),
            &cx,
        )
        .await
        .unwrap()
    else {
        panic!("expected Hits");
    };
    let ids: Vec<&str> = hits3
        .iter()
        .map(|h| h.title.split(':').next().unwrap())
        .collect();
    assert_eq!(ids, vec!["CVE-2026-7273", "CVE-2024-40891"]);
    assert_eq!(
        net.hits().len(),
        1,
        "the catalogue is fetched once and kept"
    );
    assert!(CisaKevEngine::answers_directly("CVE-2024-3094"));
    assert!(!CisaKevEngine::answers_directly("zyxel exploited"));
    assert!(engine.claims("CVE-2024-3094", None) > engine.claims("zyxel exploited", None));
}

// ---------------------------------------------------------------------------
// World Bank indicators (WDI)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_wdi_reads_indicator_and_country_from_the_question() {
    let wdi = json!([
        {"page": 1, "pages": 1, "per_page": 50, "total": 3, "sourceid": "2", "lastupdated": "2026-07-13"},
        [
            {"indicator": {"id": "NY.GDP.MKTP.CD", "value": "GDP (current US$)"},
             "country": {"id": "CN", "value": "China"}, "countryiso3code": "CHN",
             "date": "2025", "value": 19498039388042.6},
            {"indicator": {"id": "NY.GDP.MKTP.CD", "value": "GDP (current US$)"},
             "country": {"id": "CN", "value": "China"}, "countryiso3code": "CHN",
             "date": "2024", "value": 18729668435848.0},
            {"indicator": {"id": "NY.GDP.MKTP.CD", "value": "GDP (current US$)"},
             "country": {"id": "CN", "value": "China"}, "countryiso3code": "CHN",
             "date": "2023", "value": null}
        ]
    ]);
    let (net, deps) = replay_deps(vec![json_exchange(
        "https://api.worldbank.org/v2/country/CHN/indicator/NY.GDP.MKTP.CD?format=json&mrv=3",
        &wdi,
    )]);
    let engine = WdiEngine::new(deps);
    let cx = default_cx();

    assert_eq!(
        indicators_in("中国 2025 GDP", 2),
        vec![("NY.GDP.MKTP.CD", "GDP", "current US$")]
    );
    assert_eq!(
        indicators_in("japan population and unemployment", 2),
        vec![
            ("SP.POP.TOTL", "Population", "people"),
            ("SL.UEM.TOTL.ZS", "Unemployment", "% of labour force"),
        ]
    );
    assert_eq!(
        indicators_in("gdp per capita of germany", 2)[0].0,
        "NY.GDP.PCAP.CD"
    );
    // Lookahead-free `人口(?!普查)` guard: census queries must not trigger population indicator.
    assert!(indicators_in("中国人口普查", 2).is_empty());
    assert_eq!(indicators_in("中国人口", 2)[0].0, "SP.POP.TOTL");

    let SrcOutput::Hits(hits) = engine
        .call("search", json!({"query": "中国 2025 GDP", "limit": 5}), &cx)
        .await
        .unwrap()
    else {
        panic!("expected Hits");
    };
    assert_eq!(hits.len(), 1);
    let r = &hits[0];
    assert_eq!(r.title, "China GDP (current US$) 2025: 19.5 trillion");
    assert!(
        r.snippet
            .starts_with("中国（GDP (current US$)）：2025: 19.5 trillion; 2024: 18.7 trillion")
    );
    assert!(r.snippet.contains("data revised 2026-07-13"));
    assert_eq!(
        r.url,
        "https://data.worldbank.org/indicator/NY.GDP.MKTP.CD?locations=CN"
    );
    assert_eq!(r.date.as_deref(), Some("2026-07-13"));
    assert_eq!(net.hits().len(), 1);

    let SrcOutput::Hits(empty1) = engine
        .call(
            "search",
            json!({"query": "gdp of nowhere", "limit": 5}),
            &cx,
        )
        .await
        .unwrap()
    else {
        panic!("expected Hits");
    };
    assert!(empty1.is_empty());
    let SrcOutput::Hits(empty2) = engine
        .call("search", json!({"query": "china weather", "limit": 5}), &cx)
        .await
        .unwrap()
    else {
        panic!("expected Hits");
    };
    assert!(empty2.is_empty());
}

// ---------------------------------------------------------------------------
// Holidays
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_holidays_china_from_the_state_council_notice() {
    let body = json!({
        "year": 2026,
        "papers": ["https://www.gov.cn/zhengce/zhengceku/202511/content_7047091.htm"],
        "days": [
            {"name": "元旦", "date": "2026-01-01", "isOffDay": true},
            {"name": "元旦", "date": "2026-01-02", "isOffDay": true},
            {"name": "元旦", "date": "2026-01-03", "isOffDay": true},
            {"name": "元旦", "date": "2026-01-04", "isOffDay": false},
            {"name": "春节", "date": "2026-02-14", "isOffDay": false},
            {"name": "春节", "date": "2026-02-15", "isOffDay": true},
            {"name": "春节", "date": "2026-02-23", "isOffDay": true}
        ]
    });
    let (net, deps) = replay_deps(vec![json_exchange(
        "https://raw.githubusercontent.com/NateScarlet/holiday-cn/master/2026.json",
        &body,
    )]);
    let engine = HolidaysEngine::new(deps);
    let cx = default_cx();
    let SrcOutput::Hits(hits) = engine
        .call(
            "search",
            json!({"query": "2026年放假安排", "limit": 5}),
            &cx,
        )
        .await
        .unwrap()
    else {
        panic!("expected Hits");
    };
    assert_eq!(hits.len(), 1);
    assert_eq!(
        net.hits()[0].url,
        "https://raw.githubusercontent.com/NateScarlet/holiday-cn/master/2026.json"
    );
    assert_eq!(hits[0].title, "2026年中国法定节假日安排（国务院通知）");
    assert_eq!(
        hits[0].url,
        "https://www.gov.cn/zhengce/zhengceku/202511/content_7047091.htm"
    );
    assert_eq!(
        hits[0].snippet,
        "元旦 01-01至01-03（调休上班 01-04）；春节 02-15至02-23（调休上班 02-14）"
    );
    assert!(engine.claims("2026年放假安排", None) > 0.0);
    assert_eq!(engine.claims("holidays", None), 0.0);
}

#[tokio::test]
async fn test_holidays_elsewhere_from_nager() {
    let body = json!([
        {"date": "2027-01-01", "name": "New Year's Day", "countryCode": "DE", "nationalHoliday": true},
        {"date": "2027-01-06", "name": "Epiphany", "countryCode": "DE", "nationalHoliday": false}
    ]);
    let (net, deps) = replay_deps(vec![json_exchange(
        "https://nagerholidays.com/api/v4/Holidays/DE/2027",
        &body,
    )]);
    let engine = HolidaysEngine::new(deps);
    let cx = default_cx();
    let hits = engine
        .fetch_with_default_year("germany public holidays next year", 5, 2026, &cx)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(
        net.hits()[0].url,
        "https://nagerholidays.com/api/v4/Holidays/DE/2027"
    );
    assert_eq!(hits[0].title, "Germany public holidays 2027");
    assert_eq!(
        hits[0].snippet,
        "01-01 New Year's Day; 01-06 Epiphany (regional)"
    );
    assert_eq!(
        hits[0].url,
        "https://date.nager.at/PublicHoliday/Country/DE/2027"
    );
}

// ---------------------------------------------------------------------------
// CFETS, GLEIF, CoinGecko
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_cfets_reads_the_central_parity_and_per_hundred_pairs() {
    let cfets = json!({
        "data": {"lastDate": "2026-09-22 9:15"},
        "records": [
            {"vrtEName": "USD/CNY", "foreignCName": "USD", "price": "6.7459"},
            {"vrtEName": "100JPY/CNY", "foreignCName": "JPY", "price": "4.2794"},
            {"vrtEName": "CNY/MOP", "foreignCName": "MOP", "price": "1.1913"}
        ]
    });
    let (_net, deps) = replay_deps(vec![json_exchange(
        "https://www.chinamoney.com.cn/r/cms/www/chinamoney/data/fx/ccpr.json",
        &cfets,
    )]);
    let engine = CfetsEngine::new(deps);
    let cx = default_cx();

    let SrcOutput::Hits(h1) = engine
        .call("search", json!({"query": "人民币中间价", "limit": 5}), &cx)
        .await
        .unwrap()
    else {
        panic!("expected Hits");
    };
    assert_eq!(h1[0].title, "USD/CNY 中间价 6.7459（2026-09-22）");
    assert_eq!(h1[0].date.as_deref(), Some("2026-09-22"));
    assert_eq!(h1[0].date_source.as_deref(), Some("api"));

    let SrcOutput::Hits(h2) = engine
        .call(
            "search",
            json!({"query": "1万日元等于多少人民币", "limit": 5}),
            &cx,
        )
        .await
        .unwrap()
    else {
        panic!("expected Hits");
    };
    assert!(
        h2[0].snippet.starts_with(
            "10000 JPY = 427.9400 CNY · 人民币中间价 100JPY/CNY 4.2794（2026-09-22 9:15）"
        ),
        "{}",
        h2[0].snippet
    );

    let SrcOutput::Hits(h3) = engine
        .call(
            "search",
            json!({"query": "100 cny to mop", "limit": 5}),
            &cx,
        )
        .await
        .unwrap()
    else {
        panic!("expected Hits");
    };
    assert!(h3[0].snippet.starts_with("100 CNY = 119.1300 MOP"));
    assert!(engine.claims("1万日元等于多少人民币", None) > 0.0);
    assert_eq!(engine.claims("100 usd to cny", None), 0.0);
}

#[test]
fn test_gleif_uses_the_legal_name_filter_for_cjk_and_maps_records() {
    let engine = GleifEngine::new(empty_deps());
    assert_eq!(
        entity_name("alibaba group legal entity LEI"),
        "alibaba group"
    );
    assert!(
        GleifEngine::build_url("alibaba group legal entity LEI", 3)
            .contains("filter%5Bfulltext%5D=alibaba+group")
    );
    assert!(
        GleifEngine::build_url("阿里巴巴 法人", 3)
            .contains("filter%5Bentity.legalName%5D=%E9%98%BF%E9%87%8C%E5%B7%B4%E5%B7%B4")
    );
    let payload = json!({
        "meta": {"goldenCopy": {"publishDate": "2026-09-21T16:00:00Z"}},
        "data": [
            {"attributes": {
                "lei": "254900KD4P900ITKSU65",
                "entity": {
                    "legalName": {"name": "Alibaba Group Services Limited"},
                    "otherNames": [{"name": "Alibaba.com Group Services Limited"}],
                    "legalAddress": {"city": "Hong Kong", "country": "HK"},
                    "jurisdiction": "HK",
                    "status": "ACTIVE",
                    "registeredAs": "37473977",
                    "creationDate": "2006-12-19T00:00:00Z"
                }
            }}
        ]
    });
    let hits = engine.map_results(&payload);
    assert_eq!(hits.len(), 1);
    let r = &hits[0];
    assert_eq!(
        r.title,
        "Alibaba Group Services Limited (254900KD4P900ITKSU65)"
    );
    assert_eq!(
        r.url,
        "https://search.gleif.org/#/record/254900KD4P900ITKSU65"
    );
    assert!(r.snippet.starts_with(
        "LEI 254900KD4P900ITKSU65 · active · jurisdiction HK · legal address Hong Kong, HK"
    ));
    assert_eq!(r.date.as_deref(), Some("2026-09-21"));
}

#[tokio::test]
async fn test_coingecko_maps_symbols_and_searches_unknown_coins() {
    assert_eq!(
        coins_in("btc and 以太坊 price", 3),
        vec!["bitcoin", "ethereum"]
    );

    let (net, deps) = replay_deps(vec![
        json_exchange(
            "https://api.coingecko.com/api/v3/search?query=worldcoin",
            &json!({"coins": [{"id": "worldcoin-wld", "name": "Worldcoin", "symbol": "WLD"}]}),
        ),
        json_exchange(
            "https://api.coingecko.com/api/v3/simple/price?ids=worldcoin-wld&vs_currencies=usd,cny&include_24hr_change=true&include_market_cap=true&include_last_updated_at=true",
            &json!({"worldcoin-wld": {"usd": 0.92, "cny": 6.16, "usd_24h_change": -0.47, "usd_market_cap": 1_600_000_000, "last_updated_at": 1790048000}}),
        ),
    ]);
    let engine = CoinGeckoEngine::new(deps);
    let cx = default_cx();
    let SrcOutput::Hits(hits) = engine
        .call(
            "search",
            json!({"query": "worldcoin token price", "limit": 5}),
            &cx,
        )
        .await
        .unwrap()
    else {
        panic!("expected Hits");
    };
    assert_eq!(hits.len(), 1);
    let asked = net.hits();
    assert_eq!(
        asked[0].url,
        "https://api.coingecko.com/api/v3/search?query=worldcoin"
    );
    assert!(
        asked[1]
            .url
            .contains("ids=worldcoin-wld&vs_currencies=usd,cny")
    );
    let r = &hits[0];
    assert_eq!(r.title, "Worldcoin (WLD): 0.92 USD (2026-09-22 03:33 UTC)");
    assert!(
        r.snippet
            .contains("1 WLD = 0.92 USD = 6.16 CNY · 24h -0.47% · market cap 1.60 billion USD")
    );
    assert_eq!(r.date.as_deref(), Some("2026-09-22"));

    let btc_hits = engine.map_results(&json!({"bitcoin": {"usd": 85485, "cny": 572421}}));
    assert_eq!(btc_hits[0].title, "Bitcoin (BTC): 85,485 USD");
}
