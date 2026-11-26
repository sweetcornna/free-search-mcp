//! Port of `tests/test_agent_lane.py`: agent-lane configuration, seating, cache-key
//! isolation, wait/grace semantics, stretched wait when keyless is empty, coherence
//! suspect settlement, lasting vs transient health strikes, and late background cache merge.

use async_trait::async_trait;
use fsm_core::ext::w7::{agent_lasting_error, in_agent_lane};
use fsm_core::fixture::ReplayNet;
use fsm_core::{
    Availability, CallCx, Capability, Config, FsmError, Hit, NetClient, PoolRole, Registry,
    SearchApi, SearchRequest, SearchResponse, Source, SourceDecl, SourceDeps, SrcCx, SrcOutput,
    StoreApi,
};
use fsm_search::SearchService;
use fsm_search::agent_lane::{
    LANE_REFUSED_THRESHOLD, LANE_SOFT_THRESHOLD, LANE_UNCONFIRMED_THRESHOLD,
};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type ResultFn = Arc<dyn Fn(&str) -> Result<Vec<Hit>, FsmError> + Send + Sync>;

struct FakeSource {
    decl: SourceDecl,
    available: Arc<AtomicBool>,
    delay: Duration,
    result_fn: ResultFn,
    calls: Arc<AtomicUsize>,
    saw_in_lane: Arc<AtomicBool>,
}

impl FakeSource {
    fn keyless(id: &str, delay: Duration, hits: Vec<Hit>) -> Arc<Self> {
        let decl = SourceDecl::new(id, id, "fake keyless")
            .with_scopes(&["web"])
            .with_caps(&[Capability::Search]);
        Arc::new(Self {
            decl,
            available: Arc::new(AtomicBool::new(true)),
            delay,
            result_fn: Arc::new(move |_| Ok(hits.clone())),
            calls: Arc::new(AtomicUsize::new(0)),
            saw_in_lane: Arc::new(AtomicBool::new(false)),
        })
    }

    fn agent(
        id: &str,
        signed_in: bool,
        delay: Duration,
        result_fn: impl Fn(&str) -> Result<Vec<Hit>, FsmError> + Send + Sync + 'static,
    ) -> Arc<Self> {
        let mut decl = SourceDecl::new(id, id, "fake agent")
            .with_scopes(&["web"])
            .with_caps(&[Capability::Search]);
        decl.pool_role = PoolRole::Explicit;
        Arc::new(Self {
            decl,
            available: Arc::new(AtomicBool::new(signed_in)),
            delay,
            result_fn: Arc::new(result_fn),
            calls: Arc::new(AtomicUsize::new(0)),
            saw_in_lane: Arc::new(AtomicBool::new(false)),
        })
    }
}

#[async_trait]
impl Source for FakeSource {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }
    fn availability(&self, _cfg: &Config) -> Availability {
        if self.available.load(Ordering::SeqCst) {
            Availability::Available
        } else {
            Availability::Disabled("not signed in".into())
        }
    }
    async fn call(&self, _op: &str, params: Value, _cx: &SrcCx) -> Result<SrcOutput, FsmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if in_agent_lane() {
            self.saw_in_lane.store(true, Ordering::SeqCst);
        }
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        let q = params.get("query").and_then(Value::as_str).unwrap_or("");
        Ok(SrcOutput::Hits((self.result_fn)(q)?))
    }
}

/// Minimal in-memory `StoreApi` that records `search_cache_put` rows and TTLs for exact inspection.
#[derive(Default)]
struct MemCacheStore {
    rows: Mutex<HashMap<String, (SearchResponse, u64, usize)>>,
}

impl MemCacheStore {
    fn get_row(&self, key: &str) -> Option<(SearchResponse, u64, usize)> {
        self.rows.lock().unwrap().get(key).cloned()
    }
}

#[async_trait]
impl StoreApi for MemCacheStore {
    async fn cache_search(&self, _: &str, _: u32) -> Result<Vec<fsm_core::CacheHit>, FsmError> {
        Ok(vec![])
    }
    async fn get_page(
        &self,
        _: &str,
        _: Option<u64>,
    ) -> Result<Option<fsm_core::PageResult>, FsmError> {
        Ok(None)
    }
    async fn put_page(&self, _: &fsm_core::PageResult, _: u64) -> Result<(), FsmError> {
        Ok(())
    }
    async fn kv_put(&self, _: &str, _: &str, _: &Value, _: u64) -> Result<(), FsmError> {
        Ok(())
    }
    async fn kv_get(&self, _: &str, _: &str) -> Result<Option<Value>, FsmError> {
        Ok(None)
    }
    async fn search_cache_get(
        &self,
        key: &str,
        _: Option<u64>,
    ) -> Result<Option<(SearchResponse, u64)>, FsmError> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .get(key)
            .map(|(r, _, _)| (r.clone(), 0)))
    }
    async fn search_cache_put(
        &self,
        key: &str,
        resp: &SearchResponse,
        ttl: u64,
    ) -> Result<(), FsmError> {
        let mut guard = self.rows.lock().unwrap();
        let writes = guard.get(key).map_or(0, |x| x.2) + 1;
        guard.insert(key.to_owned(), (resp.clone(), ttl, writes));
        Ok(())
    }
    async fn api_cache_get(
        &self,
        _: &str,
    ) -> Result<Option<(fsm_core::Bytes, Option<String>, Option<String>)>, FsmError> {
        Ok(None)
    }
    async fn api_cache_put(
        &self,
        _: &str,
        _: &str,
        _: &[u8],
        _: Option<&str>,
        _: Option<&str>,
        _: u64,
    ) -> Result<(), FsmError> {
        Ok(())
    }
    async fn scholar_get(&self, _: &str) -> Result<Option<fsm_core::ScholarlyWork>, FsmError> {
        Ok(None)
    }
    async fn scholar_put(
        &self,
        _: &str,
        _: &fsm_core::ScholarlyWork,
        _: u64,
    ) -> Result<(), FsmError> {
        Ok(())
    }
    async fn retraction_lookup(
        &self,
        _: &str,
    ) -> Result<Option<fsm_core::RetractionNotice>, FsmError> {
        Ok(None)
    }
    async fn retractions_replace_batch(
        &self,
        _: Vec<(String, fsm_core::RetractionNotice)>,
        _: Option<String>,
    ) -> Result<(), FsmError> {
        Ok(())
    }
    async fn sync_state_get(
        &self,
        _: &str,
    ) -> Result<Option<(Option<String>, i64, Option<i64>)>, FsmError> {
        Ok(None)
    }
    async fn sync_state_mark(
        &self,
        _: &str,
        _: Option<&str>,
        _: i64,
        _: Option<i64>,
    ) -> Result<(), FsmError> {
        Ok(())
    }
    async fn health_load_all(&self) -> Result<Vec<fsm_core::HealthRecord>, FsmError> {
        Ok(vec![])
    }
    async fn health_save(&self, _: &fsm_core::HealthRecord) -> Result<(), FsmError> {
        Ok(())
    }
    async fn download_record(&self, _: &fsm_core::DownloadResult, _: u64) -> Result<(), FsmError> {
        Ok(())
    }
}

fn hit(title: &str, url: &str, snippet: &str) -> Hit {
    let mut h = Hit::new(title, url);
    h.snippet = snippet.into();
    h
}

fn base_cfg() -> Config {
    let mut cfg = Config::defaults_without_home();
    cfg.default_engines = vec!["eng_a".into(), "eng_b".into()];
    cfg.reserve_engines = vec![];
    cfg.min_healthy_engines = 1;
    cfg.search_deadline_s = 2.0;
    cfg.agent_lane_wait_seconds = 1.0;
    cfg.agent_lane_grace_seconds = 0.2;
    cfg
}

fn make_svc(
    cfg: Config,
    sources: Vec<Arc<FakeSource>>,
    store: Arc<MemCacheStore>,
) -> SearchService {
    let mut reg = Registry::new();
    for s in sources {
        reg.register(s as Arc<dyn Source>).unwrap();
    }
    let net: Arc<dyn NetClient> = Arc::new(ReplayNet::from_fixtures(vec![]).unwrap());
    let deps = SourceDeps::new(net, store as Arc<dyn StoreApi>, Arc::new(cfg));
    SearchService::new(Arc::new(reg), deps)
}

#[tokio::test]
async fn test_auto_seats_only_codex_and_isolates_cache_keys() {
    let store = Arc::new(MemCacheStore::default());
    let a = FakeSource::keyless(
        "eng_a",
        Duration::ZERO,
        vec![hit("Rust Tokio", "https://tokio.rs", "Rust tokio async")],
    );
    let b = FakeSource::keyless(
        "eng_b",
        Duration::ZERO,
        vec![hit(
            "Rust Async",
            "https://rust-lang.org",
            "Rust async book",
        )],
    );
    let codex = FakeSource::agent("codex", true, Duration::from_millis(10), |_| {
        Ok(vec![hit(
            "Codex Rust Hit",
            "https://codex.example/rust",
            "Rust tokio guide",
        )])
    });
    let ag = FakeSource::agent("antigravity", true, Duration::from_millis(10), |_| {
        Ok(vec![hit(
            "AG Hit",
            "https://ag.example/rust",
            "Rust tokio guide",
        )])
    });

    let cfg = base_cfg(); // agent_engines = "auto"
    let svc = make_svc(
        cfg.clone(),
        vec![a.clone(), b.clone(), codex.clone(), ag.clone()],
        store.clone(),
    );
    assert_eq!(svc.agent_lane_now(), vec!["codex"]);

    let resp = svc
        .search(SearchRequest::new("rust tokio"), &CallCx::default())
        .await
        .unwrap();
    assert!(
        codex.saw_in_lane.load(Ordering::SeqCst),
        "codex must run inside scope_agent_lane"
    );
    assert_eq!(codex.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        ag.calls.load(Ordering::SeqCst),
        0,
        "antigravity must be opt-in only"
    );
    assert!(
        resp.results
            .iter()
            .any(|h| h.url == "https://codex.example/rust")
    );
    let auto_key = resp.cache_key.clone().unwrap();

    // Signing codex out changes the cache key (never replays the codex-inclusive row).
    codex.available.store(false, Ordering::SeqCst);
    let resp_out = svc
        .search(SearchRequest::new("rust tokio"), &CallCx::default())
        .await
        .unwrap();
    assert_ne!(resp_out.cache_key.as_deref(), Some(auto_key.as_str()));

    // Explicit sources=["codex"] also has a distinct cache key!
    codex.available.store(true, Ordering::SeqCst);
    let mut exp_req = SearchRequest::new("rust tokio");
    exp_req.sources = Some(vec!["codex".into()]);
    let resp_exp = svc.search(exp_req, &CallCx::default()).await.unwrap();
    assert_ne!(resp_exp.cache_key.as_deref(), Some(auto_key.as_str()));
}

#[tokio::test]
async fn test_grace_returns_early_and_late_lane_merges_into_cache_respecting_row_ownership() {
    let store = Arc::new(MemCacheStore::default());
    let a = FakeSource::keyless(
        "eng_a",
        Duration::from_millis(10),
        vec![hit("Rust A", "https://a.example/rust", "Rust guide")],
    );
    let b = FakeSource::keyless(
        "eng_b",
        Duration::from_millis(10),
        vec![hit("Rust B", "https://b.example/rust", "Rust docs")],
    );
    // Slow codex takes 350ms while grace is 80ms -> caller returns fast, late merge updates cache!
    let codex = FakeSource::agent("codex", true, Duration::from_millis(350), |_| {
        Ok(vec![hit(
            "Late Codex Rust",
            "https://late.example/rust",
            "Rust late witness",
        )])
    });

    let mut cfg = base_cfg();
    cfg.agent_lane_wait_seconds = 1.0;
    cfg.agent_lane_grace_seconds = 0.08;
    let svc = make_svc(cfg, vec![a, b, codex], store.clone());

    let t0 = Instant::now();
    let resp = svc
        .search(SearchRequest::new("rust guide"), &CallCx::default())
        .await
        .unwrap();
    assert!(
        t0.elapsed() < Duration::from_millis(300),
        "must return after grace, took {:?}",
        t0.elapsed()
    );
    let lane = resp.agent_lane.as_ref().unwrap();
    assert_eq!(lane.pending, vec!["codex"]);
    let key = resp.cache_key.clone().unwrap();

    // Initial cached TTL is capped to 1h (DEGRADED_TTL_S = 3600) while lane is pending.
    let (_, initial_ttl, writes_1) = store.get_row(&key).unwrap();
    assert_eq!(initial_ttl, 3600);
    assert_eq!(writes_1, 1);

    // Wait for late merge to land in cache.
    tokio::time::sleep(Duration::from_millis(450)).await;
    let (updated, final_ttl, writes_2) = store.get_row(&key).unwrap();
    assert_eq!(writes_2, 2);
    assert!(
        final_ttl > 3600,
        "full TTL restored once late lane settles: {final_ttl}"
    );
    assert!(
        updated
            .results
            .iter()
            .any(|h| h.url == "https://late.example/rust")
    );
    assert!(updated.agent_lane.as_ref().unwrap().pending.is_empty());
}

#[tokio::test]
async fn test_stretched_wait_when_keyless_empty_and_doubted_suspect_settlement() {
    let store = Arc::new(MemCacheStore::default());
    // Keyless returns nothing -> wait stretches past lane_wait (0.15s) up to search_deadline (0.8s)!
    let a = FakeSource::keyless("eng_a", Duration::from_millis(10), vec![]);
    let b = FakeSource::keyless("eng_b", Duration::from_millis(10), vec![]);
    let codex = FakeSource::agent("codex", true, Duration::from_millis(260), |_| {
        Ok(vec![hit(
            "Rust Stretched Answer",
            "https://stretched.example/rust",
            "Rust programming language",
        )])
    });

    let mut cfg = base_cfg();
    cfg.agent_lane_wait_seconds = 0.12;
    cfg.agent_lane_grace_seconds = 0.05;
    cfg.search_deadline_s = 0.8;
    let svc = make_svc(cfg, vec![a, b, codex], store.clone());

    let resp = svc
        .search(SearchRequest::new("rust programming"), &CallCx::default())
        .await
        .unwrap();
    assert!(
        resp.results
            .iter()
            .any(|h| h.url == "https://stretched.example/rust"),
        "empty keyless must stretch wait for lane answer"
    );

    // Suspect/off-topic lane bucket: dropped when keyless witness exists (and strikes use LANE_UNCONFIRMED_THRESHOLD = 3).
    assert_eq!(LANE_UNCONFIRMED_THRESHOLD, 3);
    assert_eq!(LANE_SOFT_THRESHOLD, 2);
    assert_eq!(LANE_REFUSED_THRESHOLD, 1);

    let store2 = Arc::new(MemCacheStore::default());
    let w1 = FakeSource::keyless(
        "eng_a",
        Duration::ZERO,
        vec![
            hit(
                "Rust Tokio Async Runtime Guide",
                "https://tokio.rs/guide",
                "Rust tokio async runtime tutorial guide",
            ),
            hit(
                "Rust Tokio Async Runtime Tutorial",
                "https://tokio.rs/tut",
                "Rust tokio async runtime tutorial",
            ),
            hit(
                "Tokio Async Runtime Spawning Tasks",
                "https://tokio.rs/spawning",
                "Rust tokio async runtime tutorial tasks",
            ),
            hit(
                "Tokio Async Runtime Channels",
                "https://tokio.rs/channels",
                "Rust tokio async runtime tutorial channels",
            ),
        ],
    );
    let w2 = FakeSource::keyless(
        "eng_b",
        Duration::ZERO,
        vec![hit(
            "Rust Tokio Docs",
            "https://docs.rs/tokio",
            "Rust tokio async runtime documentation",
        )],
    );
    let decoy_codex = FakeSource::agent("codex", true, Duration::ZERO, |_| {
        Ok(vec![
            hit(
                "Completely unrelated banana bread recipe",
                "https://food.example/1",
                "baking flour sugar",
            ),
            hit(
                "Another cooking video",
                "https://food.example/2",
                "oven temperature",
            ),
            hit(
                "Chocolate cake",
                "https://food.example/3",
                "butter frosting",
            ),
            hit(
                "Sourdough starter guide",
                "https://food.example/4",
                "yeast fermentation water",
            ),
        ])
    });
    let svc2 = make_svc(base_cfg(), vec![w1, w2, decoy_codex], store2);
    let resp2 = svc2
        .search(
            SearchRequest::new("rust tokio async runtime tutorial"),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(
        !resp2.results.iter().any(|h| h.url.contains("food.example")),
        "off-topic lane bucket must be dropped"
    );
    // Must not appear in keyless gated notes.
    assert!(!resp2.notes.iter().any(|n| n.contains("off_topic")));
}

#[tokio::test]
async fn test_lasting_failure_benches_in_one_strike_without_capping_cache_ttl() {
    let store = Arc::new(MemCacheStore::default());
    let a = FakeSource::keyless(
        "eng_a",
        Duration::ZERO,
        vec![hit("Rust A", "https://a.example/rust", "Rust")],
    );
    let b = FakeSource::keyless(
        "eng_b",
        Duration::ZERO,
        vec![hit("Rust B", "https://b.example/rust", "Rust")],
    );
    let refused_codex = FakeSource::agent("codex", true, Duration::ZERO, |_| {
        Err(agent_lasting_error(
            "codex",
            Some(429),
            "usage limit reached",
        ))
    });

    let svc = make_svc(base_cfg(), vec![a, b, refused_codex], store.clone());
    let resp = svc
        .search(SearchRequest::new("rust"), &CallCx::default())
        .await
        .unwrap();
    let key = resp.cache_key.clone().unwrap();

    // 1. Benched immediately on 1 strike as lane_refused!
    let bench = svc
        .benched("codex")
        .expect("codex should be benched on 1 lasting strike");
    assert_eq!(bench.reason, "lane_refused");

    // 2. Lasting refusal must NOT shorten cache TTL to 1h!
    let (_, ttl, _) = store.get_row(&key).unwrap();
    assert_eq!(
        ttl,
        6 * 3600,
        "lasting refusal must keep full 6h cache TTL, got {ttl}"
    );
}

#[tokio::test]
async fn test_settled_empty_lane_engine_appears_in_resp_engines() {
    let store = Arc::new(MemCacheStore::default());
    let a = FakeSource::keyless(
        "eng_a",
        Duration::ZERO,
        vec![hit("Rust A", "https://a.example/rust", "Rust")],
    );
    let b = FakeSource::keyless(
        "eng_b",
        Duration::ZERO,
        vec![hit("Rust B", "https://b.example/rust", "Rust")],
    );
    let empty_codex = FakeSource::agent("codex", true, Duration::from_millis(5), |_| Ok(vec![]));

    let svc = make_svc(base_cfg(), vec![a, b, empty_codex], store);
    let resp = svc
        .search(SearchRequest::new("rust"), &CallCx::default())
        .await
        .unwrap();
    assert!(
        resp.engines.iter().any(|e| e == "codex"),
        "settled agent-lane engine must appear in resp.engines even when empty: {:?}",
        resp.engines
    );
}
