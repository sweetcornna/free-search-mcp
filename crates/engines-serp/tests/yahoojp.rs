//! Port of `tests/test_yahoojp.py`: Yahoo! JAPAN parsing, inline highlights,
//! answer/sitelink/internal filtering, `build_url`, and EEA/UK region gate.

use fsm_core::api::Freshness;
use fsm_core::config::Config;
use fsm_core::source::SrcCx;
use fsm_engines_serp::common::{SerpQuery, detect_gate, extract_date_hint_now};
use fsm_engines_serp::engines::yahoojp::{GATE_MARKERS, build_url, parse};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use url::Url;

fn algo(href: &str, title_html: &str, summary_html: &str, modifier: &str, extra: &str) -> String {
    let cls = format!("sw-Card Algo {modifier}").trim().to_owned();
    format!(
        r#"
<div class="sw-CardBase"><div class="{cls}"><section>
  <div class="sw-Card__section sw-Card__section--header"><div class="sw-Card__headerSpace"><div class="sw-Card__title">
    <a href="{href}" class="sw-Card__titleInner" ping="/xts/FOR=abc"><br/>
      <h3 class="sw-Card__titleMain"><span>{title_html}</span></h3>
      <div class="sw-Card__titleCiteWrapper"><div class="sw-Cite Algo__cite">
        <div class="sw-Cite__text"><span class="sw-Cite__textSiteName">Site name</span>
          <div class="sw-Cite__textUrl"><cite class="sw-Breadcrumbs"><ol class="sw-Breadcrumbs__orderedList">
            <li class="sw-Breadcrumbs__item">https://example</li>
            <li class="sw-Breadcrumbs__item"><span>&gt;</span><span>crumb</span></li>
          </ol></cite></div></div></div></div>
    </a>
  </div></div></div>
  <div class="sw-Card__section"><div class="sw-Card__space">
    <div class="sw-Card__floatContainer"><p class="sw-Card__summary">{summary_html}</p></div>{extra}
  </div></div>
</section></div></div>"#
    )
}

const SITELINKS: &str = r#"
<ul class="Algo__oslLinksList">
  <li class="Algo__oslLinksListItem"><a href="https://diamond.jp/zai/category/creditcard#%E5%B9%B4%E4%BC%9A%E8%B2%BB">◇年会費無料の高還元クレジット...</a></li>
</ul>"#;

const CHIEBUKURO: &str = r#"
<div class="sw-CardBase"><div class="sw-Card AnswerChiebukuro"><section>
  <div class="sw-Card__title"><a href="https://chiebukuro.yahoo.co.jp/search/?p=x" class="sw-Card__titleInner"><h3>Q&amp;A</h3></a></div>
</section></div></div>"#;

const VIDEO: &str = r#"
<div class="sw-CardBase"><div class="sw-Card AnswerVideoUniversal"><section>
  <div class="sw-Card__title"><a href="https://search.yahoo.co.jp/video/search?p=x" class="sw-Card__titleInner"><h3>動画</h3></a></div>
  <a href="https://www.youtube.com/watch?v=FvWXyAXyb4Q">Python asyncio TaskGroup</a>
</section></div></div>"#;

fn sample_page() -> String {
    format!(
        r#"<!doctype html><html lang="ja"><body><div class="Contents__innerGroupBody">{}{}{}{}{}{}{}{}{}</div></body></html>"#,
        algo(
            "https://docs.python.org/3/library/asyncio-task.html",
            "Coroutines and tasks — <b>Python</b> 3.15.2 documentation",
            "In the case where a <b>task group</b> is cancelled externally and also must raise an ExceptionGroup , it will call the parent task&#39;s cancel() method. This ensures that&nbsp;...",
            "",
            "",
        ),
        CHIEBUKURO,
        algo(
            "https://diamond.jp/zai/category/creditcard",
            "【<b>クレジットカードおすすめ</b>比較】専門家が人気<b>クレカ</b>の還元 ...",
            "\n    <span class=\"util-Text--sub\">2026/10/1</span>\n    <span class=\"util-Delimiter\">-</span>【2026年9月3日時点】 <b>おすすめ</b>の「年会費無料の高還元<b>クレジットカード</b>」はコレだ！&nbsp;...",
            "Algo-anotherSuggest",
            SITELINKS,
        ),
        algo(
            "https://stackoverflow.com/questions/79986011/why-does-asyncio-taskgroup-intermittently",
            "Why does <b>asyncio</b>.<b>TaskGroup</b> intermittently swallow exceptions?",
            r#"<span class="util-Text--sub">5日前</span><span class="util-Delimiter">-</span>I&#39;m building a high-concurrency scraper with <b>asyncio</b>.<b>TaskGroup</b>."#,
            "",
            "",
        ),
        VIDEO,
        algo(
            "https://docs.python.org/3/library/asyncio-task.html",
            "Coroutines and tasks — duplicate",
            "Duplicate row.",
            "",
            "",
        ),
        algo(
            "https://search.yahoo.co.jp/search?p=asyncio&ei=UTF-8",
            "asyncio の検索結果",
            "Yahoo's own page.",
            "",
            "",
        ),
        algo(
            "/search?p=Python%20asyncio",
            "Relative link",
            "Not a result.",
            "",
            ""
        ),
        algo("https://example.com/no-title", "", "No title.", "", ""),
    )
}

#[test]
fn test_parse_yields_direct_urls_titles_and_snippets() {
    let results = parse(&sample_page());
    assert_eq!(
        results.iter().map(|r| r.url.as_str()).collect::<Vec<_>>(),
        vec![
            "https://docs.python.org/3/library/asyncio-task.html",
            "https://diamond.jp/zai/category/creditcard",
            "https://stackoverflow.com/questions/79986011/why-does-asyncio-taskgroup-intermittently",
        ]
    );
    let first = &results[0];
    assert_eq!(
        first.title,
        "Coroutines and tasks — Python 3.15.2 documentation"
    );
    assert!(
        first
            .snippet
            .starts_with("In the case where a task group is cancelled externally")
    );
    assert!(!first.title.contains("Site name") && !first.title.contains("crumb"));
    assert!(results.iter().all(|r| r.source == "yahoojp"));
}

#[test]
fn test_highlights_keep_the_pages_own_spacing() {
    let results = parse(&sample_page());
    assert_eq!(
        results[1].title,
        "【クレジットカードおすすめ比較】専門家が人気クレカの還元 ..."
    );
    assert_eq!(
        results[2].title,
        "Why does asyncio.TaskGroup intermittently swallow exceptions?"
    );
    assert!(results[1].snippet.starts_with(
        "2026/10/1 -【2026年9月3日時点】 おすすめの「年会費無料の高還元クレジットカード」"
    ));
}

#[test]
fn test_published_age_comes_from_the_snippet_and_is_not_confident() {
    for r in parse(&sample_page()) {
        assert_eq!(r.date, extract_date_hint_now(&r.snippet));
        if r.date.is_some() {
            assert_eq!(r.date_source.as_deref(), Some("snippet"));
        } else {
            assert_eq!(r.date_source, None);
        }
    }
}

#[test]
fn test_answer_cards_sitelinks_and_yahoo_links_are_skipped() {
    let results = parse(&sample_page());
    let urls: Vec<&str> = results.iter().map(|r| r.url.as_str()).collect();
    assert!(!urls.iter().any(|u| u.contains("yahoo.co.jp")));
    assert!(!urls.iter().any(|u| u.contains("youtube.com")));
    assert!(!urls.iter().any(|u| u.contains('#')));
    let uniq: HashSet<&str> = urls.iter().copied().collect();
    assert_eq!(uniq.len(), urls.len());
}

#[test]
fn test_unparseable_page_yields_nothing() {
    for html in [
        "",
        "<html><body>JavaScriptが無効です</body></html>",
        "<<<not html at all>>>",
        r#"<div class="sw-Card Algo"><section></section></div>"#,
    ] {
        assert!(parse(html).is_empty(), "html={html}");
    }
}

#[test]
fn test_region_and_rate_limit_gates() {
    let eea = r#"<html><head><meta property="og:url" content="https://privacy.yahoo.co.jp/areacheck/index.html"></head><body>Notice</body></html>"#;
    assert_eq!(detect_gate(eea, GATE_MARKERS), Some("region"));
    let rl = "<html><body>ご覧になろうとしているページは現在表示できません</body></html>";
    assert_eq!(detect_gate(rl, GATE_MARKERS), Some("rate_limit"));
}

fn params_of(url: &str) -> HashMap<String, String> {
    Url::parse(url)
        .unwrap()
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
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
    SerpQuery::from_call("yahoojp", "search", &p, &cx, &Config::default()).unwrap()
}

#[test]
fn test_build_url_plain_operators_and_vd_freshness() {
    let url = build_url(&mk_q("人工智能 最新进展", None, vec![], None));
    assert!(url.starts_with("https://search.yahoo.co.jp/search?"));
    let p = params_of(&url);
    assert_eq!(p.get("p").map(String::as_str), Some("人工智能 最新进展"));
    assert_eq!(p.get("ei").map(String::as_str), Some("UTF-8"));
    assert!(!p.contains_key("vd"));

    let ops = build_url(&mk_q(
        "asyncio taskgroup",
        None,
        vec!["docs.python.org".into(), "-reddit.com".into()],
        None,
    ));
    assert_eq!(
        params_of(&ops).get("p").map(String::as_str),
        Some("asyncio taskgroup site:docs.python.org -site:reddit.com")
    );

    let multi = build_url(&mk_q(
        "taskgroup",
        None,
        vec!["docs.python.org".into(), "peps.python.org".into()],
        None,
    ));
    assert_eq!(
        params_of(&multi).get("p").map(String::as_str),
        Some("taskgroup (site:docs.python.org OR site:peps.python.org)")
    );

    let pdf = build_url(&mk_q("asyncio", None, vec![], Some("pdf")));
    assert_eq!(
        params_of(&pdf).get("p").map(String::as_str),
        Some("asyncio filetype:pdf")
    );

    for (f, vd) in [
        (Freshness::Day, "d"),
        (Freshness::Week, "w"),
        (Freshness::Month, "m1"),
        (Freshness::Year, "y"),
    ] {
        let u = build_url(&mk_q("python release", Some(f), vec![], None));
        assert_eq!(params_of(&u).get("vd").map(String::as_str), Some(vd));
    }
}

#[test]
fn test_parse_dedups_repeated_urls() {
    // Port of `test_yahoojp.py::test_parse_dedups_repeated_urls`.
    let urls: Vec<String> = parse(&sample_page()).into_iter().map(|r| r.url).collect();
    let unique: HashSet<&str> = urls.iter().map(String::as_str).collect();
    assert_eq!(urls.len(), unique.len());
    assert_eq!(urls.len(), 3);
}
