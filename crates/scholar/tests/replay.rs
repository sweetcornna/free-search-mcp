//! Identifier resolution against recorded upstream answers
//! (`tests/fixtures/**`, recorded 2026-11-25 with curl, one request each,
//! generic User-Agent, no key, no contact address). Offline: an unrecorded
//! request fails the test through `ReplayNet::unmatched`.
//!
//! The Semantic Scholar `ARXIV:1706.03762` fixture is a REAL anonymous
//! HTTP 429 (S2 refused every keyless request that minute), which makes the
//! arXiv flow also exercise the UTC-midnight bench.

mod common;

use common::*;
use fsm_core::fixture::ReplayNet;
use fsm_core::{CallCx, FsmError, ScholarQuery};
use fsm_scholar::{Pid, Provider, ScholarCore};
use std::sync::Arc;

const RRF_DOI: &str = "10.1145/1571941.1572114";
const RRF_TITLE: &str =
    "Reciprocal rank fusion outperforms condorcet and individual rank learning methods";
const LANCET_DOI: &str = "10.1016/S0140-6736(20)31180-6";

fn cx() -> CallCx {
    CallCx::default()
}

fn replay_core() -> (ScholarCore, Arc<ReplayNet>, Arc<MemStore>) {
    let net = replay();
    let store = MemStore::new();
    let core = core_with(Vec::new(), net.clone(), store.clone(), config(&[]));
    (core, net, store)
}

fn assert_all_matched(net: &ReplayNet) {
    assert!(
        net.unmatched().is_empty(),
        "unrecorded requests: {:#?}",
        net.unmatched()
    );
}

fn hits_for(net: &ReplayNet, needle: &str) -> usize {
    net.hits().iter().filter(|h| h.url.contains(needle)).count()
}

#[tokio::test]
async fn arxiv_1706_03762_resolves_to_attention_is_all_you_need() {
    let (core, net, store) = replay_core();
    let r = core.resolve("arXiv:1706.03762", &cx()).await.unwrap();
    assert_all_matched(&net);
    assert_eq!(r.pid, Pid::Arxiv("1706.03762".into()));
    let w = r.work.expect("resolved");
    assert_eq!(w.title, "Attention Is All You Need");
    assert_eq!(w.ids.arxiv.as_deref(), Some("1706.03762"));
    assert_eq!(w.ids.openalex.as_deref(), Some("W2626778328"));
    // The DataCite DOI is the arXiv id, not a published DOI.
    assert_eq!(w.ids.doi, None);
    assert_eq!(w.year, Some(2017));
    assert_eq!(w.published_date.as_deref(), Some("2017-06-12"));
    assert_eq!(w.authors.len(), 8);
    assert_eq!(w.authors[0].name, "Ashish Vaswani");
    assert!(
        w.abstract_text
            .unwrap()
            .starts_with("The dominant sequence transduction models")
    );
    assert!(w.cited_by.unwrap() > 100_000);
    // OpenAlex's primary location is the NeurIPS proceedings page; arXiv
    // backfills PDF and html5 structured_format (B7).
    assert!(
        w.url.starts_with("https://proceedings.neurips.cc/"),
        "{}",
        w.url
    );
    let oa = w.oa.as_ref().expect("OA location preserved");
    assert_eq!(oa.status, "diamond");
    assert!(
        oa.url
            .as_deref()
            .is_some_and(|u| u.starts_with("https://proceedings.neurips.cc/"))
    );
    assert_eq!(
        oa.pdf_url.as_deref(),
        Some("https://arxiv.org/pdf/1706.03762v7")
    );
    assert_eq!(oa.structured_format.as_deref(), Some("html5"));
    // One version known (no published DOI anywhere): no version list.
    assert!(w.versions.is_empty());
    assert!(!w.retracted);
    assert_eq!(&w.sources[..2], ["openalex", "arxiv"]);
    // Crossref's bibliographic query offers only other papers: none is linked.
    assert!(
        r.notes
            .iter()
            .any(|n| n
                .starts_with("No published version titled exactly 'Attention Is All You Need'")),
        "{:?}",
        r.notes
    );
    assert!(!w.sources.iter().any(|s| s == "crossref"));
    // The real S2 429 benched S2 until the next UTC midnight, persistently.
    assert!(
        r.notes
            .iter()
            .any(|n| n.contains("benched until 2026-11-26T00:00:00Z"))
    );
    assert!(r.partial);
    assert_eq!(
        core.benched_until(Provider::SemanticScholar).await,
        Some(NEXT_MIDNIGHT)
    );
    assert_eq!(
        store
            .api_get("scholar:bench:semanticscholar")
            .map(|(b, _)| b),
        Some(NEXT_MIDNIGHT.to_string().into_bytes())
    );
    // A partial record is not written to the scholar cache.
    assert_eq!(
        store.scholar_puts.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

#[tokio::test]
async fn every_arxiv_spelling_reaches_the_same_record_and_s2_is_asked_once() {
    let (core, net, _store) = replay_core();
    for raw in [
        "arXiv:1706.03762",
        "https://arxiv.org/abs/1706.03762v7",
        "1706.03762",
        "https://doi.org/10.48550/ARXIV.1706.03762",
    ] {
        let w = core.work(raw, &cx()).await.unwrap().expect(raw);
        assert_eq!(w.title, "Attention Is All You Need", "{raw}");
        assert_eq!(w.ids.arxiv.as_deref(), Some("1706.03762"), "{raw}");
    }
    assert_all_matched(&net);
    // Benched after the first 429: S2 is not asked again today.
    assert_eq!(hits_for(&net, "api.semanticscholar.org"), 1);
    // Raw responses are cached (30 days; Crossref 7): arXiv / OpenAlex once each.
    assert_eq!(hits_for(&net, "export.arxiv.org"), 1);
    assert_eq!(hits_for(&net, "api.openalex.org"), 1);
}

/// Port of `test_an_arxiv_doi_resolves_through_the_title`: the arXiv DataCite
/// DOI is read as the arXiv id and goes through arXiv's own record.
#[tokio::test]
async fn test_an_arxiv_doi_resolves_through_the_title() {
    let (core, net, _) = replay_core();
    let r = core
        .resolve("10.48550/arXiv.1706.03762", &cx())
        .await
        .unwrap();
    assert_eq!(r.pid.kind(), "arxiv");
    assert_eq!(r.work.unwrap().title, "Attention Is All You Need");
    assert!(
        net.hits()[0].url.contains("id_list=1706.03762")
            || net
                .hits()
                .iter()
                .any(|h| h.url.contains("id_list=1706.03762"))
    );
}

/// Port of `test_resolves_a_doi_against_the_works_endpoint` and
/// `test_a_doi_lookup_is_never_gated_on_similarity`.
#[tokio::test]
async fn test_resolves_a_doi_against_the_works_endpoint() {
    let (core, net, store) = replay_core();
    let r = core.resolve(RRF_DOI, &cx()).await.unwrap();
    assert_all_matched(&net);
    assert_eq!(r.pid, Pid::Doi(RRF_DOI.into()));
    assert!(r.notes.is_empty(), "{:?}", r.notes);
    let w = r.work.unwrap();
    assert_eq!(w.title, RRF_TITLE);
    assert_eq!(w.ids.doi.as_deref(), Some(RRF_DOI));
    assert_eq!(w.ids.openalex.as_deref(), Some("W2148972377"));
    assert_eq!(
        w.ids.s2.as_deref(),
        Some("9e698010f9d8fa374e7f49f776af301dd200c548")
    );
    assert_eq!(w.ids.dblp.as_deref(), Some("conf/sigir/CormackCB09"));
    assert_eq!(w.year, Some(2009));
    assert_eq!(w.published_date.as_deref(), Some("2009-07-19"));
    assert_eq!(
        w.venue.as_deref(),
        Some(
            "Proceedings of the 32nd international ACM SIGIR conference on Research and development in information retrieval"
        )
    );
    assert_eq!(w.authors[0].name, "Gordon V. Cormack");
    // Counts: the maximum (OpenAlex 2037 > S2 1732 > Crossref 783), attributed.
    assert_eq!(
        (w.cited_by, w.cited_by_source.as_deref()),
        (Some(2037), Some("openalex"))
    );
    assert_eq!(w.influential_citations, Some(245));
    assert_eq!(w.oa.as_ref().map(|o| o.status.as_str()), Some("closed"));
    assert_eq!(w.sources, ["crossref", "openalex", "semanticscholar"]);
    assert!(
        hits_for(
            &net,
            "api.openalex.org/works/https://doi.org/10.1145/1571941.1572114"
        ) == 1
    );
    // A complete record is cached under its canonical key; the next lookup is local.
    assert_eq!(
        store.scholar_puts.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert!(
        store
            .works
            .lock()
            .unwrap()
            .contains_key("doi:10.1145/1571941.1572114")
    );
    let before = net.hits().len();
    let again = core
        .resolve("https://doi.org/10.1145/1571941.1572114", &cx())
        .await
        .unwrap();
    assert!(again.cached);
    assert_eq!(again.work.unwrap().title, RRF_TITLE);
    assert_eq!(net.hits().len(), before);
}

/// Port of `test_crossref_retraction_notice_marks_the_paper` (and a DOI with
/// parentheses survives URL building).
#[tokio::test]
async fn test_crossref_retraction_notice_marks_the_paper() {
    let (core, net, _) = replay_core();
    let w = core.work(LANCET_DOI, &cx()).await.unwrap().unwrap();
    assert_all_matched(&net);
    assert!(w.retracted);
    let reason = w.retraction_reason.unwrap();
    assert!(
        reason.to_ascii_lowercase().contains("retraction"),
        "{reason}"
    );
    assert_eq!(w.ids.doi.as_deref(), Some("10.1016/s0140-6736(20)31180-6"));
    assert_eq!(w.ids.pmid.as_deref(), Some("32450107"));
    assert_eq!(w.ids.pmcid.as_deref(), Some("PMC7255293"));
    assert_eq!(w.venue.as_deref(), Some("The Lancet"));
    assert_eq!(w.year, Some(2020));
    assert!(hits_for(&net, "/works/10.1016/s0140-6736(20)31180-6") == 1);
}

#[tokio::test]
async fn a_cached_work_still_gets_a_newly_synced_retraction_flag() {
    let net = Arc::new(ReplayNet::from_fixtures(Vec::new()).unwrap());
    let store = MemStore::new();
    let mut cached =
        fsm_core::ScholarlyWork::new(RRF_TITLE, "https://doi.org/10.1145/1571941.1572114");
    cached.ids.doi = Some(RRF_DOI.into());
    store
        .works
        .lock()
        .unwrap()
        .insert(format!("doi:{RRF_DOI}"), cached);
    let mut notice = fsm_core::RetractionNotice::new("Retraction", "retraction-watch");
    notice.date = Some("2026-11-20".into());
    store
        .retractions
        .lock()
        .unwrap()
        .insert(RRF_DOI.into(), notice);
    let core = core_with(Vec::new(), net.clone(), store, config(&[]));
    let r = core.resolve(RRF_DOI, &cx()).await.unwrap();
    assert!(r.cached);
    let w = r.work.unwrap();
    assert!(w.retracted);
    assert_eq!(
        w.retraction_reason.as_deref(),
        Some("Retraction (2026-11-20)")
    );
    assert!(
        net.hits().is_empty() && net.unmatched().is_empty(),
        "served from the cache"
    );
}

#[tokio::test]
async fn a_pmid_goes_through_pmc_idconv_to_the_doi_flow() {
    let (core, net, _) = replay_core();
    let r = core.resolve("PMID: 32450107", &cx()).await.unwrap();
    assert_all_matched(&net);
    assert_eq!(r.pid, Pid::Pmid("32450107".into()));
    let w = r.work.unwrap();
    assert!(w.retracted);
    assert_eq!(w.ids.doi.as_deref(), Some("10.1016/s0140-6736(20)31180-6"));
    assert_eq!(w.ids.pmcid.as_deref(), Some("PMC7255293"));
    assert!(net.hits()[0].url.contains("idconv"));
}

/// Port of `test_a_paper_that_resolves_to_nothing_says_so`: three registries
/// answered 404 — "not found" is established, not an error.
#[tokio::test]
async fn test_a_paper_that_resolves_to_nothing_says_so() {
    let (core, net, store) = replay_core();
    let r = core
        .resolve("10.9999/fsm-scholar-nonexistent", &cx())
        .await
        .unwrap();
    assert_all_matched(&net);
    assert!(r.work.is_none() && !r.upstream_failed);
    assert!(
        r.notes[0].contains("unregistered or mistyped"),
        "{:?}",
        r.notes
    );
    assert_eq!(
        core.work("10.9999/fsm-scholar-nonexistent", &cx())
            .await
            .unwrap(),
        None
    );
    // 404s are remembered for a day.
    assert!(
        store
            .api_keys()
            .iter()
            .any(|k| k.starts_with("scholar:v1:crossref:"))
    );
    assert!(
        store
            .api
            .lock()
            .unwrap()
            .values()
            .any(|(b, ttl)| b == b"null" && *ttl == 86_400)
    );
}

/// Port of `test_transport_failure_never_raises`: `resolve` reports, `work`
/// turns "nobody answered" into an upstream error rather than "not found".
#[tokio::test]
async fn test_transport_failure_never_raises() {
    let net = Arc::new(ReplayNet::from_fixtures(Vec::new()).unwrap());
    let core = core_with(Vec::new(), net, MemStore::new(), config(&[]));
    let r = core.resolve(RRF_DOI, &cx()).await.unwrap();
    assert!(r.work.is_none() && r.upstream_failed && !r.notes.is_empty());
    let r = core.resolve("anything", &cx()).await.unwrap();
    assert!(r.work.is_none() && r.upstream_failed);
    match core.work(RRF_DOI, &cx()).await {
        Err(FsmError::Upstream { message, .. }) => assert!(message.contains(RRF_DOI)),
        other => panic!("expected Upstream, got {other:?}"),
    }
    assert!(matches!(
        core.work("  ", &cx()).await,
        Err(FsmError::InvalidInput(_))
    ));
}

/// Port of `test_a_failing_arxiv_lookup_never_raises`.
#[tokio::test]
async fn test_a_failing_arxiv_lookup_never_raises() {
    let net = Arc::new(ReplayNet::from_fixtures(Vec::new()).unwrap());
    let core = core_with(Vec::new(), net, MemStore::new(), config(&[]));
    let r = core.resolve("arXiv:1706.03762", &cx()).await.unwrap();
    assert!(r.work.is_none() && !r.notes.is_empty());
}

#[tokio::test]
async fn a_search_whose_query_is_an_identifier_looks_it_up() {
    let (core, net, _) = replay_core();
    let res = core
        .search(ScholarQuery::new(format!("doi:{RRF_DOI}")), &cx())
        .await
        .unwrap();
    assert_all_matched(&net);
    assert_eq!(res.works.len(), 1);
    assert_eq!(res.works[0].title, RRF_TITLE);
    assert!(res.meta.notes[0].starts_with("Read the query as doi 10.1145/1571941.1572114"));
    assert_eq!(
        res.meta.sources,
        ["crossref", "openalex", "semanticscholar"]
    );
    // Filters still apply to the looked-up work.
    let mut q = ScholarQuery::new(RRF_DOI);
    q.year_from = Some(2010);
    assert!(core.search(q, &cx()).await.unwrap().works.is_empty());
}

#[tokio::test]
async fn search_by_id_errors_on_total_upstream_failure_and_validates_subscope() {
    // All upstreams fail -> search(DOI) returns Err(Upstream), not Ok(empty) (I4).
    let empty_net = Arc::new(ReplayNet::from_fixtures(Vec::new()).unwrap());
    let failing_core = core_with(Vec::new(), empty_net, MemStore::new(), config(&[]));
    assert!(matches!(
        failing_core.search(ScholarQuery::new(RRF_DOI), &cx()).await,
        Err(FsmError::Upstream { .. })
    ));

    // Invalid subscope on an identifier query returns Err(InvalidInput).
    let (core, net, _) = replay_core();
    let mut bad = ScholarQuery::new(RRF_DOI);
    bad.subscope = Some("astrology".into());
    assert!(matches!(
        core.search(bad, &cx()).await,
        Err(FsmError::InvalidInput(m)) if m.contains("astrology")
    ));
    assert!(net.hits().is_empty(), "rejected before any network call");

    // Valid short subscope is normalized in meta.subscope.
    let mut cs = ScholarQuery::new(RRF_DOI);
    cs.subscope = Some("cs".into());
    let res = core.search(cs, &cx()).await.unwrap();
    assert_eq!(res.meta.subscope.as_deref(), Some("academic.cs"));
    assert_eq!(res.works.len(), 1);
}
