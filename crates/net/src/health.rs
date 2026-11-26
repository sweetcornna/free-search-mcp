//! Per-engine circuit breaker (port of `health.py`), persisted through
//! `StoreApi::health_save` / `health_load_all` (`health_state` table).
//!
//! A deliberate wall (captcha, consent, ...) benches an engine at once; a blip
//! (timeout, 5xx) needs [`SOFT_THRESHOLD`] in a row; silence needs
//! [`SILENT_THRESHOLD`]. A failed probe after a cooldown doubles it, up to the
//! maximum; `region` walls get the maximum at once; a success forgets all.
//!
//! # `HealthRecord` encoding (cross-lane contract with W0c / W1a)
//! | field | value |
//! |---|---|
//! | `state` | `"open:<trips>"` while a circuit is (or recently was) open; `"closed"` after a recovery |
//! | `failures` | consecutive strikes |
//! | `benched_until` | wall-clock expiry, Unix seconds |
//! | `last_error` | the reason (`captcha`, `error`, ...) |
//! | `updated_at` | when the circuit opened, Unix seconds (cooldown = `benched_until - updated_at`) |
//!
//! Rows that do not parse are ignored (the store is advisory, as the Python
//! JSON file was).

use fsm_core::HealthRecord;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime};

/// Walls an engine put up on purpose: one sighting benches it.
pub const HARD_REASONS: &[&str] = &[
    "captcha",
    "consent",
    "javascript",
    "login",
    "no_live_instance",
    "off_topic",
    "region",
];
/// Walls that do not move: benched for the maximum at once.
pub const LONG_REASONS: &[&str] = &["region"];
/// Consecutive soft failures (timeouts, resets, 5xx) that open the circuit.
pub const SOFT_THRESHOLD: u32 = 2;
/// Consecutive empty answers that open the circuit (callers pass it as `threshold`).
pub const SILENT_THRESHOLD: u32 = 3;
/// How long an expired circuit is remembered, so escalation survives restarts.
pub const REMEMBER_EXPIRED_FOR_S: f64 = 24.0 * 3600.0;

/// A clock in seconds.
pub type ClockFn = Arc<dyn Fn() -> f64 + Send + Sync>;

#[derive(Debug, Clone, Default)]
struct State {
    strikes: u32,
    trips: u32,
    opened_at: Option<f64>,
    cooldown: f64,
    reason: String,
}

/// `{"reason", "retry_in_seconds"}` of a benched engine.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Benched {
    pub reason: String,
    pub retry_in_seconds: u64,
}

/// The breaker. Thread-safe; cheap to share behind an `Arc`.
pub struct EngineHealth {
    cooldown: f64,
    max_cooldown: f64,
    clock: ClockFn,
    wall: ClockFn,
    states: Mutex<HashMap<String, State>>,
}

impl std::fmt::Debug for EngineHealth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineHealth")
            .field("cooldown", &self.cooldown)
            .field("max_cooldown", &self.max_cooldown)
            .finish_non_exhaustive()
    }
}

fn monotonic() -> ClockFn {
    let origin = Instant::now();
    Arc::new(move || origin.elapsed().as_secs_f64())
}

fn wall_clock() -> ClockFn {
    Arc::new(|| {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)
    })
}

impl EngineHealth {
    /// `cooldown_s` / `max_cooldown_s`: `Config.engine_cooldown_s` and
    /// `engine_cooldown_max_s`.
    pub fn new(cooldown_s: f64, max_cooldown_s: f64) -> Self {
        Self::with_clocks(cooldown_s, max_cooldown_s, monotonic(), wall_clock())
    }

    /// With injected clocks (`clock`: monotonic seconds for in-memory
    /// durations; `wall`: Unix seconds for persisted expiries).
    pub fn with_clocks(
        cooldown_s: f64,
        max_cooldown_s: f64,
        clock: ClockFn,
        wall: ClockFn,
    ) -> Self {
        Self {
            cooldown: cooldown_s.max(0.0),
            max_cooldown: max_cooldown_s.max(cooldown_s.max(0.0)),
            clock,
            wall,
            states: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, State>> {
        self.states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Forget everything (the caller also clears the persisted rows).
    pub fn reset(&self) {
        self.lock().clear();
    }

    /// A success clears the engine, escalation included. Returns the row to
    /// persist when the engine had been benched (`state = "closed"`).
    pub fn record_success(&self, name: &str) -> Option<HealthRecord> {
        let was = self.lock().remove(name)?;
        was.opened_at?;
        let mut rec = HealthRecord::new(name, "closed", self.wall_now());
        rec.failures = 0;
        Some(rec)
    }

    /// Count one failure; open the circuit once `threshold` (default: 1 for a
    /// [`HARD_REASONS`] wall, else [`SOFT_THRESHOLD`]) is reached. Strikes
    /// survive the cooldown. Returns the row to persist when the circuit opened.
    pub fn record_failure(
        &self,
        name: &str,
        reason: &str,
        threshold: Option<u32>,
    ) -> Option<HealthRecord> {
        let threshold = threshold.unwrap_or(if HARD_REASONS.contains(&reason) {
            1
        } else {
            SOFT_THRESHOLD
        });
        let now = (self.clock)();
        let mut states = self.lock();
        let st = states.entry(name.to_owned()).or_default();
        st.strikes += 1;
        if st.strikes < threshold {
            return None;
        }
        st.cooldown = if LONG_REASONS.contains(&reason) {
            self.max_cooldown
        } else {
            (self.cooldown * 2f64.powi(i32::try_from(st.trips).unwrap_or(i32::MAX).min(62)))
                .min(self.max_cooldown)
        };
        st.trips += 1;
        st.opened_at = Some(now);
        st.reason = reason.to_owned();
        let st = st.clone();
        drop(states);
        Some(self.to_record(name, &st, now))
    }

    fn wall_now(&self) -> i64 {
        (self.wall)().round() as i64
    }

    fn to_record(&self, name: &str, st: &State, now: f64) -> HealthRecord {
        let opened = st.opened_at.unwrap_or(now);
        let remaining = st.cooldown - (now - opened);
        let wall = (self.wall)();
        let mut rec = HealthRecord::new(
            name,
            format!("open:{}", st.trips),
            (wall - (st.cooldown - remaining)).round() as i64,
        );
        rec.failures = st.strikes;
        rec.benched_until = Some((wall + remaining).round() as i64);
        rec.last_error = Some(st.reason.clone());
        rec
    }

    /// True while benched; false again once the cooldown ran out (the next
    /// search that includes it is the probe).
    pub fn is_open(&self, name: &str) -> bool {
        let now = (self.clock)();
        self.lock()
            .get(name)
            .and_then(|s| s.opened_at.map(|o| now - o < s.cooldown))
            .unwrap_or(false)
    }

    /// Reason and seconds left for a benched engine.
    pub fn describe(&self, name: &str) -> Option<Benched> {
        let now = (self.clock)();
        let states = self.lock();
        let st = states.get(name)?;
        let opened = st.opened_at?;
        let remaining = st.cooldown - (now - opened);
        if remaining <= 0.0 {
            return None;
        }
        Some(Benched {
            reason: st.reason.clone(),
            retry_in_seconds: (remaining.round() as u64).max(1),
        })
    }

    /// Rows for every open (or recently expired) circuit (Python `_save`).
    pub fn records(&self) -> Vec<HealthRecord> {
        let now = (self.clock)();
        let states = self.lock();
        let mut out: Vec<HealthRecord> = states
            .iter()
            .filter(|(_, s)| {
                s.opened_at
                    .is_some_and(|o| s.cooldown - (now - o) > -REMEMBER_EXPIRED_FOR_S)
            })
            .map(|(n, s)| self.to_record(n, s, now))
            .collect();
        out.sort_by(|a, b| a.source_id.cmp(&b.source_id));
        out
    }

    /// Adopt the circuits another process left behind (Python `_load`).
    /// Engines this process already tracks keep their state; rows that do not
    /// parse, closed rows and long-expired rows are skipped.
    pub fn adopt(&self, rows: &[HealthRecord]) {
        let now = (self.clock)();
        let wall = (self.wall)();
        let mut states = self.lock();
        for row in rows {
            if states.contains_key(&row.source_id) {
                continue;
            }
            let Some(trips) = row
                .state
                .strip_prefix("open:")
                .and_then(|t| t.parse::<u32>().ok())
            else {
                continue;
            };
            let Some(until) = row.benched_until else {
                continue;
            };
            let cooldown = until as f64 - row.updated_at as f64;
            let remaining = until as f64 - wall;
            if !cooldown.is_finite()
                || cooldown <= 0.0
                || !remaining.is_finite()
                || remaining <= -REMEMBER_EXPIRED_FOR_S
            {
                continue;
            }
            // Placed so it has `remaining` seconds left here (an expired one is
            // adopted closed, keeping its trips for the escalation).
            states.insert(
                row.source_id.clone(),
                State {
                    strikes: row.failures.max(1),
                    trips: trips.max(1),
                    opened_at: Some(now - (cooldown - remaining.clamp(0.0, cooldown))),
                    cooldown,
                    reason: row.last_error.clone().unwrap_or_else(|| "error".into()),
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    //! Port of `tests/test_engine_health.py`; the JSON file becomes
    //! `HealthRecord` rows (a map stands in for the `health_state` table).
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[derive(Clone)]
    struct Clock(Arc<AtomicU64>);

    impl Clock {
        fn new() -> Self {
            Self(Arc::new(AtomicU64::new(1000.0f64.to_bits())))
        }
        fn now(&self) -> f64 {
            f64::from_bits(self.0.load(Ordering::SeqCst))
        }
        fn advance(&self, s: f64) {
            self.0.store((self.now() + s).to_bits(), Ordering::SeqCst);
        }
        fn f(&self) -> ClockFn {
            let c = self.clone();
            Arc::new(move || c.now())
        }
    }

    fn health(clock: &Clock) -> EngineHealth {
        EngineHealth::with_clocks(600.0, 3600.0, clock.f(), clock.f())
    }

    fn benched(h: &EngineHealth, name: &str) -> Option<(String, u64)> {
        h.describe(name).map(|b| (b.reason, b.retry_in_seconds))
    }

    /// The `health_state` table: last row per source wins.
    #[derive(Default)]
    struct Table(HashMap<String, HealthRecord>);

    impl Table {
        fn save(&mut self, row: Option<HealthRecord>) {
            if let Some(r) = row {
                self.0.insert(r.source_id.clone(), r);
            }
        }
        fn rows(&self) -> Vec<HealthRecord> {
            self.0.values().cloned().collect()
        }
    }

    #[test]
    fn test_a_wall_benches_the_engine_at_once() {
        for reason in HARD_REASONS.iter().filter(|r| !LONG_REASONS.contains(r)) {
            let c = Clock::new();
            let h = health(&c);
            assert!(h.record_failure("mojeek", reason, None).is_some());
            assert!(h.is_open("mojeek"), "{reason}");
            assert_eq!(benched(&h, "mojeek"), Some((reason.to_string(), 600)));
        }
    }

    #[test]
    fn test_a_blip_does_not() {
        let c = Clock::new();
        let h = health(&c);
        assert!(h.record_failure("bing", "error", None).is_none());
        assert!(!h.is_open("bing"));
        assert_eq!(h.describe("bing"), None);
    }

    #[test]
    fn test_repeated_blips_do() {
        let c = Clock::new();
        let h = health(&c);
        for _ in 0..SOFT_THRESHOLD {
            h.record_failure("bing", "error", None);
        }
        assert!(h.is_open("bing"));
    }

    #[test]
    fn test_a_success_between_blips_starts_the_count_again() {
        let c = Clock::new();
        let h = health(&c);
        h.record_failure("bing", "error", None);
        assert!(
            h.record_success("bing").is_none(),
            "never benched: nothing to persist"
        );
        h.record_failure("bing", "error", None);
        assert!(!h.is_open("bing"));
    }

    #[test]
    fn test_silence_needs_more_evidence_than_an_error() {
        let c = Clock::new();
        let h = health(&c);
        for i in 0..SILENT_THRESHOLD {
            assert!(!h.is_open("anysearch"), "{i}");
            h.record_failure("anysearch", "silent", Some(SILENT_THRESHOLD));
        }
        assert!(h.is_open("anysearch"));
    }

    #[test]
    fn test_the_cooldown_runs_out_and_the_next_search_is_the_probe() {
        let c = Clock::new();
        let h = health(&c);
        h.record_failure("mojeek", "captcha", None);
        c.advance(599.0);
        assert_eq!(benched(&h, "mojeek").unwrap().1, 1);
        c.advance(2.0);
        assert!(!h.is_open("mojeek"));
    }

    #[test]
    fn test_a_failed_probe_doubles_the_cooldown_up_to_the_cap() {
        let c = Clock::new();
        let h = health(&c);
        for cooldown in [600, 1200, 2400, 3600, 3600] {
            h.record_failure("mojeek", "captcha", None);
            assert_eq!(benched(&h, "mojeek").unwrap().1, cooldown);
            c.advance(cooldown as f64 + 1.0);
            assert!(!h.is_open("mojeek"));
        }
    }

    #[test]
    fn test_a_failed_probe_re_opens_on_its_first_soft_failure() {
        let c = Clock::new();
        let h = health(&c);
        for _ in 0..SOFT_THRESHOLD {
            h.record_failure("bing", "error", None);
        }
        c.advance(601.0);
        assert!(!h.is_open("bing"));
        h.record_failure("bing", "error", None);
        assert!(h.is_open("bing"));
    }

    #[test]
    fn test_recovery_forgets_the_escalation() {
        let c = Clock::new();
        let h = health(&c);
        h.record_failure("mojeek", "captcha", None);
        c.advance(601.0);
        h.record_failure("mojeek", "captcha", None); // now on the 1200 s rung
        c.advance(1201.0);
        let row = h.record_success("mojeek").unwrap();
        assert_eq!(row.state, "closed");
        h.record_failure("mojeek", "captcha", None);
        assert_eq!(benched(&h, "mojeek").unwrap().1, 600);
    }

    #[test]
    fn test_engines_are_independent() {
        let c = Clock::new();
        let h = health(&c);
        h.record_failure("mojeek", "captcha", None);
        assert!(!h.is_open("duckduckgo"));
    }

    #[test]
    fn test_reset_clears_everything() {
        let c = Clock::new();
        let h = health(&c);
        h.record_failure("mojeek", "captcha", None);
        h.reset();
        assert!(!h.is_open("mojeek"));
        assert!(h.records().is_empty());
    }

    #[test]
    fn test_the_cooldown_comes_from_settings_when_not_given() {
        // Config.engine_cooldown_s feeds the constructor.
        let c = Clock::new();
        let h = EngineHealth::with_clocks(42.0, 3600.0, c.f(), c.f());
        h.record_failure("mojeek", "captcha", None);
        assert_eq!(benched(&h, "mojeek").unwrap().1, 42);
    }

    #[test]
    fn test_an_open_circuit_is_adopted_by_the_next_process() {
        let c = Clock::new();
        let mut table = Table::default();
        let first = health(&c);
        table.save(first.record_failure("mojeek", "captcha", None));
        let row = &table.0["mojeek"];
        assert_eq!(
            (row.state.as_str(), row.benched_until, row.updated_at),
            ("open:1", Some(1600), 1000)
        );
        assert_eq!(row.last_error.as_deref(), Some("captcha"));

        c.advance(100.0);
        let second = health(&c);
        second.adopt(&table.rows());
        assert!(second.is_open("mojeek"));
        assert_eq!(benched(&second, "mojeek"), Some(("captcha".into(), 500)));
        c.advance(501.0);
        assert!(
            !second.is_open("mojeek"),
            "the probe is due at the same moment in both"
        );
    }

    #[test]
    fn test_a_recovery_is_written_too() {
        let c = Clock::new();
        let mut table = Table::default();
        let first = health(&c);
        table.save(first.record_failure("mojeek", "captcha", None));
        table.save(first.record_success("mojeek"));
        let second = health(&c);
        second.adopt(&table.rows());
        assert!(!second.is_open("mojeek"));
    }

    #[test]
    fn test_an_expired_or_broken_file_costs_nothing() {
        let c = Clock::new();
        let mut broken = HealthRecord::new("mojeek", "not-a-state", 0);
        broken.benched_until = Some(5000);
        let h = health(&c);
        h.adopt(&[broken]);
        assert!(!h.is_open("mojeek"));
        // Long expired.
        let mut old = HealthRecord::new("bing", "open:1", -200_000);
        old.benched_until = Some(-199_400);
        // No expiry at all.
        let none = HealthRecord::new("ddg", "open:2", 900);
        let h = health(&c);
        h.adopt(&[old, none]);
        assert!(!h.is_open("bing") && !h.is_open("ddg"));
        assert!(h.records().is_empty());
    }

    #[test]
    fn test_the_escalation_survives_a_restart() {
        let c = Clock::new();
        let mut table = Table::default();
        let first = health(&c);
        table.save(first.record_failure("mojeek", "captcha", None));
        c.advance(601.0);
        let second = health(&c);
        second.adopt(&table.rows());
        assert!(!second.is_open("mojeek"));
        second.record_failure("mojeek", "captcha", None);
        assert_eq!(benched(&second, "mojeek").unwrap().1, 1200);
    }

    #[test]
    fn test_a_region_block_is_benched_for_the_maximum_at_once() {
        let c = Clock::new();
        let h = health(&c);
        h.record_failure("yahoojp", "region", None);
        assert_eq!(benched(&h, "yahoojp"), Some(("region".into(), 3600)));
    }

    #[test]
    fn records_keep_expired_circuits_for_a_day() {
        let c = Clock::new();
        let h = health(&c);
        h.record_failure("mojeek", "captcha", None);
        c.advance(601.0);
        assert_eq!(h.records().len(), 1, "expired but remembered (escalation)");
        c.advance(REMEMBER_EXPIRED_FOR_S);
        assert!(h.records().is_empty());
    }

    /// I4 regression (Missing test #11): corrupted SQLite timestamps must not
    /// overflow or panic in `adopt`.
    #[test]
    fn adopt_handles_timestamp_overflow_without_panicking() {
        let c = Clock::new();
        let h = health(&c);
        let mut huge = HealthRecord::new("corrupt_hi", "open:1", -1);
        huge.benched_until = Some(i64::MAX);
        let mut neg = HealthRecord::new("corrupt_lo", "open:1", 1);
        neg.benched_until = Some(i64::MIN);
        h.adopt(&[huge, neg]);
        assert!(!h.is_open("corrupt_lo"));
    }
}
