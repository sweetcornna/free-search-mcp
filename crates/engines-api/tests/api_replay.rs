//! Offline replay and behaviour-oracle tests for all 15 `fsm-engines-api` engines.
//!
//! Test names mirror the Python oracle suites (`test_dev_engines.py`,
//! `test_finance_engines.py`, `test_fact_engines.py`, `test_record_routing.py`,
//! `test_brave_api.py`, `test_serper.py`, `test_serpbase.py`, `test_tavily.py`).

use async_trait::async_trait;
use fsm_core::api::Freshness;
use fsm_core::fixture::ReplayNet;
use fsm_core::{
    Availability, Config, FsmError, Hit, HttpBytes, HttpRequest, NetClient, NetCx, NullStore,
    PoolRole, Registry, Source, SourceDeps, SrcCx, SrcOutput, TransportProfile,
};
use fsm_engines_api::brave_api::BraveApiEngine;
use fsm_engines_api::cninfo::CninfoEngine;
use fsm_engines_api::facts::{candidate_names, ecosystem_hints, version_question};
use fsm_engines_api::github::{GitHubCodeEngine, GitHubEngine, github_get};
use fsm_engines_api::github_releases::{GitHubReleasesEngine, names_repo};
use fsm_engines_api::openmeteo::{OpenMeteoEngine, place_from};
use fsm_engines_api::rdap::{RdapEngine, domains_in};
use fsm_engines_api::registries::{RegistriesEngine, escape_go_module};
use fsm_engines_api::sec_edgar::{SecEdgarEngine, sec_user_agent, split_forms, split_ticker};
use fsm_engines_api::serpbase::SerpBaseEngine;
use fsm_engines_api::serper::SerperEngine;
use fsm_engines_api::tavily::TavilyEngine;
use fsm_engines_api::wikidata::{WikidataEngine, entity_terms};
use fsm_engines_api::worldclock::{WorldClockEngine, zones_in};
use fsm_engines_api::yahoofinance::{YahooFinanceEngine, entity_query};
use jiff::Timestamp;
use jiff::civil::date;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const FIXTURES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

const ALL_15_IDS: [&str; 15] = [
    "github",
    "github_code",
    "github_releases",
    "registries",
    "sec_edgar",
    "yahoofinance",
    "cninfo",
    "worldclock",
    "openmeteo",
    "rdap",
    "wikidata",
    "brave_api",
    "serper",
    "serpbase",
    "tavily",
];

fn replay_deps(cfg: Config) -> (Arc<ReplayNet>, SourceDeps) {
    let net = Arc::new(ReplayNet::from_dir(FIXTURES_DIR).expect("load replay fixtures"));
    let deps = SourceDeps::new(net.clone(), Arc::new(NullStore::new()), Arc::new(cfg));
    (net, deps)
}

fn replay_cx(cfg: &Config) -> SrcCx {
    let mut cx = SrcCx::from_config(cfg, Default::default(), Duration::from_secs(5));
    cx.replay_mode = true;
    cx
}

fn unwrap_hits(out: SrcOutput) -> Vec<Hit> {
    match out {
        SrcOutput::Hits(hits) => hits,
        other => panic!("expected SrcOutput::Hits, got {other:?}"),
    }
}

type MockHandler = Box<dyn Fn(&HttpRequest) -> Result<HttpBytes, FsmError> + Send + Sync>;

/// Capturing mock [`NetClient`] for unit tests that assert exact outgoing requests or error statuses.
struct MockNet {
    requests: Mutex<Vec<HttpRequest>>,
    handler: MockHandler,
}

impl MockNet {
    fn new(
        handler: impl Fn(&HttpRequest) -> Result<HttpBytes, FsmError> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
            handler: Box::new(handler),
        })
    }

    fn json_status(status: u16, body: &str) -> Arc<Self> {
        let body = body.to_owned();
        Self::new(move |req| {
            Ok(HttpBytes::new(
                req.url.clone(),
                status,
                vec![("content-type".into(), "application/json".into())],
                body.clone(),
            ))
        })
    }

    fn captured(&self) -> Vec<HttpRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl NetClient for MockNet {
    async fn send(
        &self,
        req: HttpRequest,
        _profile: TransportProfile,
        cx: &NetCx,
    ) -> Result<HttpBytes, FsmError> {
        if cx.cancel.is_cancelled() {
            return Err(FsmError::Timeout(0));
        }
        self.requests.lock().unwrap().push(req.clone());
        (self.handler)(&req)
    }

    fn check_url_offline(&self, _url: &str) -> Result<(), FsmError> {
        Ok(())
    }
}

fn mock_deps(net: Arc<dyn NetClient>, cfg: Config) -> SourceDeps {
    SourceDeps::new(net, Arc::new(NullStore::new()), Arc::new(cfg))
}

fn cfg_with_key(env_var: &str, value: &str) -> Config {
    let mut cfg = Config::default();
    cfg.api_keys.insert(env_var.to_owned(), value.to_owned());
    cfg
}

// ---------------------------------------------------------------------------
// Registry contract, M16 scope & availability
// ---------------------------------------------------------------------------

#[test]
fn register_registers_all_15_engines_and_satisfies_m16() {
    let (_, deps) = replay_deps(Config::default());
    let mut reg = Registry::new();
    fsm_engines_api::register(&mut reg, &deps).expect("register all 15");

    assert_eq!(reg.len(), 15);
    for id in ALL_15_IDS {
        let src = reg.get(id).unwrap_or_else(|| panic!("missing source {id}"));
        assert_eq!(src.decl().id, id);
        assert!(src.decl().op("search").is_some(), "{id} missing search op");
        assert!(
            !src.decl().summary.is_empty(),
            "{id} must have a non-empty summary"
        );
    }

    // M16: registries declares "code" in scopes and participates in claims().
    let registries = reg.get("registries").unwrap();
    assert!(registries.decl().scopes.iter().any(|s| s == "code"));
    assert!(
        reg.by_scope("code")
            .iter()
            .any(|s| s.decl().id == "registries")
    );
    assert!(registries.claims("guava maven latest version", None) > 0.0);

    // None of our 15 API/keyed engines enter the default web pool.
    for id in ALL_15_IDS {
        let src = reg.get(id).unwrap();
        assert_ne!(
            src.decl().pool_role,
            PoolRole::Default,
            "{id} must not be in PoolRole::Default"
        );
    }

    // WorldClock declares Local transport and 0 cache TTL.
    let wc = reg.get("worldclock").unwrap();
    assert_eq!(wc.decl().transport, TransportProfile::Local);
    assert_eq!(wc.decl().limits.cache_ttl_s, 0);

    // Keyed engines report NeedsKey without keys and Available when configured.
    let empty_cfg = Config::default();
    for (id, env) in [
        ("github_code", "SEARCH_MCP_GITHUB_TOKEN"),
        ("brave_api", "SEARCH_MCP_BRAVE_API_KEY"),
        ("serper", "SEARCH_MCP_SERPER_API_KEY"),
        ("serpbase", "SEARCH_MCP_SERPBASE_API_KEY"),
        ("tavily", "SEARCH_MCP_TAVILY_API_KEY"),
    ] {
        let src = reg.get(id).unwrap();
        assert_eq!(
            src.availability(&empty_cfg),
            Availability::NeedsKey {
                env: env.to_owned()
            }
        );
        let keyed_cfg = cfg_with_key(env, "test-secret");
        assert_eq!(src.availability(&keyed_cfg), Availability::Available);
    }

    // Keyless / optional-key engines stay Available even without keys.
    for id in [
        "github",
        "github_releases",
        "registries",
        "sec_edgar",
        "yahoofinance",
        "cninfo",
        "worldclock",
        "openmeteo",
        "rdap",
        "wikidata",
    ] {
        assert_eq!(
            reg.get(id).unwrap().availability(&empty_cfg),
            Availability::Available
        );
    }
}

#[test]
fn replay_fixtures_contain_no_embedded_api_keys() {
    fn walk(dir: &std::path::Path, count: &mut usize) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, count);
            } else if path.extension().is_some_and(|e| e == "json") {
                *count += 1;
                let text = std::fs::read_to_string(&path).unwrap();
                let low = text.to_ascii_lowercase();
                assert!(
                    !low.contains("\"authorization\"")
                        && !low.contains("\"x-subscription-token\"")
                        && !low.contains("\"x-api-key\""),
                    "fixture {} leaked an auth header",
                    path.display()
                );
            }
        }
    }
    let mut count = 0;
    walk(std::path::Path::new(FIXTURES_DIR), &mut count);
    // 14 networked engines have on-disk replay fixtures (worldclock is TransportProfile::Local).
    assert_eq!(count, 14);
}

// ---------------------------------------------------------------------------
// End-to-end ReplayNet test exercising ALL 15 engines & EVERY fixture exchange
// ---------------------------------------------------------------------------

#[tokio::test]
async fn all_15_engines_replay_offline_with_zero_unmatched_or_unused() {
    let mut cfg = Config::default();
    cfg.api_keys
        .insert("SEARCH_MCP_GITHUB_TOKEN".into(), "ghp_fake".into());
    cfg.api_keys
        .insert("SEARCH_MCP_BRAVE_API_KEY".into(), "brave_fake".into());
    cfg.api_keys
        .insert("SEARCH_MCP_SERPER_API_KEY".into(), "serper_fake".into());
    cfg.api_keys
        .insert("SEARCH_MCP_SERPBASE_API_KEY".into(), "serpbase_fake".into());
    cfg.api_keys
        .insert("SEARCH_MCP_TAVILY_API_KEY".into(), "tvly_fake".into());

    let (net, deps) = replay_deps(cfg.clone());
    let mut reg = Registry::new();
    fsm_engines_api::register(&mut reg, &deps).unwrap();
    let cx = replay_cx(&cfg);

    // 1. github (repos + issues fallback)
    let gh_hits = unwrap_hits(
        reg.get("github")
            .unwrap()
            .call(
                "search",
                json!({"query": "model context protocol", "limit": 10}),
                &cx,
            )
            .await
            .unwrap(),
    );
    assert_eq!(gh_hits.len(), 3);
    assert_eq!(gh_hits[0].title, "modelcontextprotocol/servers");
    assert_eq!(
        gh_hits[0].snippet,
        "★89050 · TypeScript · Model Context Protocol Servers"
    );
    assert_eq!(gh_hits[0].date.as_deref(), Some("2026-07-29"));
    assert!(gh_hits[1].snippet.starts_with("issue open · 4 comments"));
    assert!(gh_hits[2].snippet.starts_with("PR open"));

    // 2. github_code
    let ghc_hits = unwrap_hits(
        reg.get("github_code")
            .unwrap()
            .call("search", json!({"query": "asyncio.run", "limit": 5}), &cx)
            .await
            .unwrap(),
    );
    assert_eq!(ghc_hits.len(), 1);
    assert_eq!(ghc_hits[0].title, "o/r/src/main.py");

    // 3. github_releases (direct owner/repo + repo-search fallback)
    let ghr_direct = unwrap_hits(
        reg.get("github_releases")
            .unwrap()
            .call(
                "search",
                json!({"query": "latest release of astral-sh/uv", "limit": 5}),
                &cx,
            )
            .await
            .unwrap(),
    );
    assert_eq!(ghr_direct.len(), 1);
    assert_eq!(
        ghr_direct[0].title,
        "astral-sh/uv 0.12.17: latest release on GitHub"
    );
    assert_eq!(ghr_direct[0].date.as_deref(), Some("2026-09-18"));
    assert!(!ghr_direct[0].snippet.contains("##"));
    assert!(!ghr_direct[0].snippet.contains("d2b135cf"));
    assert!(
        ghr_direct[0]
            .snippet
            .contains("Reject unsupported Git archive paths")
    );

    let ghr_search = unwrap_hits(
        reg.get("github_releases")
            .unwrap()
            .call(
                "search",
                json!({"query": "zod latest release", "limit": 5}),
                &cx,
            )
            .await
            .unwrap(),
    );
    assert_eq!(ghr_search.len(), 1);
    assert_eq!(
        ghr_search[0].title,
        "colinhacks/zod v3.24.2: latest release on GitHub"
    );

    // 4. registries (all 7 ecosystems)
    let registries = reg.get("registries").unwrap();
    for (q, want_title_substr) in [
        (
            "guava maven latest version",
            "com.google.guava:guava 33.4.8-jre on Maven Central",
        ),
        ("rails gem latest version", "rails 8.1.3.1 on RubyGems"),
        (
            "github.com/gin-gonic/gin go module latest version",
            "github.com/gin-gonic/gin v1.12.0 (Go module)",
        ),
        ("jq homebrew latest version", "jq 1.7.1 on Homebrew"),
        ("docker nginx image latest", "nginx on Docker Hub"),
        (
            "symfony/console composer latest version",
            "symfony/console v7.3.1 on Packagist",
        ),
        (
            "newtonsoft.json nuget latest version",
            "Newtonsoft.Json 13.0.3 on NuGet",
        ),
    ] {
        let hits = unwrap_hits(
            registries
                .call("search", json!({"query": q, "limit": 5}), &cx)
                .await
                .unwrap(),
        );
        assert_eq!(hits.len(), 1, "query {q}");
        assert_eq!(hits[0].title, want_title_substr, "query {q}");
    }

    // 5. sec_edgar (scoped + unscoped fallback)
    let sec = reg.get("sec_edgar").unwrap();
    let sec_hits = unwrap_hits(
        sec.call(
            "search",
            json!({"query": "NVDA 10-K risk factors", "limit": 10}),
            &cx,
        )
        .await
        .unwrap(),
    );
    assert_eq!(sec_hits.len(), 2);
    assert_eq!(sec_hits[0].title, "10-K — Inuvo, Inc. (INUV)");
    assert_eq!(
        sec_hits[0].url,
        "https://www.sec.gov/Archives/edgar/data/829323/000165495426001943/inuvo_10k.htm"
    );
    assert_eq!(sec_hits[1].title, "424B2 — JPMORGAN CHASE & CO (JPM)");

    let sec_fallback = unwrap_hits(
        sec.call("search", json!({"query": "ACME risk", "limit": 5}), &cx)
            .await
            .unwrap(),
    );
    assert_eq!(sec_fallback.len(), 1);

    // 6. yahoofinance
    let yf_hits = unwrap_hits(
        reg.get("yahoofinance")
            .unwrap()
            .call(
                "search",
                json!({"query": "nvidia earnings", "limit": 10}),
                &cx,
            )
            .await
            .unwrap(),
    );
    assert_eq!(yf_hits.len(), 3); // 2 quotes + 1 relevant news (Sandisk dropped!)
    assert_eq!(yf_hits[0].title, "NVDA — NVIDIA Corporation");
    assert_eq!(
        yf_hits[2].title,
        "Wall Street is turning Nvidia's AI chips into a futures market"
    );
    assert_eq!(yf_hits[2].date.as_deref(), Some("2026-08-29"));

    // 7. cninfo
    let cn_hits = unwrap_hits(
        reg.get("cninfo")
            .unwrap()
            .call("search", json!({"query": "人工智能", "limit": 5}), &cx)
            .await
            .unwrap(),
    );
    assert_eq!(cn_hits.len(), 1);
    assert_eq!(
        cn_hits[0].title,
        "佰维存储 (688525) 关于参加科创板人工智能行业集体业绩说明会的公告"
    );
    assert_eq!(
        cn_hits[0].url,
        "https://static.cninfo.com.cn/finalpage/2026-08-29/1225533987.PDF"
    );
    assert_eq!(cn_hits[0].date.as_deref(), Some("2026-08-29"));

    // 8. worldclock (local: 0 HTTP hits)
    let before_wc = net.hits().len();
    let wc_hits = unwrap_hits(
        reg.get("worldclock")
            .unwrap()
            .call(
                "search",
                json!({"query": "what time is it in Tokyo and New York"}),
                &cx,
            )
            .await
            .unwrap(),
    );
    assert_eq!(
        net.hits().len(),
        before_wc,
        "worldclock must not hit network"
    );
    assert_eq!(wc_hits.len(), 2);
    assert!(wc_hits[0].title.contains("Asia/Tokyo"));
    assert!(wc_hits[1].title.contains("America/New_York"));

    // 9. openmeteo
    let om_hits = unwrap_hits(
        reg.get("openmeteo")
            .unwrap()
            .call("search", json!({"query": "上海明天天气", "limit": 5}), &cx)
            .await
            .unwrap(),
    );
    assert_eq!(om_hits.len(), 1);
    assert_eq!(
        om_hits[0].title,
        "上海, 上海市, 中国 天气预报（Open-Meteo）"
    );
    assert!(om_hits[0].snippet.starts_with(
        "现在 27.6°C，局部多云，湿度 57%，风速 5.8 km/h（2026-09-22T10:15 Asia/Shanghai）"
    ));
    assert!(
        om_hits[0]
            .snippet
            .contains("2026-09-23：小毛毛雨，22.1到29.3°C，降水概率 30%，降水量 0.3 mm")
    );
    assert_eq!(om_hits[0].date.as_deref(), Some("2026-09-22"));

    // 10. rdap
    let rdap_hits = unwrap_hits(
        reg.get("rdap")
            .unwrap()
            .call(
                "search",
                json!({"query": "when does www.github.com expire", "limit": 5}),
                &cx,
            )
            .await
            .unwrap(),
    );
    assert_eq!(rdap_hits.len(), 1);
    assert_eq!(
        rdap_hits[0].title,
        "github.com: registered 2007-10-09, expires 2028-10-09"
    );
    assert!(rdap_hits[0].snippet.contains("registrar MarkMonitor Inc."));
    assert!(
        rdap_hits[0]
            .snippet
            .contains("name servers dns1.p08.nsone.net, ns-1283.awsdns-32.org")
    );
    assert_eq!(rdap_hits[0].date.as_deref(), Some("2026-09-07"));

    // 11. wikidata (EN + ZH)
    let wd = reg.get("wikidata").unwrap();
    let wd_en = unwrap_hits(
        wd.call(
            "search",
            json!({"query": "Shanghai population", "limit": 5}),
            &cx,
        )
        .await
        .unwrap(),
    );
    assert_eq!(wd_en.len(), 1);
    assert_eq!(wd_en[0].title, "Shanghai");
    assert_eq!(wd_en[0].url, "https://en.wikipedia.org/wiki/Shanghai");
    assert!(
        wd_en[0]
            .snippet
            .contains("population 24,870,895 (as of 2020)")
    );
    assert!(!wd_en[0].snippet.contains("23,019,148"));
    assert!(wd_en[0].snippet.contains("area 6,341 km²"));
    assert!(wd_en[0].snippet.contains("inception 751"));
    assert!(
        wd_en[0]
            .snippet
            .contains("official website https://shanghai.gov.cn")
    );
    assert!(wd_en[0].snippet.ends_with("Wikidata Q8686"));

    let wd_zh = unwrap_hits(
        wd.call("search", json!({"query": "上海 人口", "limit": 5}), &cx)
            .await
            .unwrap(),
    );
    assert_eq!(wd_zh.len(), 1);
    assert_eq!(wd_zh[0].title, "上海市");
    assert_eq!(wd_zh[0].url, "https://zh.wikipedia.org/wiki/上海市");

    // 12. brave_api
    let brave_hits = unwrap_hits(
        reg.get("brave_api")
            .unwrap()
            .call("search", json!({"query": "hello", "limit": 10}), &cx)
            .await
            .unwrap(),
    );
    assert_eq!(brave_hits.len(), 2);
    assert_eq!(brave_hits[0].title, "Result A");
    assert_eq!(brave_hits[0].snippet, "a matched snippet body");
    assert_eq!(brave_hits[0].date.as_deref(), Some("2024-02-06"));
    assert_eq!(brave_hits[1].date.as_deref(), Some("2026-05-15"));

    // 13. serper
    let serper_hits = unwrap_hits(
        reg.get("serper")
            .unwrap()
            .call("search", json!({"query": "hello", "limit": 10}), &cx)
            .await
            .unwrap(),
    );
    assert_eq!(serper_hits.len(), 3);
    assert_eq!(serper_hits[0].title, "Result A");
    assert_eq!(serper_hits[0].date, None);
    assert_eq!(serper_hits[1].title, "Result B");
    assert_eq!(serper_hits[1].snippet, "a dated result");
    assert_eq!(serper_hits[1].date.as_deref(), Some("2024-01-05"));
    assert_eq!(serper_hits[2].date.as_deref(), Some("2 days ago"));

    // 14. serpbase
    let sb_hits = unwrap_hits(
        reg.get("serpbase")
            .unwrap()
            .call("search", json!({"query": "hello", "limit": 10}), &cx)
            .await
            .unwrap(),
    );
    assert_eq!(sb_hits.len(), 3);
    assert_eq!(sb_hits[1].title, "Result B");
    assert_eq!(sb_hits[1].snippet, "a highlighted result");
    assert_eq!(sb_hits[0].date, None);

    // 15. tavily
    let tv_hits = unwrap_hits(
        reg.get("tavily")
            .unwrap()
            .call("search", json!({"query": "hello", "limit": 10}), &cx)
            .await
            .unwrap(),
    );
    assert_eq!(tv_hits.len(), 2);
    assert_eq!(tv_hits[0].title, "Result A");
    assert_eq!(tv_hits[0].snippet, "an answer oriented snippet body");
    assert_eq!(tv_hits[0].date.as_deref(), Some("2024-02-06"));
    assert_eq!(tv_hits[1].date, None);

    // Every single recorded exchange across all 14 fixtures was served, zero unmatched!
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

// ---------------------------------------------------------------------------
// Ported from test_dev_engines.py (GitHub + GitHubCode)
// ---------------------------------------------------------------------------

#[test]
fn test_github_token_header_only_sent_when_configured() {
    let no_tok = github_get("https://api.github.com/search/repositories", None, "github");
    assert!(
        !no_tok
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("authorization"))
    );

    let with_tok = github_get(
        "https://api.github.com/search/repositories",
        Some("ghp_x"),
        "github",
    );
    assert!(
        with_tok
            .headers
            .iter()
            .any(|(k, v)| k == "Authorization" && v == "Bearer ghp_x")
    );
    assert!(with_tok.is_redacted_header("authorization"));
}

#[tokio::test]
async fn test_github_skips_the_issues_call_when_repos_fill_the_budget() {
    let net = MockNet::json_status(
        200,
        r#"{"items":[{"full_name":"a/b","html_url":"https://github.com/a/b","stargazers_count":5}]}"#,
    );
    let eng = GitHubEngine::new(mock_deps(net.clone(), Config::default()));
    let out = unwrap_hits(
        eng.call(
            "search",
            json!({"query": "q", "limit": 1}),
            &SrcCx::default(),
        )
        .await
        .unwrap(),
    );
    assert_eq!(out.len(), 1);
    assert_eq!(
        net.captured().len(),
        1,
        "issues call should be skipped once budget is full"
    );
}

#[test]
fn test_github_tolerates_structural_surprises() {
    for bad in [
        json!(null),
        json!({}),
        json!({"items": "nope"}),
        json!("garbage"),
        json!([]),
    ] {
        assert!(GitHubEngine::map_repos(&bad).is_empty());
        assert!(GitHubEngine::map_issues(&bad).is_empty());
        assert!(GitHubCodeEngine::map_results(&bad).is_empty());
    }
}

#[tokio::test]
async fn test_github_code_without_a_token_explains_itself() {
    let net = MockNet::new(|_| panic!("must not hit network without token"));
    let eng = GitHubCodeEngine::new(mock_deps(net, Config::default()));
    let err = eng
        .call(
            "search",
            json!({"query": "asyncio.run", "limit": 3}),
            &SrcCx::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        FsmError::AuthRequired { ref source_id, ref env_var }
            if source_id == "github_code" && env_var == "SEARCH_MCP_GITHUB_TOKEN"
    ));
}

#[tokio::test]
async fn test_github_code_rejected_token_surfaces_error() {
    let net = MockNet::json_status(401, r#"{"message":"Bad credentials"}"#);
    let cfg = cfg_with_key("SEARCH_MCP_GITHUB_TOKEN", "bad_tok");
    let eng = GitHubCodeEngine::new(mock_deps(net, cfg));
    let err = eng
        .call(
            "search",
            json!({"query": "x", "limit": 3}),
            &SrcCx::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        FsmError::Upstream {
            status: Some(401),
            ..
        }
    ));
}

// ---------------------------------------------------------------------------
// Ported from test_fact_engines.py & test_record_routing.py (GitHubReleases, Registries)
// ---------------------------------------------------------------------------

#[test]
fn test_names_repo_requires_the_query_to_name_the_repository() {
    assert!(names_repo("zod", &["zod".into()]));
    assert!(names_repo("vscode", &["vs".into(), "code".into()]));
    assert!(names_repo("scikit-learn", &["scikit_learn".into()]));
    assert!(!names_repo("Transcrypt", &["3.9".into()]));
    assert!(!names_repo("awesome-zod", &["zod".into()]));
}

#[tokio::test]
async fn test_github_releases_skips_version_only_query_with_no_names() {
    let net = MockNet::new(|_| panic!("should not issue HTTP call for stop-words-only query"));
    let eng = GitHubReleasesEngine::new(mock_deps(net.clone(), Config::default()));
    let out = unwrap_hits(
        eng.call(
            "search",
            json!({"query": "is python 3.9 still supported", "limit": 5}),
            &SrcCx::default(),
        )
        .await
        .unwrap(),
    );
    assert!(out.is_empty());
    assert!(net.captured().is_empty());
}

#[tokio::test]
async fn test_registries_asks_nothing_when_no_ecosystem_named() {
    let net = MockNet::new(|_| panic!("no ecosystem named, must not call network"));
    let eng = RegistriesEngine::new(mock_deps(net.clone(), Config::default()));
    let out = unwrap_hits(
        eng.call(
            "search",
            json!({"query": "latest fastapi version", "limit": 5}),
            &SrcCx::default(),
        )
        .await
        .unwrap(),
    );
    assert!(out.is_empty());
    assert!(net.captured().is_empty());
}

#[test]
fn test_registries_escape_go_module_and_claims() {
    assert_eq!(
        escape_go_module("github.com/Azure/azure-sdk-for-go"),
        "github.com/!azure/azure-sdk-for-go"
    );
    let (_, deps) = replay_deps(Config::default());
    let eng = RegistriesEngine::new(deps);
    assert!(eng.claims("guava maven latest version", None) > 0.0);
    assert_eq!(eng.claims("guava latest version", None), 0.0);
    assert!(eng.claims("docker nginx image latest", None) > 0.0);
    // PyPI/npm/cargo/app belong to fsm-engines-claims, not registries.
    assert_eq!(eng.claims("tokio crate latest version", None), 0.0);
    assert_eq!(eng.claims("npm express latest version", None), 0.0);
    assert_eq!(eng.claims("latest fastapi python version", None), 0.0);
}

#[test]
fn test_question_shapes_and_candidate_names() {
    assert!(version_question("latest fastapi version"));
    assert!(version_question("python 3.9 还在支持吗"));
    assert!(!version_question("what is fastapi"));
    assert!(ecosystem_hints("go module gin latest").contains("go"));
    assert!(ecosystem_hints("docker nginx image").contains("docker"));
    assert!(ecosystem_hints("微信 iOS 版").contains("app"));
    // B1 oracle parity: verbatim `\b(nuget|dotnet|\.net|c#|csharp)\b`
    assert!(!ecosystem_hints("c# latest version").contains("nuget"));
    assert!(!ecosystem_hints(".net 9 latest version").contains("nuget"));
    assert!(ecosystem_hints("asp.net core latest version").contains("nuget"));
    assert_eq!(
        candidate_names("latest version of @scope/pkg 1.2.3", 3),
        vec!["@scope/pkg"]
    );
}

// ---------------------------------------------------------------------------
// Ported from test_finance_engines.py (SEC EDGAR, Yahoo Finance, cninfo)
// ---------------------------------------------------------------------------

#[test]
fn test_sec_user_agent_carries_no_url_and_includes_contact() {
    let ua = sec_user_agent(None);
    assert!(!ua.to_ascii_lowercase().contains("http"));
    assert!(ua.starts_with("free-search-mcp"));
    assert_eq!(
        sec_user_agent(Some("ops@example.com")),
        "free-search-mcp ops@example.com"
    );
}

#[test]
fn test_sec_splits_form_names_and_tickers() {
    assert_eq!(
        split_forms("NVDA 10-K risk factors"),
        ("NVDA risk factors".into(), vec!["10-K".into()])
    );
    assert_eq!(
        split_forms("tesla 8-K 10-Q"),
        ("tesla".into(), vec!["8-K".into(), "10-Q".into()])
    );
    assert_eq!(
        split_forms("artificial intelligence"),
        ("artificial intelligence".into(), vec![])
    );
    assert_eq!(
        split_forms("SC 13D activist stake"),
        ("activist stake".into(), vec!["SC 13D".into()])
    );
    assert_eq!(split_forms("10-K"), ("10-K".into(), vec!["10-K".into()]));

    assert_eq!(split_ticker("NVDA risk factors").1, "NVDA");
    assert_eq!(split_ticker("NVDA"), ("NVDA".into(), "NVDA".into()));
    assert_eq!(split_ticker("BRK.B annual report").1, "BRK.B");
    assert_eq!(split_ticker("all filings on risk").1, "");
    assert_eq!(split_ticker("nvda risk factors").1, "");
    assert_eq!(split_ticker("US GDP outlook").1, "");
    assert_eq!(split_ticker("AI chips").1, "");
}

#[test]
fn test_sec_build_url_sends_forms_entity_and_custom_date_range() {
    let today = date(2026, 9, 22);
    let url = SecEdgarEngine::build_url("NVDA 10-K risk", Some(Freshness::Month), true, today);
    assert!(url.contains("q=NVDA+risk"));
    assert!(url.contains("entityName=NVDA"));
    assert!(url.contains("forms=10-K"));
    assert!(url.contains("dateRange=custom"));
    assert!(url.contains("startdt=2026-08-22"));
    assert!(url.contains("enddt=2026-09-22"));

    let unscoped = SecEdgarEngine::build_url("NVDA risk factors", None, false, today);
    assert!(!unscoped.contains("entityName"));
    assert!(unscoped.contains("q=NVDA+risk+factors"));
}

#[tokio::test]
async fn test_sec_does_not_retry_when_there_was_no_guess_to_undo() {
    let net = MockNet::json_status(200, r#"{"hits":{"hits":[]}}"#);
    let eng = SecEdgarEngine::new(mock_deps(net.clone(), Config::default()));
    let out = unwrap_hits(
        eng.call(
            "search",
            json!({"query": "artificial intelligence", "limit": 10}),
            &SrcCx::default(),
        )
        .await
        .unwrap(),
    );
    assert!(out.is_empty());
    assert_eq!(net.captured().len(), 1);
}

#[test]
fn test_yahoo_strips_filler_and_filters_irrelevant_news() {
    assert_eq!(entity_query("nvidia earnings"), "nvidia");
    assert_eq!(entity_query("tesla stock price forecast"), "tesla");
    assert_eq!(entity_query("AAPL"), "AAPL");
    assert_eq!(entity_query("stock price"), "stock price");

    let quotes_val = json!([
        {"symbol": "NVDA", "longname": "NVIDIA Corporation", "typeDisp": "Equity", "exchDisp": "NASDAQ", "industryDisp": "Semiconductors"},
        {"symbol": "NVHE-U.TO", "longname": "Harvest NVIDIA Enhanced High Income Shares ETF", "typeDisp": "ETF"}
    ]);
    let quotes = YahooFinanceEngine::map_quotes(Some(&quotes_val));
    assert_eq!(quotes[0].title, "NVDA — NVIDIA Corporation");
    assert_eq!(quotes[0].url, "https://finance.yahoo.com/quote/NVDA");
    assert!(quotes[0].snippet.contains("NASDAQ") && quotes[0].snippet.contains("Semiconductors"));

    let terms = YahooFinanceEngine::relevance_terms("nvidia", &quotes);
    assert!(!terms.contains("high"), "ETF words must not enter terms");
    assert!(terms.contains("nvidia") && terms.contains("nvda"));

    let news_val = json!([
        {"title": "Down 35% From Its High, Is Sandisk Stock a Bargain?", "link": "https://finance.yahoo.com/m/s.html", "providerPublishTime": 1788005497}
    ]);
    assert!(YahooFinanceEngine::map_news(Some(&news_val), "nvidia", &quotes).is_empty());
    assert!(YahooFinanceEngine::map_news(Some(&news_val), "zzzz", &[]).is_empty());
}

#[test]
fn test_cninfo_form_and_cst_date() {
    let today = date(2026, 9, 22);
    let with_fresh = CninfoEngine::build_form("人工智能", 5, Some(Freshness::Month), today);
    assert!(
        with_fresh
            .iter()
            .any(|(k, v)| *k == "searchkey" && v == "人工智能")
    );
    assert!(with_fresh.iter().any(|(k, v)| *k == "pageSize" && v == "5"));
    assert!(
        with_fresh
            .iter()
            .any(|(k, v)| *k == "seDate" && v == "2026-08-22~2026-09-22")
    );

    let without_fresh = CninfoEngine::build_form("x", 5, None, today);
    assert!(!without_fresh.iter().any(|(k, _)| *k == "seDate"));

    // Exact Beijing midnight: 1787932800000 ms == 2026-08-29T00:00:00+08:00 (2026-08-28 UTC).
    assert_eq!(
        CninfoEngine::announced_cst(Some(&json!(1787932800000_i64))),
        "2026-08-29"
    );
}

// ---------------------------------------------------------------------------
// Ported from test_fact_engines.py & test_record_routing.py (WorldClock, OpenMeteo, RDAP, Wikidata)
// ---------------------------------------------------------------------------

#[test]
fn test_worldclock_answers_from_the_zone_database_without_a_request() {
    assert_eq!(
        zones_in("what time is it in Tokyo and New York", 3),
        vec![
            ("Asia/Tokyo".into(), "Tokyo".into()),
            ("America/New_York".into(), "New York".into()),
        ]
    );
    assert_eq!(
        zones_in("现在北京时间几点", 3),
        vec![("Asia/Shanghai".into(), "Beijing".into())]
    );
    assert_eq!(
        zones_in("time in Europe/Berlin", 3),
        vec![("Europe/Berlin".into(), "Berlin".into())]
    );

    let now: Timestamp = "2026-09-22T03:33:33Z".parse().unwrap();
    let (hits, _) = WorldClockEngine::format_zones(now, &zones_in("Tokyo and New York", 3), false);
    assert_eq!(hits.len(), 2);
    assert_eq!(
        hits[0].title,
        "Current time in Tokyo: 2026-09-22 12:33 (Asia/Tokyo)"
    );
    assert!(hits[0].snippet.contains("UTC+09:00, no daylight saving"));
    assert_eq!(
        hits[1].title,
        "Current time in New York: 2026-09-21 23:33 (America/New_York)"
    );
    assert!(
        hits[1]
            .snippet
            .contains("UTC-04:00, daylight saving in force")
    );
    assert_eq!(hits[1].date.as_deref(), Some("2026-09-21"));

    let (zh_hits, _) = WorldClockEngine::format_zones(now, &zones_in("北京", 3), true);
    assert_eq!(zh_hits.len(), 1);
    assert_eq!(
        zh_hits[0].title,
        "北京现在时间：2026-09-22 11:33（Asia/Shanghai）"
    );
    assert!(zh_hits[0].snippet.contains("星期二"));
}

#[tokio::test]
async fn test_worldclock_now_op_returns_structured_records() {
    let eng = WorldClockEngine::new();
    let out = eng
        .call("now", json!({"query": "UTC"}), &SrcCx::default())
        .await
        .unwrap();
    match out {
        SrcOutput::Records { items, hits, .. } => {
            assert_eq!(items.len(), 1);
            assert_eq!(items[0]["zone"], "UTC");
            assert_eq!(items[0]["utcOffset"], "UTC+00:00");
            assert_eq!(hits.unwrap().len(), 1);
        }
        other => panic!("expected Records, got {other:?}"),
    }
}

#[test]
fn test_place_from_and_openmeteo_english_snippet() {
    assert_eq!(place_from("上海明天天气"), "上海");
    assert_eq!(place_from("weather in Berlin tomorrow"), "Berlin");
    assert_eq!(
        place_from("what is the weather like in New York this week?"),
        "New York"
    );
    assert_eq!(place_from("天气预报"), "");

    let forecast = json!({
        "timezone": "Europe/Berlin",
        "current": {"time": "2026-09-22T10:15", "temperature_2m": 27.6, "relative_humidity_2m": 57, "weather_code": 2, "wind_speed_10m": 5.8},
        "daily": {"time": ["2026-09-22", "2026-09-23"], "weather_code": [3, 51], "temperature_2m_max": [29.6, 29.3], "temperature_2m_min": [21.9, 22.1], "precipitation_probability_max": [6, 30], "precipitation_sum": [0.0, 0.3]}
    });
    let spot = json!({"name": "Berlin", "country": "Germany"})
        .as_object()
        .unwrap()
        .clone();
    let hits = OpenMeteoEngine::map_forecast(&forecast, &spot, "", false);
    assert_eq!(hits.len(), 1);
    assert_eq!(
        hits[0].title,
        "Weather forecast for Berlin, Germany (Open-Meteo)"
    );
    assert!(
        hits[0]
            .snippet
            .contains("Now 27.6°C, partly cloudy, humidity 57%")
    );
    assert!(
        hits[0]
            .snippet
            .contains("2026-09-22: overcast, 21.9 to 29.6°C, 6% chance of precipitation, 0.0 mm")
    );
}

#[tokio::test]
async fn test_openmeteo_gives_up_quietly_when_place_unknown() {
    let net = MockNet::json_status(200, r#"{"generationtime_ms":0.5}"#);
    let eng = OpenMeteoEngine::new(mock_deps(net, Config::default()));
    assert!(
        unwrap_hits(
            eng.call(
                "search",
                json!({"query": "weather in Nowhereville"}),
                &SrcCx::default()
            )
            .await
            .unwrap()
        )
        .is_empty()
    );
    assert!(
        unwrap_hits(
            eng.call(
                "search",
                json!({"query": "weather forecast"}),
                &SrcCx::default()
            )
            .await
            .unwrap()
        )
        .is_empty()
    );
}

#[tokio::test]
async fn test_rdap_rejects_http_only_registry_and_ignores_abbreviations() {
    assert_eq!(
        domains_in("when does www.github.com expire, e.g. soon?", 2),
        vec!["www.github.com"]
    );
    let (net, deps) = replay_deps(Config::default());
    let eng = RdapEngine::new(deps);
    let cx = replay_cx(&Config::default());
    // .kg in the bootstrap fixture is http-only -> skipped, no domain request issued.
    let out = unwrap_hits(
        eng.call("search", json!({"query": "example.kg registrar"}), &cx)
            .await
            .unwrap(),
    );
    assert!(out.is_empty());
    assert_eq!(net.hits().len(), 1, "only bootstrap was fetched");
}

#[test]
fn test_wikidata_entity_terms_and_item_fallback() {
    assert_eq!(entity_terms("Shanghai population"), "Shanghai");
    assert_eq!(entity_terms("上海 人口"), "上海");
    assert_eq!(
        entity_terms("when was the Eiffel Tower built"),
        "Eiffel Tower built"
    );
    assert_eq!(entity_terms("population"), "");

    let hits = WikidataEngine::map_entities(
        &json!({"entities": {"Q1": {"labels": {"en": {"value": "Thing"}}}}}),
        "en",
    );
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].url, "https://www.wikidata.org/wiki/Q1");

    assert!(
        WikidataEngine::map_entities(&json!({"entities": {"Q1": {"missing": ""}}}), "en")
            .is_empty()
    );
}

#[test]
fn test_claimants_by_question_shape_for_w1c_engines() {
    let (_, deps) = replay_deps(Config::default());
    let mut reg = Registry::new();
    fsm_engines_api::register(&mut reg, &deps).unwrap();

    let claimants = |q: &str| -> Vec<String> {
        reg.all()
            .filter(|s| s.claims(q, None) > 0.0)
            .map(|s| s.decl().id.clone())
            .collect()
    };

    assert_eq!(claimants("guava maven latest version"), vec!["registries"]);
    assert_eq!(
        claimants("astral-sh/uv latest release"),
        vec!["github_releases"]
    );
    assert_eq!(claimants("上海明天天气"), vec!["openmeteo"]);
    assert_eq!(claimants("Shanghai population"), vec!["wikidata"]);
    assert_eq!(claimants("example.com domain expiry"), vec!["rdap"]);
    assert_eq!(claimants("现在北京时间几点"), vec!["worldclock"]);
    assert!(claimants("how to cook rice").is_empty());
    assert!(claimants("python asyncio tutorial").is_empty());
    assert!(claimants("").is_empty());
}

// ---------------------------------------------------------------------------
// Ported from test_brave_api.py
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_brave_api_sends_auth_header_clamps_count_and_handles_filters() {
    let net = MockNet::json_status(200, r#"{"web":{"results":[]}}"#);
    let cfg = cfg_with_key("SEARCH_MCP_BRAVE_API_KEY", "SECRET-TOKEN");
    let eng = BraveApiEngine::new(mock_deps(net.clone(), cfg.clone()));
    let mut cx = SrcCx::from_config(&cfg, Default::default(), Duration::from_secs(5));
    cx.freshness = Some(Freshness::Week);
    cx.domains = vec!["docs.rs".into(), "-spam.com".into()];

    eng.call("search", json!({"query": "hello", "limit": 9999}), &cx)
        .await
        .unwrap();
    let reqs = net.captured();
    assert_eq!(reqs.len(), 1);
    let req = &reqs[0];
    assert!(
        req.headers
            .iter()
            .any(|(k, v)| k == "X-Subscription-Token" && v == "SECRET-TOKEN")
    );
    assert!(req.is_redacted_header("x-subscription-token"));
    assert!(req.query.iter().any(|(k, v)| k == "count" && v == "20"));
    assert!(req.query.iter().any(|(k, v)| k == "freshness" && v == "pw"));
    assert!(
        req.query
            .iter()
            .any(|(k, v)| k == "q" && v == "hello site:docs.rs -site:spam.com")
    );
}

#[tokio::test]
async fn test_brave_api_missing_key_quota_500_malformed_and_truncation() {
    // Missing key -> AuthRequired, zero HTTP requests
    let no_call = MockNet::new(|_| panic!("must not fetch without key"));
    let eng = BraveApiEngine::new(mock_deps(no_call, Config::default()));
    let err = eng
        .call("search", json!({"query": "hello"}), &SrcCx::default())
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        FsmError::AuthRequired { ref source_id, ref env_var }
            if source_id == "brave_api" && env_var == "SEARCH_MCP_BRAVE_API_KEY"
    ));

    let cfg = cfg_with_key("SEARCH_MCP_BRAVE_API_KEY", "FAKE");

    // 429 -> RateLimited
    let eng429 = BraveApiEngine::new(mock_deps(
        MockNet::json_status(429, "Too Many"),
        cfg.clone(),
    ));
    assert!(matches!(
        eng429
            .call("search", json!({"query": "hello"}), &SrcCx::default())
            .await,
        Err(FsmError::RateLimited { .. })
    ));

    // 500 -> Upstream(500) per I2 convention
    let eng500 = BraveApiEngine::new(mock_deps(
        MockNet::json_status(500, "Server Err"),
        cfg.clone(),
    ));
    assert!(matches!(
        eng500
            .call("search", json!({"query": "hello"}), &SrcCx::default())
            .await,
        Err(FsmError::Upstream {
            ref source_id,
            status: Some(500),
            ..
        }) if source_id == "brave_api"
    ));

    // Malformed JSON -> Upstream(200, invalid JSON) per I2 convention
    let eng_bad = BraveApiEngine::new(mock_deps(MockNet::json_status(200, "<<<"), cfg.clone()));
    assert!(matches!(
        eng_bad
            .call("search", json!({"query": "hello"}), &SrcCx::default())
            .await,
        Err(FsmError::Upstream {
            ref source_id,
            status: Some(200),
            ref message,
        }) if source_id == "brave_api" && message.contains("invalid JSON")
    ));

    // Missing `web` envelope -> empty
    let eng_no_web = BraveApiEngine::new(mock_deps(
        MockNet::json_status(200, r#"{"query":{"original":"hello"}}"#),
        cfg.clone(),
    ));
    assert!(
        unwrap_hits(
            eng_no_web
                .call("search", json!({"query": "hello"}), &SrcCx::default())
                .await
                .unwrap()
        )
        .is_empty()
    );

    // Skips missing title/url + truncates to limit
    let eng_skip = BraveApiEngine::new(mock_deps(
        MockNet::json_status(
            200,
            r#"{"web":{"results":[{"title":"","url":"https://a.com"},{"title":"No URL","url":""},{"title":"Good1","url":"https://example.com/1"},{"title":"Good2","url":"https://example.com/2"}]}}"#,
        ),
        cfg,
    ));
    let out = unwrap_hits(
        eng_skip
            .call(
                "search",
                json!({"query": "hello", "limit": 1}),
                &SrcCx::default(),
            )
            .await
            .unwrap(),
    );
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].url, "https://example.com/1");
}

// ---------------------------------------------------------------------------
// Ported from test_serper.py
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_serper_clamps_num_floor_and_ceiling_and_sends_tbs() {
    let net = MockNet::json_status(200, r#"{"organic":[]}"#);
    let cfg = cfg_with_key("SEARCH_MCP_SERPER_API_KEY", "FAKE");
    let eng = SerperEngine::new(mock_deps(net.clone(), cfg.clone()));

    let mut cx = SrcCx::from_config(&cfg, Default::default(), Duration::from_secs(5));
    cx.freshness = Some(Freshness::Week);
    eng.call("search", json!({"query": "hello", "limit": 9999}), &cx)
        .await
        .unwrap();

    // Floor check (limit=3 -> num=10)
    eng.call(
        "search",
        json!({"query": "hello", "limit": 3}),
        &SrcCx::default(),
    )
    .await
    .unwrap();

    let reqs = net.captured();
    assert_eq!(reqs.len(), 2);
    let body0: Value = serde_json::from_slice(reqs[0].body.as_ref().unwrap()).unwrap();
    assert_eq!(body0["num"], 20);
    assert_eq!(body0["q"], "hello");
    assert_eq!(body0["gl"], "us");
    assert_eq!(body0["hl"], "en");
    assert_eq!(body0["tbs"], "qdr:w");
    assert!(
        reqs[0]
            .headers
            .iter()
            .any(|(k, v)| k == "X-API-KEY" && v == "FAKE")
    );
    assert!(reqs[0].is_redacted_header("x-api-key"));

    let body1: Value = serde_json::from_slice(reqs[1].body.as_ref().unwrap()).unwrap();
    assert_eq!(body1["num"], 10);
    assert!(body1.get("tbs").is_none());
}

#[tokio::test]
async fn test_serper_missing_key_403_500_and_malformed() {
    let eng_nokey = SerperEngine::new(mock_deps(
        MockNet::new(|_| panic!("no network")),
        Config::default(),
    ));
    assert!(matches!(
        eng_nokey
            .call("search", json!({"query": "hello"}), &SrcCx::default())
            .await,
        Err(FsmError::AuthRequired { .. })
    ));

    let cfg = cfg_with_key("SEARCH_MCP_SERPER_API_KEY", "FAKE");
    let eng403 = SerperEngine::new(mock_deps(
        MockNet::json_status(403, "Forbidden"),
        cfg.clone(),
    ));
    assert!(matches!(
        eng403
            .call("search", json!({"query": "hello"}), &SrcCx::default())
            .await,
        Err(FsmError::Upstream {
            status: Some(403),
            ..
        })
    ));

    let eng500 = SerperEngine::new(mock_deps(MockNet::json_status(500, "Err"), cfg.clone()));
    assert!(matches!(
        eng500
            .call("search", json!({"query": "hello"}), &SrcCx::default())
            .await,
        Err(FsmError::Upstream {
            ref source_id,
            status: Some(500),
            ..
        }) if source_id == "serper"
    ));

    let eng_bad = SerperEngine::new(mock_deps(MockNet::json_status(200, "<<<"), cfg));
    assert!(matches!(
        eng_bad
            .call("search", json!({"query": "hello"}), &SrcCx::default())
            .await,
        Err(FsmError::Upstream {
            ref source_id,
            status: Some(200),
            ref message,
        }) if source_id == "serper" && message.contains("invalid JSON")
    ));
}

// ---------------------------------------------------------------------------
// Ported from test_serpbase.py (envelope status mapping!)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_serpbase_envelope_codes_and_http_errors() {
    let eng_nokey = SerpBaseEngine::new(mock_deps(
        MockNet::new(|_| panic!("no network")),
        Config::default(),
    ));
    assert!(matches!(
        eng_nokey
            .call("search", json!({"query": "hello"}), &SrcCx::default())
            .await,
        Err(FsmError::AuthRequired { .. })
    ));

    let cfg = cfg_with_key("SEARCH_MCP_SERPBASE_API_KEY", "FAKE");

    // 1001 -> rejected credential
    let e1001 = SerpBaseEngine::new(mock_deps(
        MockNet::json_status(200, r#"{"status":1001,"error":"unauthorized"}"#),
        cfg.clone(),
    ));
    let err = e1001
        .call("search", json!({"query": "hello"}), &SrcCx::default())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("status 1001"));

    // 1003 -> forbidden
    let e1003 = SerpBaseEngine::new(mock_deps(
        MockNet::json_status(200, r#"{"status":1003}"#),
        cfg.clone(),
    ));
    let err = e1003
        .call("search", json!({"query": "hello"}), &SrcCx::default())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("status 1003"));

    // 1020 -> out of credits
    let e1020 = SerpBaseEngine::new(mock_deps(
        MockNet::json_status(200, r#"{"status":1020}"#),
        cfg.clone(),
    ));
    let err = e1020
        .call("search", json!({"query": "hello"}), &SrcCx::default())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("out of credits"));

    // 1029 -> rate limit
    let e1029 = SerpBaseEngine::new(mock_deps(
        MockNet::json_status(200, r#"{"status":1029}"#),
        cfg.clone(),
    ));
    assert!(matches!(
        e1029
            .call("search", json!({"query": "hello"}), &SrcCx::default())
            .await,
        Err(FsmError::RateLimited { .. })
    ));

    // Non-zero upstream error envelopes (1000, 1500, 1502, 1503, 1504) -> Upstream error per I2
    for code in [1000, 1500, 1502, 1503, 1504] {
        let eng = SerpBaseEngine::new(mock_deps(
            MockNet::json_status(200, &format!(r#"{{"status":{code}}}"#)),
            cfg.clone(),
        ));
        let err = eng
            .call("search", json!({"query": "hello"}), &SrcCx::default())
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                FsmError::Upstream {
                    ref source_id,
                    status: Some(200),
                    ref message,
                } if source_id == "serpbase" && message.contains(&format!("status {code}"))
            ),
            "envelope {code} should surface Upstream error, got {err:?}"
        );
    }

    // Request shape: num clamped 1..100, X-API-Key header redacted
    let net = MockNet::json_status(200, r#"{"status":0,"organic":[]}"#);
    let eng_req = SerpBaseEngine::new(mock_deps(net.clone(), cfg));
    eng_req
        .call(
            "search",
            json!({"query": "hello", "limit": 9999}),
            &SrcCx::default(),
        )
        .await
        .unwrap();
    let req = &net.captured()[0];
    let body: Value = serde_json::from_slice(req.body.as_ref().unwrap()).unwrap();
    assert_eq!(body["num"], 100);
    assert!(
        req.headers
            .iter()
            .any(|(k, v)| k == "X-API-Key" && v == "FAKE")
    );
    assert!(req.is_redacted_header("x-api-key"));
}

// ---------------------------------------------------------------------------
// Ported from test_tavily.py
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_tavily_bearer_auth_clamping_and_native_domain_filters() {
    let net = MockNet::json_status(200, r#"{"results":[]}"#);
    let cfg = cfg_with_key("SEARCH_MCP_TAVILY_API_KEY", "SECRETKEY");
    let eng = TavilyEngine::new(mock_deps(net.clone(), cfg.clone()));

    let mut cx = SrcCx::from_config(&cfg, Default::default(), Duration::from_secs(5));
    cx.domains = vec!["example.com".into(), "-spam.com".into()];
    eng.call("search", json!({"query": "hello", "limit": 9999}), &cx)
        .await
        .unwrap();

    // Min clamp (limit=0 -> max_results=1) and no domain keys when empty
    eng.call(
        "search",
        json!({"query": "hello", "limit": 0}),
        &SrcCx::default(),
    )
    .await
    .unwrap();

    let reqs = net.captured();
    assert_eq!(reqs.len(), 2);
    assert!(
        reqs[0]
            .headers
            .iter()
            .any(|(k, v)| k == "Authorization" && v == "Bearer SECRETKEY")
    );
    assert!(reqs[0].is_redacted_header("authorization"));
    let body0: Value = serde_json::from_slice(reqs[0].body.as_ref().unwrap()).unwrap();
    assert!(body0.get("api_key").is_none());
    assert_eq!(body0["max_results"], 20);
    assert_eq!(body0["search_depth"], "basic");
    assert_eq!(body0["include_answer"], false);
    assert_eq!(body0["include_raw_content"], false);
    assert_eq!(body0["include_domains"], json!(["example.com"]));
    assert_eq!(body0["exclude_domains"], json!(["spam.com"]));

    let body1: Value = serde_json::from_slice(reqs[1].body.as_ref().unwrap()).unwrap();
    assert_eq!(body1["max_results"], 1);
    assert!(body1.get("include_domains").is_none());
    assert!(body1.get("exclude_domains").is_none());
}

#[tokio::test]
async fn test_tavily_missing_key_401_500_entities_and_domain_post_filter() {
    let eng_nokey = TavilyEngine::new(mock_deps(
        MockNet::new(|_| panic!("no network")),
        Config::default(),
    ));
    assert!(matches!(
        eng_nokey
            .call("search", json!({"query": "hello"}), &SrcCx::default())
            .await,
        Err(FsmError::AuthRequired { .. })
    ));

    let cfg = cfg_with_key("SEARCH_MCP_TAVILY_API_KEY", "FAKE");
    let eng401 = TavilyEngine::new(mock_deps(
        MockNet::json_status(401, "Unauthorized"),
        cfg.clone(),
    ));
    assert!(matches!(
        eng401
            .call("search", json!({"query": "hello"}), &SrcCx::default())
            .await,
        Err(FsmError::Upstream {
            status: Some(401),
            ..
        })
    ));

    let eng_ent = TavilyEngine::new(mock_deps(
        MockNet::json_status(
            200,
            r#"{"results":[{"title":"A &amp; <b>B</b>","url":"https://allowed.com/1","content":"&quot;clean&quot; text"},{"title":"Blocked","url":"https://blocked.com/2","content":"x"}]}"#,
        ),
        cfg.clone(),
    ));
    let mut cx = SrcCx::from_config(&cfg, Default::default(), Duration::from_secs(5));
    cx.domains = vec!["allowed.com".into()];
    let out = unwrap_hits(
        eng_ent
            .call("search", json!({"query": "hello", "limit": 5}), &cx)
            .await
            .unwrap(),
    );
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].title, "A & B");
    assert_eq!(out[0].snippet, "\"clean\" text");
}

#[tokio::test]
async fn test_cancellation_propagates_timeout_zero() {
    let (_, deps) = replay_deps(Config::default());
    let eng = GitHubEngine::new(deps);
    let cx = SrcCx::default();
    cx.cancel.cancel();
    let err = eng
        .call("search", json!({"query": "model context protocol"}), &cx)
        .await
        .unwrap_err();
    assert!(matches!(err, FsmError::Timeout(0)));

    let wc = WorldClockEngine::new();
    let err = wc
        .call("search", json!({"query": "Tokyo"}), &cx)
        .await
        .unwrap_err();
    assert!(matches!(err, FsmError::Timeout(0)));
}

// ---------------------------------------------------------------------------
// Deepened edge-case tests on the riskiest logic
// ---------------------------------------------------------------------------

#[test]
fn test_common_clip_unescape_compact_and_charset() {
    use fsm_engines_api::common::{
        augment_query_with_operators, clip, compact_number, decode_body, unescape_html,
    };

    // Multi-byte UTF-8 clipping never splits a char and trims trailing space before " …".
    assert_eq!(clip("  hello   world  ", 20), "hello world");
    assert_eq!(clip("中文测试字符串很长", 4), "中文测试 …");
    assert_eq!(clip("abc def", 4), "abc …");

    // HTML entities: named, decimal, hex, unknown passthrough, unclosed &.
    assert_eq!(
        unescape_html("A &amp; B &#39;ok&#x2019; &unknown; &unclosed"),
        "A & B 'ok\u{2019} &unknown; &unclosed"
    );

    // Thousands formatting.
    assert_eq!(compact_number("24870895"), "24,870,895");
    assert_eq!(compact_number("-1000000.9"), "-1,000,000");
    assert_eq!(compact_number("not-a-number"), "not-a-number");

    // Multiple include domains group with OR; exclude domains get -site:.
    assert_eq!(
        augment_query_with_operators(
            "rust",
            &["docs.rs".into(), "crates.io".into()],
            &["spam.org".into()],
            Some("pdf"),
        ),
        "rust (site:docs.rs OR site:crates.io) -site:spam.org filetype:pdf"
    );

    // Non-UTF-8 charset decoded via encoding_rs (GBK / GB18030 bytes for "中文").
    let (gbk_bytes, _, _) = encoding_rs::GB18030.encode("{\"t\":\"中文\"}");
    let resp = HttpBytes::new(
        "https://example.cn/api",
        200,
        vec![(
            "Content-Type".into(),
            "application/json; charset=gb18030".into(),
        )],
        gbk_bytes.into_owned(),
    );
    assert_eq!(decode_body(&resp), "{\"t\":\"中文\"}");
}

#[test]
fn test_script_and_region_detection_edge_cases() {
    use fsm_engines_api::facts::{detect_query_region, query_is_chinese};

    // Japanese kanji + kana tie must select jp-ja, NOT cn-zh!
    assert_eq!(detect_query_region("人工知能ニュース", "us-en"), "jp-ja");
    assert!(!query_is_chinese("人工知能ニュース"));
    // Korean Hangul selects kr-ko.
    assert_eq!(detect_query_region("파이썬 뉴스", "us-en"), "kr-ko");
    assert!(!query_is_chinese("파이썬 뉴스"));
    // Mixed Latin + Chinese above 30% threshold selects cn-zh.
    assert_eq!(detect_query_region("AI 新闻", "us-en"), "cn-zh");
    assert!(query_is_chinese("AI 新闻"));
    // Operator's configured tw-zh is preserved when detected lang is also zh.
    assert_eq!(detect_query_region("人工智能", "tw-zh"), "tw-zh");
    // Persian and Urdu distinctive letters override Arabic.
    assert_eq!(detect_query_region("اخبار فارسی گپ", "us-en"), "ir-fa");
    assert_eq!(detect_query_region("اردو خبریں ٹڈ", "us-en"), "pk-ur");
}

#[test]
fn test_worldclock_ascii_boundaries_and_3_part_iana() {
    // Substring traps must not match short ASCII timezone aliases (est, cst, ist, uk).
    assert!(zones_in("best podcast history ukulele", 3).is_empty());
    // "ukraine" must match Europe/Kyiv, NOT Europe/London via "uk".
    assert_eq!(
        zones_in("what time is it in ukraine", 3),
        vec![("Europe/Kyiv".into(), "Kyiv".into())]
    );
    // Explicit aliases bounded by whitespace / punctuation DO match.
    assert_eq!(
        zones_in("what time is it in EST?", 3),
        vec![("America/New_York".into(), "New York".into())]
    );
    // 3-segment IANA zone and invalid IANA zone handling.
    assert_eq!(
        zones_in("time in America/Argentina/Buenos_Aires and Mars/Olympus", 3),
        vec![(
            "America/Argentina/Buenos_Aires".into(),
            "Buenos Aires".into()
        )]
    );
}

#[tokio::test]
async fn test_worldclock_defaults_to_beijing_for_zh_and_utc_for_en() {
    let eng = WorldClockEngine::new();
    let zh = unwrap_hits(
        eng.call("search", json!({"query": "现在几点"}), &SrcCx::default())
            .await
            .unwrap(),
    );
    assert_eq!(zh.len(), 1);
    assert!(zh[0].title.contains("Asia/Shanghai"));

    let en = unwrap_hits(
        eng.call(
            "search",
            json!({"query": "what time is it"}),
            &SrcCx::default(),
        )
        .await
        .unwrap(),
    );
    assert_eq!(en.len(), 1);
    assert!(en[0].title.contains("(UTC)"));
}

#[test]
fn test_wikidata_bce_precision_coords_units_and_tiebreak() {
    let payload = json!({
        "entities": {
            "Q42": {
                "labels": {"en": {"value": "Parthenon"}},
                "claims": {
                    "P571": [{"mainsnak": {"datavalue": {"type": "time", "value": {"time": "-0447-00-00T00:00:00Z", "precision": 9}}}}],
                    "P1619": [{"mainsnak": {"datavalue": {"type": "time", "value": {"time": "+2020-05-00T00:00:00Z", "precision": 10}}}}],
                    "P577": [{"mainsnak": {"datavalue": {"type": "time", "value": {"time": "+2021-06-15T00:00:00Z", "precision": 11}}}}],
                    "P625": [{"mainsnak": {"datavalue": {"type": "globecoordinate", "value": {"latitude": 37.971528, "longitude": 23.726716}}}}],
                    "P2048": [{"mainsnak": {"datavalue": {"type": "quantity", "value": {"amount": "+13.7", "unit": "http://www.wikidata.org/entity/Q99999"}}}}],
                    // Tie-break: neither has P585 -> first statement must win (matching Python max()).
                    "P856": [
                        {"mainsnak": {"datavalue": {"type": "string", "value": "https://first.example"}}},
                        {"mainsnak": {"datavalue": {"type": "string", "value": "https://second.example"}}}
                    ]
                }
            }
        }
    });
    let hits = WikidataEngine::map_entities(&payload, "en");
    assert_eq!(hits.len(), 1);
    let s = &hits[0].snippet;
    assert!(s.contains("inception 447 BCE"), "{s}");
    assert!(s.contains("opened 2020-05"), "{s}");
    assert!(s.contains("published 2021-06-15"), "{s}");
    assert!(s.contains("coordinates 37.9715, 23.7267"), "{s}");
    assert!(s.contains("height 13 (Q99999)"), "{s}");
    assert!(s.contains("official website https://first.example"), "{s}");
}

#[tokio::test]
async fn test_rdap_public_suffix_and_bootstrap_cache_vs_replay_mode() {
    use fsm_engines_api::rdap::registrable_domain;

    assert_eq!(registrable_domain("www.bbc.co.uk"), "bbc.co.uk");
    assert_eq!(registrable_domain("news.sina.com.cn"), "sina.com.cn");
    assert_eq!(registrable_domain("sub.docs.github.com"), "github.com");

    // Non-replay mode caches bootstrap across calls (dns.json fetched once).
    let net = MockNet::new(|req| {
        if req.url.ends_with("dns.json") {
            Ok(HttpBytes::new(
                req.url.clone(),
                200,
                vec![],
                r#"{"services":[[["com"],["https://rdap.verisign.com/com/v1/"]]]}"#,
            ))
        } else {
            Ok(HttpBytes::new(
                req.url.clone(),
                200,
                vec![],
                r#"{"ldhName":"EXAMPLE.COM"}"#,
            ))
        }
    });
    let eng = RdapEngine::new(mock_deps(net.clone(), Config::default()));
    let normal_cx = SrcCx::default();
    eng.call("search", json!({"query": "example.com whois"}), &normal_cx)
        .await
        .unwrap();
    eng.call("search", json!({"query": "example.com whois"}), &normal_cx)
        .await
        .unwrap();
    let bootstrap_calls = net
        .captured()
        .iter()
        .filter(|r| r.url.ends_with("dns.json"))
        .count();
    assert_eq!(
        bootstrap_calls, 1,
        "bootstrap must be cached in normal mode"
    );

    // replay_mode bypasses in-memory bootstrap cache.
    let mut replay = SrcCx::default();
    replay.replay_mode = true;
    eng.call("search", json!({"query": "example.com whois"}), &replay)
        .await
        .unwrap();
    let bootstrap_calls_after = net
        .captured()
        .iter()
        .filter(|r| r.url.ends_with("dns.json"))
        .count();
    assert_eq!(
        bootstrap_calls_after, 2,
        "replay_mode must bypass in-memory bootstrap cache"
    );
}

#[tokio::test]
async fn test_yahoo_finance_interleaves_quotes_and_news_within_budget() {
    let payload = json!({
        "quotes": [
            {"symbol": "TSLA", "longname": "Tesla, Inc."},
            {"symbol": "TSLL", "longname": "Direxion Tesla 2X"},
            {"symbol": "TSLQ", "longname": "AXS Short Tesla"},
            {"symbol": "TSLY", "longname": "YieldMax Tesla"},
        ],
        "news": [
            {"title": "Tesla reports quarterly deliveries", "link": "https://finance.yahoo.com/news/tsla-1.html", "providerPublishTime": 1788005497},
            {"title": "TSLA shares rise on energy growth", "link": "https://finance.yahoo.com/news/tsla-2.html", "providerPublishTime": 1788005497},
        ]
    });
    let net = MockNet::json_status(200, &payload.to_string());
    let eng = YahooFinanceEngine::new(mock_deps(net, Config::default()));
    let hits = unwrap_hits(
        eng.call(
            "search",
            json!({"query": "tesla", "limit": 4}),
            &SrcCx::default(),
        )
        .await
        .unwrap(),
    );
    assert_eq!(hits.len(), 4);
    // First half (2) quotes, then relevant news — quotes never starve news!
    assert!(hits[0].title.starts_with("TSLA —"));
    assert!(hits[1].title.starts_with("TSLL —"));
    assert_eq!(hits[2].title, "Tesla reports quarterly deliveries");
    assert_eq!(hits[3].title, "TSLA shares rise on energy growth");
}

#[tokio::test]
async fn test_sec_edgar_rate_limit_contact_ua_and_bad_ciks() {
    // Rate-limited response propagates Retry-After cleanly.
    let net = MockNet::new(|req| {
        Ok(HttpBytes::new(
            req.url.clone(),
            429,
            vec![("Retry-After".into(), "12".into())],
            "rate limited",
        ))
    });
    let mut cfg = Config::default();
    cfg.contact_email = Some("research@example.org".into());
    let eng = SecEdgarEngine::new(mock_deps(net.clone(), cfg.clone()));
    let cx = SrcCx::from_config(&cfg, Default::default(), Duration::from_secs(5));
    let err = eng
        .call("search", json!({"query": "inflation"}), &cx)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        FsmError::RateLimited {
            ref source_id,
            retry_after_s: Some(12),
        } if source_id == "sec_edgar"
    ));
    let captured = net.captured();
    let ua = captured[0]
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("user-agent"))
        .map(|(_, v)| v.as_str());
    assert_eq!(ua, Some("free-search-mcp research@example.org"));

    // Invalid CIKs and malformed _id are dropped without panicking.
    let bad_hits = SecEdgarEngine::map_results(&json!({
        "hits": {"hits": [
            {"_id": "no-colon", "_source": {"ciks": ["00001"], "form": "10-K"}},
            {"_id": "001:doc.htm", "_source": {"ciks": ["0000000000"], "form": "10-K"}},
            {"_id": "001:doc.htm", "_source": {"ciks": ["abc"], "form": "10-K"}},
        ]}
    }));
    assert!(bad_hits.is_empty());
}

#[tokio::test]
async fn test_github_freshness_sort_and_releases_prerelease_fields() {
    let net = MockNet::json_status(200, r#"{"items":[]}"#);
    let eng = GitHubEngine::new(mock_deps(net.clone(), Config::default()));
    let mut cx = SrcCx::default();
    cx.freshness = Some(Freshness::Day);
    eng.call("search", json!({"query": "rust", "limit": 5}), &cx)
        .await
        .unwrap();
    let repo_req = &net.captured()[0];
    assert!(
        repo_req
            .query
            .iter()
            .any(|(k, v)| k == "sort" && v == "updated")
    );
    assert!(
        repo_req
            .query
            .iter()
            .any(|(k, v)| k == "order" && v == "desc")
    );

    let hit = GitHubReleasesEngine::map_release(
        "o/r",
        &json!({
            "tag_name": "v2.0.0-rc1",
            "name": "Release Candidate 1",
            "html_url": "https://github.com/o/r/releases/tag/v2.0.0-rc1",
            "published_at": "2026-10-01T00:00:00Z",
            "prerelease": true,
            "body": ""
        }),
    )
    .unwrap();
    assert!(hit.snippet.contains("marked as a pre-release"));
    assert!(hit.snippet.contains("Release Candidate 1"));

    // Non-GitHub html_url is rejected.
    assert!(
        GitHubReleasesEngine::map_release(
            "o/r",
            &json!({"tag_name": "v1", "html_url": "https://evil.example/v1"})
        )
        .is_none()
    );
}

#[test]
fn test_registries_docker_namespaced_and_packagist_validation() {
    let hit = RegistriesEngine::map_docker(
        "bitnami",
        "redis",
        &json!({
            "name": "redis",
            "description": "Bitnami Redis",
            "last_updated": "2026-09-01T00:00:00Z",
            "pull_count": 5_000_000
        }),
    );
    assert_eq!(hit.len(), 1);
    assert_eq!(hit[0].title, "bitnami/redis on Docker Hub");
    assert_eq!(hit[0].url, "https://hub.docker.com/r/bitnami/redis");
    assert!(hit[0].snippet.contains("5,000,000 pulls"));
}

#[tokio::test]
async fn test_keyed_engines_degrade_transport_errors_and_reject_bad_ops() {
    let transport_err =
        || MockNet::new(|_| Err(FsmError::upstream("net", None, "connection reset")));
    let status_422 = || MockNet::json_status(422, r#"{"error":"unprocessable"}"#);

    let engines: Vec<(&str, &str, Box<dyn Source>)> = vec![
        (
            "brave_api",
            "SEARCH_MCP_BRAVE_API_KEY",
            Box::new(BraveApiEngine::new(mock_deps(
                transport_err(),
                cfg_with_key("SEARCH_MCP_BRAVE_API_KEY", "k"),
            ))),
        ),
        (
            "serper",
            "SEARCH_MCP_SERPER_API_KEY",
            Box::new(SerperEngine::new(mock_deps(
                transport_err(),
                cfg_with_key("SEARCH_MCP_SERPER_API_KEY", "k"),
            ))),
        ),
        (
            "serpbase",
            "SEARCH_MCP_SERPBASE_API_KEY",
            Box::new(SerpBaseEngine::new(mock_deps(
                transport_err(),
                cfg_with_key("SEARCH_MCP_SERPBASE_API_KEY", "k"),
            ))),
        ),
        (
            "tavily",
            "SEARCH_MCP_TAVILY_API_KEY",
            Box::new(TavilyEngine::new(mock_deps(
                transport_err(),
                cfg_with_key("SEARCH_MCP_TAVILY_API_KEY", "k"),
            ))),
        ),
    ];

    for (id, _, eng) in engines {
        assert!(
            matches!(
                eng.call("search", json!({"query": "hello"}), &SrcCx::default())
                    .await,
                Err(FsmError::Upstream { .. })
            ),
            "{id} must propagate transport errors as FsmError::Upstream per I2"
        );
        assert!(
            matches!(
                eng.call("unknown_op", json!({"query": "hello"}), &SrcCx::default())
                    .await,
                Err(FsmError::InvalidInput(_))
            ),
            "{id} must reject unknown op"
        );
        assert!(
            matches!(
                eng.call("search", json!({}), &SrcCx::default()).await,
                Err(FsmError::InvalidInput(_))
            ),
            "{id} must reject missing query"
        );
    }

    // 422 surfaces as Upstream(Some(422))
    let brave422 = BraveApiEngine::new(mock_deps(
        status_422(),
        cfg_with_key("SEARCH_MCP_BRAVE_API_KEY", "k"),
    ));
    assert!(matches!(
        brave422
            .call("search", json!({"query": "hello"}), &SrcCx::default())
            .await,
        Err(FsmError::Upstream {
            status: Some(422),
            ..
        })
    ));

    // Replay mode allows keyed replay even when Config has no keys.
    let (net, deps) = replay_deps(Config::default());
    let brave_replay = BraveApiEngine::new(deps);
    let mut cx = SrcCx::default();
    cx.replay_mode = true;
    let hits = unwrap_hits(
        brave_replay
            .call("search", json!({"query": "hello", "limit": 10}), &cx)
            .await
            .unwrap(),
    );
    assert_eq!(hits.len(), 2);
    assert_eq!(net.hits().len(), 1);
}

// ---------------------------------------------------------------------------
// Review round 1 regression tests (B1, I1-I3, M1-M9, Missing tests 1-8)
// ---------------------------------------------------------------------------

#[test]
fn test_r1_b1_nuget_oracle_parity_and_full_29_row_claimants_table() {
    let (_, deps) = replay_deps(Config::default());
    let mut reg = Registry::new();
    fsm_engines_api::register(&mut reg, &deps).unwrap();

    // Reviewer B1 exact 9-query NuGet table (oracle parity):
    for (q, want_nuget) in [
        ("c# latest version", false),
        (".net 9 latest version", false),
        ("asp.net core latest version", true),
        ("c# json library latest version", false),
        ("entity framework .net latest version", false),
        ("example.net domain expiry", true),
        ("vb.net latest version", true),
        ("dotnet sdk latest version", true),
        ("newtonsoft.json nuget latest version", true),
    ] {
        assert_eq!(
            ecosystem_hints(q).contains("nuget"),
            want_nuget,
            "nuget hint mismatch for {q:?}"
        );
    }

    let registries = reg.get("registries").unwrap();
    assert_eq!(registries.claims("c# latest version", None), 0.0);
    assert_eq!(registries.claims(".net 9 latest version", None), 0.0);
    assert_eq!(
        registries.claims("entity framework .net latest version", None),
        0.0
    );
    assert!(registries.claims("asp.net core latest version", None) > 0.0);
    assert!(registries.claims("vb.net latest version", None) > 0.0);

    // Missing test #1: Full 29-row Python test_claimants_by_question_shape restricted to W1c ids
    // plus B1 rows.
    let claimants = |q: &str| -> Vec<String> {
        reg.all()
            .filter(|s| s.claims(q, None) > 0.0)
            .map(|s| s.decl().id.clone())
            .collect()
    };
    let rows: &[(&str, &[&str])] = &[
        ("latest fastapi version", &[]),
        ("npm express latest version", &[]),
        ("tokio crate latest version", &[]),
        ("guava maven latest version", &["registries"]),
        ("微信 iOS 版最新版本", &[]),
        ("astral-sh/uv latest release", &["github_releases"]),
        ("CVE-2024-3094", &[]),
        ("requests vulnerabilities", &[]),
        ("zyxel exploited in the wild", &[]),
        ("上海明天天气", &["openmeteo"]),
        ("Shanghai population", &["wikidata"]),
        ("japan population", &["wikidata"]),
        ("example.com domain expiry", &["rdap"]),
        ("Array.prototype.toSorted", &[]),
        ("RFC 9110", &[]),
        ("中国 2025 GDP", &["wikidata"]),
        ("2026年中国放假安排", &[]),
        ("现在北京时间几点", &["worldclock"]),
        ("100 usd to cny", &[]),
        ("1万日元等于多少人民币", &[]),
        ("人民币中间价", &[]),
        ("alibaba group legal entity LEI", &[]),
        ("btc price", &[]),
        ("executive order on AI", &[]),
        ("UK passport renewal fee", &[]),
        ("how to cook rice", &[]),
        ("python asyncio tutorial", &[]),
        ("what is fastapi", &[]),
        ("", &[]),
        // B1 rows
        ("c# latest version", &[]),
        (".net 9 latest version", &[]),
        ("asp.net core latest version", &["registries"]),
    ];
    for &(q, expected) in rows {
        let want: Vec<String> = expected.iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(claimants(q), want, "claimants mismatch for {q:?}");
        // Orchestrator claims() scale ruling: direct answers score in [0.8, 1.0], non-claims == 0.0.
        for src in reg.all() {
            let score = src.claims(q, None);
            assert!(
                (0.0..=1.0).contains(&score),
                "{} score {score} out of [0.0, 1.0] for {q:?}",
                src.decl().id
            );
            if expected.contains(&src.decl().id.as_str()) {
                assert!(
                    score >= 0.8,
                    "direct claimant {} scored {score} (< 0.8) for {q:?}",
                    src.decl().id
                );
            } else {
                assert_eq!(score, 0.0);
            }
        }
    }
}

#[tokio::test]
async fn test_r1_i1_multibyte_date_strings_never_panic() {
    use fsm_engines_api::common::iso_day;

    // Direct iso_day multibyte check
    assert_eq!(iso_day(Some(&json!("2026-09-中旬"))), "");

    // Registries call with multibyte RubyGems version_created_at
    let net = MockNet::json_status(
        200,
        r#"{"name":"rails","version":"8.1.3","version_created_at":"2026-09-中旬"}"#,
    );
    let reg = RegistriesEngine::new(mock_deps(net, Config::default()));
    let hits = unwrap_hits(
        reg.call(
            "search",
            json!({"query": "rails gem latest version", "limit": 5}),
            &SrcCx::default(),
        )
        .await
        .unwrap(),
    );
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].date, None);

    // RDAP multibyte eventDate
    let rdap_hit = RdapEngine::map_record(
        &json!({
            "ldhName": "EXAMPLE.COM",
            "events": [{"eventAction": "registration", "eventDate": "1997-09-中"}]
        }),
        None,
    )
    .unwrap();
    assert_eq!(rdap_hit.title, "example.com");

    // Wikidata multibyte time values (+€€ and +2020-1é-01T00:00:00Z)
    let wd_hits = WikidataEngine::map_entities(
        &json!({
            "entities": {
                "Q9": {
                    "labels": {"en": {"value": "BadTime"}},
                    "claims": {
                        "P571": [{"mainsnak": {"datavalue": {"type": "time", "value": {"time": "+€€", "precision": 9}}}}],
                        "P1619": [{"mainsnak": {"datavalue": {"type": "time", "value": {"time": "+2020-1é-01T00:00:00Z", "precision": 10}}}}],
                        "P577": [{"mainsnak": {"datavalue": {"type": "time", "value": {"time": "+2020-1é-01T00:00:00Z", "precision": 11}}}}]
                    }
                }
            }
        }),
        "en",
    );
    assert_eq!(wd_hits.len(), 1);
    assert!(wd_hits[0].snippet.contains("Wikidata Q9"));
}

#[tokio::test]
async fn test_r1_i2_upstream_errors_and_bad_json_surface_as_fsm_error() {
    // Keyless 503 -> Upstream { status: Some(503) }
    let sec503 = SecEdgarEngine::new(mock_deps(
        MockNet::json_status(503, "Service Unavailable"),
        Config::default(),
    ));
    let err = sec503
        .call("search", json!({"query": "inflation"}), &SrcCx::default())
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            FsmError::Upstream {
                ref source_id,
                status: Some(503),
                ..
            } if source_id == "sec_edgar"
        ),
        "expected Upstream(503), got {err:?}"
    );

    // Keyless 404 -> empty Ok(Hits([]))
    let ghr404 = GitHubReleasesEngine::new(mock_deps(
        MockNet::json_status(404, r#"{"message":"Not Found"}"#),
        Config::default(),
    ));
    assert!(
        unwrap_hits(
            ghr404
                .call(
                    "search",
                    json!({"query": "astral-sh/nonexistent latest release"}),
                    &SrcCx::default(),
                )
                .await
                .unwrap()
        )
        .is_empty()
    );

    // Keyless 200 with HTML body -> Upstream { status: Some(200), "invalid JSON" }
    let yf_html = YahooFinanceEngine::new(mock_deps(
        MockNet::json_status(200, "<html><body>Consent wall</body></html>"),
        Config::default(),
    ));
    let err = yf_html
        .call("search", json!({"query": "nvidia"}), &SrcCx::default())
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            FsmError::Upstream {
                ref source_id,
                status: Some(200),
                ref message,
            } if source_id == "yahoofinance" && message.contains("invalid JSON")
        ),
        "expected Upstream(200, invalid JSON), got {err:?}"
    );

    // Keyless 429 without Retry-After -> RateLimited { retry_after_s: None }
    let cn429 = CninfoEngine::new(mock_deps(
        MockNet::json_status(429, "Too Many Requests"),
        Config::default(),
    ));
    let err = cn429
        .call("search", json!({"query": "人工智能"}), &SrcCx::default())
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            FsmError::RateLimited {
                ref source_id,
                retry_after_s: None,
            } if source_id == "cninfo"
        ),
        "expected RateLimited(None), got {err:?}"
    );

    // Keyed 500 -> Upstream { status: Some(500) } (orchestrator ruling on I2)
    let cfg_brave = cfg_with_key("SEARCH_MCP_BRAVE_API_KEY", "FAKE");
    let brave500 = BraveApiEngine::new(mock_deps(
        MockNet::json_status(500, "Internal Server Error"),
        cfg_brave.clone(),
    ));
    let err = brave500
        .call("search", json!({"query": "hello"}), &SrcCx::default())
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        FsmError::Upstream {
            ref source_id,
            status: Some(500),
            ..
        } if source_id == "brave_api"
    ));

    // Keyed 200 invalid JSON -> Upstream { status: Some(200), "invalid JSON" }
    let brave_bad = BraveApiEngine::new(mock_deps(MockNet::json_status(200, "<<<"), cfg_brave));
    let err = brave_bad
        .call("search", json!({"query": "hello"}), &SrcCx::default())
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        FsmError::Upstream {
            ref source_id,
            status: Some(200),
            ref message,
        } if source_id == "brave_api" && message.contains("invalid JSON")
    ));
}

#[tokio::test]
async fn test_r1_i3_partial_results_preserved_on_later_subrequest_error() {
    // 1. GitHub: repos returns 1 hit, issues times out (Timeout(4000)) -> keep 1 repo hit!
    let gh_net = MockNet::new(|req| {
        if req.url.contains("/search/repositories") {
            Ok(HttpBytes::new(
                req.url.clone(),
                200,
                vec![],
                r#"{"items":[{"full_name":"o/repo","html_url":"https://github.com/o/repo","stargazers_count":42}]}"#,
            ))
        } else {
            Err(FsmError::Timeout(4000))
        }
    });
    let gh = GitHubEngine::new(mock_deps(gh_net, Config::default()));
    let hits = unwrap_hits(
        gh.call(
            "search",
            json!({"query": "o", "limit": 5}),
            &SrcCx::default(),
        )
        .await
        .unwrap(),
    );
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].title, "o/repo");

    // 2. Registries: RubyGems OK + Docker Timeout(4000) -> keep 1 RubyGems hit!
    let reg_net = MockNet::new(|req| {
        if req.url.contains("rubygems.org") {
            Ok(HttpBytes::new(
                req.url.clone(),
                200,
                vec![],
                r#"{"name":"rails","version":"8.1.3","version_created_at":"2026-07-29T00:00:00Z"}"#,
            ))
        } else {
            Err(FsmError::Timeout(4000))
        }
    });
    let reg = RegistriesEngine::new(mock_deps(reg_net, Config::default()));
    let hits = unwrap_hits(
        reg.call(
            "search",
            json!({"query": "rails gem docker image latest version", "limit": 5}),
            &SrcCx::default(),
        )
        .await
        .unwrap(),
    );
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].title, "rails 8.1.3 on RubyGems");

    // 3. RDAP: first domain OK + second registry 503 -> keep 1 hit!
    let rdap_net = MockNet::new(|req| {
        if req.url.ends_with("dns.json") {
            Ok(HttpBytes::new(
                req.url.clone(),
                200,
                vec![],
                r#"{"services":[[["com"],["https://rdap.verisign.com/com/v1/"]],[["org"],["https://rdap.org.example/v1/"]]]}"#,
            ))
        } else if req.url.contains("github.com") {
            Ok(HttpBytes::new(
                req.url.clone(),
                200,
                vec![],
                r#"{"ldhName":"GITHUB.COM"}"#,
            ))
        } else {
            Ok(HttpBytes::new(req.url.clone(), 503, vec![], "down"))
        }
    });
    let rdap = RdapEngine::new(mock_deps(rdap_net, Config::default()));
    let hits = unwrap_hits(
        rdap.call(
            "search",
            json!({"query": "whois github.com and example.org", "limit": 5}),
            &SrcCx::default(),
        )
        .await
        .unwrap(),
    );
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].title, "github.com");

    // 4. Fatal cancellation Timeout(0) on later sub-request STILL propagates immediately!
    let cancel_net = MockNet::new(|req| {
        if req.url.contains("/search/repositories") {
            Ok(HttpBytes::new(
                req.url.clone(),
                200,
                vec![],
                r#"{"items":[{"full_name":"o/repo","html_url":"https://github.com/o/repo"}]}"#,
            ))
        } else {
            Err(FsmError::Timeout(0))
        }
    });
    let gh_cancel = GitHubEngine::new(mock_deps(cancel_net, Config::default()));
    let err = gh_cancel
        .call(
            "search",
            json!({"query": "o", "limit": 5}),
            &SrcCx::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, FsmError::Timeout(0)));
}

#[tokio::test]
async fn test_r1_m1_plain_engines_send_no_explicit_user_agent() {
    let ok_empty =
        MockNet::json_status(200, r#"{"results":[],"search":[],"services":[],"data":[]}"#);
    let deps = mock_deps(ok_empty.clone(), Config::default());

    let om = OpenMeteoEngine::new(deps.clone());
    let _ = om
        .call(
            "search",
            json!({"query": "weather in Berlin"}),
            &SrcCx::default(),
        )
        .await;

    let wd = WikidataEngine::new(deps.clone());
    let _ = wd
        .call(
            "search",
            json!({"query": "Shanghai population"}),
            &SrcCx::default(),
        )
        .await;

    let rd = RdapEngine::new(deps.clone());
    let _ = rd
        .call(
            "search",
            json!({"query": "github.com whois"}),
            &SrcCx::default(),
        )
        .await;

    let rg = RegistriesEngine::new(deps);
    let _ = rg
        .call(
            "search",
            json!({"query": "newtonsoft.json nuget latest version"}),
            &SrcCx::default(),
        )
        .await;

    let captured = ok_empty.captured();
    assert_eq!(captured.len(), 4);
    for req in &captured {
        assert!(
            !req.headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("user-agent")),
            "plain engine {} should let fsm-net send contact-aware UA, found headers {:?}",
            req.source_id.as_deref().unwrap_or("?"),
            req.headers
        );
    }
}

#[test]
fn test_r1_m2_unescape_html_is_linear_and_m8_handles_extended_entities() {
    use fsm_engines_api::common::unescape_html;
    use std::time::Instant;

    let input = format!("{};", "&".repeat(100_000));
    let t0 = Instant::now();
    let out = unescape_html(&input);
    let elapsed = t0.elapsed();
    assert_eq!(out, input);
    assert!(
        elapsed < Duration::from_millis(200),
        "unescape_html took {elapsed:?} on 100k '&' (quadratic regression)"
    );

    // M8: HTML5 Latin entities and semicolon-less numeric refs
    assert_eq!(
        unescape_html("Caf&eacute; &uuml;ber &#039ok&#x27 &euro;10"),
        "Café über 'ok' €10"
    );
}

#[test]
fn test_r1_m3_script_tie_detection_is_deterministic() {
    use fsm_engines_api::facts::detect_query_region;

    for _ in 0..100 {
        assert_eq!(detect_query_region("中文 ру", "us-en"), "cn-zh");
        assert_eq!(detect_query_region("ру 中文", "us-en"), "ru-ru");
    }
}

#[tokio::test]
async fn test_r1_m4_sequential_requests_share_single_call_timeout_budget() {
    struct DelayNet {
        timeouts: Mutex<Vec<Duration>>,
    }

    #[async_trait]
    impl NetClient for DelayNet {
        async fn send(
            &self,
            req: HttpRequest,
            _profile: TransportProfile,
            cx: &NetCx,
        ) -> Result<HttpBytes, FsmError> {
            self.timeouts.lock().unwrap().push(cx.timeout);
            tokio::time::sleep(Duration::from_millis(80)).await;
            if req.url.ends_with("dns.json") {
                Ok(HttpBytes::new(
                    req.url,
                    200,
                    vec![],
                    r#"{"services":[[["com"],["https://rdap.verisign.com/com/v1/"]]]}"#,
                ))
            } else {
                Ok(HttpBytes::new(
                    req.url,
                    200,
                    vec![],
                    r#"{"ldhName":"GITHUB.COM"}"#,
                ))
            }
        }
        fn check_url_offline(&self, _url: &str) -> Result<(), FsmError> {
            Ok(())
        }
    }

    let net = Arc::new(DelayNet {
        timeouts: Mutex::new(Vec::new()),
    });
    let rd = RdapEngine::new(mock_deps(net.clone(), Config::default()));
    let mut cx = SrcCx::default();
    cx.timeout = Duration::from_millis(500);
    let hits = unwrap_hits(
        rd.call("search", json!({"query": "github.com whois"}), &cx)
            .await
            .unwrap(),
    );
    assert_eq!(hits.len(), 1);
    let timeouts = net.timeouts.lock().unwrap().clone();
    assert_eq!(timeouts.len(), 2);
    assert!(
        timeouts[1] < timeouts[0] - Duration::from_millis(50),
        "second sequential request must get remaining budget, got {timeouts:?}"
    );
}

#[tokio::test]
async fn test_r1_m5_worldclock_caps_at_3_zones_even_with_limit_10() {
    let eng = WorldClockEngine::new();
    let hits = unwrap_hits(
        eng.call(
            "search",
            json!({"query": "Tokyo New York London Paris Berlin", "limit": 10}),
            &SrcCx::default(),
        )
        .await
        .unwrap(),
    );
    assert_eq!(hits.len(), 3);
}

#[test]
fn test_r1_m6_hit_kind_vertical_for_non_direct_finance_engines() {
    use fsm_core::HitKind;

    let sec = SecEdgarEngine::map_results(&json!({
        "hits": {"hits": [{"_id": "0001-26-0001:doc.htm", "_source": {"ciks": ["0000829323"], "form": "10-K", "display_names": ["Inuvo (INUV)"]}}]}
    }));
    assert_eq!(sec[0].kind, HitKind::Vertical);

    let cn = CninfoEngine::map_results(&json!({
        "announcements": [{"secCode": "688525", "secName": "佰维存储", "announcementTitle": "公告", "adjunctUrl": "finalpage/a.PDF"}]
    }));
    assert_eq!(cn[0].kind, HitKind::Vertical);

    let yf_q =
        YahooFinanceEngine::map_quotes(Some(&json!([{"symbol": "NVDA", "longname": "NVIDIA"}])));
    assert_eq!(yf_q[0].kind, HitKind::Vertical);
}

#[test]
fn test_r1_m9_keyless_engines_declare_probe_specs() {
    let (_, deps) = replay_deps(Config::default());
    let mut reg = Registry::new();
    fsm_engines_api::register(&mut reg, &deps).unwrap();

    for id in [
        "github",
        "github_releases",
        "registries",
        "sec_edgar",
        "yahoofinance",
        "cninfo",
        "worldclock",
        "openmeteo",
        "rdap",
        "wikidata",
    ] {
        let src = reg.get(id).unwrap();
        let probe = src.decl().op("search").unwrap().probe.as_ref();
        assert!(probe.is_some(), "{id}.search must declare a ProbeSpec");
        assert!(probe.unwrap().min_results >= 1);
    }

    for id in ["github_code", "brave_api", "serper", "serpbase", "tavily"] {
        assert!(
            reg.get(id)
                .unwrap()
                .decl()
                .op("search")
                .unwrap()
                .probe
                .is_none(),
            "keyed engine {id} is replay-only and should not declare a live probe"
        );
    }
}

#[test]
fn test_r1_missing5_wikidata_newest_p585_wins_when_listed_second() {
    let payload = json!({
        "entities": {
            "Q8686": {
                "labels": {"en": {"value": "Shanghai"}},
                "claims": {
                    "P1082": [
                        {
                            "mainsnak": {"datavalue": {"type": "quantity", "value": {"amount": "+23019148", "unit": "1"}}},
                            "qualifiers": {"P585": [{"datavalue": {"value": {"time": "+2010-11-01T00:00:00Z", "precision": 9}}}]}
                        },
                        {
                            "mainsnak": {"datavalue": {"type": "quantity", "value": {"amount": "+24870895", "unit": "1"}}},
                            "qualifiers": {"P585": [{"datavalue": {"value": {"time": "+2020-11-01T00:00:00Z", "precision": 9}}}]}
                        }
                    ]
                }
            }
        }
    });
    let hits = WikidataEngine::map_entities(&payload, "en");
    assert_eq!(hits.len(), 1);
    assert!(
        hits[0]
            .snippet
            .contains("population 24,870,895 (as of 2020)")
    );
    assert!(!hits[0].snippet.contains("23,019,148"));
}

#[tokio::test]
async fn test_r1_missing6_packagist_skips_dotted_vendor_and_lowercases_valid_vendor() {
    let net = MockNet::json_status(
        200,
        r#"{"packages":{"symfony/console":[{"version":"v7.3.1","time":"2026-08-01T00:00:00+00:00"}]}}"#,
    );
    let eng = RegistriesEngine::new(mock_deps(net.clone(), Config::default()));

    // Dotted vendor ("laravel.io/foo") must emit zero requests.
    let none = unwrap_hits(
        eng.call(
            "search",
            json!({"query": "laravel.io/foo composer latest version", "limit": 5}),
            &SrcCx::default(),
        )
        .await
        .unwrap(),
    );
    assert!(none.is_empty());
    assert!(net.captured().is_empty());

    // Mixed-case valid vendor ("Symfony/Console") lowercases to p2/symfony/console.json.
    let hits = unwrap_hits(
        eng.call(
            "search",
            json!({"query": "Symfony/Console composer latest version", "limit": 5}),
            &SrcCx::default(),
        )
        .await
        .unwrap(),
    );
    assert_eq!(hits.len(), 1);
    assert_eq!(
        net.captured()[0].url,
        "https://repo.packagist.org/p2/symfony/console.json"
    );
}
