//! `fsm_net::replay::ReplayNet` is the core replay client, and both it and
//! the live `HttpClient` plug into `SourceDeps` as `Arc<dyn NetClient>`.

use fsm_core::fixture::Fixture;
use fsm_core::{
    CancellationToken, Config, HttpRequest, NetClient, NetCx, NullStore, SourceDeps,
    TransportProfile,
};
use std::sync::Arc;
use std::time::Duration;

const FIXTURE: &str = r#"{
  "version": 1,
  "source": "demo",
  "op": "search",
  "recordedAt": "2026-11-25T10:00:00Z",
  "exchanges": [
    {
      "request": {
        "method": "GET",
        "url": "https://api.example.org/v1/search?q=rust",
        "headersSubset": {},
        "bodySha256": null
      },
      "response": {
        "status": 200,
        "headers": [["content-type", "application/json"]],
        "bodyText": "{\"hits\":[]}",
        "bodyB64": null
      }
    }
  ]
}"#;

#[tokio::test]
async fn replay_net_is_reexported_and_serves_fixtures() {
    let replay =
        fsm_net::replay::ReplayNet::from_fixtures(vec![Fixture::from_json(FIXTURE).unwrap()])
            .unwrap();
    let net: Arc<dyn NetClient> = Arc::new(replay);
    let cx = NetCx::new(CancellationToken::new(), Duration::from_secs(1));
    let req = HttpRequest::get("https://api.example.org/v1/search")
        .query_param("q", "rust")
        .query_param("key", "SECRET")
        .redact_query_key("key");
    let r = net.send(req, TransportProfile::Plain, &cx).await.unwrap();
    assert_eq!((r.status, &r.body[..]), (200, &b"{\"hits\":[]}"[..]));
    assert_eq!(r.final_url, "https://api.example.org/v1/search?q=rust");
}

#[tokio::test]
async fn http_client_is_a_net_client_for_source_deps() {
    let mut cfg = Config::default();
    cfg.proxy = None;
    cfg.allow_private_hosts = false;
    let live = fsm_net::HttpClient::builder(&cfg)
        .dns(Arc::new(fsm_net::StaticDns::new()))
        .build()
        .unwrap();
    let deps = SourceDeps::new(Arc::new(live), Arc::new(NullStore::new()), Arc::new(cfg));
    // The DNS-free check a cache hit runs (SPEC §7.2).
    assert!(deps.net.check_url_offline("https://example.com/a").is_ok());
    for bad in [
        "http://169.254.169.254/",
        "http://localhost/",
        "file:///etc/passwd",
        "http://[::ffff:10.0.0.1]/",
    ] {
        assert!(deps.net.check_url_offline(bad).is_err(), "{bad}");
    }
}
