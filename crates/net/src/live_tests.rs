//! Live smoke tests, double-gated so no offline or `--ignored` run touches the
//! network by accident: `FSM_NET_LIVE=1 cargo test -p fsm-net -- --ignored live_`.
//! A handful of requests to example.com; nothing personal is sent. Set
//! `FSM_NET_EXPECT_FAKE_IP=1` on a machine behind a fake-IP tunnel (Clash TUN)
//! to also require the canary verdict.

use super::*;
use crate::dns::{HickoryDns, SystemDns};

/// The env flag; `false` means skip (the run was `--ignored` without it).
fn live_enabled() -> bool {
    let on = std::env::var("FSM_NET_LIVE").is_ok_and(|v| v == "1");
    if !on {
        eprintln!("skipped: set FSM_NET_LIVE=1 to run the live smoke");
    }
    on
}

fn live_cfg() -> Config {
    let mut c = Config::default();
    c.proxy = None;
    c.allow_private_hosts = false;
    c.ssrf_resolve_addresses = fsm_core::SsrfResolveMode::Auto;
    c.contact_email = None;
    c.user_agent = String::new();
    c
}

/// Outbound smoke requests carry a generic UA, never the product name.
const GENERIC_UA: &str = "Mozilla/5.0";

#[tokio::test]
#[ignore = "live network"]
async fn live_resolvers_and_canary_verdict() {
    if !live_enabled() {
        return;
    }
    let hickory = HickoryDns::from_system().expect("hickory reads the system DNS configuration");
    let h = hickory
        .lookup("example.com")
        .await
        .expect("hickory resolves example.com");
    let s = SystemDns
        .lookup("example.com")
        .await
        .expect("getaddrinfo resolves example.com");
    eprintln!("hickory example.com -> {h:?}");
    eprintln!("system  example.com -> {s:?}");
    assert!(!h.is_empty() && !s.is_empty());
    let c = HttpClient::new(&live_cfg()).unwrap();
    let verdict = c.fake_ip_verdict().await;
    eprintln!("fake-IP canary verdict: {verdict:?}");
    if std::env::var("FSM_NET_EXPECT_FAKE_IP").is_ok_and(|v| v == "1") {
        assert_eq!(
            verdict,
            Some(true),
            "canaries did not resolve into a fake-IP range"
        );
        assert!(
            h.iter().all(|ip| crate::ssrf::in_fake_ip_candidate(*ip)),
            "hickory saw real addresses: {h:?}"
        );
    }
}

#[tokio::test]
#[ignore = "live network"]
async fn live_example_com_through_both_stacks() {
    if !live_enabled() {
        return;
    }
    let c = HttpClient::new(&live_cfg()).unwrap();
    let ncx = NetCx::new(CancellationToken::new(), Duration::from_secs(20));
    for p in [TransportProfile::Plain, TransportProfile::Chrome] {
        let r = c
            .send(
                HttpRequest::get("https://example.com/").header("User-Agent", GENERIC_UA),
                p,
                &ncx,
            )
            .await
            .unwrap_or_else(|e| panic!("{p:?}: {e}"));
        let body = String::from_utf8_lossy(&r.body);
        eprintln!(
            "{p:?}: {} {} bytes, final {}",
            r.status,
            r.body.len(),
            r.final_url
        );
        assert_eq!(r.status, 200);
        assert!(body.contains("Example Domain"), "{p:?}");
    }
    // ALPN: both stacks negotiate HTTP/2 (rustls preconfigured with h2 + http/1.1).
    let target = c
        .inner
        .guard
        .check("https://example.com/", false)
        .await
        .unwrap();
    let v = c
        .inner
        .plain
        .direct
        .head(target.url.as_str())
        .header("user-agent", GENERIC_UA)
        .send()
        .await
        .unwrap()
        .version();
    let w = c
        .inner
        .chrome
        .direct
        .head(target.url.as_str())
        .send()
        .await
        .unwrap()
        .version();
    eprintln!("plain {v:?}, chrome {w:?}");
    assert_eq!(v, http::Version::HTTP_2);
    assert_eq!(w, http::Version::HTTP_2);
}
