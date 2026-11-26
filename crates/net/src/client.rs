//! [`HttpClient`]: the live [`NetClient`] (`SPEC.md` §7.1).
//!
//! * `plain` (Tier 0): reqwest 0.13 + rustls 0.23 (`ring`, webpki-roots),
//!   honest UA `free-search-mcp/1.0` (+ `mailto:` when `CONTACT_EMAIL` is
//!   set), no cookies at all.
//! * `chrome` / `edge_preflight` (Tier 1): wreq 0.16.5 with the wreq-util
//!   Chrome 149 / Edge 148 emulation (`emulation-compression`, so
//!   `Accept-Encoding: gzip, deflate, br, zstd` matches the UA), wreq cookies
//!   OFF, a manual cookie jar per `cookie_jar_id` and egress.
//! * Every hop: SSRF layers 1–3, then layer 4 with DNS pinning when the hop
//!   leaves directly; ≤ 5 manual redirects; unknown `Content-Encoding` and
//!   oversized identity `Content-Length` refused before the body is read;
//!   the (decompressed) body streamed against `max_bytes` and cut there
//!   (`HttpBytes.truncated`, as `ReplayNet` does).
//! * GCRA per `NetCx.host_bucket | source_id | host`; `Retry-After` on
//!   429/503 benches that key; cancellation ends as `Timeout(0)` (as `ReplayNet`).

use crate::dns::{DnsLookup, PinnedResolver, default_lookup};
use crate::flight::{MAX_FOLLOWER_WAIT, WarmGates};
use crate::inflate::{BoundedInflater, InflateKind, InflateStatus};
use crate::jar::CookieJar;
use crate::ratelimit::{RateLimits, parse_retry_after};
use crate::ssrf::{SsrfGuard, Target};
use fsm_core::{
    Bytes, CancellationToken, Config, FsmError, HttpBytes, HttpMethod, HttpRequest, NetClient,
    NetCx, TransportProfile,
};
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant, SystemTime};
use url::Url;

/// The honest Tier-0 User-Agent.
pub const PLAIN_USER_AGENT: &str = "free-search-mcp/1.0";
/// Manually followed redirect hops.
pub const MAX_REDIRECTS: usize = 5;
/// wreq-util profile behind `TransportProfile::Chrome`.
pub const CHROME_PROFILE: wreq_util::Profile = wreq_util::Profile::Chrome149;
/// wreq-util profile behind `TransportProfile::EdgePreflight`.
pub const EDGE_PROFILE: wreq_util::Profile = wreq_util::Profile::Edge148;
/// TCP + TLS connect timeout of both stacks.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Idle pooled connections are closed after this.
pub const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
/// Upper bound on request timeouts so `Instant::now() + budget` and tokio timers never overflow.
const MAX_REQUEST_TIMEOUT: Duration = Duration::from_secs(86_400);

const REDIRECT_STATUSES: [u16; 5] = [301, 302, 303, 307, 308];
/// Request headers dropped when a redirect leaves the origin.
const CROSS_ORIGIN_STRIP: [&str; 7] = [
    "authorization",
    "proxy-authorization",
    "cookie",
    "host",
    "x-api-key",
    "api-key",
    "x-auth-token",
];

static RING_PROVIDER: Once = Once::new();

/// `rustls::crypto::ring::default_provider().install_default()`, once per
/// process (an already installed provider is kept).
pub fn install_ring_provider() {
    RING_PROVIDER.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Composite `ServerCertVerifier`: verifies against bundled Mozilla `webpki-roots`
/// first (fast, deterministic, works even in minimal containers without OS roots),
/// and falls back to `rustls-platform-verifier` ONLY on `UnknownIssuer` so hosts
/// with OS-trusted CAs or AIA-chased missing intermediates (e.g. macOS Keychain /
/// Windows CryptoAPI / Linux system CA store) verify without weakening hostname,
/// expiry, or signature checks.
#[derive(Debug)]
struct HybridCertVerifier {
    webpki: Arc<dyn rustls::client::danger::ServerCertVerifier>,
    platform: Option<Arc<dyn rustls::client::danger::ServerCertVerifier>>,
}

impl HybridCertVerifier {
    fn new(provider: &Arc<rustls::crypto::CryptoProvider>) -> Result<Self, FsmError> {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let webpki = rustls::client::WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots),
            Arc::clone(provider),
        )
        .build()
        .map_err(|e| FsmError::internal("tls", e.to_string()))?;
        let platform: Option<Arc<dyn rustls::client::danger::ServerCertVerifier>> =
            match rustls_platform_verifier::Verifier::new(Arc::clone(provider)) {
                Ok(v) => Some(Arc::new(v)),
                Err(e) => {
                    tracing::debug!(
                        error = %e,
                        "platform TLS verifier unavailable; using bundled webpki-roots only"
                    );
                    None
                }
            };
        Ok(Self { webpki, platform })
    }
}

impl rustls::client::danger::ServerCertVerifier for HybridCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        intermediates: &[rustls::pki_types::CertificateDer<'_>],
        server_name: &rustls::pki_types::ServerName<'_>,
        ocsp_response: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        match self.webpki.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) {
            Ok(ok) => Ok(ok),
            Err(rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer)) => {
                if let Some(platform) = &self.platform {
                    platform.verify_server_cert(
                        end_entity,
                        intermediates,
                        server_name,
                        ocsp_response,
                        now,
                    )
                } else {
                    Err(rustls::Error::InvalidCertificate(
                        rustls::CertificateError::UnknownIssuer,
                    ))
                }
            }
            Err(err) => Err(err),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.webpki.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.webpki.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.webpki.supported_verify_schemes()
    }
}

fn rustls_config_with(
    provider: Arc<rustls::crypto::CryptoProvider>,
    verifier: Arc<HybridCertVerifier>,
    tls12_http1_only: bool,
) -> Result<rustls::ClientConfig, FsmError> {
    let builder = if tls12_http1_only {
        rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS12])
            .map_err(|e| FsmError::internal("tls", e.to_string()))?
    } else {
        rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| FsmError::internal("tls", e.to_string()))?
    };
    let mut cfg = builder
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    cfg.alpn_protocols = if tls12_http1_only {
        vec![b"http/1.1".to_vec()]
    } else {
        vec![b"h2".to_vec(), b"http/1.1".to_vec()]
    };
    Ok(cfg)
}

fn is_tls_handshake_eof(err: &reqwest::Error) -> bool {
    if !err.is_connect() {
        return false;
    }
    let (chain, _) = error_chain(err);
    let lower = chain.to_ascii_lowercase();
    lower.contains("tls handshake eof")
        || lower.contains("unexpected eof")
        || lower.contains("unexpectedeof")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stack {
    Plain,
    Chrome,
    Edge,
}

impl Stack {
    fn name(self) -> &'static str {
        match self {
            Stack::Plain => "plain",
            Stack::Chrome => "chrome",
            Stack::Edge => "edge",
        }
    }
}

/// What the client takes from [`Config`].
#[derive(Clone)]
struct Settings {
    proxy: Option<String>,
    /// True when the proxy resolves target hostnames remotely (`http`, `https`,
    /// `socks4a`, `socks5h`); false for local-DNS proxies (`socks4`, `socks5`).
    proxy_resolves_target: bool,
    /// Lowercased, bracket/dot-stripped hostname of the operator-configured proxy.
    proxy_host: Option<String>,
    proxy_engines: Vec<String>,
    no_proxy: Vec<String>,
    default_timeout: Duration,
    max_response_bytes: u64,
    plain_ua: String,
    chrome_ua: Option<String>,
    accept_language: Option<String>,
    prewarm: bool,
}

impl Settings {
    fn from_config(cfg: &Config) -> Self {
        let plain_ua = match cfg.contact_email.as_deref().map(str::trim) {
            Some(mail) if !mail.is_empty() => format!("{PLAIN_USER_AGENT} (mailto:{mail})"),
            _ => PLAIN_USER_AGENT.to_owned(),
        };
        let default_timeout = if cfg.request_timeout_s.is_finite() && cfg.request_timeout_s > 0.0 {
            Duration::try_from_secs_f64(cfg.request_timeout_s)
                .unwrap_or(Duration::from_secs(10))
                .min(MAX_REQUEST_TIMEOUT)
        } else {
            Duration::from_secs(10)
        };
        let proxy = cfg.proxy.clone().filter(|p| !p.trim().is_empty());
        let (proxy_resolves_target, proxy_host) = proxy
            .as_deref()
            .and_then(|p| Url::parse(p.trim()).ok())
            .map_or((true, None), |u| {
                let resolves_remotely = !matches!(
                    u.scheme().to_ascii_lowercase().as_str(),
                    "socks4" | "socks5"
                );
                let host = u.host_str().map(|h| {
                    h.trim_matches(|c| c == '[' || c == ']')
                        .trim_end_matches('.')
                        .to_ascii_lowercase()
                });
                (resolves_remotely, host)
            });
        Self {
            proxy,
            proxy_resolves_target,
            proxy_host,
            proxy_engines: cfg
                .proxy_engines
                .iter()
                .map(|e| e.trim().to_ascii_lowercase())
                .filter(|e| !e.is_empty())
                .collect(),
            no_proxy: cfg
                .no_proxy
                .iter()
                .map(|e| e.trim().to_ascii_lowercase())
                .filter(|e| !e.is_empty())
                .collect(),
            default_timeout,
            max_response_bytes: cfg.max_response_bytes,
            plain_ua,
            chrome_ua: Some(cfg.user_agent.trim().to_owned()).filter(|u| !u.is_empty()),
            accept_language: Some(cfg.accept_language.trim().to_owned()).filter(|a| !a.is_empty()),
            prewarm: cfg.prewarm,
        }
    }
}

/// Whether `host` matches a `NO_PROXY` entry (`*`, `host`, `.suffix`, `*.suffix`).
fn no_proxy_matches(list: &[String], host: &str) -> bool {
    let host = host
        .trim()
        .trim_matches(|c| c == '[' || c == ']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    list.iter().any(|e| {
        let e = e.trim();
        if e == "*" {
            return true;
        }
        let e = e
            .trim_start_matches("*.")
            .trim_start_matches('.')
            .trim_end_matches('.')
            .trim_matches(|c| c == '[' || c == ']')
            .to_ascii_lowercase();
        !e.is_empty() && (host == e || host.ends_with(&format!(".{e}")))
    })
}

/// Direct and (if a proxy is configured) proxied client of one stack.
struct Egress<C> {
    direct: C,
    proxied: Option<C>,
}

impl<C> Egress<C> {
    fn pick(&self, proxied: bool) -> &C {
        match (&self.proxied, proxied) {
            (Some(p), true) => p,
            _ => &self.direct,
        }
    }
}

struct Inner {
    s: Settings,
    guard: Arc<SsrfGuard>,
    plain: Egress<reqwest::Client>,
    plain_tls12: Egress<reqwest::Client>,
    tls12_origins: moka::sync::Cache<String, ()>,
    chrome: Egress<wreq::Client>,
    edge: Egress<wreq::Client>,
    jars: moka::sync::Cache<String, Arc<Mutex<CookieJar>>>,
    limits: RateLimits,
    warm: WarmGates,
}

/// The live network client: implements [`NetClient`]. Cheap to clone.
#[derive(Clone)]
pub struct HttpClient {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for HttpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpClient")
            .field("guard", &self.inner.guard)
            .field("proxy", &self.inner.s.proxy.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

/// Builder for [`HttpClient`] (inject a resolver for tests / tooling).
pub struct HttpClientBuilder {
    cfg: Config,
    dns: Option<Arc<dyn DnsLookup>>,
}

impl HttpClientBuilder {
    /// Resolve names through `dns` instead of the system configuration.
    pub fn dns(mut self, dns: Arc<dyn DnsLookup>) -> Self {
        self.dns = Some(dns);
        self
    }

    pub fn build(self) -> Result<HttpClient, FsmError> {
        install_ring_provider();
        let s = Settings::from_config(&self.cfg);
        let dns = self.dns.unwrap_or_else(default_lookup);
        let guard = Arc::new(SsrfGuard::from_config(&self.cfg, dns));
        // Direct clients dial only what layer 4 vetted (when it applies to a
        // direct request at all). Remote-DNS proxied clients (`http`/`https`/
        // `socks5h`/`socks4a`) only resolve the proxy host; local-DNS proxies
        // (`socks5`/`socks4`) resolve targets locally through the same pinned
        // guard while allowing the operator-configured proxy's own hostname.
        let direct = PinnedResolver::new(guard.clone(), guard.requires_resolution(false), None);
        let via_proxy = PinnedResolver::new(
            guard.clone(),
            !s.proxy_resolves_target && guard.requires_resolution(false),
            s.proxy_host.clone(),
        );
        let proxy = s.proxy.as_deref();
        let tls_provider = Arc::new(rustls::crypto::ring::default_provider());
        let tls_verifier = Arc::new(HybridCertVerifier::new(&tls_provider)?);
        let tls_default =
            rustls_config_with(Arc::clone(&tls_provider), Arc::clone(&tls_verifier), false)?;
        let tls_12_http1 = rustls_config_with(tls_provider, tls_verifier, true)?;
        let plain = Egress {
            direct: plain_client(&s, direct.clone(), None, tls_default.clone())?,
            proxied: proxy
                .map(|p| plain_client(&s, via_proxy.clone(), Some(p), tls_default.clone()))
                .transpose()?,
        };
        let plain_tls12 = Egress {
            direct: plain_client(&s, direct.clone(), None, tls_12_http1.clone())?,
            proxied: proxy
                .map(|p| plain_client(&s, via_proxy.clone(), Some(p), tls_12_http1.clone()))
                .transpose()?,
        };
        let chrome = Egress {
            direct: wreq_client(CHROME_PROFILE, direct.clone(), None)?,
            proxied: proxy
                .map(|p| wreq_client(CHROME_PROFILE, via_proxy.clone(), Some(p)))
                .transpose()?,
        };
        let edge = Egress {
            direct: wreq_client(EDGE_PROFILE, direct, None)?,
            proxied: proxy
                .map(|p| wreq_client(EDGE_PROFILE, via_proxy, Some(p)))
                .transpose()?,
        };
        Ok(HttpClient {
            inner: Arc::new(Inner {
                s,
                guard,
                plain,
                plain_tls12,
                tls12_origins: moka::sync::Cache::builder()
                    .max_capacity(128)
                    .time_to_idle(Duration::from_secs(3600))
                    .build(),
                chrome,
                edge,
                jars: moka::sync::Cache::builder()
                    .max_capacity(256)
                    .time_to_idle(Duration::from_secs(3600))
                    .build(),
                limits: RateLimits::new(),
                warm: WarmGates::default(),
            }),
        })
    }
}

fn proxy_error(e: impl std::fmt::Display) -> FsmError {
    // Never echo the proxy URL: it may carry credentials.
    FsmError::invalid(format!(
        "SEARCH_MCP_PROXY is not a usable proxy URL ({})",
        scrub_credentials(&e.to_string())
    ))
}

fn plain_client(
    s: &Settings,
    resolver: PinnedResolver,
    proxy: Option<&str>,
    tls: rustls::ClientConfig,
) -> Result<reqwest::Client, FsmError> {
    let mut b = reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .user_agent(s.plain_ua.as_str())
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        .dns_resolver(Arc::new(resolver))
        .no_gzip()
        .no_deflate()
        .no_brotli()
        .no_zstd()
        .no_proxy();
    if let Some(p) = proxy {
        b = b.proxy(reqwest::Proxy::all(p).map_err(proxy_error)?);
    }
    b.build()
        .map_err(|e| FsmError::internal("http_client", e.to_string()))
}

fn wreq_client(
    profile: wreq_util::Profile,
    resolver: PinnedResolver,
    proxy: Option<&str>,
) -> Result<wreq::Client, FsmError> {
    let emulation = wreq_util::Emulation::builder().profile(profile).build();
    let mut b = wreq::Client::builder()
        .emulation(emulation)
        .redirect(wreq::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        .dns_resolver(Arc::new(resolver))
        .no_gzip()
        .no_deflate()
        .no_brotli()
        .no_zstd()
        .no_proxy();
    if let Some(p) = proxy {
        b = b.proxy(wreq::Proxy::all(p).map_err(proxy_error)?);
    }
    b.build()
        .map_err(|e| FsmError::internal("http_client", e.to_string()))
}

/// Replace `scheme://user:pass@` with `scheme://<credentials>@`.
fn scrub_credentials(s: &str) -> String {
    static CREDS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)([a-z][a-z0-9+.\-]*://)[^/@\s]+@").expect("static regex")
    });
    CREDS.replace_all(s, "${1}<credentials>@").into_owned()
}

/// `url` without the query keys the request marked as credentials.
fn strip_redacted_query(url: &Url, req: &HttpRequest) -> Url {
    if req.redact_query_keys.is_empty() || url.query().is_none() {
        return url.clone();
    }
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| !req.is_redacted_query_key(k))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let mut out = url.clone();
    if kept.is_empty() {
        out.set_query(None);
    } else {
        out.query_pairs_mut().clear().extend_pairs(kept);
    }
    out
}

fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// Either stack's transport error, before mapping.
enum WireError {
    Reqwest(reqwest::Error),
    Wreq(wreq::Error),
    /// `url` (what the guard checked) and `http::Uri` (what the stacks dial)
    /// read the authority differently.
    Parser(String),
}

/// Fail closed unless `http::Uri` reads the same host and port as the
/// WHATWG URL the guard validated (both stacks connect via `http::Uri`).
fn same_authority(url: &Url) -> Result<(), String> {
    let uri: http::Uri = url
        .as_str()
        .parse()
        .map_err(|e| format!("the URL does not parse as an HTTP URI: {e}"))?;
    let (Some(a), Some(b)) = (uri.host(), url.host_str()) else {
        return Err("the URL has no host".into());
    };
    let port = uri.port_u16().or_else(|| match uri.scheme_str() {
        Some("https") => Some(443),
        Some("http") => Some(80),
        _ => None,
    });
    if !a.eq_ignore_ascii_case(b) || port != url.port_or_known_default() {
        return Err(format!(
            "URL parsers disagree on the destination ({b} vs {a})"
        ));
    }
    Ok(())
}

/// Pieces of an error message that must never reach the caller.
struct Secrets<'a> {
    req: &'a HttpRequest,
    url: &'a Url,
    proxy: Option<&'a str>,
}

impl Secrets<'_> {
    fn scrub(&self, msg: &str) -> String {
        let mut out = msg.to_owned();
        let full = self.url.as_str();
        if let Some(host) = self.url.host_str() {
            out = out.replace(full, host);
        }
        if let Some(p) = self.proxy {
            out = out.replace(p, "<proxy>");
        }
        for (k, v) in self.url.query_pairs() {
            if self.req.is_redacted_query_key(&k) && !v.is_empty() {
                out = out.replace(v.as_ref(), "<redacted>");
                let enc: String = url::form_urlencoded::byte_serialize(v.as_bytes()).collect();
                out = out.replace(&enc, "<redacted>");
            }
        }
        for (k, v) in &self.req.headers {
            if self.req.is_redacted_header(k) && !v.is_empty() {
                out = out.replace(v.as_str(), "<redacted>");
            }
        }
        scrub_credentials(&out)
    }
}

fn error_chain(top: &(dyn std::error::Error + 'static)) -> (String, Option<String>) {
    let mut parts = vec![top.to_string()];
    let mut unsafe_url = None;
    let mut cur = top.source();
    let mut depth = 0;
    while let Some(e) = cur {
        depth += 1;
        if depth > 12 {
            break;
        }
        let inner: Option<&(dyn std::error::Error + 'static)> = e
            .downcast_ref::<std::io::Error>()
            .and_then(|io| io.get_ref())
            .map(|i| i as &(dyn std::error::Error + 'static));
        for cand in [Some(e), inner].into_iter().flatten() {
            if let Some(FsmError::UnsafeUrl(m)) = cand.downcast_ref::<FsmError>() {
                unsafe_url = Some(m.clone());
            }
        }
        let s = e.to_string();
        if !parts.iter().any(|p| p.contains(&s)) {
            parts.push(s);
        }
        cur = e.source();
    }
    (parts.join(": "), unsafe_url)
}

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

impl HttpClient {
    /// The client for `cfg`, resolving through hickory (system configuration).
    pub fn new(cfg: &Config) -> Result<Self, FsmError> {
        Self::builder(cfg).build()
    }

    pub fn builder(cfg: &Config) -> HttpClientBuilder {
        HttpClientBuilder {
            cfg: cfg.clone(),
            dns: None,
        }
    }

    /// The SSRF guard (layers 1–4, canary verdict).
    pub fn guard(&self) -> &Arc<SsrfGuard> {
        &self.inner.guard
    }

    /// Keyed GCRA buckets: `configure(host_bucket, rps, burst)` from manifests.
    pub fn rate_limits(&self) -> &RateLimits {
        &self.inner.limits
    }

    /// Whether this machine's resolver is a fake-IP tunnel (`doctor
    /// --require-fake-ip`); `None` when the canaries do not resolve.
    pub async fn fake_ip_verdict(&self) -> Option<bool> {
        self.inner.guard.fake_ip_verdict().await
    }

    /// Forget a named cookie jar (all egresses), e.g. to re-mint a session.
    pub fn clear_cookie_jar(&self, jar_id: &str) {
        for stack in [Stack::Chrome, Stack::Edge] {
            for egress in ["direct", "proxy"] {
                self.inner
                    .jars
                    .invalidate(&format!("{}|{egress}|{jar_id}", stack.name()));
            }
        }
    }

    /// Open (warm) connections to `targets` with one `HEAD` each, in
    /// parallel (`SEARCH_MCP_PREWARM`). Returns how many answered (`0` when
    /// `Config.prewarm` is `false`).
    pub async fn prewarm(&self, targets: &[(&str, TransportProfile)]) -> usize {
        if !self.inner.s.prewarm {
            return 0;
        }
        let jobs = targets.iter().map(|(url, profile)| {
            let mut req = HttpRequest::head(*url);
            req.follow_redirects = false;
            let cx = NetCx::new(CancellationToken::new(), Duration::from_secs(4));
            let profile = *profile;
            async move { self.send(req, profile, &cx).await.is_ok() }
        });
        futures::future::join_all(jobs)
            .await
            .into_iter()
            .filter(|ok| *ok)
            .count()
    }

    fn proxied(&self, req: &HttpRequest, host: &str) -> bool {
        let s = &self.inner.s;
        if s.proxy.is_none() {
            return false;
        }
        if !s.proxy_engines.is_empty()
            && let Some(src) = &req.source_id
            && !s.proxy_engines.iter().any(|e| e.eq_ignore_ascii_case(src))
        {
            return false;
        }
        !no_proxy_matches(&s.no_proxy, host)
    }

    fn jar(&self, stack: Stack, proxied: bool, id: Option<&str>) -> Option<Arc<Mutex<CookieJar>>> {
        match (stack, id) {
            (Stack::Plain, _) => None, // Tier 0 is cookie-less, always.
            (_, None) => Some(Arc::new(Mutex::new(CookieJar::new()))), // one request's redirect chain
            (_, Some(id)) => {
                let egress = if proxied { "proxy" } else { "direct" };
                let key = format!("{}|{egress}|{id}", stack.name());
                Some(
                    self.inner
                        .jars
                        .get_with(key, || Arc::new(Mutex::new(CookieJar::new()))),
                )
            }
        }
    }

    async fn execute(
        &self,
        req: HttpRequest,
        profile: TransportProfile,
        cx: &NetCx,
        deadline: Instant,
    ) -> Result<HttpBytes, FsmError> {
        let stack = match profile {
            TransportProfile::Plain => Stack::Plain,
            TransportProfile::Chrome => Stack::Chrome,
            TransportProfile::EdgePreflight => Stack::Edge,
            _ => {
                return Err(FsmError::invalid(
                    "transport \"local\" does not send HTTP requests",
                ));
            }
        };
        let inner = &self.inner;
        // Layers 1–3 on the caller's raw URL (escapes intact), then the query.
        let first = inner.guard.check_offline(&req.url)?;
        let mut url = first.url.clone();
        if !req.query.is_empty() {
            let mut q = url.query_pairs_mut();
            for (k, v) in &req.query {
                q.append_pair(k, v);
            }
        }
        let label = req.source_id.clone().unwrap_or_else(|| first.host.clone());
        let key = cx
            .host_bucket
            .clone()
            .or_else(|| req.source_id.clone())
            .unwrap_or_else(|| first.host.clone());
        if let Some(left) = inner.limits.penalty(&key) {
            return Err(FsmError::RateLimited {
                source_id: label,
                retry_after_s: Some(left.as_secs().max(1)),
            });
        }
        let budget = deadline.saturating_duration_since(Instant::now());
        if let Err(wait) = inner.limits.acquire(&key, cx.rps, Some(budget)).await {
            return Err(FsmError::RateLimited {
                source_id: label,
                retry_after_s: Some(wait.as_secs().max(1)),
            });
        }
        let cap = req.max_bytes.unwrap_or(inner.s.max_response_bytes);
        // Ephemeral jars for unnamed requests are isolated per egress so a
        // redirect crossing NO_PROXY never leaks cookies across egresses.
        let mut ephemeral_direct: Option<Arc<Mutex<CookieJar>>> = None;
        let mut ephemeral_proxied: Option<Arc<Mutex<CookieJar>>> = None;
        let mut method = req.method;
        let mut body = req.body.clone();
        let mut headers = req.headers.clone();
        let mut raw_next: Option<String> = None;
        for hop in 0..=MAX_REDIRECTS {
            let target = if hop == 0 {
                inner.guard.check_offline(url.as_str())?
            } else {
                if let Some(raw) = raw_next.take()
                    && !inner.guard.allow_private()
                {
                    // The server's own spelling: refuse escaped hosts in a Location too.
                    crate::ssrf::check_raw_authority(&raw)?;
                }
                inner.guard.check_offline(url.as_str())?
            };
            let proxied = self.proxied(&req, &target.host);
            let remote_dns = proxied && inner.s.proxy_resolves_target;
            if target.literal.is_none() && inner.guard.requires_resolution(remote_dns) {
                inner.guard.vet(&target.host, target.port).await?;
            }
            let jar = if stack == Stack::Plain {
                None
            } else if let Some(id) = req.cookie_jar_id.as_deref() {
                self.jar(stack, proxied, Some(id))
            } else {
                let slot = if proxied {
                    &mut ephemeral_proxied
                } else {
                    &mut ephemeral_direct
                };
                Some(
                    slot.get_or_insert_with(|| Arc::new(Mutex::new(CookieJar::new())))
                        .clone(),
                )
            };
            let now = SystemTime::now();
            let cookie = jar
                .as_ref()
                .and_then(|j| j.lock().ok().and_then(|j| j.header_for(&target.url, now)));
            let secrets = Secrets {
                req: &req,
                url: &target.url,
                proxy: inner.s.proxy.as_deref(),
            };
            let mut resp = self
                .dispatch(
                    stack,
                    proxied,
                    method,
                    &target,
                    &headers,
                    cookie,
                    body.clone(),
                    deadline,
                )
                .await
                .map_err(|e| wire_error(e, &label, &secrets))?;
            if let Some(j) = &jar
                && let Ok(mut j) = j.lock()
            {
                for v in resp.headers().get_all("set-cookie") {
                    if let Ok(v) = v.to_str() {
                        j.store(&target.url, v, now);
                    }
                }
            }
            let status = resp.status().as_u16();
            if req.follow_redirects && REDIRECT_STATUSES.contains(&status) {
                let Some(loc) = resp
                    .headers()
                    .get("location")
                    .and_then(|v| v.to_str().ok())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                else {
                    return Err(FsmError::upstream(
                        label,
                        Some(status),
                        "redirect without a Location header",
                    ));
                };
                if hop == MAX_REDIRECTS {
                    return Err(FsmError::upstream(
                        label,
                        Some(status),
                        format!("too many redirects (>{MAX_REDIRECTS})"),
                    ));
                }
                let next = target.url.join(&loc).map_err(|e| {
                    FsmError::upstream(
                        label.clone(),
                        Some(status),
                        format!("bad redirect Location: {e}"),
                    )
                })?;
                if status == 303 && method != HttpMethod::Head
                    || matches!(status, 301 | 302) && method == HttpMethod::Post
                {
                    method = HttpMethod::Get;
                    body = None;
                    headers.retain(|(k, _)| {
                        !k.eq_ignore_ascii_case("content-type")
                            && !k.eq_ignore_ascii_case("content-length")
                            && !k.eq_ignore_ascii_case("content-encoding")
                            && !k.eq_ignore_ascii_case("transfer-encoding")
                    });
                }
                if !same_origin(&target.url, &next) {
                    headers.retain(|(k, _)| {
                        !CROSS_ORIGIN_STRIP.iter().any(|s| k.eq_ignore_ascii_case(s))
                            && !req.is_redacted_header(k)
                    });
                }
                drop(resp);
                raw_next = Some(loc);
                url = next;
                continue;
            }
            if matches!(status, 429 | 503)
                && let Some(after) = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(parse_retry_after)
                && !after.is_zero()
            {
                inner.limits.penalize(&key, after);
            }
            return self
                .finish(&mut resp, &req, method, cap, &target, &label, &secrets)
                .await;
        }
        Err(FsmError::upstream(
            label,
            None,
            format!("too many redirects (>{MAX_REDIRECTS})"),
        ))
    }

    #[allow(clippy::too_many_arguments)]
    async fn dispatch(
        &self,
        stack: Stack,
        proxied: bool,
        method: HttpMethod,
        target: &Target,
        headers: &[(String, String)],
        cookie: Option<String>,
        body: Option<Bytes>,
        deadline: Instant,
    ) -> Result<reqwest::Response, WireError> {
        let inner = &self.inner;
        let mut headers: Vec<(String, String)> = headers.to_vec();
        if let Some(c) = cookie {
            match headers
                .iter_mut()
                .find(|(k, _)| k.eq_ignore_ascii_case("cookie"))
            {
                Some((_, v)) => {
                    v.push_str("; ");
                    v.push_str(&c);
                }
                None => headers.push(("cookie".into(), c)),
            }
        }
        let has = |name: &str| headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name));
        let mut extra: Vec<(String, String)> = Vec::new();
        if stack == Stack::Plain {
            if !has("accept-encoding") && !has("range") {
                extra.push(("accept-encoding".into(), "gzip, deflate, br, zstd".into()));
            }
        } else {
            if let Some(ua) = &inner.s.chrome_ua
                && !has("user-agent")
            {
                extra.push(("user-agent".into(), ua.clone()));
            }
            if let Some(al) = &inner.s.accept_language
                && !has("accept-language")
            {
                extra.push(("accept-language".into(), al.clone()));
            }
        }
        if let Err(m) = same_authority(&target.url) {
            return Err(WireError::Parser(m));
        }
        let gate = if target.url.scheme() == "https" {
            let origin = format!("{}|{}|{}", stack.name(), proxied, target.origin());
            let wait = MAX_FOLLOWER_WAIT.min(deadline.saturating_duration_since(Instant::now()));
            Some(inner.warm.enter(&origin, wait).await)
        } else {
            None
        };
        let resp = match stack {
            Stack::Plain => {
                let m = match method {
                    HttpMethod::Post => reqwest::Method::POST,
                    HttpMethod::Head => reqwest::Method::HEAD,
                    _ => reqwest::Method::GET,
                };
                let send_with = |client: &reqwest::Client| {
                    let mut rb = client.request(m.clone(), target.url.as_str());
                    for (k, v) in headers.iter().chain(&extra) {
                        rb = rb.header(k.as_str(), v.as_str());
                    }
                    if let Some(b) = body.clone() {
                        rb = rb.body(b);
                    }
                    rb.send()
                };
                let is_https = target.url.scheme() == "https";
                let origin_key = target.origin();
                if is_https && inner.tls12_origins.contains_key(&origin_key) {
                    send_with(inner.plain_tls12.pick(proxied))
                        .await
                        .map_err(WireError::Reqwest)?
                } else {
                    match send_with(inner.plain.pick(proxied)).await {
                        Ok(r) => r,
                        Err(err)
                            if is_https
                                && matches!(method, HttpMethod::Get | HttpMethod::Head)
                                && is_tls_handshake_eof(&err)
                                && Instant::now() < deadline =>
                        {
                            tracing::debug!(
                                origin = %origin_key,
                                "TLS handshake EOF on default client; retrying with TLS 1.2 + http/1.1"
                            );
                            inner.tls12_origins.insert(origin_key, ());
                            send_with(inner.plain_tls12.pick(proxied))
                                .await
                                .map_err(WireError::Reqwest)?
                        }
                        Err(err) => return Err(WireError::Reqwest(err)),
                    }
                }
            }
            Stack::Chrome | Stack::Edge => {
                let client = if stack == Stack::Chrome {
                    inner.chrome.pick(proxied)
                } else {
                    inner.edge.pick(proxied)
                };
                let m = match method {
                    HttpMethod::Post => wreq::Method::POST,
                    HttpMethod::Head => wreq::Method::HEAD,
                    _ => wreq::Method::GET,
                };
                let mut rb = client.request(m, target.url.as_str());
                for (k, v) in extra.iter().chain(&headers) {
                    rb = rb.header(k.as_str(), v.as_str());
                }
                if let Some(b) = body {
                    rb = rb.body(b);
                }
                bridge(rb.send().await.map_err(WireError::Wreq)?)
            }
        };
        if let Some(pass) = gate {
            pass.established();
        }
        Ok(resp)
    }

    #[allow(clippy::too_many_arguments)]
    async fn finish(
        &self,
        resp: &mut reqwest::Response,
        req: &HttpRequest,
        method: HttpMethod,
        cap: u64,
        target: &Target,
        label: &str,
        secrets: &Secrets<'_>,
    ) -> Result<HttpBytes, FsmError> {
        let status = resp.status().as_u16();
        let mut headers: Vec<(String, String)> = resp
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_owned(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect();
        let final_url = strip_redacted_query(&target.url, req).to_string();
        // HEAD, 204 and 304 carry no body: preserve raw Content-Length /
        // Content-Encoding headers intact and skip decoding.
        if method == HttpMethod::Head || status == 204 || status == 304 {
            return Ok(HttpBytes::new(final_url, status, headers, Bytes::new()));
        }
        // Collect all Content-Encoding header lines and comma-separated tokens.
        // All four supported encodings (`gzip`/`x-gzip`, `deflate`, `br`, `zstd`)
        // are decoded manually below (auto-decompression disabled on both stacks)
        // so `Content-Length` pre-read, wire-byte cap, output cap, uppercase tokens,
        // stacked rejection, and `HEAD` header preservation behave uniformly.
        let mut encodings: Vec<String> = Vec::new();
        for v in resp.headers().get_all("content-encoding") {
            let raw = String::from_utf8_lossy(v.as_bytes());
            for part in raw.split(',') {
                let tok = part.trim().to_ascii_lowercase();
                if !tok.is_empty() {
                    encodings.push(tok);
                }
            }
        }
        let decode_kind = match encodings.as_slice() {
            [] => None,
            [one] if one == "identity" => None,
            [one] if one == "gzip" || one == "x-gzip" => Some(DecodeKind::Gzip),
            [one] if one == "deflate" => Some(DecodeKind::Deflate),
            [one] if one == "br" => Some(DecodeKind::Brotli),
            [one] if one == "zstd" => Some(DecodeKind::Zstd),
            _ => {
                return Err(FsmError::upstream(
                    label,
                    Some(status),
                    format!("unsupported Content-Encoding: {}", encodings.join(", ")),
                ));
            }
        };
        // Pre-read Content-Length rejection for both identity and compressed bodies.
        if let Some(len) = resp.content_length().or_else(|| {
            resp.headers()
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse().ok())
        }) && len > cap
        {
            return Err(FsmError::MaxBytesExceeded(cap));
        }
        let (body, truncated) = match decode_kind {
            None => read_capped(resp, cap)
                .await
                .map_err(|e| wire_error(WireError::Reqwest(e), label, secrets))?,
            Some(DecodeKind::Gzip) => {
                let out =
                    read_inflated_capped(resp, InflateKind::Gzip, cap, label, secrets).await?;
                headers.retain(|(k, _)| {
                    !k.eq_ignore_ascii_case("content-encoding")
                        && !k.eq_ignore_ascii_case("content-length")
                });
                out
            }
            Some(DecodeKind::Deflate) => {
                let out =
                    read_inflated_capped(resp, InflateKind::Deflate, cap, label, secrets).await?;
                headers.retain(|(k, _)| {
                    !k.eq_ignore_ascii_case("content-encoding")
                        && !k.eq_ignore_ascii_case("content-length")
                });
                out
            }
            Some(DecodeKind::Brotli) => {
                let out = read_async_decoded_capped(resp, false, cap, label, secrets).await?;
                headers.retain(|(k, _)| {
                    !k.eq_ignore_ascii_case("content-encoding")
                        && !k.eq_ignore_ascii_case("content-length")
                });
                out
            }
            Some(DecodeKind::Zstd) => {
                let out = read_async_decoded_capped(resp, true, cap, label, secrets).await?;
                headers.retain(|(k, _)| {
                    !k.eq_ignore_ascii_case("content-encoding")
                        && !k.eq_ignore_ascii_case("content-length")
                });
                out
            }
        };
        let mut out = HttpBytes::new(final_url, status, headers, body);
        out.truncated = truncated;
        Ok(out)
    }
}

fn wire_error(e: WireError, label: &str, secrets: &Secrets<'_>) -> FsmError {
    let (timeout, builder, decode, chain) = match &e {
        WireError::Reqwest(r) => {
            let (chain, unsafe_url) = error_chain(r);
            if let Some(m) = unsafe_url {
                return FsmError::UnsafeUrl(m);
            }
            (
                r.is_timeout(),
                r.is_builder(),
                r.is_decode() || r.is_body(),
                chain,
            )
        }
        WireError::Wreq(w) => {
            let (chain, unsafe_url) = error_chain(w);
            if let Some(m) = unsafe_url {
                return FsmError::UnsafeUrl(m);
            }
            (
                w.is_timeout(),
                w.is_builder(),
                w.is_decode() || w.is_body(),
                chain,
            )
        }
        WireError::Parser(m) => return FsmError::UnsafeUrl(m.clone()),
    };
    let msg = secrets.scrub(&chain);
    if timeout {
        return FsmError::Timeout(ms(CONNECT_TIMEOUT));
    }
    if builder {
        return FsmError::invalid(format!("invalid request: {msg}"));
    }
    let what = if decode {
        "reading the response body failed"
    } else {
        "request failed"
    };
    FsmError::upstream(label, None, format!("{what}: {msg}"))
}

/// A wreq response as a reqwest one, so both stacks share one capped,
/// incremental body reader (wreq's `stream` feature is off in the lock and
/// `http-body` is not a direct dependency; both crates speak `http-body` 1).
fn bridge(r: wreq::Response) -> reqwest::Response {
    let res: http::Response<wreq::Body> = r.into();
    let (parts, body) = res.into_parts();
    reqwest::Response::from(http::Response::from_parts(parts, reqwest::Body::wrap(body)))
}

/// Read at most `cap` bytes; `true` when the body was longer (the rest is
/// never downloaded: dropping the response aborts the transfer).
async fn read_capped(
    resp: &mut reqwest::Response,
    cap: u64,
) -> Result<(Bytes, bool), reqwest::Error> {
    let cap = usize::try_from(cap).unwrap_or(usize::MAX);
    let mut buf = bytes::BytesMut::new();
    while let Some(chunk) = resp.chunk().await? {
        let room = cap - buf.len();
        if chunk.len() > room {
            buf.extend_from_slice(&chunk[..room]);
            return Ok((buf.freeze(), true));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok((buf.freeze(), false))
}

/// Stream-decompress `gzip`/`x-gzip`/`deflate` with independent caps on wire
/// bytes and decompressed output bytes.
async fn read_inflated_capped(
    resp: &mut reqwest::Response,
    kind: InflateKind,
    cap: u64,
    label: &str,
    secrets: &Secrets<'_>,
) -> Result<(Bytes, bool), FsmError> {
    let mut inf = BoundedInflater::new(kind, cap);
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| wire_error(WireError::Reqwest(e), label, secrets))?
    {
        match inf.feed(&chunk) {
            Ok(InflateStatus::Continue) => {}
            Ok(InflateStatus::Truncated) => return Ok((inf.into_bytes(), true)),
            Ok(InflateStatus::WireCapExceeded) => return Err(FsmError::MaxBytesExceeded(cap)),
            Err(m) => {
                return Err(FsmError::upstream(
                    label,
                    None,
                    format!("reading the response body failed: {m}"),
                ));
            }
        }
    }
    match inf.finish() {
        Ok(InflateStatus::Continue) => Ok((inf.into_bytes(), false)),
        Ok(InflateStatus::Truncated) => Ok((inf.into_bytes(), true)),
        Ok(InflateStatus::WireCapExceeded) => Err(FsmError::MaxBytesExceeded(cap)),
        Err(m) => Err(FsmError::upstream(
            label,
            None,
            format!("reading the response body failed: {m}"),
        )),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecodeKind {
    Gzip,
    Deflate,
    Brotli,
    Zstd,
}

struct CappedSink {
    buf: bytes::BytesMut,
    cap: usize,
    truncated: bool,
}

impl CappedSink {
    fn new(cap: u64) -> Self {
        Self {
            buf: bytes::BytesMut::new(),
            cap: usize::try_from(cap).unwrap_or(usize::MAX),
            truncated: false,
        }
    }
}

impl tokio::io::AsyncWrite for CappedSink {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        src: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if self.truncated {
            return std::task::Poll::Ready(Err(std::io::Error::other("decompressed cap exceeded")));
        }
        let room = self.cap - self.buf.len();
        if src.len() > room {
            self.buf.extend_from_slice(&src[..room]);
            self.truncated = true;
            return std::task::Poll::Ready(Err(std::io::Error::other("decompressed cap exceeded")));
        }
        self.buf.extend_from_slice(src);
        std::task::Poll::Ready(Ok(src.len()))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

async fn drive_async_decoder<W>(
    resp: &mut reqwest::Response,
    mut decoder: W,
    get_sink: impl Fn(&W) -> &CappedSink,
    into_sink: impl FnOnce(W) -> CappedSink,
    cap: u64,
    label: &str,
    secrets: &Secrets<'_>,
) -> Result<(Bytes, bool), FsmError>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt as _;

    let mut wire_seen = 0u64;
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| wire_error(WireError::Reqwest(e), label, secrets))?
    {
        let room_wire = cap.saturating_sub(wire_seen);
        let chunk_len = chunk.len() as u64;
        let to_feed = if chunk_len > room_wire {
            usize::try_from(room_wire).unwrap_or(chunk.len())
        } else {
            chunk.len()
        };
        if to_feed > 0 {
            if let Err(e) = decoder.write_all(&chunk[..to_feed]).await {
                if get_sink(&decoder).truncated {
                    return Ok((into_sink(decoder).buf.freeze(), true));
                }
                return Err(FsmError::upstream(
                    label,
                    None,
                    format!("reading the response body failed: {e}"),
                ));
            }
            wire_seen += to_feed as u64;
        }
        if chunk_len > room_wire {
            let _ = decoder.flush().await;
            if get_sink(&decoder).truncated {
                return Ok((into_sink(decoder).buf.freeze(), true));
            }
            return Err(FsmError::MaxBytesExceeded(cap));
        }
    }
    if let Err(e) = decoder.shutdown().await {
        if get_sink(&decoder).truncated {
            return Ok((into_sink(decoder).buf.freeze(), true));
        }
        return Err(FsmError::upstream(
            label,
            None,
            format!("reading the response body failed: {e}"),
        ));
    }
    let sink = into_sink(decoder);
    let truncated = sink.truncated;
    Ok((sink.buf.freeze(), truncated))
}

/// Stream-decompress `br` or `zstd` via `async-compression` with independent
/// caps on wire bytes and decompressed output bytes.
async fn read_async_decoded_capped(
    resp: &mut reqwest::Response,
    is_zstd: bool,
    cap: u64,
    label: &str,
    secrets: &Secrets<'_>,
) -> Result<(Bytes, bool), FsmError> {
    if is_zstd {
        // RFC 9659 HTTP zstd limit: cap window allocation at 8 MiB (window_log_max=23)
        // instead of libzstd's 128 MiB default.
        let dec = async_compression::tokio::write::ZstdDecoder::new_for_http(CappedSink::new(cap));
        drive_async_decoder(
            resp,
            dec,
            |d| d.get_ref(),
            |d| d.into_inner(),
            cap,
            label,
            secrets,
        )
        .await
    } else {
        let dec = async_compression::tokio::write::BrotliDecoder::new(CappedSink::new(cap));
        drive_async_decoder(
            resp,
            dec,
            |d| d.get_ref(),
            |d| d.into_inner(),
            cap,
            label,
            secrets,
        )
        .await
    }
}

#[async_trait::async_trait]
impl NetClient for HttpClient {
    async fn send(
        &self,
        req: HttpRequest,
        profile: TransportProfile,
        cx: &NetCx,
    ) -> Result<HttpBytes, FsmError> {
        // There is no Cancelled variant; a cancelled call ends as Timeout(0),
        // as in ReplayNet.
        if cx.cancel.is_cancelled() {
            return Err(FsmError::Timeout(0));
        }
        let budget = if cx.timeout.is_zero() {
            self.inner.s.default_timeout
        } else {
            cx.timeout.min(MAX_REQUEST_TIMEOUT)
        };
        let deadline = Instant::now()
            .checked_add(budget)
            .unwrap_or_else(|| Instant::now() + MAX_REQUEST_TIMEOUT);
        let work = self.execute(req, profile, cx, deadline);
        tokio::select! {
            biased;
            () = cx.cancel.cancelled() => Err(FsmError::Timeout(0)),
            r = tokio::time::timeout(budget, work) => r.unwrap_or(Err(FsmError::Timeout(ms(budget)))),
        }
    }

    fn check_url_offline(&self, url: &str) -> Result<(), FsmError> {
        self.inner.guard.check_offline(url).map(|_| ())
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "live_tests.rs"]
mod live_tests;
