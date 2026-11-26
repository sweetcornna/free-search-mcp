//! Port of `tests/test_seznam.py`: embedded `renderer-state-data` JSON parsing,
//! HTML fragment stripping, ad/sitelink skipping, video/forum-only dates, and `build_url`.

use fsm_core::api::Freshness;
use fsm_core::config::Config;
use fsm_core::source::SrcCx;
use fsm_engines_serp::common::SerpQuery;
use fsm_engines_serp::engines::seznam::{build_url, parse};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn page_from_state(state: &Value) -> String {
    let blob = escape_attr(&serde_json::to_string(state).unwrap());
    format!(
        r#"<!DOCTYPE html><html lang="cs"><body><div class="Layout--left"></div><script id="renderer-state-data" data-state="{blob}"></script></body></html>"#
    )
}

fn sample_state() -> Value {
    json!({
        "odra": {"page": "search"},
        "data": {
            "query": "python asyncio taskgroup exception handling",
            "count": 10,
            "entities": [
                {
                    "id": {"service": "layout", "type": "desktop", "key": "layout-desktop"},
                    "subEntities": [
                        {
                            "id": {"service": "layout", "type": "legacy-left", "key": "legacy-left"},
                            "subEntities": [
                                {
                                    "id": {"service": "sklik", "type": "skliktop", "key": "1"},
                                    "data": {"cleanClickthruUrl": "https://www.epojisteni.cz/povinne-ruceni/"}
                                },
                                {
                                    "id": {"service": "ftxt-organic", "type": "result", "key": "4b1f"},
                                    "data": {
                                        "docId": "4b1f",
                                        "snippet": {
                                            "url": "https://www.geeksforgeeks.org/python/python-exception-handling/",
                                            "description": "<b class=sec>Exception</b> <b class=sec>Handling</b> allows a program to handle unexpected errors&hellip;",
                                            "title": "<b class=sec>Python</b> <b class=sec>Exception</b> <b class=sec>Handling</b> - GeeksforGeeks"
                                        },
                                        "attributes": {"publicationDate": 1780055857},
                                        "video": {"title": "Python Exception Handling", "duration": 1386}
                                    }
                                },
                                {
                                    "id": {"service": "ftxt-organic", "type": "result", "key": "3a5b"},
                                    "data": {
                                        "docId": "3a5b",
                                        "snippet": {
                                            "url": "https://realpython.com/python311-exception-groups/",
                                            "description": "<b class=sec>Python</b> 3.11 will be released in October 2022. Preview task and <b class=sec>exception</b> groups&hellip;",
                                            "title": "<b class=sec>Python</b> 3.11 Preview: Task and <b class=sec>Exception</b> Groups – Real Python"
                                        },
                                        "attributes": {"publicationDate": 1650463200},
                                        "relatedDocuments": [
                                            {"type": "documentSection", "url": "https://realpython.com/python311-exception-groups/#python-311-alpha", "title": "Alpha"}
                                        ]
                                    }
                                },
                                {
                                    "id": {"service": "related", "type": "relates"},
                                    "data": {"results": [{"text": "python 3"}]}
                                },
                                {
                                    "id": {"service": "ftxt-organic", "type": "result", "key": "ab12"},
                                    "data": {
                                        "docId": "ab12",
                                        "snippet": {
                                            "url": "https://github.com/python/cpython/issues/116048",
                                            "description": "Bug report Bug description: As I understand it, <b class=sec>asyncio</b>.<b class=sec>TaskGroup</b> should never silently lose a task.",
                                            "title": "<b class=sec>asyncio</b>.<b class=sec>TaskGroup</b> may silently discard request to run a task&hellip;"
                                        },
                                        "attributes": {"publicationDate": 1709078400},
                                        "forumPosting": {"name": "asyncio.TaskGroup"}
                                    }
                                },
                                {
                                    "id": {"service": "ftxt-organic", "type": "result", "key": "7e04"},
                                    "data": {
                                        "docId": "7e04",
                                        "snippet": {
                                            "url": "https://docs.python.org/3/library/asyncio-task.html",
                                            "description": "This section outlines high-level <b class=sec>asyncio</b> APIs to work with coroutines and Tasks.",
                                            "title": "Coroutines and tasks — <b class=sec>Python</b> 3.15.1 documentation"
                                        },
                                        "attributes": {"publicationDate": 1398384000}
                                    }
                                },
                                {
                                    "id": {"service": "ftxt-organic", "type": "result", "key": "dup"},
                                    "data": {
                                        "docId": "dup",
                                        "snippet": {
                                            "url": "https://realpython.com/python311-exception-groups/",
                                            "description": "duplicate",
                                            "title": "Python 3.11 Preview"
                                        },
                                        "attributes": {}
                                    }
                                },
                                {
                                    "id": {"service": "sklik", "type": "sklikbottom", "key": "2"},
                                    "data": {"cleanClickthruUrl": "https://www.klik.cz/povinne-ruceni/"}
                                }
                            ]
                        }
                    ]
                }
            ]
        }
    })
}

#[test]
fn test_parse_yields_one_result_per_organic_block() {
    let out = parse(&page_from_state(&sample_state()));
    assert_eq!(
        out.iter().map(|r| r.url.as_str()).collect::<Vec<_>>(),
        vec![
            "https://www.geeksforgeeks.org/python/python-exception-handling/",
            "https://realpython.com/python311-exception-groups/",
            "https://github.com/python/cpython/issues/116048",
            "https://docs.python.org/3/library/asyncio-task.html",
        ]
    );
    assert!(out.iter().all(|r| r.source == "seznam"));
}

#[test]
fn test_parse_strips_markup_without_splitting_dotted_names() {
    let by_url: HashMap<String, _> = parse(&page_from_state(&sample_state()))
        .into_iter()
        .map(|r| (r.url.clone(), r))
        .collect();
    let gh = &by_url["https://github.com/python/cpython/issues/116048"];
    assert_eq!(
        gh.title,
        "asyncio.TaskGroup may silently discard request to run a task…"
    );
    assert!(
        gh.snippet
            .starts_with("Bug report Bug description: As I understand it, asyncio.TaskGroup")
    );
    let rp = &by_url["https://realpython.com/python311-exception-groups/"];
    assert_eq!(
        rp.title,
        "Python 3.11 Preview: Task and Exception Groups – Real Python"
    );
    assert!(!rp.snippet.contains("<b") && !rp.snippet.contains("&hellip;"));
}

#[test]
fn test_sitelinks_ads_and_widgets_are_not_results() {
    let urls: Vec<String> = parse(&page_from_state(&sample_state()))
        .into_iter()
        .map(|r| r.url)
        .collect();
    assert!(!urls.iter().any(|u| u.contains('#')));
    assert!(!urls.contains(&"https://www.epojisteni.cz/povinne-ruceni/".to_owned()));
    assert!(!urls.contains(&"https://www.klik.cz/povinne-ruceni/".to_owned()));
    assert!(!urls.iter().any(|u| u.contains("seznam.cz")));
}

#[test]
fn test_date_only_where_seznam_prints_one() {
    let by_url: HashMap<String, _> = parse(&page_from_state(&sample_state()))
        .into_iter()
        .map(|r| (r.url.clone(), r))
        .collect();
    assert_eq!(
        by_url["https://www.geeksforgeeks.org/python/python-exception-handling/"]
            .date
            .as_deref(),
        Some("2026-05-29")
    );
    assert_eq!(
        by_url["https://github.com/python/cpython/issues/116048"]
            .date
            .as_deref(),
        Some("2024-02-28")
    );
    // First-crawl time on evergreen pages must NOT appear as a publish date.
    assert_eq!(
        by_url["https://docs.python.org/3/library/asyncio-task.html"].date,
        None
    );
    assert_eq!(
        by_url["https://realpython.com/python311-exception-groups/"].date,
        None
    );
    // All Seznam dates are display-only ("snippet"), never confident.
    for h in by_url.values() {
        if h.date.is_some() {
            assert_eq!(h.date_source.as_deref(), Some("snippet"));
        }
    }
}

#[test]
fn test_hits_without_http_url_or_title_are_skipped() {
    let mut state = sample_state();
    let arr = state["data"]["entities"][0]["subEntities"][0]["subEntities"]
        .as_array_mut()
        .unwrap();
    arr.push(json!({"id":{"service":"ftxt-organic","type":"result"},"data":{"snippet":{"url":"javascript:void(0)","title":"Bad"}}}));
    arr.push(json!({"id":{"service":"ftxt-organic","type":"result"},"data":{"snippet":{"url":"https://example.com/no-title","title":""}}}));
    arr.push(json!({"id":{"service":"ftxt-organic","type":"result"},"data":{"snippet":{"title":"No URL"}}}));

    let urls: Vec<String> = parse(&page_from_state(&state))
        .into_iter()
        .map(|r| r.url)
        .collect();
    assert_eq!(urls.len(), 4);
    assert!(!urls.contains(&"https://example.com/no-title".to_owned()));
}

#[test]
fn test_unparseable_pages_yield_empty() {
    for page in [
        "",
        "<html><body>nothing here</body></html>",
        r#"<script id="renderer-state-data" data-state="{not json"></script>"#,
        r#"<script id="renderer-state-data" data-state="[1, 2]"></script>"#,
        r#"<script id="renderer-state-data" data-state="{&quot;data&quot;: null}"></script>"#,
    ] {
        assert!(parse(page).is_empty(), "page={page}");
    }
}

fn mk_q(
    query: &str,
    limit: u32,
    freshness: Option<Freshness>,
    domains: Vec<String>,
    filetype: Option<&str>,
) -> SerpQuery {
    let mut cx = SrcCx::from_config(
        &Config::default(),
        CancellationToken::new(),
        Duration::from_secs(5),
    );
    cx.freshness = freshness;
    cx.domains = domains;
    let mut p = json!({"query": query, "limit": limit});
    if let Some(ft) = filetype {
        p["filetype"] = json!(ft);
    }
    SerpQuery::from_call("seznam", "search", &p, &cx, &Config::default()).unwrap()
}

#[test]
fn test_build_url_plain_operators_count_and_no_freshness() {
    assert_eq!(
        build_url(&mk_q("python asyncio", 10, None, vec![], None)),
        "https://search.seznam.cz/?q=python+asyncio"
    );
    let op_url = build_url(&mk_q(
        "asyncio",
        10,
        None,
        vec!["github.com".into(), "-spam.example".into()],
        Some("pdf"),
    ));
    assert!(op_url.contains("site%3Agithub.com"));
    assert!(op_url.contains("-site%3Aspam.example"));
    assert!(op_url.contains("filetype%3Apdf"));

    assert!(!build_url(&mk_q("q", 10, None, vec![], None)).contains("count="));
    assert!(build_url(&mk_q("q", 11, None, vec![], None)).ends_with("&count=20"));
    assert!(build_url(&mk_q("q", 50, None, vec![], None)).ends_with("&count=20"));

    assert_eq!(
        build_url(&mk_q("q", 10, Some(Freshness::Week), vec![], None)),
        build_url(&mk_q("q", 10, None, vec![], None))
    );
}
