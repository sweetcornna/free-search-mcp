//! Offline `ask` fast-mode tests using `ReplayNet` fixtures, real `FetchService` +
//! `SqliteStore` cache, real `SearchService`, and `fsm_surface::render` (ruling R5
//! Deliverables 1 & 6):
//! - English factual question (search + parallel top-3 page read + sentence-window ranking + warm cache)
//! - Chinese factual question (CJK-aware BM25 + rapidfuzz passage selection)
//! - Registry/version question answered by a direct claimant (`claims() >= 0.8`, RULINGS R1)
//! - Page-reading question selecting exact factual sentence from noisy paragraphs
//! - Timeout returning partial results within hard deadline

use async_trait::async_trait;
use fsm_agent::AskService;
use fsm_agent::fast::FastConfig;
use fsm_core::fixture::{Exchange, Fixture, FixtureRequest, FixtureResponse, ReplayNet};
use fsm_core::{
    AskApi, AskMode, AskOpts, CallCx, Capability, Config, DocApi, DocOutline, DocSlice, FetchApi,
    FetchMethod, FetchOpts, FetchOutcome, FsmError, Hit, HitKind, NetClient, PageResult, PoolRole,
    Registry, RenderKind, SearchApi, SearchRequest, SearchResponse, Source, SourceDecl, SourceDeps,
    SrcCx, SrcOutput, StoreApi,
};
use fsm_extract::FetchService;
use fsm_search::SearchService;
use fsm_store::SqliteStore;
use fsm_surface::{RenderOpts, render};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct NoopDoc;
#[async_trait]
impl DocApi for NoopDoc {
    async fn read(
        &self,
        s: &str,
        _: usize,
        _: Option<usize>,
        _: &CallCx,
    ) -> Result<DocSlice, FsmError> {
        Ok(DocSlice::new(s, "txt", ""))
    }
    async fn outline(&self, s: &str, _: &CallCx) -> Result<DocOutline, FsmError> {
        Ok(DocOutline::new(s, "txt"))
    }
}

/// Fixture-backed web search source that returns deterministic hits pointing at ReplayNet URLs.
struct FixtureWebSource {
    decl: SourceDecl,
    hits: Vec<Hit>,
    calls: AtomicUsize,
}

impl FixtureWebSource {
    fn new(id: &str, hits: Vec<Hit>) -> Self {
        let decl = SourceDecl::new(id, id, "offline fixture web source")
            .with_scopes(&["web"])
            .with_caps(&[Capability::Search]);
        Self {
            decl,
            hits,
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl Source for FixtureWebSource {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }
    async fn call(&self, _op: &str, _params: Value, _cx: &SrcCx) -> Result<SrcOutput, FsmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(SrcOutput::Hits(self.hits.clone()))
    }
}

/// Direct-claimant registry source (`claims() >= 0.8`, RULINGS R1) fetching JSON via `ReplayNet`.
struct DirectCratesSource {
    decl: SourceDecl,
    net: Arc<dyn NetClient>,
    calls: AtomicUsize,
}

impl DirectCratesSource {
    fn new(net: Arc<dyn NetClient>) -> Self {
        let mut decl = SourceDecl::new("crates", "crates.io", "Rust crate registry")
            .with_scopes(&["code"])
            .with_caps(&[Capability::Search]);
        decl.pool_role = PoolRole::DirectClaimant;
        Self {
            decl,
            net,
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl Source for DirectCratesSource {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }
    fn claims(&self, query: &str, _lang: Option<&str>) -> f32 {
        let low = query.to_ascii_lowercase();
        if low.contains("crate") || low.contains("crates.io") || low.contains("serde version") {
            0.92
        } else {
            0.0
        }
    }
    async fn call(&self, _op: &str, _params: Value, cx: &SrcCx) -> Result<SrcOutput, FsmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let resp = self
            .net
            .send(
                fsm_core::HttpRequest::get("https://crates.io/api/v1/crates/serde"),
                fsm_core::TransportProfile::Plain,
                &fsm_core::NetCx::new(cx.cancel.clone(), cx.timeout),
            )
            .await?;
        let val: Value = serde_json::from_slice(&resp.body).unwrap();
        let c = &val["crate"];
        let mut h = Hit::new(
            format!(
                "{} {}",
                c["name"].as_str().unwrap(),
                c["max_stable_version"].as_str().unwrap()
            ),
            "https://crates.io/crates/serde",
        );
        h.snippet = format!(
            "Latest stable version: {} (updated {}). {}",
            c["max_stable_version"].as_str().unwrap(),
            c["updated_at"].as_str().unwrap(),
            c["description"].as_str().unwrap()
        );
        h.date = Some(c["updated_at"].as_str().unwrap()[..10].to_owned());
        h.source = "crates".into();
        h.sources = vec!["crates".into()];
        h.kind = HitKind::Record;
        Ok(SrcOutput::Hits(vec![h]))
    }
}

fn html_exchange(url: &str, html: &str) -> Exchange {
    Exchange::new(
        FixtureRequest::new("GET", url),
        FixtureResponse::from_bytes(
            200,
            vec![("content-type".into(), "text/html; charset=utf-8".into())],
            html.as_bytes(),
        ),
    )
}

fn json_exchange(url: &str, body: &Value) -> Exchange {
    let text = serde_json::to_string(body).unwrap();
    Exchange::new(
        FixtureRequest::new("GET", url),
        FixtureResponse::from_bytes(
            200,
            vec![("content-type".into(), "application/json".into())],
            text.as_bytes(),
        ),
    )
}

#[tokio::test]
async fn test_fast_ask_english_and_chinese_pages_with_replay_net_and_warm_cache() {
    let tmp = TempDir::new().unwrap();
    let mut cfg = Config::defaults_without_home();
    cfg.cache_dir = tmp.path().join("cache");
    cfg.config_dir = tmp.path().join("config");
    cfg.default_engines = vec!["web_a".into(), "web_b".into()];
    cfg.reserve_engines = vec![];
    cfg.min_healthy_engines = 1;
    let cfg = Arc::new(cfg);

    let url_en = "https://blog.rust-lang.org/2025/02/20/Rust-1.85.0.html";
    let url_zh = "https://docs.example.cn/rust-185-release";
    let url_undated = "https://wiki.example.org/rust-editions";

    let mut fx = Fixture::new("ask_fast", "pages", "2026-11-25T10:00:00Z");
    fx.exchanges.push(html_exchange(
        url_en,
        r#"<!doctype html><html><head><title>Announcing Rust 1.85.0 and Rust 2024</title>
        <meta property="article:published_time" content="2025-02-20"/></head><body>
        <article>
        <p>Welcome to the Rust blog! Today we share community updates and conference links.</p>
        <p>The Rust team is happy to announce a new version of Rust, 1.85.0, released on February 20, 2025. Rust 1.85.0 stabilizes the Rust 2024 Edition and async closures.</p>
        <p>Thanks to all contributors who helped test release candidates across every tier-1 target.</p>
        </article></body></html>"#,
    ));
    fx.exchanges.push(html_exchange(
        url_zh,
        r#"<!doctype html><html><head><title>Rust 1.85 正式发布说明</title>
        <meta name="date" content="2025-02-21"/></head><body>
        <article>
        <p>社区公告与活动日程安排，欢迎参与各地线下聚会。</p>
        <p>Rust 1.85.0 正式版本于 2025年2月20日 发布，这是包含 Rust 2024 Edition 和异步闭包的重大版本更新。</p>
        <p>开发者可以通过 rustup update 命令升级到最新稳定版。</p>
        </article></body></html>"#,
    ));
    fx.exchanges.push(html_exchange(
        url_undated,
        r#"<!doctype html><html><head><title>Rust Editions Overview</title></head><body>
        <article>
        <p>Rust ships a new edition on a regular multi-year cadence with opt-in ergonomics and zero breakage for older crates.</p>
        </article></body></html>"#,
    ));

    let net: Arc<dyn NetClient> = Arc::new(ReplayNet::from_fixtures(vec![fx]).unwrap());
    let store = Arc::new(SqliteStore::open_path(tmp.path().join("ask.sqlite")).unwrap());
    let store_api: Arc<dyn StoreApi> = store.clone();
    let deps = SourceDeps::new(Arc::clone(&net), Arc::clone(&store_api), Arc::clone(&cfg));

    let mut h1 = Hit::new("Announcing Rust 1.85.0", url_en);
    h1.snippet = "Rust 1.85.0 and Rust 2024 edition announcement.".into();
    h1.date = Some("2025-02-20".into());
    let mut h2 = Hit::new("Rust 1.85 正式发布说明", url_zh);
    h2.snippet = "Rust 1.85.0 中文发布说明".into();
    h2.date = Some("2025-02-21".into());
    let mut h3 = Hit::new("Rust Editions Overview", url_undated);
    h3.snippet = "Overview of Rust editions.".into();

    let mut reg = Registry::new();
    reg.register(Arc::new(FixtureWebSource::new(
        "web_a",
        vec![h1.clone(), h2.clone(), h3.clone()],
    )))
    .unwrap();
    reg.register(Arc::new(FixtureWebSource::new("web_b", vec![h1, h2])))
        .unwrap();
    let reg = Arc::new(reg);

    let search: Arc<dyn SearchApi> = Arc::new(SearchService::new(Arc::clone(&reg), deps.clone()));
    let doc: Arc<dyn DocApi> = Arc::new(NoopDoc);
    let fetch: Arc<dyn FetchApi> = Arc::new(FetchService::new(
        deps,
        Arc::clone(&doc),
        Arc::clone(&search),
    ));
    let ask = AskService::new(search, fetch, doc, reg, cfg);

    // 1. English page-reading question
    let mut opts = AskOpts::default();
    opts.mode = AskMode::Fast;
    let resp_en = ask
        .ask(
            "When was Rust 1.85.0 released and what did it stabilize?",
            opts.clone(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert_eq!(resp_en.mode, "fast");
    assert!(!resp_en.partial);
    assert!(!resp_en.passages.is_empty());
    let top_en = &resp_en.passages[0];
    assert!(
        top_en.text.contains("1.85.0") && top_en.text.contains("February 20, 2025"),
        "expected exact factual sentence on top, got: {:?}",
        resp_en.passages
    );
    assert!(!resp_en.retrieved_at.is_empty());

    // Rendered Markdown includes source title, URL, publication date or "undated", and retrieval time!
    let md_en = render(
        RenderKind::AskResponse,
        &serde_json::to_value(&resp_en).unwrap(),
        &RenderOpts::default().with_max_tokens(1500),
    );
    assert!(md_en.contains("February 20, 2025"), "{md_en}");
    assert!(md_en.contains(url_en), "{md_en}");
    assert!(md_en.contains("2025-02-20"), "{md_en}");
    assert!(md_en.contains("retrieved"), "{md_en}");

    // 2. Chinese factual question (also hits warm SQLite page cache!)
    let t_warm = Instant::now();
    let resp_zh = ask
        .ask(
            "Rust 1.85 版本是什么时候发布的？",
            opts.clone(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(
        t_warm.elapsed() < Duration::from_secs(3),
        "warm cache must be fast"
    );
    assert_eq!(resp_zh.mode, "fast");
    assert!(!resp_zh.passages.is_empty());
    let has_zh_fact = resp_zh
        .passages
        .iter()
        .any(|p| p.text.contains("2025年2月20日") || p.text.contains("异步闭包"));
    assert!(
        has_zh_fact,
        "CJK passage selector missed answer: {:?}",
        resp_zh.passages
    );
    assert!(
        resp_zh.cache_age_s.is_some(),
        "warm cache must report cache_age_s"
    );

    // Verify undated source renders literally as "undated"
    let resp_ed = ask
        .ask(
            "Rust edition cadence ergonomics zero breakage",
            opts,
            &CallCx::default(),
        )
        .await
        .unwrap();
    let md_ed = render(
        RenderKind::AskResponse,
        &serde_json::to_value(&resp_ed).unwrap(),
        &RenderOpts::default(),
    );
    assert!(
        md_ed.contains("undated"),
        "undated page must render 'undated':\n{md_ed}"
    );

    store.close().await.unwrap();
}

#[tokio::test]
async fn test_fast_ask_direct_claimant_shortcut_skips_web_search() {
    let mut fx = Fixture::new("crates", "serde", "2026-11-25T10:00:00Z");
    fx.exchanges.push(json_exchange(
        "https://crates.io/api/v1/crates/serde",
        &json!({
            "crate": {
                "name": "serde",
                "max_stable_version": "1.0.229",
                "updated_at": "2026-11-10T12:00:00Z",
                "description": "A generic serialization/deserialization framework"
            }
        }),
    ));
    let net: Arc<dyn NetClient> = Arc::new(ReplayNet::from_fixtures(vec![fx]).unwrap());
    let cfg = Arc::new(Config::defaults_without_home());

    let web_src = Arc::new(FixtureWebSource::new("web_a", vec![]));
    let crates_src = Arc::new(DirectCratesSource::new(Arc::clone(&net)));

    let mut reg = Registry::new();
    reg.register(Arc::clone(&web_src) as Arc<dyn Source>)
        .unwrap();
    reg.register(Arc::clone(&crates_src) as Arc<dyn Source>)
        .unwrap();
    let reg = Arc::new(reg);

    let deps = SourceDeps::new(net, Arc::new(fsm_core::NullStore::new()), Arc::clone(&cfg));
    let search: Arc<dyn SearchApi> = Arc::new(SearchService::new(Arc::clone(&reg), deps.clone()));
    let doc: Arc<dyn DocApi> = Arc::new(NoopDoc);
    let fetch: Arc<dyn FetchApi> = Arc::new(FetchService::new(
        deps,
        Arc::clone(&doc),
        Arc::clone(&search),
    ));
    let ask = AskService::new(search, fetch, doc, reg, cfg);

    let resp = ask
        .ask(
            "latest serde version on crates.io",
            AskOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert_eq!(resp.mode, "direct");
    assert_eq!(crates_src.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        web_src.calls.load(Ordering::SeqCst),
        0,
        "direct claimant must bypass web search"
    );
    assert!(resp.passages[0].text.contains("1.0.229"));
    assert_eq!(resp.passages[0].source.as_deref(), Some("crates"));
}

#[tokio::test]
async fn test_fast_ask_timeout_returns_partial_results_within_deadline() {
    struct StaticSearch(Vec<Hit>);
    #[async_trait]
    impl SearchApi for StaticSearch {
        async fn search(&self, req: SearchRequest, _: &CallCx) -> Result<SearchResponse, FsmError> {
            let mut r = SearchResponse::new(req.query, req.scope);
            r.results = self.0.clone();
            r.engines = vec!["mock".into()];
            Ok(r)
        }
    }

    struct MixedSpeedFetch;
    #[async_trait]
    impl FetchApi for MixedSpeedFetch {
        async fn fetch(
            &self,
            url: &str,
            _: FetchOpts,
            _: &CallCx,
        ) -> Result<FetchOutcome, FsmError> {
            if url.contains("fast") {
                let mut p = PageResult::new(
                    url,
                    "Fast Fact Page",
                    "The default HTTP port for free-search-mcp is 8000.",
                    FetchMethod::Http,
                );
                p.published_date = Some("2026-10-01".into());
                Ok(FetchOutcome::Page(p))
            } else {
                // Deliberately exceed the hard deadline.
                tokio::time::sleep(Duration::from_secs(5)).await;
                Ok(FetchOutcome::Page(PageResult::new(
                    url,
                    "Slow",
                    "Too late",
                    FetchMethod::Http,
                )))
            }
        }
        async fn fetch_batch(
            &self,
            _: Vec<String>,
            _: FetchOpts,
            _: u8,
            _: &CallCx,
        ) -> Result<Vec<PageResult>, FsmError> {
            Ok(vec![])
        }
        async fn request(
            &self,
            o: fsm_core::RawRequestOpts,
            _: &CallCx,
        ) -> Result<fsm_core::HttpResponse, FsmError> {
            Ok(fsm_core::HttpResponse::new(o.url, 200, ""))
        }
        async fn structured(
            &self,
            u: &str,
            _: &CallCx,
        ) -> Result<fsm_core::StructuredMeta, FsmError> {
            Ok(fsm_core::StructuredMeta::new(u))
        }
        async fn research(
            &self,
            q: &str,
            _: fsm_core::ResearchOpts,
            _: &CallCx,
        ) -> Result<fsm_core::ResearchBrief, FsmError> {
            Ok(fsm_core::ResearchBrief::new(q, SearchResponse::default()))
        }
        async fn compare(
            &self,
            _: &str,
            _: Vec<String>,
            _: &CallCx,
        ) -> Result<Vec<fsm_core::CompareItem>, FsmError> {
            Ok(vec![])
        }
        async fn download(
            &self,
            u: &str,
            _: &CallCx,
        ) -> Result<fsm_core::DownloadResult, FsmError> {
            Ok(fsm_core::DownloadResult::new(u, "", "", 0, ""))
        }
        async fn html_to_md(&self, html: &str) -> Result<String, FsmError> {
            Ok(html.to_owned())
        }
    }

    let mut h1 = Hit::new("Fast Page", "https://example.org/fast");
    h1.snippet = "Default port is 8000.".into();
    let mut h2 = Hit::new("Slow Page 1", "https://example.org/slow1");
    h2.snippet = "Port configuration reference for port 8000.".into();
    let mut h3 = Hit::new("Slow Page 2", "https://example.org/slow2");
    h3.snippet = "Another slow page snippet.".into();

    let fast_cfg = FastConfig {
        hard_deadline: Duration::from_millis(500),
        direct_timeout: Duration::from_millis(200),
        search_timeout: Duration::from_millis(200),
        read_top_n: 3,
        max_passages: 4,
    };

    let ask = AskService::new(
        Arc::new(StaticSearch(vec![h1, h2, h3])),
        Arc::new(MixedSpeedFetch),
        Arc::new(NoopDoc),
        Arc::new(Registry::new()),
        Arc::new(Config::defaults_without_home()),
    )
    .with_fast_config(fast_cfg);

    let t0 = Instant::now();
    let resp = ask
        .ask(
            "What is the default HTTP port for free-search-mcp?",
            AskOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "must respect hard deadline"
    );
    assert!(resp.partial, "expected partial=true on fetch timeout");
    assert!(resp.passages.iter().any(|p| p.text.contains("8000")));
    assert!(
        resp.notes
            .iter()
            .any(|n| n.to_lowercase().contains("timed out"))
    );
}

#[tokio::test]
async fn test_fast_ask_dedups_urls_unescapes_markdown_and_prefers_authoritative_entity_match() {
    // Regression for Issue 3:
    // 1. "When was Rust 1.0 released?" must rank official/corroborated programming-language
    //    sources (May 15, 2015 on blog.rust-lang.org / wikipedia.org) above a survival-game
    //    blog (rustly.com), never repeat the same URL twice, and never emit `\&` artifacts.
    let tmp = TempDir::new().unwrap();
    let mut cfg = Config::defaults_without_home();
    cfg.cache_dir = tmp.path().join("cache");
    cfg.config_dir = tmp.path().join("config");
    cfg.default_engines = vec!["web_a".into(), "web_b".into()];
    cfg.reserve_engines = vec![];
    cfg.min_healthy_engines = 1;
    let cfg = Arc::new(cfg);

    let url_game = "https://rustly.com/rust-release-date";
    let url_blog = "https://blog.rust-lang.org/2015/05/15/Rust-1.0.html";
    let url_wiki = "https://en.wikipedia.org/wiki/Rust_(programming_language)";

    let mut fx = Fixture::new("ask_fast", "rust_1_0", "2026-11-25T10:00:00Z");
    fx.exchanges.push(html_exchange(
        url_game,
        r#"<!doctype html><html><head><title>Rust Release Date &amp; Platforms</title></head><body>
        <article>
        <p>Rust Release Date &amp; Full Launch History. Rust was released in Early Access on December 11, 2013, and its full 1.0 release launched on February 8, 2018.</p>
        <p>Rust was released in Early Access on December 11, 2013, and its full 1.0 release launched on February 8, 2018 for PC &amp; Mac.</p>
        </article></body></html>"#,
    ));
    fx.exchanges.push(html_exchange(
        url_blog,
        r#"<!doctype html><html><head><title>Announcing Rust 1.0</title>
        <meta property="article:published_time" content="2015-05-15"/></head><body>
        <article>
        <p>Today we are very proud to announce Rust 1.0, released on May 15, 2015, after years of community development &amp; stability guarantees.</p>
        </article></body></html>"#,
    ));
    fx.exchanges.push(html_exchange(
        url_wiki,
        r#"<!doctype html><html><head><title>Rust (programming language) - Wikipedia</title></head><body>
        <article>
        <p>Rust 1.0, the first stable release of the Rust programming language, was released on May 15, 2015.</p>
        </article></body></html>"#,
    ));

    let net: Arc<dyn NetClient> = Arc::new(ReplayNet::from_fixtures(vec![fx]).unwrap());
    let store = Arc::new(SqliteStore::open_path(tmp.path().join("ask2.sqlite")).unwrap());
    let store_api: Arc<dyn StoreApi> = store.clone();
    let deps = SourceDeps::new(Arc::clone(&net), Arc::clone(&store_api), Arc::clone(&cfg));

    let mut h_game = Hit::new("Rust Release Date & Platforms", url_game);
    h_game.snippet =
        "Rust was released on December 11, 2013 & full 1.0 on February 8, 2018.".into();
    let mut h_blog = Hit::new("Announcing Rust 1.0", url_blog);
    h_blog.snippet = "Rust 1.0 was released on May 15, 2015.".into();
    h_blog.date = Some("2015-05-15".into());
    let mut h_wiki = Hit::new("Rust (programming language)", url_wiki);
    h_wiki.snippet = "Rust 1.0 was released on May 15, 2015.".into();

    let mut reg = Registry::new();
    reg.register(Arc::new(FixtureWebSource::new(
        "web_a",
        vec![h_game.clone(), h_blog.clone(), h_wiki.clone()],
    )))
    .unwrap();
    reg.register(Arc::new(FixtureWebSource::new(
        "web_b",
        vec![h_game, h_blog, h_wiki],
    )))
    .unwrap();
    let reg = Arc::new(reg);

    let search: Arc<dyn SearchApi> = Arc::new(SearchService::new(Arc::clone(&reg), deps.clone()));
    let doc: Arc<dyn DocApi> = Arc::new(NoopDoc);
    let fetch: Arc<dyn FetchApi> = Arc::new(FetchService::new(
        deps,
        Arc::clone(&doc),
        Arc::clone(&search),
    ));
    let ask = AskService::new(search, fetch, doc, reg, cfg);

    let mut opts = AskOpts::default();
    opts.mode = AskMode::Fast;
    let resp = ask
        .ask("When was Rust 1.0 released?", opts, &CallCx::default())
        .await
        .unwrap();

    assert!(!resp.passages.is_empty());
    assert!(
        resp.passages[0].url.contains("rust-lang.org")
            || resp.passages[0].url.contains("wikipedia.org"),
        "expected authoritative Rust programming language source at #1, got: {:?}",
        resp.passages
    );
    assert!(
        resp.passages[0].text.contains("May 15, 2015"),
        "expected May 15, 2015 in #1 passage, got: {:?}",
        resp.passages[0]
    );
    // No duplicate URLs across passages.
    let mut urls = std::collections::HashSet::new();
    for p in &resp.passages {
        assert!(
            urls.insert(p.url.clone()),
            "duplicate passage URL: {}",
            p.url
        );
        assert!(
            !p.text.contains(r"\&"),
            "escaped ampersand artifact in quoted passage: {}",
            p.text
        );
    }
    store.close().await.unwrap();
}

#[tokio::test]
async fn test_direct_claimant_filters_out_unrelated_crates_like_asupersync() {
    // Regression for Issue 3:
    // "What is the latest stable version of the tokio crate?" must answer with `tokio` alone
    // and never include unrelated fuzzy search-tail hits like `asupersync`.
    struct MultiCratesSource(SourceDecl);
    #[async_trait]
    impl Source for MultiCratesSource {
        fn decl(&self) -> &SourceDecl {
            &self.0
        }
        fn claims(&self, query: &str, _: Option<&str>) -> f32 {
            if query.to_ascii_lowercase().contains("crate") {
                1.0
            } else {
                0.0
            }
        }
        async fn call(&self, _: &str, _: Value, _: &SrcCx) -> Result<SrcOutput, FsmError> {
            let mut h1 = Hit::new(
                "tokio 1.54.1 on crates.io",
                "https://crates.io/crates/tokio/1.54.1",
            );
            h1.snippet = "Current stable version 1.54.1 · An event-driven async runtime.".into();
            h1.source = "crates".into();
            let mut h2 = Hit::new(
                "asupersync 0.7.0 on crates.io",
                "https://crates.io/crates/asupersync/0.7.0",
            );
            h2.snippet = "Current stable version 0.7.0 · Alternative to tokio.".into();
            h2.source = "crates".into();
            let mut h3 = Hit::new(
                "dial9 0.5.9 on crates.io",
                "https://crates.io/crates/dial9/0.5.9",
            );
            h3.snippet = "Current stable version 0.5.9 · Telemetry for tokio.".into();
            h3.source = "crates".into();
            Ok(SrcOutput::Hits(vec![h1, h2, h3]))
        }
    }

    let mut decl = SourceDecl::new("crates", "crates.io", "Rust crate registry")
        .with_scopes(&["code"])
        .with_caps(&[Capability::Search]);
    decl.pool_role = PoolRole::DirectClaimant;
    let mut reg = Registry::new();
    reg.register(Arc::new(MultiCratesSource(decl))).unwrap();
    let reg = Arc::new(reg);

    let cfg = Arc::new(Config::defaults_without_home());
    let net: Arc<dyn NetClient> = Arc::new(ReplayNet::from_fixtures(vec![]).unwrap());
    let deps = SourceDeps::new(net, Arc::new(fsm_core::NullStore::new()), Arc::clone(&cfg));
    let search: Arc<dyn SearchApi> = Arc::new(SearchService::new(Arc::clone(&reg), deps.clone()));
    let doc: Arc<dyn DocApi> = Arc::new(NoopDoc);
    let fetch: Arc<dyn FetchApi> = Arc::new(FetchService::new(
        deps,
        Arc::clone(&doc),
        Arc::clone(&search),
    ));
    let ask = AskService::new(search, fetch, doc, reg, cfg);

    let resp = ask
        .ask(
            "What is the latest stable version of the tokio crate?",
            AskOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert_eq!(resp.mode, "direct");
    assert_eq!(
        resp.passages.len(),
        1,
        "expected only exact entity match, got: {:?}",
        resp.passages
    );
    assert!(resp.passages[0].url.contains("/crates/tokio/"));
    assert!(
        resp.passages
            .iter()
            .all(|p| !p.title.contains("asupersync") && !p.title.contains("dial9"))
    );
}

#[test]
fn test_fast_ask_default_budgets_cover_two_hop_direct_claimants_and_search_soft_deadline() {
    let cfg = FastConfig::default();
    assert!(
        cfg.direct_timeout >= Duration::from_millis(4500),
        "direct_timeout {:?} must accommodate 2-hop direct claimants like openmeteo",
        cfg.direct_timeout
    );
    assert!(
        cfg.search_timeout > Duration::from_millis(4000),
        "search_timeout {:?} must exceed search kernel 4.0s soft deadline",
        cfg.search_timeout
    );
    assert!(
        cfg.search_timeout + Duration::from_millis(3000) <= cfg.hard_deadline,
        "must leave >= 3s for parallel page fetching within hard_deadline"
    );
}

#[tokio::test]
async fn test_ambiguous_bare_package_version_prefers_active_crate_over_stale_pypi() {
    // Regression: `ask "What is the latest version of tokio?"` (no ecosystem cue)
    // must consult sibling registries (`crates`, `npm`) alongside `pypi` and prefer
    // the active `tokio 1.54.1 on crates.io` (2026-10-21) over stale `tokio 0.2.0 on PyPI` (2017-07-23).
    struct StubReg {
        decl: SourceDecl,
        claim_bare: bool,
        hit: Hit,
    }
    #[async_trait]
    impl Source for StubReg {
        fn decl(&self) -> &SourceDecl {
            &self.decl
        }
        fn claims(&self, _query: &str, _: Option<&str>) -> f32 {
            if self.claim_bare { 1.0 } else { 0.0 }
        }
        async fn call(&self, _: &str, _: Value, _: &SrcCx) -> Result<SrcOutput, FsmError> {
            Ok(SrcOutput::Hits(vec![self.hit.clone()]))
        }
    }

    let mut pypi_decl = SourceDecl::new("pypi", "PyPI", "PyPI registry")
        .with_scopes(&["code"])
        .with_caps(&[Capability::Search]);
    pypi_decl.pool_role = PoolRole::DirectClaimant;
    let mut pypi_hit = Hit::new(
        "tokio 0.2.0 on PyPI",
        "https://pypi.org/project/tokio/0.2.0/",
    );
    pypi_hit.snippet = "Latest release 0.2.0, uploaded 2017-07-23".into();
    pypi_hit.date = Some("2017-07-23".into());
    pypi_hit.source = "pypi".into();

    let mut crates_decl = SourceDecl::new("crates", "crates.io", "Rust crates")
        .with_scopes(&["code"])
        .with_caps(&[Capability::Search]);
    crates_decl.pool_role = PoolRole::DirectClaimant;
    let mut crates_hit = Hit::new(
        "tokio 1.54.1 on crates.io",
        "https://crates.io/crates/tokio/1.54.1",
    );
    crates_hit.snippet = "Current stable version 1.54.1, updated 2026-10-21".into();
    crates_hit.date = Some("2026-10-21".into());
    crates_hit.source = "crates".into();

    let mut reg = Registry::new();
    reg.register(Arc::new(StubReg {
        decl: pypi_decl,
        claim_bare: true,
        hit: pypi_hit,
    }))
    .unwrap();
    reg.register(Arc::new(StubReg {
        decl: crates_decl,
        claim_bare: false,
        hit: crates_hit,
    }))
    .unwrap();
    let reg = Arc::new(reg);

    let cfg = Arc::new(Config::defaults_without_home());
    let net: Arc<dyn NetClient> = Arc::new(ReplayNet::from_fixtures(vec![]).unwrap());
    let deps = SourceDeps::new(net, Arc::new(fsm_core::NullStore::new()), Arc::clone(&cfg));
    let search: Arc<dyn SearchApi> = Arc::new(SearchService::new(Arc::clone(&reg), deps.clone()));
    let doc: Arc<dyn DocApi> = Arc::new(NoopDoc);
    let fetch: Arc<dyn FetchApi> = Arc::new(FetchService::new(
        deps,
        Arc::clone(&doc),
        Arc::clone(&search),
    ));
    let ask = AskService::new(search, fetch, doc, reg, cfg);

    // 1. Bare ambiguous question -> prefers active crates.io hit and drops 2017 PyPI stub.
    let resp = ask
        .ask(
            "What is the latest version of tokio?",
            AskOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert_eq!(resp.mode, "direct");
    assert_eq!(
        resp.passages.len(),
        1,
        "expected only active crate hit, got: {:?}",
        resp.passages
    );
    assert!(
        resp.passages[0]
            .url
            .contains("crates.io/crates/tokio/1.54.1")
    );

    // 2. Explicit PyPI question -> respects PyPI without expanding to crates.io.
    let resp_py = ask
        .ask(
            "What is the latest version of tokio on PyPI?",
            AskOpts::default(),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert_eq!(resp_py.mode, "direct");
    assert_eq!(resp_py.passages.len(), 1);
    assert!(
        resp_py.passages[0]
            .url
            .contains("pypi.org/project/tokio/0.2.0/")
    );
}
