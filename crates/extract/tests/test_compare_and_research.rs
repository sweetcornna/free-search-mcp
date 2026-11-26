//! Port of `tests/test_compare.py`, `tests/test_freshness_signals.py` (research),
//! and `tests/test_read_budget.py`.

mod common;

use common::{MemStore, MockDocs, MockNet, MockSearch, make_service};
use fsm_core::config::Config;
use fsm_core::{
    CallCx, FetchApi, FetchMethod, FsmError, Hit, HttpBytes, PageResult, ResearchOpts,
    SearchResponse,
};
use fsm_extract::research::compute_date_note;
use std::sync::Arc;

#[tokio::test]
async fn test_compare_urls_shape() {
    let store = Arc::new(MemStore::new());
    let mut pa = PageResult::new(
        "https://a.example",
        "A",
        "alpha body ".repeat(50),
        FetchMethod::Http,
    );
    pa.sitename = Some("a.example".into());
    pa.published_date = Some("2024-01-01".into());
    store.seed_page(pa, 10);

    let mut pb = PageResult::new(
        "https://b.example",
        "B",
        "beta body ".repeat(50),
        FetchMethod::Http,
    );
    pb.sitename = Some("b.example".into());
    store.seed_page(pb, 10);

    let svc = make_service(
        Arc::new(MockNet::new()),
        store,
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );

    let items = svc
        .compare(
            "what is the difference?",
            vec!["https://a.example".into(), "https://b.example".into()],
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].url, "https://a.example");
    assert_eq!(items[0].title.as_deref(), Some("A"));
    assert_eq!(items[0].published_date.as_deref(), Some("2024-01-01"));
    assert!(items[0].excerpt.as_deref().unwrap().contains("alpha body"));
    assert_eq!(items[0].truncated, Some(false));
    assert!(items[0].error.is_none());

    assert_eq!(items[1].url, "https://b.example");
    assert!(items[1].excerpt.as_deref().unwrap().contains("beta body"));
}

#[tokio::test]
async fn test_compare_urls_propagates_per_url_errors() {
    let net = Arc::new(MockNet::with_handler(|req, _, _| {
        if req.url.contains("a.example") {
            Ok(HttpBytes::new(
                req.url.clone(),
                200,
                vec![("content-type".into(), "text/plain".into())],
                "ok content",
            ))
        } else {
            Err(FsmError::upstream("fetch", Some(503), "boom"))
        }
    }));
    let svc = make_service(
        net,
        Arc::new(MemStore::new()),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );

    let items = svc
        .compare(
            "q",
            vec!["https://a.example".into(), "https://b.example".into()],
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert_eq!(items[0].url, "https://a.example");
    assert!(items[0].error.is_none());
    assert_eq!(items[1].url, "https://b.example");
    assert!(items[1].error.as_deref().unwrap().contains("boom"));
}

#[tokio::test]
async fn test_compare_urls_truncates_long_bodies_to_6k() {
    let big = "lorem ipsum ".repeat(2000); // ~24k chars >> 6k budget
    let store = Arc::new(MemStore::new());
    store.seed_page(
        PageResult::new("https://a.example", "A", big.clone(), FetchMethod::Http),
        5,
    );
    store.seed_page(
        PageResult::new("https://b.example", "B", big, FetchMethod::Http),
        5,
    );
    let svc = make_service(
        Arc::new(MockNet::new()),
        store,
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );
    let items = svc
        .compare(
            "q",
            vec!["https://a.example".into(), "https://b.example".into()],
            &CallCx::default(),
        )
        .await
        .unwrap();
    for item in items {
        assert_eq!(item.truncated, Some(true));
        assert!(item.excerpt.unwrap().chars().count() <= 6100);
    }
}

#[tokio::test]
async fn test_compare_preserves_fetcher_truncation() {
    let store = Arc::new(MemStore::new());
    let mut pa = PageResult::new(
        "https://a.example",
        "A",
        "partial document",
        FetchMethod::Http,
    );
    pa.truncated = true;
    store.seed_page(pa, 5);
    store.seed_page(
        PageResult::new(
            "https://b.example",
            "B",
            "complete document",
            FetchMethod::Http,
        ),
        5,
    );
    let svc = make_service(
        Arc::new(MockNet::new()),
        store,
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );
    let items = svc
        .compare(
            "q",
            vec!["https://a.example".into(), "https://b.example".into()],
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert_eq!(items[0].truncated, Some(true));
    assert_eq!(items[1].truncated, Some(false));
}

#[tokio::test]
async fn test_compare_urls_rejects_too_few_and_too_many_urls() {
    let svc = make_service(
        Arc::new(MockNet::new()),
        Arc::new(MemStore::new()),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );
    assert!(matches!(
        svc.compare("q", vec!["https://only.example".into()], &CallCx::default())
            .await,
        Err(FsmError::InvalidInput(_))
    ));
    let six: Vec<String> = (0..6).map(|i| format!("https://x{i}.example")).collect();
    assert!(matches!(
        svc.compare("q", six, &CallCx::default()).await,
        Err(FsmError::InvalidInput(_))
    ));
}

#[tokio::test]
async fn test_research_rejects_empty_query_and_clamps_depth() {
    let mut sr = SearchResponse::new("rust", "web");
    for i in 0..12 {
        sr.results.push(Hit::new(
            format!("T{i}"),
            format!("https://example.com/{i}"),
        ));
    }
    let search = Arc::new(MockSearch::new(sr));
    let net = Arc::new(MockNet::with_handler(|req, _, _| {
        Ok(HttpBytes::new(
            req.url.clone(),
            200,
            vec![("content-type".into(), "text/plain".into())],
            "body text",
        ))
    }));
    let svc = make_service(
        net,
        Arc::new(MemStore::new()),
        Config::default(),
        Arc::new(MockDocs::default()),
        search.clone(),
    );

    assert!(matches!(
        svc.research("   ", ResearchOpts::default(), &CallCx::default())
            .await,
        Err(FsmError::InvalidInput(_))
    ));

    // depth = 20 clamps to 8
    let mut opts = ResearchOpts::default();
    opts.depth = 20;
    let brief = svc
        .research("rust", opts, &CallCx::default())
        .await
        .unwrap();
    assert_eq!(brief.pages.len(), 8);

    // depth = 0 clamps to 1
    let mut opts0 = ResearchOpts::default();
    opts0.depth = 0;
    let brief0 = svc
        .research("rust", opts0, &CallCx::default())
        .await
        .unwrap();
    assert_eq!(brief0.pages.len(), 1);
}

#[tokio::test]
async fn test_research_date_note_undated_and_over_365_days() {
    let mut sr = SearchResponse::new("robomaster rules", "web");
    let mut h1 = Hit::new("Rules 2026", "https://org.example/2026");
    h1.date = Some("2026-01-09".into());
    h1.date_source = Some("structured".into());
    let h2 = Hit::new("Rules 2023", "https://org.example/2023");
    let h3 = Hit::new("Forum thread", "https://forum.example/t");
    sr.results = vec![h1, h2, h3];

    let store = Arc::new(MemStore::new());
    store.seed_page(
        PageResult::new(
            "https://org.example/2026",
            "",
            "Body 2026.",
            FetchMethod::Http,
        ),
        60,
    );
    let mut p2023 = PageResult::new(
        "https://org.example/2023",
        "",
        "Body 2023.",
        FetchMethod::Http,
    );
    p2023.published_date = Some("2023-02-01".into());
    store.seed_page(p2023, 60);
    store.seed_page(
        PageResult::new(
            "https://forum.example/t",
            "",
            "Body forum.",
            FetchMethod::Http,
        ),
        60,
    );

    let svc = make_service(
        Arc::new(MockNet::new()),
        store,
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::new(sr)),
    );

    let brief = svc
        .research(
            "robomaster rules",
            ResearchOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert_eq!(brief.pages.len(), 3);
    // Backfilled title from search hit when page title was empty.
    assert_eq!(brief.pages[0].title, "Rules 2026");
    assert_eq!(
        brief.date_note.as_deref(),
        Some(
            "1 of 3 sources carry no publish date, so their age is unknown. Do not assume they \
             are recent. Dated sources span 2023-02-01 to 2026-01-09. Where they disagree, \
             prefer the newer one and say which you used."
        )
    );
    assert!(brief.tokens_estimated > 0);
}

#[test]
fn test_compute_date_note_within_365_days_omits_spread_warning() {
    let mut h1 = Hit::new("A", "https://a.example");
    h1.date = Some("2026-01-01".into());
    let mut h2 = Hit::new("B", "https://b.example");
    h2.date = Some("2026-10-01".into());
    assert_eq!(compute_date_note(&[h1, h2], &[None, None]), None);
}
