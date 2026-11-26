//! Port of `tests/test_naver.py`: `fds-web-doc-root` web documents, `"새 창 열림"`
//! stripping, dotted & relative Korean profile dates, ad/UGC skipping, and `nso` URLs.

use fsm_core::api::Freshness;
use fsm_core::config::Config;
use fsm_core::source::SrcCx;
use fsm_engines_serp::common::SerpQuery;
use fsm_engines_serp::engines::naver::{build_url, parse};
use serde_json::json;
use std::collections::HashSet;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const NAV: &str = r#"<div class="api_flicking_wrap"><a href="?ssc=tab.blog.all">블로그</a></div>"#;

const AD: &str = r#"<section><div class="ad_section"><a href="https://ader.naver.com/v1/6D7mN9"><span class="lnk_tit">파이썬 강의</span></a></div></section>"#;

const BLOG_CARD: &str = r#"
<div class="fds-ugc-ur-desk">
  <a href="https://blog.naver.com/vronpy/224459930245"><span class="sds-comps-text">블로그 글</span></a>
</div>"#;

fn doc_item(url: &str, date: &str, crumbs: &str, title: &str, snippet: &str) -> String {
    let date_link = if date.is_empty() {
        String::new()
    } else {
        format!(
            r#"<a href="{url}"><span class="sds-comps-text-type-body3">{date}</span><span class="PTx">새 창 열림</span></a>"#
        )
    };
    format!(
        r#"
<div class="fds-web-doc-root">
  <span class="sds-comps-profile-info-subtext">{date_link}<a href="{url}"><span>{crumbs}</span><span>새 창 열림</span></a></span>
  <a href="{url}"><span class="sds-comps-text-type-headline1">{title}</span><span>새 창 열림</span></a>
  <a href="{url}"><span class="sds-comps-text-type-body1">{snippet}</span><span>새 창 열림</span></a>
</div>"#
    )
}

fn collection(items: &[String]) -> String {
    format!(
        r#"<div class="fds-web-root"><div class="fds-web-list-root">{}</div></div>"#,
        items.join("")
    )
}

fn sample_html() -> String {
    let rp = doc_item(
        "https://realpython.com/python-async-features/",
        "2026.08.31.",
        "realpython.com › python-async-features",
        "Async Programming in <mark>Python</mark>: From Generators to <mark>asyncio</mark> – Real <mark>Python</mark>",
        "Learn how <mark>Python</mark> async programming works. Write async functions with async and await, and run slow I/O operations concurrently with <mark>asyncio</mark>.",
    );
    let yun = doc_item(
        "https://076923.github.io/posts/Python-51/",
        "3주 전",
        "076923.github.io › posts",
        "<mark>Python</mark> 강좌 : 제 51강 - 비동기 처리 - YUN DAE HEE",
        "비동기 처리(Asynchronous Processing)란 어떤 작업이 끝나기를 기다리는 동안 다른 작업을 먼저 진행하는 처리 방식입니다. <mark>Python</mark>에서.....",
    );
    let gihyo = doc_item(
        "https://gihyo.jp/article/2022/10/monthly-python-2210",
        "",
        "gihyo.jp › monthly-python-2210",
        "<mark>Python</mark> 3.11の新機能：<mark>asyncio</mark>.<mark>TaskGroup</mark>を使った予測可能でより安全な非同期処理",
        "こんにちは、福田（@JunyaFff）です。",
    );
    format!(
        "<html><body>{NAV}{AD}{}</body></html>",
        collection(&[rp.clone(), yun, BLOG_CARD.to_owned(), gihyo, rp])
    )
}

#[test]
fn test_parse_yields_web_documents_in_order() {
    let out = parse(&sample_html());
    assert_eq!(
        out.iter().map(|r| r.url.as_str()).collect::<Vec<_>>(),
        vec![
            "https://realpython.com/python-async-features/",
            "https://076923.github.io/posts/Python-51/",
            "https://gihyo.jp/article/2022/10/monthly-python-2210",
        ]
    );
    assert!(out.iter().all(|r| r.source == "naver"));
}

#[test]
fn test_titles_and_snippets_are_clean() {
    let out = parse(&sample_html());
    assert_eq!(
        out[0].title,
        "Async Programming in Python: From Generators to asyncio – Real Python"
    );
    assert!(
        out[0]
            .snippet
            .starts_with("Learn how Python async programming works.")
    );
    assert!(out[1].snippet.ends_with("Python에서....."));
    assert_eq!(
        out[2].title,
        "Python 3.11の新機能：asyncio.TaskGroupを使った予測可能でより安全な非同期処理"
    );
    assert!(
        !out.iter()
            .any(|r| r.title.contains("새 창 열림") || r.snippet.contains("새 창 열림"))
    );
}

#[test]
fn test_new_window_label_is_stripped_even_inside_the_title_span() {
    let page = sample_html().replace(
        "Real <mark>Python</mark></span>",
        r#"Real <mark>Python</mark><span class="blind">새 창 열림</span></span>"#,
    );
    assert!(parse(&page)[0].title.ends_with("– Real Python"));
}

#[test]
fn test_profile_date_becomes_published_age() {
    let out = parse(&sample_html());
    assert_eq!(out[0].date.as_deref(), Some("2026-08-31"));
    assert_eq!(out[1].date.as_deref(), Some("3 weeks ago"));
    assert_eq!(out[2].date, None);
    for h in &out {
        if h.date.is_some() {
            assert_eq!(h.date_source.as_deref(), Some("snippet"));
        }
    }
}

#[test]
fn test_ads_ugc_cards_and_navigation_are_skipped() {
    let urls: Vec<String> = parse(&sample_html()).into_iter().map(|r| r.url).collect();
    assert!(!urls.iter().any(|u| u.contains("naver.com")));
    let uniq: HashSet<&String> = urls.iter().collect();
    assert_eq!(uniq.len(), urls.len());
}

#[test]
fn test_ad_redirect_is_skipped_even_as_a_web_document() {
    let ad_doc = doc_item(
        "https://ader.naver.com/v1/6D7mN9Mp5fvBx0yOlTcN?c=naver.search.pc.plsearch",
        "",
        "ader.naver.com",
        "파이썬 강의",
        "광고",
    );
    let yun = doc_item(
        "https://076923.github.io/posts/Python-51/",
        "3주 전",
        "076923.github.io",
        "Python 강좌",
        "비동기",
    );
    let out = parse(&collection(&[ad_doc, yun]));
    assert_eq!(
        out.iter().map(|r| r.url.as_str()).collect::<Vec<_>>(),
        vec!["https://076923.github.io/posts/Python-51/"]
    );
}

#[test]
fn test_pages_without_web_documents_yield_empty() {
    for page in [
        "".to_owned(),
        "<html><body>nothing here</body></html>".to_owned(),
        format!("{NAV}{AD}{}", collection(&[BLOG_CARD.to_owned()])),
        r#"<div class="fds-web-doc-root"><span class="sds-comps-text-type-headline1">orphan</span></div>"#.to_owned(),
    ] {
        assert!(parse(&page).is_empty(), "page={page}");
    }
}

fn mk_q(
    query: &str,
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
    let mut p = json!({"query": query, "limit": 10});
    if let Some(ft) = filetype {
        p["filetype"] = json!(ft);
    }
    SerpQuery::from_call("naver", "search", &p, &cx, &Config::default()).unwrap()
}

#[test]
fn test_build_url_plain_operators_and_nso_freshness() {
    assert_eq!(
        build_url(&mk_q("파이썬 비동기", None, vec![], None)),
        "https://search.naver.com/search.naver?where=nexearch&query=%ED%8C%8C%EC%9D%B4%EC%8D%AC+%EB%B9%84%EB%8F%99%EA%B8%B0"
    );
    let ops = build_url(&mk_q(
        "asyncio",
        None,
        vec!["github.com".into(), "-spam.example".into()],
        Some("pdf"),
    ));
    assert!(ops.contains("site%3Agithub.com"));
    assert!(ops.contains("-site%3Aspam.example"));
    assert!(ops.contains("filetype%3Apdf"));

    for (f, tok) in [
        (Freshness::Day, "p%3A1d"),
        (Freshness::Week, "p%3A1w"),
        (Freshness::Month, "p%3A1m"),
        (Freshness::Year, "p%3A1y"),
    ] {
        let u = build_url(&mk_q("q", Some(f), vec![], None));
        assert!(u.ends_with(&format!("&sm=tab_opt&nso={tok}")), "{u}");
    }
    assert!(!build_url(&mk_q("q", None, vec!["a.kr".into()], None)).contains("nso="));
}
