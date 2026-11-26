//! Manual per-egress cookie jar (`SPEC.md` §7.1): the Chrome stack runs with
//! wreq's `cookies` feature OFF, so the few engines that need a session
//! (Bing warm-up, Yahoo's `_bv` handshake, Baidu) get one here, keyed by
//! `HttpRequest.cookie_jar_id` and by egress (profile + proxy), and every
//! redirect hop of one request sees the cookies the previous hop set.
//!
//! Deliberately small (RFC 6265 subset): host-only vs `Domain` cookies with a
//! no-other-site rule, `Path`, `Secure`, `Max-Age` / `Expires` (incl.
//! deletion). No public-suffix list: a `Domain` attribute must contain a dot
//! and domain-match the setting host, and jars are per engine anyway.

use std::time::{Duration, SystemTime};
use url::Url;

/// Cookies kept per jar; the oldest are evicted first.
pub const MAX_COOKIES_PER_JAR: usize = 128;
/// Longest `name=value` accepted.
pub const MAX_COOKIE_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Cookie {
    name: String,
    value: String,
    domain: String,
    host_only: bool,
    path: String,
    secure: bool,
    expires: Option<SystemTime>,
}

/// One cookie jar.
#[derive(Debug, Clone, Default)]
pub struct CookieJar {
    cookies: Vec<Cookie>,
}

fn default_path(url: &Url) -> String {
    let p = url.path();
    match p.rfind('/') {
        Some(0) | None => "/".to_owned(),
        Some(i) if p.starts_with('/') => p[..i].to_owned(),
        _ => "/".to_owned(),
    }
}

fn path_matches(request_path: &str, cookie_path: &str) -> bool {
    request_path == cookie_path
        || (request_path.starts_with(cookie_path)
            && (cookie_path.ends_with('/') || request_path[cookie_path.len()..].starts_with('/')))
}

fn domain_matches(host: &str, domain: &str) -> bool {
    host == domain
        || (host.len() > domain.len()
            && host.ends_with(domain)
            && host[..host.len() - domain.len()].ends_with('.'))
}

fn host_is_ip(host: &str) -> bool {
    host.parse::<std::net::IpAddr>().is_ok() || host.starts_with('[')
}

/// `Expires` in the RFC 1123 / RFC 850-ish forms servers send.
fn parse_expires(v: &str) -> Option<SystemTime> {
    let v = v.trim();
    let normalized = {
        // "Thursday, 01-Jan-70 00:00:00 GMT" -> "Thu, 01 Jan 70 00:00:00 GMT"
        let mut s = v.to_owned();
        if let Some(comma) = s.find(',') {
            let (raw_wd, rest_after_comma) = s.split_at(comma);
            let wd = match raw_wd.trim().to_ascii_lowercase().as_str() {
                "sunday" => "Sun",
                "monday" => "Mon",
                "tuesday" => "Tue",
                "wednesday" => "Wed",
                "thursday" => "Thu",
                "friday" => "Fri",
                "saturday" => "Sat",
                _ => raw_wd.trim(),
            };
            let tail = rest_after_comma[1..].trim_start();
            let date_end = tail.find(' ').unwrap_or(tail.len());
            let (date, rest) = tail.split_at(date_end);
            s = format!("{wd}, {}{rest}", date.replace('-', " "));
        }
        s
    };
    let ts = jiff::fmt::rfc2822::DateTimeParser::new()
        .relaxed_weekday(true)
        .parse_timestamp(&normalized)
        .ok()
        .or_else(|| normalized.parse::<jiff::Timestamp>().ok())?;
    let secs = ts.as_second();
    if secs <= 0 {
        return Some(SystemTime::UNIX_EPOCH);
    }
    SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(u64::try_from(secs).ok()?))
}

impl CookieJar {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.cookies.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cookies.is_empty()
    }

    /// Store one `Set-Cookie` header value received from `url`.
    pub fn store(&mut self, url: &Url, set_cookie: &str, now: SystemTime) {
        const MAX_SET_COOKIE_BYTES: usize = 8_192;
        const MAX_COOKIE_PATH_BYTES: usize = 1_024;
        let has_ctl = |s: &str| s.bytes().any(|b| b < 0x20 || b == 0x7f);

        if set_cookie.len() > MAX_SET_COOKIE_BYTES {
            return;
        }
        let Some(host) = url
            .host_str()
            .map(|h| h.trim_end_matches('.').to_ascii_lowercase())
        else {
            return;
        };
        let mut parts = set_cookie.split(';');
        let Some((name, value)) = parts.next().and_then(|nv| nv.split_once('=')) else {
            return;
        };
        let (name, value) = (name.trim(), value.trim());
        if name.is_empty()
            || name.len() + value.len() > MAX_COOKIE_BYTES
            || has_ctl(name)
            || has_ctl(value)
        {
            return;
        }
        let def_path = default_path(url);
        let initial_path = if def_path.len() <= MAX_COOKIE_PATH_BYTES && !has_ctl(&def_path) {
            def_path
        } else {
            "/".to_owned()
        };
        let mut cookie = Cookie {
            name: name.to_owned(),
            value: value.to_owned(),
            domain: host.clone(),
            host_only: true,
            path: initial_path,
            secure: false,
            expires: None,
        };
        let mut max_age: Option<i64> = None;
        for attr in parts {
            let (k, v) = attr
                .split_once('=')
                .map_or((attr.trim(), ""), |(k, v)| (k.trim(), v.trim()));
            match k.to_ascii_lowercase().as_str() {
                "domain" if !v.is_empty() => {
                    if has_ctl(v) {
                        return;
                    }
                    let d = v
                        .trim_start_matches('.')
                        .trim_end_matches('.')
                        .to_ascii_lowercase();
                    // No cookie for another site, for a bare TLD, or on an IP host.
                    if !d.contains('.') || host_is_ip(&host) || !domain_matches(&host, &d) {
                        return;
                    }
                    cookie.domain = d;
                    cookie.host_only = false;
                }
                "path" if v.starts_with('/') => {
                    if v.len() > MAX_COOKIE_PATH_BYTES || has_ctl(v) {
                        return;
                    }
                    cookie.path = v.to_owned();
                }
                "secure" => cookie.secure = true,
                "max-age" => max_age = v.parse::<i64>().ok(),
                "expires" if max_age.is_none() => cookie.expires = parse_expires(v),
                _ => {}
            }
        }
        if let Some(secs) = max_age {
            cookie.expires = Some(if secs <= 0 {
                SystemTime::UNIX_EPOCH
            } else {
                now + Duration::from_secs(
                    u64::try_from(secs)
                        .unwrap_or(u64::MAX / 4)
                        .min(400 * 86_400),
                )
            });
        }
        self.cookies.retain(|c| {
            !(c.name == cookie.name && c.domain == cookie.domain && c.path == cookie.path)
        });
        if cookie.expires.is_some_and(|e| e <= now) {
            return; // an expiry in the past deletes
        }
        if self.cookies.len() >= MAX_COOKIES_PER_JAR {
            self.cookies.retain(|c| c.expires.is_none_or(|e| e > now));
            if self.cookies.len() >= MAX_COOKIES_PER_JAR {
                self.cookies.remove(0);
            }
        }
        self.cookies.push(cookie);
    }

    /// The `Cookie` header value for a request to `url`, if any cookie applies.
    pub fn header_for(&self, url: &Url, now: SystemTime) -> Option<String> {
        let host = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();
        let path = url.path();
        let https = url.scheme() == "https";
        let mut hits: Vec<&Cookie> = self
            .cookies
            .iter()
            .filter(|c| c.expires.is_none_or(|e| e > now))
            .filter(|c| {
                if c.host_only {
                    c.domain == host
                } else {
                    domain_matches(&host, &c.domain)
                }
            })
            .filter(|c| path_matches(path, &c.path))
            .filter(|c| !c.secure || https)
            .collect();
        if hits.is_empty() {
            return None;
        }
        // Longer paths first (RFC 6265 §5.4); stable otherwise.
        hits.sort_by_key(|c| std::cmp::Reverse(c.path.len()));
        Some(
            hits.iter()
                .map(|c| format!("{}={}", c.name, c.value))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }

    /// Names of the cookies that would be sent to `url` (diagnostics, tests).
    pub fn names_for(&self, url: &Url, now: SystemTime) -> Vec<String> {
        self.header_for(url, now)
            .map(|h| {
                h.split("; ")
                    .filter_map(|p| p.split_once('=').map(|(n, _)| n.to_owned()))
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn host_only_cookie_round_trip() {
        let now = SystemTime::now();
        let mut j = CookieJar::new();
        j.store(
            &u("https://www.bing.com/search?q=x"),
            "MUID=abc; Path=/; HttpOnly",
            now,
        );
        assert_eq!(
            j.header_for(&u("https://www.bing.com/news"), now)
                .as_deref(),
            Some("MUID=abc")
        );
        // Host-only: not for a sibling or the parent.
        assert_eq!(j.header_for(&u("https://bing.com/"), now), None);
        assert_eq!(j.header_for(&u("https://cn.bing.com/"), now), None);
    }

    #[test]
    fn domain_cookie_reaches_subdomains_but_never_another_site() {
        let now = SystemTime::now();
        let mut j = CookieJar::new();
        j.store(
            &u("https://www4.bing.com/"),
            "SRCHD=AF; Domain=.bing.com; Path=/",
            now,
        );
        assert_eq!(
            j.header_for(&u("https://www.bing.com/x"), now).as_deref(),
            Some("SRCHD=AF")
        );
        // A response cannot set a cookie for another site, a bare TLD, or on an IP.
        j.store(&u("https://evil.example/"), "steal=1; Domain=bing.com", now);
        j.store(&u("https://evil.example/"), "tld=1; Domain=com", now);
        j.store(&u("http://127.0.0.1/"), "ip=1; Domain=0.0.1", now);
        assert_eq!(j.len(), 1);
        // Suffix tricks do not domain-match.
        assert_eq!(j.header_for(&u("https://notbing.com/"), now), None);
    }

    #[test]
    fn path_secure_and_expiry() {
        let now = SystemTime::now();
        let mut j = CookieJar::new();
        j.store(&u("https://s.yahoo.com/search/x"), "B=1", now); // default path /search
        j.store(&u("https://s.yahoo.com/"), "S=2; Secure", now);
        j.store(
            &u("https://s.yahoo.com/"),
            "old=3; Expires=Wed, 21-Oct-2015 07:28:00 GMT",
            now,
        );
        j.store(
            &u("https://s.yahoo.com/"),
            "later=4; Expires=Fri, 01 Jan 2100 00:00:00 GMT",
            now,
        );
        assert_eq!(
            j.names_for(&u("https://s.yahoo.com/search/y"), now),
            ["B", "S", "later"]
        );
        assert_eq!(
            j.names_for(&u("http://s.yahoo.com/searchy"), now),
            ["later"]
        );
        // Max-Age=0 deletes; Max-Age wins over Expires.
        j.store(&u("https://s.yahoo.com/"), "S=2; Max-Age=0", now);
        j.store(
            &u("https://s.yahoo.com/"),
            "m=5; Max-Age=60; Expires=Wed, 21 Oct 2015 07:28:00 GMT",
            now,
        );
        assert_eq!(j.names_for(&u("https://s.yahoo.com/"), now), ["later", "m"]);
        assert_eq!(
            j.names_for(&u("https://s.yahoo.com/"), now + Duration::from_secs(120)),
            ["later"]
        );
    }

    #[test]
    fn replacing_and_bounding() {
        let now = SystemTime::now();
        let mut j = CookieJar::new();
        j.store(&u("https://a.example/"), "k=1", now);
        j.store(&u("https://a.example/"), "k=2", now);
        assert_eq!(
            j.header_for(&u("https://a.example/"), now).as_deref(),
            Some("k=2")
        );
        for i in 0..(MAX_COOKIES_PER_JAR + 10) {
            j.store(&u("https://a.example/"), &format!("c{i}=v"), now);
        }
        assert_eq!(j.len(), MAX_COOKIES_PER_JAR);
        j.store(
            &u("https://a.example/"),
            &format!("big={}", "x".repeat(MAX_COOKIE_BYTES)),
            now,
        );
        assert!(
            j.names_for(&u("https://a.example/"), now)
                .iter()
                .all(|n| n != "big")
        );
        j.store(&u("https://a.example/"), "novalue", now);
        j.store(&u("https://a.example/"), "=anon", now);
        assert_eq!(j.len(), MAX_COOKIES_PER_JAR);
    }

    /// I6 regression (Missing test #10): quoted cookie values round-trip
    /// verbatim; mismatched/full weekday Expires deletes; expired cookies are
    /// purged before evicting live entries.
    #[test]
    fn quoted_values_relaxed_expires_and_purge_before_eviction() {
        let now = SystemTime::now();
        let url = u("https://example.com/");
        let mut j = CookieJar::new();

        // Quoted value stays quoted (RFC 6265 §4.1.1 / §5.2).
        j.store(&url, r#"tok="abc"; Path=/"#, now);
        assert_eq!(j.header_for(&url, now).as_deref(), Some(r#"tok="abc""#));

        // Mismatched weekday in Expires (1970-01-01 was Thursday, not Sunday)
        // and RFC 850 full weekday still parse and delete the cookie.
        j.store(&url, "sid=live; Path=/", now);
        j.store(
            &url,
            "sid=x; Expires=Sun, 01-Jan-1970 00:00:00 GMT; Path=/",
            now,
        );
        assert!(
            j.names_for(&url, now).iter().all(|n| n != "sid"),
            "mismatched-weekday past Expires must delete sid"
        );

        j.store(&url, "sid2=live; Path=/", now);
        j.store(
            &url,
            "sid2=x; Expires=Thursday, 01-Jan-70 00:00:00 GMT; Path=/",
            now,
        );
        assert!(
            j.names_for(&url, now).iter().all(|n| n != "sid2"),
            "RFC 850 full-weekday past Expires must delete sid2"
        );

        // Expired entries are purged before evicting live cookies when full.
        let mut full = CookieJar::new();
        full.store(&url, "short=1; Max-Age=1; Path=/", now);
        for i in 1..MAX_COOKIES_PER_JAR {
            full.store(&url, &format!("live{i}=v; Path=/"), now);
        }
        assert_eq!(full.len(), MAX_COOKIES_PER_JAR);
        let later = now + Duration::from_secs(10);
        full.store(&url, "fresh=1; Path=/", later);
        let names = full.names_for(&url, later);
        assert_eq!(names.len(), MAX_COOKIES_PER_JAR);
        assert!(
            names.iter().any(|n| n == "live1"),
            "live1 must not be evicted"
        );
        assert!(names.iter().any(|n| n == "fresh"));
    }

    #[test]
    fn rejects_oversized_path_and_control_characters() {
        let now = SystemTime::now();
        let url = u("https://example.com/");
        let mut j = CookieJar::new();

        // Huge Path attribute (20 KiB) must be rejected so jar memory stays bounded.
        let huge_path = format!("a=1; Path=/{}", "x".repeat(20_000));
        j.store(&url, &huge_path, now);
        assert!(j.is_empty(), "oversized Set-Cookie/Path must be rejected");

        // Control characters (CR/LF/NUL) in name, value, or path must be rejected.
        for bad in [
            "a=1\r\nX-Injected: 1",
            "a\nb=1",
            "a=val\0ue",
            "a=1; Path=/ok\r\nbad",
        ] {
            j.store(&url, bad, now);
        }
        assert!(
            j.is_empty(),
            "cookies containing control characters must be rejected"
        );
    }
}
