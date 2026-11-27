use fsm_core::{FetchMethod, PageResult, RenderKind, ResearchBrief, SearchResponse};
use fsm_surface::{RenderOpts, render};

fn brief(cached: bool, age: Option<u64>) -> ResearchBrief {
    let mut search = SearchResponse::new("provenance", "web");
    search.cached = cached;
    search.cache_age_s = age;
    let mut brief = ResearchBrief::new("provenance", search);
    let mut page = PageResult::new(
        "https://example.org/source",
        "Source",
        "Evidence body",
        FetchMethod::default(),
    );
    page.retrieved_at = "2026-11-27T12:00:00Z".into();
    page.cache_age_s = Some(3600);
    brief.pages.push(page);
    brief
}

fn rendered(brief: &ResearchBrief) -> String {
    render(
        RenderKind::ResearchBrief,
        &serde_json::to_value(brief).unwrap(),
        &RenderOpts::default(),
    )
}

#[test]
fn research_preserves_independent_search_and_document_provenance() {
    let out = rendered(&brief(true, Some(7200)));
    assert!(out.contains("search cached 2 hours ago"));
    let documents = out.split_once("## Documents").unwrap().1;
    assert!(documents.contains("cached 1 hour ago · retrieved 2026-11-27T12:00:00Z"));
    assert!(!documents.contains("cached 2 hours ago"));
    assert!(documents.contains("Evidence body"));
}

#[test]
fn cached_search_without_age_is_still_identified() {
    let out = rendered(&brief(true, None));
    let metadata = out
        .lines()
        .find(|line| line.starts_with("_engines:"))
        .unwrap();
    assert!(metadata.contains("search cached"));
    assert!(!metadata.contains("search cached 1 hour"));
}

#[test]
fn fresh_search_does_not_inherit_page_cache_status() {
    let out = rendered(&brief(false, None));
    assert!(!out.contains("search cached"));
    assert!(out.contains("cached 1 hour ago · retrieved 2026-11-27T12:00:00Z"));
}

#[test]
fn fresh_page_does_not_inherit_search_cache_status() {
    let mut brief = brief(true, Some(7200));
    brief.pages[0].cache_age_s = None;
    let out = rendered(&brief);
    assert!(out.contains("search cached 2 hours ago"));
    let documents = out.split_once("## Documents").unwrap().1;
    assert!(documents.contains("retrieved 2026-11-27T12:00:00Z"));
    assert!(!documents.contains("cached"));
}

#[test]
fn zero_age_is_not_treated_as_missing() {
    let mut brief = brief(true, Some(0));
    brief.pages[0].cache_age_s = Some(0);
    let out = rendered(&brief);
    assert!(out.contains("search cached under a minute ago"));
    assert!(out.contains("cached under a minute ago · retrieved 2026-11-27T12:00:00Z"));
}

#[test]
fn page_age_without_timestamp_is_preserved() {
    let mut brief = brief(false, None);
    brief.pages[0].retrieved_at.clear();
    let out = rendered(&brief);
    let documents = out.split_once("## Documents").unwrap().1;
    assert!(documents.contains("cached 1 hour ago"));
    assert!(!documents.contains("retrieved"));
}

#[test]
fn absent_provenance_does_not_fabricate_a_retrieval_or_cache_claim() {
    let mut brief = brief(false, None);
    brief.pages[0].retrieved_at.clear();
    brief.pages[0].cache_age_s = None;
    let out = rendered(&brief);
    assert!(!out.contains("cached"));
    assert!(!out.contains("retrieved"));
    assert!(out.contains("Evidence body"));
}

#[test]
fn cached_search_provenance_survives_without_documents() {
    let mut brief = brief(true, Some(7200));
    brief.pages.clear();
    let out = rendered(&brief);
    assert!(out.contains("search cached 2 hours ago"));
    assert!(!out.contains("## Documents"));
}
