//! Port of `tests/test_yahoo.py`: Yahoo parsing, `/RU=` unwrapping (incl. Spike N `rd.*`),
//! `build_url`, and full `_bv` cookie handshake + single-flight tests.

use async_trait::async_trait;
use fsm_core::api::Freshness;
use fsm_core::config::Config;
use fsm_core::net::{HttpBytes, HttpRequest, NetClient, NetCx};
use fsm_core::source::{Source, SrcCx, SrcOutput, TransportProfile};
use fsm_core::url_canon::resolve_yahoo_url;
use fsm_core::{FsmError, NullStore, SourceDeps};
use fsm_engines_serp::common::SerpQuery;
use fsm_engines_serp::engines::yahoo::{
    ClockFn, EgressFn, HANDSHAKE_ATTEMPTS, JAR_TTL_SECS, REMINT_ATTEMPTS, YahooEngine, build_url,
    is_dropped, parse,
};
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use url::Url;

fn wrap(target_encoded: &str, token: &str) -> String {
    format!(
        "https://r.search.yahoo.com/_ylt={token};_ylu=Y29sbwN1cy1lYXN0LTEEcG9zAzEEdnRpZAMEc2VjA3Ny/RV=2/RE=1794628399/RO=10/RU={target_encoded}/RK=2/RS=TgXmVHRHuwoXIIPaa1P2PwBK3Qg-"
    )
}

fn row(
    href: &str,
    site: &str,
    crumb: &str,
    title_html: &str,
    snippet_html: &str,
    cls: &str,
) -> String {
    format!(
        r#"<li><div class="{cls}" data-yga='{{"yModuleName":"Sr","yModType":"algo"}}'>
  <div class="compTitle options-toggle">
    <a class="d-ib va-top mt-38 mb-4 mxw-100p" data-matarget="algo" target="_blank" href="{href}">
      <div class="d-ib p-abs t-0 l-0">
        <span class="d-ib va-mid"><span class="fc-141414 d-b">{site}</span>{crumb}</span>
      </div>
      <h3 style="display:block" class="title"><span class="d-b">{title_html}</span></h3>
    </a>
  </div>
  <div class="compText aAbs"><p>{snippet_html}</p></div>
</div></li>"#
    )
}

fn sample_page() -> String {
    let gfg = wrap(
        "https%3a%2f%2fwww.geeksforgeeks.org%2fpython%2fpython-taskgroups-with-asyncio%2f",
        "A2RRAQgvZuVqxwIAOXlXNyoA",
    );
    let docs = wrap(
        "https%3a%2f%2fdocs.python.org%2f3%2flibrary%2fasyncio-task.html",
        "A2RRAQgvZuVqxwIAP3lXNyoA",
    );
    let cpp = wrap(
        "https%3a%2f%2fen.wikipedia.org%2fwiki%2fC%252B%252B",
        "A2RRAQgvZuVqxwIAQnlXNyoA",
    );
    let ad = "https://r.search.yahoo.com/rdclks/dWU9NTQ4MWZudGxlYXBvMSZ1dD0xNzkzNDE5MDA5MTU0/RV=2/RE=1796011009/RO=14/RU=https%3a%2f%2fwww.bing.com%2faclick%3fld%3de8rIfY2Yh2IMP0HDGf%26u%3daHR0cHMlM2ElMmYlMmZ3d3cuaXNsYW5kLmlv/RK=2/RS=hsTtS668IcUH8qiR4HyHe8atgVU-";
    let privacy = "https://r.search.yahoo.com/_ylt=A2RRAQgvZuVqxwIAUXlXNyoA/RV=2/RE=1794628399/RO=10/RU=https%3a%2f%2fguce.yahoo.com%2fstate-controls%3flocale%3den-US%26state%3dCA/RK=2/RS=yP5tQTB6xKuQpZ3ohAlA7RnUyzY-";

    format!(
        r#"<!doctype html><html id="atomic" lang="en-US"><head><title>Yahoo Search Results</title></head><body>
<ol class="reg scta searchCenterTopAds">
  <li class="last"><div class="dd fst lst ads"><div class="layoutMiddle">
    <div class="compTitle"><a data-matarget="ad" href="{ad}"><h3 class="title"><span>Island Zero Trust Browser</span></h3></a></div>
    <div class="compText"><p>Put AI in the browser.</p></div>
  </div></div></li>
</ol>
<ol class="reg searchCenterMiddle">
{}
{}
{}
{}
</ol>
<div id="ft"><ul><li class="sdk"><a href="{privacy}">Your Privacy Choices</a></li></ul></div>
</body></html>"#,
        row(
            &gfg,
            "GeeksForGeeks",
            "https://www.geeksforgeeks.org › python",
            "Python Taskgroups with asyncIO - GeeksforGeeks",
            r#"<span class="fc-smoke">Jul 23, 2025 · </span>  Prerequisite <b>Exception</b> <b>Handling</b> <b>Asyncio</b> <b>Exception</b> Groups in <b>Python</b>"#,
            "dd fst algo algo-sr relsrch Sr",
        ),
        row(
            &docs,
            "Python",
            "https://docs.python.org › 3 › library",
            "Coroutines and tasks — Python 3.15.2 documentation",
            r#"<span class="fc-smoke">1 day ago · </span>  Note <b>asyncio</b>.<b>TaskGroup</b>.create_task () is a new alternative leveraging structural concurrency"#,
            "dd algo algo-sr relsrch Sr",
        ),
        row(
            &cpp,
            "Wikipedia",
            "https://en.wikipedia.org › wiki › C++",
            "C++ - Wikipedia",
            "C++ is a high-level, general-purpose programming language.",
            "dd algo algo-sr relsrch richAlgo",
        ),
        row(
            &gfg.replace("OXlXNyoA", "QXlXNyoA"),
            "GeeksForGeeks",
            "https://www.geeksforgeeks.org",
            "Python Taskgroups with asyncIO (again)",
            "The same page under a different tracking token.",
            "dd lst algo algo-sr relsrch Sr",
        ),
    )
}

#[test]
fn test_parse_unwraps_redirects_to_publisher_urls() {
    let results = parse(&sample_page());
    assert_eq!(
        results.iter().map(|r| r.url.as_str()).collect::<Vec<_>>(),
        vec![
            "https://www.geeksforgeeks.org/python/python-taskgroups-with-asyncio/",
            "https://docs.python.org/3/library/asyncio-task.html",
            "https://en.wikipedia.org/wiki/C%2B%2B",
        ]
    );
    assert!(results.iter().all(|r| r.source == "yahoo"));
}

#[test]
fn test_parse_takes_title_from_h3_and_snippet_with_its_date() {
    let results = parse(&sample_page());
    let gfg = &results[0];
    let docs = &results[1];
    assert_eq!(gfg.title, "Python Taskgroups with asyncIO - GeeksforGeeks");
    assert!(
        gfg.snippet
            .starts_with("Jul 23, 2025 · Prerequisite Exception Handling Asyncio")
    );
    assert_eq!(gfg.date.as_deref(), Some("2025-07-23"));
    assert_eq!(gfg.date_source.as_deref(), Some("snippet"));
    assert!(docs.snippet.contains("asyncio.TaskGroup.create_task ()"));
    assert_eq!(docs.date.as_deref(), Some("1 day ago"));
}

#[test]
fn test_parse_skips_ads_and_yahoo_links() {
    let urls: Vec<String> = parse(&sample_page()).into_iter().map(|r| r.url).collect();
    assert!(
        !urls
            .iter()
            .any(|u| u.contains("bing.com") || u.contains("rdclks") || u.contains("island.io"))
    );
    assert!(!urls.iter().any(|u| u.contains("yahoo.com")));
}

#[test]
fn test_parse_dedups_the_same_target_under_different_tokens() {
    let results = parse(&sample_page());
    assert_eq!(results.len(), 3);
}

#[test]
fn test_unparseable_page_yields_nothing() {
    for html in [
        "",
        "<html><body>Something went wrong</body></html>",
        "<<<not html at all>>>",
        r#"<div class="dd algo"><div class="compTitle"></div></div>"#,
    ] {
        assert!(parse(html).is_empty(), "html={html}");
    }
}

#[test]
fn test_resolve_yahoo_url_and_internal_hosts() {
    let cpp = wrap(
        "https%3a%2f%2fen.wikipedia.org%2fwiki%2fC%252B%252B",
        "A2RRAQgvZuVqxwIAQnlXNyoA",
    );
    assert_eq!(
        resolve_yahoo_url(&cpp).as_deref(),
        Some("https://en.wikipedia.org/wiki/C%2B%2B")
    );
    // Spike N: rd.search.yahoo.com also unwraps cleanly.
    let rd = cpp.replace("r.search.yahoo.com", "rd.search.yahoo.com");
    assert_eq!(
        resolve_yahoo_url(&rd).as_deref(),
        Some("https://en.wikipedia.org/wiki/C%2B%2B")
    );
    assert_eq!(
        resolve_yahoo_url("https://example.com/x?a=1").as_deref(),
        Some("https://example.com/x?a=1")
    );
    assert_eq!(
        resolve_yahoo_url("https://r.search.yahoo.com/_ylt=abc/RV=2/RK=2"),
        None
    );
    assert_eq!(
        resolve_yahoo_url("https://r.search.yahoo.com/_ylt=abc/RU=javascript%3aalert(1)/RK=2"),
        None
    );

    assert!(is_dropped(
        "https://guce.yahoo.com/privacy-policy?locale=en-US"
    ));
    assert!(is_dropped(
        "https://images.search.yahoo.com/search/images?p=x"
    ));
    assert!(is_dropped("https://scout.yahoo.com/chat?q=x"));
    assert!(is_dropped("https://www.bing.com/aclick?ld=e8x&u=abc"));
    assert!(!is_dropped(
        "https://finance.yahoo.com/personal-finance/credit-cards/article/best"
    ));
    assert!(!is_dropped("https://www.bing.com/search?q=x"));
}

fn params_of(url: &str) -> HashMap<String, String> {
    Url::parse(url)
        .unwrap()
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

fn mk_q(
    query: &str,
    freshness: Option<Freshness>,
    domains: Vec<String>,
    filetype: Option<&str>,
) -> SerpQuery {
    let mut cx = SrcCx::from_config(
        &Config::default(),
        CancellationToken::new(),
        Duration::from_secs(5),
    );
    cx.freshness = freshness;
    cx.domains = domains;
    let mut p = json!({"query": query, "limit": 10});
    if let Some(ft) = filetype {
        p["filetype"] = json!(ft);
    }
    SerpQuery::from_call("yahoo", "search", &p, &cx, &Config::default()).unwrap()
}

#[test]
fn test_build_url_plain_operators_and_freshness() {
    let url = build_url(&mk_q("人工智能 最新进展", None, vec![], None));
    assert!(url.starts_with("https://search.yahoo.com/search?"));
    let p = params_of(&url);
    assert_eq!(p.get("p").map(String::as_str), Some("人工智能 最新进展"));
    assert_eq!(p.get("ei").map(String::as_str), Some("UTF-8"));
    for ignored in ["btf", "age", "n", "vm"] {
        assert!(!p.contains_key(ignored));
    }

    let op_url = build_url(&mk_q(
        "asyncio taskgroup",
        None,
        vec!["docs.python.org".into(), "-geeksforgeeks.org".into()],
        Some("pdf"),
    ));
    assert_eq!(
        params_of(&op_url).get("p").map(String::as_str),
        Some("asyncio taskgroup site:docs.python.org -site:geeksforgeeks.org filetype:pdf")
    );

    for (f, btf) in [
        (Freshness::Day, "d"),
        (Freshness::Week, "w"),
        (Freshness::Month, "m"),
    ] {
        let u = build_url(&mk_q("python release", Some(f), vec![], None));
        assert_eq!(params_of(&u).get("btf").map(String::as_str), Some(btf));
    }
    let y_url = build_url(&mk_q("python release", Some(Freshness::Year), vec![], None));
    assert!(!params_of(&y_url).contains_key("btf"));
}

// ---------------------------------------------------------------------------
// `_bv` cookie jar & handshake tests
// ---------------------------------------------------------------------------

enum Step {
    Resp {
        status: u16,
        body: String,
        set_cookies: Vec<(String, String)>,
    },
    Err(fn() -> FsmError),
}

struct YahooWire {
    /// Answers for requests arriving WITHOUT `A3=` (handshake) vs WITH `A3=` (replayed jar).
    handshake_answers: Mutex<VecDeque<Step>>,
    search_answers: Mutex<VecDeque<Step>>,
    handshake_urls: Mutex<Vec<String>>,
    search_cookies: Mutex<Vec<String>>,
    handshake_delay: Duration,
}

impl YahooWire {
    fn new(handshakes: Vec<Step>, searches: Vec<Step>, handshake_delay: Duration) -> Self {
        Self {
            handshake_answers: Mutex::new(handshakes.into()),
            search_answers: Mutex::new(searches.into()),
            handshake_urls: Mutex::new(Vec::new()),
            search_cookies: Mutex::new(Vec::new()),
            handshake_delay,
        }
    }

    fn hs_count(&self) -> usize {
        self.handshake_urls.lock().unwrap().len()
    }

    fn replayed_cookies(&self) -> Vec<String> {
        self.search_cookies.lock().unwrap().clone()
    }
}

fn step_ok(status: u16, body: String) -> Step {
    Step::Resp {
        status,
        body,
        set_cookies: Vec::new(),
    }
}

#[async_trait]
impl NetClient for YahooWire {
    async fn send(
        &self,
        req: HttpRequest,
        _profile: TransportProfile,
        _cx: &NetCx,
    ) -> Result<HttpBytes, FsmError> {
        let cookie = req
            .headers
            .iter()
            .rfind(|(k, _)| k.eq_ignore_ascii_case("cookie"))
            .map(|(_, v)| v.clone());
        let is_replayed_jar = cookie.as_deref().is_some_and(|c| c.contains("A3="));

        if is_replayed_jar {
            self.search_cookies
                .lock()
                .unwrap()
                .push(cookie.unwrap_or_default());
            let step = self
                .search_answers
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected search request");
            return match step {
                Step::Resp {
                    status,
                    body,
                    set_cookies,
                } => Ok(HttpBytes::new(req.url, status, set_cookies, body)),
                Step::Err(mk) => Err(mk()),
            };
        }

        let seq = {
            let mut hs = self.handshake_urls.lock().unwrap();
            hs.push(req.url.clone());
            hs.len()
        };
        if !self.handshake_delay.is_zero() {
            tokio::time::sleep(self.handshake_delay).await;
        }
        let step = self
            .handshake_answers
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected handshake request");
        match step {
            Step::Resp {
                status,
                body,
                mut set_cookies,
            } => {
                if set_cookies.is_empty() {
                    set_cookies.push((
                        "set-cookie".into(),
                        format!("YBV=v0.{seq}; Domain=.yahoo.com; Path=/"),
                    ));
                    if status == 200 {
                        set_cookies.push((
                            "set-cookie".into(),
                            "A3=d=AQAB; Domain=.yahoo.com; Path=/".into(),
                        ));
                    }
                }
                Ok(HttpBytes::new(req.url, status, set_cookies, body))
            }
            Step::Err(mk) => Err(mk()),
        }
    }

    fn check_url_offline(&self, _url: &str) -> Result<(), FsmError> {
        Ok(())
    }
}

fn mk_yahoo(net: Arc<YahooWire>, clock_val: Arc<Mutex<f64>>) -> YahooEngine {
    let deps = SourceDeps::new(net, Arc::new(NullStore::new()), Arc::new(Config::default()));
    let clock: ClockFn = Arc::new(move || *clock_val.lock().unwrap());
    let egress: EgressFn = Arc::new(|_| String::new());
    YahooEngine::with_hooks(deps, clock, egress)
}

fn default_cx() -> SrcCx {
    SrcCx::from_config(
        &Config::default(),
        CancellationToken::new(),
        Duration::from_secs(5),
    )
}

#[tokio::test]
async fn test_handshake_mints_a_jar_that_later_searches_reuse() {
    let page = sample_page();
    let wire = Arc::new(YahooWire::new(
        vec![step_ok(200, page.clone())],
        vec![step_ok(200, page)],
        Duration::ZERO,
    ));
    let engine = mk_yahoo(wire.clone(), Arc::new(Mutex::new(1000.0)));

    let SrcOutput::Hits(first) = engine
        .call(
            "search",
            json!({"query": "python asyncio taskgroup"}),
            &default_cx(),
        )
        .await
        .unwrap()
    else {
        panic!("expected hits");
    };
    let SrcOutput::Hits(second) = engine
        .call(
            "search",
            json!({"query": "rust tokio runtime"}),
            &default_cx(),
        )
        .await
        .unwrap()
    else {
        panic!("expected hits");
    };

    assert_eq!(first.len(), 3);
    assert_eq!(second.len(), 3);
    assert_eq!(wire.hs_count(), 1);
    assert_eq!(wire.replayed_cookies(), vec!["YBV=v0.1; A3=d=AQAB"]);
}

#[tokio::test]
async fn test_handshake_keeps_trying_in_the_same_session() {
    let page = sample_page();
    let wire = Arc::new(YahooWire::new(
        vec![
            step_ok(500, String::new()),
            step_ok(500, String::new()),
            step_ok(200, page),
        ],
        vec![],
        Duration::ZERO,
    ));
    let engine = mk_yahoo(wire.clone(), Arc::new(Mutex::new(1000.0)));

    let SrcOutput::Hits(results) = engine
        .call(
            "search",
            json!({"query": "python asyncio taskgroup"}),
            &default_cx(),
        )
        .await
        .unwrap()
    else {
        panic!("expected hits");
    };
    assert_eq!(results.len(), 3);
    assert_eq!(wire.hs_count(), HANDSHAKE_ATTEMPTS);
    let jar = engine.cached_jar("").unwrap();
    assert_eq!(
        jar.get_for_host("search.yahoo.com", "YBV").as_deref(),
        Some("v0.3")
    );
    assert_eq!(
        jar.get_for_host("search.yahoo.com", "A3").as_deref(),
        Some("d=AQAB")
    );
}

#[tokio::test]
async fn test_a_failed_handshake_raises_and_leaves_no_jar() {
    let wire = Arc::new(YahooWire::new(
        vec![
            step_ok(500, String::new()),
            step_ok(500, String::new()),
            step_ok(500, String::new()),
        ],
        vec![],
        Duration::ZERO,
    ));
    let engine = mk_yahoo(wire.clone(), Arc::new(Mutex::new(1000.0)));

    let err = engine
        .call("search", json!({"query": "python asyncio"}), &default_cx())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("bot check"), "{err}");
    assert!(engine.cached_jar("").is_none());
    assert_eq!(wire.hs_count(), 3);
}

#[tokio::test]
async fn test_a_rejected_jar_is_replaced_within_the_same_search() {
    let page = sample_page();
    let wire = Arc::new(YahooWire::new(
        vec![step_ok(200, page.clone()), step_ok(200, page)],
        vec![step_ok(500, String::new())],
        Duration::ZERO,
    ));
    let engine = mk_yahoo(wire.clone(), Arc::new(Mutex::new(1000.0)));

    engine
        .call("search", json!({"query": "first query"}), &default_cx())
        .await
        .unwrap();
    let stale_ybv = engine
        .cached_jar("")
        .unwrap()
        .get_for_host("search.yahoo.com", "YBV")
        .unwrap();

    let SrcOutput::Hits(results) = engine
        .call("search", json!({"query": "second query"}), &default_cx())
        .await
        .unwrap()
    else {
        panic!("expected hits");
    };
    assert_eq!(results.len(), 3);
    assert_eq!(wire.hs_count(), 2);
    let fresh_ybv = engine
        .cached_jar("")
        .unwrap()
        .get_for_host("search.yahoo.com", "YBV")
        .unwrap();
    assert_ne!(stale_ybv, fresh_ybv);
}

#[tokio::test]
async fn test_a_rejected_jar_gets_fewer_handshake_attempts() {
    let page = sample_page();
    let wire = Arc::new(YahooWire::new(
        vec![
            step_ok(200, page),
            step_ok(500, String::new()),
            step_ok(500, String::new()),
            step_ok(500, String::new()),
        ],
        vec![step_ok(500, String::new())],
        Duration::ZERO,
    ));
    let engine = mk_yahoo(wire.clone(), Arc::new(Mutex::new(1000.0)));

    engine
        .call("search", json!({"query": "first query"}), &default_cx())
        .await
        .unwrap();
    let err = engine
        .call("search", json!({"query": "second query"}), &default_cx())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("bot check"), "{err}");
    assert_eq!(wire.hs_count(), 1 + REMINT_ATTEMPTS);
    assert!(engine.cached_jar("").is_none());
}

#[tokio::test]
async fn test_a_timeout_with_a_jar_is_not_retried_as_a_handshake() {
    let page = sample_page();
    let wire = Arc::new(YahooWire::new(
        vec![step_ok(200, page)],
        vec![Step::Err(|| FsmError::Timeout(5000))],
        Duration::ZERO,
    ));
    let engine = mk_yahoo(wire.clone(), Arc::new(Mutex::new(1000.0)));

    engine
        .call("search", json!({"query": "first query"}), &default_cx())
        .await
        .unwrap();
    let err = engine
        .call("search", json!({"query": "second query"}), &default_cx())
        .await
        .unwrap_err();
    assert!(matches!(err, FsmError::Timeout(_)));
    assert_eq!(wire.hs_count(), 1);
    assert!(engine.cached_jar("").is_some());
}

#[tokio::test]
async fn test_a_refusal_ends_the_handshake_at_once() {
    let page = sample_page();
    // 403 -> ends in 1 attempt with HTTP 403 error
    let wire_403 = Arc::new(YahooWire::new(
        vec![step_ok(403, String::new()), step_ok(200, page.clone())],
        vec![],
        Duration::ZERO,
    ));
    let e403 = mk_yahoo(wire_403.clone(), Arc::new(Mutex::new(1000.0)));
    let err = e403
        .call("search", json!({"query": "python"}), &default_cx())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("HTTP 403"), "{err}");
    assert_eq!(wire_403.hs_count(), 1);

    // 429 -> ends in 1 attempt with RateLimited
    let wire_429 = Arc::new(YahooWire::new(
        vec![step_ok(429, String::new()), step_ok(200, page)],
        vec![],
        Duration::ZERO,
    ));
    let e429 = mk_yahoo(wire_429.clone(), Arc::new(Mutex::new(1000.0)));
    let err = e429
        .call("search", json!({"query": "python"}), &default_cx())
        .await
        .unwrap_err();
    assert!(matches!(err, FsmError::RateLimited { .. }), "{err}");
    assert_eq!(wire_429.hs_count(), 1);
}

#[tokio::test]
async fn test_a_refusal_or_reset_keeps_the_jar_and_sends_nothing_more() {
    let page = sample_page();
    for failure in [
        step_ok(403, String::new()),
        step_ok(429, String::new()),
        Step::Err(|| FsmError::upstream("yahoo", None, "connection reset")),
    ] {
        let wire = Arc::new(YahooWire::new(
            vec![step_ok(200, page.clone())],
            vec![failure],
            Duration::ZERO,
        ));
        let engine = mk_yahoo(wire.clone(), Arc::new(Mutex::new(1000.0)));
        engine
            .call("search", json!({"query": "first"}), &default_cx())
            .await
            .unwrap();
        assert!(
            engine
                .call("search", json!({"query": "second"}), &default_cx())
                .await
                .is_err()
        );
        assert_eq!(wire.hs_count(), 1);
        assert!(engine.cached_jar("").is_some());
    }
}

#[tokio::test]
async fn test_concurrent_searches_share_one_handshake() {
    let page = sample_page();
    let wire = Arc::new(YahooWire::new(
        vec![step_ok(200, page.clone())],
        vec![
            step_ok(200, page.clone()),
            step_ok(200, page.clone()),
            step_ok(200, page),
        ],
        Duration::from_millis(20),
    ));
    let engine = Arc::new(mk_yahoo(wire.clone(), Arc::new(Mutex::new(1000.0))));

    let futs = (0..4).map(|i| {
        let e = Arc::clone(&engine);
        async move {
            match e
                .call(
                    "search",
                    json!({"query": format!("query {i}")}),
                    &default_cx(),
                )
                .await
                .unwrap()
            {
                SrcOutput::Hits(h) => h.len(),
                _ => 0,
            }
        }
    });
    let counts = futures::future::join_all(futs).await;
    assert_eq!(counts, vec![3, 3, 3, 3]);
    assert_eq!(wire.hs_count(), 1);
}

#[tokio::test]
async fn test_waiters_on_a_failed_handshake_send_nothing_of_their_own() {
    let wire = Arc::new(YahooWire::new(
        vec![
            step_ok(500, String::new()),
            step_ok(500, String::new()),
            step_ok(500, String::new()),
        ],
        vec![],
        Duration::from_millis(10),
    ));
    let engine = Arc::new(mk_yahoo(wire.clone(), Arc::new(Mutex::new(1000.0))));

    let futs = (0..3).map(|i| {
        let e = Arc::clone(&engine);
        async move {
            e.call(
                "search",
                json!({"query": format!("query {i}")}),
                &default_cx(),
            )
            .await
            .is_err()
        }
    });
    let errs = futures::future::join_all(futs).await;
    assert!(errs.into_iter().all(|e| e));
    assert_eq!(wire.hs_count(), HANDSHAKE_ATTEMPTS);
}

#[tokio::test]
async fn test_a_cancelled_handshake_hands_its_waiters_their_own() {
    let page = sample_page();
    let wire = Arc::new(YahooWire::new(
        vec![step_ok(200, page.clone()), step_ok(200, page)],
        vec![],
        Duration::from_millis(40),
    ));
    let engine = Arc::new(mk_yahoo(wire.clone(), Arc::new(Mutex::new(1000.0))));

    let e1 = Arc::clone(&engine);
    let owner = tokio::spawn(async move {
        e1.call("search", json!({"query": "first"}), &default_cx())
            .await
    });
    tokio::time::sleep(Duration::from_millis(5)).await;

    let e2 = Arc::clone(&engine);
    let waiter = tokio::spawn(async move {
        e2.call("search", json!({"query": "second"}), &default_cx())
            .await
    });
    tokio::time::sleep(Duration::from_millis(5)).await;

    owner.abort();
    let _ = owner.await;

    let SrcOutput::Hits(hits) = waiter.await.unwrap().unwrap() else {
        panic!("expected hits");
    };
    assert_eq!(hits.len(), 3);
    assert_eq!(wire.hs_count(), 2);
}

#[tokio::test]
async fn test_an_expired_jar_is_never_replayed() {
    let page = sample_page();
    let clock = Arc::new(Mutex::new(1000.0));
    let wire = Arc::new(YahooWire::new(
        vec![step_ok(200, page.clone()), step_ok(200, page)],
        vec![],
        Duration::ZERO,
    ));
    let engine = mk_yahoo(wire.clone(), clock.clone());

    engine
        .call("search", json!({"query": "first"}), &default_cx())
        .await
        .unwrap();
    *clock.lock().unwrap() += JAR_TTL_SECS + 1.0;
    engine
        .call("search", json!({"query": "second"}), &default_cx())
        .await
        .unwrap();

    assert_eq!(wire.hs_count(), 2);
    assert!(wire.replayed_cookies().is_empty());
}
