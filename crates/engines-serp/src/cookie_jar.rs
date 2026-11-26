//! Manual per-egress cookie jar and manual redirect follower for stateful SERP
//! engines (`bing`, `yahoo`, `baidu`).
//!
//! `wreq` runs with `cookies` OFF (`SPEC.md` §7.1), so stateful cookie flows
//! delegate cookie storage and RFC 6265 matching (`Domain`, `Path`, `Secure`,
//! `Max-Age`, `Expires`) to [`fsm_net::jar::CookieJar`] and follow up to 5
//! redirects manually while re-checking `NetClient::check_url_offline` on every
//! hop. Only hop 0 uses `ncx`'s rate-limit configuration; redirect hops clear
//! `rps` and inherit the remaining [`CallDeadline`].

use crate::common::{CallDeadline, aux_net_cx};
use fsm_core::FsmError;
use fsm_core::config::Config;
use fsm_core::net::{HttpBytes, HttpRequest, NetClient, NetCx};
use fsm_core::source::TransportProfile;
use std::time::SystemTime;
use url::Url;

/// Maximum redirect hops followed manually (matches `SPEC.md` §7.1).
pub const MAX_REDIRECT_HOPS: usize = 5;

/// Per-egress cookie jar backed by [`fsm_net::jar::CookieJar`].
#[derive(Debug, Clone, Default)]
pub struct CookieJar {
    inner: fsm_net::jar::CookieJar,
}

impl CookieJar {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Insert or replace a host/domain cookie (convenience for tests).
    pub fn insert(&mut self, domain: &str, name: impl Into<String>, value: impl Into<String>) {
        let clean = domain.trim().trim_start_matches('.').to_ascii_lowercase();
        let Ok(url) = Url::parse(&format!("https://{clean}/")) else {
            return;
        };
        let header = format!("{}={}; Domain={clean}; Path=/", name.into(), value.into());
        self.inner.store(&url, &header, SystemTime::now());
    }

    /// Absorb every `Set-Cookie` header from `headers` for `url` at `now`.
    pub fn absorb_headers_for_url(
        &mut self,
        url: &Url,
        headers: &[(String, String)],
        now: SystemTime,
    ) {
        for (k, v) in headers {
            if k.eq_ignore_ascii_case("set-cookie") {
                self.inner.store(url, v, now);
            }
        }
    }

    /// Absorb every `Set-Cookie` header from `headers` for `https://{request_host}/`.
    pub fn absorb_headers(&mut self, request_host: &str, headers: &[(String, String)]) {
        let host = request_host.trim().to_ascii_lowercase();
        if let Ok(url) = Url::parse(&format!("https://{host}/")) {
            self.absorb_headers_for_url(&url, headers, SystemTime::now());
        }
    }

    /// Build the `Cookie` header value matching `url` at `now`, or `None`.
    pub fn header_for_url(&self, url: &Url, now: SystemTime) -> Option<String> {
        self.inner.header_for(url, now)
    }

    /// Build the `Cookie` header value matching `https://{host}/`, or `None`.
    pub fn header_for(&self, host: &str) -> Option<String> {
        let host = host.trim().to_ascii_lowercase();
        let url = Url::parse(&format!("https://{host}/")).ok()?;
        self.header_for_url(&url, SystemTime::now())
    }

    /// Lookup a cookie value applicable to `url` by name (for tests).
    pub fn get_for_url(&self, url: &Url, name: &str) -> Option<String> {
        let header = self.header_for_url(url, SystemTime::now())?;
        for pair in header.split("; ") {
            if let Some((k, v)) = pair.split_once('=')
                && k == name
            {
                return Some(v.to_owned());
            }
        }
        None
    }

    /// Lookup a cookie value applicable to `https://{host}/` by name (for tests).
    pub fn get_for_host(&self, host: &str, name: &str) -> Option<String> {
        let url = Url::parse(&format!("https://{}/", host.trim().to_ascii_lowercase())).ok()?;
        self.get_for_url(&url, name)
    }
}

/// Compute the per-egress cookie-jar key for `engine_id` from `Config`.
///
/// Mirrors Python's `_egress_key(curl_proxy_kwargs(engine))`: cookies minted
/// from one exit IP are never replayed from another.
pub fn egress_key(cfg: &Config, engine_id: &str) -> String {
    match &cfg.proxy {
        Some(proxy) if !proxy.trim().is_empty() => {
            if cfg.proxy_engines.is_empty()
                || cfg
                    .proxy_engines
                    .iter()
                    .any(|e| e.eq_ignore_ascii_case(engine_id))
            {
                proxy.clone()
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}

/// GET `url` with `follow_redirects = false`, absorbing `Set-Cookie` into `jar`
/// and manually following up to `MAX_REDIRECT_HOPS` redirects while re-checking
/// `net.check_url_offline` on every hop.
pub async fn get_with_jar(
    net: &dyn NetClient,
    source_id: &str,
    profile: TransportProfile,
    url: &str,
    accept_language: &str,
    jar: &mut CookieJar,
    ncx: &NetCx,
) -> Result<HttpBytes, FsmError> {
    let deadline = CallDeadline::from_timeout(ncx.timeout);
    get_with_jar_deadline(
        net,
        source_id,
        profile,
        url,
        accept_language,
        jar,
        ncx,
        deadline,
    )
    .await
}

/// Like [`get_with_jar`], sharing an explicit per-call [`CallDeadline`].
///
/// Hop 0 uses `ncx` (clamped to `deadline.remaining()`); redirect hops 1..=5
/// clear `rps` (`aux_net_cx`) so internal redirect hops never re-charge the
/// engine's per-call rate limit.
#[allow(clippy::too_many_arguments)]
pub async fn get_with_jar_deadline(
    net: &dyn NetClient,
    source_id: &str,
    profile: TransportProfile,
    url: &str,
    accept_language: &str,
    jar: &mut CookieJar,
    ncx: &NetCx,
    deadline: CallDeadline,
) -> Result<HttpBytes, FsmError> {
    let mut current = Url::parse(url)
        .map_err(|e| FsmError::invalid(format!("{source_id}: invalid URL {url:?}: {e}")))?;

    for hop in 0..=MAX_REDIRECT_HOPS {
        net.check_url_offline(current.as_str())?;
        let hop_ncx = if hop == 0 {
            let mut first = ncx.clone();
            first.timeout = deadline.remaining()?;
            first
        } else {
            aux_net_cx(ncx, deadline)?
        };
        let now = SystemTime::now();
        let mut req = HttpRequest::get(current.as_str())
            .header("Accept", crate::common::ACCEPT_HTML)
            .header("Accept-Language", accept_language)
            .for_source(source_id)
            .redact_header("cookie");
        req.follow_redirects = false;
        if let Some(ck) = jar.header_for_url(&current, now) {
            req = req.header("Cookie", ck);
        }
        let resp = net.send(req, profile, &hop_ncx).await?;
        jar.absorb_headers_for_url(&current, &resp.headers, SystemTime::now());

        if matches!(resp.status, 301 | 302 | 303 | 307 | 308)
            && let Some(loc) = resp.header("location")
        {
            let next = current.join(loc).map_err(|e| {
                FsmError::upstream(
                    source_id,
                    Some(resp.status),
                    format!("{source_id}: invalid redirect location {loc:?}: {e}"),
                )
            })?;
            net.check_url_offline(next.as_str())?;
            current = next;
            continue;
        }
        return Ok(resp);
    }
    Err(FsmError::upstream(
        source_id,
        None,
        format!("{source_id}: too many redirects (>{MAX_REDIRECT_HOPS})"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_jar_domain_matching_and_overwrite() {
        let mut jar = CookieJar::new();
        jar.absorb_headers(
            "www4.bing.com",
            &[
                (
                    "Set-Cookie".into(),
                    "MUID=abc; Domain=.bing.com; Path=/".into(),
                ),
                ("Set-Cookie".into(), "SRCHHPGUSR=v1; Path=/".into()),
            ],
        );
        assert_eq!(
            jar.header_for("www4.bing.com").as_deref(),
            Some("MUID=abc; SRCHHPGUSR=v1")
        );
        assert_eq!(jar.header_for("www.bing.com").as_deref(), Some("MUID=abc"));
        assert_eq!(jar.header_for("notbing.com"), None);

        // Overwrite MUID on same domain
        jar.absorb_headers(
            "www4.bing.com",
            &[(
                "set-cookie".into(),
                "MUID=def; Domain=.bing.com; Path=/".into(),
            )],
        );
        assert_eq!(
            jar.get_for_host("www4.bing.com", "MUID").as_deref(),
            Some("def")
        );
    }

    #[test]
    fn cookie_jar_handles_non_ascii_expires_domain_path_and_secure() {
        let mut jar = CookieJar::new();
        // B2: non-ASCII attribute values must never panic.
        jar.absorb_headers(
            "www4.bing.com",
            &[("Set-Cookie".into(), "MUID=1; 日本語x; Path=/".into())],
        );
        assert_eq!(
            jar.get_for_host("www4.bing.com", "MUID").as_deref(),
            Some("1")
        );

        // M3: Expires in the past deletes existing cookie.
        jar.absorb_headers(
            "search.yahoo.com",
            &[("Set-Cookie".into(), "YBV=v1; Path=/".into())],
        );
        assert_eq!(
            jar.get_for_host("search.yahoo.com", "YBV").as_deref(),
            Some("v1")
        );
        jar.absorb_headers(
            "search.yahoo.com",
            &[(
                "Set-Cookie".into(),
                "YBV=; Expires=Thu, 01 Jan 1970 00:00:00 GMT; Path=/".into(),
            )],
        );
        assert_eq!(jar.get_for_host("search.yahoo.com", "YBV"), None);

        // M3: bare TLD / cross-site Domain attributes are rejected.
        jar.absorb_headers(
            "www4.bing.com",
            &[
                ("Set-Cookie".into(), "badtld=1; Domain=com; Path=/".into()),
                (
                    "Set-Cookie".into(),
                    "cross=1; Domain=evil.com; Path=/".into(),
                ),
            ],
        );
        assert_eq!(jar.header_for("evil.com"), None);

        // M3: Path and Secure attributes are enforced.
        let search_url = Url::parse("https://search.yahoo.com/search?p=x").unwrap();
        let other_path = Url::parse("https://search.yahoo.com/other").unwrap();
        let plain_http = Url::parse("http://search.yahoo.com/search?p=x").unwrap();
        jar.absorb_headers_for_url(
            &search_url,
            &[("Set-Cookie".into(), "SEC=tok; Path=/search; Secure".into())],
            SystemTime::now(),
        );
        assert_eq!(jar.get_for_url(&search_url, "SEC").as_deref(), Some("tok"));
        assert_eq!(jar.get_for_url(&other_path, "SEC"), None);
        assert_eq!(jar.get_for_url(&plain_http, "SEC"), None);
    }

    #[test]
    fn egress_key_respects_proxy_engines() {
        let mut cfg = Config::default();
        assert_eq!(egress_key(&cfg, "bing"), "");
        cfg.proxy = Some("http://127.0.0.1:7890".into());
        assert_eq!(egress_key(&cfg, "bing"), "http://127.0.0.1:7890");
        cfg.proxy_engines = vec!["yahoo".into()];
        assert_eq!(egress_key(&cfg, "bing"), "");
        assert_eq!(egress_key(&cfg, "yahoo"), "http://127.0.0.1:7890");
    }
}
