//! Offline tests for 1.0 Retraction Watch lazy streaming sync (synthetic CSV
//! with the real 20-column header, cross-process `fd-lock`, `sync_state`, ETag
//! 304 refresh, temp file deletion) and `ScholarGraph::integrity` 5-source fusion.

mod common;

use common::{
    FnNet, MemoryStore, MockSources, json_ok, make_scholar_graph, status_resp, test_config,
};
use fsm_core::{CallCx, FsmError, GraphOpts, HttpBytes, RetractionNotice, SciteTallies, StoreApi};
use fsm_scholar_graph::rw_sync::{
    RW_CSV_URL, RW_LOCK_FILE, RW_MAX_CSV_BYTES, RW_REFRESH_INTERVAL_S, RW_RETRY_BACKOFF_S,
    RW_SYNC_NAME, SyncOutcome, parse_retraction_watch_csv,
};
use futures::FutureExt;
use serde_json::json;
use std::fs::OpenOptions;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Small synthetic Retraction Watch CSV using the real 20-column header row.
const SYNTHETIC_RW_CSV: &str = "\u{feff}Record ID,Title,Subject,Institution,Journal,Publisher,Country,Author,URLS,ArticleType,RetractionDate,RetractionDOI,RetractionPubMedID,OriginalPaperDate,OriginalPaperDOI,OriginalPaperPubMedID,RetractionNature,Reason,Paywalled,Notes\n\
10001,\"Hydroxychloroquine or chloroquine with or without a macrolide for treatment of COVID-19\",(HSC) Medicine,Brigham and Women's Hospital,The Lancet,Elsevier,United States,Mandeep R Mehra,https://retractionwatch.com/lancet,Research Article,6/3/2020 0:00,10.1016/S0140-6736(20)31290-3,32504547,5/22/2020 0:00,10.1016/S0140-6736(20)31180-6,32450107,Expression of concern,+Concerns/Issues About Data;,No,\n\
10002,\"Hydroxychloroquine or chloroquine with or without a macrolide for treatment of COVID-19\",(HSC) Medicine,Brigham and Women's Hospital,The Lancet,Elsevier,United States,Mandeep R Mehra,https://retractionwatch.com/lancet2,Research Article,6/5/2020 0:00,10.1016/S0140-6736(20)31324-6,32511943,5/22/2020 0:00,https://doi.org/10.1016/S0140-6736(20)31180-6,32450107,Retraction,+Falsification/Fabrication of Data;+Investigation by Third Party;,No,\n\
10003,\"No DOI Paper\",(PHY) Physics,Unknown,Some Journal,Pub,US,Anon,,Research Article,2021-01-15,unavailable,0,2020-01-01,unavailable,0,Retraction,+Plagiarism of Article;,No,\n\
10004,\"Zero DOI Paper\",(BIO) Biology,Unknown,Some Journal,Pub,US,Anon,,Research Article,2022-05-01,,0,2020-01-01,0,0,Retraction,+Error in Results;,No,\n\
10005,\"Corrected Paper\",(CS) CS,Univ,Journal of CS,Springer,Germany,Ada Lovelace,,Research Article,2023-08-19,10.1007/corr-1,0,2022-02-01,10.1007/orig-1,0,Correction,+Error in Text;,No,\n";

#[test]
fn test_parse_synthetic_rw_csv_deduplicates_by_severity_and_skips_invalid_dois() {
    let rows = parse_retraction_watch_csv(SYNTHETIC_RW_CSV.as_bytes()).unwrap();
    // 5 CSV records -> 2 invalid DOIs skipped, 2 Lancet rows merged into 1 Retraction, 1 Correction.
    assert_eq!(rows.len(), 2, "{rows:?}");

    let (lancet_doi, lancet) = rows
        .iter()
        .find(|(d, _)| d == "10.1016/s0140-6736(20)31180-6")
        .expect("lancet row");
    assert_eq!(lancet_doi, "10.1016/s0140-6736(20)31180-6");
    assert_eq!(lancet.nature, "Retraction");
    assert_eq!(lancet.date.as_deref(), Some("2020-06-05"));
    assert_eq!(
        lancet.notice_doi.as_deref(),
        Some("10.1016/s0140-6736(20)31324-6")
    );
    assert_eq!(
        lancet.reason.as_deref(),
        Some("Falsification/Fabrication of Data; Investigation by Third Party")
    );
    assert_eq!(lancet.source, "retraction_watch");

    let (_, corr) = rows
        .iter()
        .find(|(d, _)| d == "10.1007/orig-1")
        .expect("correction row");
    assert_eq!(corr.nature, "Correction");
    assert_eq!(corr.date.as_deref(), Some("2023-08-19"));
    assert_eq!(corr.reason.as_deref(), Some("Error in Text"));
}

#[tokio::test]
async fn test_rw_sync_streams_csv_stores_etag_and_deletes_temp_csv() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::new());
    let net = Arc::new(FnNet::new(|req, _cx| {
        async move {
            assert_eq!(req.url, RW_CSV_URL);
            assert!(
                req.max_bytes.unwrap_or(0) >= RW_MAX_CSV_BYTES,
                "RW CSV must override 25 MB default cap: {:?}",
                req.max_bytes
            );
            Ok(HttpBytes::new(
                req.url,
                200,
                vec![
                    (
                        "content-type".to_owned(),
                        "text/csv; charset=utf-8".to_owned(),
                    ),
                    ("etag".to_owned(), "\"rw-etag-v1\"".to_owned()),
                ],
                SYNTHETIC_RW_CSV.as_bytes().to_vec(),
            ))
        }
        .boxed()
    }));

    let sg = make_scholar_graph(
        net.clone(),
        store.clone(),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), true),
    );
    let outcome = sg.sync_retractions(&CallCx::default()).await;
    assert_eq!(
        outcome,
        SyncOutcome::Synced {
            rows: 2,
            etag: Some("\"rw-etag-v1\"".to_owned()),
        }
    );
    assert_eq!(store.retraction_count(), 2);
    assert_eq!(store.replace_batch_calls(), 1);
    let (etag, started, finished) = store.get_sync_state(RW_SYNC_NAME).unwrap();
    assert_eq!(etag.as_deref(), Some("\"rw-etag-v1\""));
    assert!(finished.unwrap() >= started);

    // Raw CSV tempfile must be deleted from cache_dir.
    let leftover_csv: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".csv"))
        .collect();
    assert!(
        leftover_csv.is_empty(),
        "temp CSV was not deleted: {leftover_csv:?}"
    );

    // Second call within 7-day freshness window issues zero HTTP requests.
    let second = sg.sync_retractions(&CallCx::default()).await;
    assert_eq!(second, SyncOutcome::Fresh);
    assert_eq!(net.requests().len(), 1);
}

#[tokio::test]
async fn test_rw_sync_sends_if_none_match_and_handles_304_not_modified() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::new());
    let stale = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        - (RW_REFRESH_INTERVAL_S + 3600);
    store.set_sync_state(
        RW_SYNC_NAME,
        Some("\"rw-etag-old\"".into()),
        stale,
        Some(stale),
    );

    let net = Arc::new(FnNet::new(|req, _cx| {
        async move {
            let inm = req
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("if-none-match"))
                .map(|(_, v)| v.as_str());
            assert_eq!(inm, Some("\"rw-etag-old\""));
            Ok(status_resp(&req.url, 304, ""))
        }
        .boxed()
    }));

    let sg = make_scholar_graph(
        net.clone(),
        store.clone(),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), true),
    );
    let outcome = sg.sync_retractions(&CallCx::default()).await;
    assert_eq!(outcome, SyncOutcome::NotModified);
    assert_eq!(store.replace_batch_calls(), 0);
    let (etag, _, finished) = store.get_sync_state(RW_SYNC_NAME).unwrap();
    assert_eq!(etag.as_deref(), Some("\"rw-etag-old\""));
    assert!(finished.unwrap() > stale);
}

#[tokio::test]
async fn test_rw_sync_disabled_when_offline_sync_false() {
    let tmp = tempfile::tempdir().unwrap();
    let net = Arc::new(FnNet::new(|req, _cx| {
        async move {
            panic!(
                "unexpected HTTP request when offline_sync=false: {}",
                req.url
            )
        }
        .boxed()
    }));
    let sg = make_scholar_graph(
        net,
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    assert_eq!(
        sg.sync_retractions(&CallCx::default()).await,
        SyncOutcome::Disabled
    );
    assert!(!tmp.path().join(RW_LOCK_FILE).exists());
}

#[tokio::test]
async fn test_rw_sync_skips_when_fd_lock_held_and_integrity_uses_crossref_fallback() {
    let tmp = tempfile::tempdir().unwrap();
    let lock_path = tmp.path().join(RW_LOCK_FILE);
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    let mut external_lock = fd_lock::RwLock::new(file);
    let _held_guard = external_lock.try_write().expect("acquire external lock");

    let net = Arc::new(FnNet::new(|req, _cx| {
        async move {
            let url = fsm_core::fixture::request_url(&req).unwrap().to_string();
            assert!(
                !url.contains("retraction_watch.csv"),
                "must skip CSV download while lock is held"
            );
            if url.contains("api.crossref.org") {
                return Ok(json_ok(
                    &url,
                    &json!({
                        "message": {
                            "updated-by": [{
                                "type": "retraction",
                                "DOI": "10.1016/s0140-6736(20)31324-6",
                                "label": "Retraction",
                                "source": "publisher",
                                "updated": {"date-parts": [[2020, 6, 5]]},
                            }]
                        }
                    }),
                ));
            }
            if url.contains("api.openalex.org") {
                return Ok(json_ok(&url, &json!({"is_retracted": true, "fwci": 2.5})));
            }
            Ok(status_resp(&url, 404, "not found"))
        }
        .boxed()
    }));

    let sg = make_scholar_graph(
        net,
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), true),
    );

    // Direct sync call skips immediately without blocking.
    assert_eq!(
        sg.sync_retractions(&CallCx::default()).await,
        SyncOutcome::SkippedBusy
    );

    // `integrity()` still detects the retraction via Crossref + OpenAlex fallback.
    let report = sg
        .integrity("10.1016/S0140-6736(20)31180-6", &CallCx::default())
        .await
        .unwrap();
    // Let the detached background sync observe the held lock and exit cleanly before dropping it.
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(report.is_retracted);
    assert_eq!(report.retraction_notices.len(), 1);
    assert_eq!(report.retraction_notices[0].source, "crossref");
    assert_eq!(report.retraction_notices[0].reason, None);
    assert_eq!(report.openalex_fwci, Some(2.5));
}

#[tokio::test]
async fn test_integrity_fuses_rw_crossref_openalex_scite_and_doaj() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::new());
    let sources = Arc::new(MockSources::new());
    sources.on_ok(
        "scite.tallies",
        vec![json!({
            "doi": "10.1016/s0140-6736(20)31180-6",
            "supporting": 3,
            "contradicting": 19,
            "mentioning": 412,
            "total": 434,
        })],
    );

    let net = Arc::new(FnNet::new(|req, _cx| {
        async move {
            let url = fsm_core::fixture::request_url(&req).unwrap().to_string();
            if url == RW_CSV_URL {
                return Ok(HttpBytes::new(
                    url,
                    200,
                    vec![("etag".to_owned(), "\"etag-1\"".to_owned())],
                    SYNTHETIC_RW_CSV.as_bytes().to_vec(),
                ));
            }
            if url.contains("api.crossref.org") {
                return Ok(json_ok(
                    &url,
                    &json!({
                        "message": {
                            "ISSN": ["0140-6736"],
                            "updated-by": [
                                {
                                    "type": "expression_of_concern",
                                    "DOI": "10.1016/s0140-6736(20)31290-3",
                                    "label": "Expression of concern",
                                    "source": "publisher",
                                    "updated": {"date-parts": [[2020, 6, 3]]},
                                },
                                {
                                    "type": "retraction",
                                    "DOI": "10.1016/s0140-6736(20)31324-6",
                                    "label": "Retraction",
                                    "source": "retraction-watch",
                                    "updated": {"date-parts": [[2020, 6, 5]]},
                                }
                            ]
                        }
                    }),
                ));
            }
            if url.contains("api.openalex.org") {
                return Ok(json_ok(
                    &url,
                    &json!({
                        "id": "https://openalex.org/W3027932593",
                        "doi": "https://doi.org/10.1016/s0140-6736(20)31180-6",
                        "is_retracted": true,
                        "fwci": 14.75,
                        "primary_location": {
                            "source": {
                                "display_name": "The Lancet",
                                "issn_l": "0140-6736",
                                "is_in_doaj": false
                            }
                        }
                    }),
                ));
            }
            if url.contains("doaj.org/api/search/journals/issn:0140-6736") {
                return Ok(json_ok(
                    &url,
                    &json!({"total": 1, "results": [{"id": "j1"}]}),
                ));
            }
            Ok(status_resp(&url, 404, "not found"))
        }
        .boxed()
    }));

    let sg = make_scholar_graph(
        net,
        store.clone(),
        sources.clone(),
        test_config(tmp.path(), true),
    );
    // Warm up local RW store explicitly since `integrity()` runs sync detached in background (B2).
    assert!(matches!(
        sg.sync_retractions(&CallCx::default()).await,
        SyncOutcome::Synced { rows: 2, .. }
    ));

    let report = sg
        .integrity(
            "https://doi.org/10.1016/S0140-6736(20)31180-6",
            &CallCx::default(),
        )
        .await
        .unwrap();

    assert_eq!(report.doi.as_deref(), Some("10.1016/s0140-6736(20)31180-6"));
    assert!(report.is_retracted);
    // Retraction notice (merged RW + Crossref) comes before Expression of concern.
    assert_eq!(report.retraction_notices.len(), 2);
    assert_eq!(
        report.retraction_notices[0].notice_doi.as_deref(),
        Some("10.1016/s0140-6736(20)31324-6")
    );
    assert_eq!(
        report.retraction_notices[0].reason.as_deref(),
        Some("Falsification/Fabrication of Data; Investigation by Third Party")
    );
    assert_eq!(report.scite, Some(SciteTallies::new(3, 19, 412)));
    assert_eq!(report.doaj_vetted, Some(true));
    assert_eq!(report.openalex_fwci, Some(14.75));
    assert!(
        sources
            .calls()
            .iter()
            .any(|(op, p)| op == "scite.tallies" && p["doi"] == "10.1016/s0140-6736(20)31180-6")
    );
}

#[tokio::test]
async fn test_integrity_handles_relation_is_retracted_by_and_missing_scite_gracefully() {
    let tmp = tempfile::tempdir().unwrap();
    let sources = Arc::new(MockSources::new());
    sources.on_err("scite.tallies", "scite rate limited");

    let net = Arc::new(FnNet::new(|req, _cx| {
        async move {
            let url = fsm_core::fixture::request_url(&req).unwrap().to_string();
            if url.contains("api.crossref.org") {
                return Ok(json_ok(
                    &url,
                    &json!({
                        "message": {
                            "relation": {
                                "is-retracted-by": [{
                                    "id-type": "doi",
                                    "id": "10.1000/notice-99"
                                }]
                            }
                        }
                    }),
                ));
            }
            if url.contains("api.openalex.org") {
                return Ok(json_ok(&url, &json!({"is_retracted": false, "fwci": 0.9})));
            }
            Ok(status_resp(&url, 404, "not found"))
        }
        .boxed()
    }));

    let sg = make_scholar_graph(
        net,
        Arc::new(MemoryStore::new()),
        sources,
        test_config(tmp.path(), false),
    );
    let report = sg
        .integrity("10.1000/retracted-demo", &CallCx::default())
        .await
        .unwrap();
    assert!(report.is_retracted);
    assert_eq!(report.retraction_notices.len(), 1);
    assert_eq!(
        report.retraction_notices[0].notice_doi.as_deref(),
        Some("10.1000/notice-99")
    );
    assert_eq!(report.retraction_notices[0].source, "crossref");
    assert_eq!(report.scite, None);
    assert_eq!(report.openalex_fwci, Some(0.9));
}

#[tokio::test]
async fn test_integrity_rejects_empty_input() {
    let tmp = tempfile::tempdir().unwrap();
    let net = Arc::new(FnNet::new(|_req, _cx| {
        async { Err(FsmError::Timeout(1)) }.boxed()
    }));
    let sg = make_scholar_graph(
        net,
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let err = sg.integrity("   ", &CallCx::default()).await.unwrap_err();
    assert!(matches!(err, FsmError::InvalidInput(_)));
}

#[tokio::test]
async fn test_rw_sync_concurrent_in_process_callers_single_flight_and_multiline_csv() {
    let tmp = tempfile::tempdir().unwrap();
    let nested_cache = tmp.path().join("nested").join("cache");
    let store = Arc::new(MemoryStore::new());
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());

    let multiline_csv = "Record ID,Title,Subject,Institution,Journal,Publisher,Country,Author,URLS,ArticleType,RetractionDate,RetractionDOI,RetractionPubMedID,OriginalPaperDate,OriginalPaperDOI,OriginalPaperPubMedID,RetractionNature,Reason,Paywalled,Notes\n\
20001,\"A Title, With Commas\nand \"\"Quotes\"\"\",(CS),MIT,J,P,US,A,,Article,01/02/2024 0:00,10.1000/r-ml,0,01/01/2023,10.1000/o-ml,0,Retraction,+Duplication of Image;,No,\"multi\nline note\"\n";

    let started_c = started.clone();
    let release_c = release.clone();
    let net = Arc::new(FnNet::new(move |req, _cx| {
        let started = started_c.clone();
        let release = release_c.clone();
        async move {
            started.notify_one();
            release.notified().await;
            Ok(HttpBytes::new(
                req.url,
                200,
                vec![("etag".to_owned(), "\"ml-1\"".to_owned())],
                multiline_csv.as_bytes().to_vec(),
            ))
        }
        .boxed()
    }));

    let sg = Arc::new(make_scholar_graph(
        net.clone(),
        store.clone(),
        Arc::new(MockSources::new()),
        test_config(&nested_cache, true),
    ));

    let sg1 = sg.clone();
    let h1 = tokio::spawn(async move { sg1.sync_retractions(&CallCx::default()).await });
    started.notified().await;

    // Concurrent caller while first download is in flight skips immediately.
    let second = sg.sync_retractions(&CallCx::default()).await;
    assert_eq!(second, SyncOutcome::SkippedBusy);

    release.notify_one();
    let first = h1.await.unwrap();
    assert_eq!(
        first,
        SyncOutcome::Synced {
            rows: 1,
            etag: Some("\"ml-1\"".to_owned()),
        }
    );
    assert_eq!(net.requests().len(), 1);
    assert_eq!(store.retraction_count(), 1);
}

#[tokio::test]
async fn test_rw_sync_failure_backs_off_for_one_hour_then_retries_after_window() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::new());
    let attempt = Arc::new(AtomicUsize::new(0));
    let attempt_c = attempt.clone();
    let net = Arc::new(FnNet::new(move |req, _cx| {
        let attempt = attempt_c.clone();
        async move {
            let n = attempt.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Ok(status_resp(&req.url, 503, "temporary outage"))
            } else {
                Ok(HttpBytes::new(
                    req.url,
                    200,
                    vec![("etag".to_owned(), "\"retry-ok\"".to_owned())],
                    SYNTHETIC_RW_CSV.as_bytes().to_vec(),
                ))
            }
        }
        .boxed()
    }));

    let sg = make_scholar_graph(
        net,
        store.clone(),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), true),
    );
    let first = sg.sync_retractions(&CallCx::default()).await;
    assert!(matches!(first, SyncOutcome::Failed(_)), "{first:?}");
    let (_, started, finished_after_fail) = store.get_sync_state(RW_SYNC_NAME).unwrap();
    assert_eq!(finished_after_fail, None);
    assert_eq!(attempt.load(Ordering::SeqCst), 1);

    // B2: immediate retry within the 1-hour backoff window MUST skip without hitting GitLab again.
    let second = sg.sync_retractions(&CallCx::default()).await;
    assert_eq!(second, SyncOutcome::SkippedBusy);
    assert_eq!(
        attempt.load(Ordering::SeqCst),
        1,
        "must not re-download CSV within backoff window"
    );

    // Once `started_at` ages past `RW_RETRY_BACKOFF_S`, retry proceeds and succeeds.
    store.set_sync_state(
        RW_SYNC_NAME,
        None,
        started - (RW_RETRY_BACKOFF_S + 10),
        None,
    );
    let third = sg.sync_retractions(&CallCx::default()).await;
    assert!(
        matches!(third, SyncOutcome::Synced { rows: 2, .. }),
        "{third:?}"
    );
    assert_eq!(attempt.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn test_integrity_correction_only_is_not_retracted_and_openalex_id_resolves() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::new());
    let mut corr = RetractionNotice::new("Correction", "retraction_watch");
    corr.date = Some("2023-08-19".into());
    corr.reason = Some("Error in Text".into());
    store.insert_retraction("10.1007/orig-1", corr);

    let oa_payload = json!({
        "id": "https://openalex.org/W99999",
        "doi": "https://doi.org/10.1007/orig-1",
        "display_name": "Corrected Paper",
        "publication_year": 2022,
        "is_retracted": false,
        "fwci": 1.25,
        "primary_location": {
            "source": {
                "issn_l": "1234-5678",
                "is_in_doaj": false
            }
        }
    });

    let oa_payload_c = oa_payload.clone();
    let net = Arc::new(FnNet::new(move |req, _cx| {
        let oa_payload = oa_payload_c.clone();
        async move {
            let url = fsm_core::fixture::request_url(&req).unwrap().to_string();
            if url.contains("api.openalex.org/works/W99999")
                || url.contains("api.openalex.org/works/https://doi.org/10.1007/orig-1")
            {
                return Ok(json_ok(&url, &oa_payload));
            }
            if url.contains("api.crossref.org") {
                return Ok(json_ok(&url, &json!({"message": {"updated-by": []}})));
            }
            if url.contains("doaj.org/api/search/journals/issn:1234-5678") {
                return Ok(json_ok(&url, &json!({"total": 0, "results": []})));
            }
            Ok(status_resp(&url, 404, "not found"))
        }
        .boxed()
    }));

    let sg = make_scholar_graph(
        net.clone(),
        store.clone(),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let report = sg.integrity("W99999", &CallCx::default()).await.unwrap();
    assert_eq!(report.doi.as_deref(), Some("10.1007/orig-1"));
    assert!(
        !report.is_retracted,
        "Correction alone must not mark retracted"
    );
    assert_eq!(report.retraction_notices.len(), 1);
    assert_eq!(report.retraction_notices[0].nature, "Correction");
    assert_eq!(report.doaj_vetted, Some(false));
    assert_eq!(report.openalex_fwci, Some(1.25));
    // M10: `ScholarCore::work("W99999")` populated the shared `scholar_works` cache.
    assert!(
        store
            .scholar_get("doi:10.1007/orig-1")
            .await
            .unwrap()
            .is_some(),
        "integrity non-DOI resolution should route through ScholarCore::work and populate scholar_works"
    );
}

// ---------------------------------------------------------------------------
// Review round 1 regression tests (B1, B2, I1, I5, M2, M3, M5, M9, M10)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_retraction_notice_itself_is_not_reported_retracted_by_integrity() {
    // B1 (Missing test #1): Live Crossref payload for the Wakefield retraction NOTICE
    // `10.1016/S0140-6736(10)60175-4` — no `updated-by`, one `update-to` pointing at the original.
    let tmp = tempfile::tempdir().unwrap();
    let sources = Arc::new(MockSources::new());
    sources.on_err("scite.tallies", "n/a");
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
            if url.contains("api.openalex.org") {
                return Ok(json_ok(
                    &url,
                    &json!({
                        "id": "https://openalex.org/W2000000001",
                        "doi": "https://doi.org/10.1016/s0140-6736(10)60175-4",
                        "is_retracted": false
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
        sources,
        test_config(tmp.path(), false),
    );
    let report = sg
        .integrity("10.1016/S0140-6736(10)60175-4", &CallCx::default())
        .await
        .unwrap();
    assert!(
        !report.is_retracted,
        "a retraction NOTICE is not itself retracted: {report:?}"
    );
    assert!(
        report.retraction_notices.is_empty(),
        "update-to must not produce retraction_notices: {:?}",
        report.retraction_notices
    );
}

#[tokio::test]
async fn test_detached_rw_sync_does_not_starve_integrity_deadline_and_backs_off() {
    // B2 (Missing test #3a & #3b): 200ms network latency, slow CSV download, 3s deadline.
    // `integrity()` must NOT wait on the CSV download, must report Crossref+OpenAlex retraction,
    // and repeated calls must only trigger 1 CSV download.
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::new());
    let csv_hits = Arc::new(AtomicUsize::new(0));
    let csv_hits_c = csv_hits.clone();
    let sources = Arc::new(MockSources::new());
    sources.on_err("scite.tallies", "n/a");

    let net = Arc::new(FnNet::new(move |req, ncx| {
        let csv_hits = csv_hits_c.clone();
        async move {
            let url = fsm_core::fixture::request_url(&req).unwrap().to_string();
            if url == RW_CSV_URL {
                csv_hits.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_secs(60)).await;
                return Err(FsmError::Timeout(ncx.timeout.as_millis() as u64));
            }
            let latency = Duration::from_millis(200);
            if ncx.timeout < latency {
                tokio::time::sleep(ncx.timeout).await;
                return Err(FsmError::Timeout(ncx.timeout.as_millis() as u64));
            }
            tokio::time::sleep(latency).await;
            if url.contains("api.crossref.org") {
                return Ok(json_ok(
                    &url,
                    &json!({"message": {"updated-by": [{
                        "type": "retraction",
                        "DOI": "10.1016/s0140-6736(20)31324-6",
                        "label": "Retraction",
                        "source": "publisher",
                        "updated": {"date-parts": [[2020, 6, 5]]}
                    }]}}),
                ));
            }
            if url.contains("api.openalex.org") {
                return Ok(json_ok(
                    &url,
                    &json!({"id": "https://openalex.org/W3027932593", "is_retracted": true, "fwci": 2.5}),
                ));
            }
            Ok(status_resp(&url, 404, "not found"))
        }
        .boxed()
    }));

    let sg = make_scholar_graph(net, store.clone(), sources, test_config(tmp.path(), true));
    for _ in 1..=3 {
        let cx = CallCx::new("", Instant::now() + Duration::from_secs(3));
        let t0 = Instant::now();
        let report = sg
            .integrity("10.1016/S0140-6736(20)31180-6", &cx)
            .await
            .unwrap();
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "integrity() was starved by slow CSV download: {:?}",
            t0.elapsed()
        );
        assert!(
            report.is_retracted,
            "Crossref + OpenAlex both say retracted; background sync must not starve them"
        );
        assert_eq!(report.openalex_fwci, Some(2.5));
    }
    assert_eq!(
        csv_hits.load(Ordering::SeqCst),
        1,
        "repeated calls must not spawn duplicate CSV downloads"
    );
}

#[tokio::test]
async fn test_graph_kicks_background_rw_sync() {
    // I5 (Missing test #3c): Calling `graph()` on a cold cache with `offline_sync=true`
    // starts the background Retraction Watch sync and populates the local store.
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::new());
    let synced_notify = Arc::new(tokio::sync::Notify::new());
    let synced_c = synced_notify.clone();

    let net = Arc::new(FnNet::new(move |req, _cx| {
        let synced = synced_c.clone();
        async move {
            let url = fsm_core::fixture::request_url(&req).unwrap().to_string();
            if url == RW_CSV_URL {
                synced.notify_one();
                return Ok(HttpBytes::new(
                    url,
                    200,
                    vec![("etag".to_owned(), "\"bg-1\"".to_owned())],
                    SYNTHETIC_RW_CSV.as_bytes().to_vec(),
                ));
            }
            if url.contains("api.openalex.org/works/https://doi.org/") {
                return Ok(json_ok(
                    &url,
                    &json!({
                        "id": "https://openalex.org/W2148972377",
                        "doi": "https://doi.org/10.1145/1571941.1572114",
                        "display_name": "Reciprocal rank fusion",
                        "cited_by_count": 0,
                        "referenced_works": []
                    }),
                ));
            }
            if url.contains("api.openalex.org") {
                return Ok(json_ok(&url, &json!({"results": []})));
            }
            if url.contains("api.crossref.org") {
                return Ok(json_ok(&url, &json!({"message": {"updated-by": []}})));
            }
            Ok(status_resp(&url, 404, "not found"))
        }
        .boxed()
    }));

    let sg = make_scholar_graph(
        net,
        store.clone(),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), true),
    );
    let _g = sg
        .graph(
            "10.1145/1571941.1572114",
            GraphOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();

    // Wait briefly for the detached background sync to finish storing rows.
    tokio::time::timeout(Duration::from_secs(2), synced_notify.notified())
        .await
        .expect("graph() must trigger background RW CSV sync");
    for _ in 0..50 {
        if store.retraction_count() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(store.retraction_count(), 2);
}

#[tokio::test]
async fn test_rw_sync_rejects_truncated_csv_without_committing() {
    // I1 (Missing test #4): `HttpBytes.truncated = true` must fail without replacing
    // the local retractions table or marking `sync_state.finished_at`.
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::new());
    store.insert_retraction(
        "10.1000/preexisting",
        RetractionNotice::new("Retraction", RW_SYNC_NAME),
    );

    let net = Arc::new(FnNet::new(|req, _cx| {
        async move {
            let body = "Record ID,Title,Subject,Institution,Journal,Publisher,Country,Author,URLS,ArticleType,RetractionDate,RetractionDOI,RetractionPubMedID,OriginalPaperDate,OriginalPaperDOI,OriginalPaperPubMedID,RetractionNature,Reason,Paywalled,Notes\n\
1,T,S,I,J,P,C,A,,Article,6/5/2020 0:00,10.1000/n1,0,1/1/2020 0:00,10.1000/full-row,0,Retraction,+Error;,No,\n\
2,T,S,I,J,P,C,A,,Article,6/5/2020 0:00,10.1000/n2,0,1/1/2020 0:00,10.1016/j.cel";
            let mut b = HttpBytes::new(
                req.url,
                200,
                vec![("etag".to_owned(), "\"t\"".to_owned())],
                body.as_bytes().to_vec(),
            );
            b.truncated = true;
            Ok(b)
        }
        .boxed()
    }));

    let sg = make_scholar_graph(
        net,
        store.clone(),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), true),
    );
    let out = sg.sync_retractions(&CallCx::default()).await;
    assert!(
        matches!(out, SyncOutcome::Failed(ref m) if m.contains("truncated")),
        "expected Failed(truncated), got {out:?}"
    );
    assert_eq!(store.replace_batch_calls(), 0, "table must be untouched");
    assert_eq!(store.retraction_count(), 1, "existing row must survive");
    assert!(
        store
            .retraction_lookup("10.1016/j.cel")
            .await
            .unwrap()
            .is_none(),
        "bogus truncated DOI must never be stored"
    );
    let (_, _, finished) = store.get_sync_state(RW_SYNC_NAME).unwrap();
    assert_eq!(finished, None, "truncated sync must not set finished_at");
}

#[tokio::test]
async fn test_crossref_source_normalized_and_label_not_stored_as_reason() {
    // M2 & M3 (Missing test #8): Crossref `updated-by` carries `"source": "publisher"` or
    // `"retraction-watch"` and `"label": "Retraction"`. Normalized `source` must be `"crossref"`
    // and `reason` must be `None`.
    let tmp = tempfile::tempdir().unwrap();
    let sources = Arc::new(MockSources::new());
    sources.on_err("scite.tallies", "n/a");
    let net = Arc::new(FnNet::new(|req, _cx| {
        async move {
            let url = fsm_core::fixture::request_url(&req).unwrap().to_string();
            if url.contains("api.crossref.org") {
                return Ok(json_ok(
                    &url,
                    &json!({"message": {"updated-by": [
                        {
                            "type": "retraction",
                            "DOI": "10.1016/s0140-6736(20)31324-6",
                            "label": "Retraction",
                            "source": "publisher",
                            "updated": {"date-parts": [[2020, 6, 5]]}
                        },
                        {
                            "type": "expression_of_concern",
                            "DOI": "10.1016/s0140-6736(20)31290-3",
                            "label": "Expression of concern",
                            "source": "retraction-watch",
                            "updated": {"date-parts": [[2020, 6, 3]]}
                        }
                    ]}}),
                ));
            }
            if url.contains("api.openalex.org") {
                return Ok(json_ok(&url, &json!({"is_retracted": true})));
            }
            Ok(status_resp(&url, 404, "not found"))
        }
        .boxed()
    }));

    let sg = make_scholar_graph(
        net,
        Arc::new(MemoryStore::new()),
        sources,
        test_config(tmp.path(), false),
    );
    let report = sg
        .integrity("10.1016/S0140-6736(20)31180-6", &CallCx::default())
        .await
        .unwrap();
    assert_eq!(report.retraction_notices.len(), 2);
    for n in &report.retraction_notices {
        assert_eq!(n.source, "crossref", "source must be normalized: {n:?}");
        assert_eq!(
            n.reason, None,
            "Crossref label must not be stored as reason: {n:?}"
        );
    }
}

#[tokio::test]
async fn test_rw_csv_reinstatement_supersedes_earlier_retraction_in_either_order() {
    // M9: A later Reinstatement supersedes an earlier Retraction regardless of CSV row order,
    // while a still-later second Retraction wins again.
    let header = "Record ID,Title,Subject,Institution,Journal,Publisher,Country,Author,URLS,ArticleType,RetractionDate,RetractionDOI,RetractionPubMedID,OriginalPaperDate,OriginalPaperDOI,OriginalPaperPubMedID,RetractionNature,Reason,Paywalled,Notes\n";
    let csv_order_a = format!(
        "{header}\
1,Paper A,S,I,J,P,US,A,,Article,6/5/2020 0:00,10.1000/ret-a,0,1/1/2020,10.1000/paper-a,0,Retraction,+Error;,No,\n\
2,Paper A,S,I,J,P,US,A,,Article,1/10/2022 0:00,10.1000/reinst-a,0,1/1/2020,10.1000/paper-a,0,Reinstatement,+Error by Publisher;,No,\n\
3,Paper B,S,I,J,P,US,B,,Article,1/10/2022 0:00,10.1000/reinst-b,0,1/1/2020,10.1000/paper-b,0,Reinstatement,+Error by Publisher;,No,\n\
4,Paper B,S,I,J,P,US,B,,Article,6/5/2020 0:00,10.1000/ret-b,0,1/1/2020,10.1000/paper-b,0,Retraction,+Error;,No,\n\
5,Paper C,S,I,J,P,US,C,,Article,6/5/2020 0:00,10.1000/ret-c1,0,1/1/2020,10.1000/paper-c,0,Retraction,+Error;,No,\n\
6,Paper C,S,I,J,P,US,C,,Article,1/10/2021 0:00,10.1000/reinst-c,0,1/1/2020,10.1000/paper-c,0,Reinstatement,+Error by Publisher;,No,\n\
7,Paper C,S,I,J,P,US,C,,Article,3/1/2023 0:00,10.1000/ret-c2,0,1/1/2020,10.1000/paper-c,0,Retraction,+Falsification;,No,\n"
    );
    let rows = parse_retraction_watch_csv(csv_order_a.as_bytes()).unwrap();
    let by_doi: std::collections::HashMap<_, _> = rows.into_iter().collect();
    assert_eq!(by_doi["10.1000/paper-a"].nature, "Reinstatement");
    assert_eq!(by_doi["10.1000/paper-b"].nature, "Reinstatement");
    assert_eq!(by_doi["10.1000/paper-c"].nature, "Retraction");
    assert_eq!(
        by_doi["10.1000/paper-c"].date.as_deref(),
        Some("2023-03-01")
    );

    // Confirm `integrity()` reports `!is_retracted` for the reinstated paper.
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::new());
    store.insert_retraction("10.1000/paper-a", by_doi["10.1000/paper-a"].clone());
    let net = Arc::new(FnNet::new(|req, _cx| {
        async move {
            let url = fsm_core::fixture::request_url(&req).unwrap().to_string();
            if url.contains("api.crossref.org") {
                return Ok(json_ok(&url, &json!({"message": {"updated-by": []}})));
            }
            if url.contains("api.openalex.org") {
                return Ok(json_ok(&url, &json!({"is_retracted": false})));
            }
            Ok(status_resp(&url, 404, "not found"))
        }
        .boxed()
    }));
    let sg = make_scholar_graph(
        net,
        store,
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let report = sg
        .integrity("10.1000/paper-a", &CallCx::default())
        .await
        .unwrap();
    assert!(
        !report.is_retracted,
        "reinstated paper must not be flagged retracted"
    );
    assert_eq!(report.retraction_notices[0].nature, "Reinstatement");
}

#[tokio::test]
async fn test_integrity_uses_doaj_journals_manifest_op_when_available() {
    // M5: `crates/manifests/sources/seed/doaj.toml` defines `doaj.journals` (limit 1..=50)
    // returning VenueProfile records. Verify `integrity()` invokes it with `limit: 1` and
    // skips direct `doaj.org` HTTP when the manifest op succeeds.
    let tmp = tempfile::tempdir().unwrap();
    let sources = Arc::new(MockSources::new());
    sources.set_limit_bounds("doaj.journals", 1, 50);
    sources.on_ok(
        "doaj.journals",
        vec![json!({
            "name": "PLoS ONE",
            "issn": ["1932-6203"],
            "publisher": "Public Library of Science (PLoS)",
            "doajVetted": true,
            "homepage": "https://journals.plos.org/plosone/",
            "hasApc": true,
            "subjects": ["Medicine", "Science"],
            "sources": ["doaj"]
        })],
    );
    sources.on_err("scite.tallies", "n/a");

    let net = Arc::new(FnNet::new(|req, _cx| {
        async move {
            let url = fsm_core::fixture::request_url(&req).unwrap().to_string();
            assert!(
                !url.contains("doaj.org"),
                "should not hit direct doaj.org when doaj.journals manifest op succeeds"
            );
            if url.contains("api.crossref.org") {
                return Ok(json_ok(
                    &url,
                    &json!({"message": {"ISSN": ["1932-6203"], "updated-by": []}}),
                ));
            }
            if url.contains("api.openalex.org") {
                return Ok(json_ok(&url, &json!({"is_retracted": false})));
            }
            Ok(status_resp(&url, 404, "not found"))
        }
        .boxed()
    }));

    let sg = make_scholar_graph(
        net,
        Arc::new(MemoryStore::new()),
        sources.clone(),
        test_config(tmp.path(), false),
    );
    let report = sg
        .integrity("10.1371/journal.pone.0146294", &CallCx::default())
        .await
        .unwrap();
    assert_eq!(report.doaj_vetted, Some(true));
    assert!(
        sources.calls().iter().any(|(op, p)| op == "doaj.journals"
            && p["query"] == "issn:1932-6203"
            && p["limit"] == 1)
    );
}

// ---------------------------------------------------------------------------
// Review round 2 regression tests (I1, I3, M2, Missing test #8)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_integrity_arxiv_without_published_doi_uses_openalex_wid_and_nonexistent_arxiv_has_no_doi()
 {
    // I1 (Missing test #4):
    // 1. `integrity("arXiv:1706.03762")` where `ScholarCore::work` returns `ids.doi = None`
    //    and `ids.openalex = Some("W2626778328")` MUST load OpenAlex integrity metadata from
    //    `/works/W2626778328` (not 404 on `10.48550/arxiv.1706.03762`).
    // 2. `integrity("arXiv:9999.99999")` where `ScholarCore::work` returns `Ok(None)` MUST
    //    return `doi == None` without fabricating `10.48550/arxiv.9999.99999`.
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::new());
    let mut cached = fsm_core::ScholarlyWork::new(
        "Attention Is All You Need",
        "https://arxiv.org/abs/1706.03762",
    );
    cached.ids.arxiv = Some("1706.03762".into());
    cached.ids.openalex = Some("W2626778328".into());
    cached.ids.doi = None;
    store.insert_work("arxiv:1706.03762", cached);

    let net = Arc::new(FnNet::new(|req, _cx| {
        async move {
            let url = fsm_core::fixture::request_url(&req).unwrap().to_string();
            if url.contains("api.openalex.org/works/W2626778328") {
                return Ok(json_ok(
                    &url,
                    &json!({
                        "id": "https://openalex.org/W2626778328",
                        "is_retracted": true,
                        "fwci": 42.5,
                        "primary_location": {"source": {"is_in_doaj": false}}
                    }),
                ));
            }
            if url.contains("api.openalex.org/works/https://doi.org/10.48550/arxiv.") {
                panic!("must not query OpenAlex by DataCite arXiv DOI when W-ID is known: {url}");
            }
            if url.contains("export.arxiv.org") {
                return Ok(common::text_ok(
                    &url,
                    "application/atom+xml",
                    r#"<?xml version="1.0" encoding="UTF-8"?><feed xmlns="http://www.w3.org/2005/Atom"></feed>"#,
                ));
            }
            Ok(status_resp(&url, 404, "not found"))
        }
        .boxed()
    }));

    let sg = make_scholar_graph(
        net.clone(),
        store,
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );

    let report = sg
        .integrity("arXiv:1706.03762", &CallCx::default())
        .await
        .unwrap();
    assert_eq!(report.openalex_fwci, Some(42.5));
    assert!(report.is_retracted);
    assert_eq!(report.doaj_vetted, Some(false));

    let missing = sg
        .integrity("arXiv:9999.99999", &CallCx::default())
        .await
        .unwrap();
    assert_eq!(
        missing.doi, None,
        "nonexistent arXiv ID must not fabricate a fake 10.48550/arxiv DOI"
    );
    assert!(!missing.is_retracted);
}

#[tokio::test]
async fn test_integrity_returns_upstream_error_when_all_authoritative_sources_fail() {
    // I3 / R2 (Missing test #6a): When local RW has never finished a sync and both Crossref
    // and OpenAlex return HTTP 503, `integrity()` MUST return `Err(FsmError::Upstream)`
    // instead of silently certifying a potentially retracted paper as clean (`is_retracted = false`).
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::new());
    let sources = Arc::new(MockSources::new());
    sources.on_err("scite.tallies", "scite 503");

    let net = Arc::new(FnNet::new(|req, _cx| {
        async move { Ok(status_resp(&req.url, 503, "service unavailable")) }.boxed()
    }));

    let sg = make_scholar_graph(net, store.clone(), sources, test_config(tmp.path(), false));
    let err = sg
        .integrity("10.1016/S0140-6736(20)31180-6", &CallCx::default())
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            FsmError::Upstream {
                status: Some(503),
                ..
            }
        ),
        "expected Upstream(503) when all integrity sources fail, got {err:?}"
    );

    // Conversely, once local RW has a completed sync, offline integrity still succeeds.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    store.set_sync_state(RW_SYNC_NAME, Some("\"ok\"".into()), now, Some(now));
    let report = sg
        .integrity("10.1016/S0140-6736(20)31180-6", &CallCx::default())
        .await
        .unwrap();
    assert!(!report.is_retracted);
}

#[tokio::test]
async fn test_rw_sync_rejects_header_only_zero_row_csv_without_wiping_store() {
    // M2 (Missing test #7b): A valid-header CSV containing zero valid DOI rows must fail
    // without wiping existing rows or marking `sync_state` fresh for 7 days.
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::new());
    store.insert_retraction(
        "10.1000/keep-me",
        RetractionNotice::new("Retraction", RW_SYNC_NAME),
    );
    let header_only_csv = "Record ID,Title,Subject,Institution,Journal,Publisher,Country,Author,URLS,ArticleType,RetractionDate,RetractionDOI,RetractionPubMedID,OriginalPaperDate,OriginalPaperDOI,OriginalPaperPubMedID,RetractionNature,Reason,Paywalled,Notes\n\
999,Invalid DOI row,S,I,J,P,US,A,,Article,2024-01-01,unavailable,0,2020-01-01,unavailable,0,Retraction,+Error;,No,\n";

    let net = Arc::new(FnNet::new(move |req, _cx| {
        let body = header_only_csv.as_bytes().to_vec();
        async move {
            Ok(HttpBytes::new(
                req.url,
                200,
                vec![("etag".to_owned(), "\"empty-1\"".to_owned())],
                body,
            ))
        }
        .boxed()
    }));

    let sg = make_scholar_graph(
        net,
        store.clone(),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), true),
    );
    let out = sg.sync_retractions(&CallCx::default()).await;
    assert!(
        matches!(out, SyncOutcome::Failed(ref m) if m.contains("zero valid DOI")),
        "expected Failed(zero valid DOI), got {out:?}"
    );
    assert_eq!(store.replace_batch_calls(), 0);
    assert_eq!(store.retraction_count(), 1);
    assert!(
        store
            .retraction_lookup("10.1000/keep-me")
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(store.get_sync_state(RW_SYNC_NAME).unwrap().2, None);
}

#[tokio::test]
async fn test_detached_rw_sync_fast_503_releases_mutex_and_backs_off_via_sync_state() {
    // Missing test #8: Background CSV fails quickly (20 ms, HTTP 503), releasing the
    // intra-process mutex; a subsequent `integrity()` call still skips re-downloading
    // because of the 1-hour `sync_state` backoff.
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::new());
    let csv_hits = Arc::new(AtomicUsize::new(0));
    let csv_hits_c = csv_hits.clone();
    let csv_done = Arc::new(tokio::sync::Notify::new());
    let csv_done_c = csv_done.clone();

    let net = Arc::new(FnNet::new(move |req, _cx| {
        let csv_hits = csv_hits_c.clone();
        let csv_done = csv_done_c.clone();
        async move {
            let url = fsm_core::fixture::request_url(&req).unwrap().to_string();
            if url == RW_CSV_URL {
                csv_hits.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
                csv_done.notify_one();
                return Ok(status_resp(&url, 503, "upstream 503"));
            }
            if url.contains("api.crossref.org") {
                return Ok(json_ok(&url, &json!({"message": {"updated-by": []}})));
            }
            if url.contains("api.openalex.org") {
                return Ok(json_ok(&url, &json!({"is_retracted": false})));
            }
            Ok(status_resp(&url, 404, "not found"))
        }
        .boxed()
    }));

    let sg = make_scholar_graph(
        net,
        store.clone(),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), true),
    );
    let _ = sg
        .integrity("10.1000/demo", &CallCx::default())
        .await
        .unwrap();

    // Wait for the background sync to finish failing and release the intra-process mutex.
    tokio::time::timeout(Duration::from_secs(2), csv_done.notified())
        .await
        .expect("background CSV request should complete");
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert_eq!(store.get_sync_state(RW_SYNC_NAME).unwrap().2, None);

    // Second `integrity()` call acquires the now-free mutex, sees `started_at` < 3600s, and skips.
    let _ = sg
        .integrity("10.1000/demo", &CallCx::default())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        csv_hits.load(Ordering::SeqCst),
        1,
        "post-failure sync_state backoff must suppress second download after mutex release"
    );
}
