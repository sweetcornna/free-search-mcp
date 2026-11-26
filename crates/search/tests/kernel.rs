//! Kernel behaviour with in-test fake sources on a paused clock: pool
//! selection, M16 claims routing, quorum/grace, hedging, deadlines, the
//! breaker, the coherence guard, filters and the cache. Python originals are
//! named in each test's comment (`tests/test_*.py::test_*`).

use async_trait::async_trait;
use fsm_core::ext::w1a::wall_error;
use fsm_core::fixture::ReplayNet;
use fsm_core::{
    Availability, CacheHit, CallCx, CancellationToken, Config, DownloadResult, Freshness, FsmError,
    HealthRecord, Hit, NullStore, OpDecl, PageResult, Param, ParamType, PoolRole, Registry,
    RetractionNotice, ReturnKind, ScholarlyWork, SearchApi, SearchRequest, SearchResponse, Source,
    SourceDecl, SourceDeps, SourceLimits, SrcCx, SrcOutput, StoreApi, TransportProfile,
};
use fsm_search::{SearchService, plan};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;

// ---------------------------------------------------------------- fakes ---

#[derive(Clone)]
enum Reply {
    Hits(Vec<Hit>),
    Fail(fn(&str) -> FsmError),
    Panic(&'static str),
    Hang,
}

/// What a call saw.
#[derive(Debug, Clone)]
struct Seen {
    at: Duration,
    params: Value,
    lang: Option<String>,
    freshness: Option<Freshness>,
    domains: Vec<String>,
    timeout: Duration,
}

struct Fake {
    decl: SourceDecl,
    delay: Mutex<Duration>,
    reply: Mutex<Reply>,
    claim: f32,
    avail: Availability,
    panic_avail: bool,
    calls: AtomicUsize,
    dropped: Arc<AtomicUsize>,
    cancelled_at_drop: Arc<AtomicUsize>,
    seen: Mutex<Vec<Seen>>,
    t0: Instant,
}

struct DropFlag {
    dropped: Arc<AtomicUsize>,
    cancelled_at_drop: Arc<AtomicUsize>,
    token: CancellationToken,
    done: bool,
}

impl Drop for DropFlag {
    fn drop(&mut self) {
        if !self.done {
            self.dropped.fetch_add(1, Ordering::SeqCst);
            if self.token.is_cancelled() {
                self.cancelled_at_drop.fetch_add(1, Ordering::SeqCst);
            }
        }
    }
}

impl Fake {
    fn new(decl: SourceDecl, delay_ms: u64, reply: Reply) -> Arc<Self> {
        Arc::new(Self {
            decl,
            delay: Mutex::new(Duration::from_millis(delay_ms)),
            reply: Mutex::new(reply),
            claim: 0.0,
            avail: Availability::Available,
            panic_avail: false,
            calls: AtomicUsize::new(0),
            dropped: Arc::new(AtomicUsize::new(0)),
            cancelled_at_drop: Arc::new(AtomicUsize::new(0)),
            seen: Mutex::new(Vec::new()),
            t0: Instant::now(),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn dropped(&self) -> usize {
        self.dropped.load(Ordering::SeqCst)
    }

    fn cancelled_at_drop(&self) -> usize {
        self.cancelled_at_drop.load(Ordering::SeqCst)
    }

    fn set(&self, delay_ms: u64, reply: Reply) {
        *self.delay.lock().unwrap() = Duration::from_millis(delay_ms);
        *self.reply.lock().unwrap() = reply;
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

#[async_trait]
impl Source for Fake {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    fn claims(&self, _query: &str, _lang: Option<&str>) -> f32 {
        self.claim
    }

    fn availability(&self, _cfg: &Config) -> Availability {
        if self.panic_avail {
            panic!("availability boom");
        }
        self.avail.clone()
    }

    async fn call(&self, op: &str, params: Value, cx: &SrcCx) -> Result<SrcOutput, FsmError> {
        assert_eq!(op, "search");
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(Seen {
            at: Instant::now() - self.t0,
            params,
            lang: cx.lang.clone(),
            freshness: cx.freshness,
            domains: cx.domains.clone(),
            timeout: cx.timeout,
        });
        let mut flag = DropFlag {
            dropped: Arc::clone(&self.dropped),
            cancelled_at_drop: Arc::clone(&self.cancelled_at_drop),
            token: cx.cancel.clone(),
            done: false,
        };
        let delay = *self.delay.lock().unwrap();
        let reply = self.reply.lock().unwrap().clone();
        tokio::time::sleep(delay).await;
        let out = match reply {
            Reply::Hits(h) => Ok(SrcOutput::Hits(h)),
            Reply::Fail(f) => Err(f(&self.decl.id)),
            Reply::Panic(msg) => panic!("{msg}"),
            Reply::Hang => std::future::pending().await,
        };
        flag.done = true;
        out
    }
}

fn decl(id: &str, scopes: &[&str]) -> SourceDecl {
    SourceDecl::new(id, id, "fake").with_scopes(scopes)
}

/// A general web engine.
fn web(id: &str, delay_ms: u64, reply: Reply) -> Arc<Fake> {
    Fake::new(decl(id, &["web"]), delay_ms, reply)
}

/// A web engine whose own timeout is long enough for the run's deadlines to bite.
fn slow_web(id: &str, delay_ms: u64, reply: Reply) -> Arc<Fake> {
    let mut d = decl(id, &["web"]);
    d.limits = SourceLimits::new(1.0, 1, 1, 20_000, 0);
    Fake::new(d, delay_ms, reply)
}

fn with(mut f: Arc<Fake>, edit: impl FnOnce(&mut Fake)) -> Arc<Fake> {
    edit(Arc::get_mut(&mut f).expect("unshared"));
    f
}

fn hit(title: &str, url: &str, snippet: &str) -> Hit {
    let mut h = Hit::new(title, url);
    h.snippet = snippet.to_owned();
    h
}

/// `n` distinct results on host `p.example`.
fn hits(p: &str, n: usize) -> Reply {
    Reply::Hits(
        (0..n)
            .map(|i| {
                hit(
                    &format!("{p} page {i} on its own topic"),
                    &format!("https://{p}.example/{i}"),
                    "s",
                )
            })
            .collect(),
    )
}

fn walled(id: &str) -> FsmError {
    wall_error(id, "captcha", Some(200))
}

fn rate_limited(id: &str) -> FsmError {
    FsmError::RateLimited {
        source_id: id.to_owned(),
        retry_after_s: Some(30),
    }
}

fn needs_key(id: &str) -> FsmError {
    FsmError::AuthRequired {
        source_id: id.to_owned(),
        env_var: "FAKE_API_KEY".to_owned(),
    }
}

/// Search cache + health rows in memory; everything else is `NullStore`.
#[derive(Default)]
struct MemStore {
    null: NullStore,
    rows: Mutex<HashMap<String, (SearchResponse, Instant, u64)>>,
    puts: Mutex<Vec<(String, u64)>>,
    health: Mutex<Vec<HealthRecord>>,
    pages: Vec<CacheHit>,
}

#[async_trait]
impl StoreApi for MemStore {
    async fn cache_search(&self, _q: &str, limit: u32) -> Result<Vec<CacheHit>, FsmError> {
        Ok(self.pages.iter().take(limit as usize).cloned().collect())
    }
    async fn get_page(
        &self,
        url: &str,
        max_age_s: Option<u64>,
    ) -> Result<Option<PageResult>, FsmError> {
        self.null.get_page(url, max_age_s).await
    }
    async fn put_page(&self, page: &PageResult, ttl_s: u64) -> Result<(), FsmError> {
        self.null.put_page(page, ttl_s).await
    }
    async fn kv_put(&self, s: &str, k: &str, v: &Value, ttl: u64) -> Result<(), FsmError> {
        self.null.kv_put(s, k, v, ttl).await
    }
    async fn kv_get(&self, s: &str, k: &str) -> Result<Option<Value>, FsmError> {
        self.null.kv_get(s, k).await
    }
    async fn search_cache_get(
        &self,
        key: &str,
        max_age_s: Option<u64>,
    ) -> Result<Option<(SearchResponse, u64)>, FsmError> {
        let rows = self.rows.lock().unwrap();
        Ok(rows.get(key).and_then(|(resp, at, ttl)| {
            let age = at.elapsed().as_secs();
            (age < *ttl && max_age_s.is_none_or(|m| age <= m)).then(|| (resp.clone(), age))
        }))
    }
    async fn search_cache_put(
        &self,
        key: &str,
        resp: &SearchResponse,
        ttl_s: u64,
    ) -> Result<(), FsmError> {
        self.puts.lock().unwrap().push((key.to_owned(), ttl_s));
        self.rows
            .lock()
            .unwrap()
            .insert(key.to_owned(), (resp.clone(), Instant::now(), ttl_s));
        Ok(())
    }
    async fn api_cache_get(
        &self,
        key: &str,
    ) -> Result<Option<(bytes::Bytes, Option<String>, Option<String>)>, FsmError> {
        self.null.api_cache_get(key).await
    }
    async fn api_cache_put(
        &self,
        key: &str,
        source_id: &str,
        body: &[u8],
        etag: Option<&str>,
        last_mod: Option<&str>,
        ttl_s: u64,
    ) -> Result<(), FsmError> {
        self.null
            .api_cache_put(key, source_id, body, etag, last_mod, ttl_s)
            .await
    }
    async fn scholar_get(&self, k: &str) -> Result<Option<ScholarlyWork>, FsmError> {
        self.null.scholar_get(k).await
    }
    async fn scholar_put(&self, k: &str, w: &ScholarlyWork, ttl: u64) -> Result<(), FsmError> {
        self.null.scholar_put(k, w, ttl).await
    }
    async fn retraction_lookup(&self, doi: &str) -> Result<Option<RetractionNotice>, FsmError> {
        self.null.retraction_lookup(doi).await
    }
    async fn retractions_replace_batch(
        &self,
        rows: Vec<(String, RetractionNotice)>,
        etag: Option<String>,
    ) -> Result<(), FsmError> {
        self.null.retractions_replace_batch(rows, etag).await
    }
    async fn sync_state_get(
        &self,
        n: &str,
    ) -> Result<Option<(Option<String>, i64, Option<i64>)>, FsmError> {
        self.null.sync_state_get(n).await
    }
    async fn sync_state_mark(
        &self,
        n: &str,
        etag: Option<&str>,
        started_at: i64,
        finished_at: Option<i64>,
    ) -> Result<(), FsmError> {
        self.null
            .sync_state_mark(n, etag, started_at, finished_at)
            .await
    }
    async fn health_load_all(&self) -> Result<Vec<HealthRecord>, FsmError> {
        Ok(self.health.lock().unwrap().clone())
    }
    async fn health_save(&self, rec: &HealthRecord) -> Result<(), FsmError> {
        self.health.lock().unwrap().push(rec.clone());
        Ok(())
    }
    async fn download_record(&self, rec: &DownloadResult, ttl_s: u64) -> Result<(), FsmError> {
        self.null.download_record(rec, ttl_s).await
    }
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_owned()).collect()
}

/// e1..e4 default pool, r1..r3 reserves, min healthy 3, hedging and health on.
fn cfg() -> Config {
    let mut c = Config::defaults_without_home();
    c.default_engines = strs(&["e1", "e2", "e3", "e4"]);
    c.reserve_engines = strs(&["r1", "r2", "r3"]);
    c.fresh_engines = strs(&["fresh"]);
    c.locale_engines = HashMap::from([
        ("zh".to_owned(), strs(&["zh1"])),
        ("ko".to_owned(), strs(&["ko1"])),
    ]);
    c.min_healthy_engines = 3;
    c.hedge_enabled = true;
    c.health_enabled = true;
    c.coherence_guard_enabled = true;
    c.auto_route_enabled = true;
    c.claim_engine_limit = 3;
    c.scope_engine_limit = 3;
    c.request_timeout_s = 5.0;
    c.search_deadline_s = 8.0;
    c.engine_cooldown_s = 600.0;
    c.engine_cooldown_max_s = 3600.0;
    c
}

struct Kit {
    svc: SearchService,
    store: Arc<MemStore>,
}

fn kit_with(sources: Vec<Arc<Fake>>, cfg: Config, store: MemStore) -> Kit {
    let mut reg = Registry::new();
    for s in sources {
        reg.register(s).unwrap();
    }
    let store = Arc::new(store);
    let net = Arc::new(ReplayNet::from_fixtures(Vec::new()).unwrap());
    let deps = SourceDeps::new(net, Arc::clone(&store) as Arc<dyn StoreApi>, Arc::new(cfg));
    Kit {
        svc: SearchService::new(Arc::new(reg), deps),
        store,
    }
}

fn kit(sources: Vec<Arc<Fake>>, cfg: Config) -> Kit {
    kit_with(sources, cfg, MemStore::default())
}

fn req(q: &str) -> SearchRequest {
    SearchRequest::new(q)
}

fn scoped(q: &str, scope: &str) -> SearchRequest {
    let mut r = req(q);
    r.scope = scope.to_owned();
    r
}

async fn run(k: &Kit, r: SearchRequest) -> SearchResponse {
    k.svc.search(r, &CallCx::default()).await.unwrap()
}

fn urls(r: &SearchResponse) -> Vec<&str> {
    r.results.iter().map(|h| h.url.as_str()).collect()
}

fn note_with<'a>(r: &'a SearchResponse, needle: &str) -> &'a str {
    r.notes
        .iter()
        .find(|n| n.contains(needle))
        .unwrap_or_else(|| panic!("no note containing {needle:?} in {:#?}", r.notes))
}

// ------------------------------------------------------ quorum & grace ---

#[tokio::test(start_paused = true)]
async fn quorum_then_grace_cancels_stragglers_without_penalty() {
    let e4 = web("e4", 3000, hits("d", 10));
    let k = kit(
        vec![
            web("e1", 300, hits("a", 10)),
            web("e2", 300, hits("b", 10)),
            web("e3", 500, hits("c", 10)),
            e4.clone(),
        ],
        cfg(),
    );
    for q in ["one two", "three four", "five six"] {
        let r = run(&k, req(q)).await;
        assert_eq!(r.elapsed_ms, 700, "quorum at 300 ms + 400 ms grace");
        assert_eq!(r.engines, ["e1", "e2", "e3", "e4"]);
        assert_eq!(r.results.len(), 10);
        assert!(urls(&r).iter().all(|u| !u.contains("d.example")));
        assert!(
            !r.notes.iter().any(|n| n.contains("did not answer")),
            "{:?}",
            r.notes
        );
    }
    assert_eq!(e4.dropped(), 3, "the straggler was cancelled each time");
    assert_eq!(
        k.svc.benched("e4"),
        None,
        "grace cancellations are not strikes"
    );
}

#[tokio::test(start_paused = true)]
async fn results_answering_inside_the_grace_window_are_merged() {
    let k = kit(
        vec![
            web("e1", 300, hits("a", 5)),
            web("e2", 300, hits("b", 5)),
            web("e3", 650, hits("c", 5)),
            web("e4", 900, hits("d", 5)),
        ],
        cfg(),
    );
    let mut r = req("one two");
    r.limit = 10;
    let r = run(&k, r).await;
    assert_eq!(r.elapsed_ms, 700);
    assert!(
        urls(&r).iter().any(|u| u.contains("c.example")),
        "e3 answered inside the grace"
    );
    assert!(urls(&r).iter().all(|u| !u.contains("d.example")));
}

#[tokio::test(start_paused = true)]
async fn grace_waits_for_direct_claimants() {
    // A two-word query: the coherence guard cannot judge it, so the fake
    // buckets count towards the quorum.
    let pypi = with(
        Fake::new(
            decl("pypi", &["code"]),
            1200,
            Reply::Hits(vec![hit(
                "fastapi 0.141.1 on PyPI",
                "https://pypi.org/project/fastapi",
                "FastAPI 0.141.1, released 2026-11-20",
            )]),
        ),
        |f| f.claim = 1.0,
    );
    let k = kit(
        vec![
            web("e1", 200, hits("a", 10)),
            web("e2", 200, hits("b", 10)),
            web("e3", 3000, hits("c", 10)),
            pypi,
        ],
        cfg(),
    );
    let r = run(&k, req("fastapi version")).await;
    assert_eq!(
        r.elapsed_ms, 1600,
        "no quorum before the claimant settled (1.2 s), then the 400 ms grace cuts e3"
    );
    assert_eq!(r.results[0].url, "https://pypi.org/project/fastapi");
}

// ------------------------------------------------------------ hedging ---

#[tokio::test(start_paused = true)]
async fn fewer_than_two_answers_at_1_8_s_dispatches_a_reserve() {
    // tests/test_rescue.py::test_a_slow_first_candidate_does_not_hold_a_fast_second_one (inline hedge)
    let r1 = web("r1", 100, hits("r", 10));
    let r2 = web("r2", 100, hits("s", 10));
    let k = kit(
        vec![
            web("e1", 200, hits("a", 10)),
            slow_web("e2", 0, Reply::Hang),
            slow_web("e3", 0, Reply::Hang),
            slow_web("e4", 0, Reply::Hang),
            r1.clone(),
            r2.clone(),
        ],
        cfg(),
    );
    let r = run(&k, req("one two")).await;
    assert_eq!(r1.seen()[0].at, Duration::from_millis(1800));
    assert_eq!(r.elapsed_ms, 2300, "r1 answers at 1.9 s, grace 400 ms");
    assert_eq!(
        r2.calls(),
        0,
        "the quorum was met before the staggered second hedge"
    );
    assert_eq!(r.engines, ["e1", "e2", "e3", "e4", "r1"]);
    note_with(
        &r,
        "Asked reserve r1 because fewer than 2 sources had answered after 1.8 s.",
    );
}

#[tokio::test(start_paused = true)]
async fn a_walled_engine_is_asked_once_then_benched_and_reported() {
    // tests/test_pool_selection.py::test_a_walled_engine_is_asked_once_then_benched_and_reported
    let e1 = web("e1", 100, Reply::Fail(walled));
    let r1 = web("r1", 300, hits("r", 10));
    let r2 = web("r2", 100, hits("s", 10));
    let k = kit(
        vec![
            e1.clone(),
            web("e2", 600, hits("b", 10)),
            web("e3", 600, hits("c", 10)),
            web("e4", 600, hits("d", 10)),
            r1.clone(),
            r2.clone(),
        ],
        cfg(),
    );
    let r = run(&k, req("one two")).await;
    assert_eq!(
        r1.seen()[0].at,
        Duration::from_millis(100),
        "a lost primary hedges at once"
    );
    assert_eq!(r2.calls(), 0);
    note_with(&r, "e1 was captcha-gated → served via r1.");
    note_with(&r, "Asked reserve r1 because e1 was captcha-gated.");
    let bench = k.svc.benched("e1").expect("one wall benches");
    assert_eq!(bench.reason, "captcha");
    assert_eq!(bench.retry_in_seconds, 600);
    let saved = k.store.health.lock().unwrap().clone();
    assert!(saved.iter().any(|h| h.source_id == "e1"
        && h.state == "open:1"
        && h.last_error.as_deref() == Some("captcha")));

    // The next search does not ask it. Three general engines are still
    // standing (MIN_HEALTHY_ENGINES), so no reserve is seated.
    let r = run(&k, req("three four")).await;
    assert_eq!(e1.calls(), 1);
    assert_eq!(r.engines, ["e2", "e3", "e4"]);
    note_with(
        &r,
        "e1 is benched (captcha; retried in ~10 min). A benched source",
    );
}

#[tokio::test(start_paused = true)]
async fn one_benched_engine_is_replaced_by_a_reserve() {
    // tests/test_pool_selection.py (same name) + test_a_thin_pool_gets_reserves_in_order_up_to_the_minimum
    let mut c = cfg();
    c.min_healthy_engines = 4;
    let e1 = web("e1", 100, Reply::Fail(walled));
    let e2 = web("e2", 100, Reply::Fail(walled));
    let k = kit(
        vec![
            e1.clone(),
            e2.clone(),
            web("e3", 100, hits("c", 10)),
            web("e4", 100, hits("d", 10)),
            web("r1", 100, hits("r", 10)),
            web("r2", 100, hits("s", 10)),
            web("r3", 100, hits("t", 10)),
        ],
        c,
    );
    run(&k, req("one two")).await;
    let r = run(&k, req("three four")).await;
    assert_eq!((e1.calls(), e2.calls()), (1, 1));
    assert_eq!(
        r.engines,
        ["e3", "e4", "r1", "r2"],
        "reserves in order, up to the minimum"
    );
    note_with(
        &r,
        "e1 is benched (captcha; retried in ~10 min), r1 is standing in; \
         e2 is benched (captcha; retried in ~10 min), r2 is standing in.",
    );
}

#[tokio::test(start_paused = true)]
async fn the_second_hedge_follows_800_ms_later_and_never_a_third() {
    // tests/test_rescue.py::test_no_more_than_two_candidates_run_at_once
    let (r1, r2, r3) = (
        slow_web("r1", 0, Reply::Hang),
        slow_web("r2", 0, Reply::Hang),
        slow_web("r3", 0, Reply::Hang),
    );
    let k = kit(
        vec![
            slow_web("e1", 0, Reply::Hang),
            slow_web("e2", 0, Reply::Hang),
            slow_web("e3", 0, Reply::Hang),
            slow_web("e4", 0, Reply::Hang),
            r1.clone(),
            r2.clone(),
            r3.clone(),
        ],
        cfg(),
    );
    let r = run(&k, req("one two")).await;
    assert_eq!(r1.seen()[0].at, Duration::from_millis(1800));
    assert_eq!(r2.seen()[0].at, Duration::from_millis(2600));
    assert_eq!(r3.calls(), 0, "max 2 hedges");
    assert_eq!(r.elapsed_ms, 8000, "nothing answered: the hard deadline");
    assert!(r.results.is_empty());
    note_with(&r, "did not answer within 8.0 s (hard deadline)");
    // tests/test_rescue.py::test_a_candidate_cut_off_early_by_the_deadline_is_not_struck
    assert_eq!(k.svc.benched("r2"), None);
}

#[tokio::test(start_paused = true)]
async fn everything_settled_empty_spends_the_hedges_at_once() {
    // tests/test_rescue.py::test_zero_results_triggers_rescue_and_caches
    let r1 = web("r1", 100, Reply::Hits(Vec::new()));
    let r2 = web("r2", 100, hits("s", 4));
    let k = kit(
        vec![
            web("e1", 100, Reply::Hits(Vec::new())),
            web("e2", 100, Reply::Hits(Vec::new())),
            web("e3", 100, Reply::Hits(Vec::new())),
            web("e4", 100, Reply::Hits(Vec::new())),
            r1.clone(),
            r2.clone(),
        ],
        cfg(),
    );
    let r = run(&k, req("one two")).await;
    assert_eq!(r1.seen()[0].at, Duration::from_millis(100));
    assert_eq!(
        r2.seen()[0].at,
        Duration::from_millis(200),
        "settled thin: no 800 ms wait"
    );
    assert_eq!(r.results.len(), 4);
    assert_eq!(r.elapsed_ms, 300);
    assert_eq!(
        k.store.puts.lock().unwrap().len(),
        1,
        "the rescued answer is cached"
    );
}

#[tokio::test(start_paused = true)]
async fn the_breaker_and_hedging_can_be_switched_off() {
    // tests/test_rescue.py::test_rescue_disabled_by_setting, test_pool_selection.py::test_the_breaker_can_be_switched_off
    let mut c = cfg();
    c.hedge_enabled = false;
    c.health_enabled = false;
    let r1 = web("r1", 100, hits("r", 10));
    let e1 = web("e1", 100, Reply::Fail(walled));
    let k = kit(
        vec![e1.clone(), web("e2", 200, hits("b", 3)), r1.clone()],
        c,
    );
    let r = run(&k, req("one two")).await;
    assert_eq!(r1.calls(), 0);
    assert_eq!(r.elapsed_ms, 200);
    assert_eq!(k.svc.benched("e1"), None);
    run(&k, req("three four")).await;
    assert_eq!(e1.calls(), 2);
}

#[tokio::test(start_paused = true)]
async fn exclusive_scopes_and_named_sources_do_not_hedge() {
    // tests/test_rescue.py::test_exclusive_categories_do_not_rescue, test_the_same_loss_among_named_engines_does_not
    let r1 = web("r1", 100, hits("r", 10));
    let ds = Fake::new(decl("ds1", &["dataset"]), 100, Reply::Hits(Vec::new()));
    let e1 = web("e1", 100, Reply::Fail(walled));
    let k = kit(
        vec![
            e1.clone(),
            web("e2", 100, hits("b", 2)),
            ds.clone(),
            r1.clone(),
        ],
        cfg(),
    );
    let r = run(&k, scoped("imagenet labels", "dataset")).await;
    assert_eq!(r.engines, ["ds1"]);
    assert_eq!(e1.calls(), 0, "exclusive scopes do not mix web engines");
    let mut named = req("one two");
    named.sources = Some(strs(&["e1", "e2"]));
    let r = run(&k, named).await;
    assert_eq!(r.engines, ["e1", "e2"]);
    assert_eq!(r1.calls(), 0);
}

// ------------------------------------------------------------ deadlines ---

#[tokio::test(start_paused = true)]
async fn the_soft_deadline_returns_what_is_in() {
    let mut c = cfg();
    c.hedge_enabled = false;
    c.default_engines = strs(&["e1", "e2"]);
    let e2 = slow_web("e2", 4500, hits("b", 10));
    let k = kit(vec![web("e1", 300, hits("a", 3)), e2.clone()], c);
    let r = run(&k, req("one two")).await;
    assert_eq!(r.elapsed_ms, 4000);
    assert_eq!(r.results.len(), 3);
    assert_eq!(e2.dropped(), 1);
    note_with(
        &r,
        "e2 did not answer within 4.0 s (soft deadline) and was not waited for.",
    );
    assert_eq!(k.svc.benched("e2"), None);
}

#[tokio::test(start_paused = true)]
async fn after_the_soft_deadline_the_first_answer_ends_the_search() {
    let mut c = cfg();
    c.hedge_enabled = false;
    c.default_engines = strs(&["e1", "e2"]);
    let k = kit(
        vec![
            slow_web("e1", 5000, hits("a", 3)),
            slow_web("e2", 0, Reply::Hang),
        ],
        c,
    );
    let r = run(&k, req("one two")).await;
    assert_eq!(r.elapsed_ms, 5000);
    assert_eq!(r.results.len(), 3);
}

#[tokio::test(start_paused = true)]
async fn a_source_s_own_timeout_is_a_soft_strike() {
    // tests/test_rescue.py::test_candidates_still_running_at_the_deadline_are_benched (own timeout)
    let mut c = cfg();
    c.hedge_enabled = false;
    c.default_engines = strs(&["e1", "e2", "e3"]);
    let k = kit(
        vec![
            web("e1", 0, Reply::Hang),
            web("e2", 100, Reply::Hits(Vec::new())),
            web("e3", 100, Reply::Hits(Vec::new())),
        ],
        c,
    );
    let r = run(&k, req("one two")).await;
    assert_eq!(r.elapsed_ms, 5000, "REQUEST_TIMEOUT for a web engine");
    note_with(&r, "Not answered: e1 (timed out after 5.0 s).");
    assert_eq!(k.svc.benched("e1"), None, "one timeout is a blip");
    run(&k, req("three four")).await;
    assert_eq!(k.svc.benched("e1").unwrap().reason, "timeout");
}

#[tokio::test(start_paused = true)]
async fn cancellation_returns_timeout_zero() {
    let k = kit(
        vec![
            slow_web("e1", 0, Reply::Hang),
            slow_web("e2", 0, Reply::Hang),
        ],
        cfg(),
    );
    let cx = CallCx::default();
    let token = cx.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1000)).await;
        token.cancel();
    });
    let err = k.svc.search(req("one two"), &cx).await.unwrap_err();
    assert!(matches!(err, FsmError::Timeout(0)), "{err:?}");
}

#[tokio::test(start_paused = true)]
async fn the_caller_s_deadline_caps_the_hard_deadline() {
    let mut c = cfg();
    c.hedge_enabled = false;
    let k = kit(
        vec![
            slow_web("e1", 0, Reply::Hang),
            slow_web("e2", 0, Reply::Hang),
        ],
        c,
    );
    let cx = CallCx::new("s", std::time::Instant::now() + Duration::from_millis(2500));
    let r = k.svc.search(req("one two"), &cx).await.unwrap();
    // The budget is read from the real clock at entry: allow for a slow machine.
    assert!((1500..=2500).contains(&r.elapsed_ms), "{}", r.elapsed_ms);
}

// ------------------------------------------------------ M16 claims routing ---

fn pypi(delay_ms: u64) -> Arc<Fake> {
    with(
        Fake::new(
            decl("pypi", &["code"]),
            delay_ms,
            Reply::Hits(vec![hit(
                "fastapi 0.141.1 on PyPI",
                "https://pypi.org/project/fastapi",
                "FastAPI 0.141.1, released 2026-11-20",
            )]),
        ),
        |f| f.claim = 1.0,
    )
}

fn pages_with_pypi(p: &str) -> Reply {
    Reply::Hits(vec![
        hit("FastAPI", "https://fastapi.tiangolo.com", "s"),
        hit("fastapi · PyPI", "https://pypi.org/project/fastapi/", "s"),
        hit(
            &format!("{p} other"),
            &format!("https://{p}.example/x"),
            "s",
        ),
    ])
}

#[tokio::test(start_paused = true)]
async fn a_direct_claimant_is_seated_on_web_and_code_with_the_record_on_top() {
    // M16: claims() + availability() on BOTH scope="web" AND scope="code".
    let p = pypi(300);
    let k = kit(
        vec![
            web("e1", 200, pages_with_pypi("a")),
            web("e2", 200, pages_with_pypi("b")),
            web("e3", 200, pages_with_pypi("c")),
            p.clone(),
        ],
        cfg(),
    );
    for scope in ["web", "code", "software.python", "github"] {
        let r = run(&k, scoped("fastapi latest version", scope)).await;
        assert!(
            r.engines.contains(&"pypi".to_owned()),
            "{scope}: {:?}",
            r.engines
        );
        assert_eq!(
            r.results[0].url, "https://pypi.org/project/fastapi",
            "{scope}"
        );
        assert_eq!(
            r.results[0].title, "fastapi 0.141.1 on PyPI",
            "the record names the value"
        );
        assert_eq!(r.results[0].sources, ["e1", "e2", "e3", "pypi"]);
        assert_eq!(
            r.lead.as_deref(),
            Some("According to pypi.org: FastAPI 0.141.1, released 2026-11-20")
        );
        note_with(&r, "Auto-routed to pypi: they claim this query.");
    }
    // `software.python` and `github` resolve to `code`: same cache row.
    assert_eq!(p.calls(), 2);
}

/// A registry record for "fastapi" from a claimant with the given score.
fn npm_claiming(score: f32) -> Arc<Fake> {
    with(
        Fake::new(
            decl("npm", &["code"]),
            100,
            Reply::Hits(vec![hit(
                "fastapi on npm",
                "https://www.npmjs.com/package/fastapi",
                "fastapi 1.2.3",
            )]),
        ),
        |f| f.claim = score,
    )
}

#[tokio::test(start_paused = true)]
async fn a_relevant_claim_is_seated_without_the_direct_boost() {
    // Orchestrator ruling: 0 < claims() < DIRECT_CLAIM_THRESHOLD (0.8) seats the
    // source (native weight 2.0) but gives its first result no ×5.0 boost.
    let npm = npm_claiming(0.5);
    let k = kit(
        vec![
            web("e1", 100, pages_with_pypi("a")),
            web("e2", 100, pages_with_pypi("b")),
            web("e3", 100, pages_with_pypi("c")),
            npm.clone(),
        ],
        cfg(),
    );
    let r = run(&k, req("fastapi version")).await;
    assert!(
        r.engines.contains(&"npm".to_owned()),
        "relevant claimants are seated"
    );
    note_with(&r, "Auto-routed to npm: they claim this query.");
    // Three engines agreeing (3/61) beat one native record (2/61).
    assert_eq!(r.results[0].url, "https://fastapi.tiangolo.com");
    let rec = r
        .results
        .iter()
        .position(|h| h.url.contains("npmjs"))
        .unwrap();
    assert!(rec > 0);
    assert_eq!(
        r.lead, None,
        "only a direct record is a lead without the term test"
    );
}

#[tokio::test(start_paused = true)]
async fn a_direct_claim_from_the_threshold_up_puts_the_record_first() {
    for score in [plan::DIRECT_CLAIM_THRESHOLD, 0.9, 1.0] {
        let k = kit(
            vec![
                web("e1", 100, pages_with_pypi("a")),
                web("e2", 100, pages_with_pypi("b")),
                web("e3", 100, pages_with_pypi("c")),
                npm_claiming(score),
            ],
            cfg(),
        );
        let r = run(&k, req("fastapi version")).await;
        assert_eq!(
            r.results[0].url, "https://www.npmjs.com/package/fastapi",
            "score {score}"
        );
        assert_eq!(
            r.lead.as_deref(),
            Some("According to npmjs.com: fastapi 1.2.3")
        );
    }
    let k = kit(
        vec![
            web("e1", 100, pages_with_pypi("a")),
            web("e2", 100, pages_with_pypi("b")),
            web("e3", 100, pages_with_pypi("c")),
            npm_claiming(0.79),
        ],
        cfg(),
    );
    let r = run(&k, req("fastapi version")).await;
    assert_ne!(
        r.results[0].url, "https://www.npmjs.com/package/fastapi",
        "0.79 is only relevant"
    );
}

#[tokio::test(start_paused = true)]
async fn on_scope_code_a_relevant_native_gets_no_direct_boost_either() {
    let k = kit(
        vec![
            web("e1", 100, pages_with_pypi("a")),
            web("e2", 100, pages_with_pypi("b")),
            web("e3", 100, pages_with_pypi("c")),
            npm_claiming(0.5),
        ],
        cfg(),
    );
    let r = run(&k, scoped("fastapi version", "code")).await;
    assert!(r.engines.contains(&"npm".to_owned()));
    assert_eq!(r.results[0].url, "https://fastapi.tiangolo.com");
}

#[tokio::test(start_paused = true)]
async fn the_claim_limit_takes_the_strongest_claims_first() {
    let mut c = cfg();
    c.claim_engine_limit = 1;
    let weak = npm_claiming(0.5);
    let strong = pypi(100);
    let k = kit(
        vec![web("e1", 100, hits("a", 3)), weak.clone(), strong.clone()],
        c,
    );
    let r = run(&k, req("fastapi version")).await;
    assert_eq!(r.engines, ["e1", "pypi"]);
    assert_eq!(weak.calls(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_claimant_is_not_seated_outside_its_scope_or_when_routing_is_off() {
    let p = pypi(100);
    let k = kit(
        vec![
            web("e1", 100, hits("a", 3)),
            web("e2", 100, hits("b", 3)),
            p.clone(),
        ],
        cfg(),
    );
    let r = run(&k, scoped("fastapi latest version", "news")).await;
    assert!(!r.engines.contains(&"pypi".to_owned()));
    let mut off = cfg();
    off.auto_route_enabled = false;
    let p2 = pypi(100);
    let k2 = kit(vec![web("e1", 100, hits("a", 3)), p2.clone()], off);
    let r = run(&k2, req("fastapi latest version")).await;
    assert_eq!(r.engines, ["e1"]);
    assert_eq!(p.calls() + p2.calls(), 0);
}

#[tokio::test(start_paused = true)]
async fn claimants_respect_availability_and_the_claim_limit() {
    let locked = with(pypi(100), |f| {
        f.avail = Availability::NeedsKey {
            env: "PYPI_KEY".into(),
        }
    });
    let k = kit(vec![web("e1", 100, hits("a", 3)), locked.clone()], cfg());
    let r = run(&k, req("fastapi latest version")).await;
    assert_eq!(
        r.engines,
        ["e1"],
        "an unavailable claimant is not auto-routed"
    );
}

// ---------------------------------------------------------- pool selection ---

#[tokio::test(start_paused = true)]
async fn the_default_pool_is_four_general_web_engines_and_no_news_feed() {
    // tests/test_pool_selection.py (same name)
    let fresh = Fake::new(decl("fresh", &["news"]), 100, hits("f", 3));
    let k = kit(
        vec![
            web("e1", 100, hits("a", 3)),
            web("e2", 100, hits("b", 3)),
            web("e3", 100, hits("c", 3)),
            web("e4", 100, hits("d", 3)),
            fresh.clone(),
        ],
        cfg(),
    );
    let r = run(&k, req("one two")).await;
    assert_eq!(r.engines, ["e1", "e2", "e3", "e4"]);
    // tests/test_pool_selection.py::test_the_news_feed_joins_only_when_recency_was_asked_for
    let mut fr = req("one two");
    fr.freshness = Some(Freshness::Week);
    let r = run(&k, fr).await;
    assert_eq!(r.engines, ["e1", "e2", "e3", "e4", "fresh"]);
    let mut month = req("one two");
    month.freshness = Some(Freshness::Month);
    assert!(!run(&k, month).await.engines.contains(&"fresh".to_owned()));
}

#[tokio::test(start_paused = true)]
async fn a_query_in_chinese_or_korean_adds_that_language_s_indexes() {
    // tests/test_pool_selection.py (same name) + test_the_configured_region_does_not_add_a_locale_engine
    // + test_a_chinese_query_also_asks_a_chinese_index
    let zh = web("zh1", 100, hits("z", 3));
    let ko = web("ko1", 100, hits("k", 3));
    let k = kit(
        vec![web("e1", 100, hits("a", 3)), zh.clone(), ko.clone()],
        cfg(),
    );
    let r = run(&k, req("上海 天气 预报")).await;
    assert_eq!(r.engines, ["e1", "zh1"]);
    assert_eq!(
        zh.seen()[0].lang.as_deref(),
        Some("zh"),
        "SrcCx.lang carries the query language"
    );
    let r = run(&k, req("그래프 신경망")).await;
    assert_eq!(r.engines, ["e1", "ko1"]);
    let r = run(&k, req("rust ownership")).await;
    assert_eq!(r.engines, ["e1"]);
    let mut explicit_lang = req("rust ownership");
    explicit_lang.lang = Some("zh".into());
    assert_eq!(run(&k, explicit_lang).await.engines, ["e1", "zh1"]);
}

#[tokio::test(start_paused = true)]
async fn a_scope_adds_its_native_sources_capped_and_spread_across_sub_scopes() {
    let mut c = cfg();
    c.scope_engine_limit = 2;
    let a1 = Fake::new(decl("arx", &["academic.preprint"]), 100, hits("x", 2));
    let a2 = Fake::new(decl("arx2", &["academic.preprint"]), 100, hits("y", 2));
    let b1 = Fake::new(decl("pubm", &["academic.biomed"]), 100, hits("z", 2));
    let k = kit(vec![web("e1", 100, hits("a", 2)), a1, a2.clone(), b1], c);
    let r = run(&k, scoped("protein folding", "academic")).await;
    assert_eq!(
        r.engines,
        ["arx", "pubm"],
        "round-robin over sub-scopes, then the cap"
    );
    let r = run(&k, scoped("protein folding", "paper.preprint")).await;
    assert_eq!(r.engines, ["arx", "arx2"]);
    let explicit_only = with(
        Fake::new(decl("shop", &["commerce"]), 100, hits("s", 2)),
        |f| f.decl.pool_role = fsm_core::PoolRole::Explicit,
    );
    let k = kit(
        vec![web("e1", 100, hits("a", 2)), explicit_only.clone()],
        cfg(),
    );
    let r = run(&k, scoped("keyboard", "commerce")).await;
    assert_eq!(
        r.engines,
        ["e1"],
        "explicit-only sources are never auto-routed; no specialist → web fallback"
    );
    assert_eq!(explicit_only.calls(), 0);
}

#[tokio::test(start_paused = true)]
async fn the_cache_key_does_not_wobble_with_the_breaker() {
    // tests/test_pool_selection.py (same name)
    let e1 = web("e1", 100, Reply::Fail(walled));
    let k = kit(
        vec![
            e1,
            web("e2", 100, hits("b", 3)),
            web("e3", 100, hits("c", 3)),
            web("r1", 100, hits("r", 3)),
        ],
        cfg(),
    );
    let a = run(&k, req("one two")).await;
    assert!(k.svc.benched("e1").is_some());
    let mut again = req("one two");
    again.max_age_hours = Some(0.0);
    let b = run(&k, again).await;
    assert_eq!(a.cache_key, b.cache_key);
    assert_ne!(a.engines, b.engines);
}

#[tokio::test(start_paused = true)]
async fn with_no_general_engine_left_the_breaker_is_ignored() {
    // tests/test_pool_selection.py (same name)
    let mut c = cfg();
    c.default_engines = strs(&["e1"]);
    c.reserve_engines = Vec::new();
    let e1 = web("e1", 100, Reply::Fail(walled));
    let k = kit(vec![e1.clone()], c);
    run(&k, req("one two")).await;
    assert!(k.svc.benched("e1").is_some());
    let r = run(&k, req("three four")).await;
    assert_eq!(
        e1.calls(),
        2,
        "asked anyway: three doubtful engines beat none"
    );
    assert!(!r.notes.iter().any(|n| n.contains("is benched")));
}

#[tokio::test(start_paused = true)]
async fn an_engine_that_recovers_is_un_benched() {
    // tests/test_pool_selection.py (same name)
    let e1 = web("e1", 100, Reply::Fail(walled));
    let k = kit(
        vec![
            e1.clone(),
            web("e2", 100, hits("b", 3)),
            web("e3", 100, hits("c", 3)),
            web("r1", 100, hits("r", 3)),
        ],
        cfg(),
    );
    run(&k, req("one two")).await;
    assert!(k.svc.benched("e1").is_some());
    tokio::time::advance(Duration::from_secs(601)).await;
    assert_eq!(
        k.svc.benched("e1"),
        None,
        "the cooldown ran out: the next search probes"
    );
    e1.set(100, hits("a", 3));
    run(&k, req("three four")).await;
    assert_eq!(e1.calls(), 2);
    let saved = k.store.health.lock().unwrap().clone();
    assert_eq!(saved.last().map(|h| h.state.as_str()), Some("closed"));
}

#[tokio::test(start_paused = true)]
async fn naming_a_benched_engine_still_runs_it() {
    // tests/test_pool_selection.py (same name) + test_a_named_engine_that_fails_is_still_recorded
    let e1 = web("e1", 100, Reply::Fail(walled));
    let k = kit(
        vec![
            e1.clone(),
            web("e2", 100, hits("b", 3)),
            web("r1", 100, hits("r", 3)),
        ],
        cfg(),
    );
    run(&k, req("one two")).await;
    let mut named = req("one two");
    named.sources = Some(strs(&["e1", "nope"]));
    let r = run(&k, named).await;
    assert_eq!(e1.calls(), 2);
    assert_eq!(r.engines, ["e1"]);
    note_with(&r, "Unknown source(s) skipped: nope.");
    note_with(&r, "e1 was captcha-gated (no results).");
    let mut none = req("one two");
    none.sources = Some(strs(&["nope"]));
    let err = k.svc.search(none, &CallCx::default()).await.unwrap_err();
    assert!(
        matches!(&err, FsmError::InvalidInput(m) if m.contains("unknown source(s) nope")),
        "{err:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn named_sources_resolve_aliases() {
    let ddg = with(web("duckduckgo", 100, hits("a", 2)), |f| {
        f.decl.aliases = strs(&["ddg"])
    });
    let k = kit(vec![ddg.clone()], cfg());
    let mut named = req("one two");
    named.sources = Some(strs(&["DDG"]));
    assert_eq!(run(&k, named).await.engines, ["duckduckgo"]);
}

// ------------------------------------------------------------ health ---

#[tokio::test(start_paused = true)]
async fn a_reserve_that_is_benched_unavailable_or_cannot_render_is_skipped() {
    // tests/test_pool_selection.py (same name). "Cannot render" has no 1.0
    // counterpart: a source without a browser fails with BrowserUnavailable.
    let mut c = cfg();
    c.default_engines = strs(&["e1"]);
    let r1 = web("r1", 100, hits("r1", 3));
    let r2 = with(web("r2", 100, hits("r2", 3)), |f| {
        f.avail = Availability::Disabled("switched off".into())
    });
    let r3 = web("r3", 100, hits("r3", 3));
    let k = kit(
        vec![
            web("e1", 100, Reply::Hits(Vec::new())),
            r1.clone(),
            r2.clone(),
            r3.clone(),
        ],
        c,
    );
    k.svc.breaker().record_failure("r1", "captcha", Some(1));
    let r = run(&k, req("one two")).await;
    assert_eq!((r1.calls(), r2.calls(), r3.calls()), (0, 0, 1));
    assert!(urls(&r).iter().all(|u| u.contains("r3")));
}

#[tokio::test(start_paused = true)]
async fn rescue_skips_a_benched_candidate() {
    // tests/test_pool_selection.py (same name)
    let mut c = cfg();
    c.default_engines = strs(&["e1"]);
    let (r1, r2) = (web("r1", 100, hits("r1", 3)), web("r2", 100, hits("r2", 3)));
    let k = kit(
        vec![
            web("e1", 100, Reply::Hits(Vec::new())),
            r1.clone(),
            r2.clone(),
        ],
        c,
    );
    k.svc.breaker().record_failure("r1", "captcha", Some(1));
    let r = run(&k, req("one two")).await;
    assert_eq!((r1.calls(), r2.calls()), (0, 1));
    assert!(r.engines.contains(&"r2".to_owned()));
}

#[tokio::test(start_paused = true)]
async fn a_missing_browser_is_never_held_against_an_engine() {
    // tests/test_pool_selection.py (same name)
    let mut c = cfg();
    c.default_engines = strs(&["e1", "e2"]);
    let e1 = web(
        "e1",
        100,
        Reply::Fail(|_| FsmError::BrowserUnavailable("no Chrome".into())),
    );
    let k = kit(vec![e1.clone(), web("e2", 100, hits("b", 3))], c);
    for i in 0..3 {
        run(&k, req(&format!("one two {i}"))).await;
    }
    assert_eq!(e1.calls(), 3);
    assert_eq!(k.svc.benched("e1"), None);
}

#[tokio::test(start_paused = true)]
async fn losing_engines_mid_run_triggers_a_stand_in_even_with_plenty_of_results() {
    // tests/test_pool_selection.py (same name): one survivor with ten results
    // still rests on a single index.
    let mut c = cfg();
    c.default_engines = strs(&["e1", "e2", "e3"]);
    let r1 = web("r1", 100, hits("r1", 3));
    let k = kit(
        vec![
            web("e1", 100, Reply::Fail(walled)),
            web(
                "e2",
                100,
                Reply::Fail(|id| FsmError::upstream(id, Some(500), "boom")),
            ),
            web("e3", 100, hits("c", 10)),
            r1.clone(),
        ],
        c,
    );
    let r = run(&k, req("one two")).await;
    assert!(
        r.results
            .iter()
            .filter(|h| h.sources.contains(&"e3".to_owned()))
            .count()
            > 3
    );
    assert_eq!(r1.calls(), 1);
    note_with(&r, "e1 was captcha-gated → served via r1");
}

#[tokio::test(start_paused = true)]
async fn a_configuration_error_is_not_a_health_problem() {
    // tests/test_pool_selection.py (same name)
    let e1 = web("e1", 100, Reply::Fail(needs_key));
    let k = kit(vec![e1.clone()], cfg());
    for q in ["a b", "c d", "e f"] {
        let mut named = req(q);
        named.sources = Some(strs(&["e1"]));
        let r = run(&k, named).await;
        note_with(&r, "Not answered: e1 (needs FAKE_API_KEY).");
    }
    assert_eq!(k.svc.benched("e1"), None);
}

#[tokio::test(start_paused = true)]
async fn a_rate_limit_is_a_soft_strike() {
    // tests/test_pool_selection.py::test_a_rate_limit_skip_is_ours_not_the_engines (upstream 429 = refusal)
    let mut c = cfg();
    c.hedge_enabled = false;
    let e1 = web("e1", 100, Reply::Fail(rate_limited));
    let k = kit(vec![e1, web("e2", 100, hits("b", 3))], c);
    let r = run(&k, req("one two")).await;
    note_with(&r, "Not answered: e1 (rate limited; retry after 30 s).");
    assert_eq!(k.svc.benched("e1"), None);
    run(&k, req("three four")).await;
    assert_eq!(k.svc.benched("e1").unwrap().reason, "http_429");
}

#[tokio::test(start_paused = true)]
async fn silence_counts_only_when_a_peer_found_something() {
    // tests/test_pool_selection.py (same name)
    let mut c = cfg();
    c.hedge_enabled = false;
    c.default_engines = strs(&["e1", "e2"]);
    let e2 = web("e2", 100, Reply::Hits(Vec::new()));
    let k = kit(vec![web("e1", 100, hits("a", 3)), e2.clone()], c.clone());
    for (i, q) in ["a b", "c d"].iter().enumerate() {
        let r = run(&k, req(q)).await;
        assert_eq!(k.svc.benched("e2"), None, "run {i}");
        note_with(&r, "e2 returned 0 results with no error");
    }
    run(&k, req("e f")).await;
    assert_eq!(k.svc.benched("e2").unwrap().reason, "silent");
    // Alone with nothing to compare against: never silent.
    let k = kit(
        vec![
            web("e1", 100, Reply::Hits(Vec::new())),
            web("e2", 100, Reply::Hits(Vec::new())),
        ],
        c,
    );
    for q in ["a b", "c d", "e f", "g h"] {
        run(&k, req(q)).await;
    }
    assert_eq!(k.svc.benched("e1"), None);
}

#[tokio::test(start_paused = true)]
async fn persisted_benches_are_adopted_by_a_fresh_service() {
    let mut row = HealthRecord::new("e1", "open:1", jiff::Timestamp::now().as_second());
    row.benched_until = Some(row.updated_at + 600);
    row.failures = 1;
    row.last_error = Some("captcha".into());
    let store = MemStore {
        health: Mutex::new(vec![row]),
        ..MemStore::default()
    };
    let e1 = web("e1", 100, hits("a", 3));
    let k = kit_with(
        vec![
            e1.clone(),
            web("e2", 100, hits("b", 3)),
            web("r1", 100, hits("r", 3)),
        ],
        cfg(),
        store,
    );
    let r = run(&k, req("one two")).await;
    assert_eq!(e1.calls(), 0);
    note_with(&r, "e1 is benched (captcha;");
}

// ---------------------------------------------------- coherence guard ---

fn rust_decoy() -> Reply {
    Reply::Hits(
        [
            ("Rust on Steam", "The only aim in Rust is to survive."),
            (
                "Rust — Explore, Build and Survive",
                "The only aim in Rust is to survive.",
            ),
            ("Rust Programming Language", "Rust in production."),
            (
                "Rust (video game) - Wikipedia",
                "Rust is a 2018 multiplayer survival video game",
            ),
            ("Rust - Download", "Rust is a PvP-driven game"),
        ]
        .iter()
        .enumerate()
        .map(|(i, (t, s))| hit(t, &format!("https://decoy{i}.example/"), s))
        .collect(),
    )
}

fn rust_healthy() -> Reply {
    Reply::Hits(
        [
            (
                "Understanding Ownership - The Rust Programming Language",
                "Ownership is Rust's most unique feature",
            ),
            (
                "References and Borrowing - The Rust Programming Language",
                "borrowing rules",
            ),
            (
                "Rust Basics Explained: Borrowing in Rust",
                "Borrowing is Rust's way",
            ),
            ("Ownership & Borrowing · Learn Rust", "Ownership"),
        ]
        .iter()
        .enumerate()
        .map(|(i, (t, s))| hit(t, &format!("https://good{i}.example/"), s))
        .collect(),
    )
}

#[tokio::test(start_paused = true)]
async fn a_decoy_bucket_is_dropped_and_the_healthy_one_kept() {
    // tests/test_coherence_guard.py (same name) + test_the_hint_explains_and_does_not_send_anyone_to_fix_their_network
    let mut c = cfg();
    c.reserve_engines = Vec::new();
    c.default_engines = strs(&["e1", "e2"]);
    let k = kit(
        vec![web("e1", 100, rust_healthy()), web("e2", 100, rust_decoy())],
        c,
    );
    let r = run(&k, req("rust ownership borrowing")).await;
    assert!(
        urls(&r).iter().all(|u| u.contains("good")),
        "{:?}",
        urls(&r)
    );
    let n = note_with(
        &r,
        "e2 answered with results that match only the first word of the query",
    );
    assert!(!n.contains("Configure a proxy"));
    assert_eq!(k.svc.benched("e2").unwrap().reason, "off_topic");
}

#[tokio::test(start_paused = true)]
async fn a_lone_suspect_is_kept_when_nothing_can_confirm_it() {
    // tests/test_coherence_guard.py::test_a_lone_suspect_is_kept_when_rescue_is_disabled
    let mut c = cfg();
    c.hedge_enabled = false;
    c.default_engines = strs(&["e2"]);
    let k = kit(vec![web("e2", 100, rust_decoy())], c);
    let r = run(&k, req("rust ownership borrowing")).await;
    assert_eq!(r.results.len(), 5);
    assert_eq!(k.svc.benched("e2"), None);
}

#[tokio::test(start_paused = true)]
async fn a_lone_decoy_gets_a_second_opinion_and_loses() {
    // tests/test_coherence_guard.py (same name): the suspect hedges, the reserve witnesses.
    let mut c = cfg();
    c.default_engines = strs(&["e2"]);
    let r1 = web("r1", 100, rust_healthy());
    let k = kit(vec![web("e2", 100, rust_decoy()), r1.clone()], c);
    let r = run(&k, req("rust ownership borrowing")).await;
    assert_eq!(r1.seen()[0].at, Duration::from_millis(100));
    assert!(urls(&r).iter().all(|u| u.contains("good")));
    note_with(
        &r,
        "e2 answered with results that match only the first word of the query; they were discarded → served via r1.",
    );
}

/// Two general engines (no hedging) for the coherence guard cases.
fn guard_kit(e1: Reply, e2: Reply) -> Kit {
    let mut c = cfg();
    c.default_engines = strs(&["e1", "e2"]);
    c.hedge_enabled = false;
    kit(vec![web("e1", 100, e1), web("e2", 100, e2)], c)
}

fn rows(reply: Reply) -> Vec<Hit> {
    match reply {
        Reply::Hits(h) => h,
        _ => unreachable!(),
    }
}

fn discarded(r: &SearchResponse) -> bool {
    r.notes.iter().any(|n| n.contains("discarded"))
}

#[tokio::test(start_paused = true)]
async fn two_engines_that_agree_are_the_querys_doing_not_a_decoy() {
    // tests/test_coherence_guard.py (same name): no witness, no verdict.
    let k = guard_kit(rust_decoy(), rust_decoy());
    let r = run(&k, req("rust ownership borrowing")).await;
    assert_eq!(r.results.len(), 5);
    assert!(urls(&r).iter().all(|u| u.contains("decoy")));
    assert!(!discarded(&r));
}

#[tokio::test(start_paused = true)]
async fn a_grey_bucket_is_neither_dropped_nor_a_witness() {
    // tests/test_coherence_guard.py (same name): 3 of 8 on topic = 0.375.
    let mut grey = rows(rust_decoy());
    grey.extend((0..3).map(|i| {
        hit(
            "Ownership in Rust",
            &format!("https://grey{i}.example/"),
            "borrowing",
        )
    }));
    let k = guard_kit(rust_decoy(), Reply::Hits(grey));
    let r = run(&k, req("rust ownership borrowing")).await;
    assert!(
        r.results
            .iter()
            .any(|h| h.sources.contains(&"e1".to_owned())),
        "a grey bucket cannot convict another"
    );
    assert!(!discarded(&r));
}

#[tokio::test(start_paused = true)]
async fn a_query_too_short_to_judge_is_never_guarded() {
    // tests/test_coherence_guard.py (same name, parametrized)
    for q in ["rust", "rust ownership"] {
        let k = guard_kit(rust_decoy(), rust_healthy());
        let r = run(&k, req(q)).await;
        assert!(urls(&r).iter().any(|u| u.contains("decoy")), "{q}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_bucket_too_small_to_judge_is_never_guarded() {
    // tests/test_coherence_guard.py (same name)
    let mut small = rows(rust_decoy());
    small.truncate(3);
    let k = guard_kit(Reply::Hits(small), rust_healthy());
    let r = run(&k, req("rust ownership borrowing")).await;
    assert!(urls(&r).iter().any(|u| u.contains("decoy")));
}

#[tokio::test(start_paused = true)]
async fn a_lone_suspect_is_kept_when_the_second_opinion_agrees_with_it() {
    // tests/test_coherence_guard.py (same name): every hedge tried, none usable,
    // nothing convicted.
    let mut c = cfg();
    c.default_engines = strs(&["e2"]);
    let (r1, r2) = (web("r1", 100, rust_decoy()), web("r2", 100, rust_decoy()));
    let k = kit(
        vec![web("e2", 100, rust_decoy()), r1.clone(), r2.clone()],
        c,
    );
    let r = run(&k, req("rust ownership borrowing")).await;
    assert_eq!((r1.calls(), r2.calls()), (1, 1));
    assert!(!r.results.is_empty());
    assert!(urls(&r).iter().all(|u| u.contains("decoy")));
    assert!(!discarded(&r));
}

#[tokio::test(start_paused = true)]
async fn rescue_never_recovers_into_a_decoy() {
    // tests/test_coherence_guard.py (same name): an empty primary hedges into a
    // decoy, the next reserve witnesses against it.
    let mut c = cfg();
    c.default_engines = strs(&["e1"]);
    let (r1, r2) = (web("r1", 100, rust_decoy()), web("r2", 100, rust_healthy()));
    let k = kit(
        vec![
            web("e1", 100, Reply::Hits(Vec::new())),
            r1.clone(),
            r2.clone(),
        ],
        c,
    );
    let r = run(&k, req("rust ownership borrowing")).await;
    assert_eq!((r1.calls(), r2.calls()), (1, 1));
    assert!(!r.results.is_empty());
    assert!(urls(&r).iter().all(|u| u.contains("good")));
}

#[tokio::test(start_paused = true)]
async fn a_specialist_that_does_not_echo_the_query_is_left_alone() {
    // tests/test_coherence_guard.py (same name)
    let mut c = cfg();
    c.default_engines = strs(&["e1"]);
    let filings = Fake::new(decl("sec", &["finance"]), 100, rust_decoy());
    let k = kit(vec![web("e1", 100, rust_healthy()), filings], c);
    let r = run(&k, scoped("rust ownership borrowing", "finance")).await;
    assert!(urls(&r).iter().any(|u| u.contains("decoy")));
}

#[tokio::test(start_paused = true)]
async fn the_guard_can_be_switched_off() {
    // tests/test_coherence_guard.py (same name)
    let mut c = cfg();
    c.coherence_guard_enabled = false;
    c.default_engines = strs(&["e1", "e2"]);
    let k = kit(
        vec![web("e1", 100, rust_healthy()), web("e2", 100, rust_decoy())],
        c,
    );
    let r = run(&k, req("rust ownership borrowing")).await;
    assert!(urls(&r).iter().any(|u| u.contains("decoy")));
}

// ------------------------------------------------- filters, dates, URLs ---

#[tokio::test(start_paused = true)]
async fn freshness_and_domains_reach_the_sources_and_are_re_applied() {
    // tests/test_filters.py::test_aggregate_search_passes_filters_to_engine
    let mut c = cfg();
    c.default_engines = strs(&["e1"]);
    c.fresh_engines = Vec::new();
    let mut old = hit("Old news", "https://ok.example/old", "s");
    old.date = Some("10 days ago".into());
    let e1 = web(
        "e1",
        100,
        Reply::Hits(vec![
            hit("Spam", "https://www.spam.example/x", "s"),
            old,
            hit("Fine", "https://ok.example/fine", "s"),
        ]),
    );
    let k = kit(vec![e1.clone()], c);
    let mut r = req("one two");
    r.freshness = Some(Freshness::Week);
    r.domains = Some(strs(&["-spam.example"]));
    let r = run(&k, r).await;
    let seen = &e1.seen()[0];
    assert_eq!(seen.freshness, Some(Freshness::Week));
    assert_eq!(seen.domains, ["-spam.example"]);
    assert_eq!(urls(&r), ["https://ok.example/fine"]);
    // tests/test_filter_diagnostics.py::test_aggregate_emits_diagnostics_when_results_sparse
    note_with(&r, "Filters dropped 2 of 3 raw results (kept 1).");
    // tests/test_freshness_signals.py::test_asking_for_recency_gets_told_when_recency_cannot_be_shown
    note_with(
        &r,
        "1 of 1 results carry no verifiable date, and freshness=\"week\"",
    );
}

#[tokio::test(start_paused = true)]
async fn plentiful_results_carry_no_filter_diagnostics() {
    // tests/test_filter_diagnostics.py::test_aggregate_omits_diagnostics_when_results_plentiful
    let mut c = cfg();
    c.default_engines = strs(&["e1"]);
    let k = kit(vec![web("e1", 100, hits("a", 8))], c);
    let mut r = req("one two");
    r.domains = Some(strs(&["-spam.example"]));
    let r = run(&k, r).await;
    assert!(r.notes.is_empty(), "{:?}", r.notes);
}

#[tokio::test(start_paused = true)]
async fn the_news_scope_filters_general_engines_by_host_but_trusts_natives() {
    // tests/test_category_native_and_locale.py::test_engine_declaring_category_is_trusted_end_to_end
    let mut c = cfg();
    c.default_engines = strs(&["e1"]);
    let gn = Fake::new(
        decl("gn", &["news"]),
        100,
        Reply::Hits(vec![hit(
            "Local paper",
            "https://smallpaper.example/z",
            "s",
        )]),
    );
    let k = kit(
        vec![
            web(
                "e1",
                100,
                Reply::Hits(vec![
                    hit("Wire", "https://www.reuters.com/x", "s"),
                    hit("Blog", "https://blog.example/y", "s"),
                ]),
            ),
            gn,
        ],
        c,
    );
    let r = run(&k, scoped("one two", "news")).await;
    let mut got = urls(&r);
    got.sort_unstable();
    assert_eq!(
        got,
        ["https://smallpaper.example/z", "https://www.reuters.com/x"]
    );
}

#[tokio::test(start_paused = true)]
async fn dates_are_labelled_and_extracted_from_snippets() {
    let mut c = cfg();
    c.default_engines = strs(&["e1"]);
    let mut api = hit("API dated", "https://a.example/1", "s");
    api.date = Some("2026-11-01".into());
    api.date_source = Some("api".into());
    let mut bare = hit("Bare date", "https://b.example/2", "s");
    bare.date = Some("2026-10-01".into());
    let k = kit(
        vec![web(
            "e1",
            100,
            Reply::Hits(vec![
                api,
                bare,
                hit(
                    "Relative",
                    "https://c.example/3",
                    "3 days ago — Rust 1.92 is out",
                ),
                hit("CJK", "https://d.example/4", "发布于2026年3月5日，新版本"),
                hit("Undated", "https://e.example/5", "nothing here"),
            ]),
        )],
        c,
    );
    let r = run(&k, req("one two")).await;
    let by = |u: &str| r.results.iter().find(|h| h.url.contains(u)).unwrap();
    assert_eq!(
        (
            by("a.example").date.as_deref(),
            by("a.example").date_source.as_deref()
        ),
        (Some("2026-11-01"), Some("structured"))
    );
    assert_eq!(by("b.example").date_source.as_deref(), Some("snippet"));
    assert_eq!(
        (
            by("c.example").date.as_deref(),
            by("c.example").date_source.as_deref()
        ),
        (Some("3 days ago"), Some("snippet"))
    );
    assert_eq!(by("d.example").date.as_deref(), Some("2026-03-05"));
    assert_eq!(
        (
            by("e.example").date.as_deref(),
            by("e.example").date_source.as_deref()
        ),
        (None, None)
    );
}

#[tokio::test(start_paused = true)]
async fn yahoo_redirects_are_unwrapped_or_skipped() {
    let mut c = cfg();
    c.default_engines = strs(&["e1"]);
    let k = kit(
        vec![web(
            "e1",
            100,
            Reply::Hits(vec![
                hit(
                    "R",
                    "https://r.search.yahoo.com/_ylt=x/RV=2/RE=1/RO=10/RU=https%3a%2f%2fexample.com%2fpage/RK=2/RS=abc-",
                    "s",
                ),
                hit(
                    "RD",
                    "https://rd.search.yahoo.com/_ylt=y/RU=https%3a%2f%2fother.example%2fdoc/RK=2/",
                    "s",
                ),
                hit("Broken", "https://r.search.yahoo.com/_ylt=z/RK=2/", "s"),
                hit("Plain", "//plain.example/x/", "s"),
            ]),
        )],
        c,
    );
    let r = run(&k, req("one two")).await;
    assert_eq!(
        urls(&r),
        [
            "https://example.com/page",
            "https://other.example/doc",
            "https://plain.example/x"
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn the_search_op_gets_query_limit_and_lang_within_its_declared_bounds() {
    let mut c = cfg();
    c.default_engines = strs(&["e1", "e2"]);
    let mut d = decl("e1", &["web"]);
    let op = OpDecl::new("search", "s", ReturnKind::Hits)
        .with_param(Param::new("query", ParamType::String, "query"))
        .with_param(Param::new("limit", ParamType::Int, "limit").with_range(1.0, 20.0))
        .with_param(Param::new("lang", ParamType::String, "lang"));
    d = d.with_op(op);
    let e1 = Fake::new(d, 100, hits("a", 2));
    let e2 = web("e2", 100, hits("b", 2));
    let k = kit(vec![e1.clone(), e2.clone()], c);
    let mut r = req("上海 天气");
    r.limit = 30;
    run(&k, r).await;
    assert_eq!(
        e1.seen()[0].params,
        serde_json::json!({"query": "上海 天气", "limit": 20, "lang": "zh"})
    );
    assert_eq!(
        e2.seen()[0].params,
        serde_json::json!({"query": "上海 天气", "limit": 30})
    );
    assert_eq!(e2.seen()[0].timeout, Duration::from_secs(5));
}

// --------------------------------------------------------------- cache ---

#[tokio::test(start_paused = true)]
async fn a_cache_hit_reports_the_engines_that_produced_it() {
    // tests/test_pool_selection.py (same name) + test_freshness_signals.py::test_a_replayed_answer_says_how_old_it_is
    let e1 = web("e1", 100, hits("a", 3));
    let k = kit(
        vec![
            e1.clone(),
            web("e2", 100, hits("b", 3)),
            web("e3", 100, hits("c", 3)),
        ],
        cfg(),
    );
    let first = run(&k, req("one two")).await;
    assert!(!first.cached);
    assert_eq!(k.store.puts.lock().unwrap()[0].1, 6 * 3600, "web TTL");
    tokio::time::advance(Duration::from_secs(90)).await;
    let second = run(&k, req("one two")).await;
    assert!(second.cached);
    assert_eq!(second.cache_age_s, Some(90));
    assert_eq!(second.engines, first.engines);
    assert_eq!(second.results, first.results);
    assert_eq!(e1.calls(), 1);
    // max_age_hours = 0 skips the read but still writes.
    let mut fresh = req("one two");
    fresh.max_age_hours = Some(0.0);
    assert!(!run(&k, fresh).await.cached);
    assert_eq!(e1.calls(), 2);
    assert_eq!(k.store.puts.lock().unwrap().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_degraded_answer_is_replayed_for_an_hour_not_a_week() {
    // tests/test_pool_selection.py (same name) + test_a_healthy_answer_is_not_marked_degraded
    let mut c = cfg();
    c.hedge_enabled = false;
    let k = kit(
        vec![
            web("e1", 100, hits("a", 3)),
            web("e2", 100, Reply::Fail(walled)),
            web("e3", 100, hits("c", 3)),
        ],
        c,
    );
    run(&k, req("one two")).await;
    assert_eq!(k.store.puts.lock().unwrap()[0].1, 3600);
    let k = kit(
        vec![
            web("e1", 100, hits("a", 3)),
            web("e2", 100, hits("b", 3)),
            web("e3", 100, hits("c", 3)),
        ],
        cfg(),
    );
    run(&k, req("one two")).await;
    assert_eq!(k.store.puts.lock().unwrap()[0].1, 6 * 3600);
}

#[tokio::test(start_paused = true)]
async fn a_quorum_cut_answer_is_not_degraded() {
    // Two fast engines meet the quorum; the grace cuts the two slow ones. They
    // were not lost, so the answer keeps the full web TTL.
    let k = kit(
        vec![
            web("e1", 300, hits("a", 10)),
            web("e2", 300, hits("b", 10)),
            web("e3", 3000, hits("c", 10)),
            web("e4", 3000, hits("d", 10)),
        ],
        cfg(),
    );
    let r = run(&k, req("one two")).await;
    assert_eq!(r.elapsed_ms, 700);
    assert_eq!(k.store.puts.lock().unwrap()[0].1, 6 * 3600);
}

#[tokio::test(start_paused = true)]
async fn ttls_follow_freshness_scope_and_sources() {
    let three = || {
        vec![
            web("e1", 100, hits("a", 3)),
            web("e2", 100, hits("b", 3)),
            web("e3", 100, hits("c", 3)),
        ]
    };
    let k = kit(three(), cfg());
    let mut day = req("one two");
    day.freshness = Some(Freshness::Day);
    run(&k, day).await;
    run(&k, scoped("one two", "weather")).await;
    run(&k, scoped("one two", "finance.fx")).await;
    assert_eq!(
        k.store
            .puts
            .lock()
            .unwrap()
            .iter()
            .map(|(_, t)| *t)
            .collect::<Vec<_>>(),
        [3600, 3600, 3600],
        "freshness=day, weather and FX: 1 h"
    );
    // A local claimant (worldclock) is never cached, so never read either.
    let clock = with(
        Fake::new(decl("clock", &["reference"]), 100, hits("t", 1)),
        |f| {
            f.claim = 1.0;
            f.decl.transport = TransportProfile::Local;
        },
    );
    let mut sources = three();
    sources.push(clock.clone());
    let k = kit(sources, cfg());
    run(&k, req("time in tokyo")).await;
    run(&k, req("time in tokyo")).await;
    assert_eq!(clock.calls(), 2);
    assert!(k.store.puts.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn rescue_provenance_survives_the_cache_round_trip() {
    // tests/test_rescue.py (same name): the replay still says who answered and why.
    let mut c = cfg();
    c.default_engines = strs(&["e1"]);
    let r1 = web("r1", 100, hits("r1", 3));
    let k = kit(vec![web("e1", 100, Reply::Fail(walled)), r1.clone()], c);
    let fresh = run(&k, req("one two")).await;
    let again = run(&k, req("one two")).await;
    assert!(again.cached);
    assert_eq!(r1.calls(), 1);
    assert_eq!(again.engines, fresh.engines);
    note_with(&again, "e1 was captcha-gated → served via r1");
}

#[tokio::test(start_paused = true)]
async fn augmenting_categories_still_rescue() {
    // tests/test_rescue.py (same name): a non-exclusive scope (news) keeps the
    // general pool, so it keeps hedging too.
    let mut c = cfg();
    c.default_engines = strs(&["e1"]);
    let r1 = web("r1", 100, hits("r1", 3));
    let k = kit(vec![web("e1", 100, Reply::Hits(Vec::new())), r1.clone()], c);
    run(&k, scoped("one two", "news")).await;
    assert_eq!(r1.calls(), 1);
}

#[tokio::test(start_paused = true)]
async fn an_empty_answer_is_not_cached() {
    let mut c = cfg();
    c.hedge_enabled = false;
    let k = kit(vec![web("e1", 100, Reply::Hits(Vec::new()))], c);
    let r = run(&k, req("one two")).await;
    assert!(r.results.is_empty());
    assert!(k.store.puts.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn concurrent_identical_searches_share_one_run() {
    let e1 = web("e1", 300, hits("a", 3));
    let k = kit(vec![e1.clone(), web("e2", 300, hits("b", 3))], cfg());
    let cx = CallCx::default();
    let (a, b) = tokio::join!(
        k.svc.search(req("one two"), &cx),
        k.svc.search(req("one two"), &cx)
    );
    assert_eq!(a.unwrap().results, b.unwrap().results);
    assert_eq!(e1.calls(), 1, "single-flight");
}

// ------------------------------------------------------- stray scopes ---

#[tokio::test(start_paused = true)]
async fn invalid_requests_are_rejected_with_a_fix() {
    let k = kit(vec![web("e1", 100, hits("a", 3))], cfg());
    let cx = CallCx::default();
    let e = k.svc.search(req("   "), &cx).await.unwrap_err();
    assert!(matches!(e, FsmError::InvalidInput(_)));
    let e = k
        .svc
        .search(scoped("x", "nonsense"), &cx)
        .await
        .unwrap_err();
    assert!(
        matches!(&e, FsmError::InvalidInput(m) if m.contains("valid scopes: web, academic")),
        "{e:?}"
    );
    let e = k.svc.search(scoped("x", "catalog"), &cx).await.unwrap_err();
    assert!(
        matches!(&e, FsmError::InvalidInput(m) if m.contains("catalog.search")),
        "{e:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn the_cache_scope_searches_pages_fetched_earlier() {
    let mut page = CacheHit::default();
    page.url = "https://doc.rust-lang.org/book/ch04-01-what-is-ownership.html".into();
    page.title = "What is Ownership?".into();
    page.snippet = "Ownership is a set of rules".into();
    page.sitename = Some("The Rust Book".into());
    let store = MemStore {
        pages: vec![page],
        ..MemStore::default()
    };
    let e1 = web("e1", 100, hits("a", 3));
    let k = kit_with(vec![e1.clone()], cfg(), store);
    let r = run(&k, scoped("ownership", "local_cache")).await;
    assert_eq!(r.scope, "cache");
    assert_eq!(r.engines, ["cache"]);
    assert_eq!(r.results[0].title, "What is Ownership?");
    assert_eq!(
        r.results[0].extra,
        Some(serde_json::json!({"sitename": "The Rust Book"}))
    );
    assert_eq!(e1.calls(), 0);
}

#[tokio::test(start_paused = true)]
async fn limit_is_clamped_and_the_cut_backfills_after_dedup() {
    let mut c = cfg();
    c.default_engines = strs(&["e1"]);
    let dupes = Reply::Hits(vec![
        hit(
            "How async works in Python",
            "https://www.example.com/post",
            "s",
        ),
        hit(
            "How async works in Python",
            "https://amp.example.com/post",
            "s",
        ),
        hit(
            "Totally different unique article",
            "https://other.com/a",
            "s",
        ),
        hit("Another distinct article here", "https://third.com/b", "s"),
    ]);
    let k = kit(vec![web("e1", 100, dupes)], c);
    let mut r = req("one two");
    r.limit = 3;
    let r = run(&k, r).await;
    assert_eq!(
        urls(&r),
        [
            "https://www.example.com/post",
            "https://other.com/a",
            "https://third.com/b"
        ]
    );
    let mut zero = req("one two");
    zero.limit = 0;
    assert_eq!(run(&k, zero).await.results.len(), 1);
}

// ----------------------------------------- panic isolation & concurrency ---

#[tokio::test(start_paused = true)]
async fn a_panicking_source_call_is_isolated_and_hedged() {
    let e1 = web("e1", 50, Reply::Panic("unexpected parser bug"));
    let e2 = web("e2", 100, hits("b", 4));
    let r1 = web("r1", 100, hits("r", 4));
    let mut c = cfg();
    c.default_engines = strs(&["e1", "e2"]);
    let k = kit(vec![e1.clone(), e2, r1.clone()], c);
    let r = run(&k, req("one two")).await;
    assert_eq!(r.results.len(), 8);
    assert_eq!(r1.seen()[0].at, Duration::from_millis(50), "hedged at once");
    note_with(&r, "Not answered: e1 (");
    assert_eq!(k.svc.benched("e1"), None, "one panic is a soft strike");
    run(&k, req("three four")).await;
    assert_eq!(k.svc.benched("e1").unwrap().reason, "error");
}

#[tokio::test(start_paused = true)]
async fn a_panicking_availability_is_isolated_during_planning() {
    let broken = with(web("e1", 50, hits("a", 3)), |f| f.panic_avail = true);
    let e2 = web("e2", 50, hits("b", 3));
    let mut c = cfg();
    c.default_engines = strs(&["e1", "e2"]);
    let k = kit(vec![broken.clone(), e2], c);
    let r = run(&k, req("one two")).await;
    assert_eq!(r.engines, ["e2"]);
    assert_eq!(r.results.len(), 3);
    assert_eq!(broken.calls(), 0);
}

#[tokio::test(start_paused = true)]
async fn single_flight_leader_cancellation_does_not_fail_waiter() {
    let e1 = web("e1", 1000, hits("a", 3));
    let mut c = cfg();
    c.default_engines = strs(&["e1"]);
    c.hedge_enabled = false;
    let k = kit(vec![e1.clone()], c);
    let cx_a = CallCx::default();
    let cx_b = CallCx::default();
    let cancel_a = cx_a.cancel.clone();
    let fut_a = k.svc.search(req("one two"), &cx_a);
    let fut_b = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        k.svc.search(req("one two"), &cx_b).await
    };
    let canceller = async {
        tokio::time::sleep(Duration::from_millis(400)).await;
        cancel_a.cancel();
    };
    let (res_a, res_b, ()) = tokio::join!(fut_a, fut_b, canceller);
    assert!(matches!(res_a.unwrap_err(), FsmError::Timeout(0)));
    let resp_b = res_b.expect("waiter retries and succeeds after leader cancels");
    assert_eq!(resp_b.results.len(), 3);
    assert_eq!(
        e1.dropped(),
        1,
        "leader's abandoned engine call was dropped"
    );
}

#[tokio::test(start_paused = true)]
async fn single_flight_waiter_cancellation_does_not_interrupt_leader() {
    let e1 = web("e1", 1000, hits("a", 3));
    let mut c = cfg();
    c.default_engines = strs(&["e1"]);
    c.hedge_enabled = false;
    let k = kit(vec![e1.clone()], c);
    let cx_a = CallCx::default();
    let cx_b = CallCx::default();
    let cancel_b = cx_b.cancel.clone();
    let fut_a = k.svc.search(req("one two"), &cx_a);
    let fut_b = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        k.svc.search(req("one two"), &cx_b).await
    };
    let canceller = async {
        tokio::time::sleep(Duration::from_millis(400)).await;
        cancel_b.cancel();
    };
    let (res_a, res_b, ()) = tokio::join!(fut_a, fut_b, canceller);
    assert!(matches!(res_b.unwrap_err(), FsmError::Timeout(0)));
    let resp_a = res_a.expect("leader finishes unaffected");
    assert_eq!(resp_a.results.len(), 3);
    assert_eq!(e1.calls(), 1);
    assert_eq!(e1.dropped(), 0);
}

#[tokio::test(start_paused = true)]
async fn single_flight_short_budget_caller_does_not_poison_normal_caller() {
    let e1 = slow_web("e1", 3000, hits("a", 3));
    let mut c = cfg();
    c.default_engines = strs(&["e1"]);
    c.hedge_enabled = false;
    let k = kit(vec![e1], c);
    let short_cx = CallCx::new(
        "short",
        std::time::Instant::now() + Duration::from_millis(500),
    );
    let normal_cx = CallCx::default();
    let (short_res, normal_res) = tokio::join!(k.svc.search(req("one two"), &short_cx), async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        k.svc.search(req("one two"), &normal_cx).await
    });
    assert!(short_res.unwrap().results.is_empty());
    assert_eq!(
        normal_res.unwrap().results.len(),
        3,
        "normal-budget caller was not poisoned by short-budget caller"
    );
}

#[tokio::test(start_paused = true)]
async fn single_flight_leaves_no_stale_window_for_force_refresh() {
    let e1 = web("e1", 100, hits("a", 3));
    let mut c = cfg();
    c.default_engines = strs(&["e1"]);
    c.hedge_enabled = false;
    let k = kit(vec![e1.clone()], c);
    let r1 = run(&k, req("one two")).await;
    assert!(!r1.cached);
    let mut fresh = req("one two");
    fresh.max_age_hours = Some(0.0);
    let r2 = run(&k, fresh).await;
    assert!(!r2.cached);
    assert_eq!(e1.calls(), 2);
}

#[tokio::test(start_paused = true)]
async fn straggler_tokens_are_cancelled_before_futures_are_dropped() {
    let e3 = web("e3", 3000, hits("c", 10));
    let mut c = cfg();
    c.default_engines = strs(&["e1", "e2", "e3"]);
    let k = kit(
        vec![
            web("e1", 300, hits("a", 10)),
            web("e2", 300, hits("b", 10)),
            e3.clone(),
        ],
        c,
    );
    let r = run(&k, req("one two")).await;
    assert_eq!(r.elapsed_ms, 700);
    assert_eq!(e3.dropped(), 1);
    assert_eq!(
        e3.cancelled_at_drop(),
        1,
        "CancellationToken must be cancelled before the straggler future is dropped"
    );
}

#[tokio::test(start_paused = true)]
async fn early_timeout_from_capped_hedge_is_failed_not_hard_cut() {
    fn early_timeout(_id: &str) -> FsmError {
        FsmError::Timeout(500)
    }
    let mut c = cfg();
    c.default_engines = strs(&["e1", "e2"]);
    c.reserve_engines = strs(&["r1"]);
    // r1 has a 20 s own timeout, so hedging at 1.8 s caps it to 6.2 s (< 20 s).
    // Failing with Timeout(500) at 500 ms is its own early timeout, not Cut::Hard.
    let r1 = slow_web("r1", 500, Reply::Fail(early_timeout));
    let k = kit(
        vec![
            web("e1", 100, Reply::Hits(Vec::new())),
            web("e2", 100, hits("b", 2)),
            r1,
        ],
        c,
    );
    run(&k, req("one two")).await;
    assert_eq!(k.svc.benched("r1"), None, "one timeout is a soft strike");
    run(&k, req("three four")).await;
    assert_eq!(
        k.svc.benched("r1").unwrap().reason,
        "timeout",
        "second early timeout benches r1"
    );
}

// ------------------------------------ routing, breaker & param edge cases ---

#[tokio::test(start_paused = true)]
async fn scope_only_sources_take_priority_over_unclaimed_direct_claimants() {
    let direct = |id: &str, scope: &str, claim: f32| {
        let mut d = decl(id, &[scope]);
        d.pool_role = PoolRole::DirectClaimant;
        with(Fake::new(d, 50, hits(id, 1)), |f| f.claim = claim)
    };
    let scope_only = |id: &str, scope: &str| Fake::new(decl(id, &[scope]), 50, hits(id, 1));
    let mut c = cfg();
    c.default_engines = strs(&["e1"]);
    c.scope_engine_limit = 3;
    // Register 3 unclaimed DirectClaimants BEFORE 2 ScopeOnly sources on "code".
    let k = kit(
        vec![
            web("e1", 50, hits("a", 2)),
            direct("dc1", "code", 0.0),
            direct("dc2", "code", 0.0),
            direct("dc3", "code", 0.0),
            scope_only("gh", "code"),
            scope_only("se", "code"),
            // Security has ONLY DirectClaimant sources.
            direct("osv", "security", 0.0),
            direct("kev", "security", 0.0),
        ],
        c.clone(),
    );
    let r = run(&k, scoped("rust async trait", "code")).await;
    assert!(r.engines.contains(&"gh".to_owned()), "{:?}", r.engines);
    assert!(r.engines.contains(&"se".to_owned()), "{:?}", r.engines);
    // Unclaimed DirectClaimants fill only leftover slots after ScopeOnly.
    assert_eq!(r.engines, ["e1", "gh", "se", "dc1"]);
    // A scope whose only sources are DirectClaimant still seats them.
    let r_sec = run(&k, scoped("apache httpd", "security")).await;
    assert_eq!(r_sec.engines, ["e1", "osv", "kev"]);

    // When a DirectClaimant actually claims the query, it leads before ScopeOnly.
    let k2 = kit(
        vec![
            web("e1", 50, hits("a", 2)),
            direct("dc1", "code", 0.0),
            direct("dc3", "code", 1.0),
            scope_only("gh", "code"),
            scope_only("se", "code"),
        ],
        c,
    );
    let r_claimed = run(&k2, scoped("fastapi latest version", "code")).await;
    assert_eq!(r_claimed.engines, ["e1", "dc3", "gh", "se"]);
}

#[tokio::test(start_paused = true)]
async fn exclusive_scope_ignores_breaker_when_all_specialists_are_benched() {
    let ds1 = Fake::new(decl("ds1", &["dataset"]), 50, Reply::Fail(walled));
    let ds2 = Fake::new(decl("ds2", &["dataset"]), 50, Reply::Fail(walled));
    let k = kit(vec![ds1.clone(), ds2.clone()], cfg());
    run(&k, scoped("imagenet labels", "dataset")).await;
    assert!(k.svc.benched("ds1").is_some());
    assert!(k.svc.benched("ds2").is_some());
    // Now recover both: the next search must NOT run 0 seats.
    ds1.set(50, hits("ds1", 2));
    ds2.set(50, hits("ds2", 2));
    let r = run(&k, scoped("coco dataset", "dataset")).await;
    assert_eq!(r.engines, ["ds1", "ds2"]);
    assert_eq!(r.results.len(), 4);
}

#[tokio::test(start_paused = true)]
async fn japanese_query_elevates_yahoojp_to_native_weight() {
    let mut c = cfg();
    c.default_engines = strs(&["e1", "yahoojp"]);
    c.hedge_enabled = false;
    let k = kit(
        vec![
            web(
                "e1",
                50,
                Reply::Hits(vec![hit("General hit", "https://gen.example/1", "s")]),
            ),
            web(
                "yahoojp",
                50,
                Reply::Hits(vec![hit("JP hit", "https://jp.example/1", "s")]),
            ),
        ],
        c,
    );
    // Japanese script elevates yahoojp to 2.0x weight over e1 (1.0x).
    let r = run(&k, req("東京の天気")).await;
    assert_eq!(r.results[0].url, "https://jp.example/1");
    assert!(r.results[0].score > r.results[1].score);
}

#[tokio::test(start_paused = true)]
async fn lang_param_with_enum_values_is_omitted_when_unmatched() {
    let mut c = cfg();
    c.default_engines = strs(&["e1"]);
    c.hedge_enabled = false;
    let mut lp = Param::new("lang", ParamType::String, "lang");
    lp.enum_values = Some(strs(&["en", "fr"]));
    let op = OpDecl::new("search", "s", ReturnKind::Hits)
        .with_param(Param::new("query", ParamType::String, "query"))
        .with_param(lp);
    let e1 = Fake::new(decl("e1", &["web"]).with_op(op), 50, hits("a", 2));
    let k = kit(vec![e1.clone()], c);
    run(&k, req("上海 天气")).await;
    assert!(
        e1.seen()[0].params.get("lang").is_none(),
        "unsupported lang 'zh' must not be sent: {:?}",
        e1.seen()[0].params
    );
    let mut fr = req("paris");
    fr.lang = Some("fr".into());
    run(&k, fr).await;
    assert_eq!(e1.seen()[1].params["lang"], "fr");
}

#[tokio::test(start_paused = true)]
async fn fresh_extra_matching_scope_gets_native_weight_and_bypasses_host_filter() {
    let mut c = cfg();
    c.default_engines = strs(&["e1"]);
    c.scope_engine_limit = 1;
    c.fresh_engines = strs(&["gn"]);
    c.hedge_enabled = false;
    let n1 = Fake::new(
        decl("n1", &["news"]),
        50,
        Reply::Hits(vec![hit("N1 story", "https://n1.example/a", "s")]),
    );
    let gn = Fake::new(
        decl("gn", &["news"]),
        50,
        Reply::Hits(vec![hit(
            "Local outlet",
            "https://smallpaper.example/b",
            "s",
        )]),
    );
    let k = kit(
        vec![
            web(
                "e1",
                50,
                Reply::Hits(vec![hit("Wire", "https://www.reuters.com/c", "s")]),
            ),
            n1,
            gn,
        ],
        c,
    );
    let mut r = scoped("one two", "news");
    r.freshness = Some(Freshness::Day);
    let r = run(&k, r).await;
    assert!(
        urls(&r).contains(&"https://smallpaper.example/b"),
        "gn added via fresh_engines still counts as native on scope=news"
    );
    let gn_hit = r
        .results
        .iter()
        .find(|h| h.url.contains("smallpaper"))
        .unwrap();
    let e1_hit = r
        .results
        .iter()
        .find(|h| h.url.contains("reuters"))
        .unwrap();
    assert!(
        gn_hit.score > e1_hit.score,
        "gn gets native weight 2.0 > general 1.0"
    );
}
