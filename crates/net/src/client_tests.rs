//! End-to-end tests of [`HttpClient`] against local raw-HTTP servers.
//! The SSRF guard stays ON: only each test's own server address is exempted
//! (`cfg(test)`), so redirects to any other local address are still refused.

use super::*;
use crate::dns::StaticDns;
use crate::testutil::{Reply, Seen, Server, ok, redirect, serve, serve_socks5};
use fsm_core::SsrfResolveMode;
use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};

const HELLO: &str = "hello, compressed world\n";

fn base_cfg() -> Config {
    let mut c = Config::default();
    c.proxy = None;
    c.proxy_engines.clear();
    c.no_proxy.clear();
    c.contact_email = None;
    c.user_agent = String::new();
    c.accept_language = "en-US,en;q=0.9".into();
    c.allow_private_hosts = false;
    c.ssrf_resolve_addresses = SsrfResolveMode::Auto;
    c.max_response_bytes = 25_000_000;
    c.request_timeout_s = 10.0;
    c
}

fn client_with(cfg: &Config, dns: Arc<dyn DnsLookup>) -> HttpClient {
    HttpClient::builder(cfg).dns(dns).build().unwrap()
}

fn client() -> HttpClient {
    client_with(&base_cfg(), Arc::new(StaticDns::new()))
}

fn allow(c: &HttpClient, s: &Server) {
    c.guard().exempt.lock().unwrap().push(s.addr);
}

fn cx() -> NetCx {
    NetCx::new(CancellationToken::new(), Duration::from_secs(10))
}

async fn get(c: &HttpClient, url: &str, p: TransportProfile) -> Result<HttpBytes, FsmError> {
    c.send(HttpRequest::get(url), p, &cx()).await
}

fn text(b: &HttpBytes) -> String {
    String::from_utf8_lossy(&b.body).into_owned()
}

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

/// Replies with the request's Cookie header (or `<none>`).
fn cookie_echo(s: &Seen) -> Reply {
    match s.target.as_str() {
        "/set" => Reply::Full(
            302,
            vec![
                ("Location".into(), "/check".into()),
                ("Set-Cookie".into(), "sid=abc; Path=/".into()),
            ],
            Vec::new(),
        ),
        _ => ok(s.header("cookie").unwrap_or("<none>")),
    }
}

#[tokio::test]
async fn plain_get_sends_honest_ua_and_appends_the_query() {
    let s = serve(|_| ok("hi")).await;
    let c = client();
    allow(&c, &s);
    let req = HttpRequest::get(s.url("/path?x=1"))
        .query_param("q", "a b")
        .query_param("lang", "zh");
    let r = c.send(req, TransportProfile::Plain, &cx()).await.unwrap();
    assert_eq!((r.status, text(&r).as_str()), (200, "hi"));
    let seen = &s.requests()[0];
    assert_eq!(seen.target, "/path?x=1&q=a+b&lang=zh");
    assert_eq!(seen.header("user-agent"), Some(PLAIN_USER_AGENT));
    assert_eq!(seen.header("cookie"), None);
    assert_eq!(r.final_url, s.url("/path?x=1&q=a+b&lang=zh"));
    assert_eq!(r.header("content-type"), Some("text/plain"));
}

#[tokio::test]
async fn contact_email_makes_the_plain_ua_contactable() {
    let s = serve(|_| ok("")).await;
    let mut cfg = base_cfg();
    cfg.contact_email = Some("ops@example.org".into());
    let c = client_with(&cfg, Arc::new(StaticDns::new()));
    allow(&c, &s);
    get(&c, &s.url("/"), TransportProfile::Plain).await.unwrap();
    assert_eq!(
        s.requests()[0].header("user-agent"),
        Some("free-search-mcp/1.0 (mailto:ops@example.org)")
    );
}

#[tokio::test]
async fn a_blocked_literal_never_connects() {
    let s = serve(|_| ok("secret")).await;
    let c = client(); // not exempted
    for p in [TransportProfile::Plain, TransportProfile::Chrome] {
        let e = get(&c, &s.url("/"), p).await.unwrap_err();
        assert!(matches!(e, FsmError::UnsafeUrl(_)), "{e}");
    }
    assert_eq!(s.connections(), 0);
    assert!(c.check_url_offline(&s.url("/")).is_err());
}

#[tokio::test]
async fn a_redirect_into_a_blocked_target_is_refused_before_connecting() {
    let inner = serve(|_| ok("internal")).await;
    let to = inner.url("/secret");
    let front = serve(move |_| redirect(302, &to)).await;
    let c = client();
    allow(&c, &front);
    for p in [TransportProfile::Plain, TransportProfile::Chrome] {
        let e = get(&c, &front.url("/"), p).await.unwrap_err();
        assert!(matches!(e, FsmError::UnsafeUrl(_)), "{e}");
    }
    assert_eq!(
        inner.connections(),
        0,
        "the internal server saw no connection"
    );
}

#[tokio::test]
async fn redirects_to_internal_names_escaped_hosts_and_private_dns_are_refused() {
    let inner = serve(|_| ok("internal")).await;
    let port = inner.addr.port();
    let targets = [
        "http://metadata.google.internal/computeMetadata/v1/".to_owned(),
        format!("http://%31%32%37.0.0.1:{port}/"),
        format!("http://rebind.test:{port}/"),
        "file:///etc/passwd".to_owned(),
    ];
    let dns = Arc::new(StaticDns::new().with("rebind.test", &[ip("10.0.0.1")]));
    for to in targets {
        let t = to.clone();
        let front = serve(move |_| redirect(301, &t)).await;
        let c = client_with(&base_cfg(), dns.clone());
        allow(&c, &front);
        let e = get(&c, &front.url("/"), TransportProfile::Plain)
            .await
            .unwrap_err();
        assert!(matches!(e, FsmError::UnsafeUrl(_)), "{to}: {e}");
    }
    assert_eq!(inner.connections(), 0);
}

#[tokio::test]
async fn follows_at_most_five_redirects() {
    let hop = |limit: usize| {
        move |s: &Seen| {
            let n: usize = s.target.trim_start_matches("/hop/").parse().unwrap_or(0);
            if n < limit {
                redirect(302, &format!("/hop/{}", n + 1))
            } else {
                ok(&format!("landed {n}"))
            }
        }
    };
    let five = serve(hop(5)).await;
    let six = serve(hop(6)).await;
    let c = client();
    allow(&c, &five);
    allow(&c, &six);
    for p in [TransportProfile::Plain, TransportProfile::Chrome] {
        let r = get(&c, &five.url("/hop/0"), p).await.unwrap();
        assert_eq!(text(&r), "landed 5");
        assert_eq!(r.final_url, five.url("/hop/5"));
        let e = get(&c, &six.url("/hop/0"), p).await.unwrap_err();
        assert!(e.to_string().contains("too many redirects"), "{e}");
    }
}

#[tokio::test]
async fn follow_redirects_false_returns_the_3xx_and_no_location_is_an_error() {
    let s = serve(|s| {
        if s.target == "/bare" {
            Reply::Full(302, vec![], vec![])
        } else {
            redirect(307, "/next")
        }
    })
    .await;
    let c = client();
    allow(&c, &s);
    let mut req = HttpRequest::get(s.url("/"));
    req.follow_redirects = false;
    let r = c.send(req, TransportProfile::Chrome, &cx()).await.unwrap();
    assert_eq!((r.status, r.header("location")), (307, Some("/next")));
    assert_eq!(s.requests().len(), 1);
    let e = get(&c, &s.url("/bare"), TransportProfile::Plain)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("without a Location"), "{e}");
}

#[tokio::test]
async fn a_cross_origin_redirect_drops_credentials() {
    let other = serve(|_| ok("other")).await;
    let to = other.url("/landing");
    let front = serve(move |s| {
        if s.target == "/same" {
            redirect(302, "/landing")
        } else if s.target == "/landing" {
            ok("same")
        } else {
            redirect(302, &to)
        }
    })
    .await;
    let c = client();
    allow(&c, &front);
    allow(&c, &other);
    let creds = |url: String| {
        HttpRequest::get(url)
            .header("Authorization", "Bearer A")
            .header("Cookie", "a=b")
            .header("X-Api-Key", "K")
            .header("X-Secret", "S")
            .header("X-Keep", "1")
            .redact_header("x-secret")
    };
    for p in [TransportProfile::Plain, TransportProfile::Chrome] {
        let r = c.send(creds(front.url("/cross")), p, &cx()).await.unwrap();
        assert_eq!(text(&r), "other");
        let seen = other.requests().last().cloned().unwrap();
        for h in ["authorization", "cookie", "x-api-key", "x-secret"] {
            assert_eq!(seen.header(h), None, "{h} crossed origins ({p:?})");
        }
        assert_eq!(seen.header("x-keep"), Some("1"));
        // Same origin keeps them.
        let r = c.send(creds(front.url("/same")), p, &cx()).await.unwrap();
        assert_eq!(text(&r), "same");
        let seen = front.requests().last().cloned().unwrap();
        assert_eq!(seen.header("authorization"), Some("Bearer A"));
        assert_eq!(seen.header("x-secret"), Some("S"));
    }
}

#[tokio::test]
async fn see_other_turns_a_post_into_a_get_and_307_keeps_it() {
    let s = serve(|s| match s.target.as_str() {
        "/303" => redirect(303, "/final"),
        "/307" => redirect(307, "/final"),
        _ => ok(&format!(
            "{} {} {}",
            s.method,
            String::from_utf8_lossy(&s.body),
            s.header("content-type").unwrap_or("-")
        )),
    })
    .await;
    let c = client();
    allow(&c, &s);
    let post = |path: &str| {
        HttpRequest::post(s.url(path), "{\"q\":1}").header("Content-Type", "application/json")
    };
    let r = c
        .send(post("/303"), TransportProfile::Plain, &cx())
        .await
        .unwrap();
    assert_eq!(text(&r), "GET  -");
    let r = c
        .send(post("/307"), TransportProfile::Plain, &cx())
        .await
        .unwrap();
    assert_eq!(text(&r), "POST {\"q\":1} application/json");
}

#[tokio::test]
async fn known_encodings_are_decoded_by_both_stacks() {
    let fixtures: [(&str, &[u8]); 4] = [
        ("gzip", include_bytes!("../tests/fixtures/hello.gz")),
        ("br", include_bytes!("../tests/fixtures/hello.br")),
        ("zstd", include_bytes!("../tests/fixtures/hello.zst")),
        ("deflate", include_bytes!("../tests/fixtures/hello.zlib")),
    ];
    for (enc, body) in fixtures {
        let s = serve(move |_| {
            Reply::Full(
                200,
                vec![("Content-Encoding".into(), enc.into())],
                body.to_vec(),
            )
        })
        .await;
        let c = client();
        allow(&c, &s);
        for p in [
            TransportProfile::Plain,
            TransportProfile::Chrome,
            TransportProfile::EdgePreflight,
        ] {
            let r = get(&c, &s.url("/"), p).await.unwrap();
            assert_eq!(text(&r), HELLO, "{enc} via {p:?}");
            assert!(r.header("content-encoding").is_none());
        }
    }
}

#[tokio::test]
async fn unknown_or_stacked_encodings_are_refused_before_reading() {
    for enc in ["compress", "gzip, br", "x-snappy"] {
        let s = serve(move |_| {
            Reply::Full(
                200,
                vec![("Content-Encoding".into(), enc.into())],
                b"\x1f\x8b junk".to_vec(),
            )
        })
        .await;
        let c = client();
        allow(&c, &s);
        for p in [TransportProfile::Plain, TransportProfile::Chrome] {
            let e = get(&c, &s.url("/"), p).await.unwrap_err();
            assert!(
                matches!(&e, FsmError::Upstream { status: Some(200), message, .. } if message.contains("unsupported Content-Encoding")),
                "{enc}: {e}"
            );
        }
    }
    let s = serve(|_| {
        Reply::Full(
            200,
            vec![("Content-Encoding".into(), "identity".into())],
            b"plain".to_vec(),
        )
    })
    .await;
    let c = client();
    allow(&c, &s);
    assert_eq!(
        text(&get(&c, &s.url("/"), TransportProfile::Plain).await.unwrap()),
        "plain"
    );
}

fn slow(length: bool, delay_ms: u64) -> Reply {
    Reply::Slow {
        delay: Duration::from_millis(delay_ms),
        chunk: 64 * 1024,
        chunks: 256, // 16 MiB at 10 ms a chunk: ~2.6 s in full
        every: Duration::from_millis(10),
        length,
    }
}

/// Python `test_a_stream_left_unread_stops_when_its_block_ends`: a declared
/// length over the cap is refused before the body is read, and the transfer stops.
#[tokio::test]
async fn test_a_stream_left_unread_stops_when_its_block_ends() {
    for p in [TransportProfile::Plain, TransportProfile::Chrome] {
        let s = serve(|_| slow(true, 0)).await;
        let c = client();
        allow(&c, &s);
        let started = Instant::now();
        let req = HttpRequest::get(s.url("/big")).with_max_bytes(256 * 1024);
        let e = c.send(req, p, &cx()).await.unwrap_err();
        assert!(matches!(e, FsmError::MaxBytesExceeded(262_144)), "{e}");
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(s.how_it_ended().await, Some("aborted"), "{p:?}");
    }
}

/// Python `test_a_stream_cancelled_before_its_headers_stops`.
#[tokio::test]
async fn test_a_stream_cancelled_before_its_headers_stops() {
    for p in [TransportProfile::Plain, TransportProfile::Chrome] {
        let s = serve(|_| slow(true, 300)).await;
        let c = client();
        allow(&c, &s);
        let token = CancellationToken::new();
        let ncx = NetCx::new(token.clone(), Duration::from_secs(30));
        let t2 = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            t2.cancel();
        });
        let e = c
            .send(HttpRequest::get(s.url("/big")), p, &ncx)
            .await
            .unwrap_err();
        assert!(matches!(e, FsmError::Timeout(0)), "{e}");
        assert_eq!(s.how_it_ended().await, Some("aborted"), "{p:?}");
        // Already cancelled: nothing is sent at all.
        let before = s.connections();
        let e = c
            .send(HttpRequest::get(s.url("/")), p, &ncx)
            .await
            .unwrap_err();
        assert!(matches!(e, FsmError::Timeout(0)));
        assert_eq!(s.connections(), before);
    }
}

/// Python `test_a_body_over_the_cap_stops_downloading` (no Content-Length):
/// the body is cut at the cap, flagged `truncated` (ReplayNet semantics),
/// and the rest is never downloaded.
#[tokio::test]
async fn test_a_body_over_the_cap_stops_downloading() {
    for p in [TransportProfile::Plain, TransportProfile::Chrome] {
        let s = serve(|_| slow(false, 0)).await;
        let c = client();
        allow(&c, &s);
        let started = Instant::now();
        let r = c
            .send(
                HttpRequest::get(s.url("/big?nolength")).with_max_bytes(256 * 1024),
                p,
                &cx(),
            )
            .await
            .unwrap();
        assert!(started.elapsed() < Duration::from_millis(1500));
        assert!(r.truncated);
        assert_eq!(r.body.len(), 256 * 1024);
        assert_eq!(s.how_it_ended().await, Some("aborted"), "{p:?}");
    }
}

#[tokio::test]
async fn a_decompression_bomb_is_cut_at_the_cap() {
    let bomb: &[u8] = include_bytes!("../tests/fixtures/bomb_10mib_zeros.gz");
    let s = serve(move |_| {
        Reply::Full(
            200,
            vec![("Content-Encoding".into(), "gzip".into())],
            bomb.to_vec(),
        )
    })
    .await;
    let c = client();
    allow(&c, &s);
    for p in [TransportProfile::Plain, TransportProfile::Chrome] {
        let r = c
            .send(
                HttpRequest::get(s.url("/")).with_max_bytes(256 * 1024),
                p,
                &cx(),
            )
            .await
            .unwrap();
        assert!(r.truncated, "{p:?}");
        assert_eq!(r.body.len(), 256 * 1024);
        assert!(r.body.iter().all(|b| *b == 0));
    }
    // The config cap applies when the request sets none; max_bytes overrides
    // it in both directions (review note 3). Since `bomb_10mib_zeros.gz` has a
    // declared Content-Length of 10,231 bytes, a 1,024-byte cap fails pre-read,
    // while a 16 KiB cap (> wire size, < 10 MiB inflated) truncates at 16 KiB.
    let mut cfg = base_cfg();
    cfg.max_response_bytes = 1024;
    let c = client_with(&cfg, Arc::new(StaticDns::new()));
    allow(&c, &s);
    assert!(matches!(
        get(&c, &s.url("/"), TransportProfile::Plain).await,
        Err(FsmError::MaxBytesExceeded(1024))
    ));
    cfg.max_response_bytes = 16 * 1024;
    let c = client_with(&cfg, Arc::new(StaticDns::new()));
    allow(&c, &s);
    let r = get(&c, &s.url("/"), TransportProfile::Plain).await.unwrap();
    assert!(r.truncated && r.body.len() == 16 * 1024);
    let r = c
        .send(
            HttpRequest::get(s.url("/")).with_max_bytes(20 << 20),
            TransportProfile::Plain,
            &cx(),
        )
        .await
        .unwrap();
    assert!(!r.truncated && r.body.len() == 10 << 20);
}

#[tokio::test]
async fn a_slow_server_times_out() {
    let s = serve(|_| slow(true, 1000)).await;
    let c = client();
    allow(&c, &s);
    let ncx = NetCx::new(CancellationToken::new(), Duration::from_millis(200));
    let started = Instant::now();
    let e = c
        .send(HttpRequest::get(s.url("/")), TransportProfile::Plain, &ncx)
        .await
        .unwrap_err();
    assert!(matches!(e, FsmError::Timeout(200)), "{e}");
    assert!(started.elapsed() < Duration::from_millis(600));
}

#[tokio::test]
async fn retry_after_benches_the_bucket() {
    let s = serve(|_| {
        Reply::Full(
            429,
            vec![("Retry-After".into(), "30".into())],
            b"slow down".to_vec(),
        )
    })
    .await;
    let c = client();
    allow(&c, &s);
    let mut ncx = cx();
    ncx.host_bucket = Some("wikimedia.org".into());
    let r = c
        .send(
            HttpRequest::get(s.url("/")).for_source("wiktionary"),
            TransportProfile::Plain,
            &ncx,
        )
        .await
        .unwrap();
    assert_eq!(r.status, 429, "a 429 is a response, as in ReplayNet");
    let e = c
        .send(
            HttpRequest::get(s.url("/again")).for_source("wikisource"),
            TransportProfile::Plain,
            &ncx,
        )
        .await
        .unwrap_err();
    match e {
        FsmError::RateLimited {
            source_id,
            retry_after_s: Some(left),
        } => {
            assert_eq!(source_id, "wikisource");
            assert!((28..=30).contains(&left), "{left}");
        }
        other => panic!("{other}"),
    }
    assert_eq!(s.requests().len(), 1, "the benched request was never sent");
    // Another bucket is unaffected.
    assert!(get(&c, &s.url("/"), TransportProfile::Plain).await.is_ok());
}

#[tokio::test]
async fn rps_waits_within_the_budget_and_refuses_beyond_it() {
    let s = serve(|_| ok("")).await;
    let c = client();
    allow(&c, &s);
    let mk = |ms: u64| {
        let mut n = NetCx::new(CancellationToken::new(), Duration::from_millis(ms));
        n.rps = Some(2.0);
        n.host_bucket = Some("bucket".into());
        n
    };
    // Burst = ceil(2.0) = 2.
    for _ in 0..2 {
        c.send(
            HttpRequest::get(s.url("/")),
            TransportProfile::Plain,
            &mk(2000),
        )
        .await
        .unwrap();
    }
    let e = c
        .send(
            HttpRequest::get(s.url("/")),
            TransportProfile::Plain,
            &mk(100),
        )
        .await
        .unwrap_err();
    assert!(matches!(e, FsmError::RateLimited { .. }), "{e}");
    let started = Instant::now();
    c.send(
        HttpRequest::get(s.url("/")),
        TransportProfile::Plain,
        &mk(3000),
    )
    .await
    .unwrap();
    assert!(
        started.elapsed() >= Duration::from_millis(200),
        "waited for the GCRA slot"
    );
    // Manifest quotas configured up front win over NetCx.rps.
    c.rate_limits().configure("tight", 0.2, 1);
    let mut n = mk(300);
    n.host_bucket = Some("tight".into());
    c.send(HttpRequest::get(s.url("/")), TransportProfile::Plain, &n)
        .await
        .unwrap();
    assert!(
        c.send(HttpRequest::get(s.url("/")), TransportProfile::Plain, &n)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn chrome_cookie_jars_carry_cookies_plain_stays_cookie_less() {
    let s = serve(cookie_echo).await;
    let c = client();
    allow(&c, &s);
    let with_jar = |path: &str, jar: Option<&str>| {
        let mut r = HttpRequest::get(s.url(path));
        r.cookie_jar_id = jar.map(str::to_owned);
        r
    };
    // Named jar: the redirect hop and later requests see the cookie.
    let r = c
        .send(
            with_jar("/set", Some("bing")),
            TransportProfile::Chrome,
            &cx(),
        )
        .await
        .unwrap();
    assert_eq!(text(&r), "sid=abc");
    let r = c
        .send(
            with_jar("/echo", Some("bing")),
            TransportProfile::Chrome,
            &cx(),
        )
        .await
        .unwrap();
    assert_eq!(text(&r), "sid=abc");
    // Per egress: the Edge stack has its own jar.
    let r = c
        .send(
            with_jar("/echo", Some("bing")),
            TransportProfile::EdgePreflight,
            &cx(),
        )
        .await
        .unwrap();
    assert_eq!(text(&r), "<none>");
    // Caller cookies are merged in front of the jar's.
    let r = c
        .send(
            with_jar("/echo", Some("bing")).header("Cookie", "pref=1"),
            TransportProfile::Chrome,
            &cx(),
        )
        .await
        .unwrap();
    assert_eq!(text(&r), "pref=1; sid=abc");
    c.clear_cookie_jar("bing");
    let r = c
        .send(
            with_jar("/echo", Some("bing")),
            TransportProfile::Chrome,
            &cx(),
        )
        .await
        .unwrap();
    assert_eq!(text(&r), "<none>");
    // No jar id: one request's redirect chain still carries its cookies (curl
    // allow_redirects semantics), the next request starts empty.
    let r = c
        .send(with_jar("/set", None), TransportProfile::Chrome, &cx())
        .await
        .unwrap();
    assert_eq!(text(&r), "sid=abc");
    let r = c
        .send(with_jar("/echo", None), TransportProfile::Chrome, &cx())
        .await
        .unwrap();
    assert_eq!(text(&r), "<none>");
    // Tier 0 never keeps cookies, even with a jar id.
    let r = c
        .send(
            with_jar("/set", Some("bing")),
            TransportProfile::Plain,
            &cx(),
        )
        .await
        .unwrap();
    assert_eq!(text(&r), "<none>");
}

/// The Chrome profile's request head, byte for byte (HTTP/1.1; the HTTP/2
/// pseudo-header and frame order is not observable without TLS here).
#[tokio::test]
async fn chrome_header_snapshot() {
    let s = serve(|_| ok("")).await;
    let c = client();
    allow(&c, &s);
    get(&c, &s.url("/snap"), TransportProfile::Chrome)
        .await
        .unwrap();
    let seen = &s.requests()[0];
    let host = s.addr.to_string();
    let got: Vec<(String, String)> = seen.headers.clone();
    let want: Vec<(String, String)> = CHROME_SNAPSHOT
        .iter()
        .map(|(k, v)| {
            (
                k.to_string(),
                if *k == "host" {
                    host.clone()
                } else {
                    v.to_string()
                },
            )
        })
        .collect();
    assert_eq!(got, want, "Chrome 149 header set or order changed");
    assert_eq!(
        seen.header("accept-encoding"),
        Some("gzip, deflate, br, zstd")
    );
}

/// wreq-util 0.2.1 Chrome 149 (macOS) over HTTP/1.1, with the client's
/// default `Accept-Language`. Matches curl_cffi `chrome131`'s order for the
/// tail (`sec-fetch-dest | accept-encoding | accept-language | priority`,
/// Spike N §3); wreq writes `host` last and no `connection` on HTTP/1.1.
const CHROME_SNAPSHOT: &[(&str, &str)] = &[
    (
        "sec-ch-ua",
        "\"Google Chrome\";v=\"149\", \"Chromium\";v=\"149\", \"Not)A;Brand\";v=\"24\"",
    ),
    ("sec-ch-ua-mobile", "?0"),
    ("sec-ch-ua-platform", "\"macOS\""),
    ("upgrade-insecure-requests", "1"),
    (
        "user-agent",
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36",
    ),
    (
        "accept",
        "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7",
    ),
    ("sec-fetch-site", "none"),
    ("sec-fetch-mode", "navigate"),
    ("sec-fetch-user", "?1"),
    ("sec-fetch-dest", "document"),
    ("accept-encoding", "gzip, deflate, br, zstd"),
    ("accept-language", "en-US,en;q=0.9"),
    ("priority", "u=0, i"),
    ("host", ""),
];

#[tokio::test]
async fn chrome_caller_headers_and_config_defaults_merge() {
    let s = serve(|_| ok("")).await;
    let mut cfg = base_cfg();
    cfg.user_agent = "Mozilla/5.0 (custom)".into();
    cfg.accept_language = "zh-CN,zh;q=0.9".into();
    let c = client_with(&cfg, Arc::new(StaticDns::new()));
    allow(&c, &s);
    get(&c, &s.url("/a"), TransportProfile::Chrome)
        .await
        .unwrap();
    let req = HttpRequest::get(s.url("/b"))
        .header("Accept-Language", "ja")
        .header("Referer", "https://www.bing.com/");
    c.send(req, TransportProfile::Chrome, &cx()).await.unwrap();
    let seen = s.requests();
    assert_eq!(seen[0].header("user-agent"), Some("Mozilla/5.0 (custom)"));
    assert_eq!(seen[0].header("accept-language"), Some("zh-CN,zh;q=0.9"));
    assert_eq!(seen[1].header("accept-language"), Some("ja"));
    assert_eq!(seen[1].header("referer"), Some("https://www.bing.com/"));
    assert_eq!(
        seen[1]
            .headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("accept-language"))
            .count(),
        1
    );
    assert_eq!(
        seen[1].header("accept-encoding"),
        Some("gzip, deflate, br, zstd")
    );
}

#[tokio::test]
async fn edge_preflight_speaks_edge() {
    let s = serve(|_| ok("")).await;
    let c = client();
    allow(&c, &s);
    get(&c, &s.url("/"), TransportProfile::EdgePreflight)
        .await
        .unwrap();
    let ua = s.requests()[0]
        .header("user-agent")
        .unwrap_or("")
        .to_owned();
    assert!(ua.contains("Edg/148"), "{ua}");
}

#[tokio::test]
async fn proxy_routing_scoping_and_layer_four() {
    let proxy = serve(|s| ok(&format!("via-proxy {}", s.target))).await;
    let mut cfg = base_cfg();
    cfg.proxy = Some(format!("http://{}", proxy.addr));
    let dns = Arc::new(StaticDns::new());
    let c = client_with(&cfg, dns.clone());
    // auto + proxied: layer 4 stands down; the proxy (not this process) resolves.
    for p in [TransportProfile::Plain, TransportProfile::Chrome] {
        let r = get(&c, "http://target.test/x?q=1", p).await.unwrap();
        assert_eq!(text(&r), "via-proxy http://target.test/x?q=1", "{p:?}");
    }
    assert_eq!(dns.calls(), 0);
    // Layers 1–3 still hold through the proxy.
    for url in [
        "http://127.0.0.1/",
        "http://metadata.google.internal/",
        "http://localhost:8080/",
    ] {
        assert!(
            matches!(
                get(&c, url, TransportProfile::Plain).await,
                Err(FsmError::UnsafeUrl(_))
            ),
            "{url}"
        );
    }
    let through = proxy.requests().len();

    // PROXY_ENGINES scoping: out-of-scope sources go direct and get layer 4.
    let mut scoped = cfg.clone();
    scoped.proxy_engines = vec!["bing".into()];
    let c = client_with(&scoped, dns.clone());
    let e = c
        .send(
            HttpRequest::get("http://target.test/").for_source("duckduckgo"),
            TransportProfile::Plain,
            &cx(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(e, FsmError::UnsafeUrl(ref m) if m.contains("Could not resolve")),
        "{e}"
    );
    assert!(
        c.send(
            HttpRequest::get("http://target.test/").for_source("bing"),
            TransportProfile::Plain,
            &cx()
        )
        .await
        .is_ok()
    );
    assert!(
        c.send(
            HttpRequest::get("http://target.test/"),
            TransportProfile::Plain,
            &cx()
        )
        .await
        .is_ok(),
        "unscoped callers use the proxy"
    );

    // NO_PROXY suffixes go direct (and get layer 4).
    let mut np = cfg.clone();
    np.no_proxy = vec![".test".into()];
    let c = client_with(&np, dns.clone());
    assert!(matches!(
        get(&c, "http://target.test/", TransportProfile::Chrome).await,
        Err(FsmError::UnsafeUrl(_))
    ));

    // always: layer 4 runs even through the proxy.
    let mut always = cfg.clone();
    always.ssrf_resolve_addresses = SsrfResolveMode::Always;
    let c = client_with(&always, dns.clone());
    assert!(matches!(
        get(&c, "http://target.test/", TransportProfile::Plain).await,
        Err(FsmError::UnsafeUrl(_))
    ));

    assert_eq!(proxy.requests().len(), through + 2);
}

/// Resolution goes through our resolver and the connection is pinned to the
/// vetted answer: a resolver that rebinds after the first answer is never
/// consulted again while the vetted entry lives.
#[tokio::test]
async fn connections_are_pinned_to_the_vetted_addresses() {
    struct Rebinder(AtomicUsize);
    #[async_trait::async_trait]
    impl DnsLookup for Rebinder {
        async fn lookup(&self, _host: &str) -> std::io::Result<Vec<IpAddr>> {
            let n = self.0.fetch_add(1, Ordering::SeqCst);
            Ok(vec![if n == 0 {
                ip("127.0.0.1")
            } else {
                ip("10.0.0.1")
            }])
        }
    }
    let s = serve(|s| ok(s.header("host").unwrap_or(""))).await;
    let dns = Arc::new(Rebinder(AtomicUsize::new(0)));
    let c = client_with(&base_cfg(), dns.clone());
    allow(&c, &s);
    let url = format!("http://pinned.test:{}/", s.addr.port());
    let jobs: Vec<_> = (0..8)
        .map(|i| {
            let (c, url) = (c.clone(), url.clone());
            let p = if i % 2 == 0 {
                TransportProfile::Plain
            } else {
                TransportProfile::Chrome
            };
            tokio::spawn(async move { get(&c, &url, p).await })
        })
        .collect();
    for j in jobs {
        let r = j.await.unwrap().unwrap();
        assert_eq!(text(&r), format!("pinned.test:{}", s.addr.port()));
    }
    assert_eq!(
        dns.0.load(Ordering::SeqCst),
        1,
        "one vetted lookup served every connection"
    );
}

#[tokio::test]
async fn error_messages_never_carry_credentials() {
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let c = client();
    c.guard().exempt.lock().unwrap().push(closed);
    let req = HttpRequest::get(format!("http://{closed}/q"))
        .query_param("api_key", "SEKRET-123")
        .header("X-Token", "TOKVAL-456")
        .redact_query_key("api_key")
        .redact_header("x-token");
    for p in [TransportProfile::Plain, TransportProfile::Chrome] {
        let e = c.send(req.clone(), p, &cx()).await.unwrap_err();
        let msg = e.to_string();
        assert!(matches!(e, FsmError::Upstream { .. }), "{msg}");
        assert!(!msg.contains("SEKRET") && !msg.contains("TOKVAL"), "{msg}");
    }
    let mut cfg = base_cfg();
    cfg.proxy = Some(format!("http://user:hunter2@{closed}"));
    let c = client_with(&cfg, Arc::new(StaticDns::new()));
    for p in [TransportProfile::Plain, TransportProfile::Chrome] {
        let e = get(&c, "http://target.test/", p).await.unwrap_err();
        assert!(!e.to_string().contains("hunter2"), "{e}");
        assert!(!format!("{c:?}").contains("hunter2"));
    }
}

#[tokio::test]
async fn final_url_drops_redacted_query_keys() {
    let s = serve(|_| ok("")).await;
    let c = client();
    allow(&c, &s);
    let req = HttpRequest::get(s.url("/api"))
        .query_param("q", "x")
        .query_param("key", "SECRET")
        .redact_query_key("KEY");
    let r = c.send(req, TransportProfile::Plain, &cx()).await.unwrap();
    assert_eq!(r.final_url, s.url("/api?q=x"));
    assert_eq!(
        s.requests()[0].target,
        "/api?q=x&key=SECRET",
        "the key is still sent"
    );
}

#[tokio::test]
async fn head_reads_no_body_and_local_is_not_a_network_transport() {
    let s = serve(|_| ok("0123456789")).await;
    let c = client();
    allow(&c, &s);
    let r = c
        .send(
            HttpRequest::head(s.url("/")),
            TransportProfile::Plain,
            &cx(),
        )
        .await
        .unwrap();
    assert_eq!((r.status, r.body.len()), (200, 0));
    assert_eq!(s.requests()[0].method, "HEAD");
    let e = get(&c, &s.url("/"), TransportProfile::Local)
        .await
        .unwrap_err();
    assert!(matches!(e, FsmError::InvalidInput(_)), "{e}");
    assert_eq!(
        c.prewarm(&[(s.url("/warm").as_str(), TransportProfile::Plain)])
            .await,
        1
    );
}

#[test]
fn url_and_uri_agree_on_the_authority() {
    for u in [
        "https://example.com/a?b=c",
        "http://[::1]:8080/",
        "https://xn--fsq.example/",
        "http://93.184.216.34/",
        "https://example.com:8443/x",
    ] {
        let url = Url::parse(u).unwrap();
        assert!(same_authority(&url).is_ok(), "{u}");
    }
    // IDN is converted before either parser sees it.
    let idn = Url::parse("https://例え.jp/").unwrap();
    assert!(same_authority(&idn).is_ok() && idn.host_str() == Some("xn--r8jz45g.jp"));
}

/// Python `test_proxy_reaches_session_ctor_when_configured`.
#[tokio::test]
async fn test_proxy_reaches_session_ctor_when_configured() {
    let proxy = serve(|_| ok("proxied")).await;
    let mut cfg = base_cfg();
    cfg.proxy = Some(format!("http://{}", proxy.addr));
    let c = client_with(&cfg, Arc::new(StaticDns::new()));
    let r = c
        .send(
            HttpRequest::get("http://anysearch.test/search").for_source("anysearch"),
            TransportProfile::Plain,
            &cx(),
        )
        .await
        .unwrap();
    assert_eq!(text(&r), "proxied");
    assert_eq!(proxy.requests()[0].target, "http://anysearch.test/search");
}

/// Python `test_no_proxy_key_when_unconfigured`: no proxy, no detour.
#[tokio::test]
async fn test_no_proxy_key_when_unconfigured() {
    let target = serve(|_| ok("direct")).await;
    let c = client();
    allow(&c, &target);
    // NetCx::default() has no timeout: the configured request timeout applies.
    let r = c
        .send(
            HttpRequest::get(target.url("/")),
            TransportProfile::Chrome,
            &NetCx::default(),
        )
        .await
        .unwrap();
    assert_eq!(text(&r), "direct");
    assert_eq!(
        target.requests()[0].target,
        "/",
        "origin-form: not a proxy request"
    );
    assert!(format!("{c:?}").contains("proxy: None"));
}

#[tokio::test]
async fn no_content_responses_skip_decoding() {
    let s = serve(|s| {
        let status = if s.target == "/204" { 204 } else { 304 };
        Reply::Full(
            status,
            vec![
                ("Content-Encoding".into(), "br, gzip".into()),
                ("Content-Length".into(), "0".into()),
            ],
            vec![],
        )
    })
    .await;
    let c = client();
    allow(&c, &s);
    for path in ["/204", "/304"] {
        let r = get(&c, &s.url(path), TransportProfile::Plain)
            .await
            .unwrap();
        assert!(r.body.is_empty() && !r.truncated, "{path}");
    }
}

/// `socks5h://` (the documented proxy form) is wired on both stacks: the
/// request goes to the proxy (here a closed port), and layer 4 stands down.
#[tokio::test]
async fn a_socks5h_proxy_is_used_by_both_stacks() {
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let mut cfg = base_cfg();
    cfg.proxy = Some(format!("socks5h://{closed}"));
    let dns = Arc::new(StaticDns::new());
    let c = client_with(&cfg, dns.clone());
    for p in [TransportProfile::Plain, TransportProfile::Chrome] {
        let e = get(&c, "http://target.test/", p).await.unwrap_err();
        assert!(
            matches!(e, FsmError::Upstream { status: None, .. }),
            "{p:?}: {e}"
        );
    }
    assert_eq!(dns.calls(), 0, "the proxy resolves, not this process");
}

#[tokio::test]
async fn a_caller_user_agent_wins_on_every_stack() {
    let s = serve(|_| ok("")).await;
    let c = client();
    allow(&c, &s);
    for p in [TransportProfile::Plain, TransportProfile::Chrome] {
        let req = HttpRequest::get(s.url("/")).header("User-Agent", "Mozilla/5.0");
        c.send(req, p, &cx()).await.unwrap();
        let seen = s.requests().last().cloned().unwrap();
        assert_eq!(seen.header("user-agent"), Some("Mozilla/5.0"), "{p:?}");
        let n = seen
            .headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("user-agent"))
            .count();
        assert_eq!(n, 1, "{p:?}");
    }
}

/// B2 regression (Missing test #2): `socks5://` / `socks4://` proxies resolve
/// targets locally, so Layer 4 MUST run in `Auto` mode and connect-time lookups
/// MUST be pinned to the vetted address set (while allowing the proxy's own host).
#[tokio::test]
async fn socks5_local_dns_enforces_l4_in_auto_and_pins_in_always() {
    // (a) Auto mode + socks5:// -> private DNS resolution is blocked before connecting.
    let socks = serve_socks5().await;
    let mut cfg = base_cfg();
    cfg.proxy = Some(format!("socks5://{}", socks.addr));
    let dns = Arc::new(
        StaticDns::new()
            .with("evil.example", &[ip("169.254.169.254")])
            .with("loopback.example", &[ip("127.0.0.1")]),
    );
    let c = client_with(&cfg, dns.clone());
    for p in [TransportProfile::Plain, TransportProfile::Chrome] {
        for url in ["http://evil.example/", "http://loopback.example/"] {
            let e = get(&c, url, p).await.unwrap_err();
            assert!(matches!(e, FsmError::UnsafeUrl(_)), "{p:?} {url}: {e}");
        }
    }
    assert_eq!(
        socks.connections(),
        0,
        "socks5 proxy must never be contacted for SSRF-blocked target"
    );

    // Also socks4:// in Auto mode blocks private DNS before connecting.
    let mut cfg4 = base_cfg();
    cfg4.proxy = Some(format!("socks4://{}", socks.addr));
    let c4 = client_with(&cfg4, dns.clone());
    for p in [TransportProfile::Plain, TransportProfile::Chrome] {
        let e = get(&c4, "http://evil.example/", p).await.unwrap_err();
        assert!(matches!(e, FsmError::UnsafeUrl(_)), "socks4 {p:?}: {e}");
    }
    assert_eq!(socks.connections(), 0);

    // (b) Always mode + socks5:// with a rebinding resolver: must pin to lookup #1
    // (1 DNS call total for the target, and proxy's own hostname resolves via skip_host).
    struct RebindAndProxy {
        target_calls: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl DnsLookup for RebindAndProxy {
        async fn lookup(&self, host: &str) -> std::io::Result<Vec<IpAddr>> {
            if host.eq_ignore_ascii_case("my-socks-proxy.test") {
                return Ok(vec![ip("127.0.0.1")]);
            }
            let n = self.target_calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![if n == 0 {
                ip("127.0.0.1")
            } else {
                ip("10.0.0.1")
            }])
        }
    }
    let target_srv = serve(|_| ok("via-socks5-pinned")).await;
    let socks2 = serve_socks5().await;
    let mut always = base_cfg();
    always.ssrf_resolve_addresses = SsrfResolveMode::Always;
    always.proxy = Some(format!(
        "socks5://my-socks-proxy.test:{}",
        socks2.addr.port()
    ));
    let rebind_dns = Arc::new(RebindAndProxy {
        target_calls: AtomicUsize::new(0),
    });
    let c = client_with(&always, rebind_dns.clone());
    allow(&c, &target_srv);
    let url = format!("http://rebind.example:{}/", target_srv.addr.port());
    for p in [TransportProfile::Plain, TransportProfile::Chrome] {
        let r = get(&c, &url, p)
            .await
            .unwrap_or_else(|e| panic!("{p:?}: {e}"));
        assert_eq!(text(&r), "via-socks5-pinned", "{p:?}");
    }
    assert_eq!(
        rebind_dns.target_calls.load(Ordering::SeqCst),
        1,
        "target must be resolved once and pinned across both stacks"
    );
    assert!(
        socks2
            .targets()
            .iter()
            .all(|(atyp, host, _)| *atyp == 0x01 && host == "127.0.0.1"),
        "socks5 received locally resolved pinned IPv4: {:?}",
        socks2.targets()
    );
}

/// B3 regression (Missing test #3): relative redirects containing `//` + `%` in
/// path/query MUST succeed, while backslash authority redirects MUST fail.
#[tokio::test]
async fn relative_redirect_with_double_slash_and_percent_succeeds_while_backslash_fails() {
    let s = serve(|s| match s.target.as_str() {
        "/redir-q" => redirect(302, "/login?next=https://example.com&msg=hello%20world"),
        "/redir-p" => redirect(302, "/a//b%20c"),
        "/redir-bs" => redirect(302, "/\\%65xample.com/"),
        t => ok(t),
    })
    .await;
    let c = client();
    allow(&c, &s);
    for p in [TransportProfile::Plain, TransportProfile::Chrome] {
        let r1 = get(&c, &s.url("/redir-q"), p).await.unwrap();
        assert_eq!(
            text(&r1),
            "/login?next=https://example.com&msg=hello%20world",
            "{p:?}"
        );
        let r2 = get(&c, &s.url("/redir-p"), p).await.unwrap();
        assert_eq!(text(&r2), "/a//b%20c", "{p:?}");
        let e = get(&c, &s.url("/redir-bs"), p).await.unwrap_err();
        assert!(matches!(e, FsmError::UnsafeUrl(_)), "{p:?}: {e}");
    }
}

/// B4 regression (Missing test #4): pre-read compressed Content-Length > cap,
/// separate stacked Content-Encoding headers, case-insensitive GZIP/x-gzip,
/// zero-output wire-byte cap, multi-member gzip, raw DEFLATE, and HEAD headers.
#[tokio::test]
async fn compressed_pre_read_checks_case_insensitivity_wire_cap_and_head() {
    // 1. Compressed Content-Length > cap fails pre-read and aborts slow server.
    for p in [TransportProfile::Plain, TransportProfile::Chrome] {
        let s = serve(|_| Reply::SlowCustom {
            headers: vec![
                ("Content-Encoding".into(), "gzip".into()),
                ("Content-Length".into(), "500000000".into()),
            ],
            prefix: vec![0x1f, 0x8b, 0x08, 0, 0, 0, 0, 0, 0, 0xff],
            fill: 0,
            chunk: 64 * 1024,
            chunks: 256,
            every: Duration::from_millis(10),
        })
        .await;
        let c = client();
        allow(&c, &s);
        let started = Instant::now();
        let req = HttpRequest::get(s.url("/big.gz")).with_max_bytes(256 * 1024);
        let e = c.send(req, p, &cx()).await.unwrap_err();
        assert!(
            matches!(e, FsmError::MaxBytesExceeded(262_144)),
            "{p:?}: {e}"
        );
        assert!(started.elapsed() < Duration::from_secs(1), "{p:?}");
        assert_eq!(s.how_it_ended().await, Some("aborted"), "{p:?}");
    }

    // 2. Two separate Content-Encoding headers fail pre-read with Upstream.
    for p in [TransportProfile::Plain, TransportProfile::Chrome] {
        let s = serve(|_| Reply::SlowCustom {
            headers: vec![
                ("Content-Encoding".into(), "gzip".into()),
                ("Content-Encoding".into(), "br".into()),
            ],
            prefix: include_bytes!("../tests/fixtures/hello.gz").to_vec(),
            fill: 0,
            chunk: 64 * 1024,
            chunks: 256,
            every: Duration::from_millis(10),
        })
        .await;
        let c = client();
        allow(&c, &s);
        let e = get(&c, &s.url("/stacked"), p).await.unwrap_err();
        assert!(
            matches!(&e, FsmError::Upstream { status: Some(200), message, .. } if message.contains("unsupported Content-Encoding")),
            "{p:?}: {e}"
        );
        assert_eq!(s.how_it_ended().await, Some("aborted"), "{p:?}");
    }

    // 3. Case-insensitive GZIP, Gzip, x-gzip, X-GZIP, DEFLATE decode on both stacks.
    let gz = include_bytes!("../tests/fixtures/hello.gz");
    let zl = include_bytes!("../tests/fixtures/hello.zlib");
    for (enc, body) in [
        ("GZIP", gz.as_slice()),
        ("Gzip", gz.as_slice()),
        ("x-gzip", gz.as_slice()),
        ("X-GZIP", gz.as_slice()),
        ("DEFLATE", zl.as_slice()),
    ] {
        let s = serve(move |_| {
            Reply::Full(
                200,
                vec![("Content-Encoding".into(), enc.into())],
                body.to_vec(),
            )
        })
        .await;
        let c = client();
        allow(&c, &s);
        for p in [TransportProfile::Plain, TransportProfile::Chrome] {
            let r = get(&c, &s.url("/"), p)
                .await
                .unwrap_or_else(|e| panic!("{enc} {p:?}: {e}"));
            assert_eq!(text(&r), HELLO, "{enc} {p:?}");
            assert!(r.header("content-encoding").is_none(), "{enc} {p:?}");
        }
    }

    // 4. Wire-byte cap: gzip FCOMMENT streaming > cap wire bytes with 0 decoded output aborts.
    for p in [TransportProfile::Plain, TransportProfile::Chrome] {
        let s = serve(|_| Reply::SlowCustom {
            headers: vec![("Content-Encoding".into(), "gzip".into())],
            // Gzip header with FCOMMENT (0x10); never sends NUL terminator -> 0 decoded bytes.
            prefix: vec![0x1f, 0x8b, 0x08, 0x10, 0, 0, 0, 0, 0, 0xff],
            fill: b'A',
            chunk: 16 * 1024,
            chunks: 512, // 8 MiB of comment bytes over ~5 s if not aborted
            every: Duration::from_millis(10),
        })
        .await;
        let c = client();
        allow(&c, &s);
        let started = Instant::now();
        let req = HttpRequest::get(s.url("/wire-bomb")).with_max_bytes(64 * 1024);
        let e = c.send(req, p, &cx()).await.unwrap_err();
        assert!(
            matches!(e, FsmError::MaxBytesExceeded(65_536)),
            "{p:?}: {e}"
        );
        assert!(started.elapsed() < Duration::from_secs(1), "{p:?}");
        assert_eq!(s.how_it_ended().await, Some("aborted"), "{p:?}");
    }

    // 5. Multi-member gzip, raw RFC 1951 DEFLATE, and HEAD preserving Content-Length.
    let mut multi_gz = Vec::with_capacity(gz.len() * 2);
    multi_gz.extend_from_slice(gz);
    multi_gz.extend_from_slice(gz);
    let raw_deflate = zl[2..zl.len() - 4].to_vec();
    let s = serve(move |req| match req.target.as_str() {
        "/multi" => Reply::Full(
            200,
            vec![("Content-Encoding".into(), "gzip".into())],
            multi_gz.clone(),
        ),
        "/raw-deflate" => Reply::Full(
            200,
            vec![("Content-Encoding".into(), "deflate".into())],
            raw_deflate.clone(),
        ),
        "/head-gz" => Reply::Full(
            200,
            vec![
                ("Content-Encoding".into(), "gzip".into()),
                ("Content-Length".into(), "12345".into()),
            ],
            Vec::new(),
        ),
        _ => ok(""),
    })
    .await;
    let c = client();
    allow(&c, &s);
    for p in [TransportProfile::Plain, TransportProfile::Chrome] {
        let r = get(&c, &s.url("/multi"), p).await.unwrap();
        assert_eq!(text(&r), format!("{HELLO}{HELLO}"), "multi-gzip {p:?}");

        let r = get(&c, &s.url("/raw-deflate"), p).await.unwrap();
        assert_eq!(text(&r), HELLO, "raw-deflate {p:?}");

        let r = c
            .send(HttpRequest::head(s.url("/head-gz")), p, &cx())
            .await
            .unwrap();
        assert_eq!(r.status, 200);
        assert!(r.body.is_empty());
        assert_eq!(
            r.header("content-length"),
            Some("12345"),
            "HEAD must preserve Content-Length on gzipped resource ({p:?})"
        );
        assert_eq!(
            r.header("content-encoding"),
            Some("gzip"),
            "HEAD must preserve Content-Encoding ({p:?})"
        );
    }
}

/// Missing test #5: connect-time `PinnedResolver` miss through both `Plain` and
/// `Chrome` maps the resolver error to `FsmError::UnsafeUrl` through
/// `wire_error`/`error_chain` (both direct and `socks5://` paths).
#[tokio::test]
async fn connect_time_pinned_resolver_miss_maps_to_unsafe_url_on_both_stacks() {
    // 1. Direct path: trigger connect-time miss in `HttpClient::send` by racing
    // behind an HTTPS WarmGate while clearing the vetted cache before connect.
    struct RebindOnSecond(AtomicUsize);
    #[async_trait::async_trait]
    impl DnsLookup for RebindOnSecond {
        async fn lookup(&self, _host: &str) -> std::io::Result<Vec<IpAddr>> {
            let n = self.0.fetch_add(1, Ordering::SeqCst);
            Ok(vec![if n == 0 {
                ip("93.184.216.34")
            } else {
                ip("10.0.0.1")
            }])
        }
    }
    for p in [TransportProfile::Plain, TransportProfile::Chrome] {
        let dns = Arc::new(RebindOnSecond(AtomicUsize::new(0)));
        let c = client_with(&base_cfg(), dns.clone());
        let stack_name = match p {
            TransportProfile::Plain => "plain",
            _ => "chrome",
        };
        // Hold the HTTPS warm gate so `send()` passes `vet()` (call #0 -> public IP),
        // then waits at `WarmGates::enter()`. While it waits, clear the vetted cache
        // and release the gate -> `PinnedResolver` misses and re-vets (call #1 -> 10.0.0.1).
        let origin = format!("{stack_name}|false|https://rebind-miss.test:443");
        let lead = c.inner.warm.enter(&origin, Duration::from_secs(1)).await;
        let c2 = c.clone();
        let req_task = tokio::spawn(async move {
            let ncx = NetCx::new(CancellationToken::new(), Duration::from_secs(5));
            c2.send(HttpRequest::get("https://rebind-miss.test/"), p, &ncx)
                .await
        });
        // Wait until `vet()` has completed (call count reaches 1).
        for _ in 0..100 {
            if dns.0.load(Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(dns.0.load(Ordering::SeqCst), 1);
        c.guard().clear_cache();
        drop(lead);
        let e = req_task.await.unwrap().unwrap_err();
        assert!(
            matches!(e, FsmError::UnsafeUrl(_)),
            "{p:?} connect-time miss must map to UnsafeUrl, got: {e}"
        );
        assert_eq!(dns.0.load(Ordering::SeqCst), 2, "{p:?} re-vetted on miss");
    }

    // 2. Also verify `socks5://` connect-time `PinnedResolver` miss maps to `UnsafeUrl`
    // through `wire_error` on both `reqwest` and `wreq`.
    let socks = serve_socks5().await;
    let mut cfg = base_cfg();
    cfg.proxy = Some(format!("socks5://{}", socks.addr));
    let dns = Arc::new(StaticDns::new().with("private-socks.test", &[ip("10.0.0.1")]));
    let c = client_with(&cfg, dns);
    let url = Url::parse("http://private-socks.test/").unwrap();
    let dummy_req = HttpRequest::get(url.as_str());
    let secrets = Secrets {
        req: &dummy_req,
        url: &url,
        proxy: cfg.proxy.as_deref(),
    };
    let reqwest_err = c
        .inner
        .plain
        .proxied
        .as_ref()
        .unwrap()
        .get(url.as_str())
        .send()
        .await
        .unwrap_err();
    let mapped = wire_error(WireError::Reqwest(reqwest_err), "test", &secrets);
    assert!(
        matches!(mapped, FsmError::UnsafeUrl(_)),
        "reqwest socks5 resolver block -> {mapped}"
    );
    let wreq_err = c
        .inner
        .chrome
        .proxied
        .as_ref()
        .unwrap()
        .get(url.as_str())
        .send()
        .await
        .unwrap_err();
    let mapped = wire_error(WireError::Wreq(wreq_err), "test", &secrets);
    assert!(
        matches!(mapped, FsmError::UnsafeUrl(_)),
        "wreq socks5 resolver block -> {mapped}"
    );
}

/// I1 regression (Missing test #6): a redirect crossing a `NO_PROXY` egress
/// boundary with a named `cookie_jar_id` MUST store and read cookies per hop's
/// egress, never leaking cookies between direct and proxied jars.
#[tokio::test]
async fn redirect_crossing_no_proxy_egress_isolates_named_cookie_jars() {
    let direct_srv = serve(|s| match s.target.as_str() {
        "/start" => Reply::Full(
            302,
            vec![
                ("Location".into(), "http://proxied.shared.test/set".into()),
                (
                    "Set-Cookie".into(),
                    "direct_only=D1; Domain=.shared.test; Path=/".into(),
                ),
            ],
            Vec::new(),
        ),
        _ => ok(s.header("cookie").unwrap_or("<none>")),
    })
    .await;
    let direct_port = direct_srv.addr.port();
    let proxy_srv = serve(move |s| {
        if s.target.ends_with("/set") {
            Reply::Full(
                200,
                vec![(
                    "Set-Cookie".into(),
                    "proxy_only=P1; Domain=.shared.test; Path=/".into(),
                )],
                s.header("cookie").unwrap_or("<none>").as_bytes().to_vec(),
            )
        } else if s.target.ends_with("/back") {
            Reply::Full(
                302,
                vec![
                    (
                        "Location".into(),
                        format!("http://direct.shared.test:{direct_port}/echo"),
                    ),
                    (
                        "Set-Cookie".into(),
                        "proxy_hop=P2; Domain=.shared.test; Path=/".into(),
                    ),
                ],
                Vec::new(),
            )
        } else {
            ok(s.header("cookie").unwrap_or("<none>"))
        }
    })
    .await;

    let mut cfg = base_cfg();
    cfg.proxy = Some(format!("http://{}", proxy_srv.addr));
    cfg.no_proxy = vec!["direct.shared.test".into()];
    let dns = Arc::new(StaticDns::new().with("direct.shared.test", &[ip("127.0.0.1")]));
    let c = client_with(&cfg, dns);
    allow(&c, &direct_srv);

    let with_jar = |url: String| {
        let mut r = HttpRequest::get(url);
        r.cookie_jar_id = Some("egress-test".into());
        r
    };

    // Direct -> Proxied redirect: hop 1 (proxied) must NOT see `direct_only`,
    // and its `proxy_only` cookie must land in the proxied jar, not direct.
    let r = c
        .send(
            with_jar(format!("http://direct.shared.test:{direct_port}/start")),
            TransportProfile::Chrome,
            &cx(),
        )
        .await
        .unwrap();
    assert_eq!(
        text(&r),
        "<none>",
        "direct cookie must not cross to proxy hop"
    );

    // Follow-up direct request sees only `direct_only`, never `proxy_only`.
    let r = c
        .send(
            with_jar(format!("http://direct.shared.test:{direct_port}/echo")),
            TransportProfile::Chrome,
            &cx(),
        )
        .await
        .unwrap();
    assert_eq!(text(&r), "direct_only=D1");

    // Follow-up proxied request sees only `proxy_only`, never `direct_only`.
    let r = c
        .send(
            with_jar("http://proxied.shared.test/echo".into()),
            TransportProfile::Chrome,
            &cx(),
        )
        .await
        .unwrap();
    assert_eq!(text(&r), "proxy_only=P1");

    // Reverse direction: Proxied -> Direct redirect keeps jars separate too.
    let r = c
        .send(
            with_jar("http://proxied.shared.test/back".into()),
            TransportProfile::Chrome,
            &cx(),
        )
        .await
        .unwrap();
    assert_eq!(
        text(&r),
        "direct_only=D1",
        "proxy_hop must not leak into direct jar"
    );
}

/// I4 regression (Missing test #7e): extreme `request_timeout_s` and
/// `cx.timeout = Duration::MAX` must not panic.
#[tokio::test]
async fn extreme_timeouts_do_not_panic() {
    let s = serve(|_| ok("ok")).await;
    let mut cfg = base_cfg();
    cfg.request_timeout_s = 1e25;
    let c = client_with(&cfg, Arc::new(StaticDns::new()));
    allow(&c, &s);
    let r = c
        .send(
            HttpRequest::get(s.url("/")),
            TransportProfile::Plain,
            &NetCx::default(),
        )
        .await
        .unwrap();
    assert_eq!(text(&r), "ok");

    let ncx = NetCx::new(CancellationToken::new(), Duration::MAX);
    let r = c
        .send(HttpRequest::get(s.url("/")), TransportProfile::Chrome, &ncx)
        .await
        .unwrap();
    assert_eq!(text(&r), "ok");
}

/// I5 regression (Missing test #8): invalid request headers map to
/// `FsmError::InvalidInput` on `Chrome` and `EdgePreflight` as well as `Plain`.
#[tokio::test]
async fn invalid_request_headers_return_invalid_input_on_all_stacks() {
    let s = serve(|_| ok("ok")).await;
    let c = client();
    allow(&c, &s);
    for p in [
        TransportProfile::Plain,
        TransportProfile::Chrome,
        TransportProfile::EdgePreflight,
    ] {
        for req in [
            HttpRequest::get(s.url("/")).header("Bad Header", "val"),
            HttpRequest::get(s.url("/")).header("X-Test", "line1\r\nline2"),
        ] {
            let e = c.send(req, p, &cx()).await.unwrap_err();
            assert!(
                matches!(e, FsmError::InvalidInput(_)),
                "{p:?} invalid header -> {e}"
            );
        }
    }
    assert_eq!(s.connections(), 0);
}

/// M1 regression (Missing test #9): `302` with an empty or whitespace-only
/// `Location:` header fails immediately after 1 request, not 6.
#[tokio::test]
async fn empty_location_header_fails_immediately_after_one_request() {
    for loc in ["", "   "] {
        for p in [TransportProfile::Plain, TransportProfile::Chrome] {
            let s = serve(move |_| redirect(302, loc)).await;
            let c = client();
            allow(&c, &s);
            let e = get(&c, &s.url("/empty-loc"), p).await.unwrap_err();
            assert!(
                e.to_string().contains("without a Location"),
                "loc={loc:?} {p:?}: {e}"
            );
            assert_eq!(
                s.requests().len(),
                1,
                "must fail immediately on hop 0, not loop 6 times (loc={loc:?}, {p:?})"
            );
        }
    }
}

/// M2 regression: cross-origin redirects strip caller-supplied `Host`, and
/// 301/302/303 POST->GET drops `Content-Encoding` and `Transfer-Encoding`.
#[tokio::test]
async fn cross_origin_strips_host_and_post_to_get_drops_encoding_headers() {
    let other = serve(|_| ok("other")).await;
    let to = other.url("/dest");
    let front = serve(move |s| match s.target.as_str() {
        "/cross" => redirect(302, &to),
        "/post302" => redirect(302, "/landed"),
        _ => ok("landed"),
    })
    .await;
    let c = client();
    allow(&c, &front);
    allow(&c, &other);

    let req = HttpRequest::get(front.url("/cross")).header("Host", "spoofed.example");
    c.send(req, TransportProfile::Plain, &cx()).await.unwrap();
    let seen_other = other.requests().last().cloned().unwrap();
    assert_ne!(
        seen_other.header("host"),
        Some("spoofed.example"),
        "caller Host must not cross origins"
    );

    let post = HttpRequest::post(front.url("/post302"), "body")
        .header("Content-Type", "text/plain")
        .header("Content-Encoding", "gzip")
        .header("Transfer-Encoding", "identity");
    c.send(post, TransportProfile::Plain, &cx()).await.unwrap();
    let landed = front.requests().last().cloned().unwrap();
    assert_eq!(landed.method, "GET");
    assert_eq!(landed.header("content-type"), None);
    assert_eq!(landed.header("content-encoding"), None);
    assert_eq!(landed.header("transfer-encoding"), None);
}

/// M3 regression: `prewarm` honours `Config.prewarm = false`, and
/// `no_proxy_matches` normalizes trailing dots and IPv6 brackets.
#[tokio::test]
async fn prewarm_respects_config_and_no_proxy_normalizes_entries() {
    let s = serve(|_| ok("")).await;
    let mut cfg = base_cfg();
    cfg.prewarm = false;
    let c = client_with(&cfg, Arc::new(StaticDns::new()));
    allow(&c, &s);
    assert_eq!(
        c.prewarm(&[(s.url("/warm").as_str(), TransportProfile::Plain)])
            .await,
        0
    );
    assert_eq!(s.connections(), 0, "prewarm=false must not connect");

    assert!(no_proxy_matches(&["example.com.".into()], "example.com"));
    assert!(no_proxy_matches(&[".example.com.".into()], "a.example.com"));
    assert!(no_proxy_matches(&["[::1]".into()], "::1"));
    assert!(no_proxy_matches(&["::1".into()], "[::1]"));
}

/// RFC 9659 regression: a 9-byte zstd frame requesting a 64 MiB decompression
/// window (`windowLog = 26 > 23`) must be rejected instead of allocating 64 MiB.
#[tokio::test]
async fn zstd_huge_window_frame_is_rejected_under_rfc9659() {
    // Magic (4) + FHD=0x00 (1) + Window_Descriptor=0x80 (1 => 64 MiB) + empty last raw block (3).
    const HUGE_WINDOW_ZSTD: &[u8] = &[0x28, 0xB5, 0x2F, 0xFD, 0x00, 0x80, 0x01, 0x00, 0x00];
    let s = serve(|_| {
        Reply::Full(
            200,
            vec![("Content-Encoding".into(), "zstd".into())],
            HUGE_WINDOW_ZSTD.to_vec(),
        )
    })
    .await;
    let c = client();
    allow(&c, &s);
    for p in [TransportProfile::Plain, TransportProfile::Chrome] {
        let err = get(&c, &s.url("/zstd-bomb"), p)
            .await
            .expect_err("64 MiB zstd window frame must be rejected");
        assert!(
            matches!(err, FsmError::Upstream { .. }),
            "{p:?}: expected Upstream error, got {err:?}"
        );
    }
}

/// Hostile upstream sending >512 KiB of HTTP response headers must be rejected
/// cleanly without panicking or allocating unbounded memory.
#[tokio::test]
async fn huge_response_headers_are_rejected_without_panic() {
    let big_val = "A".repeat(600 * 1024);
    let s = serve(move |_| {
        Reply::Full(
            200,
            vec![("X-Huge".into(), big_val.clone())],
            b"ok".to_vec(),
        )
    })
    .await;
    let c = client();
    allow(&c, &s);
    for p in [TransportProfile::Plain, TransportProfile::Chrome] {
        let err = get(&c, &s.url("/huge-hdr"), p)
            .await
            .expect_err("600 KiB response header must be rejected");
        assert!(
            matches!(err, FsmError::Upstream { .. }),
            "{p:?}: expected Upstream error, got {err:?}"
        );
    }
}

#[test]
fn hybrid_cert_verifier_falls_back_only_on_unknown_issuer() {
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{CertificateError, DigitallySignedStruct, Error as TlsError, SignatureScheme};

    #[derive(Debug)]
    struct StubVerifier {
        calls: Arc<AtomicUsize>,
        outcome: fn() -> Result<ServerCertVerified, TlsError>,
    }

    impl ServerCertVerifier for StubVerifier {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, TlsError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            (self.outcome)()
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, TlsError> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, TlsError> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            vec![SignatureScheme::ECDSA_NISTP256_SHA256]
        }
    }

    let cert = CertificateDer::from(vec![0u8; 4]);
    let name = ServerName::try_from("example.test").unwrap();
    let now = UnixTime::now();

    // 1. Primary Ok -> fallback NOT called.
    let fb_calls = Arc::new(AtomicUsize::new(0));
    let v_ok = HybridCertVerifier {
        webpki: Arc::new(StubVerifier {
            calls: Arc::new(AtomicUsize::new(0)),
            outcome: || Ok(ServerCertVerified::assertion()),
        }),
        platform: Some(Arc::new(StubVerifier {
            calls: Arc::clone(&fb_calls),
            outcome: || Ok(ServerCertVerified::assertion()),
        })),
    };
    assert!(v_ok.verify_server_cert(&cert, &[], &name, &[], now).is_ok());
    assert_eq!(fb_calls.load(Ordering::SeqCst), 0);

    // 2. Primary UnknownIssuer -> fallback IS called and its Ok is accepted.
    let fb_calls = Arc::new(AtomicUsize::new(0));
    let v_unknown = HybridCertVerifier {
        webpki: Arc::new(StubVerifier {
            calls: Arc::new(AtomicUsize::new(0)),
            outcome: || {
                Err(TlsError::InvalidCertificate(
                    CertificateError::UnknownIssuer,
                ))
            },
        }),
        platform: Some(Arc::new(StubVerifier {
            calls: Arc::clone(&fb_calls),
            outcome: || Ok(ServerCertVerified::assertion()),
        })),
    };
    assert!(
        v_unknown
            .verify_server_cert(&cert, &[], &name, &[], now)
            .is_ok()
    );
    assert_eq!(fb_calls.load(Ordering::SeqCst), 1);

    // 3. Primary Expired -> fallback MUST NOT be called.
    let fb_calls = Arc::new(AtomicUsize::new(0));
    let v_expired = HybridCertVerifier {
        webpki: Arc::new(StubVerifier {
            calls: Arc::new(AtomicUsize::new(0)),
            outcome: || Err(TlsError::InvalidCertificate(CertificateError::Expired)),
        }),
        platform: Some(Arc::new(StubVerifier {
            calls: Arc::clone(&fb_calls),
            outcome: || Ok(ServerCertVerified::assertion()),
        })),
    };
    assert!(matches!(
        v_expired.verify_server_cert(&cert, &[], &name, &[], now),
        Err(TlsError::InvalidCertificate(CertificateError::Expired))
    ));
    assert_eq!(fb_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn tls_handshake_eof_retries_with_tls12_http1_for_idempotent_get_only() {
    use tokio::io::AsyncReadExt as _;
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hellos: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let hellos_srv = Arc::clone(&hellos);
    let srv = tokio::spawn(async move {
        for _ in 0..3 {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let mut buf = vec![0u8; 2048];
            let n = tokio::time::timeout(Duration::from_secs(2), sock.read(&mut buf))
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or(0);
            buf.truncate(n);
            hellos_srv.lock().unwrap().push(buf);
            // Immediately drop TCP socket to trigger TLS handshake EOF.
            drop(sock);
        }
    });

    let c = client();
    c.guard().exempt.lock().unwrap().push(addr);

    // 1. GET triggers one retry via the TLS 1.2 + http/1.1 fallback client (2 ClientHellos).
    let url = format!("https://127.0.0.1:{}/oai", addr.port());
    let _ = c
        .send(HttpRequest::get(&url), TransportProfile::Plain, &cx())
        .await
        .expect_err("mock listener closes both connections");
    {
        let recorded = hellos.lock().unwrap();
        assert_eq!(
            recorded.len(),
            2,
            "idempotent GET must retry once on TLS handshake EOF"
        );
        // Attempt 1 advertises h2 + http/1.1 ALPN.
        assert!(
            recorded[0].windows(2).any(|w| w == b"h2"),
            "first ClientHello should offer h2 ALPN"
        );
        // Attempt 2 advertises http/1.1 only (no h2 ALPN).
        assert!(
            !recorded[1].windows(2).any(|w| w == b"h2"),
            "TLS 1.2 fallback ClientHello must not offer h2 ALPN"
        );
        assert!(
            recorded[1].windows(8).any(|w| w == b"http/1.1"),
            "TLS 1.2 fallback ClientHello must offer http/1.1 ALPN"
        );
    }

    // 2. POST on a fresh client must NOT retry on TLS handshake EOF (1 ClientHello only).
    let c2 = client();
    c2.guard().exempt.lock().unwrap().push(addr);
    let _ = c2
        .send(
            HttpRequest::post(&url, "body"),
            TransportProfile::Plain,
            &cx(),
        )
        .await
        .expect_err("mock listener closes connection");
    assert_eq!(
        hellos.lock().unwrap().len(),
        3,
        "non-idempotent POST must not retry on TLS handshake EOF"
    );

    srv.abort();
}
