//! The fan-out race (quorum, grace, hedging, deadlines) and the merge of
//! what came back (coherence guard, fusion, lead, health outcomes).
//!
//! Timeline of one search (`SPEC.md` §5), all on `tokio::time`:
//!
//! * t = 0: every seat is dispatched, each under its own timeout
//!   (`REQUEST_TIMEOUT` for web engines, 4 s for APIs, or the source's own).
//! * A general engine that fails, is walled, returns 0 results or answers
//!   off-topic dispatches the first reserve at once; so does "fewer than 2
//!   sources answered" at 1.8 s. The second reserve follows 0.8 s after the
//!   first if fewer than 2 sources have answered by then, or at once when
//!   everything settled that thin (at most 2 hedges per search).
//! * Quorum = ≥ 2 sources answered with usable results AND the deduped merge
//!   holds ≥ `limit` hits AND every direct claimant has settled: a 400 ms grace
//!   window opens, then stragglers are cancelled without a health penalty.
//! * Soft deadline 4.0 s: return as soon as anything usable is in.
//! * Hard deadline `SEARCH_DEADLINE_SECONDS` (8.0 s) or the caller's budget.

use crate::coherence::{is_witness, looks_like_decoy};
use crate::filters::{self, Drops, Filters, HostClass};
use crate::plan::Seat;
use crate::rank::{Weighting, fuse};
use fsm_core::dates::extract_date_hint;
use fsm_core::ext::w1a::wall_reason;
use fsm_core::url_canon::{normalize_url, resolve_yahoo_url};
use fsm_core::{CancellationToken, Config, Freshness, FsmError, Hit, HitKind, SrcCx, SrcOutput};
use futures::FutureExt;
use futures::future::BoxFuture;
use futures::stream::{FuturesUnordered, StreamExt};
use serde_json::{Map, Value, json};
use std::collections::VecDeque;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{Instant, sleep_until};

/// "Fewer than 2 answered" is checked at this point.
pub const HEDGE_AFTER: Duration = Duration::from_millis(1800);
/// Gap between the first and the second hedge.
pub const HEDGE_STAGGER: Duration = Duration::from_millis(800);
/// Reserves one search may add.
pub const MAX_HEDGES: usize = 2;
/// Stragglers get this long once the quorum is met.
pub const GRACE: Duration = Duration::from_millis(400);
/// Maximum wait for at least one scope-native specialist (e.g. `sec_edgar`, `github`,
/// `googlenews`) to answer before generic web quorum opens early grace.
pub const NATIVE_WAIT: Duration = Duration::from_millis(2600);
/// After this, the first usable answer ends the search.
pub const SOFT_DEADLINE: Duration = Duration::from_millis(4000);
/// Sources that must have answered for the quorum.
pub const QUORUM: usize = 2;
/// Most results the kernel keeps per source and per search.
pub const MAX_LIMIT: usize = 50;

/// The deadlines of one run.
#[derive(Debug, Clone, Copy)]
pub struct Deadlines {
    pub hedge_after: Duration,
    pub stagger: Duration,
    pub grace: Duration,
    pub soft: Duration,
    pub hard: Duration,
}

impl Deadlines {
    /// The spec's timings, the hard deadline capped by `budget` (what is left
    /// of the caller's own deadline).
    pub fn new(cfg: &Config, budget: Duration) -> Self {
        let hard = Duration::from_secs_f64(cfg.search_deadline_s.clamp(0.5, 120.0)).min(budget);
        Self {
            hedge_after: HEDGE_AFTER,
            stagger: HEDGE_STAGGER,
            grace: GRACE,
            soft: SOFT_DEADLINE.min(hard),
            hard,
        }
    }
}

/// Why the kernel stopped waiting for a source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cut {
    /// The quorum's grace window closed.
    Grace,
    /// The soft deadline passed with an answer in hand.
    Soft,
    /// The hard deadline passed.
    Hard,
    /// The caller cancelled.
    Cancelled,
}

/// How one source's part of the search ended.
#[derive(Debug)]
pub enum Outcome {
    /// It answered. `hits` are cleaned and filtered; `raw` counts what it
    /// returned before the filters; `suspect` = coherence says decoy.
    Answered {
        hits: Vec<Hit>,
        raw: usize,
        drops: Drops,
        suspect: bool,
    },
    Failed(FsmError),
    /// Its own timeout passed.
    TimedOut(Duration),
    Cut(Cut),
}

/// Why a reserve was dispatched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HedgeWhy {
    /// Fewer than [`QUORUM`] sources had answered at [`HEDGE_AFTER`].
    Slow,
    /// A general engine failed (id, how).
    Lost(String, String),
    /// Second hedge: still fewer than [`QUORUM`] usable answers.
    Stagger,
    /// Everything settled with fewer than [`QUORUM`] usable answers.
    Thin,
}

impl HedgeWhy {
    pub fn describe(&self) -> String {
        match self {
            Self::Slow => format!(
                "fewer than {QUORUM} sources had answered after {:.1} s",
                HEDGE_AFTER.as_secs_f64()
            ),
            Self::Lost(id, how) => format!("{id} {how}"),
            Self::Stagger => "the pool was still short after the first reserve".to_owned(),
            Self::Thin => format!("fewer than {QUORUM} sources returned usable results"),
        }
    }
}

/// One source of the run.
#[derive(Debug)]
pub struct Lane {
    pub seat: Seat,
    pub hedge: Option<HedgeWhy>,
    pub outcome: Outcome,
    /// Since the start of the run.
    pub dispatched_at: Duration,
    pub elapsed: Duration,
}

impl Lane {
    /// Answered with usable (filtered, non-decoy) results.
    pub fn usable(&self) -> bool {
        matches!(&self.outcome, Outcome::Answered { hits, suspect: false, .. } if !hits.is_empty())
    }
}

/// The run's result: lanes in dispatch order.
#[derive(Debug)]
pub struct Run {
    pub lanes: Vec<Lane>,
    pub cut: Option<Cut>,
}

/// How a failure counts against a source's health.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Not the source's fault (configuration, missing browser, cancellation).
    NoStrike,
    /// A deliberate wall (`captcha`, `consent`, ...).
    Wall(String),
    /// A refusal or blip: `http_429`, `http_503`, `timeout`, `error`.
    Strike(String),
}

/// `_record_health`'s view of an error.
pub fn verdict(e: &FsmError) -> Verdict {
    match e {
        FsmError::InvalidInput(_)
        | FsmError::AuthRequired { .. }
        | FsmError::PermissionDenied(_)
        | FsmError::UnsafeUrl(_)
        | FsmError::BrowserUnavailable(_)
        | FsmError::Timeout(0) => Verdict::NoStrike,
        FsmError::Timeout(_) => Verdict::Strike("timeout".to_owned()),
        FsmError::RateLimited { .. } => Verdict::Strike("http_429".to_owned()),
        FsmError::Upstream {
            status, message, ..
        } => {
            if let Some(r) = wall_reason(e) {
                return Verdict::Wall(r.to_owned());
            }
            let low = message.to_lowercase();
            let wall = [
                ("captcha", "captcha"),
                ("unusual traffic", "captcha"),
                ("anomaly", "captcha"),
                ("are you a robot", "captcha"),
                ("consent", "consent"),
                ("enable javascript", "javascript"),
                ("javascript is required", "javascript"),
                ("sign in to continue", "login"),
                ("login required", "login"),
                ("not available in your region", "region"),
            ]
            .iter()
            .find(|(m, _)| low.contains(m))
            .map(|(_, r)| (*r).to_owned());
            match (wall, status) {
                (Some(r), _) => Verdict::Wall(r),
                (None, Some(s)) if *s >= 400 => Verdict::Strike(format!("http_{s}")),
                (None, _) => Verdict::Strike("error".to_owned()),
            }
        }
        _ => Verdict::Strike("error".to_owned()),
    }
}

/// Everything the race and the merge need to know about the request.
pub struct Ctx<'a> {
    pub query: &'a str,
    pub limit: usize,
    pub cfg: &'a Config,
    pub lang: Option<String>,
    pub freshness: Option<Freshness>,
    pub domains: Vec<String>,
    pub filters: Filters,
    pub host_class: Option<HostClass>,
    pub weighting: &'a Weighting,
    pub today: jiff::civil::Date,
    pub coherence: bool,
}

impl Ctx<'_> {
    /// Results requested from (and kept per) source: enough candidates for
    /// fusion and dedup backfill, never more than [`MAX_LIMIT`].
    fn per_source(&self) -> usize {
        self.limit
            .max(self.cfg.max_results_per_engine as usize)
            .clamp(1, MAX_LIMIT)
    }

    /// Which search op to call on `seat`: `"recent"` when freshness is set and
    /// the source declares a `recent` search-by-date op (e.g. `hackernews.recent`)
    /// while `search` has no `freshness`/`timespan` param.
    fn search_op_name<'s>(&self, seat: &'s Seat) -> &'s str {
        let decl = seat.src.decl();
        if self.freshness.is_some()
            && let Some(search_op) = decl.op("search")
            && !search_op
                .params
                .iter()
                .any(|p| p.name == "freshness" || p.name == "timespan")
            && let Some(recent_op) = decl.op("recent")
            && recent_op.params.iter().any(|p| p.name == "query")
        {
            return "recent";
        }
        "search"
    }

    /// `{"query"}` plus `limit` (clamped to the op's bounds) when the op
    /// declares it or declares nothing, plus `lang`, `timespan`, and `freshness`
    /// when declared.
    pub fn params(&self, seat: &Seat) -> Value {
        let mut p = Map::new();
        p.insert("query".into(), json!(self.query));
        let want = self.per_source() as f64;
        let op_name = self.search_op_name(seat);
        match seat.src.decl().op(op_name) {
            Some(op) if !op.params.is_empty() => {
                if let Some(lp) = op.params.iter().find(|p| p.name == "limit") {
                    let mut v = want;
                    if let Some(max) = lp.max {
                        v = v.min(max);
                    }
                    if let Some(min) = lp.min {
                        v = v.max(min);
                    }
                    p.insert("limit".into(), json!(v.round() as i64));
                }
                if let Some(lang) = &self.lang
                    && let Some(lp) = op.params.iter().find(|p| p.name == "lang")
                    && lp
                        .enum_values
                        .as_ref()
                        .is_none_or(|ev| ev.iter().any(|v| v == lang))
                {
                    p.insert("lang".into(), json!(lang));
                }
                if let Some(fresh) = self.freshness {
                    if let Some(tp) = op.params.iter().find(|p| p.name == "timespan") {
                        let ts = match fresh {
                            Freshness::Day => "1d",
                            Freshness::Week => "1w",
                            Freshness::Month => "1m",
                            _ => "12m",
                        };
                        if tp
                            .enum_values
                            .as_ref()
                            .is_none_or(|ev| ev.iter().any(|v| v == ts))
                        {
                            p.insert("timespan".into(), json!(ts));
                        }
                    }
                    if let Some(fp) = op.params.iter().find(|p| p.name == "freshness") {
                        let val = if fp.doc.contains("interval=") {
                            match fresh {
                                Freshness::Day => r#"interval="4""#,
                                Freshness::Week => r#"interval="7""#,
                                _ => r#"interval="9""#,
                            }
                        } else {
                            match fresh {
                                Freshness::Day => "day",
                                Freshness::Week => "week",
                                Freshness::Month => "month",
                                _ => "year",
                            }
                        };
                        if fp
                            .enum_values
                            .as_ref()
                            .is_none_or(|ev| ev.iter().any(|v| v == val))
                        {
                            p.insert("freshness".into(), json!(val));
                        }
                    }
                }
            }
            _ => {
                p.insert("limit".into(), json!(want as i64));
            }
        }
        Value::Object(p)
    }

    fn launch(
        &self,
        seat: &Seat,
        cancel: CancellationToken,
        timeout: Duration,
    ) -> BoxFuture<'static, Result<SrcOutput, FsmError>> {
        let src = Arc::clone(&seat.src);
        let id = seat.id.clone();
        let op_name = self.search_op_name(seat).to_owned();
        let params = self.params(seat);
        let mut scx = SrcCx::from_config(self.cfg, cancel, timeout);
        scx.lang = self.lang.clone();
        scx.freshness = self.freshness;
        scx.domains = self.domains.clone();
        let envs = seat.src.decl().key_envs();
        if let [env] = envs.as_slice() {
            scx.api_key = self.cfg.api_key(env).map(str::to_owned);
        }
        Box::pin(async move {
            match AssertUnwindSafe(src.call(&op_name, params, &scx))
                .catch_unwind()
                .await
            {
                Ok(res) => res,
                Err(payload) => {
                    let msg = panic_message(&*payload);
                    tracing::warn!(source = %id, panic = %msg, "source call() panicked");
                    Err(FsmError::internal(
                        "search",
                        format!("source panicked: {msg}"),
                    ))
                }
            }
        })
    }

    /// Clean one source's output: unwrap Yahoo & Bing News redirects, normalise URLs,
    /// label dates (extracting EN+CJK hints from snippets when missing), then
    /// apply the request's filters and the per-source cap.
    pub fn prepare(&self, seat: &Seat, out: SrcOutput) -> (Vec<Hit>, usize, Drops) {
        let mut clean = Vec::new();
        for mut h in to_hits(out, &seat.id) {
            let Some(url) = resolve_yahoo_url(&h.url) else {
                continue;
            };
            let url = unwrap_bing_news_redirect(&url);
            h.url = normalize_url(&url);
            if h.url.is_empty() {
                continue;
            }
            if h.source.is_empty() {
                h.source = seat.id.clone();
            }
            label_date(&mut h, self.today);
            clean.push(h);
        }
        let raw = clean.len();
        let class = if seat.guarded && !seat.native {
            self.host_class
        } else {
            None
        };
        let (mut kept, drops) = filters::apply(clean, &self.filters, class, self.today);
        kept.truncate(self.per_source());
        (kept, raw, drops)
    }

    fn suspect(&self, seat: &Seat, hits: &[Hit]) -> bool {
        self.coherence && seat.guarded && looks_like_decoy(self.query, hits)
    }

    fn quorum(&self, lanes: &[Option<Lane>]) -> bool {
        let settled = lanes.iter().flatten();
        let usable: Vec<(String, Vec<Hit>)> = settled
            .clone()
            .filter(|l| l.usable())
            .map(|l| match &l.outcome {
                Outcome::Answered { hits, .. } => (l.seat.id.clone(), hits.clone()),
                _ => unreachable!("usable() implies Answered"),
            })
            .collect();
        usable.len() >= QUORUM && fuse(&usable, self.weighting, self.limit).len() == self.limit
    }
}

fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_owned()
    }
}

/// Unwrap Bing News RSS redirect links (`https://www.bing.com/news/apiclick.aspx?...&url=<target>`)
/// to their real publisher URL so host filters and cross-engine deduplication see the target site.
pub fn unwrap_bing_news_redirect(raw: &str) -> String {
    let Ok(parsed) = url::Url::parse(raw) else {
        return raw.to_owned();
    };
    let Some(host) = parsed.host_str() else {
        return raw.to_owned();
    };
    if !(host.eq_ignore_ascii_case("www.bing.com") || host.eq_ignore_ascii_case("bing.com")) {
        return raw.to_owned();
    }
    if !parsed.path().eq_ignore_ascii_case("/news/apiclick.aspx") {
        return raw.to_owned();
    }
    for (k, v) in parsed.query_pairs() {
        if k.eq_ignore_ascii_case("url") {
            let trimmed = v.trim();
            if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
                return trimmed.to_owned();
            }
        }
    }
    raw.to_owned()
}

/// `date` + `date_source`: a missing source means scraped; a missing date
/// is looked for in the snippet, then the title.
fn label_date(h: &mut Hit, today: jiff::civil::Date) {
    h.date = h
        .date
        .take()
        .map(|d| d.trim().to_owned())
        .filter(|d| !d.is_empty());
    if h.date.is_some() {
        if h.date_source.as_deref().is_none_or(|s| s.trim().is_empty()) {
            h.date_source = Some("snippet".to_owned());
        }
        return;
    }
    h.date_source = None;
    if let Some(hint) =
        extract_date_hint(&h.snippet, today).or_else(|| extract_date_hint(&h.title, today))
    {
        h.date = Some(hint);
        h.date_source = Some("snippet".to_owned());
    }
}

fn str_field<'v>(v: &'v Value, keys: &[&str]) -> Option<&'v str> {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Truncate on a char boundary.
fn clip(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", s[..i].trim_end()),
        None => s.to_owned(),
    }
}

/// Every `SrcOutput` shape as hits: works become academic hits, records
/// without hits are synthesised from `title`/`url`/`snippet`-like fields,
/// text and raw outputs carry no hits.
pub fn to_hits(out: SrcOutput, source: &str) -> Vec<Hit> {
    match out {
        SrcOutput::Hits(v) => v,
        SrcOutput::Records { hits: Some(v), .. } => v,
        SrcOutput::Records { items, .. } => items
            .iter()
            .filter_map(|it| {
                let url = str_field(it, &["url", "link", "html_url", "href"])?;
                let title = str_field(it, &["title", "name", "label"]).unwrap_or(url);
                let mut h = Hit::new(title, url);
                h.snippet = str_field(it, &["snippet", "description", "summary", "abstract"])
                    .map(|s| clip(s, 500))
                    .unwrap_or_default();
                h.date = str_field(
                    it,
                    &[
                        "date",
                        "published",
                        "publishedDate",
                        "published_date",
                        "updated",
                        "modified",
                    ],
                )
                .map(str::to_owned);
                h.date_source = h.date.as_ref().map(|_| "api".to_owned());
                h.source = source.to_owned();
                h.kind = HitKind::Record;
                Some(h)
            })
            .collect(),
        SrcOutput::Works(works) => works
            .into_iter()
            .filter(|w| !w.url.trim().is_empty())
            .map(|w| {
                let mut h = Hit::new(w.title.clone(), w.url.clone());
                h.snippet = w
                    .abstract_text
                    .as_deref()
                    .map(|a| clip(a, 500))
                    .unwrap_or_default();
                h.date = w
                    .published_date
                    .clone()
                    .or_else(|| w.year.map(|y| y.to_string()));
                h.date_source = h.date.as_ref().map(|_| "api".to_owned());
                h.source = source.to_owned();
                h.kind = HitKind::Academic;
                h.extra = serde_json::to_value(&w.ids)
                    .ok()
                    .filter(|v| v.as_object().is_some_and(|o| !o.is_empty()));
                h
            })
            .collect(),
        _ => Vec::new(),
    }
}

type LaneFut = BoxFuture<
    'static,
    (
        usize,
        Result<Result<SrcOutput, FsmError>, tokio::time::error::Elapsed>,
    ),
>;

/// A dispatched source.
struct Pending {
    seat: Seat,
    why: Option<HedgeWhy>,
    token: CancellationToken,
    /// Since the start of the run.
    at: Duration,
    /// Its timeout for this run.
    timeout: Duration,
    /// The timeout was shortened to fit the hard deadline: running out is
    /// the run's deadline, not the source's fault.
    capped: bool,
}

struct Race<'c, 'a> {
    ctx: &'c Ctx<'a>,
    d: Deadlines,
    start: Instant,
    hard_at: Instant,
    cancel: CancellationToken,
    seats: Vec<Pending>,
    settled: Vec<Option<Lane>>,
    futs: FuturesUnordered<LaneFut>,
    reserves: VecDeque<Seat>,
    hedge: bool,
    hedges: usize,
    stagger_at: Option<Instant>,
}

impl Race<'_, '_> {
    fn dispatch(&mut self, seat: Seat, why: Option<HedgeWhy>) {
        let now = Instant::now();
        let left = self.hard_at.saturating_duration_since(now);
        let timeout = seat.timeout.min(left);
        let token = self.cancel.child_token();
        let fut = self.ctx.launch(&seat, token.clone(), timeout);
        let idx = self.seats.len();
        self.futs.push(Box::pin(async move {
            (idx, tokio::time::timeout(timeout, fut).await)
        }));
        self.seats.push(Pending {
            capped: timeout < seat.timeout,
            seat,
            why,
            token,
            at: now.saturating_duration_since(self.start),
            timeout,
        });
        self.settled.push(None);
    }

    /// Dispatch the next reserve; schedules the staggered second one.
    fn fire(&mut self, why: HedgeWhy) -> bool {
        if !self.hedge || self.hedges >= MAX_HEDGES || Instant::now() >= self.hard_at {
            return false;
        }
        let Some(seat) = self.reserves.pop_front() else {
            return false;
        };
        tracing::debug!(reserve = %seat.id, ?why, "hedging");
        self.dispatch(seat, Some(why));
        self.hedges += 1;
        self.stagger_at = (self.hedges < MAX_HEDGES && !self.reserves.is_empty())
            .then(|| Instant::now() + self.d.stagger);
        true
    }

    fn usable(&self) -> usize {
        self.settled.iter().flatten().filter(|l| l.usable()).count()
    }

    fn claimants_settled(&self) -> bool {
        self.seats
            .iter()
            .zip(&self.settled)
            .all(|(p, l)| l.is_some() || !self.ctx.weighting.direct.contains(&p.seat.id))
    }

    /// True when early grace need not wait for scope-native specialists: either
    /// this search has no native seats (e.g. `scope="web"`), at least one native
    /// seat already returned usable hits, every native seat has settled, or the
    /// bounded `native_wait_at` window has elapsed.
    fn natives_ready(&self, now: Instant, native_wait_at: Option<Instant>) -> bool {
        let Some(wait_at) = native_wait_at else {
            return true;
        };
        if now >= wait_at {
            return true;
        }
        let mut any_pending_native = false;
        for (p, done) in self.seats.iter().zip(&self.settled) {
            if !p.seat.native {
                continue;
            }
            match done {
                Some(lane) if lane.usable() => return true,
                Some(_) => {}
                None => any_pending_native = true,
            }
        }
        !any_pending_native
    }

    fn settle(
        &mut self,
        idx: usize,
        res: Result<Result<SrcOutput, FsmError>, tokio::time::error::Elapsed>,
    ) -> &Lane {
        let Pending {
            seat,
            why,
            at,
            timeout,
            capped,
            ..
        } = &self.seats[idx];
        let now = Instant::now();
        let elapsed = now
            .saturating_duration_since(self.start)
            .saturating_sub(*at);
        let outcome = match res {
            Ok(Ok(out)) => {
                let (hits, raw, drops) = self.ctx.prepare(seat, out);
                let suspect = self.ctx.suspect(seat, &hits);
                Outcome::Answered {
                    hits,
                    raw,
                    drops,
                    suspect,
                }
            }
            Ok(Err(FsmError::Timeout(ms)))
                if *capped
                    && ms > 0
                    && (now >= self.hard_at
                        || elapsed.saturating_add(Duration::from_millis(50)) >= *timeout
                        || u128::from(ms) >= timeout.as_millis()) =>
            {
                Outcome::Cut(Cut::Hard)
            }
            Ok(Err(e)) => Outcome::Failed(e),
            Err(_) if *capped => Outcome::Cut(Cut::Hard),
            Err(_) => Outcome::TimedOut(*timeout),
        };
        self.settled[idx] = Some(Lane {
            seat: seat.clone(),
            hedge: why.clone(),
            outcome,
            dispatched_at: *at,
            elapsed,
        });
        self.settled[idx].as_ref().expect("just set")
    }
}

/// How a settled general engine was lost, for a hedge note (`None` = fine).
fn lost(lane: &Lane) -> Option<String> {
    match &lane.outcome {
        Outcome::Answered { suspect: true, .. } => Some("answered off-topic".to_owned()),
        Outcome::Answered { raw: 0, .. } => Some("returned no results".to_owned()),
        Outcome::Answered { .. } | Outcome::Cut(_) => None,
        Outcome::TimedOut(_) => Some("timed out".to_owned()),
        Outcome::Failed(e) => Some(match verdict(e) {
            Verdict::Wall(r) => format!("was {r}-gated"),
            _ => "failed".to_owned(),
        }),
    }
}

enum Ev {
    Cancel,
    Done(
        usize,
        Result<Result<SrcOutput, FsmError>, tokio::time::error::Elapsed>,
    ),
    Tick,
}

/// Run the race. Hedging needs `hedge` and a reserve; `cancel` is the
/// caller's token (each source gets a child of it).
pub async fn race(
    ctx: &Ctx<'_>,
    seats: Vec<Seat>,
    reserves: Vec<Seat>,
    hedge: bool,
    d: Deadlines,
    cancel: &CancellationToken,
) -> Run {
    let start = Instant::now();
    let mut r = Race {
        ctx,
        d,
        start,
        hard_at: start + d.hard,
        cancel: cancel.clone(),
        seats: Vec::new(),
        settled: Vec::new(),
        futs: FuturesUnordered::new(),
        reserves: reserves.into(),
        hedge,
        hedges: 0,
        stagger_at: None,
    };
    for s in seats {
        r.dispatch(s, None);
    }
    let soft_at = start + d.soft;
    let native_wait_at = r
        .seats
        .iter()
        .any(|p| p.seat.native)
        .then(|| (start + NATIVE_WAIT).min(soft_at));
    let mut slow_check = (hedge && !r.reserves.is_empty()).then_some(start + d.hedge_after);
    let mut grace_at: Option<Instant> = None;
    let mut soft_passed = false;
    let cut = loop {
        if r.futs.is_empty() {
            if grace_at.is_none() && r.usable() < QUORUM && r.fire(HedgeWhy::Thin) {
                continue;
            }
            break None;
        }
        let now_before_wait = Instant::now();
        let wait_for_native = (grace_at.is_none()
            && !r.natives_ready(now_before_wait, native_wait_at))
        .then_some(native_wait_at)
        .flatten();
        let mut wake = r.hard_at;
        for t in [
            grace_at,
            slow_check,
            r.stagger_at,
            wait_for_native,
            (!soft_passed).then_some(soft_at),
        ]
        .into_iter()
        .flatten()
        {
            wake = wake.min(t);
        }
        let ev = tokio::select! {
            biased;
            () = cancel.cancelled() => Ev::Cancel,
            Some((idx, res)) = r.futs.next() => Ev::Done(idx, res),
            () = sleep_until(wake) => Ev::Tick,
        };
        let now = Instant::now();
        match ev {
            Ev::Cancel => break Some(Cut::Cancelled),
            Ev::Done(idx, res) => {
                let lane = r.settle(idx, res);
                let lost_how = (lane.seat.guarded && lane.hedge.is_none())
                    .then(|| lost(lane))
                    .flatten();
                let id = lane.seat.id.clone();
                if let Some(how) = lost_how
                    && r.hedges == 0
                    && grace_at.is_none()
                {
                    r.fire(HedgeWhy::Lost(id, how));
                }
                if grace_at.is_none()
                    && r.usable() >= QUORUM
                    && r.claimants_settled()
                    && r.natives_ready(now, native_wait_at)
                    && ctx.quorum(&r.settled)
                {
                    grace_at = Some((now + d.grace).min(r.hard_at));
                }
                if soft_passed && r.usable() >= 1 {
                    break Some(Cut::Soft);
                }
            }
            Ev::Tick => {
                if now >= r.hard_at {
                    break Some(Cut::Hard);
                }
                if grace_at.is_some_and(|g| now >= g) {
                    break Some(Cut::Grace);
                }
                if !soft_passed && now >= soft_at {
                    soft_passed = true;
                    if r.usable() >= 1 {
                        break Some(Cut::Soft);
                    }
                }
                if grace_at.is_none()
                    && r.usable() >= QUORUM
                    && r.claimants_settled()
                    && r.natives_ready(now, native_wait_at)
                    && ctx.quorum(&r.settled)
                {
                    grace_at = Some((now + d.grace).min(r.hard_at));
                }
                if slow_check.is_some_and(|t| now >= t) {
                    slow_check = None;
                    if r.hedges == 0 && grace_at.is_none() && r.usable() < QUORUM {
                        r.fire(HedgeWhy::Slow);
                    }
                }
                if r.stagger_at.is_some_and(|t| now >= t) {
                    r.stagger_at = None;
                    if grace_at.is_none() && r.usable() < QUORUM {
                        r.fire(HedgeWhy::Stagger);
                    }
                }
            }
        }
    };
    // Stop the stragglers: cancel their tokens before dropping their futures
    // so child tasks and drop guards observe cancellation immediately.
    let Race {
        seats,
        settled,
        futs,
        ..
    } = r;
    for (p, done) in seats.iter().zip(&settled) {
        if done.is_none() {
            p.token.cancel();
        }
    }
    drop(futs);
    let lanes = seats
        .into_iter()
        .zip(settled)
        .map(|(p, done)| {
            done.unwrap_or_else(|| Lane {
                seat: p.seat,
                hedge: p.why,
                outcome: Outcome::Cut(cut.unwrap_or(Cut::Grace)),
                dispatched_at: p.at,
                elapsed: Instant::now()
                    .saturating_duration_since(start)
                    .saturating_sub(p.at),
            })
        })
        .collect();
    Run { lanes, cut }
}

/// What the merge produced.
#[derive(Debug, Default)]
pub struct Merged {
    pub results: Vec<Hit>,
    /// Buckets dropped by the coherence guard (a witness confirmed them).
    pub off_topic: Vec<String>,
    /// Suspect buckets kept because nothing could confirm them.
    pub unconfirmed: Vec<String>,
    /// General engines whose results were kept, plus those the grace window
    /// cut (they were not lost; the kernel stopped waiting).
    pub standing: usize,
    pub drops: Drops,
    pub raw_total: usize,
}

/// `_drop_decoy_buckets` + `_merge`: drop off-topic buckets when a witness
/// confirms them, fuse the rest.
pub fn merge(ctx: &Ctx<'_>, run: &Run) -> Merged {
    let mut m = Merged::default();
    let answered: Vec<(&Lane, &Vec<Hit>, bool)> = run
        .lanes
        .iter()
        .filter_map(|l| match &l.outcome {
            Outcome::Answered {
                hits,
                raw,
                drops,
                suspect,
            } => {
                m.drops.merge(drops);
                m.raw_total += raw;
                Some((l, hits, *suspect))
            }
            _ => None,
        })
        .collect();
    let witness = answered
        .iter()
        .any(|(_, hits, suspect)| !suspect && is_witness(ctx.query, hits));
    let mut buckets: Vec<(String, Vec<Hit>)> = Vec::new();
    for (lane, hits, suspect) in answered {
        if suspect && witness {
            m.off_topic.push(lane.seat.id.clone());
            continue;
        }
        if suspect {
            m.unconfirmed.push(lane.seat.id.clone());
        }
        if lane.seat.guarded && !hits.is_empty() {
            m.standing += 1;
        }
        buckets.push((lane.seat.id.clone(), hits.clone()));
    }
    // A general engine cut by the grace window was not lost: the kernel
    // chose not to wait once the quorum was in. It still stands.
    m.standing += run
        .lanes
        .iter()
        .filter(|l| l.seat.guarded && matches!(l.outcome, Outcome::Cut(Cut::Grace)))
        .count();
    m.results = fuse(&buckets, ctx.weighting, ctx.limit);
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsm_core::ScholarlyWork;
    use fsm_core::ext::w1a::wall_error;

    #[test]
    fn errors_map_to_health_verdicts() {
        let up = |s: Option<u16>, m: &str| FsmError::upstream("x", s, m);
        assert_eq!(
            verdict(&wall_error("x", "consent", Some(200))),
            Verdict::Wall("consent".into())
        );
        // fsm-engines-serp's wall message.
        assert_eq!(
            verdict(&up(Some(200), "so360 was captcha-gated")),
            Verdict::Wall("captcha".into())
        );
        assert_eq!(
            verdict(&up(Some(200), "quark was x5sec-gated")),
            Verdict::Wall("x5sec".into())
        );
        // Heuristic fallback for messages that only describe the wall.
        assert_eq!(
            verdict(&up(None, "unusual traffic from your network")),
            Verdict::Wall("captcha".into())
        );
        assert_eq!(
            verdict(&up(Some(503), "bing: HTTP 503")),
            Verdict::Strike("http_503".into())
        );
        assert_eq!(
            verdict(&up(None, "connection reset")),
            Verdict::Strike("error".into())
        );
        let rl = FsmError::RateLimited {
            source_id: "x".into(),
            retry_after_s: None,
        };
        assert_eq!(verdict(&rl), Verdict::Strike("http_429".into()));
        assert_eq!(
            verdict(&FsmError::Timeout(5000)),
            Verdict::Strike("timeout".into())
        );
        assert_eq!(
            verdict(&FsmError::Timeout(0)),
            Verdict::NoStrike,
            "cancellation"
        );
        let auth = FsmError::AuthRequired {
            source_id: "x".into(),
            env_var: "K".into(),
        };
        assert_eq!(verdict(&auth), Verdict::NoStrike);
        assert_eq!(verdict(&FsmError::invalid("bad")), Verdict::NoStrike);
        assert_eq!(
            verdict(&FsmError::BrowserUnavailable("stub".into())),
            Verdict::NoStrike
        );
        assert_eq!(
            verdict(&FsmError::internal("x", "bug")),
            Verdict::Strike("error".into())
        );
    }

    #[test]
    fn every_output_shape_becomes_hits() {
        let mut w = ScholarlyWork::new(
            "Deep Residual Learning",
            "https://doi.org/10.1109/cvpr.2016.90",
        );
        w.year = Some(2016);
        w.abstract_text = Some("We present a residual learning framework.".into());
        w.ids.doi = Some("10.1109/cvpr.2016.90".into());
        let no_url = ScholarlyWork::new("No URL", "");
        let hits = to_hits(SrcOutput::Works(vec![w, no_url]), "openalex");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, HitKind::Academic);
        assert_eq!(
            (hits[0].date.as_deref(), hits[0].date_source.as_deref()),
            (Some("2016"), Some("api"))
        );
        assert_eq!(hits[0].extra, Some(json!({"doi": "10.1109/cvpr.2016.90"})));

        let items = vec![
            json!({"name": "fastapi", "html_url": "https://github.com/fastapi/fastapi", "description": "web framework", "updated": "2026-11-20"}),
            json!({"title": "no url"}),
        ];
        let hits = to_hits(
            SrcOutput::Records {
                items,
                hits: None,
                next_cursor: None,
            },
            "github",
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(
            (hits[0].title.as_str(), hits[0].url.as_str()),
            ("fastapi", "https://github.com/fastapi/fastapi")
        );
        assert_eq!(hits[0].kind, HitKind::Record);
        let given = vec![Hit::new("t", "https://a.example")];
        let hits = to_hits(
            SrcOutput::Records {
                items: vec![json!({})],
                hits: Some(given),
                next_cursor: None,
            },
            "x",
        );
        assert_eq!(hits.len(), 1);
        assert!(to_hits(SrcOutput::Text("x".into()), "x").is_empty());
        assert!(to_hits(SrcOutput::Raw(json!({"url": "https://a"})), "x").is_empty());
    }

    #[test]
    fn long_snippets_are_clipped_on_a_char_boundary() {
        let long = "长".repeat(600);
        let c = clip(&long, 500);
        assert_eq!(c.chars().count(), 501);
        assert!(c.ends_with('…'));
        assert_eq!(clip("short", 500), "short");
    }

    #[test]
    fn deadlines_respect_the_callers_budget() {
        let cfg = Config::defaults_without_home();
        let d = Deadlines::new(&cfg, Duration::from_secs(30));
        assert_eq!(
            (d.soft, d.hard),
            (Duration::from_secs(4), Duration::from_secs(8))
        );
        let d = Deadlines::new(&cfg, Duration::from_secs(2));
        assert_eq!(
            (d.soft, d.hard),
            (Duration::from_secs(2), Duration::from_secs(2))
        );
        assert_eq!(
            (d.hedge_after, d.stagger, d.grace),
            (HEDGE_AFTER, HEDGE_STAGGER, GRACE)
        );
    }

    #[test]
    fn bing_news_apiclick_redirects_unwrap_to_publisher_url() {
        let wrapped = "http://www.bing.com/news/apiclick.aspx?ref=FexRss&aid=&tid=abc&url=https%3a%2f%2fwww.reuters.com%2fworld%2feu-ai-act%2f&c=123";
        assert_eq!(
            unwrap_bing_news_redirect(wrapped),
            "https://www.reuters.com/world/eu-ai-act/"
        );
        assert_eq!(
            unwrap_bing_news_redirect("https://www.bbc.com/news/story"),
            "https://www.bbc.com/news/story"
        );
    }

    #[test]
    fn freshness_maps_to_timespan_bing_interval_and_recent_op() {
        use fsm_core::{OpDecl, Param, ParamType, ReturnKind, Source, SourceDecl};

        struct Stub(SourceDecl);
        #[async_trait::async_trait]
        impl Source for Stub {
            fn decl(&self) -> &SourceDecl {
                &self.0
            }
            async fn call(&self, _: &str, _: Value, _: &SrcCx) -> Result<SrcOutput, FsmError> {
                Ok(SrcOutput::Hits(Vec::new()))
            }
        }

        let cfg = Config::defaults_without_home();
        let w = Weighting::default();
        let ctx = Ctx {
            query: "EU AI Act enforcement",
            limit: 10,
            cfg: &cfg,
            lang: None,
            freshness: Some(Freshness::Week),
            domains: vec![],
            filters: Filters::default(),
            host_class: None,
            weighting: &w,
            today: jiff::civil::Date::constant(2026, 11, 25),
            coherence: true,
        };

        let mk_seat = |d: SourceDecl| Seat {
            id: d.id.clone(),
            src: Arc::new(Stub(d)),
            role: crate::plan::Role::Native,
            guarded: false,
            native: true,
            timeout: Duration::from_secs(5),
        };

        let gdelt = mk_seat(
            SourceDecl::new("gdelt", "GDELT", "gdelt").with_op(
                OpDecl::new("search", "s", ReturnKind::Records)
                    .with_param(Param::new("query", ParamType::String, "q"))
                    .with_param(Param::new("timespan", ParamType::String, "Lookback")),
            ),
        );
        assert_eq!(ctx.params(&gdelt)["timespan"], "1w");

        let bing = mk_seat(
            SourceDecl::new("bing_news_rss", "Bing News", "bing").with_op(
                OpDecl::new("search", "s", ReturnKind::Records)
                    .with_param(Param::new("query", ParamType::String, "q"))
                    .with_param(Param::new(
                        "freshness",
                        ParamType::String,
                        r#"Optional Bing freshness filter e.g. interval="7""#,
                    )),
            ),
        );
        assert_eq!(ctx.params(&bing)["freshness"], r#"interval="7""#);

        let hn = mk_seat(
            SourceDecl::new("hackernews", "HN", "hn")
                .with_op(
                    OpDecl::new("search", "s", ReturnKind::Records).with_param(Param::new(
                        "query",
                        ParamType::String,
                        "q",
                    )),
                )
                .with_op(
                    OpDecl::new("recent", "r", ReturnKind::Records).with_param(Param::new(
                        "query",
                        ParamType::String,
                        "q",
                    )),
                ),
        );
        assert_eq!(ctx.search_op_name(&hn), "recent");
    }

    #[tokio::test(start_paused = true)]
    async fn early_grace_waits_for_scope_native_specialist_up_to_native_wait() {
        use fsm_core::{OpDecl, Param, ParamType, ReturnKind, Source, SourceDecl};

        struct DelayedSource {
            decl: SourceDecl,
            delay: Duration,
            url: &'static str,
        }
        #[async_trait::async_trait]
        impl Source for DelayedSource {
            fn decl(&self) -> &SourceDecl {
                &self.decl
            }
            async fn call(&self, _: &str, _: Value, _: &SrcCx) -> Result<SrcOutput, FsmError> {
                tokio::time::sleep(self.delay).await;
                let mut h = Hit::new("NVIDIA 10-Q SEC filing", self.url);
                h.snippet = "NVIDIA NVDA quarterly earnings SEC 10-Q filing".to_owned();
                h.source = self.decl.id.clone();
                Ok(SrcOutput::Hits(vec![h]))
            }
        }

        let mk_seat = |id: &str, delay_ms: u64, url: &'static str, native: bool| Seat {
            id: id.to_owned(),
            src: Arc::new(DelayedSource {
                decl: SourceDecl::new(id, id, id).with_op(
                    OpDecl::new("search", "s", ReturnKind::Hits).with_param(Param::new(
                        "query",
                        ParamType::String,
                        "q",
                    )),
                ),
                delay: Duration::from_millis(delay_ms),
                url,
            }),
            role: if native {
                crate::plan::Role::Native
            } else {
                crate::plan::Role::Primary
            },
            guarded: !native,
            native,
            timeout: Duration::from_secs(5),
        };

        let cfg = Config::defaults_without_home();
        let w = Weighting::default();
        let ctx = Ctx {
            query: "NVIDIA NVDA quarterly earnings SEC",
            limit: 1,
            cfg: &cfg,
            lang: None,
            freshness: None,
            domains: vec![],
            filters: Filters::default(),
            host_class: None,
            weighting: &w,
            today: jiff::civil::Date::constant(2026, 11, 25),
            coherence: false,
        };

        // Two generic engines answer at 400ms; the scope-native specialist (`sec_edgar`)
        // answers at 1900ms (inside NATIVE_WAIT=2600ms, well past 400ms+GRACE=800ms).
        // Early grace MUST wait for the native specialist instead of cancelling it at 800ms.
        let seats = vec![
            mk_seat("bing", 400, "https://investor.nvidia.com/1", false),
            mk_seat("yahoo", 400, "https://investor.nvidia.com/2", false),
            mk_seat(
                "sec_edgar",
                1900,
                "https://www.sec.gov/Archives/edgar/data/1045810/q3.htm",
                true,
            ),
        ];
        let run = race(
            &ctx,
            seats,
            Vec::new(),
            false,
            Deadlines::new(&cfg, Duration::from_secs(8)),
            &CancellationToken::new(),
        )
        .await;
        let sec = run.lanes.iter().find(|l| l.seat.id == "sec_edgar").unwrap();
        assert!(
            sec.usable(),
            "scope-native sec_edgar must not be cut off by early generic grace; got {:?}",
            sec.outcome
        );
    }
}
