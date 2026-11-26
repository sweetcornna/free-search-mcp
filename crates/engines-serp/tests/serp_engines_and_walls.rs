//! Tests for `duckduckgo`, `so360`, `baidu`, `daum`, `register()`, and wall/gate
//! detection (porting applicable cases from `test_engines_extra.py` and `test_http_walls.py`).

use async_trait::async_trait;
use fsm_core::api::Freshness;
use fsm_core::config::{Config, SafeSearch};
use fsm_core::net::{HttpBytes, HttpRequest, NetClient, NetCx};
use fsm_core::source::{PoolRole, Source, SrcCx, SrcOutput, TransportProfile};
use fsm_core::{FsmError, NullStore, Registry, SourceDeps};
use fsm_engines_serp::common::{SerpQuery, check_wall_if_empty, detect_gate};
use fsm_engines_serp::engines::{baidu, bing, daum, duckduckgo, quark, so360, yahoo, yahoojp};
use fsm_engines_serp::{ENGINE_IDS, register};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn default_cx() -> SrcCx {
    SrcCx::from_config(
        &Config::default(),
        CancellationToken::new(),
        Duration::from_secs(5),
    )
}

struct StaticHtmlNet {
    status: u16,
    body: String,
}

#[async_trait]
impl NetClient for StaticHtmlNet {
    async fn send(
        &self,
        req: HttpRequest,
        _profile: TransportProfile,
        _cx: &NetCx,
    ) -> Result<HttpBytes, FsmError> {
        Ok(HttpBytes::new(
            req.url,
            self.status,
            vec![("content-type".into(), "text/html; charset=utf-8".into())],
            self.body.clone(),
        ))
    }

    fn check_url_offline(&self, _url: &str) -> Result<(), FsmError> {
        Ok(())
    }
}

#[test]
fn test_register_registers_all_13_engines_with_expected_roles() {
    let deps = SourceDeps::new(
        Arc::new(StaticHtmlNet {
            status: 200,
            body: String::new(),
        }),
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    );
    let mut reg = Registry::new();
    register(&mut reg, &deps).expect("register all 13");
    assert_eq!(reg.len(), 13);
    assert_eq!(ENGINE_IDS.len(), 13);

    for id in ENGINE_IDS {
        let src = reg.get(id).unwrap_or_else(|| panic!("missing {id}"));
        let decl = src.decl();
        assert_eq!(decl.id, *id);
        assert!(!decl.summary.is_empty() && !decl.summary.contains('\n'));
        assert!(decl.op("search").is_some(), "{id} missing op.search");
    }

    // Pool roles match SPEC.md §5
    for id in ["duckduckgo", "bing", "yahoojp", "seznam"] {
        assert_eq!(
            reg.get(id).unwrap().decl().pool_role,
            PoolRole::Default,
            "{id}"
        );
    }
    for id in ["yahoo", "youcom"] {
        assert_eq!(
            reg.get(id).unwrap().decl().pool_role,
            PoolRole::Reserve,
            "{id}"
        );
    }
    for id in ["so360", "quark", "baidu"] {
        assert_eq!(
            reg.get(id).unwrap().decl().pool_role,
            PoolRole::Locale("zh".into()),
            "{id}"
        );
    }
    for id in ["naver", "daum"] {
        assert_eq!(
            reg.get(id).unwrap().decl().pool_role,
            PoolRole::Locale("ko".into()),
            "{id}"
        );
    }
    assert_eq!(
        reg.get("googlenews").unwrap().decl().pool_role,
        PoolRole::ScopeOnly
    );
    assert_eq!(
        reg.get("bilibili").unwrap().decl().pool_role,
        PoolRole::Explicit
    );

    // Transports match Spike N / SPEC.md §7.1
    assert_eq!(
        reg.get("bing").unwrap().decl().transport,
        TransportProfile::EdgePreflight
    );
    assert_eq!(
        reg.get("googlenews").unwrap().decl().transport,
        TransportProfile::Plain
    );
}

// ---------------------------------------------------------------------------
// DuckDuckGo (port of test_engines_extra.py A1/A2)
// ---------------------------------------------------------------------------

#[test]
fn test_unwrap_single_decodes_literal_percent_xx() {
    let wrapped =
        "https://duckduckgo.com/l/?uddg=https%3A%2F%2Fen.wikipedia.org%2Fwiki%2FC%252B%252B";
    assert_eq!(
        duckduckgo::unwrap_ddg_url(wrapped),
        "https://en.wikipedia.org/wiki/C%2B%2B"
    );
    let space_wrapped = "https://duckduckgo.com/l/?uddg=https%3A%2F%2Fa.com%2Fb%2520c";
    assert_eq!(
        duckduckgo::unwrap_ddg_url(space_wrapped),
        "https://a.com/b%20c"
    );
    let plain = "https://example.com/page";
    assert_eq!(duckduckgo::unwrap_ddg_url(plain), plain);
    assert_eq!(
        duckduckgo::unwrap_ddg_url(&duckduckgo::unwrap_ddg_url(plain)),
        plain
    );
}

#[test]
fn test_ddg_build_url_emits_safesearch_region_and_freshness() {
    let mut cx = default_cx();
    cx.safesearch = SafeSearch::Strict;
    cx.region = "uk-en".into();
    cx.freshness = Some(Freshness::Week);
    let q = SerpQuery::from_call(
        "duckduckgo",
        "search",
        &json!({"query": "hello", "limit": 10}),
        &cx,
        &Config::default(),
    )
    .unwrap();
    let url = duckduckgo::build_url(&q);
    assert!(url.contains("kp=1"));
    assert!(url.contains("kl=uk-en"));
    assert!(url.contains("df=w"));

    cx.safesearch = SafeSearch::Off;
    let q_off = SerpQuery::from_call(
        "duckduckgo",
        "search",
        &json!({"query": "hello"}),
        &cx,
        &Config::default(),
    )
    .unwrap();
    assert!(duckduckgo::build_url(&q_off).contains("kp=-2"));
}

#[test]
fn test_ddg_parse_skips_ads_and_dedups() {
    let html = r#"<html><body>
      <div class="result result--ad"><a class="result__a" href="https://duckduckgo.com/y.js?ad_provider=x">Ad</a></div>
      <div class="result web-result">
        <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdocs.rs%2Ftokio">Tokio Docs</a>
        <a class="result__snippet">Apr 28, 2026 — Async runtime for Rust.</a>
      </div>
      <div class="result web-result">
        <a class="result__a" href="https://docs.rs/tokio">Tokio Docs Dup</a>
        <a class="result__snippet">Duplicate</a>
      </div>
    </body></html>"#;
    let hits = duckduckgo::parse(html);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].url, "https://docs.rs/tokio");
    assert_eq!(hits[0].title, "Tokio Docs");
    assert_eq!(hits[0].date.as_deref(), Some("2026-04-28"));
}

// ---------------------------------------------------------------------------
// So360 & Baidu
// ---------------------------------------------------------------------------

#[test]
fn test_cn_indexes_inject_query_operators() {
    let mut cx = default_cx();
    cx.domains = vec!["python.org".into(), "-spam.example".into()];
    let p = json!({"query": "asyncio", "limit": 10, "filetype": "pdf"});
    let q_so = SerpQuery::from_call("so360", "search", &p, &cx, &Config::default()).unwrap();
    let u_so = so360::build_url(&q_so);
    assert!(u_so.contains("site%3Apython.org"));
    assert!(u_so.contains("-site%3Aspam.example"));
    assert!(u_so.contains("filetype%3Apdf"));
    assert!(u_so.contains("secure=1"));

    let q_bd = SerpQuery::from_call("baidu", "search", &p, &cx, &Config::default()).unwrap();
    let u_bd = baidu::build_url(&q_bd);
    assert!(u_bd.contains("site%3Apython.org"));
    assert!(u_bd.contains("-site%3Aspam.example"));
    assert!(u_bd.contains("filetype%3Apdf"));
}

#[test]
fn test_so360_parse_prefers_data_mdurl_and_dedups() {
    let html = r#"<ul>
      <li class="res-list">
        <h3><a href="https://www.so.com/link?m=abc" data-mdurl="https://docs.python.org/3/library/asyncio.html">Python asyncio 文档</a></h3>
        <p class="res-desc">2026-10-01 Python 标准库异步 I/O。</p>
      </li>
      <li class="res-list">
        <h3><a href="https://realpython.com/async-io-python/">Async IO in Python</a></h3>
        <div class="res-comm-con">Complete walkthrough of asyncio.</div>
      </li>
      <li class="res-list">
        <h3><a href="https://docs.python.org/3/library/asyncio.html">Dup</a></h3>
      </li>
    </ul>"#;
    let hits = so360::parse(html);
    assert_eq!(
        hits.iter().map(|h| h.url.as_str()).collect::<Vec<_>>(),
        vec![
            "https://docs.python.org/3/library/asyncio.html",
            "https://realpython.com/async-io-python/",
        ]
    );
    assert_eq!(hits[0].date.as_deref(), Some("2026-10-01"));
    assert_eq!(hits[1].snippet, "Complete walkthrough of asyncio.");
}

#[test]
fn test_baidu_parse_prefers_mu_over_redirect() {
    let html = r#"<div>
      <div class="result c-container" mu="https://docs.python.org/3/library/asyncio.html">
        <h3 class="t"><a href="http://www.baidu.com/link?url=opaque123">asyncio — Asynchronous I/O</a></h3>
        <div class="c-abstract">2026年10月1日 Python 异步编程标准库。</div>
      </div>
      <div class="result-op c-container">
        <h3><a href="https://realpython.com/async-io-python/">Real Python Asyncio</a></h3>
        <span class="content-right_8Zs40">Hands-on asyncio guide.</span>
      </div>
    </div>"#;
    let hits = baidu::parse(html);
    assert_eq!(
        hits.iter().map(|h| h.url.as_str()).collect::<Vec<_>>(),
        vec![
            "https://docs.python.org/3/library/asyncio.html",
            "https://realpython.com/async-io-python/",
        ]
    );
    assert_eq!(hits[0].date.as_deref(), Some("2026-10-01"));
    assert_eq!(hits[1].snippet, "Hands-on asyncio guide.");
}

// ---------------------------------------------------------------------------
// Daum (new 13th SERP engine)
// ---------------------------------------------------------------------------

#[test]
fn test_daum_build_url_and_parse_cubic_template_and_classic_fallback() {
    let mut cx = default_cx();
    cx.freshness = Some(Freshness::Week);
    cx.domains = vec!["tistory.com".into()];
    let q = SerpQuery::from_call(
        "daum",
        "search",
        &json!({"query": "파이썬 비동기", "limit": 10}),
        &cx,
        &Config::default(),
    )
    .unwrap();
    let url = daum::build_url(&q);
    assert!(url.starts_with("https://search.daum.net/search?w=fusion&q="));
    assert!(url.contains("site%3Atistory.com"));
    assert!(url.ends_with("&period=w"));

    let cubic_html = r#"<!doctype html><html><body><div id="twcColl"><div class="cubic"><template>
      <c-container data-dc="TWC">
        <c-card data-docid="t1">
          <c-doc-web>
            <c-title slot="title" data-href="https://the-black.tistory.com/163"><b>Python</b> <b>asyncio</b> TaskGroup</c-title>
            <c-contents-desc slot="contents"><b>Python</b>의 <b>asyncio</b>.TaskGroup 정리.</c-contents-desc>
          </c-doc-web>
          <c-footer><c-footer-desc slot="info">2026.11.06</c-footer-desc></c-footer>
        </c-card>
        <c-card data-docid="ad">
          <c-doc-web>
            <c-title slot="title" data-href="https://ader.naver.com/v1/ad123">광고</c-title>
          </c-doc-web>
        </c-card>
        <c-card data-docid="t2">
          <c-doc-web>
            <c-title slot="title" data-href="https://youngsam.net/q26d">asyncio.gather 취소</c-title>
            <c-contents-desc slot="contents">타임아웃을 주고 일부 Task만 취소.</c-contents-desc>
          </c-doc-web>
          <c-footer><c-footer-desc slot="info">3일 전</c-footer-desc></c-footer>
        </c-card>
        <c-card data-docid="t3">
          <c-doc-web>
            <c-title slot="title" data-href="https://example.kr/hours">비동기 실전 팁</c-title>
            <c-contents-desc slot="contents">방금 올라온 요약.</c-contents-desc>
          </c-doc-web>
          <c-footer><c-footer-desc slot="info">5시간 전</c-footer-desc></c-footer>
        </c-card>
        <c-card data-docid="dup">
          <c-doc-web>
            <c-title slot="title" data-href="https://the-black.tistory.com/163">Duplicate</c-title>
          </c-doc-web>
        </c-card>
      </c-container>
    </template></div></div></body></html>"#;

    let hits = daum::parse(cubic_html);
    assert_eq!(
        hits.iter().map(|h| h.url.as_str()).collect::<Vec<_>>(),
        vec![
            "https://the-black.tistory.com/163",
            "https://youngsam.net/q26d",
            "https://example.kr/hours",
        ]
    );
    assert_eq!(hits[0].title, "Python asyncio TaskGroup");
    assert_eq!(hits[0].snippet, "Python의 asyncio.TaskGroup 정리.");
    assert_eq!(hits[0].date.as_deref(), Some("2026-11-06"));
    assert_eq!(hits[1].date.as_deref(), Some("3 days ago"));
    assert_eq!(hits[2].date.as_deref(), Some("5 hours ago"));
    assert!(hits.iter().all(|h| h.source == "daum"));

    // Classic fallback when no <c-card> is present
    let classic = r#"<ul class="c-list-basic"><li>
      <div class="item-title"><a href="https://example.kr/post">Classic Daum Hit</a></div>
      <p class="desc">2026-10-12 Classic summary.</p>
    </li></ul>"#;
    let c_hits = daum::parse(classic);
    assert_eq!(c_hits.len(), 1);
    assert_eq!(c_hits[0].url, "https://example.kr/post");
    assert_eq!(c_hits[0].date.as_deref(), Some("2026-10-12"));
}

// ---------------------------------------------------------------------------
// Wall detection invariants (Spike N §4 #8 + test_http_walls.py)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_walls_trigger_only_on_zero_results_and_return_upstream_error() {
    // 1. Real hits present alongside an `h-captcha` / `CONF_RECAPTCHA_KEY` script
    // must NEVER trigger a false wall (Spike N §4 #8).
    let ok_html_with_captcha_script = r#"<html><head><script>var CONF_RECAPTCHA_KEY="x"; // h-captcha</script></head><body>
      <div class="result"><a class="result__a" href="https://example.org/ok">Good Hit</a><a class="result__snippet">Fine.</a></div>
    </body></html>"#;
    let deps_ok = SourceDeps::new(
        Arc::new(StaticHtmlNet {
            status: 200,
            body: ok_html_with_captcha_script.to_owned(),
        }),
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    );
    let ddg = duckduckgo::DuckDuckGoEngine::new(deps_ok);
    let SrcOutput::Hits(hits) = ddg
        .call("search", json!({"query": "test"}), &default_cx())
        .await
        .expect("page with hits must NOT be flagged as wall")
    else {
        panic!("expected hits");
    };
    assert_eq!(hits.len(), 1);

    // 2. Zero results + DDG HTTP 202 anomaly-modal -> Upstream captcha gate error!
    let deps_wall = SourceDeps::new(
        Arc::new(StaticHtmlNet {
            status: 202,
            body: r#"<html><body><div class="anomaly-modal">Please confirm you are human</div></body></html>"#.to_owned(),
        }),
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    );
    let ddg_walled = duckduckgo::DuckDuckGoEngine::new(deps_wall);
    let err = ddg_walled
        .call("search", json!({"query": "test"}), &default_cx())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("captcha-gated"), "{err}");

    // 3. Zero results + NO wall markers -> clean empty Ok(Hits([])), NOT an error!
    let deps_empty = SourceDeps::new(
        Arc::new(StaticHtmlNet {
            status: 200,
            body: "<html><body>No results found for your query.</body></html>".to_owned(),
        }),
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    );
    let ddg_empty = duckduckgo::DuckDuckGoEngine::new(deps_empty);
    let SrcOutput::Hits(empty) = ddg_empty
        .call("search", json!({"query": "test"}), &default_cx())
        .await
        .unwrap()
    else {
        panic!("expected hits");
    };
    assert!(empty.is_empty());

    // 4. Engine-specific gate markers take priority over generic ones.
    assert_eq!(
        detect_gate("访问异常页面 请输入验证码", so360::GATE_MARKERS),
        Some("captcha")
    );
    assert_eq!(
        detect_gate("百度安全验证", baidu::GATE_MARKERS),
        Some("captcha")
    );
    assert_eq!(
        detect_gate("_____tmd_____/punish?x5secdata=abc", quark::GATE_MARKERS),
        Some("captcha")
    );
    assert_eq!(
        detect_gate(
            r#"<meta property="og:url" content="https://privacy.yahoo.co.jp/areacheck">"#,
            yahoojp::GATE_MARKERS
        ),
        Some("region")
    );
    assert_eq!(
        check_wall_if_empty(
            "walled",
            200,
            "<html><body>Please complete the challenge</body></html>",
            &[],
            vec![]
        )
        .unwrap_err()
        .to_string(),
        "Upstream error (walled): walled was captcha-gated"
    );
}

#[test]
fn test_baidu_parse_extracts_summary_text_and_drops_internal_urls() {
    // I3: `[class*="summary-text"]` extracts modern Baidu snippets and drops
    // `baidu.com/s?...`, `nourl.ubs.baidu.com`, and `recommend_list.baidu.com`.
    let html = r#"<html><body>
      <div class="result-op c-container" mu="http://nourl.ubs.baidu.com/61344">
        <h3><a href="https://www.baidu.com/link?url=x">UBS Card</a></h3>
      </div>
      <div class="result-op c-container" mu="http://28608.recommend_list.baidu.com">
        <h3><a href="https://www.baidu.com/link?url=y">Recommend Card</a></h3>
      </div>
      <div class="result-op c-container" mu="https://www.baidu.com/s?tn=news&amp;wd=python">
        <h3><a href="https://www.baidu.com/s?tn=news&amp;wd=python">Baidu News Agg</a></h3>
      </div>
      <div class="result c-container" mu="https://docs.python.org/zh-cn/3/library/asyncio.html">
        <h3><a href="https://www.baidu.com/link?url=z">asyncio — Python 文档</a></h3>
        <span class="content-right_2s-H4 summary-text_560AW">2026年10月1日 asyncio 是用来编写并发代码的库。</span>
      </div>
      <div class="result c-container" mu="https://developer.baidu.com/article/details/3294940">
        <h3><a href="https://www.baidu.com/link?url=w">百度开发者中心文章</a></h3>
        <div class="summary-text_abc">合法的开发者博客结果不应被过滤。</div>
      </div>
    </body></html>"#;
    let hits = baidu::parse(html);
    assert_eq!(
        hits.iter().map(|h| h.url.as_str()).collect::<Vec<_>>(),
        vec![
            "https://docs.python.org/zh-cn/3/library/asyncio.html",
            "https://developer.baidu.com/article/details/3294940",
        ]
    );
    assert!(hits[0].snippet.contains("编写并发代码"));
    assert_eq!(hits[0].date.as_deref(), Some("2026-10-01"));
    assert!(hits[1].snippet.contains("开发者博客"));
}

#[tokio::test]
async fn test_non_ascii_headers_do_not_panic_unit_and_engine_level() {
    use fsm_engines_serp::common::decode_body;
    use fsm_engines_serp::cookie_jar::CookieJar;

    // B2 unit: Content-Type with multi-byte chars around byte 8 must not panic.
    for ct in [
        "text/html; 日本語",
        "text/html; abc\u{fffd}\u{fffd}",
        "text/html; 日本語=1; charset=UTF-8",
    ] {
        let resp = HttpBytes::new(
            "https://example.test/",
            200,
            vec![("content-type".into(), ct.into())],
            "hello",
        );
        assert_eq!(decode_body(&resp), "hello");
    }

    // B2 unit: Set-Cookie with multi-byte chars around byte 7/8 must not panic.
    let mut jar = CookieJar::new();
    jar.absorb_headers(
        "www4.bing.com",
        &[("Set-Cookie".into(), "MUID=1; 日本語x; Path=/".into())],
    );
    assert_eq!(
        jar.get_for_host("www4.bing.com", "MUID").as_deref(),
        Some("1")
    );

    // B2 engine-level: DuckDuckGo with non-ASCII Content-Type + Bing with non-ASCII Set-Cookie.
    struct WeirdHeaderNet {
        headers: Vec<(String, String)>,
        body: String,
    }
    #[async_trait]
    impl NetClient for WeirdHeaderNet {
        async fn send(
            &self,
            req: HttpRequest,
            _profile: TransportProfile,
            _cx: &NetCx,
        ) -> Result<HttpBytes, FsmError> {
            Ok(HttpBytes::new(
                req.url,
                200,
                self.headers.clone(),
                self.body.clone(),
            ))
        }
        fn check_url_offline(&self, _url: &str) -> Result<(), FsmError> {
            Ok(())
        }
    }

    let ddg_deps = SourceDeps::new(
        Arc::new(WeirdHeaderNet {
            headers: vec![("content-type".into(), "text/html; 日本語".into())],
            body: r#"<div class="result"><a class="result__a" href="https://example.org/a">OK</a><a class="result__snippet">Fine</a></div>"#.into(),
        }),
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    );
    let SrcOutput::Hits(ddg_hits) = duckduckgo::DuckDuckGoEngine::new(ddg_deps)
        .call("search", json!({"query": "rust"}), &default_cx())
        .await
        .unwrap()
    else {
        panic!("expected hits");
    };
    assert_eq!(ddg_hits.len(), 1);

    let bing_deps = SourceDeps::new(
        Arc::new(WeirdHeaderNet {
            headers: vec![
                ("content-type".into(), "text/html; 日本語".into()),
                ("set-cookie".into(), "MUID=1; 日本語x; Path=/".into()),
            ],
            body: r#"<ol id="b_results"><li class="b_algo"><h2><a href="https://example.org/rust">Rust Ownership</a></h2><div class="b_caption"><p>Rust ownership and borrowing.</p></div></li></ol>"#.into(),
        }),
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    );
    let SrcOutput::Hits(bing_hits) = bing::BingEngine::new(bing_deps)
        .call(
            "search",
            json!({"query": "rust ownership borrowing"}),
            &default_cx(),
        )
        .await
        .unwrap()
    else {
        panic!("expected hits");
    };
    assert_eq!(bing_hits.len(), 1);
}

#[test]
fn test_finalize_hits_pdf_keeps_query_and_fragment_urls() {
    use fsm_core::types::{Hit, HitKind};
    use fsm_engines_serp::common::{finalize_hits, make_hit};

    // M2: `.pdf?dl=1`, `.pdf#p2`, and `.pdf` must all be kept under filetype=pdf.
    let mk = |u: &str| {
        make_hit(
            "test",
            HitKind::Web,
            "T".into(),
            u.into(),
            "".into(),
            None,
            None,
        )
    };
    let hits: Vec<Hit> = vec![
        mk("https://example.org/a.pdf?dl=1"),
        mk("https://example.org/a.pdf#p2"),
        mk("https://example.org/a.pdf"),
        mk("https://example.org/a.html?f=x.pdf"),
    ];
    let kept = finalize_hits(hits, &[], &[], Some("pdf"), 10);
    assert_eq!(
        kept.iter().map(|h| h.url.as_str()).collect::<Vec<_>>(),
        vec![
            "https://example.org/a.pdf?dl=1",
            "https://example.org/a.pdf#p2",
            "https://example.org/a.pdf",
        ]
    );
}

#[test]
fn test_all_13_engines_declare_search_probe_spec() {
    // M6: every registered SERP engine's `search` op declares a valid ProbeSpec.
    let deps = SourceDeps::new(
        Arc::new(StaticHtmlNet {
            status: 200,
            body: String::new(),
        }),
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    );
    let mut reg = Registry::new();
    register(&mut reg, &deps).unwrap();

    for id in ENGINE_IDS {
        let src = reg.get(id).unwrap();
        let decl = src.decl();
        let search_op = decl
            .ops
            .iter()
            .find(|op| op.name == "search")
            .unwrap_or_else(|| panic!("{id} missing search op"));
        let probe = search_op
            .probe
            .as_ref()
            .unwrap_or_else(|| panic!("{id}.search missing ProbeSpec"));
        assert!(probe.min_results >= 1, "{id}: min_results must be >= 1");
        let q = probe
            .params
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(!q.trim().is_empty(), "{id}: probe query must not be empty");
    }
}

#[test]
fn test_unescape_html_full_html5_and_linear_time_on_1mb_ampersands() {
    use fsm_engines_serp::common::unescape_html;
    use std::time::Instant;

    // M7: full HTML5 named entities beyond the 22 hardcoded ones.
    assert_eq!(
        unescape_html("Caf&eacute; &uuml;ber &ccedil;a &euro;5 &mdash; &notanentity; a < b"),
        "Café über ça €5 — &notanentity; a < b"
    );

    // M8: 1 MB of '&' must finish linearly (< 100 ms even in debug builds).
    let payload = "&".repeat(1_000_000);
    let started = Instant::now();
    let out = unescape_html(&payload);
    let elapsed = started.elapsed();
    assert_eq!(out.len(), 1_000_000);
    assert!(
        elapsed < Duration::from_millis(100),
        "unescape_html took {elapsed:?} on 1 MB of '&' (expected < 100 ms)"
    );
}

#[tokio::test]
async fn test_per_call_deadline_bounds_multi_hop_timeouts() {
    use std::sync::Mutex;

    // M4: multi-hop Yahoo handshake shares one per-call deadline instead of
    // resetting a fresh full timeout per hop, and stops with Timeout when spent.
    struct SlowHopNet {
        seen_timeouts: Mutex<Vec<Duration>>,
    }
    #[async_trait]
    impl NetClient for SlowHopNet {
        async fn send(
            &self,
            req: HttpRequest,
            _profile: TransportProfile,
            cx: &NetCx,
        ) -> Result<HttpBytes, FsmError> {
            self.seen_timeouts.lock().unwrap().push(cx.timeout);
            tokio::time::sleep(Duration::from_millis(45)).await;
            Ok(HttpBytes::new(req.url, 500, vec![], ""))
        }
        fn check_url_offline(&self, _url: &str) -> Result<(), FsmError> {
            Ok(())
        }
    }

    let net = Arc::new(SlowHopNet {
        seen_timeouts: Mutex::new(Vec::new()),
    });
    let deps = SourceDeps::new(
        net.clone(),
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    );
    let engine = yahoo::YahooEngine::new(deps);
    let cx = SrcCx::from_config(
        &Config::default(),
        CancellationToken::new(),
        Duration::from_millis(80),
    );
    let err = engine
        .call("search", json!({"query": "python asyncio"}), &cx)
        .await
        .unwrap_err();
    assert!(
        matches!(err, FsmError::Timeout(_)),
        "expected per-call deadline Timeout, got {err:?}"
    );
    let seen = net.seen_timeouts.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "should expire before attempt 3: {seen:?}");
    assert!(
        seen[1] < seen[0],
        "second hop timeout ({:?}) must be strictly smaller than first ({:?})",
        seen[1],
        seen[0]
    );
}
