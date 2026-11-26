//! Offline tests for `ScholarGraph::cite` (hayagriva 0.10 + DOI content negotiation)
//! and `ScholarGraph::entity` (M13 `Arc<dyn SourcesApi>` replay fixtures covering
//! `orcid`, `ror`, `dblp`, `icore_ranks`, `nih_reporter`, `nsf_awards`, `cordis`, `ukri`).

mod common;

use common::{
    FnNet, MemoryStore, MockSources, make_scholar_graph, status_resp, test_config, text_ok,
};
use fsm_core::fixture::{Exchange, Fixture, FixtureRequest, FixtureResponse, ReplayNet};
use fsm_core::{
    AuthorProfile, AuthorRef, CallCx, CiteInput, CiteStyle, EntityKind, FsmError, GrantRecord,
    OrgProfile, ScholarlyWork, VenueProfile,
};
use futures::FutureExt;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Citations (`hayagriva 0.10` + DOI content negotiation)
// ---------------------------------------------------------------------------

fn attention_work() -> ScholarlyWork {
    let mut w = ScholarlyWork::new(
        "Attention Is All You Need",
        "https://arxiv.org/abs/1706.03762",
    );
    w.authors = vec![
        AuthorRef::new("Ashish Vaswani"),
        AuthorRef::new("Noam Shazeer"),
        AuthorRef::new("Niki Parmar"),
    ];
    w.year = Some(2017);
    w.published_date = Some("2017-06-12".into());
    w.venue = Some("Advances in Neural Information Processing Systems".into());
    w.work_type = "inproceedings".into();
    w.ids.arxiv = Some("1706.03762".into());
    w.ids.doi = Some("10.48550/arxiv.1706.03762".into());
    w
}

#[tokio::test]
async fn test_cite_offline_work_all_six_styles_zero_network() {
    let tmp = tempfile::tempdir().unwrap();
    let net = Arc::new(FnNet::new(|req, _cx| {
        async move {
            panic!(
                "offline CiteInput::Work must never hit network: {}",
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
    let work = attention_work();

    let bib = sg
        .cite(
            CiteInput::Work(Box::new(work.clone())),
            CiteStyle::Bibtex,
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(
        bib.starts_with("@inproceedings{vaswani2017attention,"),
        "{bib}"
    );
    assert!(
        bib.contains("Ashish Vaswani and Noam Shazeer and Niki Parmar"),
        "{bib}"
    );
    assert!(bib.contains("eprint = {1706.03762}"), "{bib}");

    let csl_str = sg
        .cite(
            CiteInput::Work(Box::new(work.clone())),
            CiteStyle::Csl,
            &CallCx::default(),
        )
        .await
        .unwrap();
    let csl: Value = serde_json::from_str(&csl_str).unwrap();
    assert_eq!(csl["title"], "Attention Is All You Need");
    assert_eq!(csl["author"][0]["family"], "Vaswani");
    assert_eq!(csl["issued"]["date-parts"][0], json!([2017, 6, 12]));

    for style in [
        CiteStyle::Apa,
        CiteStyle::Ieee,
        CiteStyle::Chicago,
        CiteStyle::Vancouver,
    ] {
        let out = sg
            .cite(
                CiteInput::Work(Box::new(work.clone())),
                style,
                &CallCx::default(),
            )
            .await
            .unwrap();
        assert!(
            !out.contains('\x1b'),
            "ANSI codes leaked in {style:?}: {out:?}"
        );
        assert!(
            out.contains("Vaswani"),
            "missing author in {style:?}: {out}"
        );
        assert!(out.contains("2017"), "missing year in {style:?}: {out}");
        assert!(
            out.to_ascii_lowercase()
                .contains("attention is all you need"),
            "missing title in {style:?}: {out}"
        );
    }
}

#[tokio::test]
async fn test_cite_doi_content_negotiation_bibtex_csl_and_hayagriva_via_replay_net() {
    let tmp = tempfile::tempdir().unwrap();
    let bibtex_body = "@article{Kucsko_2013,\n  title={Nanometre-scale thermometry in a living cell},\n  volume={500},\n  DOI={10.1038/nature12373},\n  journal={Nature},\n  author={Kucsko, G. and Maurer, P. C. and Yao, N. Y.},\n  year={2013}\n}";
    // Realistic Crossref CSL-JSON including extra fields (score float, array ISSN, link array)
    // to prove `sanitize_csl_json_item` cleanly prepares it for `citationberg::json::Item`.
    let csl_body = json!({
        "DOI": "10.1038/nature12373",
        "type": "article-journal",
        "title": "Nanometre-scale thermometry in a living cell",
        "container-title": "Nature",
        "volume": "500",
        "issue": "7460",
        "page": "54-58",
        "ISSN": ["0028-0836", "1476-4687"],
        "score": 1.0,
        "link": [{"URL": "https://www.nature.com/articles/nature12373.pdf"}],
        "author": [
            {"given": "G.", "family": "Kucsko"},
            {"given": "P. C.", "family": "Maurer"}
        ],
        "issued": {"date-parts": [[2013, 7, 31]]}
    });

    let mut req_bib = FixtureRequest::new("GET", "https://doi.org/10.1038/nature12373");
    req_bib.headers_subset = BTreeMap::from([("accept".into(), "application/x-bibtex".into())]);

    let mut req_csl = FixtureRequest::new("GET", "https://doi.org/10.1038/nature12373");
    req_csl.headers_subset = BTreeMap::from([(
        "accept".into(),
        "application/vnd.citationstyles.csl+json".into(),
    )]);

    let mut fx = Fixture::new("doi_cn", "cite", "2026-11-25T12:00:00Z");
    fx.exchanges = vec![
        Exchange::new(
            req_bib,
            FixtureResponse::from_bytes(200, vec![], bibtex_body.as_bytes()),
        ),
        Exchange::new(
            req_csl,
            FixtureResponse::from_bytes(200, vec![], &serde_json::to_vec(&csl_body).unwrap()),
        ),
    ];

    let net = Arc::new(ReplayNet::from_fixtures(vec![fx]).unwrap());
    let sg = make_scholar_graph(
        net.clone(),
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );

    let bib = sg
        .cite(
            CiteInput::Id("10.1038/nature12373".into()),
            CiteStyle::Bibtex,
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(bib.contains("@article{Kucsko_2013,"), "{bib}");

    let csl_raw = sg
        .cite(
            CiteInput::Id("10.1038/nature12373".into()),
            CiteStyle::Csl,
            &CallCx::default(),
        )
        .await
        .unwrap();
    let csl_v: Value = serde_json::from_str(&csl_raw).unwrap();
    assert_eq!(csl_v["volume"], "500");

    let apa = sg
        .cite(
            CiteInput::Id("10.1038/nature12373".into()),
            CiteStyle::Apa,
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(apa.contains("Kucsko"), "{apa}");
    assert!(apa.contains("2013"), "{apa}");
    assert!(apa.contains("500"), "{apa}");
    assert!(apa.contains("54"), "{apa}");
    assert!(!apa.contains('\x1b'));

    let ieee = sg
        .cite(
            CiteInput::Id("10.1038/nature12373".into()),
            CiteStyle::Ieee,
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(ieee.contains("Kucsko") && ieee.contains("Nature"), "{ieee}");
    assert!(net.unmatched().is_empty());
}

#[tokio::test]
async fn test_cite_falls_back_to_store_and_arxiv_when_doi_cn_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::new());
    store.insert_work("doi:10.1145/1571941.1572114", attention_work());

    let net = Arc::new(FnNet::new(|req, _cx| {
        async move {
            let url = fsm_core::fixture::request_url(&req).unwrap().to_string();
            if url.contains("doi.org") {
                return Ok(status_resp(&url, 406, "Not Acceptable"));
            }
            if url.contains("export.arxiv.org") {
                let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <entry>
    <id>http://arxiv.org/abs/1706.03762v7</id>
    <title>Attention Is All You Need</title>
    <published>2017-06-12T17:57:34Z</published>
    <author><name>Ashish Vaswani</name></author>
  </entry>
</feed>"#;
                return Ok(text_ok(&url, "application/atom+xml", xml));
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

    // Cached work fallback when doi.org returns 406.
    let vancouver = sg
        .cite(
            CiteInput::Id("10.1145/1571941.1572114".into()),
            CiteStyle::Vancouver,
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(
        vancouver.contains("Vaswani") && vancouver.contains("2017"),
        "{vancouver}"
    );

    // arXiv fallback via Atom XML.
    let chicago = sg
        .cite(
            CiteInput::Id("arXiv:1706.03762".into()),
            CiteStyle::Chicago,
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(
        chicago.contains("Vaswani") && chicago.contains("2017"),
        "{chicago}"
    );

    // Empty input rejected.
    let err = sg
        .cite(
            CiteInput::Id("  ".into()),
            CiteStyle::Bibtex,
            &CallCx::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, FsmError::InvalidInput(_)));
}

// ---------------------------------------------------------------------------
// Entities (M13 `Arc<dyn SourcesApi>` replay fixtures for all 8 entity sources)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_entity_author_by_orcid_and_by_name_fuses_orcid_and_dblp() {
    let tmp = tempfile::tempdir().unwrap();
    let net = Arc::new(FnNet::new(|req, _cx| {
        async move {
            panic!(
                "entity() must dispatch via SourcesApi, not raw NetClient: {}",
                req.url
            )
        }
        .boxed()
    }));
    let sources = Arc::new(MockSources::new());

    // Replay shapes matching `crates/manifests/sources/w5a/{orcid,dblp}.toml` M13 outputs.
    sources.on_ok(
        "orcid.search",
        vec![json!({
            "name": "Geoffrey Hinton",
            "orcid": "0000-0002-1825-0097",
            "affiliations": ["University of Toronto"],
            "topWorks": [],
            "sources": ["orcid"]
        })],
    );
    sources.on_ok(
        "orcid.author",
        vec![json!({
            "name": "Geoffrey E. Hinton",
            "orcid": "0000-0002-1825-0097",
            "affiliations": ["University of Toronto", "Vector Institute"],
            "worksCount": 312,
            "topWorks": [{
                "title": "Deep learning",
                "authors": [{"name": "Geoffrey E. Hinton", "orcid": "0000-0002-1825-0097"}],
                "year": 2015,
                "venue": "Nature",
                "workType": "article",
                "ids": {"doi": "10.1038/nature14539"},
                "retracted": false,
                "versions": [],
                "url": "https://doi.org/10.1038/nature14539",
                "sources": ["orcid"]
            }],
            "sources": ["orcid"]
        })],
    );
    sources.on_ok(
        "dblp.author",
        vec![json!({
            "name": "Geoffrey E. Hinton",
            "orcid": "0000-0002-1825-0097",
            "affiliations": ["Google DeepMind"],
            "worksCount": 420,
            "topWorks": [],
            "sources": ["dblp"]
        })],
    );

    let sg = make_scholar_graph(
        net,
        Arc::new(MemoryStore::new()),
        sources.clone(),
        test_config(tmp.path(), false),
    );

    // By ORCID iD.
    let val = sg
        .entity(
            EntityKind::Author,
            "https://orcid.org/0000-0002-1825-0097",
            json!({}),
            &CallCx::default(),
        )
        .await
        .unwrap();
    let prof: AuthorProfile = serde_json::from_value(val).unwrap();
    assert_eq!(prof.orcid.as_deref(), Some("0000-0002-1825-0097"));
    assert_eq!(prof.works_count, Some(420)); // max(312, 420)
    assert_eq!(prof.top_works.len(), 1);
    assert!(prof.affiliations.contains(&"Vector Institute".to_owned()));
    assert!(prof.affiliations.contains(&"Google DeepMind".to_owned()));
    assert_eq!(prof.sources, vec!["orcid", "dblp"]);

    // By name query.
    let val2 = sg
        .entity(
            EntityKind::Author,
            "Geoffrey Hinton",
            json!({}),
            &CallCx::default(),
        )
        .await
        .unwrap();
    let prof2: AuthorProfile = serde_json::from_value(val2).unwrap();
    assert_eq!(prof2.orcid.as_deref(), Some("0000-0002-1825-0097"));
    assert_eq!(prof2.works_count, Some(420));
    assert_eq!(prof2.top_works.len(), 1);
}

#[tokio::test]
async fn test_entity_org_dispatches_ror_search_org_and_affiliation() {
    let tmp = tempfile::tempdir().unwrap();
    let net = Arc::new(FnNet::new(|req, _cx| {
        async move { panic!("unexpected NetClient call: {}", req.url) }.boxed()
    }));
    let sources = Arc::new(MockSources::new());
    let tsinghua = json!({
        "name": "Tsinghua University",
        "ror": "https://ror.org/03cve4549",
        "country": "CN",
        "orgType": "education",
        "homepage": "https://www.tsinghua.edu.cn/en/",
        "sources": ["ror"]
    });
    sources.on_ok("ror.search", vec![tsinghua.clone()]);
    sources.on_ok("ror.org", vec![tsinghua.clone()]);
    sources.on_ok("ror.affiliation", vec![tsinghua]);

    let sg = make_scholar_graph(
        net,
        Arc::new(MemoryStore::new()),
        sources.clone(),
        test_config(tmp.path(), false),
    );

    let by_name: OrgProfile = serde_json::from_value(
        sg.entity(
            EntityKind::Org,
            "Tsinghua University",
            json!({}),
            &CallCx::default(),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(by_name.ror.as_deref(), Some("https://ror.org/03cve4549"));
    assert_eq!(by_name.country.as_deref(), Some("CN"));

    let by_id: OrgProfile = serde_json::from_value(
        sg.entity(
            EntityKind::Org,
            "https://ror.org/03cve4549",
            json!({}),
            &CallCx::default(),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(by_id.name, "Tsinghua University");

    let by_aff: OrgProfile = serde_json::from_value(
        sg.entity(
            EntityKind::Org,
            "Department of Computer Science, Tsinghua University, Beijing",
            json!({}),
            &CallCx::default(),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(by_aff.ror.as_deref(), Some("https://ror.org/03cve4549"));

    let ops: Vec<String> = sources.calls().into_iter().map(|(op, _)| op).collect();
    assert_eq!(ops, vec!["ror.search", "ror.org", "ror.affiliation"]);
}

#[tokio::test]
async fn test_entity_venue_fuses_dblp_and_icore_ranks_and_issn_portal() {
    let tmp = tempfile::tempdir().unwrap();
    let net = Arc::new(FnNet::new(|req, _cx| {
        async move { panic!("unexpected NetClient call: {}", req.url) }.boxed()
    }));
    let sources = Arc::new(MockSources::new());

    sources.on_ok(
        "dblp.venue",
        vec![json!({
            "name": "International Conference on Machine Learning",
            "issn": ["2640-3498"],
            "worksCount": 14820,
            "sources": ["dblp"]
        })],
    );
    // Raw HTML-extracted row format emitted by `crates/manifests/sources/w5b/icore_ranks.toml`.
    sources.on_ok(
        "icore_ranks.rank",
        vec![
            json!({
                "name": "Australian International Conference on Machine Learning",
                "acronym": "AICML",
                "edition": "ICORE2026",
                "icoreRank": "C",
                "dblpUrl": null
            }),
            json!({
                "name": "International Conference on Machine Learning",
                "acronym": "ICML",
                "edition": "ICORE2026",
                "icoreRank": "A*",
                "dblpUrl": "https://dblp.org/db/conf/icml/"
            }),
        ],
    );
    sources.on_ok(
        "issn_portal.lookup",
        vec![json!({
            "name": "Nature",
            "issn": ["0028-0836", "1476-4687"],
            "publisher": null,
            "sources": ["issn_portal"]
        })],
    );
    sources.on_ok(
        "doaj.journals",
        vec![json!({
            "name": "Nature",
            "publisher": "Springer Nature",
            "issn": ["0028-0836"]
        })],
    );

    let sg = make_scholar_graph(
        net,
        Arc::new(MemoryStore::new()),
        sources,
        test_config(tmp.path(), false),
    );

    // Conference acronym picks exact ICML A* over decoy AICML C and fuses DBLP worksCount.
    let icml: VenueProfile = serde_json::from_value(
        sg.entity(EntityKind::Venue, "ICML", json!({}), &CallCx::default())
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(icml.name, "International Conference on Machine Learning");
    assert_eq!(icml.icore_rank.as_deref(), Some("A*"));
    assert_eq!(icml.works_count, Some(14820));
    assert!(icml.issn.contains(&"2640-3498".to_owned()));
    assert!(icml.sources.contains(&"dblp".to_owned()));
    assert!(icml.sources.contains(&"icore_ranks".to_owned()));

    // ISSN lookup fuses issn_portal + DOAJ.
    let nature: VenueProfile = serde_json::from_value(
        sg.entity(
            EntityKind::Venue,
            "0028-0836",
            json!({}),
            &CallCx::default(),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(nature.name, "Nature");
    assert_eq!(nature.doaj_vetted, Some(true));
    assert_eq!(nature.publisher.as_deref(), Some("Springer Nature"));
    assert!(nature.issn.contains(&"0028-0836".to_owned()));
}

#[tokio::test]
async fn test_entity_grants_dispatches_all_four_funders_and_enforces_ukri_min_10() {
    let tmp = tempfile::tempdir().unwrap();
    let net = Arc::new(FnNet::new(|req, _cx| {
        async move { panic!("unexpected NetClient call: {}", req.url) }.boxed()
    }));
    let sources = Arc::new(MockSources::new());

    sources.on_ok(
        "nih_reporter.search",
        vec![json!({
            "id": "5R01AI187493-03",
            "title": "CRISPR Gene Editing Mechanisms",
            "funder": "NIAID",
            "pi": "Jane Doe",
            "org": "MIT",
            "amount": 520000.0,
            "currency": "USD",
            "startDate": "2024-05-01",
            "endDate": "2027-04-30",
            "url": "https://reporter.nih.gov/project-details/1001",
            "source": "nih_reporter"
        })],
    );
    sources.on_ok(
        "nsf_awards.search",
        vec![json!({
            "id": "2621509",
            "title": "CRISPR Diagnostics at Scale",
            "funder": "NSF",
            "pi": "John Smith",
            "org": "Stanford University",
            "amount": 350000.0,
            "currency": "USD",
            "startDate": "2025-01-01",
            "endDate": "2028-01-01",
            "url": "https://www.nsf.gov/awardsearch/showAward?AWD_ID=2621509",
            "source": "nsf_awards"
        })],
    );
    sources.on_ok(
        "cordis.search",
        vec![json!({
            "id": "101201649",
            "title": "Horizon Europe Quantum CRISPR",
            "funder": "European Commission (CORDIS)",
            "org": "ETH Zurich",
            "amount": 2500000.0,
            "currency": "EUR",
            "startDate": "2025-03-01",
            "endDate": "2029-02-28",
            "url": "https://cordis.europa.eu/project/id/101201649",
            "source": "cordis"
        })],
    );
    sources.on_ok(
        "ukri.search",
        vec![json!({
            "id": "EP/X012345/1",
            "title": "UKRI Genome Engineering Platform",
            "funder": "EPSRC",
            "org": "University of Oxford",
            "currency": "GBP",
            "url": "https://gtr.ukri.org/projects?ref=EP/X012345/1",
            "source": "ukri"
        })],
    );

    let sg = make_scholar_graph(
        net,
        Arc::new(MemoryStore::new()),
        sources.clone(),
        test_config(tmp.path(), false),
    );

    // Caller asks for limit=4 -> UKRI must STILL receive limit=10 (GtR API minimum).
    let val = sg
        .entity(
            EntityKind::Grants,
            "crispr",
            json!({"limit": 4}),
            &CallCx::default(),
        )
        .await
        .unwrap();
    let grants: Vec<GrantRecord> = serde_json::from_value(val).unwrap();
    assert_eq!(grants.len(), 4);
    let srcs: Vec<&str> = grants.iter().map(|g| g.source.as_str()).collect();
    assert_eq!(srcs, vec!["nih_reporter", "nsf_awards", "cordis", "ukri"]);

    let ukri_call = sources
        .calls()
        .into_iter()
        .find(|(op, _)| op == "ukri.search")
        .expect("ukri.search called");
    assert_eq!(ukri_call.1["limit"], 10);
}

#[tokio::test]
async fn test_entity_grants_subset_and_partial_failure_and_empty_query() {
    let tmp = tempfile::tempdir().unwrap();
    let net = Arc::new(FnNet::new(|req, _cx| {
        async move { panic!("unexpected NetClient call: {}", req.url) }.boxed()
    }));
    let sources = Arc::new(MockSources::new());
    sources.on_err("nih_reporter.search", "upstream 503");
    sources.on_ok(
        "cordis.search",
        vec![json!({
            "id": "101201649",
            "title": "EU Quantum Project",
            "funder": "European Commission (CORDIS)",
            "source": "cordis"
        })],
    );

    let sg = make_scholar_graph(
        net,
        Arc::new(MemoryStore::new()),
        sources.clone(),
        test_config(tmp.path(), false),
    );
    let val = sg
        .entity(
            EntityKind::Grants,
            "quantum",
            json!({"sources": ["nih_reporter", "cordis"], "limit": 5}),
            &CallCx::default(),
        )
        .await
        .unwrap();
    let grants: Vec<GrantRecord> = serde_json::from_value(val).unwrap();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].source, "cordis");
    assert_eq!(sources.calls().len(), 2);

    let err = sg
        .entity(EntityKind::Author, "   ", json!({}), &CallCx::default())
        .await
        .unwrap_err();
    assert!(matches!(err, FsmError::InvalidInput(_)));
}

#[tokio::test]
async fn test_cite_special_chars_nobiliary_particles_and_unresolved_error() {
    let tmp = tempfile::tempdir().unwrap();
    let net = Arc::new(FnNet::new(|req, _cx| {
        async move { Ok(status_resp(&req.url, 404, "not found")) }.boxed()
    }));
    let sg = make_scholar_graph(
        net,
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );

    let mut w = ScholarlyWork::new("R&D: 100% Fast_Search #1", "https://example.org/rd");
    w.authors = vec![
        AuthorRef::new("Ludwig van Beethoven"),
        AuthorRef::new("Lovelace, Ada"),
    ];
    w.year = Some(2025);
    w.venue = Some("Journal of R&D".into());
    w.work_type = "article".into();

    let bib = sg
        .cite(
            CiteInput::Work(Box::new(w.clone())),
            CiteStyle::Bibtex,
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(bib.contains(r"R\&D: 100\% Fast\_Search \#1"), "{bib}");
    assert!(bib.contains(r"Journal of R\&D"), "{bib}");

    let apa = sg
        .cite(
            CiteInput::Work(Box::new(w)),
            CiteStyle::Apa,
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(
        apa.contains("Beethoven") && apa.contains("Lovelace"),
        "{apa}"
    );
    assert!(!apa.contains('\x1b'));

    let err = sg
        .cite(
            CiteInput::Id("10.9999/unresolvable".into()),
            CiteStyle::Apa,
            &CallCx::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, FsmError::InvalidInput(_)));
}

#[tokio::test]
async fn test_entity_partial_failures_null_opts_and_dedup() {
    let tmp = tempfile::tempdir().unwrap();
    let net = Arc::new(FnNet::new(|req, _cx| {
        async move { panic!("unexpected NetClient call: {}", req.url) }.boxed()
    }));
    let sources = Arc::new(MockSources::new());

    // Author: orcid fails, dblp succeeds.
    sources.on_err("orcid.search", "orcid 503");
    sources.on_ok(
        "dblp.author",
        vec![json!({
            "name": "Yann LeCun",
            "worksCount": 350,
            "affiliations": ["NYU"],
            "topWorks": [],
            "sources": ["dblp"]
        })],
    );
    // Org: affiliation empty -> falls back to ror.search.
    sources.on_ok("ror.affiliation", vec![]);
    sources.on_ok(
        "ror.search",
        vec![json!({"name": "MIT", "ror": "https://ror.org/042nb2s44", "sources": ["ror"]})],
    );
    // Grants: Value::Null opts, duplicate IDs deduped.
    sources.on_ok(
        "nih_reporter.search",
        vec![
            json!({"id": "R01-1", "title": "Grant A", "funder": "NIH", "source": "nih_reporter"}),
            json!({"id": "R01-1", "title": "Grant A dup", "funder": "NIH", "source": "nih_reporter"}),
        ],
    );
    sources.on_err("nsf_awards.search", "timeout");
    sources.on_err("cordis.search", "timeout");
    sources.on_err("ukri.search", "timeout");

    let sg = make_scholar_graph(
        net,
        Arc::new(MemoryStore::new()),
        sources,
        test_config(tmp.path(), false),
    );

    let author: AuthorProfile = serde_json::from_value(
        sg.entity(
            EntityKind::Author,
            "Yann LeCun",
            Value::Null,
            &CallCx::default(),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(author.name, "Yann LeCun");
    assert_eq!(author.sources, vec!["dblp"]);

    let org: OrgProfile = serde_json::from_value(
        sg.entity(
            EntityKind::Org,
            "MIT",
            json!({"affiliation": true}),
            &CallCx::default(),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(org.ror.as_deref(), Some("https://ror.org/042nb2s44"));

    let grants: Vec<GrantRecord> = serde_json::from_value(
        sg.entity(EntityKind::Grants, "ai", Value::Null, &CallCx::default())
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].id, "R01-1");
}

// ---------------------------------------------------------------------------
// Review round 1 regression tests (I2, I3, I4, M8, M10)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_orcid_profile_does_not_absorb_dblp_homonym_by_orcid_or_name() {
    // I2 (Missing test #5): An ORCID-identified author MUST NOT merge a DBLP homonym
    // with a different ORCID or a disambiguated name ("Wei Wang 0001"), while an exact
    // name match without an ORCID IS merged.
    let tmp = tempfile::tempdir().unwrap();
    let net = Arc::new(FnNet::new(|req, _cx| {
        async move { Ok(status_resp(&req.url, 404, "")) }.boxed()
    }));

    // Case 1: ORCID lookup with different-ORCID DBLP homonym.
    let sources = Arc::new(MockSources::new());
    sources.on_ok(
        "orcid.author",
        vec![json!({
            "name": "Wei Wang",
            "orcid": "0000-0001-0000-0001",
            "affiliations": ["University A"],
            "worksCount": 12,
            "topWorks": [],
            "sources": ["orcid"]
        })],
    );
    sources.on_ok(
        "dblp.author",
        vec![
            json!({
                "name": "Wei Wang",
                "orcid": "0000-0009-9999-9999",
                "affiliations": ["University B"],
                "worksCount": 900,
                "topWorks": [],
                "sources": ["dblp"]
            }),
            json!({
                "name": "Wei Wang 0001",
                "affiliations": ["University C"],
                "worksCount": 500,
                "topWorks": [],
                "sources": ["dblp"]
            }),
        ],
    );
    let sg = make_scholar_graph(
        net.clone(),
        Arc::new(MemoryStore::new()),
        sources,
        test_config(tmp.path(), false),
    );
    let v = sg
        .entity(
            EntityKind::Author,
            "0000-0001-0000-0001",
            Value::Null,
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        v["affiliations"],
        json!(["University A"]),
        "different-ORCID or disambiguated DBLP homonym must not be merged: {v}"
    );
    assert_eq!(v["worksCount"], json!(12));
    assert_eq!(v["sources"], json!(["orcid"]));

    // Case 2: Name lookup with different-ORCID DBLP homonym -> also NOT merged.
    let sources2 = Arc::new(MockSources::new());
    sources2.on_ok(
        "orcid.search",
        vec![json!({
            "name": "Wei Wang",
            "orcid": "0000-0001-0000-0001",
            "affiliations": ["University A"],
            "worksCount": 12,
            "topWorks": [],
            "sources": ["orcid"]
        })],
    );
    sources2.on_ok(
        "dblp.author",
        vec![json!({
            "name": "Wei Wang",
            "orcid": "0000-0009-9999-9999",
            "affiliations": ["University B"],
            "worksCount": 900,
            "topWorks": [],
            "sources": ["dblp"]
        })],
    );
    let sg2 = make_scholar_graph(
        net.clone(),
        Arc::new(MemoryStore::new()),
        sources2,
        test_config(tmp.path(), false),
    );
    let v2 = sg2
        .entity(
            EntityKind::Author,
            "Wei Wang",
            Value::Null,
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert_eq!(v2["affiliations"], json!(["University A"]));
    assert_eq!(v2["worksCount"], json!(12));

    // Case 3: DBLP candidate with NO ORCID and exact normalized name match -> IS merged.
    let sources3 = Arc::new(MockSources::new());
    sources3.on_ok(
        "orcid.author",
        vec![json!({
            "name": "Ada Lovelace",
            "orcid": "0000-0001-0000-0002",
            "affiliations": ["University A"],
            "worksCount": 10,
            "topWorks": [],
            "sources": ["orcid"]
        })],
    );
    sources3.on_ok(
        "dblp.author",
        vec![json!({
            "name": "Ada Lovelace",
            "affiliations": ["Analytical Engine Lab"],
            "worksCount": 15,
            "topWorks": [],
            "sources": ["dblp"]
        })],
    );
    let sg3 = make_scholar_graph(
        net,
        Arc::new(MemoryStore::new()),
        sources3,
        test_config(tmp.path(), false),
    );
    let v3 = sg3
        .entity(
            EntityKind::Author,
            "0000-0001-0000-0002",
            Value::Null,
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert_eq!(v3["worksCount"], json!(15));
    assert_eq!(v3["sources"], json!(["orcid", "dblp"]));
}

#[tokio::test]
async fn test_entity_grants_enforces_manifest_limit_bounds_and_validates_sources() {
    // I3 & I4 (Missing test #6): Bounds-validating MockSources matching exact staged
    // manifests (`nih_reporter: 1..=50`, `nsf_awards: 1..=25`, `cordis: 1..=50`, `ukri: 10..=100`).
    // `limit: 30` clamps `nsf_awards` to `<= 25` so NSF results are not silently dropped;
    // duplicate `sources` are deduplicated; unknown/non-grant `sources` return `InvalidInput`.
    let tmp = tempfile::tempdir().unwrap();
    let net = Arc::new(FnNet::new(|req, _cx| {
        async move { panic!("unexpected NetClient call: {}", req.url) }.boxed()
    }));
    let sources = Arc::new(MockSources::new());
    sources.set_limit_bounds("nih_reporter.search", 1, 50);
    sources.set_limit_bounds("nsf_awards.search", 1, 25);
    sources.set_limit_bounds("cordis.search", 1, 50);
    sources.set_limit_bounds("ukri.search", 10, 100);

    sources.on_ok(
        "nih_reporter.search",
        vec![
            json!({"id": "R01-1", "title": "NIH Grant", "funder": "NIH", "source": "nih_reporter"}),
        ],
    );
    sources.on_ok(
        "nsf_awards.search",
        vec![
            json!({"id": "2621509", "title": "NSF Grant", "funder": "NSF", "source": "nsf_awards"}),
        ],
    );
    sources.on_ok(
        "cordis.search",
        vec![
            json!({"id": "101201649", "title": "CORDIS Grant", "funder": "EC", "source": "cordis"}),
        ],
    );
    sources.on_ok(
        "ukri.search",
        vec![json!({"id": "EP/X01", "title": "UKRI Grant", "funder": "EPSRC", "source": "ukri"})],
    );

    let sg = make_scholar_graph(
        net,
        Arc::new(MemoryStore::new()),
        sources.clone(),
        test_config(tmp.path(), false),
    );

    // I3: `limit: 30` MUST clamp `nsf_awards` to 25 and return results from all 4 funders.
    let val = sg
        .entity(
            EntityKind::Grants,
            "crispr",
            json!({"limit": 30}),
            &CallCx::default(),
        )
        .await
        .unwrap();
    let grants: Vec<GrantRecord> = serde_json::from_value(val).unwrap();
    assert_eq!(grants.len(), 4);
    assert!(
        grants.iter().any(|g| g.source == "nsf_awards"),
        "NSF must not be dropped when limit > 25: {grants:?}"
    );
    let nsf_call = sources
        .calls()
        .into_iter()
        .find(|(op, _)| op == "nsf_awards.search")
        .expect("nsf_awards called");
    assert_eq!(nsf_call.1["limit"], 25);

    // I4: Duplicate grant sources are deduplicated preserving order.
    let calls_before = sources.calls().len();
    let val_dup = sg
        .entity(
            EntityKind::Grants,
            "crispr",
            json!({"sources": ["nih_reporter", "NIH_REPORTER", "cordis", "nih_reporter"], "limit": 5}),
            &CallCx::default(),
        )
        .await
        .unwrap();
    let grants_dup: Vec<GrantRecord> = serde_json::from_value(val_dup).unwrap();
    assert_eq!(grants_dup.len(), 2);
    assert_eq!(
        sources.calls().len() - calls_before,
        2,
        "duplicate sources must be deduplicated before calling SourcesApi"
    );

    // I4: Non-grant / unknown sources MUST be rejected with InvalidInput without calling them.
    let calls_before_bad = sources.calls().len();
    let err_arxiv = sg
        .entity(
            EntityKind::Grants,
            "crispr",
            json!({"sources": ["arxiv", "crossref"]}),
            &CallCx::default(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err_arxiv, FsmError::InvalidInput(_)),
        "expected InvalidInput for non-grant sources, got {err_arxiv:?}"
    );
    assert_eq!(
        sources.calls().len(),
        calls_before_bad,
        "invalid sources must not dispatch any SourcesApi::call"
    );
}

#[tokio::test]
async fn test_cite_csl_from_doi_cn_strips_crossref_bloat_fields() {
    // M8: `CiteStyle::Csl` from DOI Content Negotiation must sanitize away bloated Crossref
    // arrays (`reference`, `link`, `license`, `score`) while keeping standard CSL fields.
    let tmp = tempfile::tempdir().unwrap();
    let bloated_csl = json!({
        "DOI": "10.1038/nature12373",
        "type": "article-journal",
        "title": "Nanometre-scale thermometry in a living cell",
        "container-title": "Nature",
        "volume": "500",
        "issue": "7460",
        "page": "54-58",
        "ISSN": ["0028-0836", "1476-4687"],
        "score": 1.0,
        "link": [{"URL": "https://www.nature.com/articles/nature12373.pdf"}],
        "license": [{"URL": "https://example.org/license"}],
        "reference": (0..50).map(|i| json!({"key": format!("ref{i}"), "DOI": format!("10.1000/r{i}")})).collect::<Vec<_>>(),
        "author": [{"given": "G.", "family": "Kucsko"}],
        "issued": {"date-parts": [[2013, 7, 31]]}
    });
    let net = Arc::new(FnNet::new(move |req, _cx| {
        let body = bloated_csl.clone();
        async move {
            let url = fsm_core::fixture::request_url(&req).unwrap().to_string();
            if url.contains("doi.org/10.1038/nature12373") {
                return Ok(text_ok(
                    &url,
                    "application/vnd.citationstyles.csl+json",
                    &serde_json::to_string(&body).unwrap(),
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
    let csl_str = sg
        .cite(
            CiteInput::Id("10.1038/nature12373".into()),
            CiteStyle::Csl,
            &CallCx::default(),
        )
        .await
        .unwrap();
    let parsed: Value = serde_json::from_str(&csl_str).unwrap();
    assert_eq!(parsed["DOI"], "10.1038/nature12373");
    assert_eq!(parsed["volume"], "500");
    assert_eq!(parsed["ISSN"], "0028-0836");
    assert!(
        parsed.get("reference").is_none(),
        "reference[] must be stripped"
    );
    assert!(parsed.get("link").is_none(), "link[] must be stripped");
    assert!(
        parsed.get("license").is_none(),
        "license[] must be stripped"
    );
    assert!(parsed.get("score").is_none(), "score must be stripped");
}

#[tokio::test]
async fn test_cite_routes_s2_id_resolution_through_scholar_core_work() {
    // M10: A Semantic Scholar 40-hex paper ID (which `ScholarCore::work` classifies and
    // resolves) works seamlessly in `cite()` and caches into `scholar_works`.
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::new());
    let s2_hex = "204e3073870fae3d05bcbc2f6a8e263d9b72e776";
    let net = Arc::new(FnNet::new(move |req, _cx| {
        async move {
            let url = fsm_core::fixture::request_url(&req).unwrap().to_string();
            if url.contains(
                "api.semanticscholar.org/graph/v1/paper/204e3073870fae3d05bcbc2f6a8e263d9b72e776",
            ) {
                return Ok(common::json_ok(
                    &url,
                    &json!({
                        "paperId": "204e3073870fae3d05bcbc2f6a8e263d9b72e776",
                        "title": "Attention Is All You Need",
                        "year": 2017,
                        "venue": "NeurIPS",
                        "authors": [{"name": "Ashish Vaswani"}],
                        "externalIds": {"ArXiv": "1706.03762"}
                    }),
                ));
            }
            Ok(status_resp(&url, 404, "not found"))
        }
        .boxed()
    }));

    let sg = make_scholar_graph(
        net,
        store.clone(),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let bib = sg
        .cite(
            CiteInput::Id(s2_hex.into()),
            CiteStyle::Bibtex,
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(bib.contains("Attention Is All You Need"), "{bib}");
    assert!(bib.contains("Vaswani"), "{bib}");
    assert!(bib.contains("2017"), "{bib}");
}

// ---------------------------------------------------------------------------
// Review round 2 regression tests (B2, B3, I3, I4, M1)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_entity_venue_icml_real_icore_ranks_mapped_output_without_acronym() {
    // B2 (Missing test #2): Exact `[op.rank.map]` records emitted by promoted
    // `crates/manifests/sources/w5b/icore_ranks.toml` for `fixtures/w5b/icore_ranks/rank/default.json`
    // (no `"acronym"` key!). Must pick ICML `A*`, NOT the last tie `"National: China"`.
    let tmp = tempfile::tempdir().unwrap();
    let net = Arc::new(FnNet::new(|req, _cx| {
        async move { panic!("unexpected NetClient call: {}", req.url) }.boxed()
    }));
    let real_icore_items = vec![
        json!({
            "name": "International Conference on Machine Learning",
            "issn": [],
            "publisher": null,
            "icoreRank": "A*",
            "sources": ["icore_ranks"]
        }),
        json!({
            "name": "International Conference on Machine Learning and Applications",
            "issn": [],
            "publisher": null,
            "icoreRank": "C",
            "sources": ["icore_ranks"]
        }),
        json!({
            "name": "International Conference on Machine Learning and Cybernetics",
            "issn": [],
            "publisher": null,
            "icoreRank": "National: China",
            "sources": ["icore_ranks"]
        }),
    ];

    // Case 1: With DBLP + real mapped ICORE rows (no acronym field).
    let sources1 = Arc::new(MockSources::new());
    sources1.on_ok(
        "dblp.venue",
        vec![json!({
            "name": "International Conference on Machine Learning",
            "issn": ["2640-3498"],
            "worksCount": 14820,
            "sources": ["dblp"]
        })],
    );
    sources1.on_ok("icore_ranks.rank", real_icore_items.clone());
    sources1.on_ok("doaj.journals", vec![]);
    let sg1 = make_scholar_graph(
        net.clone(),
        Arc::new(MemoryStore::new()),
        sources1,
        test_config(tmp.path(), false),
    );
    let icml1: VenueProfile = serde_json::from_value(
        sg1.entity(EntityKind::Venue, "ICML", json!({}), &CallCx::default())
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(icml1.name, "International Conference on Machine Learning");
    assert_eq!(icml1.icore_rank.as_deref(), Some("A*"));

    // Case 2: Without DBLP (DBLP fails) — initials match must still pick ICML A*.
    let sources2 = Arc::new(MockSources::new());
    sources2.on_err("dblp.venue", "dblp timeout");
    sources2.on_ok("icore_ranks.rank", real_icore_items);
    sources2.on_ok("doaj.journals", vec![]);
    let sg2 = make_scholar_graph(
        net.clone(),
        Arc::new(MemoryStore::new()),
        sources2,
        test_config(tmp.path(), false),
    );
    let icml2: VenueProfile = serde_json::from_value(
        sg2.entity(EntityKind::Venue, "ICML", json!({}), &CallCx::default())
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(icml2.name, "International Conference on Machine Learning");
    assert_eq!(icml2.icore_rank.as_deref(), Some("A*"));

    // Case 3: Unrelated ICORE row must NOT attach a bogus rank to a DBLP venue.
    let sources3 = Arc::new(MockSources::new());
    sources3.on_ok(
        "dblp.venue",
        vec![json!({
            "name": "Journal of Machine Learning Research",
            "sources": ["dblp"]
        })],
    );
    sources3.on_ok(
        "icore_ranks.rank",
        vec![json!({
            "name": "International Workshop on Unrelated Systems",
            "icoreRank": "C",
            "sources": ["icore_ranks"]
        })],
    );
    sources3.on_ok("doaj.journals", vec![]);
    let sg3 = make_scholar_graph(
        net,
        Arc::new(MemoryStore::new()),
        sources3,
        test_config(tmp.path(), false),
    );
    let jmlr: VenueProfile = serde_json::from_value(
        sg3.entity(
            EntityKind::Venue,
            "Journal of Machine Learning Research",
            json!({}),
            &CallCx::default(),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(jmlr.name, "Journal of Machine Learning Research");
    assert_eq!(jmlr.icore_rank, None);
}

#[tokio::test]
async fn test_cite_doi_cn_normalizes_crossref_journal_and_proceedings_types() {
    // B3 (Missing test #3): Live Crossref DOI Content Negotiation emits `"type": "journal-article"`
    // and `"type": "proceedings-article"`, which hayagriva/citationberg ignore unless normalized
    // to CSL 1.0.2 `"article-journal"` and `"paper-conference"`.
    let tmp = tempfile::tempdir().unwrap();
    let nature_crossref = json!({
        "DOI": "10.1038/nature12373",
        "type": "journal-article",
        "title": "Nanometre-scale thermometry in a living cell",
        "container-title": "Nature",
        "volume": "500",
        "issue": "7460",
        "page": "54-58",
        "ISSN": ["0028-0836"],
        "author": [
            {"given": "G.", "family": "Kucsko"},
            {"given": "P. C.", "family": "Maurer"}
        ],
        "issued": {"date-parts": [[2013, 7, 31]]}
    });
    let sigir_crossref = json!({
        "DOI": "10.1145/1571941.1572114",
        "type": "proceedings-article",
        "title": "Reciprocal rank fusion outperforms condorcet and individual rank learning methods",
        "container-title": "Proceedings of the 32nd international ACM SIGIR conference on Research and development in information retrieval",
        "page": "758-759",
        "publisher": "ACM",
        "author": [
            {"given": "Gordon V.", "family": "Cormack"},
            {"given": "Charles L. A.", "family": "Clarke"},
            {"given": "Stefan", "family": "Buettcher"}
        ],
        "issued": {"date-parts": [[2009, 7, 19]]}
    });

    let net = Arc::new(FnNet::new(move |req, _cx| {
        let nature = nature_crossref.clone();
        let sigir = sigir_crossref.clone();
        async move {
            let url = fsm_core::fixture::request_url(&req).unwrap().to_string();
            if url.contains("doi.org/10.1038/nature12373") {
                return Ok(text_ok(
                    &url,
                    "application/vnd.citationstyles.csl+json",
                    &serde_json::to_string(&nature).unwrap(),
                ));
            }
            if url.contains("doi.org/10.1145/1571941.1572114") {
                return Ok(text_ok(
                    &url,
                    "application/vnd.citationstyles.csl+json",
                    &serde_json::to_string(&sigir).unwrap(),
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

    // Journal article (`journal-article` -> `article-journal`)
    let nature_csl: Value = serde_json::from_str(
        &sg.cite(
            CiteInput::Id("10.1038/nature12373".into()),
            CiteStyle::Csl,
            &CallCx::default(),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(nature_csl["type"], "article-journal");

    let nature_vanc = sg
        .cite(
            CiteInput::Id("10.1038/nature12373".into()),
            CiteStyle::Vancouver,
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(
        nature_vanc.contains("Nature"),
        "Vancouver must include journal container-title when type is normalized: {nature_vanc}"
    );
    let nature_ieee = sg
        .cite(
            CiteInput::Id("10.1038/nature12373".into()),
            CiteStyle::Ieee,
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(
        nature_ieee.contains("Nature") && nature_ieee.contains("vol."),
        "IEEE must format journal container-title + vol.: {nature_ieee}"
    );
    for style in [CiteStyle::Apa, CiteStyle::Chicago] {
        let s = sg
            .cite(
                CiteInput::Id("10.1038/nature12373".into()),
                style,
                &CallCx::default(),
            )
            .await
            .unwrap();
        assert!(
            s.contains("Nature") && s.contains("500") && !s.contains('\x1b'),
            "{style:?}: {s}"
        );
    }

    // Proceedings article (`proceedings-article` -> `paper-conference`)
    let sigir_csl: Value = serde_json::from_str(
        &sg.cite(
            CiteInput::Id("10.1145/1571941.1572114".into()),
            CiteStyle::Csl,
            &CallCx::default(),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(sigir_csl["type"], "paper-conference");

    let sigir_ieee = sg
        .cite(
            CiteInput::Id("10.1145/1571941.1572114".into()),
            CiteStyle::Ieee,
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(
        sigir_ieee.to_ascii_lowercase().contains("in ") && sigir_ieee.contains("SIGIR"),
        "IEEE must format proceedings with 'in <Proceedings>': {sigir_ieee}"
    );
    let sigir_vanc = sg
        .cite(
            CiteInput::Id("10.1145/1571941.1572114".into()),
            CiteStyle::Vancouver,
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(
        sigir_vanc.contains("SIGIR"),
        "Vancouver must include proceedings title: {sigir_vanc}"
    );
}

#[tokio::test]
async fn test_cite_propagates_upstream_outage_and_does_not_search_openalex_for_s2_id() {
    // I3 / R2 (Missing test #6b): Upstream HTTP 503 on typed IDs must return `Upstream`,
    // NOT `InvalidInput`, and S2 40-hex IDs must never trigger OpenAlex `?search=<hash>`.
    let tmp = tempfile::tempdir().unwrap();
    let net = Arc::new(FnNet::new(|req, _cx| {
        async move { Ok(status_resp(&req.url, 503, "service unavailable")) }.boxed()
    }));
    let sg = make_scholar_graph(
        net.clone(),
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );

    let err_oa = sg
        .cite(
            CiteInput::Id("W2626778328".into()),
            CiteStyle::Apa,
            &CallCx::default(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err_oa, FsmError::Upstream { .. }),
        "expected Upstream error on OpenAlex 503, got {err_oa:?}"
    );

    net.clear();
    let s2_hex = "204e3073870fae3d05bcbc2f6a8e263d9b72e776";
    let err_s2 = sg
        .cite(
            CiteInput::Id(s2_hex.into()),
            CiteStyle::Apa,
            &CallCx::default(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err_s2, FsmError::Upstream { .. }),
        "expected Upstream error on S2 503, got {err_s2:?}"
    );
    assert!(
        !net.urls()
            .iter()
            .any(|u| u.contains("api.openalex.org") && u.contains(s2_hex)),
        "S2 40-hex ID must never fire a bogus OpenAlex title search: {:?}",
        net.urls()
    );
}

#[tokio::test]
async fn test_entity_venue_issn_preserves_doaj_name_and_errors_on_total_failure() {
    // I4 / R2 (Missing test #6c):
    // 1. ISSN lookup when `issn_portal` is empty/fails preserves DOAJ's `"PLoS ONE"` name
    //    and paired ISSNs (and does not call `dblp.venue` with bare ISSN).
    // 2. ISSN lookup returns `Err` when all called sources fail.
    // 3. Non-ISSN lookup succeeds when only `doaj.journals` succeeds (DBLP + ICORE fail).
    let tmp = tempfile::tempdir().unwrap();
    let net = Arc::new(FnNet::new(|req, _cx| {
        async move { panic!("unexpected NetClient call: {}", req.url) }.boxed()
    }));

    // Case 1: ISSN lookup with failing issn_portal + succeeding DOAJ.
    let sources1 = Arc::new(MockSources::new());
    sources1.on_err("issn_portal.lookup", "issn portal 503");
    sources1.on_ok(
        "doaj.journals",
        vec![json!({
            "name": "PLoS ONE",
            "issn": ["1932-6203", "1932-620X"],
            "publisher": "Public Library of Science (PLoS)",
            "doajVetted": true,
            "sources": ["doaj"]
        })],
    );
    let sg1 = make_scholar_graph(
        net.clone(),
        Arc::new(MemoryStore::new()),
        sources1.clone(),
        test_config(tmp.path(), false),
    );
    let plos: VenueProfile = serde_json::from_value(
        sg1.entity(
            EntityKind::Venue,
            "1932-6203",
            json!({}),
            &CallCx::default(),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(plos.name, "PLoS ONE");
    assert_eq!(plos.doaj_vetted, Some(true));
    assert_eq!(
        plos.publisher.as_deref(),
        Some("Public Library of Science (PLoS)")
    );
    assert!(plos.issn.contains(&"1932-6203".to_owned()));
    assert!(plos.issn.contains(&"1932-620X".to_owned()));
    assert!(
        !sources1.calls().iter().any(|(op, _)| op == "dblp.venue"),
        "ISSN lookup must not call dblp.venue"
    );

    // Case 2: Total ISSN failure returns Err (R2).
    let sources2 = Arc::new(MockSources::new());
    sources2.on_err("issn_portal.lookup", "503");
    sources2.on_err("doaj.journals", "503");
    let sg2 = make_scholar_graph(
        net.clone(),
        Arc::new(MemoryStore::new()),
        sources2,
        test_config(tmp.path(), false),
    );
    let err = sg2
        .entity(
            EntityKind::Venue,
            "1932-6203",
            json!({}),
            &CallCx::default(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, FsmError::Upstream { .. }),
        "total ISSN failure must return Upstream error, got {err:?}"
    );

    // Case 3: Non-ISSN lookup succeeds when DBLP + ICORE fail but DOAJ succeeds.
    let sources3 = Arc::new(MockSources::new());
    sources3.on_err("dblp.venue", "503");
    sources3.on_err("icore_ranks.rank", "503");
    sources3.on_ok(
        "doaj.journals",
        vec![json!({
            "name": "PLoS ONE",
            "issn": ["1932-6203"],
            "publisher": "Public Library of Science (PLoS)",
            "doajVetted": true,
            "sources": ["doaj"]
        })],
    );
    let sg3 = make_scholar_graph(
        net,
        Arc::new(MemoryStore::new()),
        sources3,
        test_config(tmp.path(), false),
    );
    let plos_by_name: VenueProfile = serde_json::from_value(
        sg3.entity(EntityKind::Venue, "PLoS ONE", json!({}), &CallCx::default())
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(plos_by_name.name, "PLoS ONE");
    assert_eq!(plos_by_name.doaj_vetted, Some(true));
}

#[tokio::test]
async fn test_cite_offline_csl_preserves_nobiliary_particle_and_suffix() {
    // M1 (Missing test #7a): Offline `CiteStyle::Csl` must preserve `non-dropping-particle`
    // (`"van"`) and `suffix` (`"Jr."`) from `parse_person_name`.
    let tmp = tempfile::tempdir().unwrap();
    let net = Arc::new(FnNet::new(|req, _cx| {
        async move { panic!("offline cite must not hit net: {}", req.url) }.boxed()
    }));
    let sg = make_scholar_graph(
        net,
        Arc::new(MemoryStore::new()),
        Arc::new(MockSources::new()),
        test_config(tmp.path(), false),
    );
    let mut w = ScholarlyWork::new("Symphony No. 9", "https://example.org/sym9");
    w.authors = vec![
        AuthorRef::new("Ludwig van Beethoven"),
        AuthorRef::new("Smith, Jr., John"),
    ];
    w.year = Some(1824);
    let csl_str = sg
        .cite(
            CiteInput::Work(Box::new(w)),
            CiteStyle::Csl,
            &CallCx::default(),
        )
        .await
        .unwrap();
    let csl: Value = serde_json::from_str(&csl_str).unwrap();
    assert_eq!(csl["author"][0]["family"], "Beethoven");
    assert_eq!(csl["author"][0]["given"], "Ludwig");
    assert_eq!(csl["author"][0]["non-dropping-particle"], "van");
    assert_eq!(csl["author"][1]["family"], "Smith");
    assert_eq!(csl["author"][1]["given"], "John");
    assert_eq!(csl["author"][1]["suffix"], "Jr.");
}
