//! SSRF guard: the four layers of the Python `url_safety.py` port
//! (`SPEC.md` §7.2, normative B1).
//!
//! 1. **Scheme**: `http` / `https` only. No DNS.
//! 2. **Address literals**: a host that *is* an address (after WHATWG
//!    canonicalisation, so `0x7f.1`, `2130706433` and `127。0。0。1` are all
//!    `127.0.0.1`) is checked directly. No DNS.
//! 3. **Internal names**: `localhost`, `*.internal`, `*.local`, cloud metadata
//!    names, ... are refused by name. No DNS.
//! 4. **Resolved addresses**: every A/AAAA record of the host is checked; one
//!    blocked record refuses the whole host ("mixed A records fail closed").
//!    The vetted set is cached for 30 s (successes only, 512 hosts) and is
//!    exactly what the HTTP clients connect to ([`crate::dns`] pins it).
//!
//! Layer 4 only means something when this process picks the destination, so
//! [`SsrfResolveMode::Auto`] skips it for a request that actually leaves
//! through the configured proxy, and excuses addresses in the fake-IP tunnel
//! ranges only after the canary hosts resolve there too (Clash / Surge /
//! sing-box in TUN mode). Loopback, RFC 1918 and link-local are never excused.
//! Layers 1–3 hold in every mode; `allow_private_hosts` lifts 2–4 but never 1.

use crate::dns::DnsLookup;
use fsm_core::{Config, FsmError, SsrfResolveMode};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;
use url::{Host, Url};

/// Hostnames that are public by definition. If they resolve into a fake-IP
/// candidate range, the resolver synthesises addresses for everything.
pub const CANARY_HOSTS: [&str; 2] = ["example.com", "a.root-servers.net"];

/// How long a successful layer-4 validation is reused.
pub const VETTED_TTL: Duration = Duration::from_secs(30);
/// How many hosts the layer-4 cache keeps.
pub const VETTED_CAPACITY: u64 = 512;

/// `(network, prefix)` tables, ported from CPython 3.12 `ipaddress`
/// (`_IPv4Constants` / `_IPv6Constants`) plus `url_safety._EXTRA_BLOCKED_NETWORKS`.
const V4_PRIVATE: &[(u32, u8)] = &[
    (0x0000_0000, 8),  // 0.0.0.0/8
    (0x0A00_0000, 8),  // 10.0.0.0/8
    (0x7F00_0000, 8),  // 127.0.0.0/8
    (0xA9FE_0000, 16), // 169.254.0.0/16
    (0xAC10_0000, 12), // 172.16.0.0/12
    (0xC000_0000, 24), // 192.0.0.0/24
    (0xC000_00AA, 31), // 192.0.0.170/31
    (0xC000_0200, 24), // 192.0.2.0/24
    (0xC0A8_0000, 16), // 192.168.0.0/16
    (0xC612_0000, 15), // 198.18.0.0/15
    (0xC633_6400, 24), // 198.51.100.0/24
    (0xCB00_7100, 24), // 203.0.113.0/24
    (0xF000_0000, 4),  // 240.0.0.0/4
    (0xFFFF_FFFF, 32), // 255.255.255.255/32
];
const V4_PRIVATE_EXCEPTIONS: &[(u32, u8)] = &[(0xC000_0009, 32), (0xC000_000A, 32)];
const V4_OTHER_BLOCKED: &[(u32, u8)] = &[
    (0x7F00_0000, 8),  // loopback
    (0xA9FE_0000, 16), // link-local (169.254.169.254 metadata)
    (0xE000_0000, 4),  // multicast
    (0xF000_0000, 4),  // reserved
    (0x6440_0000, 10), // CGNAT 100.64.0.0/10 (Alibaba metadata 100.100.100.200)
];

const V6_PRIVATE: &[(u128, u8)] = &[
    (0x0000_0000_0000_0000_0000_0000_0000_0001, 128), // ::1
    (0, 128),                                         // ::
    (0x0000_0000_0000_0000_0000_FFFF_0000_0000, 96),  // ::ffff:0:0/96
    (0x0064_FF9B_0001_0000_0000_0000_0000_0000, 48),  // 64:ff9b:1::/48
    (0x0100_0000_0000_0000_0000_0000_0000_0000, 64),  // 100::/64
    (0x2001_0000_0000_0000_0000_0000_0000_0000, 23),  // 2001::/23
    (0x2001_0DB8_0000_0000_0000_0000_0000_0000, 32),  // 2001:db8::/32
    (0x2002_0000_0000_0000_0000_0000_0000_0000, 16),  // 2002::/16
    (0x3FFF_0000_0000_0000_0000_0000_0000_0000, 20),  // 3fff::/20
    (0xFC00_0000_0000_0000_0000_0000_0000_0000, 7),   // fc00::/7
    (0xFE80_0000_0000_0000_0000_0000_0000_0000, 10),  // fe80::/10
];
const V6_PRIVATE_EXCEPTIONS: &[(u128, u8)] = &[
    (0x2001_0001_0000_0000_0000_0000_0000_0001, 128),
    (0x2001_0001_0000_0000_0000_0000_0000_0002, 128),
    (0x2001_0003_0000_0000_0000_0000_0000_0000, 32),
    (0x2001_0004_0112_0000_0000_0000_0000_0000, 48),
    (0x2001_0020_0000_0000_0000_0000_0000_0000, 28),
    (0x2001_0030_0000_0000_0000_0000_0000_0000, 28),
];
const V6_OTHER_BLOCKED: &[(u128, u8)] = &[
    (0xFE80_0000_0000_0000_0000_0000_0000_0000, 10), // link-local
    (0xFF00_0000_0000_0000_0000_0000_0000_0000, 8),  // multicast
    // reserved (CPython _reserved_networks)
    (0, 8),
    (0x0100_0000_0000_0000_0000_0000_0000_0000, 8),
    (0x0200_0000_0000_0000_0000_0000_0000_0000, 7),
    (0x0400_0000_0000_0000_0000_0000_0000_0000, 6),
    (0x0800_0000_0000_0000_0000_0000_0000_0000, 5),
    (0x1000_0000_0000_0000_0000_0000_0000_0000, 4),
    (0x4000_0000_0000_0000_0000_0000_0000_0000, 3),
    (0x6000_0000_0000_0000_0000_0000_0000_0000, 3),
    (0x8000_0000_0000_0000_0000_0000_0000_0000, 3),
    (0xA000_0000_0000_0000_0000_0000_0000_0000, 3),
    (0xC000_0000_0000_0000_0000_0000_0000_0000, 3),
    (0xE000_0000_0000_0000_0000_0000_0000_0000, 4),
    (0xF000_0000_0000_0000_0000_0000_0000_0000, 5),
    (0xF800_0000_0000_0000_0000_0000_0000_0000, 6),
    (0xFE00_0000_0000_0000_0000_0000_0000_0000, 9),
    // Deprecated site-local fec0::/10: not in CPython's tables, blocked here
    // as well (strict superset of the oracle; see the differential test).
    (0xFEC0_0000_0000_0000_0000_0000_0000_0000, 10),
];

/// Ranges a fake-IP tunnel hands out as synthetic handles. Never the real
/// private networks.
const FAKE_V4: &[(u32, u8)] = &[(0xC612_0000, 15), (0x1C00_0000, 8)]; // 198.18.0.0/15, 28.0.0.0/8
const FAKE_V6: &[(u128, u8)] = &[
    (0xFC00_0000_0000_0000_0000_0000_0000_0000, 7), // fc00::/7
    (0x2001_0002_0000_0000_0000_0000_0000_0000, 48), // 2001:2::/48
];

/// Exact hostnames that name an internal resource (refused without DNS).
const INTERNAL_HOST_EXACT: &[&str] = &[
    "localhost",
    "metadata",
    "metadata.google.internal",
    "metadata.goog",
    "instance-data",
    "instance-data.ec2.internal",
];
/// Hostname suffixes that name an internal resource (refused without DNS).
const INTERNAL_HOST_SUFFIXES: &[&str] = &[
    ".localhost",
    ".local",
    ".internal",
    ".intranet",
    ".private",
    ".corp",
    ".lan",
    ".home.arpa",
];

fn in_v4(ip: u32, nets: &[(u32, u8)]) -> bool {
    nets.iter().any(|&(base, prefix)| {
        let mask = if prefix == 0 {
            0
        } else {
            u32::MAX << (32 - u32::from(prefix))
        };
        ip & mask == base & mask
    })
}

fn in_v6(ip: u128, nets: &[(u128, u8)]) -> bool {
    nets.iter().any(|&(base, prefix)| {
        let mask = if prefix == 0 {
            0
        } else {
            u128::MAX << (128 - u32::from(prefix))
        };
        ip & mask == base & mask
    })
}

fn v4_blocked(ip: Ipv4Addr) -> bool {
    let n = u32::from(ip);
    (in_v4(n, V4_PRIVATE) && !in_v4(n, V4_PRIVATE_EXCEPTIONS)) || in_v4(n, V4_OTHER_BLOCKED)
}

/// True if `ip` is in a range this server must never fetch from: loopback,
/// link-local (incl. 169.254.169.254), private/RFC 1918/ULA, CGNAT
/// `100.64.0.0/10`, unspecified, multicast, reserved, and IPv4-mapped forms of
/// all of them. Mirrors `url_safety._ip_is_blocked` (CPython 3.12 tables).
pub fn ip_is_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4_blocked(v4),
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return v4_blocked(mapped);
            }
            let n = u128::from(v6);
            (in_v6(n, V6_PRIVATE) && !in_v6(n, V6_PRIVATE_EXCEPTIONS)) || in_v6(n, V6_OTHER_BLOCKED)
        }
    }
}

/// Bitflags for each individual fake-IP candidate range so an IPv4-only tunnel
/// (e.g. `198.18.0.0/15`) never excuses unverified ranges like `fc00::/7` (AWS IPv6 IMDS).
const FAKE_BIT_V4_BENCH: u8 = 1 << 0; // 198.18.0.0/15
const FAKE_BIT_V4_28: u8 = 1 << 1; // 28.0.0.0/8
const FAKE_BIT_V6_ULA: u8 = 1 << 2; // fc00::/7
const FAKE_BIT_V6_BENCH: u8 = 1 << 3; // 2001:2::/48

fn fake_ip_candidate_bit(ip: IpAddr) -> u8 {
    match ip {
        IpAddr::V4(v4) => {
            let n = u32::from(v4);
            if in_v4(n, &[FAKE_V4[0]]) {
                FAKE_BIT_V4_BENCH
            } else if in_v4(n, &[FAKE_V4[1]]) {
                FAKE_BIT_V4_28
            } else {
                0
            }
        }
        IpAddr::V6(v6) => {
            let n = u128::from(v6);
            if in_v6(n, &[FAKE_V6[0]]) {
                FAKE_BIT_V6_ULA
            } else if in_v6(n, &[FAKE_V6[1]]) {
                FAKE_BIT_V6_BENCH
            } else {
                0
            }
        }
    }
}

/// True if `ip` lies in a range a fake-IP tunnel may synthesise
/// (`198.18.0.0/15`, `28.0.0.0/8`, `fc00::/7`, `2001:2::/48`).
pub fn in_fake_ip_candidate(ip: IpAddr) -> bool {
    fake_ip_candidate_bit(ip) != 0
}

/// True when the hostname itself names an internal resource.
pub fn host_is_internal(host: &str) -> bool {
    let h = host.trim().trim_end_matches('.').to_ascii_lowercase();
    !h.is_empty()
        && (INTERNAL_HOST_EXACT.contains(&h.as_str())
            || INTERNAL_HOST_SUFFIXES.iter().any(|s| h.ends_with(s)))
}

fn blocked_message(ip: IpAddr) -> String {
    format!(
        "Refusing to connect to blocked address {ip} (loopback/link-local/private/reserved). \
         Set SEARCH_MCP_ALLOW_PRIVATE_HOSTS=true to override."
    )
}

/// A URL that passed layers 1–3.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Target {
    /// The canonical (WHATWG-serialised) URL: what the HTTP clients are given.
    pub url: Url,
    /// Lower-case ASCII host without a trailing dot (IPv6 without brackets).
    pub host: String,
    pub port: u16,
    /// `Some` when the host is an address literal (already checked).
    pub literal: Option<IpAddr>,
}

impl Target {
    /// `scheme://host:port`, the connection-pool identity.
    pub fn origin(&self) -> String {
        format!(
            "{}://{}:{}",
            self.url.scheme(),
            self.url.host_str().unwrap_or(""),
            self.port
        )
    }
}

/// Different stacks decode escapes, backslashes and odd whitespace in the
/// authority differently: refuse rather than guess which reading wins
/// (checked on the caller's raw spelling, before WHATWG normalisation).
pub(crate) fn check_raw_authority(raw: &str) -> Result<(), FsmError> {
    let s = raw.trim();
    let (scheme, after_scheme) = match s.find([':', '/', '\\', '?', '#']) {
        Some(i) if s.as_bytes()[i] == b':' => (Some(&s[..i]), &s[i + 1..]),
        _ => (None, s),
    };
    let is_http_scheme = scheme
        .is_some_and(|sc| sc.eq_ignore_ascii_case("http") || sc.eq_ignore_ascii_case("https"));
    // Only URLs/Locations carrying an authority (`http(s)://...`, `//...`,
    // `/\...`, `\...`) have a host to inspect. A relative path or query such as
    // `/login?next=https://example.com&msg=hello%20world` has no authority.
    let Some(b0) = after_scheme.as_bytes().first().copied() else {
        return Ok(());
    };
    let has_authority = is_http_scheme
        || matches!(
            (b0, after_scheme.as_bytes().get(1).copied()),
            (b'/', Some(b'/' | b'\\')) | (b'\\', _)
        );
    if !has_authority {
        return Ok(());
    }
    if !after_scheme.starts_with("//") || after_scheme.starts_with("///") {
        return Err(FsmError::UnsafeUrl(
            "Ambiguous or escaped URL host is not allowed".into(),
        ));
    }
    let rest = &after_scheme[2..];
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let netloc = &rest[..end];
    let raw_host = netloc.rsplit('@').next().unwrap_or(netloc);
    if netloc.contains('\\')
        || raw_host.contains('%')
        || raw_host.chars().any(|c| u32::from(c) <= 32)
    {
        return Err(FsmError::UnsafeUrl(
            "Ambiguous or escaped URL host is not allowed".into(),
        ));
    }
    Ok(())
}

/// Layers 1–3 (no DNS). `allow_private` lifts layers 2–3 (the scheme check
/// always holds). Returns the canonical target on success.
pub fn check_offline(raw: &str, allow_private: bool) -> Result<Target, FsmError> {
    let url =
        Url::parse(raw.trim()).map_err(|e| FsmError::UnsafeUrl(format!("Invalid URL: {e}")))?;
    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(FsmError::UnsafeUrl(format!(
            "Refusing URL with scheme '{scheme}': only http and https are allowed."
        )));
    }
    let Some(host) = url.host() else {
        return Err(FsmError::UnsafeUrl("URL has no host to validate".into()));
    };
    if !url.username().is_empty() || url.password().is_some() {
        return Err(FsmError::UnsafeUrl(
            "Credentials in the URL (user:pass@host) are not allowed".into(),
        ));
    }
    let port = url.port_or_known_default().unwrap_or(80);
    let (host_s, literal) = match host {
        Host::Ipv4(a) => (a.to_string(), Some(IpAddr::V4(a))),
        Host::Ipv6(a) => (a.to_string(), Some(IpAddr::V6(a))),
        Host::Domain(d) => {
            let trimmed = d.strip_suffix('.').unwrap_or(d);
            if trimmed.is_empty()
                || trimmed.starts_with('.')
                || trimmed.ends_with('.')
                || trimmed.contains("..")
            {
                return Err(FsmError::UnsafeUrl(format!("Invalid URL host '{d}'")));
            }
            let lower = trimmed.to_ascii_lowercase();
            match Host::parse(&lower) {
                Ok(Host::Ipv4(a)) => (a.to_string(), Some(IpAddr::V4(a))),
                Ok(Host::Ipv6(a)) => (a.to_string(), Some(IpAddr::V6(a))),
                _ => (lower, None),
            }
        }
    };
    if host_s.is_empty() {
        return Err(FsmError::UnsafeUrl("URL has no host to validate".into()));
    }
    let target = Target {
        url: url.clone(),
        host: host_s,
        port,
        literal,
    };
    if allow_private {
        return Ok(target);
    }
    check_raw_authority(raw.trim())?;
    if let Some(ip) = target.literal {
        if ip_is_blocked(ip) {
            return Err(FsmError::UnsafeUrl(blocked_message(ip)));
        }
        return Ok(target);
    }
    if host_is_internal(&target.host) {
        return Err(FsmError::UnsafeUrl(format!(
            "Refusing to connect to internal hostname '{}' (localhost / .internal / .local / cloud metadata). \
             Set SEARCH_MCP_ALLOW_PRIVATE_HOSTS=true to override.",
            target.host
        )));
    }
    Ok(target)
}

/// Validate an already-resolved address (redirect-hop helper, Python
/// `assert_ip_allowed`).
pub fn assert_ip_allowed(ip: IpAddr, allow_private: bool) -> Result<(), FsmError> {
    if ip_is_blocked(ip) && !allow_private {
        return Err(FsmError::UnsafeUrl(blocked_message(ip)));
    }
    Ok(())
}

/// The process-wide SSRF policy plus layer 4 (DNS resolve-and-check, fake-IP
/// canary, vetted-address cache). One per [`crate::HttpClient`].
pub struct SsrfGuard {
    allow_private: bool,
    mode: SsrfResolveMode,
    dns: Arc<dyn DnsLookup>,
    /// host -> vetted addresses. `try_get_with` coalesces concurrent lookups
    /// of one host (moka single-flight) and never caches a failure.
    vetted: moka::future::Cache<String, Arc<Vec<IpAddr>>>,
    /// Definitive bitmask of fake-IP candidate ranges proven by ALL canaries,
    /// memoised per guard (`0` = honest resolver / no common range); a DNS
    /// failure is not memoised (it proves nothing either way).
    canary: tokio::sync::OnceCell<u8>,
    #[cfg(test)]
    pub(crate) exempt: std::sync::Mutex<Vec<std::net::SocketAddr>>,
}

impl std::fmt::Debug for SsrfGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SsrfGuard")
            .field("allow_private", &self.allow_private)
            .field("mode", &self.mode)
            .field("canary", &self.canary.get())
            .finish_non_exhaustive()
    }
}

impl SsrfGuard {
    pub fn new(allow_private: bool, mode: SsrfResolveMode, dns: Arc<dyn DnsLookup>) -> Self {
        Self {
            allow_private,
            mode,
            dns,
            vetted: moka::future::Cache::builder()
                .max_capacity(VETTED_CAPACITY)
                .time_to_live(VETTED_TTL)
                .build(),
            canary: tokio::sync::OnceCell::new(),
            #[cfg(test)]
            exempt: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// `allow_private_hosts` + `ssrf_resolve_addresses` from the config.
    pub fn from_config(cfg: &Config, dns: Arc<dyn DnsLookup>) -> Self {
        Self::new(cfg.allow_private_hosts, cfg.ssrf_resolve_addresses, dns)
    }

    pub fn allow_private(&self) -> bool {
        self.allow_private
    }

    pub fn mode(&self) -> SsrfResolveMode {
        self.mode
    }

    pub(crate) fn dns(&self) -> &Arc<dyn DnsLookup> {
        &self.dns
    }

    /// Layers 1–3 (no DNS): run before serving anything from cache.
    pub fn check_offline(&self, url: &str) -> Result<Target, FsmError> {
        self.check_offline_inner(url)
    }

    /// Whether layer 4 applies to a request that does (`proxied`) or does not
    /// leave through the configured proxy.
    pub fn requires_resolution(&self, proxied: bool) -> bool {
        if self.allow_private {
            return false;
        }
        match self.mode {
            SsrfResolveMode::Never => false,
            SsrfResolveMode::Always => true,
            _ => !proxied,
        }
    }

    /// All four layers for `url`; `proxied` says whether the request leaves
    /// through the proxy (it decides layer 4 in `auto` mode).
    pub async fn check(&self, url: &str, proxied: bool) -> Result<Target, FsmError> {
        let t = self.check_offline_inner(url)?;
        if t.literal.is_none() && self.requires_resolution(proxied) {
            self.vet(&t.host, t.port).await?;
        }
        Ok(t)
    }

    fn check_offline_inner(&self, url: &str) -> Result<Target, FsmError> {
        #[cfg(test)]
        {
            // Test-only: the exempt socket address of a local server passes
            // layer 2 as if it were public; everything else is unchanged.
            match check_offline(url, self.allow_private) {
                Err(e) => {
                    if let Ok(t) = check_offline(url, true)
                        && let Some(ip) = t.literal
                        && self.is_exempt(ip, t.port)
                    {
                        return Ok(t);
                    }
                    Err(e)
                }
                ok => ok,
            }
        }
        #[cfg(not(test))]
        {
            check_offline(url, self.allow_private)
        }
    }

    #[cfg(test)]
    fn is_exempt(&self, ip: IpAddr, port: u16) -> bool {
        self.exempt
            .lock()
            .map(|v| v.iter().any(|s| s.ip() == ip && s.port() == port))
            .unwrap_or(false)
    }

    /// Layer 4: resolve `host` and check every address; the vetted set is
    /// cached for [`VETTED_TTL`] and is what the pinned resolvers hand to the
    /// HTTP clients.
    pub async fn vet(&self, host: &str, port: u16) -> Result<Arc<Vec<IpAddr>>, FsmError> {
        let key = host.trim_end_matches('.').to_ascii_lowercase();
        self.vetted
            .try_get_with(key.clone(), async {
                self.resolve_and_check(&key, port).await.map(Arc::new)
            })
            .await
            .map_err(|e: Arc<String>| FsmError::UnsafeUrl((*e).clone()))
    }

    /// The cached vetted set for `host`, re-vetting on a miss (connect-time
    /// path of the pinned resolver).
    pub(crate) async fn vetted_for_connect(
        &self,
        host: &str,
    ) -> Result<Arc<Vec<IpAddr>>, FsmError> {
        let key = host.trim_end_matches('.').to_ascii_lowercase();
        if let Some(v) = self.vetted.get(&key).await {
            return Ok(v);
        }
        self.vet(&key, 0).await
    }

    async fn resolve_and_check(&self, host: &str, port: u16) -> Result<Vec<IpAddr>, String> {
        let mut ips =
            self.dns.lookup(host).await.map_err(|e| {
                format!("Could not resolve host '{host}': {e}. Refusing to connect.")
            })?;
        ips.sort();
        ips.dedup();
        if ips.is_empty() {
            return Err(format!(
                "Host '{host}' resolved to no addresses. Refusing to connect."
            ));
        }
        for &ip in &ips {
            if !ip_is_blocked(ip) {
                continue;
            }
            #[cfg(test)]
            if self.is_exempt(ip, port) {
                continue;
            }
            let _ = port;
            // A synthetic tunnel address is a handle, not a destination. Only
            // ever excused for the exact candidate range(s) proven by ALL
            // canary hosts, only in auto mode (so an IPv4-only 198.18/15 TUN
            // never excuses fc00::/7 / fd00:ec2::254).
            let bit = fake_ip_candidate_bit(ip);
            if bit != 0
                && self.mode == SsrfResolveMode::Auto
                && self
                    .fake_ip_ranges()
                    .await
                    .is_some_and(|mask| (mask & bit) != 0)
            {
                continue;
            }
            return Err(blocked_message(ip));
        }
        Ok(ips)
    }

    /// Bitmask of fake-IP candidate ranges present across every canary host.
    /// `None` when a canary could not be resolved (not memoised); `Some(0)`
    /// when the resolver is honest (or canaries do not share a fake-IP range).
    async fn fake_ip_ranges(&self) -> Option<u8> {
        self.canary
            .get_or_try_init(|| async {
                let mut active_mask: u8 = u8::MAX;
                for canary in CANARY_HOSTS {
                    let ips = self.dns.lookup(canary).await.map_err(|_| ())?;
                    if ips.is_empty() {
                        return Err(());
                    }
                    let mut host_mask: u8 = 0;
                    for &ip in &ips {
                        let bit = fake_ip_candidate_bit(ip);
                        if bit == 0 {
                            // A canary landed on a real address: the resolver is honest.
                            return Ok(0);
                        }
                        host_mask |= bit;
                    }
                    active_mask &= host_mask;
                }
                Ok(active_mask)
            })
            .await
            .ok()
            .copied()
    }

    /// Whether the resolver maps public canary names into fake-IP ranges.
    /// `None` when the canaries could not be resolved (not memoised).
    /// Memoised per guard; concurrent callers share one probe.
    pub async fn fake_ip_verdict(&self) -> Option<bool> {
        self.fake_ip_ranges().await.map(|mask| mask != 0)
    }

    /// Drop every cached validation (tests re-stub the resolver).
    pub fn clear_cache(&self) {
        self.vetted.invalidate_all();
    }
}

#[cfg(test)]
mod tests {
    //! Ports of `tests/test_url_safety.py` and `tests/test_url_safety_proxy.py`
    //! (one async guard, so the Python sync/async parametrisation collapses),
    //! plus a differential vector against CPython 3.12 `ipaddress`.
    use super::*;
    use crate::dns::StaticDns;
    use std::net::Ipv6Addr;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn guard_with(dns: StaticDns, mode: SsrfResolveMode, allow_private: bool) -> SsrfGuard {
        SsrfGuard::new(allow_private, mode, Arc::new(dns))
    }

    fn auto(dns: StaticDns) -> SsrfGuard {
        guard_with(dns, SsrfResolveMode::Auto, false)
    }

    fn public_dns() -> StaticDns {
        StaticDns::new().with_fallback(&[ip("93.184.216.34")])
    }

    /// Proxy-only egress: nothing resolves.
    fn dead_dns() -> StaticDns {
        StaticDns::new()
    }

    /// A resolver answering everything with a blackhole address.
    fn poisoned_dns() -> StaticDns {
        StaticDns::new().with_fallback(&[ip("0.0.0.0")])
    }

    /// Every host into 198.18.0.0/15 + a v6 ULA, as a TUN tunnel does.
    fn fake_ip_dns() -> StaticDns {
        StaticDns::new().with_fallback(&[ip("198.18.0.7"), ip("fdfe:dcba:9876::7")])
    }

    async fn blocked(g: &SsrfGuard, url: &str, proxied: bool) -> bool {
        matches!(g.check(url, proxied).await, Err(FsmError::UnsafeUrl(_)))
    }

    // --- test_url_safety.py ---------------------------------------------

    #[tokio::test]
    async fn test_rejects_non_http_schemes() {
        let g = auto(public_dns());
        for url in [
            "file:///etc/passwd",
            "ftp://x/",
            "gopher://x/",
            "data:text/plain,hi",
            "javascript:alert(1)",
        ] {
            assert!(blocked(&g, url, false).await, "{url}");
        }
    }

    #[tokio::test]
    async fn test_rejects_blocked_ip_literals() {
        let g = auto(public_dns());
        for url in [
            "http://169.254.169.254/latest/meta-data/",
            "http://127.0.0.1:8080/",
            "http://10.0.0.1/",
            "http://192.168.1.1/",
            "http://172.16.0.1/",
            "http://[::1]/",
            "http://0.0.0.0/",
            // inet_aton / WHATWG spellings of loopback and mapped forms
            "http://0x7f.1/",
            "http://2130706433/",
            "http://0177.0.0.1/",
            "http://127.1/",
            "http://127.0.0.1./",
            "http://[::ffff:127.0.0.1]/",
            "http://127\u{3002}0\u{3002}0\u{3002}1/",
        ] {
            assert!(blocked(&g, url, false).await, "{url}");
        }
    }

    #[tokio::test]
    async fn test_rejects_metadata_host_via_dns() {
        let g = auto(StaticDns::new().with_fallback(&[ip("169.254.169.254")]));
        assert!(blocked(&g, "http://metadata.internal.example/", false).await);
    }

    #[tokio::test]
    async fn test_rejects_when_any_resolved_ip_is_blocked() {
        // Mixed A records: one public, one private -> must block.
        let g = auto(StaticDns::new().with_fallback(&[ip("93.184.216.34"), ip("10.1.2.3")]));
        assert!(blocked(&g, "http://rebind.example/", false).await);
        // Mixed families: a public A with a loopback AAAA.
        let g = auto(StaticDns::new().with_fallback(&[ip("93.184.216.34"), ip("::1")]));
        assert!(blocked(&g, "http://rebind6.example/", false).await);
    }

    #[tokio::test]
    async fn test_dns_failure_fails_closed() {
        let g = auto(dead_dns());
        assert!(blocked(&g, "http://does-not-resolve.invalid/", false).await);
    }

    #[tokio::test]
    async fn test_allows_public_ip_literal() {
        let g = auto(dead_dns());
        let t = g.check("http://93.184.216.34/", false).await.unwrap();
        assert_eq!(t.url.as_str(), "http://93.184.216.34/");
        assert_eq!(t.literal, Some(ip("93.184.216.34")));
    }

    #[tokio::test]
    async fn test_allows_public_hostname() {
        let g = auto(public_dns());
        let t = g
            .check("https://example.com/path?q=1", false)
            .await
            .unwrap();
        assert_eq!(t.url.as_str(), "https://example.com/path?q=1");
        assert_eq!((t.host.as_str(), t.port), ("example.com", 443));
    }

    #[test]
    fn test_assert_ip_allowed_passes_public() {
        assert!(assert_ip_allowed(ip("93.184.216.34"), false).is_ok());
    }

    #[test]
    fn test_assert_ip_allowed_rejects_blocked() {
        for a in ["127.0.0.1", "169.254.169.254", "10.0.0.1", "::1"] {
            assert!(assert_ip_allowed(ip(a), false).is_err(), "{a}");
        }
    }

    #[tokio::test]
    async fn test_allow_private_bypasses_url_check() {
        let dns = Arc::new(dead_dns());
        let g = SsrfGuard::new(true, SsrfResolveMode::Auto, dns.clone());
        assert!(g.check("http://127.0.0.1:8080/admin", false).await.is_ok());
        assert!(g.check("http://localhost:8080/admin", false).await.is_ok());
        assert_eq!(dns.calls(), 0, "no DNS when the escape hatch is on");
    }

    #[tokio::test]
    async fn test_allow_private_bypasses_metadata() {
        let g = guard_with(dead_dns(), SsrfResolveMode::Auto, true);
        assert!(
            g.check("http://169.254.169.254/latest/meta-data/", false)
                .await
                .is_ok()
        );
    }

    #[test]
    fn test_allow_private_bypasses_ip_check() {
        assert!(assert_ip_allowed(ip("10.0.0.1"), true).is_ok());
    }

    #[tokio::test]
    async fn test_allow_private_still_rejects_bad_scheme() {
        let g = guard_with(dead_dns(), SsrfResolveMode::Auto, true);
        assert!(blocked(&g, "file:///etc/passwd", false).await);
    }

    // --- test_url_safety_proxy.py ---------------------------------------

    #[tokio::test]
    async fn test_proxied_fetch_survives_dead_local_dns() {
        let g = auto(dead_dns());
        assert!(
            g.check("https://www.google.com/search?q=x", true)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn test_proxied_fetch_survives_poisoned_local_dns() {
        let g = auto(poisoned_dns());
        assert!(
            g.check("https://www.google.com/search?q=x", true)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn test_unproxied_fetch_still_fails_closed_on_dead_dns() {
        let g = auto(dead_dns());
        assert!(blocked(&g, "https://www.google.com/search?q=x", false).await);
    }

    #[tokio::test]
    async fn test_unproxied_fetch_still_blocks_a_private_resolution() {
        let g = auto(poisoned_dns());
        assert!(blocked(&g, "https://internal.example.com/", false).await);
    }

    #[tokio::test]
    async fn test_always_mode_restores_strict_resolution_under_proxy() {
        let g = guard_with(dead_dns(), SsrfResolveMode::Always, false);
        assert!(blocked(&g, "https://www.google.com/search?q=x", true).await);
    }

    #[tokio::test]
    async fn test_never_mode_skips_resolution_entirely() {
        let dns = Arc::new(dead_dns());
        let g = SsrfGuard::new(false, SsrfResolveMode::Never, dns.clone());
        assert!(
            g.check("https://www.google.com/search?q=x", false)
                .await
                .is_ok()
        );
        assert_eq!(dns.calls(), 0);
        // ...but the DNS-free layers still hold.
        for url in ["http://127.0.0.1/", "http://metadata.google.internal/"] {
            assert!(blocked(&g, url, false).await, "{url}");
        }
    }

    #[tokio::test]
    async fn test_fake_ip_tunnel_does_not_block_public_hosts() {
        let g = auto(fake_ip_dns());
        assert!(g.check("https://example.com/", false).await.is_ok());
        assert!(
            g.check("https://www.google.com/search?q=x", false)
                .await
                .is_ok()
        );
        assert_eq!(g.fake_ip_verdict().await, Some(true));
    }

    #[tokio::test]
    async fn test_fake_ip_tunnel_on_the_v6_benchmarking_range() {
        let g = auto(StaticDns::new().with_fallback(&[ip("198.18.1.98"), ip("2001:2::16f")]));
        assert!(g.check("https://docs.python.org/3/", false).await.is_ok());
        assert!(blocked(&g, "http://127.0.0.1:8080/", false).await);
    }

    #[tokio::test]
    async fn test_fake_ip_tunnel_still_blocks_the_dns_free_layers() {
        let g = auto(fake_ip_dns());
        for url in [
            "http://127.0.0.1:8080/",
            "http://169.254.169.254/latest/meta-data/",
            "http://10.0.0.1/",
            "http://localhost/",
            "http://metadata.google.internal/v1/",
            "file:///etc/passwd",
        ] {
            assert!(blocked(&g, url, false).await, "{url}");
        }
    }

    #[tokio::test]
    async fn test_fake_ip_detection_never_excuses_rfc1918() {
        let g = auto(StaticDns::new().with_fallback(&[ip("10.0.0.5")]));
        assert!(blocked(&g, "https://example.org/", false).await);
    }

    #[tokio::test]
    async fn fake_ip_verdict_never_excuses_loopback_mixed_into_tunnel_answers() {
        // A tunnel is detected, yet one record is loopback: never excused.
        let dns = StaticDns::new()
            .with("evil.example", &[ip("198.18.0.9"), ip("127.0.0.1")])
            .with_fallback(&[ip("198.18.0.3")]);
        let g = auto(dns);
        assert!(blocked(&g, "https://evil.example/", false).await);
        assert!(g.check("https://fine.example/", false).await.is_ok());
    }

    #[tokio::test]
    async fn test_honest_resolver_still_blocks_a_tunnel_range_result() {
        let dns = StaticDns::new()
            .with("example.com", &[ip("93.184.216.34")])
            .with("a.root-servers.net", &[ip("198.41.0.4")])
            .with_fallback(&[ip("198.18.0.99")]);
        let g = auto(dns);
        assert!(blocked(&g, "https://evil.example.net/", false).await);
        assert_eq!(g.fake_ip_verdict().await, Some(false));
    }

    #[tokio::test]
    async fn test_fake_ip_detection_is_off_in_always_mode() {
        let g = guard_with(fake_ip_dns(), SsrfResolveMode::Always, false);
        assert!(blocked(&g, "https://example.com/", false).await);
    }

    #[tokio::test]
    async fn canary_dns_failure_is_not_memoised() {
        // No answer for the canaries proves nothing: stay strict, ask again later.
        let g = auto(StaticDns::new().with("host.example", &[ip("198.18.0.4")]));
        assert_eq!(g.fake_ip_verdict().await, None);
        assert!(blocked(&g, "https://host.example/", false).await);
    }

    #[tokio::test]
    async fn test_address_literals_blocked_even_when_proxied() {
        let g = auto(dead_dns());
        for url in [
            "http://127.0.0.1:8080/",
            "http://[::1]/",
            "http://169.254.169.254/latest/meta-data/",
            "http://10.0.0.1/",
            "http://192.168.1.1/",
            "http://0.0.0.0/",
            "http://100.100.100.200/latest/meta-data/",
        ] {
            assert!(blocked(&g, url, true).await, "{url}");
        }
    }

    #[tokio::test]
    async fn test_internal_hostnames_blocked_even_when_proxied() {
        let g = auto(dead_dns());
        for url in [
            "http://localhost:8080/",
            "http://metadata.google.internal/computeMetadata/v1/",
            "http://metadata.goog/",
            "http://instance-data/latest/meta-data/",
            "http://foo.internal/",
            "http://printer.local/",
            "http://db.corp/",
            "http://nas.lan/",
            "http://svc.home.arpa/",
            "http://app.localhost/",
        ] {
            assert!(blocked(&g, url, true).await, "{url}");
        }
    }

    #[tokio::test]
    async fn test_internal_hostname_check_is_case_and_dot_insensitive() {
        let g = auto(dead_dns());
        for url in ["http://LOCALHOST/", "http://Metadata.Google.Internal./"] {
            assert!(blocked(&g, url, true).await, "{url}");
        }
    }

    #[tokio::test]
    async fn test_scheme_check_holds_when_proxied() {
        let g = auto(dead_dns());
        for url in ["file:///etc/passwd", "gopher://x/"] {
            assert!(blocked(&g, url, true).await, "{url}");
        }
    }

    #[tokio::test]
    async fn test_ordinary_public_hostnames_are_not_caught_by_the_name_check() {
        let g = auto(dead_dns());
        for url in [
            "https://example.com/",
            "https://api.internal-tools.com/",
            "https://local.google.com/",
            "https://corp.example.org/",
        ] {
            assert!(g.check(url, true).await.is_ok(), "{url}");
        }
    }

    #[tokio::test]
    async fn test_alibaba_metadata_range_blocked_unproxied() {
        let g = auto(StaticDns::new().with_fallback(&[ip("100.100.100.200")]));
        assert!(blocked(&g, "https://evil.example.com/", false).await);
    }

    #[tokio::test]
    async fn test_offline_guard_needs_no_dns() {
        let dns = Arc::new(public_dns());
        let g = SsrfGuard::new(false, SsrfResolveMode::Always, dns.clone());
        assert!(g.check_offline("https://example.com/").is_ok());
        assert!(g.check_offline("http://localhost/").is_err());
        assert!(g.check_offline("http://169.254.169.254/").is_err());
        assert_eq!(dns.calls(), 0, "the offline guard performed a DNS lookup");
    }

    #[tokio::test]
    async fn test_cache_hit_cannot_bypass_the_guard() {
        // What a cache-serving caller runs (NetClient::check_url_offline).
        let g = auto(public_dns());
        assert!(g.check_offline("https://example.com/").is_ok());
        for url in [
            "http://169.254.169.254/latest/meta-data/",
            "http://metadata.google.internal/v1/",
            "http://127.0.0.1:8000/",
        ] {
            assert!(g.check_offline(url).is_err(), "{url}");
        }
    }

    #[tokio::test]
    async fn test_public_addresses_still_allowed() {
        let g = auto(public_dns());
        assert!(g.check("https://example.com/", false).await.is_ok());
    }

    #[tokio::test]
    async fn ambiguous_and_escaped_hosts_are_refused() {
        let g = auto(public_dns());
        for url in [
            "http://%31%32%37.0.0.1/",
            "http://example.com%2f@127.0.0.1/",
            "http://evil.com\\@127.0.0.1/",
            "http://user:pass@example.com/",
            "http://example.com:99999/",
            "not a url",
            "http://",
            "http://[::1/",
        ] {
            assert!(blocked(&g, url, false).await, "{url}");
        }
    }

    #[tokio::test]
    async fn vetted_lookups_are_cached_and_single_flight() {
        let dns = Arc::new(public_dns());
        let g = Arc::new(SsrfGuard::new(false, SsrfResolveMode::Auto, dns.clone()));
        let jobs: Vec<_> = (0..16)
            .map(|_| {
                let g = g.clone();
                tokio::spawn(async move { g.check("https://same.example/", false).await.is_ok() })
            })
            .collect();
        for j in jobs {
            assert!(j.await.unwrap());
        }
        assert_eq!(
            dns.calls(),
            1,
            "16 concurrent checks of one host share one lookup"
        );
        // Failures are not cached.
        let dead = Arc::new(dead_dns());
        let g = SsrfGuard::new(false, SsrfResolveMode::Auto, dead.clone());
        assert!(blocked(&g, "https://x.example/", false).await);
        assert!(blocked(&g, "https://x.example/", false).await);
        assert_eq!(dead.calls(), 2);
    }

    #[test]
    fn requires_resolution_matrix() {
        let g = |allow, mode| SsrfGuard::new(allow, mode, Arc::new(StaticDns::new()));
        assert!(g(false, SsrfResolveMode::Auto).requires_resolution(false));
        assert!(!g(false, SsrfResolveMode::Auto).requires_resolution(true));
        assert!(g(false, SsrfResolveMode::Always).requires_resolution(true));
        assert!(!g(false, SsrfResolveMode::Never).requires_resolution(false));
        assert!(!g(true, SsrfResolveMode::Always).requires_resolution(false));
    }

    #[test]
    fn fake_ip_candidates_are_only_the_tunnel_ranges() {
        for a in [
            "198.18.0.1",
            "198.19.255.255",
            "28.1.2.3",
            "fc00::1",
            "fdfe:dcba:9876::1",
            "2001:2::16f",
        ] {
            assert!(in_fake_ip_candidate(ip(a)), "{a}");
        }
        for a in [
            "127.0.0.1",
            "10.0.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "198.20.0.1",
            "::1",
            "fe80::1",
        ] {
            assert!(!in_fake_ip_candidate(ip(a)), "{a}");
        }
    }

    #[test]
    fn ip_tables_match_cpython_ipaddress() {
        // Generated read-only from CPython 3.12.13 `ipaddress` with
        // url_safety._ip_is_blocked's rule (boundaries of every range ±1).
        let raw = include_str!("../tests/fixtures/ip_vectors.json");
        let vectors: Vec<(String, bool)> = serde_json::from_str(raw).unwrap();
        assert!(vectors.len() > 300);
        let site_local =
            |a: IpAddr| matches!(a, IpAddr::V6(v) if (u128::from(v) >> 118) == (0xFEC0u128 >> 6));
        for (addr, py) in vectors {
            let a = ip(&addr);
            let ours = ip_is_blocked(a);
            if site_local(a) {
                assert!(
                    ours,
                    "{addr}: fec0::/10 is blocked here (superset of CPython)"
                );
            } else {
                assert_eq!(ours, py, "{addr}");
            }
        }
        assert!(ip_is_blocked(IpAddr::V6(Ipv6Addr::new(
            0xfec0, 0, 0, 0, 0, 0, 0, 1
        ))));
    }
    /// The connect-time resolver's miss path (TTL lapsed, entry evicted or
    /// never admitted by moka): it re-runs the full vetting, never a bare
    /// lookup, so a rebinding answer is refused instead of dialled.
    #[tokio::test]
    async fn a_lapsed_vetted_entry_is_vetted_again_not_trusted() {
        struct Rebinder(std::sync::atomic::AtomicUsize);
        #[async_trait::async_trait]
        impl DnsLookup for Rebinder {
            async fn lookup(&self, _host: &str) -> std::io::Result<Vec<IpAddr>> {
                let n = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(vec![if n == 0 {
                    ip("93.184.216.34")
                } else {
                    ip("10.0.0.1")
                }])
            }
        }
        let dns = Arc::new(Rebinder(std::sync::atomic::AtomicUsize::new(0)));
        let g = SsrfGuard::new(false, SsrfResolveMode::Auto, dns.clone());
        g.vet("rebind.example", 443).await.unwrap();
        let hit = g.vetted_for_connect("REBIND.example.").await.unwrap();
        assert_eq!(
            hit.as_slice(),
            &[ip("93.184.216.34")],
            "served from the vetted set"
        );
        assert_eq!(dns.0.load(std::sync::atomic::Ordering::SeqCst), 1);
        g.clear_cache();
        let miss = g.vetted_for_connect("rebind.example").await;
        assert!(matches!(miss, Err(FsmError::UnsafeUrl(_))), "{miss:?}");
        assert_eq!(
            dns.0.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "re-vetted on the miss"
        );
        // A host never vetted at all (as if the insert was not admitted).
        assert!(g.vetted_for_connect("other.example").await.is_err());
    }

    /// B1 regression (Missing test #1): IP literals with >= 2 trailing dots or
    /// empty labels must be rejected across offline, proxied, and `Never` modes.
    #[tokio::test]
    async fn ssrf_l2_rejects_double_dot_and_empty_label_ip_literals() {
        let auto_g = auto(public_dns());
        let never_g = guard_with(dead_dns(), SsrfResolveMode::Never, false);
        for url in [
            "http://127.0.0.1../",
            "http://169.254.169.254../latest/meta-data/",
            "http://0x7f.1../",
            "http://2130706433../",
            "http://.127.0.0.1/",
            "http://127.0.0.1.../",
            "http://a..b.example/",
        ] {
            assert!(
                matches!(check_offline(url, false), Err(FsmError::UnsafeUrl(_))),
                "check_offline allowed {url}"
            );
            assert!(blocked(&auto_g, url, true).await, "proxied allowed {url}");
            assert!(blocked(&never_g, url, false).await, "never allowed {url}");
        }
        // Single trailing dot FQDN on a public domain still passes.
        assert!(check_offline("https://example.com./", false).is_ok());
    }

    /// B3 regression (Missing test #3): relative paths/queries containing `//`
    /// and `%` are valid, while backslash and malformed scheme authorities fail.
    #[test]
    fn raw_authority_allows_relative_double_slash_and_rejects_backslash_authorities() {
        for valid in [
            "/login?next=https://example.com&msg=hello%20world",
            "/r?u=https://site.org%2Fpage",
            "/a//b%20c",
            "https://example.com/path?next=https://other.org&x=%20",
        ] {
            assert!(
                check_raw_authority(valid).is_ok(),
                "false positive on {valid}"
            );
        }
        for bad in [
            "http:/\\%65xample.com/",
            "http:\\\\example.com\\@127.0.0.1/",
            "/\\%65xample.com/",
            "\\example.com/",
            "http:/%65xample.com/",
            "http:///%65xample.com/",
        ] {
            assert!(
                matches!(check_raw_authority(bad), Err(FsmError::UnsafeUrl(_))),
                "missed raw authority {bad}"
            );
        }
        for bad_url in [
            "http:/\\%65xample.com/",
            "http:\\\\example.com\\@127.0.0.1/",
            "http:/%65xample.com/",
            "http:///%65xample.com/",
        ] {
            assert!(
                matches!(check_offline(bad_url, false), Err(FsmError::UnsafeUrl(_))),
                "check_offline allowed {bad_url}"
            );
        }
    }

    #[tokio::test]
    async fn fake_ip_ipv4_canary_never_excuses_ipv6_ula_or_other_ranges() {
        // Typical IPv4-only Clash/Surge TUN: canaries resolve ONLY to 198.18.0.0/15.
        let dns = StaticDns::new()
            .with("example.com", &[ip("198.18.0.1")])
            .with("a.root-servers.net", &[ip("198.18.0.2")])
            .with("aws-imds-v6.example", &[ip("fd00:ec2::254")])
            .with("corp-ula.example", &[ip("fdfe:dcba:9876::1")])
            .with("v6-bench.example", &[ip("2001:2::1")])
            .with("public-tun.example", &[ip("198.18.0.99")]);
        let g = auto(dns);
        assert!(g.check("https://public-tun.example/", false).await.is_ok());
        assert_eq!(g.fake_ip_verdict().await, Some(true));
        // Unverified blocked candidate ranges MUST remain blocked!
        assert!(blocked(&g, "https://aws-imds-v6.example/", false).await);
        assert!(blocked(&g, "https://corp-ula.example/", false).await);
        assert!(blocked(&g, "https://v6-bench.example/", false).await);

        // Conversely, an IPv6-benchmarking-only canary must NOT excuse 198.18/15 or fc00::/7.
        let dns_v6 = StaticDns::new()
            .with("example.com", &[ip("2001:2::10")])
            .with("a.root-servers.net", &[ip("2001:2::11")])
            .with("v6-ok.example", &[ip("2001:2::99")])
            .with("v4-bench.example", &[ip("198.18.0.1")])
            .with("aws-imds-v6.example", &[ip("fd00:ec2::254")]);
        let g_v6 = auto(dns_v6);
        assert!(g_v6.check("https://v6-ok.example/", false).await.is_ok());
        assert!(blocked(&g_v6, "https://v4-bench.example/", false).await);
        assert!(blocked(&g_v6, "https://aws-imds-v6.example/", false).await);
    }
}
