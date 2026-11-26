//! `ReplayNet` (A1 §B.9) against real recorded W5 fixtures (copied into
//! `tests/fixtures/`: w5b clinicaltrials, w5d1 worldbank) and synthetic ones.

use bytes::Bytes;
use fsm_core::fixture::{
    Exchange, Fixture, FixtureRequest, FixtureResponse, ReplayNet, sha256_hex,
};
use fsm_core::{FsmError, HttpRequest, NetClient, NetCx, TransportProfile};

const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
const WB: &str = "https://api.worldbank.org/v2/country/USA/indicator/NY.GDP.MKTP.CD";

async fn send(net: &ReplayNet, req: HttpRequest) -> Result<fsm_core::HttpBytes, FsmError> {
    net.send(req, TransportProfile::Plain, &NetCx::default())
        .await
}

fn synthetic(exchanges: Vec<(FixtureRequest, &str)>) -> ReplayNet {
    let mut f = Fixture::new("demo", "op", "2026-11-25T00:00:00Z");
    for (req, body) in exchanges {
        f.exchanges.push(Exchange::new(
            req,
            FixtureResponse::from_bytes(200, vec![], body.as_bytes()),
        ));
    }
    ReplayNet::from_fixtures(vec![f]).unwrap()
}

#[tokio::test]
async fn serves_a_recorded_exchange() {
    let net = ReplayNet::from_dir(DIR).unwrap();
    assert_eq!(net.len(), 5);
    let req = HttpRequest::get(format!("{WB}?format=json&per_page=3"))
        .header("Accept", "application/json");
    let r = send(&net, req).await.unwrap();
    assert_eq!(r.status, 200);
    assert!(r.body.starts_with(b"[{\"page\":1"));
    assert_eq!(r.media_type().as_deref(), Some("application/json"));
    let hits = net.hits();
    assert_eq!(
        (hits[0].fixture.as_str(), hits[0].exchange),
        ("worldbank/indicator", 0)
    );
    assert_eq!(net.unused().len(), 4);
    let single =
        ReplayNet::from_file(format!("{DIR}/w5b/clinicaltrials/study/default.json")).unwrap();
    assert_eq!(single.len(), 1);
}

#[tokio::test]
async fn query_order_does_not_matter() {
    let net = ReplayNet::from_dir(DIR).unwrap();
    // Pairs split between url and HttpRequest::query, in another order.
    let req = HttpRequest::get(WB)
        .query_param("per_page", "3")
        .query_param("format", "json")
        .header("accept", "application/json");
    assert!(send(&net, req).await.is_ok());
    let url = "https://clinicaltrials.gov/api/v2/studies?format=json&pageSize=3&fields=protocolSection.identificationModule,protocolSection.statusModule,protocolSection.designModule,protocolSection.conditionsModule,protocolSection.sponsorCollaboratorsModule,protocolSection.descriptionModule&query.term=crispr";
    let r = send(
        &net,
        HttpRequest::get(url).header("Accept", "application/json"),
    )
    .await
    .unwrap();
    assert!(String::from_utf8_lossy(&r.body).contains("studies"));
    assert_eq!(net.hits()[1].fixture, "clinicaltrials/search");
}

#[tokio::test]
async fn headers_subset_must_be_present() {
    let net = ReplayNet::from_dir(DIR).unwrap();
    let base = || HttpRequest::get(format!("{WB}?format=json&per_page=3"));
    let extra = base()
        .header("ACCEPT", "application/json")
        .header("User-Agent", "test");
    assert!(
        send(&net, extra).await.is_ok(),
        "extra headers are fine; names case-insensitive"
    );
    let err = send(&net, base()).await.unwrap_err().to_string();
    assert!(err.contains("header accept missing"), "{err}");
    let err = send(&net, base().header("Accept", "text/html"))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("header accept: \"text/html\" vs recorded \"application/json\""),
        "{err}"
    );
}

#[tokio::test]
async fn body_hash_is_compared_when_recorded() {
    let body = Bytes::from_static(b"{\"a\":1}");
    let mut rec = FixtureRequest::new("POST", "https://api.example.org/search");
    rec.body_sha256 = Some(sha256_hex(&body));
    let net = synthetic(vec![(rec, "ok")]);
    assert!(
        send(
            &net,
            HttpRequest::post("https://api.example.org/search", body)
        )
        .await
        .is_ok()
    );
    let err = send(
        &net,
        HttpRequest::post(
            "https://api.example.org/search",
            Bytes::from_static(b"{\"a\":2}"),
        ),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("body sha256"), "{err}");
    assert_eq!(
        sha256_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

#[tokio::test]
async fn redacted_credentials_are_ignored() {
    let net = ReplayNet::from_dir(DIR).unwrap();
    let keyed = || {
        HttpRequest::get(format!("{WB}?format=json&api_key=SECRET&per_page=3"))
            .header("Accept", "application/json")
            .header("Authorization", "Bearer SECRET")
    };
    let ok = keyed()
        .redact_query_key("API_KEY")
        .redact_header("authorization");
    let r = send(&net, ok).await.unwrap();
    assert!(!r.final_url.contains("SECRET"), "{}", r.final_url);
    assert!(!net.hits()[0].url.contains("SECRET"));
    let err = send(&net, keyed()).await.unwrap_err().to_string();
    assert!(err.contains("unexpected [api_key=SECRET]"), "{err}");
    // A credential header recorded by mistake is ignored when redacted.
    let mut rec = FixtureRequest::new("GET", "https://api.example.org/x");
    rec.headers_subset
        .insert("authorization".into(), "Bearer recorded".into());
    let net = synthetic(vec![(rec, "ok")]);
    let req = HttpRequest::get("https://api.example.org/x")
        .header("Authorization", "Bearer live")
        .redact_header("Authorization");
    assert!(send(&net, req).await.is_ok());
    // Recording strips them too.
    let req = HttpRequest::get("https://api.example.org/x?api_key=S&q=1")
        .header("Authorization", "Bearer S")
        .header("Accept", "application/json")
        .redact_query_key("api_key")
        .redact_header("authorization");
    let rec = FixtureRequest::from_http(&req, &["accept", "authorization"]);
    assert_eq!(rec.url, "https://api.example.org/x?q=1");
    assert_eq!(rec.headers_subset.len(), 1);
    assert_eq!(rec.body_sha256, None);
}

#[tokio::test]
async fn multi_exchange_fixtures_are_served_in_order() {
    let req = || FixtureRequest::new("GET", "https://feed.example.org/osdd");
    let net = synthetic(vec![
        (req(), "first"),
        (req(), "second"),
        (
            FixtureRequest::new("GET", "https://feed.example.org/feed?q=x"),
            "feed",
        ),
    ]);
    assert_eq!(net.unused().len(), 3);
    let get = |u: &str| HttpRequest::get(u);
    assert_eq!(
        &send(&net, get("https://feed.example.org/osdd"))
            .await
            .unwrap()
            .body[..],
        b"first"
    );
    assert_eq!(
        &send(&net, get("https://feed.example.org/feed?q=x"))
            .await
            .unwrap()
            .body[..],
        b"feed"
    );
    assert_eq!(
        &send(&net, get("https://feed.example.org/osdd"))
            .await
            .unwrap()
            .body[..],
        b"second"
    );
    assert_eq!(
        &send(&net, get("https://feed.example.org/osdd"))
            .await
            .unwrap()
            .body[..],
        b"second",
        "last one repeats"
    );
    assert!(net.unused().is_empty());
    let order: Vec<usize> = net.hits().iter().map(|h| h.exchange).collect();
    assert_eq!(order, [0, 2, 1, 1]);
}

#[tokio::test]
async fn unmatched_requests_name_the_closest_candidate() {
    let net = ReplayNet::from_dir(DIR).unwrap();
    let url = format!("{WB}?format=json&per_page=5");
    let err = send(
        &net,
        HttpRequest::get(url.clone()).header("Accept", "application/json"),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            err,
            FsmError::Internal {
                kind: "replay_unmatched",
                ..
            }
        ),
        "{err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains(&format!("no recorded exchange for GET {url}")),
        "{msg}"
    );
    assert!(msg.contains("closest: worldbank/indicator#0"), "{msg}");
    assert!(
        msg.contains("query missing [per_page=3] unexpected [per_page=5]"),
        "{msg}"
    );
    assert_eq!(net.unmatched(), [format!("GET {url}")]);
    let err = send(
        &net,
        HttpRequest::post("https://other.example/", Bytes::new()),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("method POST vs recorded GET") && err.contains("origin"),
        "{err}"
    );
    let empty = tempfile::tempdir().unwrap();
    assert!(
        ReplayNet::from_dir(empty.path())
            .unwrap_err()
            .to_string()
            .contains("no *.json fixtures")
    );
}

#[tokio::test]
async fn max_bytes_truncates_and_urls_are_checked() {
    let net = ReplayNet::from_dir(DIR).unwrap();
    let req = HttpRequest::get(format!("{WB}?format=json&per_page=3"))
        .header("Accept", "application/json")
        .with_max_bytes(10);
    let r = send(&net, req).await.unwrap();
    assert_eq!((r.body.len(), r.truncated), (10, true));
    assert!(net.check_url_offline("https://example.org/").is_ok());
    assert!(net.check_url_offline("file:///etc/passwd").is_err());
}

#[tokio::test]
async fn review_edge_cases() {
    // Duplicate query pairs count (multiset), header lookup is last-wins.
    let mut rec = FixtureRequest::new("GET", "https://api.example.org/x?id=1&id=1");
    rec.headers_subset
        .insert("accept".into(), "application/json".into());
    let net = synthetic(vec![(rec, "ok")]);
    let one =
        HttpRequest::get("https://api.example.org/x?id=1").header("Accept", "application/json");
    let err = send(&net, one).await.unwrap_err().to_string();
    assert!(err.contains("query missing [id=1] unexpected []"), "{err}");
    let two = HttpRequest::get("https://api.example.org/x?id=1&id=1")
        .header("Accept", "text/html")
        .header("accept", "application/json");
    assert!(send(&net, two).await.is_ok());
    let req = HttpRequest::get("https://api.example.org/x")
        .header("Accept", "text/html")
        .header("accept", "application/json");
    let rec = FixtureRequest::from_http(&req, &["accept"]);
    assert_eq!(
        rec.headers_subset["accept"], "application/json",
        "recording is last-wins too"
    );
    // A cancelled call fails before matching.
    let cx = NetCx::default();
    cx.cancel.cancel();
    let req = HttpRequest::get("https://api.example.org/x?id=1&id=1")
        .header("accept", "application/json");
    assert!(matches!(
        net.send(req, TransportProfile::Plain, &cx).await,
        Err(FsmError::Timeout(0))
    ));
    assert_eq!(net.hits().len(), 1);
}

#[test]
fn canonical_json_sorts_keys_recursively() {
    let v = serde_json::json!({"b": 1, "a": {"z": [{"y": 1, "x": 2}], "m": null}});
    assert_eq!(
        fsm_core::fixture::canonical_json(&v),
        r#"{"a":{"m":null,"z":[{"x":2,"y":1}]},"b":1}"#
    );
    assert_eq!(
        fsm_core::fixture::canonical_json(&serde_json::json!([3, 1])),
        "[3,1]"
    );
}
