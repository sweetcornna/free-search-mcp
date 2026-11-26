//! `fsm-net`: the network layer of free-search-mcp (lane `W0b`, `SPEC.md` §7.1–7.2).
//!
//! * [`HttpClient`]: the live [`fsm_core::NetClient`]: reqwest + rustls(ring) for
//!   `plain`, wreq Chrome/Edge emulation for `chrome` / `edge_preflight`,
//!   manual redirects, bounded bodies, proxy scoping, cookie jars, GCRA.
//! * [`ssrf`]: the four-layer SSRF guard with the fake-IP canary and
//!   [`dns`] pinning; [`ssrf::check_offline`] is the DNS-free cache-hit check.
//! * [`ratelimit`]: `governor` GCRA buckets + `Retry-After` penalties.
//! * [`health`]: the per-engine circuit breaker (`health_state` rows).
//! * [`replay::ReplayNet`]: offline fixtures (implemented in `fsm-core`).

pub mod client;
pub mod dns;
mod flight;
pub mod health;
mod inflate;
pub mod jar;
pub mod ratelimit;
pub mod ssrf;
#[cfg(test)]
mod testutil;

pub use client::{
    CHROME_PROFILE, EDGE_PROFILE, HttpClient, HttpClientBuilder, MAX_REDIRECTS, PLAIN_USER_AGENT,
    install_ring_provider,
};
pub use dns::{DnsLookup, HickoryDns, StaticDns, SystemDns, default_lookup};
pub use health::{Benched, EngineHealth};
pub use ratelimit::{Bucket, RateLimits, parse_retry_after};
pub use ssrf::{SsrfGuard, Target, check_offline, in_fake_ip_candidate, ip_is_blocked};

/// Offline replay client; implemented in `fsm-core` so every lane can run
/// fixture tests without depending on `fsm-net` (A1 §B.9).
pub mod replay {
    pub use fsm_core::fixture::ReplayNet;
}
