//! Snapshot tests (`insta`) for all 11 [`fsm_core::RenderKind`] variants plus
//! `render_cache_hits` and truncated `render_json`, built directly from real recorded
//! fixtures in the repository (`crates/manifests/fixtures/**`, `crates/scholar/tests/fixtures/**`,
//! `crates/scholar-fulltext/tests/fixtures/**`, `crates/extract/tests/fixtures/**`).

use fsm_core::fixture::Fixture;
use fsm_core::{
    AuthorRef, CacheHit, CatalogHit, CitationGraph, DocSlice, FetchMethod, FulltextResult,
    FulltextSection, GraphNotice, Hit, HitKind, InlineImageMeta, OaLocation, PageResult,
    RenderKind, ResearchBrief, ScholarlyWork, SearchMeta, SearchResponse, WorkResult,
};
use fsm_surface::{HOST_API, RenderOpts, host_fn, render, render_cache_hits, render_json};
use serde_json::{Value, json};
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates dir")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn load_fixture_body_json(rel: &str, exchange_idx: usize) -> Value {
    let path = repo_root().join(rel);
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read fixture {}: {e}", path.display()));
    let fx: Fixture = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("invalid fixture {}: {e}", path.display()));
    let body = fx.exchanges[exchange_idx]
        .response
        .body_text
        .as_deref()
        .expect("fixture body_text");
    serde_json::from_str(body).expect("fixture JSON body")
}

fn load_golden_pages() -> Vec<Value> {
    let path = repo_root().join("crates/extract/tests/fixtures/corpus/golden.json");
    let raw = std::fs::read_to_string(&path).expect("read golden.json");
    serde_json::from_str(&raw).expect("parse golden.json")
}

fn build_search_response_from_mwmbl_and_clinicaltrials() -> SearchResponse {
    let mwmbl = load_fixture_body_json(
        "crates/manifests/fixtures/seed/mwmbl/search/default.json",
        0,
    );
    let arr = mwmbl.as_array().expect("mwmbl array");
    let mut hits = Vec::new();
    for item in arr.iter().take(2) {
        let title: String = item["title"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|p| p["value"].as_str())
            .collect();
        let snippet: String = item["extract"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|p| p["value"].as_str())
            .collect();
        let mut h = Hit::new(title, item["url"].as_str().unwrap());
        h.snippet = snippet;
        h.source = "mwmbl".into();
        h.sources = vec!["mwmbl".into()];
        h.date_source = Some("none".into());
        h.score = 0.032;
        hits.push(h);
    }

    let ct = load_fixture_body_json(
        "crates/manifests/fixtures/w5b/clinicaltrials/search/default.json",
        0,
    );
    let study = &ct["studies"][0]["protocolSection"];
    let nct = study["identificationModule"]["nctId"].as_str().unwrap();
    let mut ct_hit = Hit::new(
        study["identificationModule"]["briefTitle"]
            .as_str()
            .unwrap(),
        format!("https://clinicaltrials.gov/study/{nct}"),
    );
    ct_hit.snippet = study["descriptionModule"]["briefSummary"]
        .as_str()
        .unwrap()
        .to_owned();
    ct_hit.date = Some(
        study["statusModule"]["lastUpdatePostDateStruct"]["date"]
            .as_str()
            .unwrap()
            .to_owned(),
    );
    ct_hit.date_source = Some("structured".into());
    ct_hit.source = "clinicaltrials".into();
    ct_hit.sources = vec!["clinicaltrials".into()];
    ct_hit.kind = HitKind::Record;
    ct_hit.score = 0.081;
    hits.push(ct_hit);

    let mut sr = SearchResponse::new("python crispr", "web");
    sr.results = hits;
    sr.lead = Some("Python is a high-level, general-purpose programming language.".into());
    sr.engines = vec!["mwmbl".into(), "clinicaltrials".into(), "bing".into()];
    sr.cached = true;
    sr.cache_age_s = Some(300);
    sr.elapsed_ms = 1240;
    sr.notes = vec!["bing timed out after 5.0s; served via mwmbl + clinicaltrials".into()];
    sr
}

fn build_works_from_europepmc_and_retracted_lancet() -> Vec<ScholarlyWork> {
    let epmc = load_fixture_body_json(
        "crates/manifests/fixtures/seed/europepmc/search/default.json",
        0,
    );
    let mut works = Vec::new();
    for r in epmc["resultList"]["result"]
        .as_array()
        .unwrap()
        .iter()
        .take(2)
    {
        let doi = r["doi"].as_str().unwrap();
        let mut w = ScholarlyWork::new(
            r["title"].as_str().unwrap(),
            format!("https://doi.org/{doi}"),
        );
        w.authors = r["authorList"]["author"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|a| a["fullName"].as_str())
            .map(AuthorRef::new)
            .collect();
        w.year = r["pubYear"].as_str().and_then(|s| s.parse().ok());
        w.venue = r["journalInfo"]["journal"]["title"]
            .as_str()
            .map(str::to_owned);
        w.work_type = "article".into();
        w.ids.doi = Some(doi.to_owned());
        w.ids.pmid = r["pmid"].as_str().map(str::to_owned);
        w.abstract_text = r["abstractText"].as_str().map(str::to_owned);
        w.cited_by = r["citedByCount"].as_u64();
        w.cited_by_source = Some("europepmc".into());
        w.sources = vec!["europepmc".into()];
        works.push(w);
    }

    // Add the real retracted Lancet paper from crates/scholar/tests/fixtures/doi_retracted_lancet/
    let oa = load_fixture_body_json(
        "crates/scholar/tests/fixtures/doi_retracted_lancet/openalex_work.json",
        0,
    );
    let mut ret = ScholarlyWork::new(
        oa["display_name"].as_str().unwrap(),
        oa["doi"].as_str().unwrap(),
    );
    ret.authors = oa["authorships"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["author"]["display_name"].as_str())
        .map(AuthorRef::new)
        .collect();
    ret.year = oa["publication_year"].as_u64().map(|y| y as u16);
    ret.venue = oa["primary_location"]["source"]["display_name"]
        .as_str()
        .map(str::to_owned);
    ret.work_type = "article".into();
    ret.ids.doi = Some("10.1016/s0140-6736(20)31180-6".into());
    ret.ids.openalex = Some("W3027680906".into());
    ret.cited_by = oa["cited_by_count"].as_u64();
    ret.cited_by_source = Some("openalex".into());
    let mut oa_loc = OaLocation::new(oa["open_access"]["oa_status"].as_str().unwrap());
    oa_loc.pdf_url = oa["open_access"]["oa_url"].as_str().map(str::to_owned);
    ret.oa = Some(oa_loc);
    ret.retracted = oa["is_retracted"].as_bool().unwrap();
    ret.retraction_reason =
        Some("Publisher retraction (2020-06-05): unverified primary data".into());
    ret.sources = vec!["crossref".into(), "openalex".into()];
    works.push(ret);
    works
}

fn page_from_golden(entry: &Value) -> PageResult {
    let mut p = PageResult::new(
        entry["url"].as_str().unwrap(),
        entry["py_title"].as_str().unwrap(),
        entry["py_markdown"].as_str().unwrap(),
        FetchMethod::Http,
    );
    p.author = entry["py_author"].as_str().map(str::to_owned);
    p.published_date = entry["py_date"].as_str().map(str::to_owned);
    p.sitename = entry["py_sitename"].as_str().map(str::to_owned);
    p.retrieved_at = "2026-11-25T14:00:00Z".into();
    p
}

#[test]
fn snapshot_search_response() {
    let sr = build_search_response_from_mwmbl_and_clinicaltrials();
    let opts = RenderOpts::default().with_now("2026-11-25T14:15:00Z");
    let out = render(
        RenderKind::SearchResponse,
        &serde_json::to_value(&sr).unwrap(),
        &opts,
    );
    insta::assert_snapshot!("search_response", out);
}

#[test]
fn snapshot_page_result() {
    let golden = load_golden_pages();
    let mut page = page_from_golden(&golden[0]);
    page.cache_age_s = Some(7200);
    let out = render(
        RenderKind::PageResult,
        &serde_json::to_value(&page).unwrap(),
        &RenderOpts::default().with_max_tokens(350),
    );
    insta::assert_snapshot!("page_result", out);
}

#[test]
fn snapshot_page_list() {
    let golden = load_golden_pages();
    let pages = vec![page_from_golden(&golden[0]), page_from_golden(&golden[1])];
    let out = render(
        RenderKind::PageList,
        &serde_json::to_value(&pages).unwrap(),
        &RenderOpts::default().with_max_tokens(450),
    );
    insta::assert_snapshot!("page_list", out);
}

#[test]
fn snapshot_doc_slice() {
    let ct = load_fixture_body_json(
        "crates/manifests/fixtures/w5b/clinicaltrials/search/default.json",
        0,
    );
    let protocol = &ct["studies"][2]["protocolSection"];
    let text = protocol["descriptionModule"]["detailedDescription"]
        .as_str()
        .unwrap();
    let mut slice = DocSlice::new(
        "https://clinicaltrials.gov/study/NCT07340853.pdf",
        "pdf",
        &text[..500],
    );
    slice.title = Some(
        protocol["identificationModule"]["briefTitle"]
            .as_str()
            .unwrap()
            .to_owned(),
    );
    slice.pages = Some(14);
    slice.start = 0;
    slice.total_chars = text.chars().count();
    slice.truncated = true;
    let out = render(
        RenderKind::DocSlice,
        &serde_json::to_value(&slice).unwrap(),
        &RenderOpts::default(),
    );
    insta::assert_snapshot!("doc_slice", out);
}

#[test]
fn snapshot_fulltext_result() {
    let bioc = load_fixture_body_json(
        "crates/scholar-fulltext/tests/fixtures/step1_3_pmc_bioc/default.json",
        2,
    );
    let doc = &bioc[0]["documents"][0];
    let passages = doc["passages"].as_array().unwrap();
    let title = passages[0]["text"].as_str().unwrap();
    let abs = passages[1]["text"].as_str().unwrap();
    let intro = passages[5]["text"].as_str().unwrap();

    let mut ft = FulltextResult::new("10.1371/journal.ppat.1002485", true);
    ft.format = Some("bioc".into());
    ft.source = Some("pmc_bioc".into());
    ft.license = doc["infons"]["license"].as_str().map(str::to_owned);
    ft.url = Some("https://pmc.ncbi.nlm.nih.gov/articles/PMC3257301/".into());
    ft.sections = Some(vec![
        FulltextSection::new("Title", title),
        FulltextSection::new("Abstract", abs),
        FulltextSection::new("Introduction", intro),
    ]);

    let out = render(
        RenderKind::FulltextResult,
        &serde_json::to_value(&ft).unwrap(),
        &RenderOpts::default().with_max_tokens(400),
    );
    insta::assert_snapshot!("fulltext_result", out);
}

#[test]
fn snapshot_work_result() {
    let works = build_works_from_europepmc_and_retracted_lancet();
    let mut meta = SearchMeta::new("crispr");
    meta.subscope = Some("academic.biomed".into());
    meta.sources = vec!["europepmc".into(), "crossref".into(), "openalex".into()];
    meta.elapsed_ms = 1480;
    meta.notes = vec!["1 retracted work flagged and demoted".into()];
    let wr = WorkResult::new(works, meta);
    let out = render(
        RenderKind::WorkResult,
        &serde_json::to_value(&wr).unwrap(),
        &RenderOpts::default(),
    );
    insta::assert_snapshot!("work_result", out);
}

#[test]
fn snapshot_work_list() {
    // Single resolved paper from crates/scholar/tests/fixtures/doi_rrf/crossref_work.json
    let cr = load_fixture_body_json(
        "crates/scholar/tests/fixtures/doi_rrf/crossref_work.json",
        0,
    );
    let m = &cr["message"];
    let mut w = ScholarlyWork::new(m["title"][0].as_str().unwrap(), m["URL"].as_str().unwrap());
    w.authors = m["author"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| {
            AuthorRef::new(format!(
                "{} {}",
                a["given"].as_str().unwrap_or(""),
                a["family"].as_str().unwrap_or("")
            ))
        })
        .collect();
    w.year = Some(2009);
    w.venue = m["container-title"][0].as_str().map(str::to_owned);
    w.work_type = m["type"].as_str().unwrap().to_owned();
    w.ids.doi = m["DOI"].as_str().map(str::to_owned);
    w.cited_by = m["is-referenced-by-count"].as_u64();
    w.cited_by_source = Some("crossref".into());
    w.sources = vec!["crossref".into()];

    let out = render(
        RenderKind::WorkList,
        &serde_json::to_value(vec![w]).unwrap(),
        &RenderOpts::default(),
    );
    insta::assert_snapshot!("work_list", out);
}

#[test]
fn snapshot_research_brief() {
    let golden = load_golden_pages();
    let p1 = page_from_golden(&golden[0]);
    let p2 = page_from_golden(&golden[1]);

    let mut sr = SearchResponse::new("xz utils backdoor", "security");
    sr.engines = vec!["duckduckgo".into(), "nvd".into()];
    let mut h1 = Hit::new(&p1.title, &p1.url);
    h1.snippet = "Investigation into a backdoor shipped with xz 5.6.0 and 5.6.1.".into();
    h1.date = p1.published_date.clone();
    h1.date_source = Some("structured".into());
    h1.kind = HitKind::News;
    let mut h2 = Hit::new(&p2.title, &p2.url);
    h2.snippet = "Malicious backdoor targeting OpenSSH sshd via xz tarballs.".into();
    h2.date = p2.published_date.clone();
    h2.date_source = Some("structured".into());
    h2.kind = HitKind::News;
    sr.results = vec![h1, h2];

    let mut brief = ResearchBrief::new("xz utils backdoor", sr);
    brief.pages = vec![p1, p2];
    brief.tokens_estimated = 950;
    brief.date_note = Some("All sources published 2024-03-29 (spread 0 days).".into());

    let out = render(
        RenderKind::ResearchBrief,
        &serde_json::to_value(&brief).unwrap(),
        &RenderOpts::default().with_max_tokens(500),
    );
    insta::assert_snapshot!("research_brief", out);
}

#[test]
fn snapshot_citation_graph() {
    let cr = load_fixture_body_json(
        "crates/scholar/tests/fixtures/doi_retracted_lancet/crossref_work.json",
        0,
    );
    let m = &cr["message"];
    let works = build_works_from_europepmc_and_retracted_lancet();
    let retracted_paper = works[2].clone();

    let mut cg = CitationGraph::new("10.1016/s0140-6736(20)31180-6", "doi", retracted_paper);
    for upd in m["updated-by"].as_array().unwrap().iter().take(2) {
        let mut n = GraphNotice::new(upd["label"].as_str().unwrap());
        let dp = upd["updated"]["date-parts"][0].as_array().unwrap();
        n.date = Some(format!(
            "{:04}-{:02}-{:02}",
            dp[0].as_u64().unwrap(),
            dp[1].as_u64().unwrap(),
            dp[2].as_u64().unwrap()
        ));
        n.doi = upd["DOI"].as_str().map(str::to_owned);
        cg.notices.push(n);
    }
    for r in m["reference"].as_array().unwrap().iter().take(2) {
        let doi = r["DOI"].as_str().unwrap_or("");
        let mut rw = ScholarlyWork::new(
            r["article-title"].as_str().unwrap_or("(untitled)"),
            format!("https://doi.org/{doi}"),
        );
        if let Some(a) = r["author"].as_str() {
            rw.authors.push(AuthorRef::new(a));
        }
        rw.year = r["year"].as_str().and_then(|y| y.parse().ok());
        rw.venue = r["journal-title"].as_str().map(str::to_owned);
        rw.ids.doi = (!doi.is_empty()).then(|| doi.to_owned());
        cg.references.push(rw);
    }
    cg.notes = vec!["Retraction notices verified via Crossref updated-by & OpenAlex.".into()];

    let out = render(
        RenderKind::CitationGraph,
        &serde_json::to_value(&cg).unwrap(),
        &RenderOpts::default(),
    );
    insta::assert_snapshot!("citation_graph", out);
}

#[test]
fn snapshot_catalog_list() {
    let req_fn = host_fn("web.request").unwrap();
    let mut h1 = CatalogHit::new(
        "clinicaltrials",
        "source",
        "ClinicalTrials.gov v2 study search",
    );
    h1.signature = "source.call(\"clinicaltrials.search\", { query: string, limit?: int })".into();
    h1.example =
        "source.call(\"clinicaltrials.search\", {\"query\":\"crispr\",\"limit\":3})".into();
    h1.auth = "keyless".into();
    h1.tier = 1;
    h1.health = "ok".into();

    let mut h2 = CatalogHit::new(req_fn.name, "api", req_fn.doc);
    h2.signature = req_fn.sig.to_owned();
    h2.example = req_fn.example.to_owned();
    h2.auth = "keyless".into();
    h2.tier = 1;
    h2.health = "ok".into();

    let out = render(
        RenderKind::CatalogList,
        &serde_json::to_value(vec![h1, h2]).unwrap(),
        &RenderOpts::default().with_query("clinical trials api"),
    );
    insta::assert_snapshot!("catalog_list", out);
}

#[test]
fn snapshot_inline_image() {
    let mut img = InlineImageMeta::new(
        "https://upload.wikimedia.org/wikipedia/commons/example.png",
        "image/png",
        48_120,
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    );
    img.width = Some(1024);
    img.height = Some(768);
    let out = render(
        RenderKind::InlineImage,
        &serde_json::to_value(&img).unwrap(),
        &RenderOpts::default(),
    );
    insta::assert_snapshot!("inline_image", out);
}

#[test]
fn snapshot_cache_hits_and_truncated_json() {
    let mut c1 = CacheHit::new(
        "https://lwn.net/Articles/967180/",
        "A backdoor in xz",
        "…investigation into a [backdoor] that was shipped with versions 5.6.0 and 5.6.1…",
    );
    c1.sitename = Some("LWN.net".into());
    c1.date = Some("2024-03-29".into());
    let cache_md = render_cache_hits(&[c1], &RenderOpts::default().with_query("backdoor"));
    insta::assert_snapshot!("cache_hits", cache_md);

    let bad_fts = render_cache_hits(
        &[],
        &RenderOpts::default().with_query("\"unterminated phrase"),
    );
    insta::assert_snapshot!("cache_hits_invalid_fts", bad_fts);

    let big_json = json!({
        "source": "nvd",
        "hostApiCount": HOST_API.len(),
        "items": (1..=12).map(|i| json!({
            "id": format!("CVE-2026-{i:04}"),
            "description": format!("Vulnerability description #{i}: {}", "buffer bounds check missing. ".repeat(15))
        })).collect::<Vec<_>>()
    });
    let shrunk = render_json(&big_json, &RenderOpts::default().with_max_tokens(140));
    insta::assert_snapshot!("render_json_truncated", shrunk);
}
