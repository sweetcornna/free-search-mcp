//! Rate limits: `governor` GCRA buckets keyed by
//! `limits.host_bucket.unwrap_or(source_id)` (`SPEC.md` §5), plus
//! `Retry-After` penalties. Port of `ratelimit.py`'s `TokenBucket` contract:
//! a non-positive rate means unlimited, a caller that will not wait is never
//! queued behind a sleeper, waiters are served in arrival order, and a
//! cancelled waiter consumes nothing (GCRA only charges on success).

use std::collections::{HashMap, VecDeque};
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Longest `Retry-After` honoured (a server asking for a day still gets one hour).
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(3600);

#[derive(Debug)]
struct GcraState {
    origin: Instant,
    /// GCRA Theoretical Arrival Time (elapsed since `origin`): when the bucket
    /// will next be full after all committed + reserved acquisitions.
    tat: Duration,
    /// Replenish interval per token (derived via `governor::Quota`).
    interval: Duration,
    /// Full-bucket window: `interval * burst`.
    limit: Duration,
    /// Arrival-ordered active reservations `(id, ready_at)`.
    waiters: VecDeque<(u64, Duration)>,
    next_id: u64,
}

/// One GCRA bucket (`governor::Quota` parameters + exact atomic TAT reservation
/// with cancellation rollback).
pub struct Bucket {
    state: Option<Mutex<GcraState>>,
    notify: tokio::sync::Notify,
}

impl std::fmt::Debug for Bucket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let interval = self
            .state
            .as_ref()
            .and_then(|s| s.lock().ok().map(|g| g.interval))
            .unwrap_or(Duration::ZERO);
        f.debug_struct("Bucket")
            .field("unlimited", &self.state.is_none())
            .field("interval", &interval)
            .finish_non_exhaustive()
    }
}

struct ReservationGuard<'a> {
    bucket: &'a Bucket,
    id: u64,
    armed: bool,
}

impl Drop for ReservationGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(m) = &self.bucket.state {
            let mut st = m.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(pos) = st.waiters.iter().position(|(wid, _)| *wid == self.id) {
                let interval = st.interval;
                st.waiters.remove(pos);
                for (_, ready_at) in st.waiters.iter_mut().skip(pos) {
                    *ready_at = ready_at.saturating_sub(interval);
                }
                st.tat = st.tat.saturating_sub(interval);
            }
            drop(st);
            self.bucket.notify.notify_waiters();
        }
    }
}

fn quota_for(rps: f64, burst: u32) -> Option<governor::Quota> {
    if !rps.is_finite() || rps <= 0.0 {
        return None;
    }
    let period = Duration::try_from_secs_f64(1.0 / rps).ok()?;
    let quota = governor::Quota::with_period(period)?;
    Some(quota.allow_burst(NonZeroU32::new(burst.max(1)).unwrap_or(NonZeroU32::MIN)))
}

impl Bucket {
    /// `rps` requests per second with `burst` capacity; `rps <= 0` (or not
    /// finite, or period outside `Duration` range) is unlimited.
    pub fn per_second(rps: f64, burst: u32) -> Self {
        let state = quota_for(rps, burst).map(|q| {
            let interval = q.replenish_interval();
            let burst_n = q.burst_size().get();
            Mutex::new(GcraState {
                origin: Instant::now(),
                tat: Duration::ZERO,
                interval,
                limit: interval.saturating_mul(burst_n),
                waiters: VecDeque::new(),
                next_id: 0,
            })
        });
        Self {
            state,
            notify: tokio::sync::Notify::new(),
        }
    }

    /// Python `TokenBucket(rate_per_minute, burst)`: capacity defaults to a
    /// full minute's worth (`max(rpm, 1)`).
    pub fn per_minute(rpm: f64, burst: Option<u32>) -> Self {
        let cap = burst.unwrap_or_else(|| {
            if rpm.is_finite() && rpm >= 1.0 {
                rpm.min(f64::from(u32::MAX)) as u32
            } else {
                1
            }
        });
        Self::per_second(rpm / 60.0, cap)
    }

    /// Tighten a shared implicit bucket in place if `rps` is stricter (larger
    /// interval) than its current rate, preserving live reservations.
    fn tighten(&self, rps: f64, burst: u32) {
        let Some(m) = &self.state else {
            return;
        };
        let Some(q) = quota_for(rps, burst) else {
            return;
        };
        let new_interval = q.replenish_interval();
        let new_limit = new_interval.saturating_mul(q.burst_size().get());
        let mut st = m.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if new_interval > st.interval {
            st.interval = new_interval;
            st.limit = new_limit;
        } else if new_interval == st.interval && new_limit < st.limit {
            st.limit = new_limit;
        }
    }

    pub fn is_unlimited(&self) -> bool {
        self.state.is_none()
    }

    /// Take a token now if one is free and nobody is queued.
    pub fn try_acquire(&self) -> bool {
        let Some(m) = &self.state else {
            return true;
        };
        let mut st = m.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if !st.waiters.is_empty() {
            return false;
        }
        let now = st.origin.elapsed();
        let base = st.tat.max(now);
        let next_tat = base.saturating_add(st.interval);
        let ready_at = next_tat.saturating_sub(st.limit);
        if ready_at <= now {
            st.tat = next_tat;
            true
        } else {
            false
        }
    }

    /// Take one token, waiting in arrival order. With `max_wait`, give up
    /// immediately (consuming nothing) when the exact wait would be longer;
    /// `Err` carries the exact remaining wait.
    pub async fn acquire(&self, max_wait: Option<Duration>) -> Result<(), Duration> {
        let Some(m) = &self.state else {
            return Ok(());
        };
        let id = {
            let mut st = m.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let now = st.origin.elapsed();
            let base = st.tat.max(now);
            let next_tat = base.saturating_add(st.interval);
            let ready_at = next_tat.saturating_sub(st.limit);
            let wait = ready_at.saturating_sub(now);
            if max_wait.is_some_and(|lim| wait > lim) {
                return Err(wait);
            }
            st.tat = next_tat;
            if wait.is_zero() && st.waiters.is_empty() {
                return Ok(());
            }
            let id = st.next_id;
            st.next_id = st.next_id.wrapping_add(1);
            st.waiters.push_back((id, ready_at));
            id
        };

        let mut guard = ReservationGuard {
            bucket: self,
            id,
            armed: true,
        };
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let sleep_for = {
                let mut st = m.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let now = st.origin.elapsed();
                match st.waiters.iter().position(|(wid, _)| *wid == id) {
                    Some(0) => {
                        let ready_at = st.waiters[0].1;
                        if now >= ready_at {
                            st.waiters.pop_front();
                            guard.armed = false;
                            drop(st);
                            self.notify.notify_waiters();
                            return Ok(());
                        }
                        Some(ready_at - now)
                    }
                    Some(pos) => {
                        let ready_at = st.waiters[pos].1;
                        Some(ready_at.saturating_sub(now))
                    }
                    None => {
                        guard.armed = false;
                        return Ok(());
                    }
                }
            };
            match sleep_for {
                Some(d) if !d.is_zero() => {
                    tokio::select! {
                        () = tokio::time::sleep(d) => {}
                        () = notified => {}
                    }
                }
                _ => notified.await,
            }
        }
    }
}

/// Keyed buckets and `Retry-After` penalties, shared by every request of one
/// [`crate::HttpClient`].
pub struct RateLimits {
    configured: Mutex<HashMap<String, Arc<Bucket>>>,
    implicit: moka::sync::Cache<String, Arc<Bucket>>,
    penalties: Mutex<HashMap<String, Instant>>,
}

impl std::fmt::Debug for RateLimits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RateLimits").finish_non_exhaustive()
    }
}

impl Default for RateLimits {
    fn default() -> Self {
        Self::new()
    }
}

impl RateLimits {
    pub fn new() -> Self {
        Self {
            configured: Mutex::new(HashMap::new()),
            implicit: moka::sync::Cache::builder()
                .max_capacity(4096)
                .time_to_idle(Duration::from_secs(3600))
                .build(),
            penalties: Mutex::new(HashMap::new()),
        }
    }

    /// Fix the quota of `key` (e.g. a manifest's `limits = { rps, burst }`).
    pub fn configure(&self, key: &str, rps: f64, burst: u32) {
        if let Ok(mut m) = self.configured.lock() {
            m.insert(key.to_owned(), Arc::new(Bucket::per_second(rps, burst)));
        }
    }

    /// Python `RateLimiter.configure(key, rpm, burst)`.
    pub fn configure_per_minute(&self, key: &str, rpm: f64, burst: Option<u32>) {
        if let Ok(mut m) = self.configured.lock() {
            m.insert(key.to_owned(), Arc::new(Bucket::per_minute(rpm, burst)));
        }
    }

    /// The bucket for `key`: the configured one, else one shared implicit
    /// bucket keyed by `key` alone (tightening to the stricter rate if callers
    /// pass different `default_rps` for the same `host_bucket`). `None` when
    /// unlimited.
    pub fn bucket(&self, key: &str, default_rps: Option<f64>) -> Option<Arc<Bucket>> {
        if let Some(b) = self
            .configured
            .lock()
            .ok()
            .and_then(|m| m.get(key).cloned())
        {
            return Some(b);
        }
        let rps = default_rps?;
        if !rps.is_finite() || rps <= 0.0 {
            return None;
        }
        let burst = if rps > 1.0 {
            rps.ceil().min(f64::from(u32::MAX)) as u32
        } else {
            1
        };
        let b = self
            .implicit
            .get_with(key.to_owned(), || Arc::new(Bucket::per_second(rps, burst)));
        b.tighten(rps, burst);
        Some(b)
    }

    /// Wait for a token of `key` (see [`Bucket::acquire`]).
    pub async fn acquire(
        &self,
        key: &str,
        default_rps: Option<f64>,
        max_wait: Option<Duration>,
    ) -> Result<(), Duration> {
        match self.bucket(key, default_rps) {
            Some(b) => b.acquire(max_wait).await,
            None => Ok(()),
        }
    }

    /// Bench `key` for `d` (capped at [`MAX_RETRY_AFTER`]); a longer existing
    /// penalty is kept.
    pub fn penalize(&self, key: &str, d: Duration) {
        let until = Instant::now() + d.min(MAX_RETRY_AFTER);
        if let Ok(mut m) = self.penalties.lock() {
            let e = m.entry(key.to_owned()).or_insert(until);
            if *e < until {
                *e = until;
            }
            if m.len() > 4096 {
                let now = Instant::now();
                m.retain(|_, t| *t > now);
            }
        }
    }

    /// Time left on `key`'s `Retry-After` penalty.
    pub fn penalty(&self, key: &str) -> Option<Duration> {
        let mut m = self.penalties.lock().ok()?;
        let until = *m.get(key)?;
        let now = Instant::now();
        if until <= now {
            m.remove(key);
            return None;
        }
        Some(until - now)
    }
}

/// Parse a `Retry-After` value: delta-seconds or an HTTP-date.
pub fn parse_retry_after(value: &str) -> Option<Duration> {
    let v = value.trim();
    if !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) {
        return Some(
            Duration::from_secs(v.parse::<u64>().unwrap_or(u64::MAX)).min(MAX_RETRY_AFTER),
        );
    }
    let at = jiff::fmt::rfc2822::DateTimeParser::new()
        .relaxed_weekday(true)
        .parse_timestamp(v)
        .ok()?;
    let now = jiff::Timestamp::now();
    let secs = (at.as_second() - now.as_second()).max(0);
    Some(Duration::from_secs(u64::try_from(secs).unwrap_or(0)).min(MAX_RETRY_AFTER))
}

#[cfg(test)]
mod tests {
    //! Port of `tests/test_ratelimit.py` (TokenBucket -> GCRA `Bucket`).
    use super::*;

    #[tokio::test]
    async fn test_zero_rate_acquire_does_not_raise_repeatedly() {
        let b = Bucket::per_minute(0.0, None);
        assert!(b.is_unlimited());
        for _ in 0..3 {
            tokio::time::timeout(Duration::from_secs(1), b.acquire(None))
                .await
                .unwrap()
                .unwrap();
        }
    }

    #[tokio::test]
    async fn test_negative_rate_acquire_does_not_raise() {
        let b = Bucket::per_minute(-5.0, None);
        tokio::time::timeout(Duration::from_secs(1), b.acquire(None))
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), b.acquire(Some(Duration::ZERO)))
            .await
            .unwrap()
            .unwrap();
        assert!(Bucket::per_second(f64::NAN, 3).is_unlimited());
    }

    #[tokio::test]
    async fn test_zero_rate_acquire_returns_immediately() {
        let b = Bucket::per_minute(0.0, None);
        for _ in 0..100 {
            assert!(b.try_acquire());
        }
    }

    #[tokio::test]
    async fn test_positive_rate_still_consumes_tokens() {
        let b = Bucket::per_minute(60.0, Some(2));
        assert!(b.acquire(Some(Duration::ZERO)).await.is_ok());
        assert!(b.acquire(Some(Duration::ZERO)).await.is_ok());
        assert!(
            b.acquire(Some(Duration::ZERO)).await.is_err(),
            "burst of 2 is spent"
        );
    }

    #[tokio::test]
    async fn test_positive_rate_blocks_when_exhausted() {
        let b = Bucket::per_minute(60.0, Some(1));
        b.acquire(None).await.unwrap();
        let start = Instant::now();
        tokio::time::timeout(Duration::from_secs(5), b.acquire(None))
            .await
            .unwrap()
            .unwrap();
        assert!(
            start.elapsed() > Duration::from_millis(100),
            "had to wait for a refill"
        );
    }

    #[tokio::test]
    async fn test_ratelimiter_with_zero_default_does_not_raise() {
        let l = RateLimits::new();
        for _ in 0..2 {
            tokio::time::timeout(
                Duration::from_secs(1),
                l.acquire("any-key", Some(0.0), None),
            )
            .await
            .unwrap()
            .unwrap();
        }
        assert!(
            l.acquire("unconfigured", None, Some(Duration::ZERO))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn test_a_burst_cap_below_the_rate_skips_the_fourth_request() {
        let l = RateLimits::new();
        l.configure_per_minute("quark", 10.0, Some(3));
        let mut got = Vec::new();
        for _ in 0..4 {
            got.push(
                l.acquire("quark", Some(30.0 / 60.0), Some(Duration::ZERO))
                    .await
                    .is_ok(),
            );
        }
        assert_eq!(got, [true, true, true, false]);
    }

    #[tokio::test]
    async fn test_a_caller_that_will_not_wait_is_not_queued_behind_a_sleeper() {
        let b = Arc::new(Bucket::per_minute(60.0, Some(1)));
        b.acquire(None).await.unwrap();
        let sleeper = {
            let b = b.clone();
            tokio::spawn(async move { b.acquire(None).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        let started = Instant::now();
        assert!(b.acquire(Some(Duration::ZERO)).await.is_err());
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "not queued behind the 1 s sleeper"
        );
        assert!(sleeper.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn test_waiters_are_served_in_arrival_order() {
        let b = Arc::new(Bucket::per_minute(600.0, Some(1))); // a token every 0.1 s
        b.acquire(None).await.unwrap();
        let order = Arc::new(Mutex::new(Vec::new()));
        let take = |i: usize| {
            let (b, order) = (b.clone(), order.clone());
            async move {
                b.acquire(None).await.unwrap();
                order.lock().unwrap().push(i);
            }
        };
        futures::future::join3(take(0), take(1), take(2)).await;
        assert_eq!(*order.lock().unwrap(), [0, 1, 2]);
    }

    #[tokio::test]
    async fn test_a_cancelled_waiter_gives_its_reservation_back() {
        let b = Arc::new(Bucket::per_minute(60.0, Some(1)));
        b.acquire(None).await.unwrap();
        let waiter = {
            let b = b.clone();
            tokio::spawn(async move { b.acquire(None).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        // One token owed, not two: a 1.5 s wait is enough.
        assert!(b.acquire(Some(Duration::from_millis(1500))).await.is_ok());
    }

    #[tokio::test]
    async fn max_wait_bounds_a_queued_wait() {
        let b = Bucket::per_second(0.5, 1); // one token every 2 s
        b.acquire(None).await.unwrap();
        let started = Instant::now();
        let r = b.acquire(Some(Duration::from_millis(300))).await;
        assert!(r.is_err());
        assert!(started.elapsed() < Duration::from_millis(400));
    }

    #[tokio::test]
    async fn implicit_buckets_and_penalties() {
        let l = RateLimits::new();
        // Default rps with burst = ceil(rps) = 2.
        assert!(
            l.acquire("h", Some(2.0), Some(Duration::ZERO))
                .await
                .is_ok()
        );
        assert!(
            l.acquire("h", Some(2.0), Some(Duration::ZERO))
                .await
                .is_ok()
        );
        assert!(
            l.acquire("h", Some(2.0), Some(Duration::ZERO))
                .await
                .is_err()
        );
        assert!(l.penalty("h").is_none());
        l.penalize("h", Duration::from_secs(30));
        l.penalize("h", Duration::from_secs(5)); // a shorter one does not shorten it
        let left = l.penalty("h").unwrap();
        assert!(left > Duration::from_secs(25) && left <= Duration::from_secs(30));
        l.penalize("k", Duration::from_secs(86_400));
        assert!(l.penalty("k").unwrap() <= MAX_RETRY_AFTER);
    }

    #[test]
    fn retry_after_forms() {
        assert_eq!(parse_retry_after("120"), Some(Duration::from_secs(120)));
        assert_eq!(parse_retry_after(" 0 "), Some(Duration::ZERO));
        assert_eq!(parse_retry_after("999999999"), Some(MAX_RETRY_AFTER));
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"),
            Some(Duration::ZERO)
        );
        let future = jiff::Timestamp::now() + jiff::SignedDuration::from_secs(90);
        let http_date = jiff::fmt::rfc2822::DateTimePrinter::new()
            .timestamp_to_rfc9110_string(&future)
            .unwrap();
        let d = parse_retry_after(&http_date).unwrap();
        assert!(
            d >= Duration::from_secs(85) && d <= Duration::from_secs(90),
            "{http_date} -> {d:?}"
        );
        assert_eq!(parse_retry_after("soon"), None);
        assert_eq!(parse_retry_after(""), None);
    }

    /// I2 regression (Missing test #7a): queued waiter with ~0.1s left + new
    /// caller arriving with `max_wait = 1.5s` on a `1 rps` bucket must succeed
    /// (actual wait ~1.1s <= 1.5s, not falsely rejected at `2.0s`).
    #[tokio::test]
    async fn queued_waiter_almost_ready_does_not_falsely_reject_new_caller() {
        let b = Arc::new(Bucket::per_second(1.0, 1));
        b.acquire(None).await.unwrap(); // t = 0.0s, drains burst
        tokio::time::sleep(Duration::from_millis(100)).await; // t = 0.1s
        let b_waiter = {
            let b = b.clone();
            tokio::spawn(async move { b.acquire(Some(Duration::from_millis(1500))).await })
        };
        tokio::time::sleep(Duration::from_millis(800)).await; // t = 0.9s (B has ~0.1s left)
        // C's true wait is ~1.1s <= 1.5s.
        let res_c = b.acquire(Some(Duration::from_millis(1500))).await;
        assert!(res_c.is_ok(), "C should succeed, got {res_c:?}");
        assert!(b_waiter.await.unwrap().is_ok());
    }

    /// I2 regression (Missing test #7b): two concurrent callers on a drained
    /// `1 rps` bucket with `max_wait = 1.5s` — one succeeds at ~1s, the second
    /// fails fast (<200ms) with the true remaining wait (~2s), not `max_wait`.
    #[tokio::test]
    async fn concurrent_callers_on_drained_bucket_fail_fast_with_true_wait() {
        let b = Arc::new(Bucket::per_second(1.0, 1));
        b.acquire(None).await.unwrap();
        let started = Instant::now();
        let (r1, r2) = tokio::join!(
            async {
                let r = b.acquire(Some(Duration::from_millis(1500))).await;
                (r, started.elapsed())
            },
            async {
                let r = b.acquire(Some(Duration::from_millis(1500))).await;
                (r, started.elapsed())
            }
        );
        let (ok_t, err_w, err_t) = match (r1, r2) {
            ((Ok(()), ok_t), (Err(w), err_t)) | ((Err(w), err_t), (Ok(()), ok_t)) => {
                (ok_t, w, err_t)
            }
            other => panic!("expected one Ok and one Err, got {other:?}"),
        };
        assert!(
            ok_t >= Duration::from_millis(800),
            "winner waited: {ok_t:?}"
        );
        assert!(
            err_t < Duration::from_millis(200),
            "loser must fail fast without burning budget, took {err_t:?}"
        );
        assert!(
            err_w >= Duration::from_millis(1800),
            "loser must see true ~2s wait, got {err_w:?}"
        );

        // Slow bucket (1 req / 60s) with short max_wait = 100ms reports ~60s, not 100ms.
        let slow = Bucket::per_minute(1.0, Some(1));
        slow.acquire(None).await.unwrap();
        let w = slow
            .acquire(Some(Duration::from_millis(100)))
            .await
            .unwrap_err();
        assert!(
            w >= Duration::from_secs(55),
            "expected ~60s retry_after, got {w:?}"
        );
    }

    /// I3 regression (Missing test #7c): two callers sharing a `host_bucket`
    /// with different `cx.rps` values share one rate limit.
    #[tokio::test]
    async fn shared_host_bucket_with_different_rps_shares_one_bucket() {
        let l = RateLimits::new();
        let key = "eutils.ncbi.nlm.nih.gov";
        // Caller 1 drains the burst of 3 at 3.0 rps.
        for _ in 0..3 {
            assert!(
                l.acquire(key, Some(3.0), Some(Duration::ZERO))
                    .await
                    .is_ok()
            );
        }
        // Caller 2 uses a different rps (2.0) on the same host_bucket: must be
        // throttled by the SAME shared bucket, not given a brand-new bucket.
        assert!(
            l.acquire(key, Some(2.0), Some(Duration::ZERO))
                .await
                .is_err(),
            "second rps must not split the shared host_bucket"
        );
    }

    /// I4 regression (Missing test #7d): extreme finite rates never panic.
    #[tokio::test]
    async fn extreme_rates_do_not_panic() {
        for rps in [1e-309, 1e-25, f64::MIN_POSITIVE, 1e308, f64::INFINITY] {
            let b = Bucket::per_second(rps, 1);
            let _ = b.acquire(Some(Duration::ZERO)).await;
        }
    }
}
