//! DNS for the SSRF guard and the HTTP clients.
//!
//! [`DnsLookup`] is the one resolver both stacks use. Direct (unproxied)
//! connections resolve through [`PinnedResolver`], which hands the client only
//! the address set layer 4 vetted (`SafeResolver` DNS pinning, `SPEC.md`
//! §7.2): the address that was checked is the address that is dialled, so a
//! DNS-rebinding answer between check and connect cannot redirect the socket.

use crate::ssrf::SsrfGuard;
use fsm_core::FsmError;
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Resolve a hostname to every A/AAAA address.
#[async_trait::async_trait]
pub trait DnsLookup: Send + Sync + 'static {
    async fn lookup(&self, host: &str) -> io::Result<Vec<IpAddr>>;
}

/// The operating system resolver (`getaddrinfo` on tokio's blocking pool).
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemDns;

#[async_trait::async_trait]
impl DnsLookup for SystemDns {
    async fn lookup(&self, host: &str) -> io::Result<Vec<IpAddr>> {
        let addrs = tokio::net::lookup_host((host, 0)).await?;
        Ok(addrs.map(|a| a.ip()).collect())
    }
}

/// `hickory-resolver 0.26` with the system configuration, querying A and
/// AAAA in parallel so a blocked record of either family is seen.
#[derive(Clone)]
pub struct HickoryDns {
    resolver: hickory_resolver::TokioResolver,
}

impl std::fmt::Debug for HickoryDns {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HickoryDns").finish_non_exhaustive()
    }
}

impl HickoryDns {
    /// Build from `/etc/resolv.conf` (Unix) or the registry (Windows).
    pub fn from_system() -> Result<Self, FsmError> {
        let mut builder = hickory_resolver::TokioResolver::builder_tokio().map_err(|e| {
            FsmError::internal(
                "dns",
                format!("cannot read the system DNS configuration: {e}"),
            )
        })?;
        builder.options_mut().ip_strategy = hickory_resolver::config::LookupIpStrategy::Ipv4AndIpv6;
        let resolver = builder.build().map_err(|e| {
            FsmError::internal("dns", format!("cannot build the DNS resolver: {e}"))
        })?;
        Ok(Self { resolver })
    }
}

#[async_trait::async_trait]
impl DnsLookup for HickoryDns {
    async fn lookup(&self, host: &str) -> io::Result<Vec<IpAddr>> {
        let found = self
            .resolver
            .lookup_ip(host)
            .await
            .map_err(|e| io::Error::other(e.to_string()))?;
        Ok(found.iter().collect())
    }
}

/// The default lookup: hickory with the system configuration, falling back
/// to the operating-system resolver when that configuration is unreadable.
pub fn default_lookup() -> Arc<dyn DnsLookup> {
    match HickoryDns::from_system() {
        Ok(h) => Arc::new(h),
        Err(e) => {
            tracing::warn!("hickory DNS unavailable ({e}); using the system resolver");
            Arc::new(SystemDns)
        }
    }
}

/// A fixed name -> addresses table (tests and offline tooling). Unknown names
/// fail like NXDOMAIN; every call is counted.
#[derive(Debug, Default)]
pub struct StaticDns {
    table: HashMap<String, Vec<IpAddr>>,
    fallback: Option<Vec<IpAddr>>,
    calls: AtomicUsize,
}

impl StaticDns {
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder: `host` resolves to `ips`.
    pub fn with(mut self, host: &str, ips: &[IpAddr]) -> Self {
        self.table.insert(host.to_ascii_lowercase(), ips.to_vec());
        self
    }

    /// Builder: every name not in the table resolves to `ips`.
    pub fn with_fallback(mut self, ips: &[IpAddr]) -> Self {
        self.fallback = Some(ips.to_vec());
        self
    }

    /// How many lookups were made.
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl DnsLookup for StaticDns {
    async fn lookup(&self, host: &str) -> io::Result<Vec<IpAddr>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let key = host.trim_end_matches('.').to_ascii_lowercase();
        match self.table.get(&key).or(self.fallback.as_ref()) {
            Some(ips) => Ok(ips.clone()),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no such host: {key}"),
            )),
        }
    }
}

/// Connect-time resolver for both HTTP stacks.
///
/// `vet = true` (direct connections under layer 4, and `socks4://`/`socks5://`
/// local-DNS proxy targets): serve only the vetted address set from the guard's
/// cache, except for `skip_host` (the operator-configured proxy's own hostname).
/// `vet = false` (remote-DNS proxied clients or a policy without layer 4): a
/// plain lookup through the same [`DnsLookup`].
#[derive(Clone)]
pub(crate) struct PinnedResolver {
    guard: Arc<SsrfGuard>,
    vet: bool,
    skip_host: Option<String>,
}

impl PinnedResolver {
    pub(crate) fn new(guard: Arc<SsrfGuard>, vet: bool, skip_host: Option<String>) -> Self {
        let skip_host = skip_host
            .map(|h| {
                h.trim()
                    .trim_matches(|c| c == '[' || c == ']')
                    .trim_end_matches('.')
                    .to_ascii_lowercase()
            })
            .filter(|h| !h.is_empty());
        Self {
            guard,
            vet,
            skip_host,
        }
    }

    async fn addrs(&self, host: String) -> Result<Vec<SocketAddr>, FsmError> {
        let key = host.trim_end_matches('.').to_ascii_lowercase();
        let is_skip = self.skip_host.as_deref() == Some(key.as_str());
        let ips: Vec<IpAddr> = if self.vet && !is_skip {
            self.guard.vetted_for_connect(&host).await?.as_ref().clone()
        } else {
            self.guard.dns().lookup(&host).await.map_err(|e| {
                FsmError::upstream(host.clone(), None, format!("DNS lookup failed: {e}"))
            })?
        };
        // Port 0 is replaced by the URL's port (or the scheme default) by both clients.
        Ok(ips.into_iter().map(|ip| SocketAddr::new(ip, 0)).collect())
    }
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

impl reqwest::dns::Resolve for PinnedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let this = self.clone();
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs = this
                .addrs(host)
                .await
                .map_err(|e| Box::new(e) as BoxError)?;
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

impl wreq::dns::Resolve for PinnedResolver {
    fn resolve(&self, name: wreq::dns::Name) -> wreq::dns::Resolving {
        let this = self.clone();
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs = this
                .addrs(host)
                .await
                .map_err(|e| Box::new(e) as BoxError)?;
            Ok(Box::new(addrs.into_iter()) as wreq::dns::Addrs)
        })
    }
}
