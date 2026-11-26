//! Port of Python `tests/test_paper_graph.py` + 1.0 anti-429 & multi-source graph tests.
//!
//! Test names preserve the Python originals for coverage auditability.

mod common;

use common::{
    FnNet, MemoryStore, MockSources, json_ok, make_scholar_graph, status_resp, test_config, text_ok,
};
use fsm_core::fixture::{Exchange, Fixture, FixtureRequest, FixtureResponse, ReplayNet};
use fsm_core::{CallCx, FsmError, GraphDirection, GraphOpts, RetractionNotice};
use fsm_scholar_graph::graph::best_title_match;
use fsm_scholar_graph::ids::{
    PaperInput, TITLE_MATCH_MIN, classify_paper_input, crossref_date, notice_rank, title_score,
};
use futures::FutureExt;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tokio::sync::Barrier;

fn target_work() -> Value {
    json!({
        "id": "https://openalex.org/W2148972377",
        "doi": "https://doi.org/10.1145/1571941.1572114",
        "display_name": "Reciprocal rank fusion outperforms condorcet",
        "publication_year": 2009,
        "cited_by_count": 671,
        "is_retracted": false,
        "primary_location": {"landing_page_url": "https://dl.acm.org/doi/10.1145/1571941.1572114"},
        "referenced_works": [
            "https://openalex.org/W2117665592",
            "https://openalex.org/W2153579005",
            "https://openalex.org/W2166560380",
        ],
        "authorships": [{"author": {"display_name": "Gordon V. Cormack"}}],
    })
}

fn refs_payload() -> Value {
    json!({
        "results": [
            {
                "id": "https://openalex.org/W2117665592",
                "doi": "https://doi.org/10.1/low",
                "display_name": "Least cited reference",
                "publication_year": 2001,
                "cited_by_count": 12,
            },
            {
                "id": "https://openalex.org/W2153579005",
                "display_name": "Most cited reference",
                "publication_year": 2002,
                "cited_by_count": 3940,
                "primary_location": {"landing_page_url": "https://example.org/most"},
            },
        ]
    })
}

fn cites_payload() -> Value {
    json!({
        "results": [
            {
                "id": "https://openalex.org/W3005",
                "display_name": "Image Quality Assessment",
                "publication_year": 2020,
                "cited_by_count": 931,
                "primary_location": {"landing_page_url": "https://example.org/iqa"},
            },
            {
                "id": "https://openalex.org/W3006",
                "display_name": "A retracted follow-up",
                "publication_year": 2021,
                "cited_by_count": 4,
                "is_retracted": true,
            },
        ]
    })
}

fn crossref_ok() -> Value {
    json!({"message": {"title": ["Reciprocal rank fusion"], "updated-by": []}})
}

fn crossref_retracted() -> Value {
    json!({
        "message": {
            "title": ["Hydroxychloroquine or chloroquine"],
            "updated-by": [
                {
                    "type": "expression_of_concern",
                    "DOI": "10.1016/s0140-6736(20)31290-3",
                    "label": "Expression of concern",
                    "updated": {"date-parts": [[2020, 6, 3]]},
                },
                {
                    "type": "retraction",
                    "DOI": "10.1016/s0140-6736(20)31324-6",
                    "label": "Retraction",
                    "updated": {"date-parts": [[2020, 6, 5]]},
                },
                {
                    "type": "erratum",
                    "DOI": "10.1016/s0140-6736(20)31324-6",
                    "updated": {"date-parts": [[2020, 6, 13]]},
                },
            ],
        }
    })
}

fn arxiv_feed(title: Option<&str>) -> String {
    match title {
        Some(t) => format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>arXiv Query: id_list=1706.03762</title>
  <entry>
    <id>http://arxiv.org/abs/1706.03762v7</id>
    <title>{t}</title>
  </entry>
</feed>"#
        ),
        None => "<feed></feed>".to_owned(),
    }
}

#[derive(Clone)]
struct Routes {
    target: Option<Value>,
    search: Option<Vec<Value>>,
    refs: Option<Value>,
    cites: Option<Value>,
    crossref: Option<Value>,
    crossref_status: u16,
    arxiv_xml: Option<String>,
    arxiv_err: bool,
}

impl Default for Routes {
    fn default() -> Self {
        Self {
            target: Some(target_work()),
            search: Some(vec![target_work()]),
            refs: Some(refs_payload()),
            cites: Some(cites_payload()),
            crossref: Some(crossref_ok()),
            crossref_status: 200,
            arxiv_xml: None,
            arxiv_err: false,
        }
    }
}

fn make_routed_net(routes: Routes) -> Arc<FnNet> {
    Arc::new(FnNet::new(Box::new(move |req, _cx| {
        let routes = routes.clone();
        async move {
            let url = fsm_core::fixture::request_url(&req)
                .map_or_else(|_| req.url.clone(), |u| u.to_string());
            if url.contains("export.arxiv.org") {
                if routes.arxiv_err {
                    return Err(FsmError::upstream("arxiv", Some(503), "arxiv is down"));
                }
                let body = routes
                    .arxiv_xml
                    .unwrap_or_else(|| "<feed></feed>".to_owned());
                return Ok(text_ok(&url, "application/atom+xml", &body));
            }
            if url.contains("api.crossref.org") {
                if routes.crossref_status != 200 {
                    return Ok(status_resp(&url, routes.crossref_status, "not found"));
                }
                return match routes.crossref {
                    Some(v) => Ok(json_ok(&url, &v)),
                    None => Err(FsmError::Timeout(5000)),
                };
            }
            if url.contains("filter=cites:") || url.contains("filter=cites%3A") {
                return match routes.cites {
                    Some(v) => Ok(json_ok(&url, &v)),
                    None => Err(FsmError::Timeout(5000)),
                };
            }
            if url.contains("filter=openalex_id:") || url.contains("filter=openalex_id%3A") {
                return match routes.refs {
                    Some(v) => Ok(json_ok(&url, &v)),
                    None => Err(FsmError::Timeout(5000)),
                };
            }
            if url.contains("?search=") || url.contains("&search=") {
                let items = routes.search.unwrap_or_default();
                return Ok(json_ok(&url, &json!({ "results": items })));
            }
            match routes.target {
                Some(v) => Ok(json_ok(&url, &v)),
                None => Ok(status_resp(&url, 404, "not found")),
            }
        }
        .boxed()
    })))
}

// ---------------------------------------------------------------------------
// Identifier parsing & Crossref dates
// ---------------------------------------------------------------------------

#[test]
fn test_normalize_classifies_the_identifier() {
    let cases = [
        (
            "10.1145/1571941.1572114",
            PaperInput::Doi("10.1145/1571941.1572114".into()),
        ),
        (
            "https://doi.org/10.1145/1571941.1572114",
            PaperInput::Doi("10.1145/1571941.1572114".into()),
        ),
        (
            "doi:10.1145/1571941.1572114",
            PaperInput::Doi("10.1145/1571941.1572114".into()),
        ),
        (
            "(10.1145/1571941.1572114).",
            PaperInput::Doi("10.1145/1571941.1572114".into()),
        ),
        ("W2148972377", PaperInput::OpenAlex("W2148972377".into())),
        (
            "https://openalex.org/W2148972377",
            PaperInput::OpenAlex("W2148972377".into()),
        ),
        (
            "attention is all you need",
            PaperInput::Title("attention is all you need".into()),
        ),
        ("", PaperInput::Title(String::new())),
    ];
    for (text, expected) in cases {
        assert_eq!(classify_paper_input(text), expected, "{text}");
    }
}

#[test]
fn test_crossref_date_truncates_at_the_first_gap() {
    assert_eq!(
        crossref_date(Some(&json!({"date-parts": [[2020, 6, 5]]}))).as_deref(),
        Some("2020-06-05")
    );
    assert_eq!(
        crossref_date(Some(&json!({"date-parts": [[2020, 6]]}))).as_deref(),
        Some("2020-06")
    );
    assert_eq!(
        crossref_date(Some(&json!({"date-parts": [[2020]]}))).as_deref(),
        Some("2020")
    );
    assert_eq!(
        crossref_date(Some(&json!({"date-parts": [[2020, null, 5]]}))).as_deref(),
        Some("2020")
    );
    assert_eq!(crossref_date(Some(&json!({}))), None);
    assert_eq!(crossref_date(None), None);
}

// ---------------------------------------------------------------------------
// Title similarity gate
// ---------------------------------------------------------------------------

#[test]
fn test_title_score_separates_the_paper_from_its_neighbours() {
    let cases = [
        (
            "attention is all you need",
            "Attention Is All You Need",
            true,
        ),
        (
            "reciprocal rank fusion outperforms condorcet",
            "Reciprocal rank fusion outperforms condorcet and individual rank learning methods",
            true,
        ),
        (
            "attention is all you need",
            "Channel Attention Is All You Need for Video Frame Interpolation",
            false,
        ),
        (
            "BERT pre-training of deep bidirectional transformers",
            "BEiT: BERT Pre-Training of Image Transformers",
            false,
        ),
        (
            "BERT pre-training of deep bidirectional transformers",
            "BioBERT: a pre-trained biomedical language representation model",
            false,
        ),
    ];
    for (q, title, confident) in cases {
        let score = title_score(q, title);
        assert_eq!(
            score >= TITLE_MATCH_MIN,
            confident,
            "q={q:?} title={title:?} score={score}"
        );
    }
}

#[test]
fn test_title_match_follows_relevance_order_not_the_highest_score() {
    let items = vec![
        json!({"display_name": "Attention Is All You Need"}),
        json!({"display_name": "Attention Is All You Need In Speech Separation"}),
    ];
    let picked = best_title_match("attention is all you need", &items).unwrap();
    assert_eq!(picked["display_name"], "Attention Is All You Need");
}

#[test]
fn test_no_candidate_clearing_the_gate_resolves_to_nothing() {
    let items = vec![
        json!({"display_name": "BioBERT: a pre-trained biomedical language representation model"}),
        json!({"display_name": "Sentence-BERT: Sentence Embeddings using Siamese Networks"}),
        json!({"display_name": "BEiT: BERT Pre-Training of Image Transformers"}),
    ];
    assert!(best_title_match("BERT pre-training of deep bidirectional", &items).is_none());
}

// ---------------------------------------------------------------------------
// Traversal & verification
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_resolves_a_doi_against_the_works_endpoint() {
    let tmp = tempfile::tempdir().unwrap();
    let net = make_routed_net(Routes::default());
    let sg = make_scholar_graph(
        net.clone(),
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let out = sg
        .graph(
            "10.1145/1571941.1572114",
            GraphOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert_eq!(out.resolved_as, "doi");
    assert_eq!(out.paper.ids.openalex.as_deref(), Some("W2148972377"));
    assert!(
        net.urls()[0]
            .starts_with("https://api.openalex.org/works/https://doi.org/10.1145/1571941.1572114"),
        "{:?}",
        net.urls()
    );
}

#[tokio::test]
async fn test_resolves_a_title_through_search() {
    let tmp = tempfile::tempdir().unwrap();
    let net = make_routed_net(Routes::default());
    let sg = make_scholar_graph(
        net.clone(),
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let out = sg
        .graph(
            "Reciprocal rank fusion outperforms condorcet",
            GraphOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert_eq!(out.resolved_as, "title");
    assert!(
        net.urls()[0].contains("search=Reciprocal+rank+fusion"),
        "{}",
        net.urls()[0]
    );
    assert!(out.paper.title.starts_with("Reciprocal rank fusion"));
}

#[tokio::test]
async fn test_a_rejected_title_lists_the_near_misses() {
    let tmp = tempfile::tempdir().unwrap();
    let routes = Routes {
        search: Some(vec![json!({
            "id": "https://openalex.org/W1",
            "display_name": "BEiT: BERT Pre-Training of Image Transformers",
        })]),
        ..Routes::default()
    };
    let sg = make_scholar_graph(
        make_routed_net(routes),
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let out = sg
        .graph(
            "BERT pre-training of deep bidirectional transformers",
            GraphOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(out.paper.title.is_empty());
    assert!(
        out.notes.iter().any(|n| n.contains("Closest titles found")),
        "{:?}",
        out.notes
    );
    assert!(
        out.notes.iter().any(|n| n.contains("BEiT")),
        "{:?}",
        out.notes
    );
}

#[tokio::test]
async fn test_a_doi_lookup_is_never_gated_on_similarity() {
    let tmp = tempfile::tempdir().unwrap();
    let sg = make_scholar_graph(
        make_routed_net(Routes::default()),
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let out = sg
        .graph(
            "10.1145/1571941.1572114",
            GraphOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert_eq!(out.paper.ids.openalex.as_deref(), Some("W2148972377"));
}

#[tokio::test]
async fn test_references_are_restored_in_one_batched_call() {
    let tmp = tempfile::tempdir().unwrap();
    let net = make_routed_net(Routes::default());
    let sg = make_scholar_graph(
        net.clone(),
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let mut opts = GraphOpts::default();
    opts.direction = GraphDirection::References;
    opts.limit = 10;
    sg.graph("W2148972377", opts, &CallCx::default())
        .await
        .unwrap();
    let batched: Vec<String> = net
        .urls()
        .into_iter()
        .filter(|u| u.contains("openalex_id"))
        .collect();
    assert_eq!(batched.len(), 1, "{batched:?}");
    assert!(
        batched[0].contains("W2117665592")
            && batched[0].contains("W2153579005")
            && batched[0].contains("W2166560380"),
        "{}",
        batched[0]
    );
}

#[tokio::test]
async fn test_references_are_ordered_by_influence_not_by_id() {
    let tmp = tempfile::tempdir().unwrap();
    let sg = make_scholar_graph(
        make_routed_net(Routes::default()),
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let mut opts = GraphOpts::default();
    opts.direction = GraphDirection::References;
    let out = sg
        .graph("W2148972377", opts, &CallCx::default())
        .await
        .unwrap();
    let titles: Vec<&str> = out.references.iter().map(|r| r.title.as_str()).collect();
    assert_eq!(titles, ["Most cited reference", "Least cited reference"]);
}

#[tokio::test]
async fn test_unrestorable_references_are_reported_not_silently_dropped() {
    let tmp = tempfile::tempdir().unwrap();
    let sg = make_scholar_graph(
        make_routed_net(Routes::default()),
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let mut opts = GraphOpts::default();
    opts.direction = GraphDirection::References;
    let out = sg
        .graph("W2148972377", opts, &CallCx::default())
        .await
        .unwrap();
    assert!(
        out.notes.iter().any(|n| n.contains("of 3 references")),
        "{:?}",
        out.notes
    );
}

#[tokio::test]
async fn test_citations_use_the_cites_filter_sorted_by_citation_count() {
    let tmp = tempfile::tempdir().unwrap();
    let net = make_routed_net(Routes::default());
    let sg = make_scholar_graph(
        net.clone(),
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let mut opts = GraphOpts::default();
    opts.direction = GraphDirection::Citations;
    opts.limit = 5;
    sg.graph("W2148972377", opts, &CallCx::default())
        .await
        .unwrap();
    let cited: Vec<String> = net
        .urls()
        .into_iter()
        .filter(|u| u.contains("cites"))
        .collect();
    assert_eq!(cited.len(), 1);
    assert!(cited[0].contains("W2148972377"), "{}", cited[0]);
    assert!(cited[0].contains("cited_by_count"), "{}", cited[0]);
    assert!(cited[0].contains("per-page=5"), "{}", cited[0]);
}

#[tokio::test]
async fn test_direction_skips_the_half_that_was_not_asked_for() {
    let tmp = tempfile::tempdir().unwrap();
    let net = make_routed_net(Routes::default());
    let sg = make_scholar_graph(
        net.clone(),
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let mut opts = GraphOpts::default();
    opts.direction = GraphDirection::References;
    sg.graph("W2148972377", opts, &CallCx::default())
        .await
        .unwrap();
    assert!(!net.urls().iter().any(|u| u.contains("cites")));

    net.clear();
    let mut opts = GraphOpts::default();
    opts.direction = GraphDirection::Citations;
    sg.graph("W2148972377", opts, &CallCx::default())
        .await
        .unwrap();
    assert!(!net.urls().iter().any(|u| u.contains("openalex_id")));
}

#[tokio::test]
async fn test_a_full_walk_costs_four_requests_regardless_of_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let net = make_routed_net(Routes::default());
    let sg = make_scholar_graph(
        net.clone(),
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let mut opts = GraphOpts::default();
    opts.limit = 50;
    sg.graph("W2148972377", opts, &CallCx::default())
        .await
        .unwrap();
    assert_eq!(net.urls().len(), 4, "{:?}", net.urls());
}

#[tokio::test]
async fn test_independent_graph_lookups_run_concurrently() {
    let tmp = tempfile::tempdir().unwrap();
    let barrier = Arc::new(Barrier::new(3));
    let entered = Arc::new(Mutex::new(HashSet::<&'static str>::new()));

    let barrier_c = barrier.clone();
    let entered_c = entered.clone();
    let net = Arc::new(FnNet::new(move |req, _cx| {
        let barrier = barrier_c.clone();
        let entered = entered_c.clone();
        async move {
            let url = fsm_core::fixture::request_url(&req).unwrap().to_string();
            if url.contains("openalex_id") {
                entered.lock().unwrap().insert("references");
                barrier.wait().await;
                return Ok(json_ok(&url, &refs_payload()));
            }
            if url.contains("cites") {
                entered.lock().unwrap().insert("citations");
                barrier.wait().await;
                return Ok(json_ok(&url, &cites_payload()));
            }
            if url.contains("api.crossref.org") {
                entered.lock().unwrap().insert("crossref");
                barrier.wait().await;
                return Ok(json_ok(&url, &crossref_ok()));
            }
            Ok(json_ok(&url, &target_work()))
        }
        .boxed()
    }));

    let sg = make_scholar_graph(
        net.clone(),
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        sg.graph("W2148972377", GraphOpts::default(), &CallCx::default()),
    )
    .await
    .expect("all 3 concurrent lookups must meet at barrier")
    .unwrap();

    let got = entered.lock().unwrap().clone();
    assert_eq!(got, HashSet::from(["references", "citations", "crossref"]));
    assert!(!out.references.is_empty() && !out.citations.is_empty());
    assert_eq!(net.urls().len(), 4);
}

#[tokio::test]
async fn test_graph_cancellation_settles_all_inflight_lookups() {
    let tmp = tempfile::tempdir().unwrap();
    let barrier = Arc::new(Barrier::new(4)); // 3 inflight tasks + test driver
    let finished = Arc::new(Mutex::new(0usize));

    let barrier_c = barrier.clone();
    let finished_c = finished.clone();
    let net = Arc::new(FnNet::new(move |req, ncx| {
        let barrier = barrier_c.clone();
        let finished = finished_c.clone();
        async move {
            let url = fsm_core::fixture::request_url(&req).unwrap().to_string();
            if !url.contains("filter=") && !url.contains("api.crossref.org") {
                return Ok(json_ok(&url, &target_work()));
            }
            barrier.wait().await;
            ncx.cancel.cancelled().await;
            *finished.lock().unwrap() += 1;
            Err(FsmError::Timeout(0))
        }
        .boxed()
    }));

    let sg = Arc::new(make_scholar_graph(
        net,
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    ));
    let cx = Arc::new(CallCx::default());
    let sg_c = sg.clone();
    let cx_c = cx.clone();
    let handle =
        tokio::spawn(async move { sg_c.graph("W2148972377", GraphOpts::default(), &cx_c).await });

    barrier.wait().await;
    cx.cancel.cancel();
    let res = tokio::time::timeout(std::time::Duration::from_secs(2), handle)
        .await
        .expect("cancelled task must settle promptly")
        .unwrap();
    assert!(matches!(res, Err(FsmError::Timeout(0))), "{res:?}");
    assert_eq!(*finished.lock().unwrap(), 3);
}

#[tokio::test]
async fn test_limit_is_clamped_to_a_sane_range() {
    let tmp = tempfile::tempdir().unwrap();
    let net = make_routed_net(Routes::default());
    let sg = make_scholar_graph(
        net.clone(),
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let mut opts = GraphOpts::default();
    opts.limit = 99_999;
    let out = sg
        .graph("W2148972377", opts, &CallCx::default())
        .await
        .unwrap();
    assert!(out.citations.len() <= 50);

    net.clear();
    let mut opts = GraphOpts::default();
    opts.direction = GraphDirection::Citations;
    opts.limit = 0;
    sg.graph("W2148972377", opts, &CallCx::default())
        .await
        .unwrap();
    let cites_url = net
        .urls()
        .into_iter()
        .find(|u| u.contains("cites"))
        .unwrap();
    assert!(cites_url.contains("per-page=1"), "{cites_url}");
}

#[tokio::test]
async fn test_neighbour_retraction_flags_survive_into_the_payload() {
    let tmp = tempfile::tempdir().unwrap();
    let sg = make_scholar_graph(
        make_routed_net(Routes::default()),
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let mut opts = GraphOpts::default();
    opts.direction = GraphDirection::Citations;
    let out = sg
        .graph("W2148972377", opts, &CallCx::default())
        .await
        .unwrap();
    let flags: Vec<bool> = out.citations.iter().map(|c| c.retracted).collect();
    assert_eq!(flags, [false, true]);
}

#[tokio::test]
async fn test_crossref_retraction_notice_marks_the_paper() {
    let tmp = tempfile::tempdir().unwrap();
    let routes = Routes {
        crossref: Some(crossref_retracted()),
        ..Routes::default()
    };
    let sg = make_scholar_graph(
        make_routed_net(routes),
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let out = sg
        .graph(
            "10.1016/S0140-6736(20)31180-6",
            GraphOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(out.paper.retracted);
    assert!(out.notes[0].starts_with("RETRACTED"), "{:?}", out.notes);
}

#[tokio::test]
async fn test_notices_are_deduplicated_by_doi_keeping_the_worst_reading() {
    let tmp = tempfile::tempdir().unwrap();
    let routes = Routes {
        crossref: Some(crossref_retracted()),
        ..Routes::default()
    };
    let sg = make_scholar_graph(
        make_routed_net(routes),
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let out = sg
        .graph(
            "10.1016/S0140-6736(20)31180-6",
            GraphOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    let kinds: Vec<&str> = out.notices.iter().map(|n| n.kind.as_str()).collect();
    assert_eq!(kinds, ["retraction", "expression_of_concern"]);
    assert_eq!(
        out.notices[0].doi.as_deref(),
        Some("10.1016/s0140-6736(20)31324-6")
    );
    assert_eq!(out.notices[0].date.as_deref(), Some("2020-06-05"));
}

#[test]
fn test_notice_severity_ordering_and_unknown_type() {
    assert!(notice_rank("retraction") < notice_rank("expression_of_concern"));
    assert!(notice_rank("expression_of_concern") < notice_rank("erratum"));
    assert!(notice_rank("something_new") > notice_rank("addendum"));
}

#[tokio::test]
async fn test_openalex_retraction_flag_alone_is_enough() {
    let tmp = tempfile::tempdir().unwrap();
    let mut retracted = target_work();
    retracted["is_retracted"] = json!(true);
    let routes = Routes {
        target: Some(retracted),
        ..Routes::default()
    };
    let sg = make_scholar_graph(
        make_routed_net(routes),
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let out = sg
        .graph("W2148972377", GraphOpts::default(), &CallCx::default())
        .await
        .unwrap();
    assert!(out.paper.retracted);
}

#[tokio::test]
async fn test_a_crossref_404_is_reported_as_a_datacite_doi_not_a_fake() {
    let tmp = tempfile::tempdir().unwrap();
    let routes = Routes {
        crossref_status: 404,
        ..Routes::default()
    };
    let sg = make_scholar_graph(
        make_routed_net(routes),
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let out = sg
        .graph(
            "10.1145/1571941.1572114",
            GraphOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(out.notes[0].contains("DataCite"), "{:?}", out.notes);
}

#[tokio::test]
async fn test_a_crossref_timeout_claims_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let routes = Routes {
        crossref: None,
        ..Routes::default()
    };
    let sg = make_scholar_graph(
        make_routed_net(routes),
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let out = sg
        .graph(
            "10.1145/1571941.1572114",
            GraphOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(
        !out.notes.iter().any(|n| n.contains("Crossref")),
        "{:?}",
        out.notes
    );
}

#[tokio::test]
async fn test_a_paper_that_resolves_to_nothing_says_so() {
    let tmp = tempfile::tempdir().unwrap();
    let routes = Routes {
        target: None,
        search: Some(vec![]),
        ..Routes::default()
    };
    let sg = make_scholar_graph(
        make_routed_net(routes),
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let out = sg
        .graph(
            "10.9999/definitely-not-real",
            GraphOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(out.paper.title.is_empty());
    assert!(out.references.is_empty() && out.citations.is_empty());
    assert!(
        out.notes[0].contains("unregistered or mistyped"),
        "{:?}",
        out.notes
    );
}

#[tokio::test]
async fn test_transport_failure_never_raises() {
    let tmp = tempfile::tempdir().unwrap();
    let net = Arc::new(FnNet::new(|_req, _cx| {
        async { Err(FsmError::upstream("openalex", Some(503), "down")) }.boxed()
    }));
    let sg = make_scholar_graph(
        net,
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let out = sg
        .graph("anything", GraphOpts::default(), &CallCx::default())
        .await
        .unwrap();
    assert!(out.paper.title.is_empty() && !out.notes.is_empty());
}

// ---------------------------------------------------------------------------
// arXiv identifier & exact-title parity
// ---------------------------------------------------------------------------

#[test]
fn test_arxiv_identifiers_are_recognised() {
    let cases = [
        (
            "10.48550/arXiv.1706.03762",
            PaperInput::Arxiv("1706.03762".into()),
        ),
        (
            "https://doi.org/10.48550/ARXIV.1706.03762",
            PaperInput::Arxiv("1706.03762".into()),
        ),
        (
            "https://arxiv.org/abs/1706.03762v7",
            PaperInput::Arxiv("1706.03762".into()),
        ),
        (
            "https://arxiv.org/pdf/1706.03762",
            PaperInput::Arxiv("1706.03762".into()),
        ),
        ("arXiv:1706.03762", PaperInput::Arxiv("1706.03762".into())),
        (
            "arXiv:hep-th/9901001v2",
            PaperInput::Arxiv("hep-th/9901001".into()),
        ),
        ("1706.03762", PaperInput::Arxiv("1706.03762".into())),
        ("2401.12345v3", PaperInput::Arxiv("2401.12345".into())),
        ("hep-th/9901001", PaperInput::Arxiv("hep-th/9901001".into())),
        (
            "GPT-4 scored 1706.0376 on it",
            PaperInput::Title("GPT-4 scored 1706.0376 on it".into()),
        ),
        (
            "Results for 1706.03762 tokens",
            PaperInput::Title("Results for 1706.03762 tokens".into()),
        ),
        (
            "10.1145/1571941.1572114",
            PaperInput::Doi("10.1145/1571941.1572114".into()),
        ),
        ("W2741809807", PaperInput::OpenAlex("W2741809807".into())),
        (
            "Attention Is All You Need",
            PaperInput::Title("Attention Is All You Need".into()),
        ),
    ];
    for (given, expected) in cases {
        assert_eq!(classify_paper_input(given), expected, "{given}");
    }
}

#[tokio::test]
async fn test_an_arxiv_doi_resolves_through_the_title() {
    let tmp = tempfile::tempdir().unwrap();
    let title = target_work()["display_name"].as_str().unwrap().to_owned();
    let routes = Routes {
        arxiv_xml: Some(arxiv_feed(Some(&title))),
        ..Routes::default()
    };
    let net = make_routed_net(routes);
    let sg = make_scholar_graph(
        net.clone(),
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let out = sg
        .graph(
            "10.48550/arXiv.1706.03762",
            GraphOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert_eq!(out.paper.title, title);
    assert_eq!(out.resolved_as, "arxiv");
    assert!(net.urls()[0].contains("id_list=1706.03762"));
    assert!(!net.urls().iter().any(|u| u.contains("10.48550")));
    assert!(
        out.notes[0].starts_with("Resolved arXiv:1706.03762 by its title"),
        "{:?}",
        out.notes
    );
    assert!(!out.references.is_empty() && !out.citations.is_empty());
}

#[tokio::test]
async fn test_an_unknown_arxiv_id_says_so() {
    let tmp = tempfile::tempdir().unwrap();
    let routes = Routes {
        arxiv_xml: Some(arxiv_feed(None)),
        ..Routes::default()
    };
    let sg = make_scholar_graph(
        make_routed_net(routes),
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let out = sg
        .graph("arXiv:9999.99999", GraphOpts::default(), &CallCx::default())
        .await
        .unwrap();
    assert!(out.paper.title.is_empty());
    assert!(out.notes[0].contains("arXiv returned no record for '9999.99999'"));
    assert!(!out.notes[0].contains("mistyped"));
}

#[tokio::test]
async fn test_a_preprint_openalex_has_not_indexed_yet_says_that() {
    let tmp = tempfile::tempdir().unwrap();
    let routes = Routes {
        arxiv_xml: Some(arxiv_feed(Some(
            "A Paper From Yesterday Nobody Has Indexed",
        ))),
        search: Some(vec![]),
        ..Routes::default()
    };
    let sg = make_scholar_graph(
        make_routed_net(routes),
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let out = sg
        .graph(
            "https://arxiv.org/abs/2609.01234",
            GraphOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(out.paper.title.is_empty());
    assert!(out.notes[0].contains("arXiv calls it 'A Paper From Yesterday Nobody Has Indexed'"));
    assert!(out.notes[0].contains("may not be indexed yet"));
}

#[tokio::test]
async fn test_a_failing_arxiv_lookup_never_raises() {
    let tmp = tempfile::tempdir().unwrap();
    let routes = Routes {
        arxiv_err: true,
        ..Routes::default()
    };
    let sg = make_scholar_graph(
        make_routed_net(routes),
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let out = sg
        .graph("arXiv:1706.03762", GraphOpts::default(), &CallCx::default())
        .await
        .unwrap();
    assert!(out.paper.title.is_empty() && !out.notes.is_empty());
}

#[tokio::test]
async fn test_among_records_with_the_exact_arxiv_title_the_most_cited_wins() {
    let tmp = tempfile::tempdir().unwrap();
    let mut redeposit = target_work();
    redeposit["id"] = json!("https://openalex.org/W1");
    redeposit["cited_by_count"] = json!(12);
    let mut canonical = target_work();
    canonical["id"] = json!("https://openalex.org/W2");
    canonical["cited_by_count"] = json!(90_000);
    let mut other = target_work();
    other["id"] = json!("https://openalex.org/W3");
    other["cited_by_count"] = json!(999_999);
    other["display_name"] = json!("Reciprocal rank fusion in speech separation");

    let title = target_work()["display_name"].as_str().unwrap().to_owned();
    let routes = Routes {
        arxiv_xml: Some(arxiv_feed(Some(&title))),
        search: Some(vec![redeposit, other, canonical]),
        ..Routes::default()
    };
    let sg = make_scholar_graph(
        make_routed_net(routes),
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let mut opts = GraphOpts::default();
    opts.direction = GraphDirection::References;
    let out = sg
        .graph("arXiv:1706.03762", opts, &CallCx::default())
        .await
        .unwrap();
    assert_eq!(out.paper.ids.openalex.as_deref(), Some("W2"));
}

#[tokio::test]
async fn test_arxiv_exact_match_normalizes_spacing_and_punctuation() {
    let tmp = tempfile::tempdir().unwrap();
    for variant in [
        "Attention  Is  All  You  Need",
        "Attention — Is — All — You — Need",
    ] {
        let mut redeposit = target_work();
        redeposit["id"] = json!("https://openalex.org/W1234");
        redeposit["cited_by_count"] = json!(1);
        redeposit["display_name"] = json!("Attention Is All You Need");
        let mut canonical = target_work();
        canonical["id"] = json!("https://openalex.org/W5678");
        canonical["cited_by_count"] = json!(1000);
        canonical["display_name"] = json!(variant);

        let routes = Routes {
            arxiv_xml: Some(arxiv_feed(Some("Attention Is All You Need"))),
            search: Some(vec![redeposit, canonical]),
            ..Routes::default()
        };
        let sg = make_scholar_graph(
            make_routed_net(routes),
            Arc::new(MemoryStore::new()),
            Arc::new(MockSources::new()),
            test_config(tmp.path(), false),
        );
        let mut opts = GraphOpts::default();
        opts.direction = GraphDirection::References;
        let out = sg
            .graph("arXiv:1706.03762", opts, &CallCx::default())
            .await
            .unwrap();
        assert_eq!(
            out.paper.ids.openalex.as_deref(),
            Some("W5678"),
            "{variant}"
        );
    }
}

#[tokio::test]
async fn test_arxiv_registry_title_does_not_resolve_to_a_prefix_only_match() {
    let tmp = tempfile::tempdir().unwrap();
    let mut wrong = target_work();
    wrong["display_name"] = json!("Attention Is All You Need In Speech Separation");
    let routes = Routes {
        arxiv_xml: Some(arxiv_feed(Some("Attention Is All You Need"))),
        search: Some(vec![wrong]),
        ..Routes::default()
    };
    let sg = make_scholar_graph(
        make_routed_net(routes),
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let out = sg
        .graph("arXiv:1706.03762", GraphOpts::default(), &CallCx::default())
        .await
        .unwrap();
    assert!(out.paper.title.is_empty());
    assert!(
        out.notes
            .iter()
            .any(|n| n.contains("Attention Is All You Need In Speech Separation"))
    );
}

#[tokio::test]
async fn test_arxiv_registry_title_does_not_pick_a_short_sequel_suffix() {
    let tmp = tempfile::tempdir().unwrap();
    let title = "BERT: Pre-training of Deep Bidirectional Transformers for Language Understanding";
    let mut canonical = target_work();
    canonical["id"] = json!("https://openalex.org/W1234");
    canonical["display_name"] = json!(title);
    canonical["cited_by_count"] = json!(100);
    let mut sequel = target_work();
    sequel["id"] = json!("https://openalex.org/W5678");
    sequel["display_name"] = json!(format!("{title} II"));
    sequel["cited_by_count"] = json!(1000);

    let routes = Routes {
        arxiv_xml: Some(arxiv_feed(Some(title))),
        search: Some(vec![sequel, canonical]),
        ..Routes::default()
    };
    let sg = make_scholar_graph(
        make_routed_net(routes),
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let out = sg
        .graph("arXiv:1706.03762", GraphOpts::default(), &CallCx::default())
        .await
        .unwrap();
    assert_eq!(out.paper.ids.openalex.as_deref(), Some("W1234"));
}

#[tokio::test]
async fn test_arxiv_exact_match_does_not_promote_a_more_cited_title_prefix() {
    let tmp = tempfile::tempdir().unwrap();
    let title = target_work()["display_name"].as_str().unwrap().to_owned();
    let mut canonical = target_work();
    canonical["id"] = json!("https://openalex.org/W1234");
    canonical["cited_by_count"] = json!(100);
    let mut different = target_work();
    different["id"] = json!("https://openalex.org/W5678");
    different["cited_by_count"] = json!(1_000_000);
    different["display_name"] = json!(format!("{title} in speech separation"));

    let routes = Routes {
        arxiv_xml: Some(arxiv_feed(Some(&title))),
        search: Some(vec![different, canonical]),
        ..Routes::default()
    };
    let sg = make_scholar_graph(
        make_routed_net(routes),
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let mut opts = GraphOpts::default();
    opts.direction = GraphDirection::References;
    let out = sg
        .graph("arXiv:1706.03762", opts, &CallCx::default())
        .await
        .unwrap();
    assert_eq!(out.paper.ids.openalex.as_deref(), Some("W1234"));
}

// ---------------------------------------------------------------------------
// 1.0 Anti-429 fallbacks, ReplayNet, and local Retraction Watch integration
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_graph_anti_429_arxiv_fallback_via_s2_and_opencitations() {
    let tmp = tempfile::tempdir().unwrap();
    let net = Arc::new(FnNet::new(|req, _cx| {
        async move {
            let url = fsm_core::fixture::request_url(&req).unwrap().to_string();
            if url.contains("export.arxiv.org") {
                return Ok(text_ok(
                    &url,
                    "application/atom+xml",
                    &arxiv_feed(Some("Attention Is All You Need")),
                ));
            }
            if url.contains("api.openalex.org") {
                return Ok(status_resp(&url, 429, "daily_budget exhausted"));
            }
            if url.contains("api.semanticscholar.org/graph/v1/paper/ARXIV:1706.03762") {
                return Ok(json_ok(
                    &url,
                    &json!({
                        "paperId": "204e3073870fae3d05bcbc2f6a8e263d9b72e776",
                        "externalIds": {"ArXiv": "1706.03762", "DOI": "10.48550/arXiv.1706.03762"},
                        "title": "Attention Is All You Need",
                        "year": 2017,
                        "venue": "NeurIPS",
                        "citationCount": 140000,
                        "authors": [{"name": "Ashish Vaswani"}],
                        "references": [
                            {"paperId": "ref1", "title": "Neural Machine Translation by Jointly Learning to Align and Translate", "year": 2014, "citationCount": 42000}
                        ],
                        "citations": [
                            {"paperId": "cit1", "title": "BERT: Pre-training of Deep Bidirectional Transformers", "year": 2019, "citationCount": 110000}
                        ]
                    }),
                ));
            }
            if url.contains("api.opencitations.net/index/v2/citations/") {
                return Ok(json_ok(
                    &url,
                    &json!([{"citing": "doi:10.18653/v1/n19-1423", "creation": "2019-06-01"}]),
                ));
            }
            if url.contains("api.opencitations.net/index/v2/references/") {
                return Ok(json_ok(&url, &json!([])));
            }
            if url.contains("api.crossref.org") {
                return Ok(status_resp(&url, 404, "not found"));
            }
            Ok(status_resp(&url, 404, "unhandled"))
        }
        .boxed()
    }));

    let sg = make_scholar_graph(
        net,
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let out = sg
        .graph("arXiv:1706.03762", GraphOpts::default(), &CallCx::default())
        .await
        .unwrap();
    assert_eq!(out.paper.title, "Attention Is All You Need");
    assert_eq!(out.paper.ids.arxiv.as_deref(), Some("1706.03762"));
    assert_eq!(out.references.len(), 1);
    assert!(
        out.citations.len() >= 2,
        "expected S2 + OpenCitations fused: {:?}",
        out.citations
    );
}

#[tokio::test]
async fn test_graph_local_retraction_watch_notice_attaches_with_reason() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::new());
    let mut rw = RetractionNotice::new("Retraction", "retraction_watch");
    rw.date = Some("2020-06-05".into());
    rw.reason = Some("Falsification/Fabrication of Data".into());
    rw.notice_doi = Some("10.1016/s0140-6736(20)31324-6".into());
    store.insert_retraction("10.1145/1571941.1572114", rw);

    let sg = make_scholar_graph(
        make_routed_net(Routes::default()),
        store,
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let out = sg
        .graph(
            "10.1145/1571941.1572114",
            GraphOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(out.paper.retracted);
    assert_eq!(
        out.paper.retraction_reason.as_deref(),
        Some("Falsification/Fabrication of Data")
    );
    assert_eq!(out.notices.len(), 1);
    assert_eq!(
        out.notices[0].doi.as_deref(),
        Some("10.1016/s0140-6736(20)31324-6")
    );
}

#[tokio::test]
async fn test_graph_replay_net_fixture_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let select_target = "id,ids,type,doi,display_name,publication_year,cited_by_count,primary_location,is_retracted,referenced_works,authorships";
    let select_node =
        "id,doi,display_name,publication_year,cited_by_count,primary_location,is_retracted";

    let mut fx = Fixture::new("paper_graph", "doi_walk", "2026-11-25T12:00:00Z");
    fx.exchanges = vec![
        Exchange::new(
            FixtureRequest::new(
                "GET",
                format!(
                    "https://api.openalex.org/works/https://doi.org/10.1145/1571941.1572114?select={select_target}"
                ),
            ),
            FixtureResponse::from_bytes(200, vec![], &serde_json::to_vec(&target_work()).unwrap()),
        ),
        Exchange::new(
            FixtureRequest::new(
                "GET",
                format!(
                    "https://api.openalex.org/works?filter=openalex_id:W2117665592|W2153579005|W2166560380&per-page=3&select={select_node}"
                ),
            ),
            FixtureResponse::from_bytes(200, vec![], &serde_json::to_vec(&refs_payload()).unwrap()),
        ),
        Exchange::new(
            FixtureRequest::new(
                "GET",
                format!(
                    "https://api.openalex.org/works?filter=cites:W2148972377&sort=cited_by_count:desc&per-page=10&select={select_node}"
                ),
            ),
            FixtureResponse::from_bytes(
                200,
                vec![],
                &serde_json::to_vec(&cites_payload()).unwrap(),
            ),
        ),
        Exchange::new(
            FixtureRequest::new(
                "GET",
                "https://api.crossref.org/works/10.1145/1571941.1572114",
            ),
            FixtureResponse::from_bytes(200, vec![], &serde_json::to_vec(&crossref_ok()).unwrap()),
        ),
    ];

    let net = Arc::new(ReplayNet::from_fixtures(vec![fx]).unwrap());
    let sg = make_scholar_graph(
        net.clone(),
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let out = sg
        .graph(
            "10.1145/1571941.1572114",
            GraphOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(net.unmatched().is_empty(), "{:?}", net.unmatched());
    assert!(net.unused().is_empty(), "{:?}", net.unused());
    assert_eq!(net.hits().len(), 4);
    assert_eq!(out.paper.ids.openalex.as_deref(), Some("W2148972377"));
    assert_eq!(out.references.len(), 2);
    assert_eq!(out.citations.len(), 2);
}

#[tokio::test]
async fn test_graph_anti_429_crossref_reference_list_and_title_fallback() {
    let tmp = tempfile::tempdir().unwrap();
    let net = Arc::new(FnNet::new(|req, _cx| {
        async move {
            let url = fsm_core::fixture::request_url(&req).unwrap().to_string();
            if url.contains("api.openalex.org") {
                return Ok(status_resp(&url, 429, "daily_budget exhausted"));
            }
            if url.contains("api.crossref.org/works?query.title=") {
                return Ok(json_ok(
                    &url,
                    &json!({
                        "message": {
                            "items": [{
                                "DOI": "10.1145/1571941.1572114",
                                "title": ["Reciprocal rank fusion outperforms condorcet"],
                                "issued": {"date-parts": [[2009]]},
                                "is-referenced-by-count": 671,
                                "author": [{"given": "Gordon V.", "family": "Cormack"}]
                            }]
                        }
                    }),
                ));
            }
            if url.contains("api.crossref.org/works/10.1145/1571941.1572114") {
                return Ok(json_ok(
                    &url,
                    &json!({
                        "message": {
                            "DOI": "10.1145/1571941.1572114",
                            "title": ["Reciprocal rank fusion outperforms condorcet"],
                            "updated-by": [],
                            "reference": [
                                {
                                    "DOI": "10.1000/cr-ref-1",
                                    "article-title": "Condorcet Fuse for Information Retrieval",
                                    "author": "Montague",
                                    "year": "2002",
                                    "journal-title": "CIKM"
                                }
                            ]
                        }
                    }),
                ));
            }
            Ok(status_resp(&url, 404, "not found"))
        }
        .boxed()
    }));

    let sg = make_scholar_graph(
        net,
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let mut opts = GraphOpts::default();
    opts.direction = GraphDirection::References;
    let out = sg
        .graph(
            "Reciprocal rank fusion outperforms condorcet",
            opts,
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        out.paper.ids.doi.as_deref(),
        Some("10.1145/1571941.1572114")
    );
    assert_eq!(out.references.len(), 1);
    assert_eq!(
        out.references[0].title,
        "Condorcet Fuse for Information Retrieval"
    );
    assert_eq!(
        out.references[0].ids.doi.as_deref(),
        Some("10.1000/cr-ref-1")
    );
}

#[tokio::test]
async fn test_graph_europepmc_edges_and_neighbor_dedup_on_pmid() {
    let tmp = tempfile::tempdir().unwrap();
    let net = Arc::new(FnNet::new(|req, _cx| {
        async move {
            let url = fsm_core::fixture::request_url(&req).unwrap().to_string();
            if url.contains("api.openalex.org/works/pmid:32450107") {
                return Ok(json_ok(
                    &url,
                    &json!({
                        "id": "https://openalex.org/W3027932593",
                        "doi": "https://doi.org/10.1016/j.demo.2020.01",
                        "display_name": "Clinical Study Example",
                        "publication_year": 2020,
                        "cited_by_count": 50,
                        "is_retracted": false,
                        "referenced_works": []
                    }),
                ));
            }
            if url.contains("api.openalex.org") {
                return Ok(status_resp(&url, 429, "rate limited"));
            }
            if url.contains("ebi.ac.uk/europepmc/webservices/rest/MED/32450107/references") {
                return Ok(json_ok(
                    &url,
                    &json!({
                        "referenceList": {
                            "reference": [{
                                "id": "29000001",
                                "doi": "10.1000/shared-ref",
                                "title": "Rich Europe PMC Reference Title",
                                "pubYear": "2018",
                                "journalAbbreviation": "BMJ",
                                "citedByCount": 88
                            }]
                        }
                    }),
                ));
            }
            if url.contains("ebi.ac.uk/europepmc/webservices/rest/MED/32450107/citations") {
                return Ok(json_ok(
                    &url,
                    &json!({
                        "citationList": {
                            "citation": [{
                                "id": "34000002",
                                "doi": "10.1000/citing-epmc",
                                "title": "Follow-up Clinical Trial",
                                "pubYear": "2021",
                                "citedByCount": 14
                            }]
                        }
                    }),
                ));
            }
            if url.contains("api.opencitations.net/index/v2/references/") {
                // Same DOI as Europe PMC, with bare DOI only: dedup must keep Europe PMC's rich title.
                return Ok(json_ok(
                    &url,
                    &json!([{"cited": "doi:10.1000/shared-ref", "creation": "2018-04-01"}]),
                ));
            }
            if url.contains("api.crossref.org") {
                return Ok(json_ok(&url, &crossref_ok()));
            }
            Ok(status_resp(&url, 404, "not found"))
        }
        .boxed()
    }));

    let sg = make_scholar_graph(
        net,
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let out = sg
        .graph("PMID:32450107", GraphOpts::default(), &CallCx::default())
        .await
        .unwrap();
    assert_eq!(out.resolved_as, "pmid");
    assert_eq!(out.references.len(), 1);
    assert_eq!(out.references[0].title, "Rich Europe PMC Reference Title");
    assert_eq!(out.references[0].cited_by, Some(88));
    assert!(out.references[0].sources.contains(&"europepmc".to_owned()));
    assert!(
        out.references[0]
            .sources
            .contains(&"opencitations".to_owned())
    );
    assert_eq!(out.citations.len(), 1);
    assert_eq!(out.citations[0].title, "Follow-up Clinical Trial");
}

#[tokio::test]
async fn test_arxiv_entry_with_published_doi_resolves_directly_via_openalex_doi() {
    let tmp = tempfile::tempdir().unwrap();
    let feed_with_doi = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom" xmlns:arxiv="http://arxiv.org/schemas/atom">
  <entry>
    <id>http://arxiv.org/abs/1706.03762v7</id>
    <title>Reciprocal rank fusion outperforms condorcet</title>
    <arxiv:doi>10.1145/1571941.1572114</arxiv:doi>
  </entry>
</feed>"#;
    let routes = Routes {
        arxiv_xml: Some(feed_with_doi.to_owned()),
        ..Routes::default()
    };
    let net = make_routed_net(routes);
    let sg = make_scholar_graph(
        net.clone(),
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let out = sg
        .graph("arXiv:1706.03762", GraphOpts::default(), &CallCx::default())
        .await
        .unwrap();
    assert_eq!(out.paper.ids.openalex.as_deref(), Some("W2148972377"));
    assert_eq!(out.paper.ids.arxiv.as_deref(), Some("1706.03762"));
    assert!(
        net.urls()
            .iter()
            .any(|u| u.contains("works/https://doi.org/10.1145/1571941.1572114"))
    );
    assert!(
        !out.notes.iter().any(|n| n.contains("by its title")),
        "M1: arXiv resolved via <arxiv:doi> must not claim title resolution: {:?}",
        out.notes
    );
    assert!(
        out.notes
            .iter()
            .any(|n| n.contains("through its published DOI 10.1145/1571941.1572114")),
        "M1: expected published DOI resolution note: {:?}",
        out.notes
    );
}

#[tokio::test]
async fn test_zero_restorable_references_reports_explicit_note() {
    let tmp = tempfile::tempdir().unwrap();
    let routes = Routes {
        refs: Some(json!({"results": []})),
        ..Routes::default()
    };
    let sg = make_scholar_graph(
        make_routed_net(routes),
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let mut opts = GraphOpts::default();
    opts.direction = GraphDirection::References;
    let out = sg
        .graph("W2148972377", opts, &CallCx::default())
        .await
        .unwrap();
    assert!(out.references.is_empty());
    assert!(
        out.notes
            .iter()
            .any(|n| n.contains("3 references are recorded but none could be restored")),
        "{:?}",
        out.notes
    );
}

// ---------------------------------------------------------------------------
// Review round 1 regression tests (B1, I6, M1, M4, M7, M10, M11)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_retraction_notice_itself_does_not_get_retracted_banner_in_graph() {
    // B1 (Missing test #1): Wakefield retraction notice `10.1016/S0140-6736(10)60175-4`
    // has `update-to` pointing at the original paper, and empty `updated-by`.
    let tmp = tempfile::tempdir().unwrap();
    let net = Arc::new(FnNet::new(|req, _cx| {
        async move {
            let url = fsm_core::fixture::request_url(&req).unwrap().to_string();
            if url.contains("api.crossref.org") {
                return Ok(json_ok(
                    &url,
                    &json!({"message": {
                        "type": "journal-article",
                        "title": ["Retraction\u{2014}Ileal-lymphoid-nodular hyperplasia, non-specific colitis, and pervasive developmental disorder in children"],
                        "DOI": "10.1016/s0140-6736(10)60175-4",
                        "update-to": [{
                            "DOI": "10.1016/s0140-6736(97)11096-0",
                            "type": "retraction",
                            "label": "Retraction",
                            "source": "retraction-watch",
                            "updated": {"date-parts": [[2010, 2, 6]]},
                            "record-id": "4036"
                        }],
                        "relation": {}
                    }}),
                ));
            }
            if url.contains("filter=cites:") {
                return Ok(json_ok(&url, &json!({"results": []})));
            }
            if url.contains("api.openalex.org") {
                return Ok(json_ok(
                    &url,
                    &json!({
                        "id": "https://openalex.org/W2000000001",
                        "doi": "https://doi.org/10.1016/s0140-6736(10)60175-4",
                        "display_name": "Retraction\u{2014}Ileal-lymphoid-nodular hyperplasia",
                        "publication_year": 2010,
                        "cited_by_count": 3,
                        "is_retracted": false,
                        "referenced_works": []
                    }),
                ));
            }
            Ok(status_resp(&url, 404, "not found"))
        }
        .boxed()
    }));

    let sg = make_scholar_graph(
        net,
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let g = sg
        .graph(
            "10.1016/S0140-6736(10)60175-4",
            GraphOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(
        !g.paper.retracted,
        "retraction notice itself is not retracted"
    );
    assert!(
        g.notices.is_empty(),
        "update-to must not populate notices: {:?}",
        g.notices
    );
    assert!(
        !g.notes.iter().any(|n| n.contains("RETRACTED")),
        "notice must not get RETRACTED banner: {:?}",
        g.notes
    );
}

#[tokio::test]
async fn test_surgisphere_original_paper_full_updated_by_and_update_to_parity() {
    // B1 / M2 / M3 (Missing test #2): Full realistic Crossref `updated-by` (with publisher +
    // retraction-watch duplicates) and decoy `update-to` on `10.1016/S0140-6736(20)31180-6`.
    // Must match Python's `updated-by`-only dedupe order: 31174-0, 31324-6, 31290-3, 31249-6, 31528-2.
    let tmp = tempfile::tempdir().unwrap();
    let crossref_surgisphere = json!({"message": {
        "DOI": "10.1016/s0140-6736(20)31180-6",
        "title": ["Hydroxychloroquine or chloroquine with or without a macrolide for treatment of COVID-19"],
        "updated-by": [
            {
                "type": "erratum",
                "DOI": "10.1016/s0140-6736(20)31528-2",
                "label": "Erratum",
                "source": "publisher",
                "updated": {"date-parts": [[2020, 7, 11]]}
            },
            {
                "type": "correction",
                "DOI": "10.1016/s0140-6736(20)31249-6",
                "label": "Correction",
                "source": "publisher",
                "updated": {"date-parts": [[2020, 6, 1]]}
            },
            {
                "type": "expression_of_concern",
                "DOI": "10.1016/s0140-6736(20)31290-3",
                "label": "Expression of concern",
                "source": "publisher",
                "updated": {"date-parts": [[2020, 6, 3]]}
            },
            {
                "type": "retraction",
                "DOI": "10.1016/s0140-6736(20)31324-6",
                "label": "Retraction",
                "source": "publisher",
                "updated": {"date-parts": [[2020, 6, 5]]}
            },
            {
                "type": "retraction",
                "DOI": "10.1016/s0140-6736(20)31324-6",
                "label": "Retraction",
                "source": "retraction-watch",
                "updated": {"date-parts": [[2020, 6, 5]]}
            },
            {
                "type": "retraction",
                "DOI": "10.1016/s0140-6736(20)31174-0",
                "label": "Retraction",
                "source": "publisher",
                "updated": {"date-parts": [[2020, 6, 4]]}
            }
        ],
        "update-to": [{
            "type": "retraction",
            "DOI": "10.1016/s0140-6736(99)99999-9",
            "label": "Retraction",
            "source": "retraction-watch",
            "updated": {"date-parts": [[2020, 6, 6]]}
        }]
    }});

    let net = Arc::new(FnNet::new(move |req, _cx| {
        let cr = crossref_surgisphere.clone();
        async move {
            let url = fsm_core::fixture::request_url(&req).unwrap().to_string();
            if url.contains("api.crossref.org") {
                return Ok(json_ok(&url, &cr));
            }
            if url.contains("filter=cites:") {
                return Ok(json_ok(&url, &json!({"results": []})));
            }
            if url.contains("api.openalex.org") {
                return Ok(json_ok(
                    &url,
                    &json!({
                        "id": "https://openalex.org/W3027932593",
                        "doi": "https://doi.org/10.1016/s0140-6736(20)31180-6",
                        "display_name": "Hydroxychloroquine or chloroquine",
                        "publication_year": 2020,
                        "cited_by_count": 0,
                        "is_retracted": true,
                        "referenced_works": []
                    }),
                ));
            }
            Ok(status_resp(&url, 404, "not found"))
        }
        .boxed()
    }));

    let sg = make_scholar_graph(
        net,
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let g = sg
        .graph(
            "10.1016/S0140-6736(20)31180-6",
            GraphOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(g.paper.retracted);
    assert_eq!(
        g.paper.retraction_reason, None,
        "M2: Crossref label must not populate retraction_reason"
    );
    let dois: Vec<&str> = g.notices.iter().filter_map(|n| n.doi.as_deref()).collect();
    assert_eq!(
        dois,
        vec![
            "10.1016/s0140-6736(20)31174-0",
            "10.1016/s0140-6736(20)31324-6",
            "10.1016/s0140-6736(20)31290-3",
            "10.1016/s0140-6736(20)31249-6",
            "10.1016/s0140-6736(20)31528-2",
        ]
    );
}

#[tokio::test]
async fn test_keyless_429_path_fuses_crossref_titles_preserves_order_and_omits_false_most_cited_note()
 {
    // I6 (Missing test #10): OpenAlex single-ID returns 200, filter= calls return 429,
    // S2 returns 429, OpenCitations returns rows, Crossref returns titled reference[].
    let tmp = tempfile::tempdir().unwrap();
    let net = Arc::new(FnNet::new(|req, _cx| {
        async move {
            let url = fsm_core::fixture::request_url(&req).unwrap().to_string();
            if url.contains("api.openalex.org/works/https://doi.org/") {
                return Ok(json_ok(
                    &url,
                    &json!({
                        "id": "https://openalex.org/W2148972377",
                        "doi": "https://doi.org/10.1145/1571941.1572114",
                        "display_name": "Reciprocal rank fusion outperforms condorcet",
                        "publication_year": 2009,
                        "cited_by_count": 671,
                        "referenced_works": ["https://openalex.org/W1", "https://openalex.org/W2"]
                    }),
                ));
            }
            if url.contains("api.openalex.org") {
                return Ok(status_resp(&url, 429, "rate limited"));
            }
            if url.contains("api.semanticscholar.org") {
                return Ok(status_resp(&url, 429, "rate limited"));
            }
            if url.contains("api.opencitations.net/index/v2/references/") {
                return Ok(json_ok(
                    &url,
                    &json!([
                        {"cited": "omid:br/1 doi:10.1000/zzz-most-cited", "creation": "2001"},
                        {"cited": "omid:br/2 doi:10.1000/aaa-obscure", "creation": "2003"}
                    ]),
                ));
            }
            if url.contains("api.opencitations.net/index/v2/citations/") {
                return Ok(json_ok(
                    &url,
                    &json!([
                        {"citing": "omid:br/3 doi:10.2000/b", "creation": "2020-01-01"},
                        {"citing": "omid:br/4 doi:10.2000/a", "creation": "2021-01-01"}
                    ]),
                ));
            }
            if url.contains("api.crossref.org") {
                return Ok(json_ok(
                    &url,
                    &json!({"message": {
                        "title": ["Reciprocal rank fusion"],
                        "updated-by": [],
                        "reference": [
                            {"DOI": "10.1000/zzz-most-cited", "article-title": "The Most Cited Reference", "year": "2001"},
                            {"DOI": "10.1000/aaa-obscure", "article-title": "An Obscure Reference", "year": "2003"}
                        ]
                    }}),
                ));
            }
            Ok(status_resp(&url, 404, ""))
        }
        .boxed()
    }));

    let sg = make_scholar_graph(
        net,
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let g = sg
        .graph(
            "10.1145/1571941.1572114",
            GraphOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    let ref_titles: Vec<&str> = g.references.iter().map(|w| w.title.as_str()).collect();
    assert_eq!(
        ref_titles,
        vec!["The Most Cited Reference", "An Obscure Reference"],
        "Crossref bibliography order and real titles must win over bare OpenCitations DOIs"
    );
    assert!(g.references.iter().all(|w| !w.title.starts_with("10.")));
    assert!(g.references[0].sources.contains(&"crossref".to_owned()));
    assert!(
        g.references[0]
            .sources
            .contains(&"opencitations".to_owned())
    );
    let cite_titles: Vec<&str> = g.citations.iter().map(|w| w.title.as_str()).collect();
    assert_eq!(
        cite_titles,
        vec!["10.2000/b", "10.2000/a"],
        "unranked citations must preserve source order instead of sorting alphabetically"
    );
    assert!(
        !g.notes.iter().any(|n| n.contains("most-cited first")),
        "unranked fallback list must not falsely claim most-cited first: {:?}",
        g.notes
    );
    assert!(
        g.notes
            .iter()
            .any(|n| n == "Showing 2 of 671 citing works."),
        "expected honest truncation note: {:?}",
        g.notes
    );
}

#[tokio::test]
async fn test_graph_doi_openalex_ids_pmid_triggers_europepmc_fallback_and_work_type() {
    // M4 (Missing test #9): `OPENALEX_SELECT_TARGET` includes `ids` and `type`, so a DOI
    // lookup populates `paper.work_type` and `paper.ids.pmid`, enabling Europe PMC fallback.
    let tmp = tempfile::tempdir().unwrap();
    let net = Arc::new(FnNet::new(|req, _cx| {
        async move {
            let url = fsm_core::fixture::request_url(&req).unwrap().to_string();
            if url.contains("api.openalex.org/works/https://doi.org/10.1016/j.demo.2020.01") {
                assert!(url.contains("ids"), "select must request ids: {url}");
                assert!(url.contains("type"), "select must request type: {url}");
                return Ok(json_ok(
                    &url,
                    &json!({
                        "id": "https://openalex.org/W3027932593",
                        "ids": {
                            "openalex": "https://openalex.org/W3027932593",
                            "doi": "https://doi.org/10.1016/j.demo.2020.01",
                            "pmid": "https://pubmed.ncbi.nlm.nih.gov/32450107"
                        },
                        "type": "review",
                        "doi": "https://doi.org/10.1016/j.demo.2020.01",
                        "display_name": "Clinical Review Example",
                        "publication_year": 2020,
                        "cited_by_count": 20,
                        "is_retracted": false,
                        "referenced_works": ["https://openalex.org/W1"]
                    }),
                ));
            }
            if url.contains("api.openalex.org") || url.contains("api.semanticscholar.org") {
                return Ok(status_resp(&url, 429, "rate limited"));
            }
            if url.contains("ebi.ac.uk/europepmc/webservices/rest/MED/32450107/references") {
                return Ok(json_ok(
                    &url,
                    &json!({"referenceList": {"reference": [{
                        "id": "29000001",
                        "doi": "10.1000/epmc-ref",
                        "title": "Europe PMC Reference via OpenAlex PMID",
                        "pubYear": "2019",
                        "citedByCount": 42
                    }]}}),
                ));
            }
            if url.contains("ebi.ac.uk/europepmc/webservices/rest/MED/32450107/citations") {
                return Ok(json_ok(
                    &url,
                    &json!({"citationList": {"citation": [{
                        "id": "34000002",
                        "doi": "10.1000/epmc-cite",
                        "title": "Europe PMC Citation via OpenAlex PMID",
                        "pubYear": "2021",
                        "citedByCount": 7
                    }]}}),
                ));
            }
            if url.contains("api.crossref.org") {
                return Ok(json_ok(&url, &crossref_ok()));
            }
            Ok(status_resp(&url, 404, "not found"))
        }
        .boxed()
    }));

    let sg = make_scholar_graph(
        net.clone(),
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let g = sg
        .graph(
            "10.1016/j.demo.2020.01",
            GraphOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert_eq!(g.paper.work_type, "review");
    assert_eq!(g.paper.ids.pmid.as_deref(), Some("32450107"));
    assert_eq!(
        g.references[0].title,
        "Europe PMC Reference via OpenAlex PMID"
    );
    assert_eq!(
        g.citations[0].title,
        "Europe PMC Citation via OpenAlex PMID"
    );
    assert!(
        net.urls()
            .iter()
            .any(|u| u.contains("europepmc/webservices/rest/MED/32450107/references"))
    );
}

#[tokio::test]
async fn test_withdrawal_and_removal_notices_flag_retracted_policy() {
    // M7: Deliberate 1.0 policy — `retraction`, `withdrawal`, and `removal` mark the work
    // retracted, whereas `expression_of_concern`, `correction`, `erratum`, `addendum`, and
    // `reinstatement` do not.
    use fsm_scholar_graph::ids::is_retraction_nature;

    for nature in [
        "retraction",
        "Retraction",
        "withdrawal",
        "Article Withdrawal",
        "removal",
    ] {
        assert!(
            is_retraction_nature(nature),
            "{nature} should count as retracted"
        );
    }
    for nature in [
        "expression_of_concern",
        "Expression of concern",
        "correction",
        "corrigendum",
        "erratum",
        "addendum",
        "Reinstatement",
    ] {
        assert!(
            !is_retraction_nature(nature),
            "{nature} must not count as retracted"
        );
    }

    let tmp = tempfile::tempdir().unwrap();
    let routes = Routes {
        crossref: Some(json!({"message": {"updated-by": [{
            "type": "withdrawal",
            "DOI": "10.1016/withdrawal-notice",
            "source": "publisher",
            "updated": {"date-parts": [[2021, 4, 1]]}
        }]}})),
        ..Routes::default()
    };
    let sg = make_scholar_graph(
        make_routed_net(routes),
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let g = sg
        .graph(
            "10.1145/1571941.1572114",
            GraphOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(g.paper.retracted);
    assert!(g.notes[0].starts_with("RETRACTED."));
}

#[tokio::test]
async fn test_graph_shares_openalex_429_bench_with_scholar_core() {
    // M10: After OpenAlex returns HTTP 429 on one `graph()` call, the shared `ScholarCore`
    // bench suppresses further OpenAlex calls on subsequent lookups.
    let tmp = tempfile::tempdir().unwrap();
    let oa_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let oa_hits_c = oa_hits.clone();
    let net = Arc::new(FnNet::new(move |req, _cx| {
        let oa_hits = oa_hits_c.clone();
        async move {
            let url = fsm_core::fixture::request_url(&req).unwrap().to_string();
            if url.contains("api.openalex.org") {
                oa_hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                return Ok(status_resp(&url, 429, "daily budget exhausted"));
            }
            if url.contains("api.crossref.org/works/10.1145/1571941.1572114") {
                return Ok(json_ok(
                    &url,
                    &json!({"message": {
                        "DOI": "10.1145/1571941.1572114",
                        "title": ["Reciprocal rank fusion"],
                        "updated-by": []
                    }}),
                ));
            }
            Ok(status_resp(&url, 404, "not found"))
        }
        .boxed()
    }));

    let sg = make_scholar_graph(
        net,
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let _g1 = sg
        .graph(
            "10.1145/1571941.1572114",
            GraphOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    let hits_after_first = oa_hits.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(hits_after_first, 1);

    let g2 = sg
        .graph(
            "10.1145/1571941.1572114",
            GraphOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert_eq!(g2.paper.title, "Reciprocal rank fusion");
    assert_eq!(
        oa_hits.load(std::sync::atomic::Ordering::SeqCst),
        hits_after_first,
        "benched OpenAlex must not be called again"
    );
}

#[tokio::test]
async fn test_unresolved_graph_matches_core_ext_w3b_sentinel() {
    // M11: Verify `fsm_core::ext::w3b::is_unresolved_graph` distinguishes unresolved
    // from resolved `CitationGraph` payloads for downstream `W0d`/`W6a` callers.
    let tmp = tempfile::tempdir().unwrap();
    let net_miss = Arc::new(FnNet::new(|req, _cx| {
        async move { Ok(status_resp(&req.url, 404, "not found")) }.boxed()
    }));
    let sg_miss = make_scholar_graph(
        net_miss,
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let unresolved = sg_miss
        .graph("10.9999/missing", GraphOpts::default(), &CallCx::default())
        .await
        .unwrap();
    assert!(fsm_core::ext::w3b::is_unresolved_graph(&unresolved));
    assert!(!unresolved.notes.is_empty());

    let sg_hit = make_scholar_graph(
        make_routed_net(Routes::default()),
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let resolved = sg_hit
        .graph(
            "10.1145/1571941.1572114",
            GraphOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(!fsm_core::ext::w3b::is_unresolved_graph(&resolved));
}

// ---------------------------------------------------------------------------
// Review round 2 regression tests (B1, I2, I5, M3, M4)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_local_gcra_rate_limited_does_not_bench_openalex_and_sets_host_buckets() {
    // B1 (Missing test #1): Local GCRA `FsmError::RateLimited` from `fsm-net` must NOT
    // poison `ScholarCore`'s UTC-midnight daily bench; outgoing `NetCx` must also carry
    // the shared host_bucket and rps matching `fsm-scholar`.
    let tmp = tempfile::tempdir().unwrap();
    let seen_buckets = Arc::new(std::sync::Mutex::new(Vec::<(
        String,
        Option<String>,
        Option<f64>,
    )>::new()));
    let seen_c = seen_buckets.clone();

    let net = Arc::new(FnNet::new(move |req, ncx| {
        let seen = seen_c.clone();
        async move {
            let url = fsm_core::fixture::request_url(&req).unwrap().to_string();
            seen.lock().unwrap().push((
                req.source_id.clone().unwrap_or_default(),
                ncx.host_bucket.clone(),
                ncx.rps,
            ));
            if url.contains("api.openalex.org") {
                return Err(FsmError::RateLimited {
                    source_id: "openalex".into(),
                    retry_after_s: Some(2),
                });
            }
            if url.contains("api.crossref.org") {
                return Ok(json_ok(
                    &url,
                    &json!({"message": {"DOI": "10.1145/1571941.1572114", "title": ["RRF"], "updated-by": []}}),
                ));
            }
            Ok(status_resp(&url, 404, "not found"))
        }
        .boxed()
    }));

    let store = Arc::new(MemoryStore::new());
    let cfg = Arc::new(test_config(tmp.path(), false));
    let deps = fsm_core::SourceDeps::new(net, store, cfg);
    let core = Arc::new(fsm_scholar::ScholarCore::new(
        Arc::new(fsm_core::Registry::new()),
        deps.clone(),
    ));
    let sg = fsm_scholar_graph::ScholarGraph::new(core.clone(), Arc::new(MockSources::new()), deps);

    let _ = sg
        .graph(
            "10.1145/1571941.1572114",
            GraphOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(
        core.benched_until(fsm_scholar::Provider::OpenAlex)
            .await
            .is_none(),
        "local FsmError::RateLimited must NOT bench OpenAlex until UTC midnight"
    );

    let report = sg
        .integrity("10.1145/1571941.1572114", &CallCx::default())
        .await
        .unwrap();
    assert!(!report.is_retracted);
    assert!(
        core.benched_until(fsm_scholar::Provider::OpenAlex)
            .await
            .is_none(),
        "integrity() local RateLimited must NOT bench OpenAlex"
    );

    let buckets = seen_buckets.lock().unwrap();
    assert!(
        buckets.iter().any(|(s, b, r)| s == "openalex"
            && b.as_deref() == Some("api.openalex.org")
            && *r == Some(5.0)),
        "expected shared OpenAlex host bucket: {buckets:?}"
    );
    assert!(
        buckets.iter().any(|(s, b, r)| s == "crossref"
            && b.as_deref() == Some("api.crossref.org")
            && *r == Some(5.0)),
        "expected shared Crossref host bucket: {buckets:?}"
    );
}

#[tokio::test]
async fn test_arxiv_atom_rejects_error_entries_dtd_entities_and_clips_title() {
    // I2 (Missing test #5): arXiv Atom error entries and R3 DTD/entity XML must be rejected,
    // and oversized titles clipped.
    use fsm_scholar_graph::graph::parse_arxiv_atom_xml;

    let error_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <entry>
    <id>https://arxiv.org/api/errors#incorrect_id_format_for_not-an-archive/9901001</id>
    <title>Error</title>
    <summary>incorrect id format</summary>
  </entry>
</feed>"#;
    assert_eq!(
        parse_arxiv_atom_xml(error_xml),
        None,
        "arXiv /api/errors entry must be rejected"
    );

    let entity_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE feed [<!ENTITY boom "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA">]>
<feed xmlns="http://www.w3.org/2005/Atom">
  <entry>
    <id>http://arxiv.org/abs/1706.03762v7</id>
    <title>&boom;</title>
  </entry>
</feed>"#;
    assert_eq!(
        parse_arxiv_atom_xml(entity_xml),
        None,
        "R3: XML with <!ENTITY / <!DOCTYPE must be rejected"
    );

    let huge_title = "A".repeat(4000);
    let huge_xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <entry>
    <id>http://arxiv.org/abs/1706.03762v7</id>
    <title>{huge_title}</title>
  </entry>
</feed>"#
    );
    let parsed = parse_arxiv_atom_xml(&huge_xml).expect("valid arXiv entry");
    assert!(
        parsed.title.chars().count() <= 1024,
        "extracted arXiv title must be bounded, got {} chars",
        parsed.title.chars().count()
    );

    // End-to-end: `graph("not-an-archive/9901001")` must NOT search OpenAlex for "Error".
    let tmp = tempfile::tempdir().unwrap();
    let net = Arc::new(FnNet::new(move |req, ncx| {
        let err_body = error_xml.to_owned();
        async move {
            let url = fsm_core::fixture::request_url(&req).unwrap().to_string();
            if url.contains("export.arxiv.org") {
                assert!(
                    req.max_bytes.unwrap_or(u64::MAX) <= 512 * 1024,
                    "R3: arXiv request must set max_bytes cap"
                );
                assert_eq!(ncx.host_bucket.as_deref(), Some("export.arxiv.org"));
                return Ok(common::text_ok(&url, "application/atom+xml", &err_body));
            }
            panic!("must not query OpenAlex after arXiv /api/errors entry: {url}");
        }
        .boxed()
    }));
    let sg = make_scholar_graph(
        net,
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let g = sg
        .graph(
            "not-an-archive/9901001",
            GraphOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(fsm_core::ext::w3b::is_unresolved_graph(&g));
    assert!(
        g.notes
            .iter()
            .any(|n| n.contains("arXiv returned no record")),
        "expected no-record diagnostic, got {:?}",
        g.notes
    );
}

#[test]
fn test_title_score_and_indel_ratio_bound_huge_untrusted_strings() {
    // I5: Two 20,000-character untrusted strings must complete rapidly due to MAX_FUZZY_CHARS cap.
    let big_a = "word ".repeat(4000);
    let big_b = format!("{}different", "word ".repeat(3999));
    let t0 = std::time::Instant::now();
    let s1 = title_score(&big_a, &big_b);
    let s2 = fsm_scholar_graph::ids::indel_ratio(&big_a, &big_b);
    let elapsed = t0.elapsed();
    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "bounded fuzzy DP took too long: {elapsed:?}"
    );
    assert!((0.0..=100.0).contains(&s1) && (0.0..=100.0).contains(&s2));
}

#[tokio::test]
async fn test_request_timeout_s_below_8s_is_honored_on_net_cx() {
    // M3: User-configured `cfg.request_timeout_s = 3.0` must NOT be bumped up to 8s.
    let tmp = tempfile::tempdir().unwrap();
    let observed_ms = Arc::new(std::sync::Mutex::new(Vec::<u128>::new()));
    let obs_c = observed_ms.clone();

    let net = Arc::new(FnNet::new(move |req, ncx| {
        let obs = obs_c.clone();
        async move {
            obs.lock().unwrap().push(ncx.timeout.as_millis());
            Ok(status_resp(&req.url, 404, "not found"))
        }
        .boxed()
    }));
    let mut cfg = test_config(tmp.path(), false);
    cfg.request_timeout_s = 3.0;
    let sg = make_scholar_graph(
        net,
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        cfg,
    );
    let _ = sg
        .graph("10.9999/missing", GraphOpts::default(), &CallCx::default())
        .await
        .unwrap();
    let times = observed_ms.lock().unwrap();
    assert!(!times.is_empty());
    assert!(
        times.iter().all(|&ms| ms <= 3000),
        "request_timeout_s=3.0 was overridden above 3000ms: {times:?}"
    );
}

#[tokio::test]
async fn test_neighbor_fallback_dedup_merges_same_pmid_with_different_title_punctuation() {
    // M4: Two DOI-less fallback neighbor records with the same PMID from S2 and Europe PMC
    // (differing only by trailing punctuation) must deduplicate into one entry.
    let tmp = tempfile::tempdir().unwrap();
    let net = Arc::new(FnNet::new(|req, _cx| {
        async move {
            let url = fsm_core::fixture::request_url(&req).unwrap().to_string();
            if url.contains("api.openalex.org/works/pmid:32450107") {
                return Ok(json_ok(
                    &url,
                    &json!({
                        "id": "https://openalex.org/W3027932593",
                        "doi": "https://doi.org/10.1016/j.demo.2020.01",
                        "display_name": "Clinical Study Example",
                        "publication_year": 2020,
                        "cited_by_count": 10,
                        "is_retracted": false,
                        "referenced_works": ["https://openalex.org/W1"]
                    }),
                ));
            }
            if url.contains("api.openalex.org") {
                return Ok(status_resp(&url, 429, "rate limited"));
            }
            if url.contains("api.semanticscholar.org") && url.contains("/references") {
                return Ok(json_ok(
                    &url,
                    &json!({"data": [{
                        "citedPaper": {
                            "paperId": "s2-ref-1",
                            "externalIds": {"PubMed": "29000001"},
                            "title": "Shared Clinical Study",
                            "year": 2018,
                            "citationCount": 55
                        }
                    }]}),
                ));
            }
            if url.contains("ebi.ac.uk/europepmc/webservices/rest/MED/32450107/references") {
                return Ok(json_ok(
                    &url,
                    &json!({"referenceList": {"reference": [{
                        "id": "29000001",
                        "title": "Shared Clinical Study.",
                        "pubYear": "2018",
                        "citedByCount": 50
                    }]}}),
                ));
            }
            if url.contains("api.crossref.org") {
                return Ok(json_ok(&url, &crossref_ok()));
            }
            Ok(status_resp(&url, 404, "not found"))
        }
        .boxed()
    }));

    let sg = make_scholar_graph(
        net,
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let mut opts = GraphOpts::default();
    opts.direction = GraphDirection::References;
    let g = sg
        .graph("PMID:32450107", opts, &CallCx::default())
        .await
        .unwrap();
    assert_eq!(
        g.references.len(),
        1,
        "DOI-less neighbors sharing PMID must dedupe: {:?}",
        g.references
    );
    assert_eq!(g.references[0].cited_by, Some(55));
    assert!(
        g.references[0]
            .sources
            .contains(&"semanticscholar".to_owned())
    );
    assert!(g.references[0].sources.contains(&"europepmc".to_owned()));
}
