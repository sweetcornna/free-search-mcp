//! Single-flight cold-origin warm-up (`SPEC.md` §7.1 "single-flight H2
//! warmup", Spike N §4.6): a cold fan-out to one HTTPS origin opened one TCP+TLS
//! connection per request. The first request to a cold origin goes alone; the
//! others wait (bounded) until its response headers arrive, then share its
//! HTTP/2 connection. Gates live in a `moka` cache (single-flight creation,
//! idle expiry shorter than the clients' 90 s pool idle timeout, so a gate is
//! cold again once its connection is likely gone).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Longest a follower waits for the leader before going on its own.
pub const MAX_FOLLOWER_WAIT: Duration = Duration::from_secs(2);
/// Idle time after which an origin counts as cold again.
pub const WARM_IDLE: Duration = Duration::from_secs(60);

#[derive(Debug, Default)]
pub(crate) struct Gate {
    warm: AtomicBool,
    lead: Arc<tokio::sync::Mutex<()>>,
}

/// What [`WarmGates::enter`] decided.
#[derive(Debug)]
pub(crate) enum Pass {
    /// The origin is warm: go.
    Warm,
    /// This request leads; call [`Pass::established`] once headers arrived.
    /// Dropping it (error, cancellation) hands the lead to the next waiter.
    Leader(
        #[allow(dead_code)] tokio::sync::OwnedMutexGuard<()>,
        Arc<Gate>,
    ),
    /// Waited [`MAX_FOLLOWER_WAIT`] (or the budget) without a verdict: go anyway.
    Unwaited,
}

impl Pass {
    /// The leader's connection is up: release the followers.
    pub(crate) fn established(self) {
        if let Pass::Leader(_, gate) = &self {
            gate.warm.store(true, Ordering::Release);
        }
    }
}

pub(crate) struct WarmGates {
    gates: moka::sync::Cache<String, Arc<Gate>>,
}

impl Default for WarmGates {
    fn default() -> Self {
        Self {
            gates: moka::sync::Cache::builder()
                .max_capacity(1024)
                .time_to_idle(WARM_IDLE)
                .build(),
        }
    }
}

impl WarmGates {
    /// Wait (at most `max_wait`) for the right to send to `origin`.
    pub(crate) async fn enter(&self, origin: &str, max_wait: Duration) -> Pass {
        let gate = self
            .gates
            .get_with(origin.to_owned(), || Arc::new(Gate::default()));
        if gate.warm.load(Ordering::Acquire) {
            return Pass::Warm;
        }
        match tokio::time::timeout(max_wait, gate.lead.clone().lock_owned()).await {
            Ok(guard) if !gate.warm.load(Ordering::Acquire) => Pass::Leader(guard, gate),
            Ok(_) => Pass::Warm,
            Err(_) => Pass::Unwaited,
        }
    }

    #[cfg(test)]
    pub(crate) fn is_warm(&self, origin: &str) -> bool {
        self.gates
            .get(origin)
            .is_some_and(|g| g.warm.load(Ordering::Acquire))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_first_request_leads_and_releases_the_rest() {
        let g = WarmGates::default();
        let o = "chrome|false|https://api.example:443";
        let lead = g.enter(o, MAX_FOLLOWER_WAIT).await;
        assert!(matches!(lead, Pass::Leader(..)));
        // A follower waits while the leader is in flight...
        let follower = g.enter(o, Duration::from_millis(50)).await;
        assert!(matches!(follower, Pass::Unwaited), "bounded wait");
        // ...and goes straight through once the connection is up.
        lead.established();
        assert!(g.is_warm(o));
        assert!(matches!(g.enter(o, Duration::ZERO).await, Pass::Warm));
    }

    #[tokio::test]
    async fn a_cancelled_leader_does_not_strand_its_followers() {
        let g = Arc::new(WarmGates::default());
        let o = "plain|false|https://slow.example:443";
        let lead = g.enter(o, MAX_FOLLOWER_WAIT).await;
        let waiting = {
            let g = g.clone();
            tokio::spawn(
                async move { matches!(g.enter(o, MAX_FOLLOWER_WAIT).await, Pass::Leader(..)) },
            )
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(lead); // cancelled or failed before headers: not warm
        assert!(waiting.await.unwrap(), "the next waiter took the lead");
        assert!(!g.is_warm(o));
    }
}
