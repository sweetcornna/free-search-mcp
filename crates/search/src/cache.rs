//! Search-cache key (`v4`) and TTLs (`SPEC.md` §5; ports of `_key` and
//! `_read_ttl`).

use fsm_core::scopes;
use fsm_core::{Config, Freshness, SourceDecl, TransportProfile};
use serde_json::json;
use sha2::{Digest, Sha256};

/// The key's own version: bump it to orphan every existing row.
pub const KEY_VERSION: u32 = 4;
/// Default TTL of a web search.
pub const WEB_TTL_S: u64 = 6 * 3600;
/// Academic searches (scholarly indexes move slowly).
pub const ACADEMIC_TTL_S: u64 = 12 * 3600;
/// A run on a thinned pool is replayed for an hour at most.
pub const DEGRADED_TTL_S: u64 = 3600;
/// News, software and security records move within hours.
pub const NEWS_TTL_S: u64 = 6 * 3600;

/// Wire spelling of a freshness value.
pub fn freshness_str(f: Freshness) -> &'static str {
    match f {
        Freshness::Day => "day",
        Freshness::Week => "week",
        Freshness::Month => "month",
        _ => "year",
    }
}

/// What the key is computed from.
#[derive(Debug, Clone, Copy)]
pub struct KeyParts<'a> {
    pub query: &'a str,
    /// Canonical scope id.
    pub scope: &'a str,
    /// The nominal pool (or the named sources).
    pub sources: &'a [String],
    pub freshness: Option<Freshness>,
    pub domains: &'a [String],
    pub lang: Option<&'a str>,
    pub limit: u32,
}

/// `v4 = sha256(query, scope, sorted sources, freshness, sorted domains,
/// lang, limit, region, safesearch, accept_language)` over canonical JSON.
pub fn cache_key(p: &KeyParts<'_>, cfg: &Config) -> String {
    let mut sources: Vec<&str> = p.sources.iter().map(String::as_str).collect();
    sources.sort_unstable();
    sources.dedup();
    let mut domains: Vec<String> = p
        .domains
        .iter()
        .map(|d| d.trim().to_ascii_lowercase())
        .filter(|d| !d.is_empty())
        .collect();
    domains.sort_unstable();
    domains.dedup();
    let v = json!({
        "v": KEY_VERSION,
        "q": p.query,
        "scope": p.scope,
        "sources": sources,
        "freshness": p.freshness.map(freshness_str),
        "domains": domains,
        "lang": p.lang,
        "limit": p.limit,
        "region": cfg.region,
        "safesearch": cfg.safesearch.as_str(),
        "accept_language": cfg.accept_language,
    });
    let canon = fsm_core::fixture::canonical_json(&v);
    hex::encode(Sha256::digest(canon.as_bytes()))
}

/// Cap of a freshness request (`_FRESHNESS_CACHE_TTL`).
pub fn freshness_ttl(f: Option<Freshness>) -> Option<u64> {
    match f? {
        Freshness::Day => Some(3600),
        Freshness::Week => Some(6 * 3600),
        Freshness::Month => Some(24 * 3600),
        _ => None,
    }
}

/// Cap of the scope token as the caller wrote it (synonyms such as
/// `weather` resolve to `reference`, so the raw token is consulted first).
pub fn token_ttl(token: &str) -> Option<u64> {
    let t = token.trim().to_ascii_lowercase();
    match t.as_str() {
        "weather" | "finance.fx" => return Some(3600),
        "calendar.clock" | "worldclock" => return Some(0),
        _ => {}
    }
    scope_ttl(scopes::resolve(&t)?.id)
}

fn scope_ttl(scope: &str) -> Option<u64> {
    match scopes::root(scope) {
        "news" | "code" | "security" => Some(NEWS_TTL_S),
        _ => None,
    }
}

/// Cap a participating source imposes: local computation is never cached,
/// a declared `cache_ttl_s` caps, and so do its scopes.
pub fn source_ttl(decl: &SourceDecl) -> Option<u64> {
    if decl.transport == TransportProfile::Local {
        return Some(0);
    }
    let declared = (decl.limits.cache_ttl_s > 0).then_some(decl.limits.cache_ttl_s);
    let by_scope = decl.scopes.iter().filter_map(|s| scope_ttl(s)).min();
    [declared, by_scope].into_iter().flatten().min()
}

/// The TTL of this search: the tightest of the configured cache TTL, the
/// scope's default (6 h, academic 12 h), freshness, the scope token and every
/// participating source; one hour when degraded. `0` = do not cache.
pub fn ttl(
    token: &str,
    scope: &str,
    freshness: Option<Freshness>,
    decls: &[&SourceDecl],
    degraded: bool,
    cfg: &Config,
) -> u64 {
    let base = if scopes::root(scope) == "academic" {
        ACADEMIC_TTL_S
    } else {
        WEB_TTL_S
    };
    let mut caps = vec![cfg.cache_ttl_s, base];
    caps.extend(freshness_ttl(freshness));
    caps.extend(token_ttl(token));
    caps.extend(decls.iter().filter_map(|d| source_ttl(d)));
    if degraded {
        caps.push(DEGRADED_TTL_S);
    }
    caps.into_iter().min().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsm_core::SourceLimits;

    fn cfg() -> Config {
        Config::defaults_without_home()
    }

    fn parts<'a>(q: &'a str, sources: &'a [String], domains: &'a [String]) -> KeyParts<'a> {
        KeyParts {
            query: q,
            scope: "web",
            sources,
            freshness: None,
            domains,
            lang: None,
            limit: 10,
        }
    }

    #[test]
    fn the_key_ignores_order_but_not_content() {
        let c = cfg();
        let a = cache_key(
            &parts("rust", &["bing".into(), "duckduckgo".into()], &[]),
            &c,
        );
        let b = cache_key(
            &parts("rust", &["duckduckgo".into(), "bing".into()], &[]),
            &c,
        );
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        let d1 = cache_key(&parts("rust", &[], &["a.com".into(), "-b.com".into()]), &c);
        let d2 = cache_key(&parts("rust", &[], &["-B.com".into(), "a.com".into()]), &c);
        assert_eq!(d1, d2);
        assert_ne!(
            a,
            cache_key(
                &parts("rust lang", &["bing".into(), "duckduckgo".into()], &[]),
                &c
            )
        );
        let mut p = parts("rust", &[], &[]);
        let base = cache_key(&p, &c);
        p.limit = 5;
        assert_ne!(base, cache_key(&p, &c));
        p.limit = 10;
        p.scope = "code";
        assert_ne!(base, cache_key(&p, &c));
        p.scope = "web";
        p.freshness = Some(Freshness::Day);
        assert_ne!(base, cache_key(&p, &c));
        p.freshness = None;
        p.lang = Some("zh");
        assert_ne!(base, cache_key(&p, &c));
    }

    #[test]
    fn region_safesearch_and_accept_language_are_in_the_key() {
        // A server restarted with another region must not replay old answers.
        let mut c = cfg();
        let p = parts("rust", &[], &[]);
        let base = cache_key(&p, &c);
        c.region = "jp-ja".into();
        assert_ne!(base, cache_key(&p, &c));
        let mut c = cfg();
        c.accept_language = "de-DE".into();
        assert_ne!(base, cache_key(&p, &c));
    }

    #[test]
    fn ttls_follow_the_spec_table() {
        let c = cfg();
        assert_eq!(ttl("web", "web", None, &[], false, &c), 6 * 3600);
        assert_eq!(
            ttl("web", "web", Some(Freshness::Day), &[], false, &c),
            3600
        );
        assert_eq!(
            ttl("web", "web", Some(Freshness::Week), &[], false, &c),
            6 * 3600
        );
        assert_eq!(
            ttl("web", "web", Some(Freshness::Month), &[], false, &c),
            6 * 3600
        );
        assert_eq!(ttl("news", "news", None, &[], false, &c), 6 * 3600);
        assert_eq!(ttl("weather", "reference", None, &[], false, &c), 3600);
        assert_eq!(ttl("finance.fx", "finance", None, &[], false, &c), 3600);
        assert_eq!(
            ttl("software.python", "code", None, &[], false, &c),
            6 * 3600
        );
        assert_eq!(ttl("calendar.clock", "reference", None, &[], false, &c), 0);
        assert_eq!(ttl("web", "web", None, &[], true, &c), 3600);
        assert_eq!(ttl("academic", "academic", None, &[], false, &c), 12 * 3600);
        let mut small = cfg();
        small.cache_ttl_s = 60;
        assert_eq!(ttl("web", "web", None, &[], false, &small), 60);
    }

    #[test]
    fn participating_sources_cap_the_ttl() {
        let c = cfg();
        let mut clock = SourceDecl::new("worldclock", "World clock", "local time");
        clock.transport = TransportProfile::Local;
        assert_eq!(ttl("web", "web", None, &[&clock], false, &c), 0);
        let mut fx = SourceDecl::new("frankfurter", "FX", "rates");
        fx.limits = SourceLimits::new(1.0, 1, 1, 0, 1800);
        assert_eq!(ttl("web", "web", None, &[&fx], false, &c), 1800);
        let pypi = SourceDecl::new("pypi", "PyPI", "packages").with_scopes(&["code"]);
        assert_eq!(source_ttl(&pypi), Some(6 * 3600));
    }
}
