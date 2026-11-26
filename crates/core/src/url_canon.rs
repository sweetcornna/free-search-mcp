//! URL identity helpers shared by search, scholar and fetch
//! (ports of `aggregator._normalize_url`, `_url_key`, `_canonical_host`,
//! `engines.base.host_matches` and `engines.yahoo.resolve_yahoo_url`).

use percent_encoding::percent_decode_str;
use url::Url;

/// The URL to emit: scheme-relative `//x` gets `https:`, the fragment and
/// trailing slashes are dropped. Nothing else changes.
pub fn normalize_url(url: &str) -> String {
    let url = url.trim();
    let owned;
    let url = if url.starts_with("//") {
        owned = format!("https:{url}");
        owned.as_str()
    } else {
        url
    };
    let no_frag = url.split_once('#').map_or(url, |(a, _)| a);
    no_frag.trim_end_matches('/').to_owned()
}

/// Identity of a page for rank fusion — a key, never a URL to emit.
///
/// Ignores what cannot change which page it is: scheme, a leading `www.`,
/// host case, a default port, the fragment, a trailing slash. Keeps the
/// query (tracking parameters included) and does not fold ccTLDs.
pub fn url_key(url: &str) -> String {
    let normalized = normalize_url(url);
    let Ok(parsed) = Url::parse(&normalized) else {
        return normalized;
    };
    let Some(host) = parsed.host_str().filter(|h| !h.is_empty()) else {
        return normalized;
    };
    let host = host.to_ascii_lowercase();
    let mut key = host.strip_prefix("www.").unwrap_or(&host).to_owned();
    if let Some(port) = parsed.port().filter(|p| *p != 80 && *p != 443) {
        key.push_str(&format!(":{port}"));
    }
    key.push_str(parsed.path().trim_end_matches('/'));
    if let Some(q) = parsed.query().filter(|q| !q.is_empty()) {
        key.push('?');
        key.push_str(q);
    }
    key
}

/// Lower-cased host of an absolute URL (`""` when there is none).
pub fn host_of(url: &str) -> String {
    let url = url.trim();
    let candidate = if url.starts_with("//") {
        format!("https:{url}")
    } else {
        url.to_owned()
    };
    Url::parse(&candidate)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        .unwrap_or_default()
}

const HOST_PREFIXES: [&str; 4] = ["www.", "m.", "amp.", "mobile."];
const TLD_NORMALIZE: [&str; 4] = [".co.uk", ".co.jp", ".com.au", ".co.in"];

/// Dedup signal: strip one mobile/AMP prefix and collapse syndication ccTLDs
/// to `.com` (`www.bbc.co.uk` → `bbc.com`). Never used as identity.
pub fn canonical_host(url: &str) -> String {
    let mut h = host_of(url);
    if let Some(p) = HOST_PREFIXES.iter().find(|p| h.starts_with(**p)) {
        h = h[p.len()..].to_owned();
    }
    if let Some(t) = TLD_NORMALIZE.iter().find(|t| h.ends_with(**t)) {
        h = format!("{}.com", &h[..h.len() - t.len()]);
    }
    h
}

/// `host` equals one of `suffixes` or is a subdomain of one.
pub fn host_matches(host: &str, suffixes: &[&str]) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    suffixes.iter().any(|s| {
        let s = s.trim_start_matches('.').to_ascii_lowercase();
        host == s
            || (host.len() > s.len()
                && host.ends_with(&s)
                && host.as_bytes()[host.len() - s.len() - 1] == b'.')
    })
}

/// Yahoo click-redirect hosts (`r.` and, per Spike N, `rd.`).
pub const YAHOO_REDIRECT_HOSTS: [&str; 2] = ["r.search.yahoo.com", "rd.search.yahoo.com"];

/// Unwrap a Yahoo click redirect to its target.
///
/// * not a redirect → `Some(href)` (scheme-relative gets `https:`);
/// * redirect with an `/RU=<target>/` that decodes (exactly once) to an
///   http(s) URL → `Some(target)`;
/// * redirect without a usable target, or empty input → `None` (skip it
///   rather than emit Yahoo's tracking link).
pub fn resolve_yahoo_url(href: &str) -> Option<String> {
    let href = href.trim();
    if href.is_empty() {
        return None;
    }
    let href = if href.starts_with("//") {
        format!("https:{href}")
    } else {
        href.to_owned()
    };
    let host = host_of(&href);
    if !YAHOO_REDIRECT_HOSTS.contains(&host.as_str()) {
        return Some(href);
    }
    let start = href.find("/RU=")? + 4;
    let rest = &href[start..];
    let raw = rest.split('/').next().unwrap_or("");
    if raw.is_empty() {
        return None;
    }
    let target = percent_decode_str(raw).decode_utf8_lossy().into_owned();
    let lower = target.to_ascii_lowercase();
    (lower.starts_with("http://") || lower.starts_with("https://")).then_some(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_host_matches_python_tests() {
        // tests/test_dedup_and_lead.py
        assert_eq!(canonical_host("https://www.bbc.co.uk/news"), "bbc.com");
        assert_eq!(canonical_host("https://amp.example.com/x"), "example.com");
        assert_eq!(canonical_host("https://m.example.com/x"), "example.com");
        assert_eq!(
            canonical_host("https://mobile.example.com/x"),
            "example.com"
        );
        assert_eq!(canonical_host("https://news.co.jp/foo"), "news.com");
        assert_eq!(canonical_host("https://shop.com.au/foo"), "shop.com");
        assert_eq!(canonical_host("https://times.co.in/foo"), "times.com");
        assert_eq!(canonical_host("not-a-url"), "");
        assert_eq!(canonical_host("https://example.com/x"), "example.com");
    }

    #[test]
    fn url_key_ignores_scheme_www_port_fragment_slash() {
        let k = url_key("http://arxiv.org/abs/1512.03385");
        assert_eq!(k, "arxiv.org/abs/1512.03385");
        assert_eq!(url_key("https://www.arxiv.org/abs/1512.03385/#v2"), k);
        assert_eq!(url_key("https://ARXIV.org:443/abs/1512.03385"), k);
        assert_eq!(
            url_key("http://example.com:8080/a?b=1&utm_source=x"),
            "example.com:8080/a?b=1&utm_source=x"
        );
        assert_ne!(
            url_key("https://amazon.co.uk/dp/X"),
            url_key("https://amazon.com/dp/X")
        );
        assert_eq!(url_key("//example.com/x/"), "example.com/x");
        assert_eq!(url_key("not a url"), "not a url");
    }

    #[test]
    fn normalize_keeps_everything_but_fragment_and_slash() {
        assert_eq!(normalize_url("https://a.org/x/#frag"), "https://a.org/x");
        assert_eq!(normalize_url("//a.org/p"), "https://a.org/p");
        assert_eq!(normalize_url("https://a.org/x?q=1"), "https://a.org/x?q=1");
    }

    #[test]
    fn host_matching() {
        assert!(host_matches("www.arxiv.org", &["arxiv.org"]));
        assert!(host_matches("arxiv.org", &["arxiv.org"]));
        assert!(!host_matches("notarxiv.org", &["arxiv.org"]));
        assert!(host_matches(
            "images.search.yahoo.com",
            &["search.yahoo.com"]
        ));
        assert_eq!(host_of("HTTPS://Example.COM/x"), "example.com");
    }

    fn wrap(target: &str, host: &str) -> String {
        format!(
            "https://{host}/_ylt=A2RRAQgvZuVqxwIAQnlXNyoA;_ylu=Y29sbwM-/RV=2/RE=1794628399/RO=10/RU={target}/RK=2/RS=abc-"
        )
    }

    #[test]
    fn yahoo_redirects_match_python_tests() {
        // tests/test_yahoo.py
        let cpp = wrap(
            "https%3a%2f%2fen.wikipedia.org%2fwiki%2fC%252B%252B",
            "r.search.yahoo.com",
        );
        assert_eq!(
            resolve_yahoo_url(&cpp).as_deref(),
            Some("https://en.wikipedia.org/wiki/C%2B%2B")
        );
        let privacy = "https://r.search.yahoo.com/_ylt=A2RRAQgvZuVqxwIAUXlXNyoA/RV=2/RE=1794628399/RO=10/RU=https%3a%2f%2fguce.yahoo.com%2fstate-controls%3flocale%3den-US%26state%3dCA/RK=2/RS=yP5tQTB6xKuQpZ3ohAlA7RnUyzY-";
        assert_eq!(
            resolve_yahoo_url(privacy).as_deref(),
            Some("https://guce.yahoo.com/state-controls?locale=en-US&state=CA")
        );
        assert_eq!(
            resolve_yahoo_url("https://example.com/x?a=1").as_deref(),
            Some("https://example.com/x?a=1")
        );
        for bad in [
            "https://r.search.yahoo.com/_ylt=abc/RV=2/RE=1/RO=10/RK=2/RS=x-",
            "https://r.search.yahoo.com/_ylt=abc/RV=2/RU=javascript%3aalert(1)/RK=2",
            "",
        ] {
            assert_eq!(resolve_yahoo_url(bad), None, "{bad}");
        }
        // Spike N: rd.* wrappers are unwrapped too (the Python version dropped them).
        let rd = wrap("https%3a%2f%2fdocs.rs%2ftokio", "rd.search.yahoo.com");
        assert_eq!(
            resolve_yahoo_url(&rd).as_deref(),
            Some("https://docs.rs/tokio")
        );
    }
}
