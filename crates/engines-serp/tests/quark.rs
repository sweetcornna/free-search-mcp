//! Port of `tests/test_quark.py`: `div.sc[data-sc]` organic card filtering,
//! `ads` flag, `sm.cn`/`quark.cn` own-host rejection, source-line dates, x5sec wall,
//! and operator-free `build_url` with `tl_request`.

use fsm_core::api::Freshness;
use fsm_core::config::Config;
use fsm_core::source::SrcCx;
use fsm_engines_serp::common::{SerpQuery, detect_gate};
use fsm_engines_serp::engines::quark::{GATE_MARKERS, build_url, parse};
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn sc_attr(sc: &str, ads: Option<i32>) -> String {
    let mut obj =
        json!({"sc": sc, "q": "python+%E5%BC%82%E6%AD%A5%E7%BC%96%E7%A8%8B", "pos": "1_1"});
    if let Some(a) = ads {
        obj["ads"] = json!(a);
    }
    serde_json::to_string(&obj)
        .unwrap()
        .replace('&', "&amp;")
        .replace('"', "&quot;")
}

fn sample_html() -> String {
    let ss_text = format!(
        r#"<div class="sc" data-sc="{}"><div class="qk-card">
 <div class="qk-title"><a href="https://blog.csdn.net/agoutongxie/article/details/167988798" class="qk-link-wrapper"><div class="qk-title-text"><em>Python异步编程</em>实战指南-CSDN博客</div></a></div>
 <div class="summary-section"><div class="qk-paragraph-text">IO 密集场景异步才是王道——<em>asyncio 是 Python 异步编程的核心</em>。本文覆盖协程、事件循环、async/await。</div></div>
 <div class="source-section"><span class="qk-source-item">CSDN博客 · 2026-10-15</span></div>
</div></div>"#,
        sc_attr("ss_text", Some(0))
    );
    let ss_kv = format!(
        r#"<div class="sc" data-sc="{}"><div class="qk-card">
 <div class="qk-title"><a href="https://cloud.tencent.com/developer/article/2532143"><div class="qk-title-text">rust select! 与取消安全的 Future-腾讯云</div></a></div>
 <div class="qk-paragraph-text"><em>tokio</em>::<em>select</em>! 宏用于并发 poll 多个 Future，并在首个完成时返回。</div>
 <span class="qk-source-item">cloud.tencent.com</span>
</div></div>"#,
        sc_attr("ss_kv", Some(0))
    );
    let nature = format!(
        r#"<div class="sc" data-sc="{}"><div class="qk-card">
 <a href="http://m.php.cn/article/561601.html"><div class="qk-title-text"><em>Python</em> web<em>异步编程</em>的基础-php中文网</div>
 <div class="qk-paragraph-text">随着web应用程序的不断增长和复杂性的增加，web<em>异步编程</em>变得越来越流行。</div></a>
 <span class="qk-source-item">php中文网</span>
</div></div>"#,
        sc_attr("nature_result", None)
    );
    let ss_pic = format!(
        r#"<div class="sc" data-sc="{}"><div class="qk-card">
 <a href="https://liaoxuefeng.com/books/python/async-io/asyncio/index.html"><div class="qk-title-text">使用<em>asyncio</em>-<em>Python</em>教程-廖雪峰的官方网站</div></a>
 <div class="qk-paragraph-text"><em>asyncio</em> 是Python 3.4版本引入的标准库，直接内置了对异步IO的支持。</div>
 <span class="qk-source-item">廖雪峰的官方网站 · 6天前</span>
</div></div>"#,
        sc_attr("ss_pic", Some(0))
    );
    let related = format!(
        r#"<div class="sc" data-sc="{}"><div class="qk-title-text">其他人还搜了</div><a href="https://quark.sm.cn/s?q=x">rust tokio</a></div>"#,
        sc_attr("text_recommend", Some(0))
    );
    let baike = format!(
        r#"<div class="sc" data-sc="{}"><a href="https://baike.baidu.com/item/Python"><div class="qk-title-text">Python异步编程-百度百科</div></a></div>"#,
        sc_attr("baike", Some(0))
    );
    let own_video = format!(
        r#"<div class="sc" data-sc="{}"><a href="https://page.sm.cn/blm/video-page-710/video"><div class="qk-title-text">Async Rust</div></a></div>"#,
        sc_attr("nature_result", None)
    );
    let note = format!(
        r#"<div class="sc" data-sc="{}"><div class="qk-card"></div></div>"#,
        sc_attr("ss_note", Some(0))
    );
    let ad = r#"<div class="cpc-card"><a href="http://quark.sm.cn/adclick?x=1">广告</a></div>"#;

    format!(
        "<html><body><div id='results'>{own_video}{ss_text}{related}{ad}{ss_kv}{baike}{nature}{note}{ss_pic}{ss_text}</div></body></html>"
    )
}

#[test]
fn test_parse_yields_the_organic_cards_in_order() {
    let out = parse(&sample_html());
    assert_eq!(
        out.iter().map(|r| r.url.as_str()).collect::<Vec<_>>(),
        vec![
            "https://blog.csdn.net/agoutongxie/article/details/167988798",
            "https://cloud.tencent.com/developer/article/2532143",
            "http://m.php.cn/article/561601.html",
            "https://liaoxuefeng.com/books/python/async-io/asyncio/index.html",
        ]
    );
    assert!(out.iter().all(|r| r.source == "quark"));
}

#[test]
fn test_titles_and_snippets_are_clean() {
    let out = parse(&sample_html());
    assert_eq!(out[0].title, "Python异步编程实战指南-CSDN博客");
    assert!(
        out[0]
            .snippet
            .starts_with("IO 密集场景异步才是王道——asyncio 是 Python 异步编程的核心")
    );
    assert!(out[1].snippet.starts_with("tokio::select! 宏用于并发 poll"));
    assert_eq!(out[2].title, "Python web异步编程的基础-php中文网");
}

#[test]
fn test_widgets_ads_and_own_pages_are_skipped() {
    let out = parse(&sample_html());
    assert!(
        !out.iter()
            .any(|r| r.url.contains("sm.cn") || r.url.contains("quark.cn"))
    );
    assert!(!out.iter().any(|r| r.url.contains("baike.baidu.com")));
    assert!(!out.iter().any(|r| r.title == "其他人还搜了"));
}

#[test]
fn test_ads_flag_on_a_card_is_honoured() {
    let flagged = format!(
        r#"<div class="sc" data-sc="{}"><a href="https://ad.example/1"><div class="qk-title-text">Ad</div></a></div>
           <div class="sc" data-sc="{}"><a href="https://cloud.tencent.com/developer/article/2532143"><div class="qk-title-text">Real</div></a></div>"#,
        sc_attr("ss_text", Some(1)),
        sc_attr("ss_kv", Some(0))
    );
    let out = parse(&flagged);
    assert_eq!(out.len(), 1);
    assert_eq!(
        out[0].url,
        "https://cloud.tencent.com/developer/article/2532143"
    );
}

#[test]
fn test_source_line_date_becomes_published_age() {
    let by_url: HashMap<String, _> = parse(&sample_html())
        .into_iter()
        .map(|r| (r.url.clone(), r))
        .collect();
    assert_eq!(
        by_url["https://blog.csdn.net/agoutongxie/article/details/167988798"]
            .date
            .as_deref(),
        Some("2026-10-15")
    );
    assert_eq!(
        by_url["https://liaoxuefeng.com/books/python/async-io/asyncio/index.html"]
            .date
            .as_deref(),
        Some("6 days ago")
    );
    assert_eq!(by_url["http://m.php.cn/article/561601.html"].date, None);
    for h in by_url.values() {
        if h.date.is_some() {
            assert_eq!(h.date_source.as_deref(), Some("snippet"));
        }
    }
}

#[test]
fn test_pages_without_organic_cards_yield_empty() {
    for page in [
        "",
        "<html><body>nothing here</body></html>",
        "<html><body><div id='results'>抱歉，没有找到相关的结果。</div></body></html>",
        r#"<div class="sc" data-sc="{oops"><div class="qk-title-text">x</div></div>"#,
    ] {
        assert!(parse(page).is_empty(), "page={page}");
    }
}

#[test]
fn test_x5sec_punish_page_yields_empty_and_triggers_captcha_gate() {
    let wall = r#"<script>sessionStorage.x5referer = window.location.href;var url = window.location.protocol + "//quark.sm.cn//s/_____tmd_____/punish?x5secdata=xfD2F0HgaLjOD8vVoxMij&x5step=1";window.location.replace(url);</script><!--rgv587_flag:sm-->"#;
    assert!(parse(wall).is_empty());
    assert_eq!(detect_gate(wall, GATE_MARKERS), Some("captcha"));
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
    SerpQuery::from_call("quark", "search", &p, &cx, &Config::default()).unwrap()
}

#[test]
fn test_build_url_plain_no_operators_and_tl_request() {
    assert_eq!(
        build_url(&mk_q("python 异步编程", None, vec![], None)),
        "https://quark.sm.cn/s?q=python+%E5%BC%82%E6%AD%A5%E7%BC%96%E7%A8%8B"
    );
    let filtered = build_url(&mk_q(
        "asyncio",
        None,
        vec!["zhihu.com".into(), "-csdn.net".into()],
        Some("pdf"),
    ));
    assert_eq!(filtered, build_url(&mk_q("asyncio", None, vec![], None)));
    assert!(!filtered.contains("site") && !filtered.contains("filetype"));

    for (f, tok) in [
        (Freshness::Day, "4"),
        (Freshness::Week, "3"),
        (Freshness::Month, "2"),
        (Freshness::Year, "1"),
    ] {
        let u = build_url(&mk_q("q", Some(f), vec![], None));
        assert!(u.ends_with(&format!("&tl_request={tok}")), "{u}");
    }
}
