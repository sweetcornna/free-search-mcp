//! `ScholarCore::search` over scripted sources: pool selection, deadlines,
//! dedup + merge, enrichment budget, retraction flags and ranking. No
//! network: sources are fakes, lookups go to a scripted net or to the
//! recorded fixtures.

mod common;

use common::*;
use fsm_core::{
    CallCx, FsmError, OpDecl, Param, ParamType, RetractionNotice, ReturnKind, ScholarQuery,
    ScholarlyWork,
};
use fsm_scholar::{Provider, Timing};
use std::sync::Arc;
use std::time::Duration;

fn cx() -> CallCx {
    CallCx::default()
}

/// A net where every lookup is a quick 404 (enrichment finds nothing).
fn quiet_net() -> Arc<ScriptNet> {
    ScriptNet::new(|_| (Duration::from_millis(5), 404, String::new()))
}

fn aiayn_arxiv() -> ScholarlyWork {
    let mut w = work(
        "Attention Is All You Need",
        Some(2017),
        Some("Ashish Vaswani"),
    );
    w.ids.arxiv = Some("1706.03762v7".into());
    w.work_type = "preprint".into();
    w.url = "https://arxiv.org/abs/1706.03762v7".into();
    w
}

fn aiayn_openaire() -> ScholarlyWork {
    let mut w = work(
        "Attention is all you need",
        Some(2017),
        Some("Vaswani, Ashish"),
    );
    w.ids.doi = Some("https://doi.org/10.48550/ARXIV.1706.03762".into());
    w
}

fn speech() -> ScholarlyWork {
    work(
        "Attention Is All You Need In Speech Separation",
        Some(2021),
        Some("Cem Subakan"),
    )
}

fn titles(ws: &[ScholarlyWork]) -> Vec<&str> {
    ws.iter().map(|w| w.title.as_str()).collect()
}

fn pool_of_four() -> Vec<Arc<FakeSource>> {
    ["arxiv", "crossref", "openaire", "europepmc"]
        .into_iter()
        .map(|id| FakeSource::new(id).arc())
        .collect()
}

#[tokio::test]
async fn default_pool_is_the_four_keyless_sources_and_never_core() {
    let four = pool_of_four();
    let core_ac = FakeSource::new("core_ac")
        .key("SEARCH_MCP_CORE_API_KEY")
        .arc();
    let openalex = FakeSource::new("openalex")
        .key("SEARCH_MCP_OPENALEX_API_KEY")
        .arc();
    let s2 = FakeSource::new("semanticscholar").arc();
    let dblp = FakeSource::new("dblp").scopes(&["academic.cs"]).arc();
    let mut all = four.clone();
    all.extend([core_ac.clone(), openalex.clone(), s2.clone(), dblp.clone()]);
    let core = core_with(as_sources(all), quiet_net(), MemStore::new(), config(&[]));
    let res = core
        .search(ScholarQuery::new("graph neural networks"), &cx())
        .await
        .unwrap();
    for s in &four {
        assert_eq!(s.seen().len(), 1);
        assert_eq!(s.seen()[0].op, "search");
    }
    for s in [&core_ac, &openalex, &s2, &dblp] {
        assert!(s.seen().is_empty());
    }
    assert_eq!(
        res.meta.sources,
        ["arxiv", "crossref", "openaire", "europepmc"]
    );
}

#[tokio::test]
async fn keyed_sources_join_only_with_their_key_and_get_it() {
    let mut all = pool_of_four();
    let core_ac = FakeSource::new("core_ac")
        .key("SEARCH_MCP_CORE_API_KEY")
        .arc();
    let openalex = FakeSource::new("openalex")
        .key("SEARCH_MCP_OPENALEX_API_KEY")
        .arc();
    let s2 = FakeSource::new("semanticscholar")
        .key("SEARCH_MCP_SEMANTICSCHOLAR_API_KEY")
        .arc();
    all.extend([core_ac.clone(), openalex.clone(), s2.clone()]);
    let cfg = config(&[
        ("SEARCH_MCP_CORE_API_KEY", "core-key"),
        ("SEARCH_MCP_OPENALEX_API_KEY", "oa-key"),
    ]);
    let core = core_with(as_sources(all), quiet_net(), MemStore::new(), cfg);
    let res = core
        .search(ScholarQuery::new("protein folding"), &cx())
        .await
        .unwrap();
    assert_eq!(core_ac.seen()[0].api_key.as_deref(), Some("core-key"));
    assert_eq!(openalex.seen()[0].api_key.as_deref(), Some("oa-key"));
    assert!(s2.seen().is_empty());
    assert_eq!(res.meta.sources.len(), 6);
}

#[tokio::test]
async fn optional_key_core_never_joins_a_scope_fallback_without_its_key() {
    // Even if a manifest sets route_requires_key: false, CORE's id guard keeps it out keyless.
    let core_ac = || {
        FakeSource::new("core_ac")
            .optional_key_auto("SEARCH_MCP_CORE_API_KEY")
            .scopes(&["academic", "academic.openaccess", "academic.theses"])
            .arc()
    };
    let ndltd = FakeSource::new("ndltd").scopes(&["academic.theses"]).arc();
    let mut q = ScholarQuery::new("soil microbiome");
    q.subscope = Some("theses".into());

    let keyless = core_ac();
    let mut all = pool_of_four();
    all.extend([keyless.clone(), ndltd.clone()]);
    let core = core_with(as_sources(all), quiet_net(), MemStore::new(), config(&[]));
    let res = core.search(q.clone(), &cx()).await.unwrap();
    assert!(keyless.seen().is_empty(), "CORE must not join keyless");
    assert_eq!(
        ndltd.seen().len(),
        1,
        "the theses fallback still adds other sources"
    );
    assert!(!res.meta.sources.iter().any(|s| s == "core_ac"));

    let keyed = core_ac();
    let mut all = pool_of_four();
    all.push(keyed.clone());
    let cfg = config(&[("SEARCH_MCP_CORE_API_KEY", "core-key")]);
    let core = core_with(as_sources(all), quiet_net(), MemStore::new(), cfg);
    core.search(q, &cx()).await.unwrap();
    assert_eq!(keyed.seen().len(), 1);
    assert_eq!(keyed.seen()[0].api_key.as_deref(), Some("core-key"));
}

#[tokio::test]
async fn explicit_sources_replace_the_pool() {
    let mut all = pool_of_four();
    let dblp = FakeSource::new("dblp").scopes(&["academic.cs"]).arc();
    all.push(dblp.clone());
    let arxiv = all[0].clone();
    let core = core_with(as_sources(all), quiet_net(), MemStore::new(), config(&[]));
    let mut q = ScholarQuery::new("transformers");
    q.sources = Some(vec!["DBLP".into(), "nope".into()]);
    let res = core.search(q, &cx()).await.unwrap();
    assert_eq!(dblp.seen().len(), 1);
    assert!(arxiv.seen().is_empty());
    assert!(
        res.meta
            .notes
            .iter()
            .any(|n| n.contains("unknown source(s) nope"))
    );
    let mut q = ScholarQuery::new("transformers");
    q.sources = Some(vec!["nope".into()]);
    assert!(
        matches!(core.search(q, &cx()).await, Err(FsmError::InvalidInput(m)) if m.contains("nope"))
    );
}

#[tokio::test]
async fn subscopes_add_specialists_at_double_weight() {
    let mut all = pool_of_four();
    let mut alpha = work("graph networks alpha", Some(2020), Some("A Alpha"));
    alpha.ids.doi = Some("10.1234/alpha".into());
    let mut beta = work("graph networks beta", Some(2020), Some("B Beta"));
    beta.ids.doi = Some("10.1234/beta".into());
    all[0] = FakeSource::new("arxiv").works(vec![alpha]).arc();
    let dblp = FakeSource::new("dblp")
        .scopes(&["academic.cs"])
        .works(vec![beta])
        .arc();
    all.push(dblp.clone());
    let core = core_with(as_sources(all), quiet_net(), MemStore::new(), config(&[]));
    let mut q = ScholarQuery::new("graph networks");
    q.subscope = Some("cs".into());
    let res = core.search(q, &cx()).await.unwrap();
    assert_eq!(dblp.seen().len(), 1);
    assert_eq!(res.meta.subscope.as_deref(), Some("academic.cs"));
    assert_eq!(
        titles(&res.works),
        ["graph networks beta", "graph networks alpha"]
    );

    let mut q = ScholarQuery::new("graph networks");
    q.subscope = Some("astrology".into());
    assert!(
        matches!(core.search(q, &cx()).await, Err(FsmError::InvalidInput(m)) if m.contains("cs"))
    );
}

#[tokio::test]
async fn only_declared_params_are_sent_and_limit_is_clamped() {
    let mut all = pool_of_four();
    let query_only = OpDecl::new("search", "s", ReturnKind::Works).with_param(Param::new(
        "query",
        ParamType::String,
        "q",
    ));
    let crossref = FakeSource::new("crossref").op(query_only).arc();
    all[1] = crossref.clone();
    let arxiv = all[0].clone();
    let core = core_with(as_sources(all), quiet_net(), MemStore::new(), config(&[]));
    let mut q = ScholarQuery::new("  diffusion models ");
    q.limit = 500;
    core.search(q, &cx()).await.unwrap();
    assert_eq!(
        arxiv.seen()[0].params,
        serde_json::json!({"query": "diffusion models", "limit": 20})
    );
    assert_eq!(
        crossref.seen()[0].params,
        serde_json::json!({"query": "diffusion models"})
    );
}

#[tokio::test(start_paused = true)]
async fn the_soft_deadline_returns_what_arrived() {
    let started = tokio::time::Instant::now();
    let arxiv = FakeSource::new("arxiv")
        .works(vec![aiayn_arxiv()])
        .delay(Duration::from_millis(100))
        .arc();
    let crossref = FakeSource::new("crossref")
        .works(vec![speech()])
        .delay(Duration::from_secs(4))
        .arc();
    let openaire = FakeSource::new("openaire")
        .delay(Duration::from_millis(100))
        .arc();
    let europepmc = FakeSource::new("europepmc").failing("boom").arc();
    let core = core_with(
        as_sources(vec![arxiv, crossref, openaire, europepmc]),
        quiet_net(),
        MemStore::new(),
        config(&[]),
    );
    let res = core
        .search(ScholarQuery::new("attention"), &cx())
        .await
        .unwrap();
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(2_500) && waited < Duration::from_millis(3_500),
        "{waited:?}"
    );
    assert_eq!(titles(&res.works), ["Attention Is All You Need"]);
    assert_eq!(res.meta.sources, ["arxiv", "openaire"]);
    assert!(
        res.meta
            .notes
            .iter()
            .any(|n| n.starts_with("crossref: no answer within 2500 ms")),
        "{:?}",
        res.meta.notes
    );
    assert!(
        res.meta
            .notes
            .iter()
            .any(|n| n.starts_with("europepmc: ") && n.contains("boom"))
    );
}

#[tokio::test(start_paused = true)]
async fn after_the_soft_deadline_the_first_results_end_the_wait() {
    let started = tokio::time::Instant::now();
    let arxiv = FakeSource::new("arxiv")
        .works(vec![aiayn_arxiv()])
        .delay(Duration::from_secs(3))
        .arc();
    let crossref = FakeSource::new("crossref")
        .works(vec![speech()])
        .delay(Duration::from_secs(5))
        .arc();
    let core = core_with(
        as_sources(vec![arxiv, crossref]),
        quiet_net(),
        MemStore::new(),
        config(&[]),
    );
    let res = core
        .search(ScholarQuery::new("attention"), &cx())
        .await
        .unwrap();
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_secs(3) && waited < Duration::from_millis(3_900),
        "{waited:?}"
    );
    assert_eq!(res.meta.sources, ["arxiv"]);
}

#[tokio::test(start_paused = true)]
async fn nothing_by_the_hard_deadline_is_an_upstream_error() {
    let started = tokio::time::Instant::now();
    let slow: Vec<Arc<FakeSource>> = ["arxiv", "crossref", "openaire", "europepmc"]
        .into_iter()
        .map(|id| {
            FakeSource::new(id)
                .works(vec![speech()])
                .delay(Duration::from_secs(30))
                .arc()
        })
        .collect();
    let core = core_with(as_sources(slow), quiet_net(), MemStore::new(), config(&[]));
    let err = core
        .search(ScholarQuery::new("attention"), &cx())
        .await
        .unwrap_err();
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_secs(6) && waited < Duration::from_millis(6_500),
        "{waited:?}"
    );
    assert!(
        matches!(err, FsmError::Upstream { ref message, .. } if message.contains("arxiv: timed out after 6000 ms")),
        "{err:?}"
    );
}

#[tokio::test]
async fn every_source_failing_is_an_upstream_error() {
    let failing: Vec<Arc<FakeSource>> = ["arxiv", "crossref", "openaire", "europepmc"]
        .into_iter()
        .map(|id| FakeSource::new(id).failing("down").arc())
        .collect();
    let core = core_with(
        as_sources(failing),
        quiet_net(),
        MemStore::new(),
        config(&[]),
    );
    assert!(matches!(
        core.search(ScholarQuery::new("x y"), &cx()).await,
        Err(FsmError::Upstream { .. })
    ));
    let core = core_with(Vec::new(), quiet_net(), MemStore::new(), config(&[]));
    assert!(matches!(
        core.search(ScholarQuery::new("x y"), &cx()).await,
        Err(FsmError::InvalidInput(m)) if m.contains("no scholarly source")
    ));
}

#[tokio::test(start_paused = true)]
async fn enrichment_runs_four_at_a_time_and_stops_at_800_ms() {
    let works: Vec<ScholarlyWork> = (1..=5)
        .map(|i| {
            let mut w = work(
                &format!("deep learning study number {i}"),
                Some(2020),
                Some("A Author"),
            );
            w.ids.doi = Some(format!("10.1234/study.{i}"));
            w
        })
        .collect();
    let arxiv = FakeSource::new("arxiv").works(works).arc();
    let net = ScriptNet::new(|_| (Duration::from_millis(300), 404, String::new()));
    let store = MemStore::new();
    let core = core_with(
        as_sources(vec![arxiv]),
        net.clone(),
        store.clone(),
        config(&[]),
    );
    assert_eq!(core.timing(), Timing::default());
    let started = tokio::time::Instant::now();
    let res = core
        .search(ScholarQuery::new("deep learning"), &cx())
        .await
        .unwrap();
    let waited = started.elapsed();
    assert_eq!(res.works.len(), 5);
    // Top 5 × (OpenAlex + S2) = 10 lookups, 300 ms each, 4 at a time:
    // 4 done at 300 ms, 8 at 600 ms, the last two cut at 800 ms.
    assert_eq!(
        net.max_in_flight.load(std::sync::atomic::Ordering::SeqCst),
        4
    );
    assert_eq!(net.requests().len(), 10);
    assert!(
        res.meta
            .notes
            .iter()
            .any(|n| n == "enrichment: 8 of 10 lookups finished within 800 ms"),
        "{:?}",
        res.meta.notes
    );
    assert!(
        waited >= Duration::from_millis(800) && waited < Duration::from_millis(900),
        "{waited:?}"
    );
    // The eight 404s are remembered for a day.
    assert_eq!(
        store
            .api
            .lock()
            .unwrap()
            .values()
            .filter(|(b, ttl)| b == b"null" && *ttl == 86_400)
            .count(),
        8
    );
}

#[tokio::test]
async fn enrichment_adds_openalex_counts_and_a_real_s2_429_benches_s2() {
    let net = replay();
    let store = MemStore::new();
    let arxiv = FakeSource::new("arxiv").works(vec![aiayn_arxiv()]).arc();
    let crossref = FakeSource::new("crossref").works(vec![speech()]).arc();
    let core = core_with(
        as_sources(vec![arxiv, crossref]),
        net.clone(),
        store.clone(),
        config(&[]),
    );
    let res = core
        .search(ScholarQuery::new("attention is all you need"), &cx())
        .await
        .unwrap();
    assert!(net.unmatched().is_empty(), "{:?}", net.unmatched());
    let top = &res.works[0];
    assert_eq!(top.title, "Attention Is All You Need");
    assert!(top.cited_by.unwrap() > 100_000);
    assert_eq!(top.cited_by_source.as_deref(), Some("openalex"));
    assert_eq!(top.ids.openalex.as_deref(), Some("W2626778328"));
    assert_eq!(top.year, Some(2017));
    assert_eq!(top.sources, ["arxiv", "openalex"]);
    assert_eq!(res.meta.sources, ["arxiv", "crossref", "openalex"]);
    assert!(res.meta.notes.iter().any(|n| n.starts_with("enrichment semanticscholar: rate limited (HTTP 429); benched until 2026-11-26T00:00:00Z")), "{:?}", res.meta.notes);
    // The OpenAlex body is cached for 30 days.
    let oa_keys: Vec<String> = store
        .api_keys()
        .into_iter()
        .filter(|k| k.starts_with("scholar:v1:openalex:"))
        .collect();
    assert_eq!(oa_keys.len(), 1);
    assert_eq!(store.api_get(&oa_keys[0]).unwrap().1, 30 * 86_400);
    // Again: OpenAlex from cache, S2 benched — no new requests.
    let before = net.hits().len();
    let res = core
        .search(ScholarQuery::new("attention is all you need"), &cx())
        .await
        .unwrap();
    assert_eq!(net.hits().len(), before);
    assert!(res.meta.notes.iter().any(|n| n == "semanticscholar: benched until 2026-11-26T00:00:00Z after HTTP 429; enrichment skipped"), "{:?}", res.meta.notes);
    assert!(res.works[0].cited_by.unwrap() > 100_000);
    assert_eq!(
        core.benched_until(Provider::SemanticScholar).await,
        Some(NEXT_MIDNIGHT)
    );
}

#[tokio::test]
async fn dedup_merges_preprint_and_datacite_records_and_sums_their_ranks() {
    let other = work(
        "Neural machine translation by jointly learning to align",
        Some(2014),
        Some("Dzmitry Bahdanau"),
    );
    let arxiv = FakeSource::new("arxiv").works(vec![aiayn_arxiv()]).arc();
    let openaire = FakeSource::new("openaire")
        .works(vec![other, aiayn_openaire()])
        .arc();
    let crossref = FakeSource::new("crossref").works(vec![speech()]).arc();
    let core = core_with(
        as_sources(vec![arxiv, crossref, openaire]),
        quiet_net(),
        MemStore::new(),
        config(&[]),
    );
    let res = core
        .search(ScholarQuery::new("attention is all you need"), &cx())
        .await
        .unwrap();
    assert_eq!(res.works.len(), 3);
    let top = &res.works[0];
    assert_eq!(top.title, "Attention Is All You Need");
    assert_eq!(top.ids.arxiv.as_deref(), Some("1706.03762"));
    assert_eq!(top.ids.doi, None, "the DataCite DOI is the arXiv id");
    assert_eq!(top.sources, ["arxiv", "openaire"]);
    assert_eq!(
        res.works[1].title,
        "Attention Is All You Need In Speech Separation"
    );
}

#[tokio::test]
async fn retraction_watch_flags_and_demotes() {
    let mut a = work(
        "Hydroxychloroquine outcomes in covid-19: a registry analysis",
        Some(2020),
        Some("Mandeep Mehra"),
    );
    a.ids.doi = Some("10.1234/hcq.registry".into());
    a.cited_by = Some(900);
    let mut b = work(
        "Hydroxychloroquine outcomes in covid-19: a randomized trial",
        Some(2020),
        Some("Peter Horby"),
    );
    b.ids.doi = Some("10.1234/hcq.trial".into());
    b.cited_by = Some(300);
    let store = MemStore::new();
    let mut notice = RetractionNotice::new("Retraction", "retraction-watch");
    notice.reason = Some("Concerns about data".into());
    notice.date = Some("2020-06-05".into());
    store
        .retractions
        .lock()
        .unwrap()
        .insert("10.1234/hcq.registry".into(), notice);
    let arxiv = FakeSource::new("arxiv")
        .works(vec![a.clone(), b.clone()])
        .arc();
    let europepmc = FakeSource::new("europepmc").works(vec![a, b]).arc();
    let core = core_with(
        as_sources(vec![arxiv, europepmc]),
        quiet_net(),
        store,
        config(&[]),
    );
    let res = core
        .search(ScholarQuery::new("hydroxychloroquine covid-19"), &cx())
        .await
        .unwrap();
    assert_eq!(res.works.len(), 2);
    assert!(!res.works[0].retracted);
    assert!(res.works[1].retracted);
    assert_eq!(
        res.works[1].retraction_reason.as_deref(),
        Some("Retraction: Concerns about data (2020-06-05)")
    );
    let mut q = ScholarQuery::new("hydroxychloroquine covid-19");
    q.sort = Some("citations".into());
    let res = core.search(q, &cx()).await.unwrap();
    assert!(
        res.works[1].retracted,
        "retracted works list last in every order"
    );
}

#[tokio::test]
async fn filters_sorts_and_input_errors() {
    let mut old = work("graph learning old", Some(2001), Some("A Old"));
    old.cited_by = Some(5000);
    let mut open = work("graph learning open", Some(2024), Some("B Open"));
    open.oa = Some(fsm_core::OaLocation::new("gold"));
    open.cited_by = Some(10);
    let undated = work("graph learning undated", None, Some("C Undated"));
    let arxiv = FakeSource::new("arxiv")
        .works(vec![old, open, undated])
        .arc();
    let core = core_with(
        as_sources(vec![arxiv]),
        quiet_net(),
        MemStore::new(),
        config(&[]),
    );

    let mut q = ScholarQuery::new("graph learning");
    q.year_from = Some(2020);
    let res = core.search(q, &cx()).await.unwrap();
    assert_eq!(titles(&res.works), ["graph learning open"]);
    assert!(
        res.meta
            .notes
            .iter()
            .any(|n| n.starts_with("2 result(s) outside the year range"))
    );

    let mut q = ScholarQuery::new("graph learning");
    q.oa_only = true;
    assert_eq!(
        titles(&core.search(q, &cx()).await.unwrap().works),
        ["graph learning open"]
    );

    let mut q = ScholarQuery::new("graph learning");
    q.sort = Some("citations".into());
    assert_eq!(
        titles(&core.search(q, &cx()).await.unwrap().works),
        [
            "graph learning old",
            "graph learning open",
            "graph learning undated"
        ]
    );
    let mut q = ScholarQuery::new("graph learning");
    q.sort = Some("recent".into());
    assert_eq!(
        titles(&core.search(q, &cx()).await.unwrap().works)[0],
        "graph learning open"
    );
    let mut q = ScholarQuery::new("graph learning");
    q.limit = 1;
    assert_eq!(core.search(q, &cx()).await.unwrap().works.len(), 1);

    let mut q = ScholarQuery::new("graph learning");
    q.sort = Some("sideways".into());
    assert!(matches!(
        core.search(q, &cx()).await,
        Err(FsmError::InvalidInput(_))
    ));
    let mut q = ScholarQuery::new("graph learning");
    q.year_from = Some(2024);
    q.year_to = Some(2020);
    assert!(matches!(
        core.search(q, &cx()).await,
        Err(FsmError::InvalidInput(_))
    ));
    assert!(matches!(
        core.search(ScholarQuery::new("   "), &cx()).await,
        Err(FsmError::InvalidInput(_))
    ));
}

/// Port of `test_resolves_a_title_through_search`.
#[tokio::test]
async fn test_resolves_a_title_through_search() {
    let mut rrf = work(
        "Reciprocal rank fusion outperforms condorcet and individual rank learning methods",
        Some(2009),
        Some("Gordon V. Cormack"),
    );
    rrf.ids.doi = Some("10.1145/1571941.1572114".into());
    let crossref = FakeSource::new("crossref").works(vec![rrf]).arc();
    let core = core_with(
        as_sources(vec![crossref.clone()]),
        quiet_net(),
        MemStore::new(),
        config(&[]),
    );
    let r = core
        .resolve("Reciprocal rank fusion outperforms condorcet", &cx())
        .await
        .unwrap();
    assert_eq!(r.pid.kind(), "title");
    assert!(r.work.unwrap().title.starts_with("Reciprocal rank fusion"));
    assert_eq!(
        crossref.seen()[0].params["query"],
        "Reciprocal rank fusion outperforms condorcet"
    );
}

/// Port of `test_a_rejected_title_lists_the_near_misses`.
#[tokio::test]
async fn test_a_rejected_title_lists_the_near_misses() {
    let beit = work(
        "BEiT: BERT Pre-Training of Image Transformers",
        Some(2021),
        Some("Hangbo Bao"),
    );
    let crossref = FakeSource::new("crossref").works(vec![beit]).arc();
    let core = core_with(
        as_sources(vec![crossref]),
        quiet_net(),
        MemStore::new(),
        config(&[]),
    );
    let q = "BERT pre-training of deep bidirectional transformers";
    let r = core.resolve(q, &cx()).await.unwrap();
    assert!(r.work.is_none() && !r.upstream_failed);
    assert!(r.notes.iter().any(|n| n.contains("Closest titles found")));
    assert!(r.notes.iter().any(|n| n.contains("BEiT")));
    assert_eq!(
        r.near_misses,
        ["BEiT: BERT Pre-Training of Image Transformers"]
    );
    assert_eq!(core.work(q, &cx()).await.unwrap(), None);
}

/// Regression for B1 & port of `test_semanticscholar_stays_out_of_auto_routing_without_a_key`
/// on `subscope: "index"` (`academic.index`):
/// - `route_requires_key = true` (OpenAlex, S2, Google Books) never auto-route keyless;
/// - `PoolRole::Explicit` (ORCID, ROR) never auto-route into paper search;
/// - `NeedsKey` sources skipped by `push` do not burn the 3-source fallback cap.
#[tokio::test]
async fn subscope_fallback_respects_route_requires_key_explicit_role_and_needs_key_cap() {
    let mut all = pool_of_four();
    let openalex = FakeSource::new("openalex")
        .optional_key("SEARCH_MCP_OPENALEX_API_KEY")
        .scopes(&["academic", "academic.index"])
        .arc();
    let s2 = FakeSource::new("semanticscholar")
        .optional_key("SEARCH_MCP_SEMANTICSCHOLAR_API_KEY")
        .scopes(&["academic", "academic.index"])
        .arc();
    let orcid = FakeSource::new("orcid")
        .scopes(&["academic", "academic.index"])
        .pool_role(fsm_core::PoolRole::Explicit)
        .arc();
    let ror = FakeSource::new("ror")
        .scopes(&["academic", "academic.index"])
        .pool_role(fsm_core::PoolRole::Explicit)
        .arc();
    let gbooks = FakeSource::new("google_books")
        .optional_key("SEARCH_MCP_GOOGLE_BOOKS_API_KEY")
        .scopes(&["academic", "academic.books"])
        .arc();
    // 3 RequiredKey humanities sources registered BEFORE 2 keyless humanities sources.
    let dpla = FakeSource::new("dpla")
        .key("SEARCH_MCP_DPLA_API_KEY")
        .scopes(&["academic.humanities"])
        .arc();
    let europeana = FakeSource::new("europeana")
        .key("SEARCH_MCP_EUROPEANA_API_KEY")
        .scopes(&["academic.humanities"])
        .arc();
    let trove = FakeSource::new("trove_au")
        .key("SEARCH_MCP_TROVE_API_KEY")
        .scopes(&["academic.humanities"])
        .arc();
    let wellcome = FakeSource::new("wellcome_collection")
        .scopes(&["academic.humanities"])
        .arc();
    let loc = FakeSource::new("loc_digital")
        .scopes(&["academic.humanities"])
        .arc();

    all.extend([
        openalex.clone(),
        s2.clone(),
        orcid.clone(),
        ror.clone(),
        gbooks.clone(),
        dpla.clone(),
        europeana.clone(),
        trove.clone(),
        wellcome.clone(),
        loc.clone(),
    ]);
    let core = core_with(as_sources(all), quiet_net(), MemStore::new(), config(&[]));

    // subscope: "index" keyless -> none of openalex, semanticscholar, orcid, ror called.
    let mut q_index = ScholarQuery::new("transformers");
    q_index.subscope = Some("index".into());
    core.search(q_index, &cx()).await.unwrap();
    assert!(
        openalex.seen().is_empty(),
        "OpenAlex must not auto-route keyless"
    );
    assert!(s2.seen().is_empty(), "S2 must not auto-route keyless");
    assert!(
        orcid.seen().is_empty(),
        "ORCID (Explicit) must not auto-route"
    );
    assert!(ror.seen().is_empty(), "ROR (Explicit) must not auto-route");

    // subscope: "books" keyless -> google_books skipped.
    let mut q_books = ScholarQuery::new("transformers");
    q_books.subscope = Some("books".into());
    core.search(q_books, &cx()).await.unwrap();
    assert!(
        gbooks.seen().is_empty(),
        "Google Books must not auto-route keyless"
    );

    // subscope: "humanities" keyless -> skipped NeedsKey sources do NOT starve keyless sources.
    let mut q_hum = ScholarQuery::new("manuscripts");
    q_hum.subscope = Some("humanities".into());
    core.search(q_hum, &cx()).await.unwrap();
    assert!(dpla.seen().is_empty() && europeana.seen().is_empty() && trove.seen().is_empty());
    assert_eq!(wellcome.seen().len(), 1);
    assert_eq!(loc.seen().len(), 1);
}

/// Regression for B2 & port of `test_semanticscholar_sends_the_key_as_a_header`:
/// `SEARCH_MCP_S2_API_KEY` both admits `semanticscholar` into the pool AND
/// forwards the key in `SrcCx.api_key` even though the manifest declares
/// `SEARCH_MCP_SEMANTICSCHOLAR_API_KEY`.
#[tokio::test]
async fn test_semanticscholar_sends_the_key_as_a_header() {
    let mut all = pool_of_four();
    let s2 = FakeSource::new("semanticscholar")
        .optional_key("SEARCH_MCP_SEMANTICSCHOLAR_API_KEY")
        .scopes(&["academic", "academic.index"])
        .arc();
    all.push(s2.clone());
    let cfg = config(&[("SEARCH_MCP_S2_API_KEY", "s2-alias-key")]);
    let core = core_with(as_sources(all), quiet_net(), MemStore::new(), cfg);
    core.search(ScholarQuery::new("attention"), &cx())
        .await
        .unwrap();
    assert_eq!(s2.seen().len(), 1);
    assert_eq!(s2.seen()[0].api_key.as_deref(), Some("s2-alias-key"));
}

/// Regression for B6: benched OpenAlex/S2 are skipped in `search()`, all-benched
/// returns an error surfacing the bench note, and an HTTP 429 in `pool::fan_out`
/// benches the provider BEFORE `enrich()` runs in the same call.
#[tokio::test]
async fn bench_skips_benched_sources_and_429_in_search_benches_before_enrich() {
    // 1. Pre-benched OpenAlex is skipped during search().
    let mut all = pool_of_four();
    let oa = FakeSource::new("openalex")
        .optional_key("SEARCH_MCP_OPENALEX_API_KEY")
        .arc();
    all.push(oa.clone());
    let cfg = config(&[("SEARCH_MCP_OPENALEX_API_KEY", "oa-key")]);
    let core = core_with(as_sources(all), quiet_net(), MemStore::new(), cfg);
    core.note_rate_limited(Provider::OpenAlex).await;
    let res = core
        .search(ScholarQuery::new("transformers"), &cx())
        .await
        .unwrap();
    assert!(
        oa.seen().is_empty(),
        "benched OpenAlex must not be searched"
    );
    assert!(
        res.meta
            .notes
            .iter()
            .any(|n| n.contains("openalex: benched until 2026-11-26T00:00:00Z")),
        "{:?}",
        res.meta.notes
    );

    // 2. Explicit sources containing only benched OpenAlex errors with the bench note.
    let mut q_only = ScholarQuery::new("transformers");
    q_only.sources = Some(vec!["openalex".into()]);
    let err = core.search(q_only, &cx()).await.unwrap_err();
    assert!(
        matches!(err, FsmError::Upstream { status: Some(429), ref message, .. } if message.contains("benched until 2026-11-26T00:00:00Z")),
        "{err:?}"
    );

    // 3. HTTP 429 from S2 in fan_out (both with and without Retry-After at normal timeout)
    // benches S2 immediately so enrich() sends 0 requests to S2 in the same search() call.
    for retry_after in [None, Some(60)] {
        let mut w = work(
            "Attention Is All You Need",
            Some(2017),
            Some("Ashish Vaswani"),
        );
        w.ids.doi = Some("10.1234/attn".into());
        let arxiv = FakeSource::new("arxiv").works(vec![w]).arc();
        let s2 = FakeSource::new("semanticscholar")
            .optional_key("SEARCH_MCP_SEMANTICSCHOLAR_API_KEY")
            .failing_rate_limited(retry_after)
            .arc();
        let net = quiet_net();
        let cfg = config(&[("SEARCH_MCP_S2_API_KEY", "s2-key")]);
        let core = core_with(
            as_sources(vec![arxiv, s2.clone()]),
            net.clone(),
            MemStore::new(),
            cfg,
        );
        let res = core
            .search(ScholarQuery::new("attention"), &cx())
            .await
            .unwrap();
        assert_eq!(s2.seen().len(), 1);
        assert_eq!(
            core.benched_until(Provider::SemanticScholar).await,
            Some(NEXT_MIDNIGHT),
            "retry_after={retry_after:?}"
        );
        assert!(
            !net.requests()
                .iter()
                .any(|u| u.contains("api.semanticscholar.org")),
            "enrich() must not hit S2 after S2 429'd in fan_out: {:?}",
            net.requests()
        );
        assert!(
            res.meta
                .notes
                .iter()
                .any(|n| n.contains("semanticscholar: benched until 2026-11-26T00:00:00Z")),
            "{:?}",
            res.meta.notes
        );
    }

    // 4. Local GCRA RateLimited under a tight timeout (< 2 / rps) skips S2 in this
    // call's enrichment WITHOUT benching S2 until UTC midnight.
    let mut w = work(
        "Attention Is All You Need",
        Some(2017),
        Some("Ashish Vaswani"),
    );
    w.ids.doi = Some("10.1234/attn".into());
    let arxiv = FakeSource::new("arxiv").works(vec![w]).arc();
    let mut s2_lim = fsm_core::SourceLimits::new(1.0, 2, 2, 1000, 0);
    s2_lim.host_bucket = Some("api.semanticscholar.org".into());
    let s2_local = FakeSource::new("semanticscholar")
        .optional_key("SEARCH_MCP_SEMANTICSCHOLAR_API_KEY")
        .limits(s2_lim)
        .failing_rate_limited(Some(1))
        .arc();
    let net_local = quiet_net();
    let cfg = config(&[("SEARCH_MCP_S2_API_KEY", "s2-key")]);
    let core_local = core_with(
        as_sources(vec![arxiv, s2_local]),
        net_local.clone(),
        MemStore::new(),
        cfg,
    );
    core_local
        .search(ScholarQuery::new("attention"), &cx())
        .await
        .unwrap();
    assert_eq!(
        core_local.benched_until(Provider::SemanticScholar).await,
        None
    );
    assert!(
        !net_local
            .requests()
            .iter()
            .any(|u| u.contains("api.semanticscholar.org")),
        "S2 still skipped for enrichment in this call"
    );
}

/// Regression for I6: Retraction Watch handles Withdrawal / Removal and ignores Reinstatement.
#[tokio::test]
async fn retraction_watch_handles_withdrawal_removal_and_ignores_reinstatement() {
    let mk = |title: &str, doi: &str| {
        let mut w = work(title, Some(2020), Some("A Author"));
        w.ids.doi = Some(doi.into());
        w
    };
    let works = vec![
        mk("Study on withdrawal alpha", "10.1234/withdrawn"),
        mk("Study on removal beta", "10.1234/removed"),
        mk("Study on reinstatement gamma", "10.1234/reinstated"),
        mk("Study on correction delta", "10.1234/corrected"),
    ];
    let store = MemStore::new();
    {
        let mut r = store.retractions.lock().unwrap();
        r.insert(
            "10.1234/withdrawn".into(),
            RetractionNotice::new("Withdrawal", "retraction-watch"),
        );
        r.insert(
            "10.1234/removed".into(),
            RetractionNotice::new("Article Removal", "retraction-watch"),
        );
        r.insert(
            "10.1234/reinstated".into(),
            RetractionNotice::new("Reinstatement of Retracted Article", "retraction-watch"),
        );
        r.insert(
            "10.1234/corrected".into(),
            RetractionNotice::new("Correction", "retraction-watch"),
        );
    }
    let arxiv = FakeSource::new("arxiv").works(works).arc();
    let core = core_with(as_sources(vec![arxiv]), quiet_net(), store, config(&[]));
    let res = core
        .search(ScholarQuery::new("study"), &cx())
        .await
        .unwrap();
    let by_doi = |d: &str| {
        res.works
            .iter()
            .find(|w| w.ids.doi.as_deref() == Some(d))
            .unwrap()
    };
    assert!(by_doi("10.1234/withdrawn").retracted);
    assert!(by_doi("10.1234/removed").retracted);
    assert!(!by_doi("10.1234/reinstated").retracted);
    assert!(!by_doi("10.1234/corrected").retracted);
    // Retracted works appear after non-retracted works.
    assert!(!res.works[0].retracted && !res.works[1].retracted);
    assert!(res.works[2].retracted && res.works[3].retracted);
}

/// Regression for I8a & port of `test_the_key_changes_with_anything_that_changes_the_answer`:
/// complete keyword searches are cached for 12h in `api_cache`, re-check Retraction Watch
/// on hit, and vary the cache key on every query/pool dimension.
#[tokio::test]
async fn academic_search_cache_hits_on_repeat_and_varies_with_query_shape() {
    let mut w = work("Sparse Attention Mechanisms", Some(2023), Some("A Author"));
    w.ids.doi = Some("10.1234/sparse".into());
    let arxiv = FakeSource::new("arxiv").works(vec![w]).arc();
    let dblp = FakeSource::new("dblp")
        .scopes(&["academic.cs"])
        .works(vec![])
        .arc();
    let store = MemStore::new();
    let core = core_with(
        as_sources(vec![arxiv.clone(), dblp]),
        quiet_net(),
        store.clone(),
        config(&[]),
    );

    let r1 = core
        .search(ScholarQuery::new("sparse attention"), &cx())
        .await
        .unwrap();
    assert!(!r1.meta.cached);
    assert_eq!(arxiv.seen().len(), 1);
    let search_keys: Vec<String> = store
        .api_keys()
        .into_iter()
        .filter(|k| k.starts_with("scholar:search:v1:"))
        .collect();
    assert_eq!(search_keys.len(), 1);
    assert_eq!(store.api_get(&search_keys[0]).unwrap().1, 12 * 3_600);

    // Identical query (with case/whitespace variation) hits cache and refreshes retractions.
    store.retractions.lock().unwrap().insert(
        "10.1234/sparse".into(),
        RetractionNotice::new("Retraction", "retraction-watch"),
    );
    let r2 = core
        .search(ScholarQuery::new("  SPARSE   Attention "), &cx())
        .await
        .unwrap();
    assert!(r2.meta.cached);
    assert_eq!(arxiv.seen().len(), 1, "cache hit must not re-call sources");
    assert!(
        r2.works[0].retracted,
        "retraction watch refreshed on cache hit"
    );

    // Any parameter change misses cache and creates a distinct cache key.
    let mut variants = Vec::new();
    let mut q_sub = ScholarQuery::new("sparse attention");
    q_sub.subscope = Some("cs".into());
    variants.push(q_sub);
    let mut q_sort = ScholarQuery::new("sparse attention");
    q_sort.sort = Some("citations".into());
    variants.push(q_sort);
    let mut q_yf = ScholarQuery::new("sparse attention");
    q_yf.year_from = Some(2020);
    variants.push(q_yf);
    let mut q_yt = ScholarQuery::new("sparse attention");
    q_yt.year_to = Some(2025);
    variants.push(q_yt);
    let mut q_oa = ScholarQuery::new("sparse attention");
    q_oa.oa_only = true;
    variants.push(q_oa);
    let mut q_lim = ScholarQuery::new("sparse attention");
    q_lim.limit = 5;
    variants.push(q_lim);

    for (idx, q) in variants.into_iter().enumerate() {
        let r = core.search(q, &cx()).await.unwrap();
        assert!(!r.meta.cached, "variant {idx} must miss cache");
    }
    let final_keys: Vec<String> = store
        .api_keys()
        .into_iter()
        .filter(|k| k.starts_with("scholar:search:v1:"))
        .collect();
    assert_eq!(final_keys.len(), 7);
}

/// Regression for I8b:
/// - `sort = "citations"` picks top-N enrichment in relevance order so an arXiv preprint
///   (`cited_by = None`) is not starved behind `cited_by = Some(0)` Crossref items;
/// - `oaOnly = true` skips candidates already known `"closed"` when selecting enrichment picks.
#[tokio::test]
async fn enrichment_picks_relevance_order_for_citations_sort_and_skips_known_closed_for_oa_only() {
    // 1. sort = "citations": arXiv preprint (#1 relevance, cited_by = None) vs 5 Crossref items (cited_by = 0).
    let mut preprint = work(
        "Attention Is All You Need",
        Some(2017),
        Some("Ashish Vaswani"),
    );
    preprint.ids.arxiv = Some("1706.03762".into());
    preprint.cited_by = None;
    let mut cr_works = Vec::new();
    for i in 1..=5 {
        let mut w = work(
            &format!("Minor workshop note on attention {i}"),
            Some(2018),
            Some("B Author"),
        );
        w.ids.doi = Some(format!("10.1234/minor.{i}"));
        w.cited_by = Some(0);
        cr_works.push(w);
    }
    let arxiv = FakeSource::new("arxiv").works(vec![preprint]).arc();
    let crossref = FakeSource::new("crossref").works(cr_works).arc();
    let oa_aiayn = fixture_body("arxiv_1706.03762/openalex_datacite_doi.json");
    let net = ScriptNet::new(move |url| {
        if url.contains("10.48550") {
            (Duration::from_millis(5), 200, oa_aiayn.clone())
        } else {
            (Duration::from_millis(5), 404, String::new())
        }
    });
    let core = core_with(
        as_sources(vec![arxiv, crossref]),
        net,
        MemStore::new(),
        config(&[]),
    );
    let mut q = ScholarQuery::new("attention is all you need");
    q.sort = Some("citations".into());
    let res = core.search(q, &cx()).await.unwrap();
    assert_eq!(res.works[0].title, "Attention Is All You Need");
    assert!(
        res.works[0].cited_by.unwrap_or(0) > 100_000,
        "seminal arXiv preprint must be enriched despite initial cited_by = None"
    );

    // 2. oaOnly = true: 5 known-closed works precede 1 unknown work that OpenAlex reports as open.
    let mut mixed = Vec::new();
    for i in 1..=5 {
        let mut w = work(
            &format!("Closed neural study {i}"),
            Some(2022),
            Some("A Author"),
        );
        w.ids.doi = Some(format!("10.1234/closed.{i}"));
        w.oa = Some(fsm_core::OaLocation::new("closed"));
        mixed.push(w);
    }
    let mut sixth = work("Open neural study", Some(2022), Some("B Author"));
    sixth.ids.doi = Some("10.1234/open.6".into());
    sixth.oa = None;
    mixed.push(sixth);
    let cr_oa = FakeSource::new("crossref").works(mixed).arc();
    let oa_gold = r#"{
        "id": "https://openalex.org/W9999999999",
        "doi": "https://doi.org/10.1234/open.6",
        "display_name": "Open neural study",
        "publication_year": 2022,
        "open_access": {"is_oa": true, "oa_status": "gold", "oa_url": "https://example.org/open6"}
    }"#;
    let net_oa = ScriptNet::new(move |url| {
        if url.contains("10.1234/open.6") && url.contains("api.openalex.org") {
            (Duration::from_millis(5), 200, oa_gold.into())
        } else {
            (Duration::from_millis(5), 404, String::new())
        }
    });
    let core_oa = core_with(
        as_sources(vec![cr_oa]),
        net_oa,
        MemStore::new(),
        config(&[]),
    );
    let mut q_oa = ScholarQuery::new("neural study");
    q_oa.oa_only = true;
    q_oa.limit = 10;
    let res_oa = core_oa.search(q_oa, &cx()).await.unwrap();
    assert_eq!(titles(&res_oa.works), ["Open neural study"]);
    assert_eq!(
        res_oa.works[0].oa.as_ref().map(|o| o.status.as_str()),
        Some("gold")
    );
}

/// Regression for M1: `subscope: "biomed"` and `"trial"` do not emit spurious
/// `"biorxiv: skipped, it has no search op"` warnings.
#[tokio::test]
async fn subscope_biomed_and_trial_do_not_warn_about_lookup_only_biorxiv() {
    let mut all = pool_of_four();
    let biorxiv = FakeSource::new("biorxiv")
        .scopes(&["academic.biomed", "academic.preprint"])
        .caps(&[fsm_core::Capability::Lookup, fsm_core::Capability::Fulltext])
        .pool_role(fsm_core::PoolRole::Explicit)
        .op(OpDecl::new("work", "lookup", ReturnKind::Works))
        .arc();
    let pubmed = FakeSource::new("pubmed").scopes(&["academic.biomed"]).arc();
    let trials = FakeSource::new("clinicaltrials")
        .scopes(&["academic.trial"])
        .arc();
    all.extend([biorxiv.clone(), pubmed.clone(), trials.clone()]);
    let core = core_with(as_sources(all), quiet_net(), MemStore::new(), config(&[]));

    for sub in ["biomed", "trial"] {
        let mut q = ScholarQuery::new("crispr");
        q.subscope = Some(sub.into());
        let res = core.search(q, &cx()).await.unwrap();
        assert!(biorxiv.seen().is_empty());
        assert!(
            !res.meta.notes.iter().any(|n| n.contains("biorxiv")),
            "subscope {sub}: {:?}",
            res.meta.notes
        );
    }
    assert_eq!(pubmed.seen().len(), 2);
    assert_eq!(trials.seen().len(), 2);
}

/// Port of `test_merge_identity.py::test_the_captured_arxiv_duplicate_now_merges`
/// across realistic arXiv, OpenAIRE, and Crossref manifest work shapes.
#[tokio::test]
async fn test_the_captured_arxiv_duplicate_now_merges() {
    let mut ax = work(
        "Deep Residual Learning for Image Recognition",
        Some(2015),
        Some("Kaiming He"),
    );
    ax.work_type = "preprint".into();
    ax.ids.arxiv = Some("1512.03385".into());
    ax.url = "http://arxiv.org/abs/1512.03385v1".into();
    let mut ax_oa = fsm_core::OaLocation::new("green");
    ax_oa.url = Some("https://arxiv.org/abs/1512.03385".into());
    ax_oa.pdf_url = Some("https://arxiv.org/pdf/1512.03385".into());
    ax_oa.structured_format = Some("html5".into());
    ax.oa = Some(ax_oa);

    let mut oaire = work(
        "Deep Residual Learning for Image Recognition",
        Some(2015),
        Some("Kaiming HE"),
    );
    oaire.ids.doi = Some("https://doi.org/10.48550/arXiv.1512.03385".into());
    oaire.ids.openaire = Some("doi_dedup___::abc123".into());
    oaire.url = "https://explore.openaire.eu/search/result?id=abc123".into();

    let mut cr = work(
        "Deep Residual Learning for Image Recognition",
        Some(2016),
        Some("He, Kaiming"),
    );
    cr.ids.doi = Some("10.1109/CVPR.2016.90".into());
    cr.venue =
        Some("2016 IEEE Conference on Computer Vision and Pattern Recognition (CVPR)".into());
    cr.cited_by = Some(80_000);
    cr.cited_by_source = Some("crossref".into());
    cr.url = "http://dx.doi.org/10.1109/CVPR.2016.90".into();

    let core = core_with(
        as_sources(vec![
            FakeSource::new("arxiv").works(vec![ax]).arc(),
            FakeSource::new("openaire").works(vec![oaire]).arc(),
            FakeSource::new("crossref").works(vec![cr]).arc(),
        ]),
        quiet_net(),
        MemStore::new(),
        config(&[]),
    );
    let res = core
        .search(ScholarQuery::new("deep residual learning"), &cx())
        .await
        .unwrap();
    assert_eq!(
        res.works.len(),
        1,
        "all three variants must merge into one work"
    );
    let w = &res.works[0];
    assert_eq!(w.ids.arxiv.as_deref(), Some("1512.03385"));
    assert_eq!(w.ids.doi.as_deref(), Some("10.1109/cvpr.2016.90"));
    assert_eq!(w.sources, ["crossref", "arxiv", "openaire"]);
    assert!(w.url.starts_with("https://"));
    assert_eq!(
        w.oa.as_ref().and_then(|o| o.structured_format.as_deref()),
        Some("html5")
    );
}
