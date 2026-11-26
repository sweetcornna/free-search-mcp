//! Identifier lookups against a scripted network: the failure shapes the
//! recorded fixtures cannot show (timeouts, 404 mixes, synthetic 429s in
//! both the status and the error form, empty arXiv feeds) and the
//! UTC-midnight bench. The 429 answers here are SYNTHETIC; the recorded
//! real one is in `tests/fixtures/arxiv_1706.03762/s2_arxiv_429.json`.

mod common;

use common::*;
use fsm_core::CallCx;
use fsm_scholar::Provider;
use std::sync::Arc;
use std::time::Duration;

const RRF_DOI: &str = "10.1145/1571941.1572114";

fn cx() -> CallCx {
    CallCx::default()
}

fn openalex_rrf() -> String {
    fixture_body("doi_rrf/openalex_work.json")
}

/// Route by host: (openalex, crossref, s2, arxiv, idconv) bodies; `None` = 404.
fn routed(
    openalex: Option<String>,
    crossref: Option<String>,
    s2: Option<String>,
    arxiv: Option<String>,
) -> Arc<ScriptNet> {
    ScriptNet::new(move |url| {
        let pick = |b: &Option<String>| match b {
            Some(body) => (Duration::from_millis(5), 200, body.clone()),
            None => (Duration::from_millis(5), 404, String::new()),
        };
        if url.contains("api.openalex.org") {
            pick(&openalex)
        } else if url.contains("api.crossref.org") {
            pick(&crossref)
        } else if url.contains("api.semanticscholar.org") {
            pick(&s2)
        } else if url.contains("export.arxiv.org") {
            pick(&arxiv)
        } else {
            (Duration::from_millis(5), 404, String::new())
        }
    })
}

/// Port of `test_openalex_retraction_flag_alone_is_enough`.
#[tokio::test]
async fn test_openalex_retraction_flag_alone_is_enough() {
    let body = openalex_rrf().replace("\"is_retracted\":false", "\"is_retracted\":true");
    assert!(body.contains("\"is_retracted\":true"));
    let net = routed(Some(body), None, None, None);
    let core = core_with(Vec::new(), net, MemStore::new(), config(&[]));
    let w = core.work("W2148972377", &cx()).await.unwrap().unwrap();
    assert!(w.retracted);
    assert_eq!(w.ids.openalex.as_deref(), Some("W2148972377"));
}

/// Port of `test_a_crossref_404_is_reported_as_a_datacite_doi_not_a_fake`.
#[tokio::test]
async fn test_a_crossref_404_is_reported_as_a_datacite_doi_not_a_fake() {
    let net = routed(Some(openalex_rrf()), None, None, None);
    let core = core_with(Vec::new(), net, MemStore::new(), config(&[]));
    let r = core.resolve(RRF_DOI, &cx()).await.unwrap();
    assert!(r.work.is_some());
    assert!(r.notes[0].contains("DataCite"), "{:?}", r.notes);
}

/// Port of `test_a_crossref_timeout_claims_nothing`: a timeout is not a 404.
#[tokio::test]
async fn test_a_crossref_timeout_claims_nothing() {
    let oa = openalex_rrf();
    let net = ScriptNet::new(move |url| {
        if url.contains("api.crossref.org") {
            (Duration::from_millis(5), 0, "!timeout".into())
        } else if url.contains("api.openalex.org") {
            (Duration::from_millis(5), 200, oa.clone())
        } else {
            (Duration::from_millis(5), 404, String::new())
        }
    });
    let store = MemStore::new();
    let core = core_with(Vec::new(), net, store.clone(), config(&[]));
    let r = core.resolve(RRF_DOI, &cx()).await.unwrap();
    assert!(r.work.is_some());
    assert!(
        !r.notes
            .iter()
            .any(|n| n.contains("DataCite") || n.contains("no record")),
        "{:?}",
        r.notes
    );
    assert!(
        r.notes.iter().any(|n| n.starts_with("crossref: Timed out")),
        "{:?}",
        r.notes
    );
    // Partial: not written to the scholar cache, and the failure is not cached.
    assert!(r.partial);
    assert_eq!(
        store.scholar_puts.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert!(
        !store
            .api_keys()
            .iter()
            .any(|k| k.starts_with("scholar:v1:crossref:"))
    );
}

#[tokio::test(start_paused = true)]
async fn a_hanging_lookup_is_cut_at_the_lookup_timeout() {
    let oa = openalex_rrf();
    let net = ScriptNet::new(move |url| {
        if url.contains("api.crossref.org") {
            (Duration::from_secs(60), 200, "{}".into())
        } else if url.contains("api.openalex.org") {
            (Duration::from_millis(5), 200, oa.clone())
        } else {
            (Duration::from_millis(5), 404, String::new())
        }
    });
    let core = core_with(Vec::new(), net, MemStore::new(), config(&[]));
    let started = tokio::time::Instant::now();
    let r = core.resolve(RRF_DOI, &cx()).await.unwrap();
    assert!(started.elapsed() < Duration::from_millis(6_100));
    assert!(r.work.is_some());
    assert!(
        r.notes
            .iter()
            .any(|n| n == "crossref: timed out after 6000 ms"),
        "{:?}",
        r.notes
    );
}

/// Port of `test_an_unknown_arxiv_id_says_so`.
#[tokio::test]
async fn test_an_unknown_arxiv_id_says_so() {
    let empty = r#"<?xml version="1.0" encoding="UTF-8"?><feed xmlns="http://www.w3.org/2005/Atom"><title>arXiv Query</title></feed>"#;
    let net = routed(None, None, None, Some(empty.into()));
    let core = core_with(Vec::new(), net, MemStore::new(), config(&[]));
    let r = core.resolve("arXiv:9999.99999", &cx()).await.unwrap();
    assert!(r.work.is_none() && !r.upstream_failed);
    assert!(
        r.notes[0].contains("arXiv returned no record for '9999.99999'"),
        "{:?}",
        r.notes
    );
    assert!(!r.notes[0].contains("mistyped"));
}

/// Port of `test_a_preprint_openalex_has_not_indexed_yet_says_that`: the
/// arXiv record itself is returned (Python returned nothing), with the note.
#[tokio::test]
async fn test_a_preprint_openalex_has_not_indexed_yet_says_that() {
    let feed = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom" xmlns:arxiv="http://arxiv.org/schemas/atom">
  <entry>
    <id>http://arxiv.org/abs/2609.01234v1</id>
    <title>A Paper From Yesterday Nobody Has Indexed</title>
    <summary>Fresh results.</summary>
    <published>2026-09-01T00:00:00Z</published>
    <author><name>Ada Lovelace</name></author>
  </entry>
</feed>"#;
    let net = routed(
        None,
        Some(r#"{"message":{"items":[]}}"#.into()),
        None,
        Some(feed.into()),
    );
    let core = core_with(Vec::new(), net.clone(), MemStore::new(), config(&[]));
    let r = core
        .resolve("https://arxiv.org/abs/2609.01234", &cx())
        .await
        .unwrap();
    let w = r.work.expect("the arXiv record");
    assert_eq!(w.title, "A Paper From Yesterday Nobody Has Indexed");
    assert_eq!(w.ids.arxiv.as_deref(), Some("2609.01234"));
    assert!(
        r.notes.iter().any(|n| n.contains("may not be indexed yet")),
        "{:?}",
        r.notes
    );
    // No published DOI anywhere: Crossref was asked by title + first author.
    assert!(
        net.requests()
            .iter()
            .any(|u| u.contains("query.bibliographic=A+Paper+From+Yesterday")
                && u.contains("query.author=lovelace"))
    );
}

/// Synthetic 429, status form (what ReplayNet and raw clients return).
#[tokio::test]
async fn openalex_429_status_benches_it_until_utc_midnight() {
    let net = ScriptNet::new(|url| {
        if url.contains("api.openalex.org") {
            (
                Duration::from_millis(5),
                429,
                "{\"error\":\"Too Many Requests\"}".into(),
            )
        } else {
            (Duration::from_millis(5), 404, String::new())
        }
    });
    let store = MemStore::new();
    let core = core_with(Vec::new(), net.clone(), store.clone(), config(&[]));
    let r = core.resolve(RRF_DOI, &cx()).await.unwrap();
    assert!(
        r.notes
            .iter()
            .any(|n| n == "openalex: rate limited (HTTP 429); benched until 2026-11-26T00:00:00Z"),
        "{:?}",
        r.notes
    );
    assert_eq!(
        core.benched_until(Provider::OpenAlex).await,
        Some(NEXT_MIDNIGHT)
    );
    let n = net.requests().len();
    core.resolve("10.1234/another", &cx()).await.unwrap();
    let after: Vec<String> = net.requests()[n..].to_vec();
    assert!(
        !after.iter().any(|u| u.contains("api.openalex.org")),
        "{after:?}"
    );
    // The bench outlives the process (api_cache) and ends at midnight UTC.
    let reborn = core_with(Vec::new(), net.clone(), store.clone(), config(&[]));
    assert_eq!(
        reborn.benched_until(Provider::OpenAlex).await,
        Some(NEXT_MIDNIGHT)
    );
    let tomorrow = core_with(Vec::new(), net, store, config(&[])).with_clock(|| NEXT_MIDNIGHT);
    assert_eq!(tomorrow.benched_until(Provider::OpenAlex).await, None);
}

/// Real fsm-net contract: `Http::fetch` populates shared `host_bucket` and
/// `rps` (manifest override or provider default); a local `Err(RateLimited)`
/// GCRA wait does NOT bench OpenAlex/S2 for 24h, while HTTP 429 status and
/// `Err(Upstream { status: Some(429) })` DO bench (B5).
#[tokio::test]
async fn http_fetch_sets_host_bucket_and_rps_and_local_gcra_never_benches() {
    let net = ScriptNet::new(|url| {
        if url.contains("api.semanticscholar.org") {
            (Duration::from_millis(5), 0, "!rate-limited".into())
        } else {
            (Duration::from_millis(5), 404, String::new())
        }
    });
    // Register a manifest-like `openalex` with custom limits to test manifest precedence.
    let mut oa_lim = fsm_core::SourceLimits::new(7.5, 10, 4, 6000, 0);
    oa_lim.host_bucket = Some("custom.openalex.bucket".into());
    let oa_src = FakeSource::new("openalex")
        .optional_key("SEARCH_MCP_OPENALEX_API_KEY")
        .limits(oa_lim)
        .arc();
    let core = core_with(
        as_sources(vec![oa_src]),
        net.clone(),
        MemStore::new(),
        config(&[]),
    );
    let r = core.resolve(RRF_DOI, &cx()).await.unwrap();
    assert!(r.work.is_none() && !r.upstream_failed);
    // Local GCRA rejection must NOT bench S2 until UTC midnight!
    assert_eq!(core.benched_until(Provider::SemanticScholar).await, None);
    assert!(
        r.notes
            .iter()
            .any(|n| n.contains("semanticscholar: local rate limit")),
        "{:?}",
        r.notes
    );
    // Check host_bucket and rps on every provider request.
    let caps = net.captured();
    let oa_req = caps
        .iter()
        .find(|c| c.redacted_url.contains("api.openalex.org"))
        .unwrap();
    assert_eq!(
        oa_req.host_bucket.as_deref(),
        Some("custom.openalex.bucket")
    );
    assert_eq!(oa_req.rps, Some(7.5));
    let s2_req = caps
        .iter()
        .find(|c| c.redacted_url.contains("api.semanticscholar.org"))
        .unwrap();
    assert_eq!(
        s2_req.host_bucket.as_deref(),
        Some("api.semanticscholar.org")
    );
    assert_eq!(s2_req.rps, Some(1.0));
    let cr_req = caps
        .iter()
        .find(|c| c.redacted_url.contains("api.crossref.org"))
        .unwrap();
    assert_eq!(cr_req.host_bucket.as_deref(), Some("api.crossref.org"));
    assert_eq!(cr_req.rps, Some(5.0));

    // Also during 800 ms enrichment: local GCRA rejection never benches S2.
    let mut w = work("Enriched Paper", Some(2022), Some("A Author"));
    w.ids.doi = Some(RRF_DOI.into());
    let arxiv = FakeSource::new("arxiv").works(vec![w]).arc();
    let core_enrich = core_with(
        as_sources(vec![arxiv]),
        net.clone(),
        MemStore::new(),
        config(&[]),
    );
    core_enrich
        .search(fsm_core::ScholarQuery::new("enriched paper"), &cx())
        .await
        .unwrap();
    assert_eq!(
        core_enrich.benched_until(Provider::SemanticScholar).await,
        None
    );

    // Upstream 429 error variant (`FsmError::Upstream { status: Some(429) }`) DOES bench.
    let net429 = ScriptNet::new(|url| {
        if url.contains("api.semanticscholar.org") {
            (Duration::from_millis(5), 0, "!upstream-429".into())
        } else {
            (Duration::from_millis(5), 404, String::new())
        }
    });
    let core429 = core_with(Vec::new(), net429, MemStore::new(), config(&[]));
    core429.resolve(RRF_DOI, &cx()).await.unwrap();
    assert_eq!(
        core429.benched_until(Provider::SemanticScholar).await,
        Some(NEXT_MIDNIGHT)
    );
    assert_eq!(core429.benched_until(Provider::Crossref).await, None);
}

/// Real `fsm_net::HttpClient` offline loopback test confirming the exact
/// contract relied on by B5: upstream HTTP 429 returns `Ok(HttpBytes { status: 429 })`,
/// while exceeding local GCRA budget returns `Err(FsmError::RateLimited)`.
#[tokio::test]
async fn real_http_client_gcra_contract_against_loopback() {
    use fsm_core::{HttpRequest, NetClient, NetCx, TransportProfile};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for status_line in ["HTTP/1.1 200 OK", "HTTP/1.1 429 Too Many Requests"] {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            let resp =
                format!("{status_line}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}");
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });

    let mut cfg = config(&[]);
    cfg.allow_private_hosts = true;
    let client = fsm_net::HttpClient::builder(&cfg).build().unwrap();
    let url = format!("http://{addr}/works");

    // Dynamic 1 rps bucket (burst 1): request 1 succeeds, request 2 within 200 ms
    // exceeds local GCRA budget -> Err(FsmError::RateLimited) without hitting server.
    let mut ncx = NetCx::new(
        fsm_core::CancellationToken::new(),
        Duration::from_millis(200),
    );
    ncx.host_bucket = Some("test.s2.bucket".into());
    ncx.rps = Some(1.0);
    let r1 = client
        .send(HttpRequest::get(&url), TransportProfile::Plain, &ncx)
        .await
        .unwrap();
    assert_eq!(r1.status, 200);
    let r2 = client
        .send(HttpRequest::get(&url), TransportProfile::Plain, &ncx)
        .await;
    assert!(
        matches!(
            r2,
            Err(fsm_core::FsmError::RateLimited {
                retry_after_s: Some(_),
                ..
            })
        ),
        "local GCRA pacing must return RateLimited, got {r2:?}"
    );

    // An actual upstream HTTP 429 (on a fresh bucket) returns Ok(HttpBytes { status: 429 }).
    let mut ncx429 = NetCx::new(fsm_core::CancellationToken::new(), Duration::from_secs(2));
    ncx429.host_bucket = Some("test.429.bucket".into());
    ncx429.rps = Some(5.0);
    let r3 = client
        .send(HttpRequest::get(&url), TransportProfile::Plain, &ncx429)
        .await
        .unwrap();
    assert_eq!(r3.status, 429);
    server.abort();
}

#[tokio::test]
async fn siblings_can_report_and_query_the_bench() {
    let net = routed(Some(openalex_rrf()), None, None, None);
    let core = core_with(Vec::new(), net.clone(), MemStore::new(), config(&[]));
    assert_eq!(core.benched_until(Provider::SemanticScholar).await, None);
    assert_eq!(
        core.note_rate_limited(Provider::SemanticScholar).await,
        Some(NEXT_MIDNIGHT)
    );
    assert_eq!(core.note_rate_limited(Provider::Crossref).await, None);
    let r = core.resolve(RRF_DOI, &cx()).await.unwrap();
    assert!(r.work.is_some());
    assert!(
        !net.requests()
            .iter()
            .any(|u| u.contains("api.semanticscholar.org"))
    );
    assert!(
        r.notes
            .iter()
            .any(|n| n == "semanticscholar: benched until 2026-11-26T00:00:00Z after HTTP 429"),
        "{:?}",
        r.notes
    );
}

#[tokio::test]
async fn keys_and_contact_email_are_sent_but_never_cached_in_keys() {
    let idconv_body = fixture_body("pmid_lancet/pmc_idconv.json");
    let oa_body = openalex_rrf();
    let net = ScriptNet::new(move |url| {
        if url.contains("api.openalex.org") {
            (Duration::from_millis(5), 200, oa_body.clone())
        } else if url.contains("pmc.ncbi.nlm.nih.gov") {
            (Duration::from_millis(5), 200, idconv_body.clone())
        } else {
            (Duration::from_millis(5), 404, String::new())
        }
    });
    let store = MemStore::new();
    let mut cfg = config(&[
        ("SEARCH_MCP_OPENALEX_API_KEY", "oa-secret"),
        ("SEARCH_MCP_SEMANTICSCHOLAR_API_KEY", "s2-secret"),
    ]);
    cfg.contact_email = Some("team@example.org".into());
    let core = core_with(Vec::new(), net.clone(), store.clone(), cfg);
    core.resolve(RRF_DOI, &cx()).await.unwrap();
    core.resolve("PMID: 32450107", &cx()).await.unwrap();

    // Raw requests carry mailto / email, while redacted request_url() omits them and api_key (M5).
    let caps = net.captured();
    let cr = caps
        .iter()
        .find(|c| c.redacted_url.contains("api.crossref.org"))
        .unwrap();
    assert!(
        cr.raw_query
            .iter()
            .any(|(k, v)| k == "mailto" && v == "team@example.org"),
        "{:?}",
        cr.raw_query
    );
    assert!(!cr.redacted_url.contains("team"), "{}", cr.redacted_url);
    assert!(!cr.redacted_url.contains("mailto"), "{}", cr.redacted_url);

    let idconv = caps
        .iter()
        .find(|c| c.redacted_url.contains("pmc.ncbi.nlm.nih.gov"))
        .unwrap();
    assert!(
        idconv
            .raw_query
            .iter()
            .any(|(k, v)| k == "email" && v == "team@example.org"),
        "{:?}",
        idconv.raw_query
    );
    assert!(
        !idconv.redacted_url.contains("team"),
        "{}",
        idconv.redacted_url
    );
    assert!(
        !idconv.redacted_url.contains("email="),
        "{}",
        idconv.redacted_url
    );

    assert!(net.requests().iter().all(|u| !u.contains("oa-secret")));

    // Cache keys match a keyless, no-email run across OpenAlex, Crossref, and PMC idconv.
    let keys_for = |s: &MemStore, prefix: &str| -> Vec<String> {
        s.api_keys()
            .into_iter()
            .filter(|k| k.starts_with(prefix))
            .collect()
    };
    let keyless = MemStore::new();
    let core2 = core_with(Vec::new(), net, keyless.clone(), config(&[]));
    core2.resolve(RRF_DOI, &cx()).await.unwrap();
    core2.resolve("PMID: 32450107", &cx()).await.unwrap();
    for prefix in [
        "scholar:v1:openalex:",
        "scholar:v1:crossref:",
        "scholar:v1:pmc_idconv:",
    ] {
        assert!(!keys_for(&store, prefix).is_empty(), "{prefix}");
        assert_eq!(
            keys_for(&store, prefix),
            keys_for(&keyless, prefix),
            "{prefix}"
        );
    }
}

#[tokio::test]
async fn by_doi_lazily_calls_idconv_and_keeps_integer_pmid_when_openalex_is_benched() {
    let cr_body = fixture_body("doi_retracted_lancet/crossref_work.json");
    let idconv_body = fixture_body("pmid_lancet/pmc_idconv.json");
    let net = ScriptNet::new(move |url| {
        if url.contains("api.crossref.org") {
            (Duration::from_millis(5), 200, cr_body.clone())
        } else if url.contains("pmc.ncbi.nlm.nih.gov") {
            (Duration::from_millis(5), 200, idconv_body.clone())
        } else {
            (Duration::from_millis(5), 404, String::new())
        }
    });
    let core = core_with(Vec::new(), net.clone(), MemStore::new(), config(&[]));
    core.note_rate_limited(Provider::OpenAlex).await;
    let w = core
        .work("10.1016/S0140-6736(20)31180-6", &cx())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(w.ids.pmid.as_deref(), Some("32450107"));
    assert_eq!(w.ids.pmcid.as_deref(), Some("PMC7255293"));
    assert!(
        net.requests()
            .iter()
            .any(|u| u.contains("pmc.ncbi.nlm.nih.gov"))
    );
}

#[tokio::test]
async fn by_arxiv_keeps_published_openalex_record_alongside_preprint_openalex() {
    let arxiv_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom" xmlns:arxiv="http://arxiv.org/schemas/atom">
  <entry>
    <id>http://arxiv.org/abs/1512.03385v1</id>
    <title>Deep Residual Learning for Image Recognition</title>
    <summary>Deeper neural networks are more difficult to train.</summary>
    <published>2015-12-10T00:00:00Z</published>
    <author><name>Kaiming He</name></author>
    <arxiv:doi>10.1109/cvpr.2016.90</arxiv:doi>
  </entry>
</feed>"#;
    let oa_preprint = r#"{
        "id": "https://openalex.org/W1111111111",
        "doi": "https://doi.org/10.48550/arXiv.1512.03385",
        "display_name": "Deep Residual Learning for Image Recognition",
        "publication_year": 2015,
        "type": "preprint",
        "cited_by_count": 12,
        "is_retracted": false,
        "primary_location": {"source": {"display_name": "arXiv"}},
        "authorships": [{"author": {"display_name": "Kaiming He"}}]
    }"#;
    let oa_published = r#"{
        "id": "https://openalex.org/W2194775991",
        "doi": "https://doi.org/10.1109/cvpr.2016.90",
        "display_name": "Deep Residual Learning for Image Recognition",
        "publication_year": 2016,
        "type": "article",
        "cited_by_count": 200000,
        "is_retracted": false,
        "primary_location": {"landing_page_url": "https://ieeexplore.ieee.org/document/7780459", "source": {"display_name": "CVPR"}},
        "authorships": [{"author": {"display_name": "Kaiming He"}}]
    }"#;
    let net = ScriptNet::new(move |url| {
        if url.contains("export.arxiv.org") {
            (Duration::from_millis(5), 200, arxiv_xml.into())
        } else if url.contains("10.48550") {
            (Duration::from_millis(5), 200, oa_preprint.into())
        } else if url.contains("api.openalex.org") && url.contains("10.1109/cvpr.2016.90") {
            (Duration::from_millis(5), 200, oa_published.into())
        } else {
            (Duration::from_millis(5), 404, String::new())
        }
    });
    let core = core_with(Vec::new(), net, MemStore::new(), config(&[]));
    let w = core.work("arXiv:1512.03385", &cx()).await.unwrap().unwrap();
    assert_eq!(w.ids.doi.as_deref(), Some("10.1109/cvpr.2016.90"));
    assert_eq!(w.ids.openalex.as_deref(), Some("W2194775991"));
    assert_eq!(w.cited_by, Some(200_000));
    assert_eq!(w.venue.as_deref(), Some("CVPR"));
}

#[tokio::test]
async fn by_arxiv_crossref_title_crosswalk_follows_published_doi_to_openalex_and_s2() {
    let arxiv_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom" xmlns:arxiv="http://arxiv.org/schemas/atom">
  <entry>
    <id>http://arxiv.org/abs/1512.03385v1</id>
    <title>Deep Residual Learning for Image Recognition</title>
    <summary>Deeper neural networks are more difficult to train.</summary>
    <published>2015-12-10T00:00:00Z</published>
    <author><name>Kaiming He</name></author>
  </entry>
</feed>"#;
    let cr_search = r#"{"message":{"items":[{
        "DOI": "10.1109/CVPR.2016.90",
        "title": ["Deep Residual Learning for Image Recognition"],
        "author": [{"given": "Kaiming", "family": "He"}],
        "issued": {"date-parts": [[2016, 6]]},
        "type": "proceedings-article",
        "container-title": ["CVPR"],
        "is-referenced-by-count": 80000
    }]}}"#;
    let oa_published = r#"{
        "id": "https://openalex.org/W2194775991",
        "doi": "https://doi.org/10.1109/cvpr.2016.90",
        "display_name": "Deep Residual Learning for Image Recognition",
        "publication_year": 2016,
        "type": "article",
        "cited_by_count": 210000,
        "is_retracted": false,
        "open_access": {"is_oa": true, "oa_status": "green", "oa_url": "https://arxiv.org/abs/1512.03385"},
        "primary_location": {"source": {"display_name": "CVPR"}}
    }"#;
    let s2_published = r#"{
        "paperId": "2c03df8b48bf3fa39054345bafabfeff15bfd11d",
        "title": "Deep Residual Learning for Image Recognition",
        "year": 2016,
        "citationCount": 220000,
        "influentialCitationCount": 25000,
        "externalIds": {"DOI": "10.1109/CVPR.2016.90", "ArXiv": "1512.03385"}
    }"#;
    let net = ScriptNet::new(move |url| {
        if url.contains("export.arxiv.org") {
            (Duration::from_millis(5), 200, arxiv_xml.into())
        } else if url.contains("api.crossref.org/works?") {
            (Duration::from_millis(5), 200, cr_search.into())
        } else if url.contains("api.openalex.org") && url.contains("10.1109/cvpr.2016.90") {
            (Duration::from_millis(5), 200, oa_published.into())
        } else if url.contains("api.semanticscholar.org")
            && url.contains("DOI:10.1109/cvpr.2016.90")
        {
            (Duration::from_millis(5), 200, s2_published.into())
        } else {
            // 404 for DataCite DOI on OpenAlex and ARXIV: on S2.
            (Duration::from_millis(5), 404, String::new())
        }
    });
    let core = core_with(Vec::new(), net, MemStore::new(), config(&[]));
    let r = core
        .resolve("10.48550/arXiv.1512.03385", &cx())
        .await
        .unwrap();
    let w = r.work.expect("resolved via crosswalk");
    assert_eq!(w.ids.doi.as_deref(), Some("10.1109/cvpr.2016.90"));
    assert_eq!(w.ids.openalex.as_deref(), Some("W2194775991"));
    assert_eq!(w.cited_by, Some(220_000));
    assert_eq!(w.influential_citations, Some(25_000));
    assert!(
        !r.notes.iter().any(|n| n.contains("may not be indexed yet")),
        "stale OpenAlex note must be cleared once follow-up succeeds: {:?}",
        r.notes
    );
}

#[tokio::test]
async fn truncated_empty_and_malformed_200_bodies_are_not_cached() {
    for (label, body) in [
        ("truncated", "!truncated"),
        ("empty", "   "),
        ("waf-html", "<html><body>Service Unavailable</body></html>"),
        ("json-null", "null"),
    ] {
        let body = body.to_owned();
        let net = ScriptNet::new(move |_| (Duration::from_millis(5), 200, body.clone()));
        let store = MemStore::new();
        let core = core_with(Vec::new(), net, store.clone(), config(&[]));
        let r = core.resolve(RRF_DOI, &cx()).await.unwrap();
        assert!(r.work.is_none() && r.upstream_failed, "{label}: {r:?}");
        assert!(
            store.api_keys().is_empty(),
            "{label}: bad 200 body must never be cached, got {:?}",
            store.api_keys()
        );
    }

    // arXiv XML with <!ENTITY declaration is rejected and not cached (R3 + I2).
    let xxe = r#"<?xml version="1.0"?><!DOCTYPE feed [<!ENTITY a "x">]><feed/>"#;
    let net = ScriptNet::new(move |url| {
        if url.contains("export.arxiv.org") {
            (Duration::from_millis(5), 200, xxe.into())
        } else {
            (Duration::from_millis(5), 404, String::new())
        }
    });
    let store = MemStore::new();
    let core = core_with(Vec::new(), net, store.clone(), config(&[]));
    let r = core.resolve("arXiv:1706.03762", &cx()).await.unwrap();
    assert!(r.work.is_none());
    assert!(
        !store
            .api_keys()
            .iter()
            .any(|k| k.starts_with("scholar:v1:arxiv:")),
        "{:?}",
        store.api_keys()
    );
}

#[tokio::test(start_paused = true)]
async fn concurrent_bench_until_startup_waits_for_persisted_bench_and_retries_errors() {
    let store = MemStore::new();
    store.api.lock().unwrap().insert(
        "scholar:bench:semanticscholar".into(),
        (NEXT_MIDNIGHT.to_string().into_bytes(), 86_400),
    );
    store.set_api_get_delay(Duration::from_millis(50));
    let core = Arc::new(core_with(
        Vec::new(),
        ScriptNet::new(|_| (Duration::from_millis(5), 404, String::new())),
        store.clone(),
        config(&[]),
    ));
    let (a, b) = tokio::join!(
        core.benched_until(Provider::SemanticScholar),
        core.benched_until(Provider::SemanticScholar),
    );
    assert_eq!(a, Some(NEXT_MIDNIGHT));
    assert_eq!(b, Some(NEXT_MIDNIGHT));
    assert_eq!(
        store
            .api_get_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "concurrent callers must share the single startup load"
    );

    // If the initial store read fails, a later call retries instead of caching a false miss.
    let store2 = MemStore::new();
    store2.api.lock().unwrap().insert(
        "scholar:bench:openalex".into(),
        (NEXT_MIDNIGHT.to_string().into_bytes(), 86_400),
    );
    store2
        .api_get_fail_next
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let core2 = core_with(
        Vec::new(),
        ScriptNet::new(|_| (Duration::from_millis(5), 404, String::new())),
        store2,
        config(&[]),
    );
    assert_eq!(core2.benched_until(Provider::OpenAlex).await, None);
    assert_eq!(
        core2.benched_until(Provider::OpenAlex).await,
        Some(NEXT_MIDNIGHT)
    );
}

#[tokio::test]
async fn by_openalex_follows_arxiv_id_for_preprint_work() {
    let oa = fixture_body("arxiv_1706.03762/openalex_datacite_doi.json");
    let ax = fixture_body("arxiv_1706.03762/arxiv_id_list.json");
    let net = ScriptNet::new(move |url| {
        if url.contains("api.openalex.org/works/W2626778328") {
            (Duration::from_millis(5), 200, oa.clone())
        } else if url.contains("export.arxiv.org") && url.contains("id_list=1706.03762") {
            (Duration::from_millis(5), 200, ax.clone())
        } else {
            (Duration::from_millis(5), 404, String::new())
        }
    });
    let core = core_with(Vec::new(), net.clone(), MemStore::new(), config(&[]));
    let w = core.work("W2626778328", &cx()).await.unwrap().unwrap();
    assert_eq!(w.title, "Attention Is All You Need");
    assert_eq!(w.ids.arxiv.as_deref(), Some("1706.03762"));
    let oa_loc = w.oa.as_ref().unwrap();
    assert_eq!(
        oa_loc.pdf_url.as_deref(),
        Some("https://arxiv.org/pdf/1706.03762v7")
    );
    assert_eq!(oa_loc.structured_format.as_deref(), Some("html5"));
    assert!(
        net.requests()
            .iter()
            .any(|u| u.contains("export.arxiv.org"))
    );
}

#[tokio::test]
async fn by_pubmed_checks_crossref_retraction_and_marks_partial_on_idconv_failure() {
    // OpenAlex has is_retracted = false, while Crossref has the retraction notice!
    let oa = fixture_body("doi_retracted_lancet/openalex_work.json")
        .replace("\"is_retracted\":true", "\"is_retracted\":false");
    let cr = fixture_body("doi_retracted_lancet/crossref_work.json");
    let net = ScriptNet::new(move |url| {
        if url.contains("pmc.ncbi.nlm.nih.gov") {
            (Duration::from_millis(5), 0, "!timeout".into())
        } else if url.contains("api.openalex.org") {
            (Duration::from_millis(5), 200, oa.clone())
        } else if url.contains("api.crossref.org") {
            (Duration::from_millis(5), 200, cr.clone())
        } else {
            (Duration::from_millis(5), 404, String::new())
        }
    });
    let store = MemStore::new();
    let core = core_with(Vec::new(), net.clone(), store.clone(), config(&[]));
    let r = core.resolve("PMID: 32450107", &cx()).await.unwrap();
    let w = r.work.expect("resolved via OpenAlex + Crossref");
    assert!(
        w.retracted,
        "Crossref retraction check must run in by_pubmed"
    );
    assert!(r.partial, "pmc_idconv failure must mark resolution partial");
    assert_eq!(
        store.scholar_puts.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "partial record must not be cached in scholar_works"
    );
    assert!(
        net.requests()
            .iter()
            .any(|u| u.contains("api.crossref.org"))
    );
}

/// Real `fsm_store::SqliteStore` integration test (offline, temp db cleaned up).
#[tokio::test]
async fn sqlite_store_end_to_end_caches_works_searches_retractions_and_bench() {
    use fsm_core::{RetractionNotice, ScholarQuery, SourceDeps, StoreApi};
    use fsm_scholar::ScholarCore;

    let path = std::env::temp_dir().join(format!(
        "fsm-scholar-test-{}-{}.sqlite",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let store = Arc::new(fsm_store::SqliteStore::open_path(&path).unwrap());
    let net = replay();
    let deps = SourceDeps::new(net.clone(), store.clone(), Arc::new(config(&[])));

    let mut reg = fsm_core::Registry::new();
    let mut w = work(
        "Graph Neural Networks Survey",
        Some(2023),
        Some("Alice Smith"),
    );
    w.ids.doi = Some("10.1234/gnn.2023".into());
    let arxiv = FakeSource::new("arxiv").works(vec![w]).arc();
    reg.register(arxiv.clone()).unwrap();
    let core = ScholarCore::new(Arc::new(reg), deps).with_clock(|| NOW);

    // 1. Complete DOI lookup persists in `scholar_works` and resolves by alias too.
    let r1 = core.resolve(RRF_DOI, &cx()).await.unwrap();
    assert!(!r1.cached && r1.work.is_some());
    let hits_after_first = net.hits().len();
    let r2 = core.resolve("W2148972377", &cx()).await.unwrap();
    assert!(
        r2.cached,
        "SqliteStore alias lookup on W-id should hit cache"
    );
    assert_eq!(net.hits().len(), hits_after_first);

    // 2. Retraction Watch table in SQLite flags cached works too.
    let mut notice = RetractionNotice::new("Withdrawal", "retraction-watch");
    notice.date = Some("2026-11-21".into());
    store
        .retractions_replace_batch(vec![(RRF_DOI.into(), notice)], None)
        .await
        .unwrap();
    let r3 = core.resolve(RRF_DOI, &cx()).await.unwrap();
    assert!(r3.cached);
    assert!(r3.work.unwrap().retracted);

    // 3. Complete keyword search caches in SQLite `api_cache` for 12h.
    let quiet = ScriptNet::new(|_| (Duration::from_millis(5), 404, String::new()));
    let mut reg2 = fsm_core::Registry::new();
    reg2.register(arxiv.clone()).unwrap();
    let search_core = ScholarCore::new(
        Arc::new(reg2),
        SourceDeps::new(quiet, store.clone(), Arc::new(config(&[]))),
    )
    .with_clock(|| NOW);
    let s1 = search_core
        .search(ScholarQuery::new("graph neural networks"), &cx())
        .await
        .unwrap();
    assert!(!s1.meta.cached);
    assert_eq!(arxiv.seen().len(), 1);
    let s2 = search_core
        .search(ScholarQuery::new("graph neural networks"), &cx())
        .await
        .unwrap();
    assert!(s2.meta.cached);
    assert_eq!(arxiv.seen().len(), 1, "served from SQLite 12h search cache");

    // 4. Persistent UTC-midnight bench survives a fresh ScholarCore instance on the same SQLite DB.
    core.note_rate_limited(Provider::SemanticScholar).await;
    let reborn = ScholarCore::new(
        Arc::new(fsm_core::Registry::new()),
        SourceDeps::new(net, store.clone(), Arc::new(config(&[]))),
    )
    .with_clock(|| NOW);
    assert_eq!(
        reborn.benched_until(Provider::SemanticScholar).await,
        Some(NEXT_MIDNIGHT)
    );

    store.close().await.unwrap();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
}
