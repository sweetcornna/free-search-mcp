//! Offline `ReplayNet` tests verifying all 13 native SERP engines against
//! §4 replay fixtures under `crates/engines-serp/tests/fixtures/**`.
//!
//! Zero network access: every test constructs a `ReplayNet::from_file(...)` (or
//! a `RateLimits`-enforcing wrapper around `ReplayNet`), registers all 13
//! engines via `fsm_engines_serp::register`, resolves `<id>.search`, calls it,
//! and asserts golden parsed hits, snippet ratios, dates, wall exemptions,
//! fixture privacy hygiene, per-call rate-limit accounting, and zero
//! `unmatched()` / `unused()` exchanges.

use async_trait::async_trait;
use fsm_core::config::Config;
use fsm_core::fixture::{Exchange, Fixture, FixtureRequest, FixtureResponse, ReplayNet};
use fsm_core::net::{HttpBytes, HttpRequest, NetClient, NetCx};
use fsm_core::source::{Source, SrcCx, SrcOutput, TransportProfile};
use fsm_core::types::Hit;
use fsm_core::{FsmError, NullStore, Registry, SourceDeps};
use fsm_engines_serp::{ENGINE_IDS, register};
use fsm_net::RateLimits;
use regex::Regex;
use serde_json::{Value, json};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

fn fixture_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(rel)
}

fn default_cx() -> SrcCx {
    SrcCx::from_config(
        &Config::default(),
        CancellationToken::new(),
        Duration::from_secs(10),
    )
}

async fn run_replay(
    source_id: &str,
    fixture_rel: &str,
    params: Value,
) -> (Vec<Hit>, Arc<ReplayNet>) {
    let net = Arc::new(
        ReplayNet::from_file(fixture_path(fixture_rel))
            .unwrap_or_else(|e| panic!("failed to load fixture {fixture_rel}: {e}")),
    );
    let deps = SourceDeps::new(
        Arc::clone(&net) as Arc<dyn NetClient>,
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    );
    let mut reg = Registry::new();
    register(&mut reg, &deps).expect("register all 13 SERP engines");
    let target = format!("{source_id}.search");
    let (src, op) = reg
        .resolve_op(&target)
        .unwrap_or_else(|e| panic!("resolve_op {target}: {e}"));
    let out = src
        .call(op, params, &default_cx())
        .await
        .unwrap_or_else(|e| panic!("{source_id}.search failed: {e}"));
    let SrcOutput::Hits(hits) = out else {
        panic!("{source_id}.search did not return SrcOutput::Hits");
    };
    assert!(
        net.unmatched().is_empty(),
        "{source_id}: unmatched replay requests: {:?}",
        net.unmatched()
    );
    assert!(
        net.unused().is_empty(),
        "{source_id}: unused fixture exchanges: {:?}",
        net.unused()
    );
    for hit in &hits {
        assert_eq!(hit.source, source_id);
        assert_eq!(hit.sources, vec![source_id.to_owned()]);
        assert!(!hit.title.is_empty(), "{source_id}: empty title");
        assert!(
            hit.url.starts_with("http://") || hit.url.starts_with("https://"),
            "{source_id}: non-http url {:?}",
            hit.url
        );
    }
    (hits, net)
}

fn assert_golden_prefix(source_id: &str, hits: &[Hit], expected: &[(&str, &str)]) {
    assert!(
        hits.len() >= expected.len(),
        "{source_id}: expected at least {} hits, got {}",
        expected.len(),
        hits.len()
    );
    for (i, (want_title, want_url)) in expected.iter().enumerate() {
        assert_eq!(
            hits[i].title, *want_title,
            "{source_id} hit[{i}] title mismatch"
        );
        assert_eq!(hits[i].url, *want_url, "{source_id} hit[{i}] url mismatch");
    }
}

fn assert_snippet_ratio(source_id: &str, hits: &[Hit], min_pct: usize) {
    assert!(!hits.is_empty(), "{source_id}: no hits");
    let non_empty = hits.iter().filter(|h| !h.snippet.trim().is_empty()).count();
    assert!(
        non_empty * 100 >= hits.len() * min_pct,
        "{source_id}: expected >= {min_pct}% non-empty snippets, got {non_empty}/{}",
        hits.len()
    );
}

// ---------------------------------------------------------------------------
// I2: Fixture privacy & hygiene lint (IPv4 literals + live session tokens)
// ---------------------------------------------------------------------------

/// Return `true` only for reserved non-routable IPv4 ranges allowed in fixtures:
/// loopback (127/8), RFC 1918 (10/8, 172.16/12, 192.168/16), benchmarking
/// (198.18/15), and TEST-NET-1/2/3 (192.0.2/24, 198.51.100/24, 203.0.113/24).
fn is_allowed_reserved_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    a == 127
        || a == 10
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 192 && b == 0 && c == 2)
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
}

/// Explicit allowlist for dotted-quads that are upstream browser UA versions
/// rather than IP addresses (must appear immediately after `Chrome/` or
/// `Chrome&#47;`).
fn is_allowlisted_upstream_quad(haystack: &str, start: usize, quad: &str) -> bool {
    if quad == "131.0.0.0" {
        let prefix = &haystack[..start];
        return prefix.ends_with("Chrome/") || prefix.ends_with("Chrome&#47;");
    }
    false
}

/// Synthetic placeholder values allowed for session cookies inside fixtures.
fn is_synthetic_session_token(val: &str) -> bool {
    val.starts_with("REPLAY_TEST_")
        || val.contains(".replay")
        || val == "d=AQABreplay"
        || val.starts_with("d=AQABreplay")
}

fn lint_fixture_text(label: &str, text: &str) -> Vec<String> {
    let mut violations = Vec::new();
    let ipv4_re = Regex::new(r"(?m)(?:^|[^0-9.])(\d{1,3}(?:\.\d{1,3}){3})(?:$|[^0-9.])").unwrap();
    for cap in ipv4_re.captures_iter(text) {
        let m = cap.get(1).unwrap();
        let quad = m.as_str();
        if let Ok(ip) = quad.parse::<Ipv4Addr>()
            && !is_allowed_reserved_ipv4(ip)
            && !is_allowlisted_upstream_quad(text, m.start(), quad)
        {
            violations.push(format!("{label}: non-reserved IPv4 literal {quad}"));
        }
    }
    let tok_re = Regex::new(r#"(?i)\b(BAIDUID|MUID|YBV|A3|buvid3)=([^;\s"'&<>\\]+)"#).unwrap();
    for cap in tok_re.captures_iter(text) {
        let name = cap.get(1).unwrap().as_str();
        let val = cap.get(2).unwrap().as_str();
        if !is_synthetic_session_token(val) {
            violations.push(format!("{label}: live-looking session token {name}={val}"));
        }
    }
    violations
}

fn collect_fixture_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_fixture_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "json") {
            out.push(path);
        }
    }
}

#[test]
fn fixtures_pass_privacy_and_session_token_lint() {
    // Self-check: verify the lint catches the original leaked IPv4 and a live token.
    let bad_ip = lint_fixture_text("self-test", "您的IP是：222.168.40.109");
    assert!(
        bad_ip.iter().any(|v| v.contains("222.168.40.109")),
        "lint must reject public IPv4 literals: {bad_ip:?}"
    );
    let bad_tok = lint_fixture_text("self-test", "Set-Cookie: BAIDUID=E42AFA1D35E3C790:FG=1;");
    assert!(
        bad_tok.iter().any(|v| v.contains("BAIDUID=")),
        "lint must reject live session tokens: {bad_tok:?}"
    );
    // Reserved ranges and explicit UA version pass cleanly.
    assert!(
        lint_fixture_text(
            "ok",
            "127.0.0.1 10.0.0.1 172.16.5.1 192.168.1.1 198.18.0.1 192.0.2.1 198.51.100.1 203.0.113.7 Chrome/131.0.0.0"
        )
        .is_empty()
    );

    let root = fixture_path("");
    let mut files = Vec::new();
    collect_fixture_files(&root, &mut files);
    files.sort();
    assert!(files.len() >= 14);

    let mut all_violations = Vec::new();
    for path in &files {
        let rel = path.strip_prefix(&root).unwrap().display().to_string();
        let raw = std::fs::read_to_string(path).unwrap();
        let fixture =
            Fixture::from_json(&raw).unwrap_or_else(|e| panic!("invalid fixture {rel}: {e}"));
        for (i, ex) in fixture.exchanges.iter().enumerate() {
            let tag = format!("{rel}#{i}");
            all_violations.extend(lint_fixture_text(
                &format!("{tag}.req.url"),
                &ex.request.url,
            ));
            for (k, v) in &ex.request.headers_subset {
                all_violations.extend(lint_fixture_text(&format!("{tag}.req.hdr.{k}"), v));
            }
            for (k, v) in &ex.response.headers {
                all_violations.extend(lint_fixture_text(&format!("{tag}.resp.hdr.{k}"), v));
            }
            if let Some(body) = &ex.response.body_text {
                all_violations.extend(lint_fixture_text(&format!("{tag}.resp.body"), body));
            }
        }
    }
    assert!(
        all_violations.is_empty(),
        "fixture privacy/token lint failed:\n{}",
        all_violations.join("\n")
    );
}

#[test]
fn all_13_engines_have_valid_default_fixtures() {
    for id in ENGINE_IDS {
        let path = fixture_path(&format!("{id}/search/default.json"));
        assert!(path.exists(), "missing fixture {}", path.display());
        let replay = ReplayNet::from_file(&path)
            .unwrap_or_else(|e| panic!("invalid fixture {}: {e}", path.display()));
        assert!(!replay.is_empty(), "empty fixture {}", path.display());
    }
}

// ---------------------------------------------------------------------------
// Per-engine ReplayNet fixture tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn replay_duckduckgo_live_fixture() {
    let (hits, _) = run_replay(
        "duckduckgo",
        "duckduckgo/search/default.json",
        json!({"query": "python asyncio", "limit": 10}),
    )
    .await;
    assert_eq!(hits.len(), 10);
    assert!(
        hits.iter().all(|h| !h.url.contains("duckduckgo.com/l/")),
        "DDG redirects should be unwrapped"
    );
    assert_golden_prefix(
        "duckduckgo",
        &hits,
        &[
            (
                "asyncio — Asynchronous I/O — Python 3.15.2 documentation",
                "https://docs.python.org/3/library/asyncio.html",
            ),
            (
                "Python's asyncio: A Hands-On Walkthrough - Real Python",
                "https://realpython.com/async-io-python/",
            ),
            (
                "A conceptual overview of asyncio — Python 3.15.2 documentation",
                "https://docs.python.org/3/howto/a-conceptual-overview-of-asyncio.html",
            ),
        ],
    );
    assert_snippet_ratio("duckduckgo", &hits, 100);
}

#[tokio::test]
async fn replay_bing_live_fixture() {
    let (hits, net) = run_replay(
        "bing",
        "bing/search/default.json",
        json!({"query": "rust ownership borrowing", "limit": 10}),
    )
    .await;
    assert_eq!(hits.len(), 10);
    assert_eq!(net.hits().len(), 2, "warm-up + search");
    assert!(
        hits.iter().all(|h| !h.url.contains("bing.com/ck/a")),
        "Bing ck/a redirects should be unwrapped"
    );
    assert_golden_prefix(
        "bing",
        &hits,
        &[
            (
                "Understanding Ownership - The Rust Programming Language",
                "https://doc.rust-lang.org/book/ch04-00-understanding-ownership.html",
            ),
            (
                "What is Ownership? - The Rust Programming Language",
                "https://doc.rust-lang.org/book/ch04-01-what-is-ownership.html",
            ),
            (
                "Rust ownership and borrowing - understanding ownership in Rust",
                "https://www.zetcode.com/rust/ownership/",
            ),
        ],
    );
    assert_snippet_ratio("bing", &hits, 100);
    assert_eq!(hits[2].date.as_deref(), Some("2026-10-25"));
    assert_eq!(hits[2].date_source.as_deref(), Some("snippet"));
}

#[tokio::test]
async fn replay_yahoojp_live_fixture() {
    let (hits, _) = run_replay(
        "yahoojp",
        "yahoojp/search/default.json",
        json!({"query": "python asyncio", "limit": 10}),
    )
    .await;
    assert_eq!(hits.len(), 8);
    assert!(
        hits.iter().all(|h| !h.url.contains("search.yahoo.co.jp")),
        "Yahoo JP internal links should be excluded"
    );
    assert_golden_prefix(
        "yahoojp",
        &hits,
        &[
            (
                "asyncio — Asynchronous I/O — Python 3.15.2 documentation",
                "https://docs.python.org/3/library/asyncio.html",
            ),
            (
                "Python's asyncio: A Hands-On Walkthrough",
                "https://realpython.com/async-io-python/",
            ),
            (
                "Asyncio - The Blue Book",
                "https://lyz-code.github.io/blue-book/asyncio/",
            ),
        ],
    );
    assert_snippet_ratio("yahoojp", &hits, 100);
    assert_eq!(hits[1].date.as_deref(), Some("2026-10-24"));
    assert_eq!(hits[1].date_source.as_deref(), Some("snippet"));
}

#[tokio::test]
async fn replay_yahoo_fixture() {
    let (hits, net) = run_replay(
        "yahoo",
        "yahoo/search/default.json",
        json!({"query": "python asyncio taskgroup", "limit": 10}),
    )
    .await;
    assert_eq!(hits.len(), 3);
    assert_eq!(net.hits().len(), 2, "500 _bv handshake hop + 200 search");
    assert!(
        hits.iter().all(|h| !h.url.contains("r.search.yahoo.com")),
        "Yahoo redirect links should be unwrapped"
    );
    assert_golden_prefix(
        "yahoo",
        &hits,
        &[
            (
                "Python Taskgroups with asyncIO - GeeksforGeeks",
                "https://www.geeksforgeeks.org/python/python-taskgroups-with-asyncio/",
            ),
            (
                "Coroutines and tasks — Python 3.15.2 documentation",
                "https://docs.python.org/3/library/asyncio-task.html",
            ),
            ("C++ - Wikipedia", "https://en.wikipedia.org/wiki/C%2B%2B"),
        ],
    );
    assert_snippet_ratio("yahoo", &hits, 100);
    assert_eq!(hits[0].date.as_deref(), Some("2025-07-23"));
    assert_eq!(hits[0].date_source.as_deref(), Some("snippet"));
    assert_eq!(hits[1].date.as_deref(), Some("1 day ago"));
    assert_eq!(hits[1].date_source.as_deref(), Some("snippet"));
}

#[tokio::test]
async fn replay_seznam_live_fixture() {
    let (hits, _) = run_replay(
        "seznam",
        "seznam/search/default.json",
        json!({"query": "python asyncio", "limit": 10}),
    )
    .await;
    assert_eq!(hits.len(), 10);
    assert_golden_prefix(
        "seznam",
        &hits,
        &[
            (
                "asyncio — Asynchronous I/O — Python 3.14.7 documentation",
                "https://docs.python.org/3/library/asyncio.html",
            ),
            (
                "Python Asyncio: The Complete Guide – SuperFastPython",
                "https://superfastpython.com/python-asyncio/",
            ),
            (
                "Python's asyncio: A Hands-On Walkthrough – Real Python",
                "https://realpython.com/async-io-python/",
            ),
        ],
    );
    assert_snippet_ratio("seznam", &hits, 100);
}

#[tokio::test]
async fn replay_so360_default_and_live_wall_fixture() {
    let (hits, _) = run_replay(
        "so360",
        "so360/search/default.json",
        json!({"query": "python asyncio", "limit": 10}),
    )
    .await;
    assert_eq!(hits.len(), 3);
    assert_golden_prefix(
        "so360",
        &hits,
        &[
            (
                "asyncio — 异步 I/O — Python 3 文档",
                "https://docs.python.org/zh-cn/3/library/asyncio.html",
            ),
            (
                "PEP 492 – Coroutines with async and await syntax",
                "https://peps.python.org/pep-0492/",
            ),
            (
                "Async IO in Python: A Complete Walkthrough",
                "https://realpython.com/async-io-python/",
            ),
        ],
    );
    assert_snippet_ratio("so360", &hits, 100);
    assert_eq!(hits[0].date.as_deref(), Some("2026-10-15"));
    assert_eq!(hits[0].date_source.as_deref(), Some("snippet"));

    // Live 360 CAPTCHA wall fixture must produce an explicit Upstream error.
    let wall_net = Arc::new(
        ReplayNet::from_file(fixture_path("so360/search/wall.json")).expect("load so360 wall"),
    );
    let deps = SourceDeps::new(
        Arc::clone(&wall_net) as Arc<dyn NetClient>,
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    );
    let mut reg = Registry::new();
    register(&mut reg, &deps).unwrap();
    let (src, op) = reg.resolve_op("so360.search").unwrap();
    let err = src
        .call(
            op,
            json!({"query": "python asyncio", "limit": 10}),
            &default_cx(),
        )
        .await
        .expect_err("live so360 CAPTCHA wall must error");
    match err {
        FsmError::Upstream {
            source_id, message, ..
        } => {
            assert_eq!(source_id, "so360");
            assert!(
                message.contains("captcha-gated"),
                "unexpected wall error: {message}"
            );
        }
        other => panic!("expected Upstream wall error, got {other:?}"),
    }
    assert!(wall_net.unmatched().is_empty());
    assert!(wall_net.unused().is_empty());
}

#[tokio::test]
async fn replay_quark_live_fixture() {
    let (hits, _) = run_replay(
        "quark",
        "quark/search/default.json",
        json!({"query": "python asyncio", "limit": 10}),
    )
    .await;
    assert_eq!(hits.len(), 7);
    assert!(
        hits.iter()
            .all(|h| !h.url.contains("quark.cn") && !h.url.contains("sm.cn"))
    );
    assert_golden_prefix(
        "quark",
        &hits,
        &[
            (
                "asyncio-Python documentation",
                "https://docs.python.org/zh-cn/3/library/asyncio.html",
            ),
            (
                "Python asyncio 异步编程全套学习文档（零基础完整版）-CSDN博客",
                "https://m.blog.csdn.net/m0_73656461/article/details/163923978",
            ),
            (
                "【Python3教程】Python3高级篇之asyncio模块-CSDN博客",
                "https://m.blog.csdn.net/weixin_44299027/article/details/161068165",
            ),
        ],
    );
    assert_snippet_ratio("quark", &hits, 100);
    assert_eq!(hits[1].date.as_deref(), Some("2026-08-21"));
    assert_eq!(hits[1].date_source.as_deref(), Some("snippet"));
    assert_eq!(hits[2].date.as_deref(), Some("2026-05-14"));
}

#[tokio::test]
async fn replay_naver_live_fixture() {
    let (hits, _) = run_replay(
        "naver",
        "naver/search/default.json",
        json!({"query": "python asyncio", "limit": 10}),
    )
    .await;
    assert_eq!(hits.len(), 10);
    assert_golden_prefix(
        "naver",
        &hits,
        &[
            (
                "Pure-Python gRPC implementation for asyncio — grpclib documentation",
                "https://grpclib.readthedocs.io/",
            ),
            (
                "【Python】AsyncIO 이벤트 루프 내부 아키텍처와 이벤트 디스패칭 심층 분석",
                "https://velog.io/@aman179102/python-asyncio-event-loop-internals-deep-dive-opvvjac8",
            ),
            (
                "Python's asyncio: A Hands-On Walkthrough",
                "https://realpython.com/async-io-python/",
            ),
        ],
    );
    assert_snippet_ratio("naver", &hits, 100);
    assert_eq!(hits[1].date.as_deref(), Some("2 days ago"));
    assert_eq!(hits[1].date_source.as_deref(), Some("snippet"));
    assert_eq!(hits[2].date.as_deref(), Some("2026-10-24"));
}

#[tokio::test]
async fn replay_daum_live_fixture() {
    let (hits, _) = run_replay(
        "daum",
        "daum/search/default.json",
        json!({"query": "python asyncio", "limit": 10}),
    )
    .await;
    assert_eq!(
        hits.len(),
        10,
        "daum w=fusion returns 15 trimmed to limit=10"
    );
    assert_golden_prefix(
        "daum",
        &hits,
        &[
            (
                "Python asyncio TaskGroup: 하나가 실패하면 나머지 작업은 어떻게 될까?",
                "https://the-black.tistory.com/163",
            ),
            (
                "[Python] 파이썬 asyncio 처음 쓸 때 헷갈리는 5가지, await 했는데 안 빨라지는 이유",
                "https://rorobong.tistory.com/390",
            ),
            (
                "Python asyncio.gather에서 일부 Task만 취소하고 나머지는 계속 실행하고 싶어요",
                "https://youngsam.net/community/q26d-python-asyncio-gather-%EC%9D%BC%EB%B6%80-task-%EC%B7%A8%EC%86%8C",
            ),
        ],
    );
    assert_snippet_ratio("daum", &hits, 100);
    assert_eq!(hits[0].date.as_deref(), Some("2026-11-06"));
    assert_eq!(hits[1].date.as_deref(), Some("2026-11-05"));
    assert_eq!(hits[2].date.as_deref(), Some("2026-04-16"));
    assert!(
        hits.iter()
            .all(|h| h.date.is_some() && h.date_source.as_deref() == Some("snippet"))
    );
}

#[tokio::test]
async fn replay_baidu_live_fixture() {
    let (hits, net) = run_replay(
        "baidu",
        "baidu/search/default.json",
        json!({"query": "python asyncio", "limit": 10}),
    )
    .await;
    assert_eq!(
        hits.len(),
        7,
        "7 organic results after filtering Baidu internal cards"
    );
    assert_eq!(net.hits().len(), 2, "warm-up + search");
    assert!(
        hits.iter().all(|h| !h.url.contains("baidu.com/s?")
            && !h.url.contains("nourl.ubs.baidu.com")
            && !h.url.contains("recommend_list.baidu.com")),
        "Baidu internal/news-agg URLs must be dropped: {:?}",
        hits.iter().map(|h| &h.url).collect::<Vec<_>>()
    );
    assert_golden_prefix(
        "baidu",
        &hits,
        &[
            (
                "asyncio --- 异步 I/O — Python 3.14.10 文档",
                "https://docs.python.org/zh-cn/3.14/library/asyncio.html",
            ),
            (
                "Python asyncio 是什么?-腾讯云开发者社区-腾讯云",
                "https://cloud.tencent.com/developer/article/2434022",
            ),
            (
                "Python 异步编程入门: asyncio 实战指南-阿里云开发者社区",
                "https://developer.aliyun.com/article/1689146",
            ),
        ],
    );
    assert_snippet_ratio("baidu", &hits, 80);
}

#[tokio::test]
async fn replay_googlenews_live_fixture() {
    let (hits, net) = run_replay(
        "googlenews",
        "googlenews/search/default.json",
        json!({"query": "python asyncio", "limit": 2}),
    )
    .await;
    assert_eq!(hits.len(), 2);
    assert_eq!(
        net.hits().len(),
        5,
        "RSS + 2 article shells + 2 batchexecute RPCs"
    );
    assert_golden_prefix(
        "googlenews",
        &hits,
        &[
            (
                "Kubernetes Python client gets a modern overhaul for the AI/ML era - Google Open Source Blog (Google Open Source Blog)",
                "https://opensource.googleblog.com/2026/11/kubernetes-python-client-modern-overhaul-ai-ml.html",
            ),
            (
                "Rapidly scaling online storage to serve over 1 billion ChatGPT users - OpenAI (OpenAI)",
                "https://openai.com/index/scaling-storage-one-billion-users-part-one/",
            ),
        ],
    );
    assert_snippet_ratio("googlenews", &hits, 100);
    assert!(hits[0].date.is_some());
    assert_eq!(hits[1].date.as_deref(), Some("2026-09-11"));
    assert!(hits.iter().all(|h| h.date_source.as_deref() == Some("rss")));
}

#[tokio::test]
async fn replay_youcom_live_fixture() {
    let (hits, _) = run_replay(
        "youcom",
        "youcom/search/default.json",
        json!({"query": "python asyncio", "limit": 5}),
    )
    .await;
    assert_eq!(hits.len(), 5);
    assert_golden_prefix(
        "youcom",
        &hits,
        &[
            (
                "asyncio — Asynchronous I/O",
                "https://docs.python.org/3/library/asyncio.html",
            ),
            (
                "Python's asyncio: A Hands-On Walkthrough – Real Python",
                "https://realpython.com/async-io-python/",
            ),
            (
                "A conceptual overview of asyncio — Python 3.15.2 documentation",
                "https://docs.python.org/3/howto/a-conceptual-overview-of-asyncio.html",
            ),
        ],
    );
    assert_snippet_ratio("youcom", &hits, 100);
    assert_eq!(hits[1].date.as_deref(), Some("2026-11-09"));
    assert_eq!(hits[1].date_source.as_deref(), Some("snippet"));
}

#[tokio::test]
async fn replay_bilibili_live_fixture() {
    let (hits, _) = run_replay(
        "bilibili",
        "bilibili/search/default.json",
        json!({"query": "python asyncio", "limit": 10}),
    )
    .await;
    assert_eq!(hits.len(), 10);
    assert_golden_prefix(
        "bilibili",
        &hits,
        &[
            (
                "【 python 】 asyncio 的理解与入门，搞不明白协程？看这个视频就够了。",
                "https://www.bilibili.com/video/av212234657",
            ),
            (
                "Python 之os模块讲解",
                "https://www.bilibili.com/video/av506507938",
            ),
            (
                "python 零基础教程， Python 入门到大神，AI人工智能必备",
                "https://www.bilibili.com/video/av115405267993963",
            ),
        ],
    );
    assert_snippet_ratio("bilibili", &hits, 100);
    assert_eq!(hits[0].date.as_deref(), Some("2022-03-11"));
    assert_eq!(hits[1].date.as_deref(), Some("2021-11-10"));
    assert_eq!(hits[2].date.as_deref(), Some("2025-10-21"));
    assert!(
        hits.iter()
            .all(|h| h.url.starts_with("https://") && h.date_source.as_deref() == Some("api"))
    );
}

// ---------------------------------------------------------------------------
// I1: Unstripped Naver & Seznam zero-result pages must NOT false-fire walls
// ---------------------------------------------------------------------------

#[tokio::test]
async fn replay_unstripped_empty_naver_and_seznam_return_empty_not_wall() {
    // Live unstripped Naver page with all `fds-web-doc-root` removed still
    // carries `gw.in.naver.com/.../refresh-captcha` (contains `h-captcha`).
    let (naver_hits, _) = run_replay(
        "naver",
        "naver/search/empty_with_scripts.json",
        json!({"query": "python asyncio", "limit": 10}),
    )
    .await;
    assert!(
        naver_hits.is_empty(),
        "expected empty hits on unstripped zero-doc Naver page"
    );

    // Live unstripped Seznam page with organic results removed still carries
    // `CONF_RECAPTCHA_KEY` and all inline scripts.
    let (seznam_hits, _) = run_replay(
        "seznam",
        "seznam/search/empty_with_scripts.json",
        json!({"query": "python asyncio", "limit": 10}),
    )
    .await;
    assert!(
        seznam_hits.is_empty(),
        "expected empty hits on unstripped zero-hit Seznam page"
    );
}

// ---------------------------------------------------------------------------
// I4: Yahoo 500 -> 307 -> /_bv/v.gif -> 302 -> 200 redirect handshake + reuse + re-mint
// ---------------------------------------------------------------------------

type SeenRequest = (String, Option<String>, Option<f64>);

struct RecordingReplayNet {
    inner: Arc<ReplayNet>,
    seen: Mutex<Vec<SeenRequest>>,
}

impl RecordingReplayNet {
    fn new(inner: Arc<ReplayNet>) -> Self {
        Self {
            inner,
            seen: Mutex::new(Vec::new()),
        }
    }
    fn seen(&self) -> Vec<SeenRequest> {
        self.seen.lock().unwrap().clone()
    }
}

#[async_trait]
impl NetClient for RecordingReplayNet {
    async fn send(
        &self,
        req: HttpRequest,
        profile: TransportProfile,
        cx: &NetCx,
    ) -> Result<HttpBytes, FsmError> {
        let cookie = req
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("cookie"))
            .map(|(_, v)| v.clone());
        self.seen
            .lock()
            .unwrap()
            .push((req.url.clone(), cookie, cx.rps));
        self.inner.send(req, profile, cx).await
    }

    fn check_url_offline(&self, url: &str) -> Result<(), FsmError> {
        self.inner.check_url_offline(url)
    }
}

#[tokio::test]
async fn replay_yahoo_redirect_handshake_reuses_jar_and_remints_on_500() {
    let base_raw = std::fs::read_to_string(fixture_path("yahoo/search/redirect_handshake.json"))
        .expect("read redirect_handshake.json");
    let mut fixture = Fixture::from_json(&base_raw).unwrap();
    let search_url = "https://search.yahoo.com/search?p=python+asyncio+taskgroup&ei=UTF-8";
    let bv_url = "https://search.yahoo.com/_bv/v.gif?orig=1";
    let html = fixture.exchanges[3].response.body_text.clone().unwrap();

    // Append exchanges for call #2 (cached jar -> 200) and call #3 (500 rejected jar -> re-mint 307 -> /_bv/v.gif -> 302 -> 200).
    fixture.exchanges.push(Exchange::new(
        FixtureRequest::new("GET", search_url),
        FixtureResponse::from_bytes(
            200,
            vec![("content-type".into(), "text/html; charset=UTF-8".into())],
            html.as_bytes(),
        ),
    ));
    fixture.exchanges.push(Exchange::new(
        FixtureRequest::new("GET", search_url),
        FixtureResponse::from_bytes(500, vec![], b""),
    ));
    fixture.exchanges.push(Exchange::new(
        FixtureRequest::new("GET", search_url),
        FixtureResponse::from_bytes(
            307,
            vec![
                ("location".into(), "/_bv/v.gif?orig=1".into()),
                (
                    "set-cookie".into(),
                    "YBV=v0.4.replay; Domain=search.yahoo.com; Path=/".into(),
                ),
            ],
            b"",
        ),
    ));
    fixture.exchanges.push(Exchange::new(
        FixtureRequest::new("GET", bv_url),
        FixtureResponse::from_bytes(
            302,
            vec![
                (
                    "location".into(),
                    "/search?p=python+asyncio+taskgroup&ei=UTF-8".into(),
                ),
                (
                    "set-cookie".into(),
                    "YBV=v0.5.replay; Domain=search.yahoo.com; Path=/".into(),
                ),
                (
                    "set-cookie".into(),
                    "A3=d=AQABreplay2; Domain=.yahoo.com; Path=/".into(),
                ),
            ],
            b"",
        ),
    ));
    fixture.exchanges.push(Exchange::new(
        FixtureRequest::new("GET", search_url),
        FixtureResponse::from_bytes(
            200,
            vec![("content-type".into(), "text/html; charset=UTF-8".into())],
            html.as_bytes(),
        ),
    ));

    let replay = Arc::new(ReplayNet::from_fixtures(vec![fixture]).unwrap());
    let rec = Arc::new(RecordingReplayNet::new(Arc::clone(&replay)));
    let deps = SourceDeps::new(
        Arc::clone(&rec) as Arc<dyn NetClient>,
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    );
    let engine = fsm_engines_serp::engines::yahoo::YahooEngine::new(deps);

    // Call 1: cold 500 -> 307 -> /_bv/v.gif -> 302 -> 200
    let SrcOutput::Hits(h1) = engine
        .call(
            "search",
            json!({"query": "python asyncio taskgroup", "limit": 10}),
            &default_cx(),
        )
        .await
        .unwrap()
    else {
        panic!("expected hits");
    };
    assert_eq!(h1.len(), 3);

    // Call 2: reuses cached jar (1 request carrying YBV=v0.3.replay & A3=d=AQABreplay)
    let SrcOutput::Hits(h2) = engine
        .call(
            "search",
            json!({"query": "python asyncio taskgroup", "limit": 10}),
            &default_cx(),
        )
        .await
        .unwrap()
    else {
        panic!("expected hits");
    };
    assert_eq!(h2.len(), 3);

    // Call 3: cached jar rejected with 500 -> re-mints via 307 -> /_bv/v.gif -> 302 -> 200
    let SrcOutput::Hits(h3) = engine
        .call(
            "search",
            json!({"query": "python asyncio taskgroup", "limit": 10}),
            &default_cx(),
        )
        .await
        .unwrap()
    else {
        panic!("expected hits");
    };
    assert_eq!(h3.len(), 3);

    assert!(replay.unmatched().is_empty());
    assert!(replay.unused().is_empty());

    let seen = rec.seen();
    assert_eq!(seen.len(), 9, "4 cold + 1 cached + 4 remint");
    // Hop 0: no cookie, primary rps charged
    assert_eq!(seen[0].1, None);
    assert!(seen[0].2.is_some());
    // Hop 1 (attempt 2): carries YBV=v0.1.replay, aux rps=None
    assert_eq!(seen[1].1.as_deref(), Some("YBV=v0.1.replay"));
    assert_eq!(seen[1].2, None);
    // Hop 2 (/_bv/v.gif): carries YBV=v0.2.replay, aux rps=None
    assert_eq!(seen[2].0, bv_url);
    assert_eq!(seen[2].1.as_deref(), Some("YBV=v0.2.replay"));
    assert_eq!(seen[2].2, None);
    // Hop 3 (final 200): carries YBV=v0.3.replay + A3=d=AQABreplay, aux rps=None
    assert!(seen[3].1.as_deref().unwrap().contains("YBV=v0.3.replay"));
    assert!(seen[3].1.as_deref().unwrap().contains("A3=d=AQABreplay"));
    assert_eq!(seen[3].2, None);
    // Call 2 (index 4): cached jar reused, primary rps charged once
    assert!(seen[4].1.as_deref().unwrap().contains("YBV=v0.3.replay"));
    assert!(seen[4].2.is_some());
    // Call 3 (index 5 = 500 primary charged; indices 6..=8 = re-mint aux rps=None)
    assert!(seen[5].2.is_some());
    assert!(seen[6..=8].iter().all(|(_, _, rps)| rps.is_none()));
    assert_eq!(
        engine
            .cached_jar("")
            .unwrap()
            .get_for_host("search.yahoo.com", "YBV")
            .as_deref(),
        Some("v0.5.replay")
    );
}

// ---------------------------------------------------------------------------
// B1: RateLimits-realistic replay (mirrors `fsm-net::HttpClient::execute`)
// ---------------------------------------------------------------------------

struct RateLimitedReplayNet {
    inner: Arc<ReplayNet>,
    limits: Arc<RateLimits>,
    observed_rps: Mutex<Vec<(String, Option<f64>)>>,
}

impl RateLimitedReplayNet {
    fn new(inner: Arc<ReplayNet>, limits: Arc<RateLimits>) -> Self {
        Self {
            inner,
            limits,
            observed_rps: Mutex::new(Vec::new()),
        }
    }

    fn charged_count(&self) -> usize {
        self.observed_rps
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, r)| r.is_some())
            .count()
    }
}

#[async_trait]
impl NetClient for RateLimitedReplayNet {
    async fn send(
        &self,
        req: HttpRequest,
        profile: TransportProfile,
        cx: &NetCx,
    ) -> Result<HttpBytes, FsmError> {
        self.observed_rps
            .lock()
            .unwrap()
            .push((req.url.clone(), cx.rps));
        if let Some(key) = &cx.host_bucket
            && cx.rps.is_some()
        {
            let source_id = req.source_id.as_deref().unwrap_or(key.as_str());
            if let Err(wait) = self.limits.acquire(key, cx.rps, Some(cx.timeout)).await {
                return Err(FsmError::RateLimited {
                    source_id: source_id.to_owned(),
                    retry_after_s: Some(wait.as_secs().max(1)),
                });
            }
        }
        self.inner.send(req, profile, cx).await
    }

    fn check_url_offline(&self, url: &str) -> Result<(), FsmError> {
        self.inner.check_url_offline(url)
    }
}

#[tokio::test]
async fn b1_limiter_realistic_googlenews_10_items_all_resolved_within_budget() {
    // Model a 10-item feed using 10 distinct copies of the recorded Google News
    // item, article page, and batchexecute reply. With an implicit burst-1
    // RateLimits bucket, only the primary RSS GET charges the limiter, so all
    // 10 publisher URLs resolve cleanly well inside RESOLVE_BUDGET.
    let raw = std::fs::read_to_string(fixture_path("googlenews/search/default.json")).unwrap();
    let base = Fixture::from_json(&raw).unwrap();
    let base_rss = base.exchanges[0].response.body_text.as_ref().unwrap();
    let base_link = &base.exchanges[1].request.url;
    let base_page = base.exchanges[1].response.body_text.as_ref().unwrap();
    let base_rpc = base.exchanges[2].response.body_text.as_ref().unwrap();

    let mut rss_items = String::new();
    let mut exchanges = Vec::new();
    for i in 0..10usize {
        let link = format!("{base_link}&copy={i}");
        let link_xml = link.replace('&', "&amp;");
        rss_items.push_str(&format!(
            "<item><title>Story {i}</title><link>{link_xml}</link><pubDate>Wed, 25 Nov 2026 12:00:00 GMT</pubDate><source>Blog</source></item>\n"
        ));
        let aid = format!("AID{i:02}");
        let ts = "1795614589";
        let sg = format!("SIG{i:02}");
        let page = format!(
            r#"<!doctype html><html><body><c-wiz data-n-a-id="{aid}" data-n-a-ts="{ts}" data-n-a-sg="{sg}"></c-wiz></body></html>"#
        );
        let freq = fsm_engines_serp::gnews::build_freq(&aid, ts, &sg).unwrap();
        let rpc = base_rpc.replace(
            "https://opensource.googleblog.com/2026/11/kubernetes-python-client-modern-overhaul-ai-ml.html",
            &format!("https://publisher{i}.example/story/{i}"),
        );
        exchanges.push(Exchange::new(
            FixtureRequest::new("GET", link),
            FixtureResponse::from_bytes(
                200,
                vec![("content-type".into(), "text/html; charset=utf-8".into())],
                page.as_bytes(),
            ),
        ));
        let mut rpc_req = FixtureRequest::new("POST", fsm_engines_serp::gnews::BATCHEXECUTE_URL);
        rpc_req.body_sha256 = Some(fsm_core::fixture::sha256_hex(freq.as_bytes()));
        exchanges.push(Exchange::new(
            rpc_req,
            FixtureResponse::from_bytes(
                200,
                vec![("content-type".into(), "application/json".into())],
                rpc.as_bytes(),
            ),
        ));
    }
    let _ = (base_rss, base_page);
    let full_rss = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><rss version="2.0"><channel>{rss_items}</channel></rss>"#
    );
    let mut all_exchanges = vec![Exchange::new(
        FixtureRequest::new(
            "GET",
            "https://news.google.com/rss/search?q=python+asyncio&hl=en-US&gl=US&ceid=US:en",
        ),
        FixtureResponse::from_bytes(
            200,
            vec![(
                "content-type".into(),
                "application/xml; charset=utf-8".into(),
            )],
            full_rss.as_bytes(),
        ),
    )];
    all_exchanges.extend(exchanges);

    let mut fixture = Fixture::new("googlenews", "search", "2026-11-25T13:48:00Z");
    fixture.exchanges = all_exchanges;

    let replay = Arc::new(ReplayNet::from_fixtures(vec![fixture]).unwrap());
    let limited = Arc::new(RateLimitedReplayNet::new(
        Arc::clone(&replay),
        Arc::new(RateLimits::new()),
    ));
    let deps = SourceDeps::new(
        Arc::clone(&limited) as Arc<dyn NetClient>,
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    );
    let engine = fsm_engines_serp::engines::googlenews::GoogleNewsEngine::new(deps);

    let started = Instant::now();
    let SrcOutput::Hits(hits) = engine
        .call(
            "search",
            json!({"query": "python asyncio", "limit": 10}),
            &default_cx(),
        )
        .await
        .unwrap()
    else {
        panic!("expected hits");
    };
    let elapsed = started.elapsed();
    assert_eq!(hits.len(), 10);
    assert!(
        hits.iter().all(|h| h.url.contains(".example/story/")),
        "all 10 publisher URLs must be resolved: {:?}",
        hits.iter().map(|h| &h.url).collect::<Vec<_>>()
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "expected no limiter stall across aux resolution hops, took {elapsed:?}"
    );
    assert_eq!(
        limited.charged_count(),
        1,
        "only the primary RSS GET may charge the rate limiter"
    );
    assert!(replay.unmatched().is_empty());
    assert!(replay.unused().is_empty());
}

#[tokio::test]
async fn b1_limiter_realistic_yahoo_bing_baidu_and_quark() {
    // 1. Yahoo cold redirect handshake (500 -> 307 -> /_bv/v.gif -> 302 -> 200)
    // under unconfigured implicit burst=1 RateLimits: completes in < 1s (0 extra wait).
    let yahoo_replay = Arc::new(
        ReplayNet::from_file(fixture_path("yahoo/search/redirect_handshake.json")).unwrap(),
    );
    let yahoo_net = Arc::new(RateLimitedReplayNet::new(
        Arc::clone(&yahoo_replay),
        Arc::new(RateLimits::new()),
    ));
    let yahoo_engine = fsm_engines_serp::engines::yahoo::YahooEngine::new(SourceDeps::new(
        Arc::clone(&yahoo_net) as Arc<dyn NetClient>,
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    ));
    let t0 = Instant::now();
    let SrcOutput::Hits(yh) = yahoo_engine
        .call(
            "search",
            json!({"query": "python asyncio taskgroup", "limit": 10}),
            &default_cx(),
        )
        .await
        .unwrap()
    else {
        panic!("expected hits");
    };
    assert_eq!(yh.len(), 3);
    assert!(
        t0.elapsed() < Duration::from_millis(500),
        "cold yahoo redirect handshake should not wait on limiter: {:?}",
        t0.elapsed()
    );
    assert_eq!(yahoo_net.charged_count(), 1);
    assert!(yahoo_replay.unused().is_empty());

    // 2. Bing warm-up + page 1 + page 2 under implicit burst=1 RateLimits:
    // only page 1 charges the limiter (warm-up and page 2 carry rps=None).
    let bing_raw = std::fs::read_to_string(fixture_path("bing/search/default.json")).unwrap();
    let mut bing_fix = Fixture::from_json(&bing_raw).unwrap();
    bing_fix.exchanges.push(Exchange::new(
        FixtureRequest::new(
            "GET",
            "https://www4.bing.com/search?q=rust+ownership+borrowing&form=QBRE&adlt=moderate&mkt=en-US&first=11",
        ),
        FixtureResponse::from_bytes(
            200,
            vec![("content-type".into(), "text/html; charset=utf-8".into())],
            br#"<ol id="b_results"><li class="b_algo"><h2><a href="https://example.org/p2">Page 2 Rust Ownership</a></h2><div class="b_caption"><p>Page two borrowing guide.</p></div></li></ol>"#,
        ),
    ));
    let bing_replay = Arc::new(ReplayNet::from_fixtures(vec![bing_fix]).unwrap());
    let bing_net = Arc::new(RateLimitedReplayNet::new(
        Arc::clone(&bing_replay),
        Arc::new(RateLimits::new()),
    ));
    let bing_engine = fsm_engines_serp::engines::bing::BingEngine::new(SourceDeps::new(
        Arc::clone(&bing_net) as Arc<dyn NetClient>,
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    ));
    let t1 = Instant::now();
    let SrcOutput::Hits(bh) = bing_engine
        .call(
            "search",
            json!({"query": "rust ownership borrowing", "limit": 15}),
            &default_cx(),
        )
        .await
        .unwrap()
    else {
        panic!("expected hits");
    };
    assert!(bh.iter().any(|h| h.url == "https://example.org/p2"));
    assert!(
        t1.elapsed() < Duration::from_millis(500),
        "bing warm-up + p1 + p2 must not stall on limiter: {:?}",
        t1.elapsed()
    );
    assert_eq!(bing_net.charged_count(), 1);
    assert!(bing_replay.unused().is_empty());

    // 2b. Baidu warm-up + search under implicit burst=1 RateLimits:
    // only primary search charges the limiter (warm-up carries rps=None).
    let baidu_replay =
        Arc::new(ReplayNet::from_file(fixture_path("baidu/search/default.json")).unwrap());
    let baidu_net = Arc::new(RateLimitedReplayNet::new(
        Arc::clone(&baidu_replay),
        Arc::new(RateLimits::new()),
    ));
    let baidu_engine = fsm_engines_serp::engines::baidu::BaiduEngine::new(SourceDeps::new(
        Arc::clone(&baidu_net) as Arc<dyn NetClient>,
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    ));
    let t_bd = Instant::now();
    let SrcOutput::Hits(bdh) = baidu_engine
        .call(
            "search",
            json!({"query": "python asyncio", "limit": 10}),
            &default_cx(),
        )
        .await
        .unwrap()
    else {
        panic!("expected hits");
    };
    assert_eq!(bdh.len(), 7);
    assert!(
        t_bd.elapsed() < Duration::from_millis(500),
        "baidu warm-up + search must not stall on limiter: {:?}",
        t_bd.elapsed()
    );
    assert_eq!(baidu_net.charged_count(), 1);
    assert!(baidu_replay.unused().is_empty());

    // 3. Quark: when server configures RateLimits from `decl.limits` (rps, burst=2),
    // two back-to-back searches both succeed immediately.
    let quark_replay =
        Arc::new(ReplayNet::from_file(fixture_path("quark/search/default.json")).unwrap());
    let limits = Arc::new(RateLimits::new());
    let quark_net = Arc::new(RateLimitedReplayNet::new(
        Arc::clone(&quark_replay),
        Arc::clone(&limits),
    ));
    let quark_engine = fsm_engines_serp::engines::quark::QuarkEngine::new(SourceDeps::new(
        Arc::clone(&quark_net) as Arc<dyn NetClient>,
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    ));
    let decl = quark_engine.decl();
    limits.configure(&decl.id, decl.limits.rps, decl.limits.burst);

    for i in 0..2 {
        let SrcOutput::Hits(qh) = quark_engine
            .call(
                "search",
                json!({"query": "python asyncio", "limit": 10}),
                &default_cx(),
            )
            .await
            .unwrap_or_else(|e| panic!("quark call #{i} failed: {e}"))
        else {
            panic!("expected hits");
        };
        assert!(qh.len() >= 5);
    }
    assert_eq!(quark_net.charged_count(), 2);
}

// ---------------------------------------------------------------------------
// M1: Cancelled warm-up must NOT be cached (Bing & Baidu)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn m1_cancelled_warmup_is_not_cached_for_bing_and_baidu() {
    for (id, fixture_rel, query) in [
        (
            "bing",
            "bing/search/default.json",
            "rust ownership borrowing",
        ),
        ("baidu", "baidu/search/default.json", "python asyncio"),
    ] {
        let replay = Arc::new(ReplayNet::from_file(fixture_path(fixture_rel)).unwrap());
        let deps = SourceDeps::new(
            Arc::clone(&replay) as Arc<dyn NetClient>,
            Arc::new(NullStore::new()),
            Arc::new(Config::default()),
        );
        let mut reg = Registry::new();
        register(&mut reg, &deps).unwrap();
        let src = reg.get(id).unwrap();

        // Call #1 with an already-cancelled token must fail without caching an
        // empty failed-warmup jar.
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let cancel_cx = SrcCx::from_config(&Config::default(), cancelled, Duration::from_secs(5));
        let err = src
            .call("search", json!({"query": query, "limit": 10}), &cancel_cx)
            .await
            .unwrap_err();
        assert!(matches!(err, FsmError::Timeout(0)), "{id}: {err:?}");

        // Call #2 with a fresh token MUST still replay warm-up exchange #0 and
        // search exchange #1.
        let SrcOutput::Hits(hits) = src
            .call(
                "search",
                json!({"query": query, "limit": 10}),
                &default_cx(),
            )
            .await
            .unwrap_or_else(|e| panic!("{id} call #2 failed: {e}"))
        else {
            panic!("expected hits");
        };
        assert!(hits.len() >= 5, "{id}: expected hits");
        assert!(
            replay.unused().is_empty(),
            "{id}: warm-up exchange #0 was skipped because cancelled warm-up was cached! unused={:?}",
            replay.unused()
        );
    }
}

#[tokio::test]
async fn registered_sources_reject_invalid_op_and_empty_query() {
    let net =
        Arc::new(ReplayNet::from_file(fixture_path("duckduckgo/search/default.json")).unwrap());
    let deps = SourceDeps::new(
        net as Arc<dyn NetClient>,
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    );
    let mut reg = Registry::new();
    register(&mut reg, &deps).unwrap();

    for id in ENGINE_IDS {
        let src = reg.get(id).unwrap();
        let bad_op = src
            .call("unknown", json!({"query": "rust"}), &default_cx())
            .await
            .unwrap_err();
        assert!(
            matches!(bad_op, FsmError::InvalidInput(_)),
            "{id}: {bad_op:?}"
        );

        let empty_q = src
            .call("search", json!({"query": "   "}), &default_cx())
            .await
            .unwrap_err();
        assert!(
            matches!(empty_q, FsmError::InvalidInput(_)),
            "{id}: {empty_q:?}"
        );
    }
}
