//! Direct single-record lookups (OpenAlex `$0`, Semantic Scholar, Crossref,
//! arXiv Atom `id_list`, PMC idconv) through `SourceDeps::net`, with the
//! response cache and the UTC-midnight bench (SPEC.md §6, I16).
//!
//! * Raw response bodies are cached in `api_cache` (OpenAlex / S2 / arXiv /
//!   idconv 30 days, Crossref 7 days because its `updated-by` notices move;
//!   a 404 is remembered for one day as the body `null`). Merged works are
//!   never written here.
//! * OpenAlex and Semantic Scholar are benched until the next UTC midnight on
//!   their first HTTP 429, in memory and in `api_cache`
//!   (`scholar:bench:<provider>`) so a restarted server keeps the bench.
//! * An upstream HTTP 429 arrives as `Ok(HttpBytes { status: 429 })` (or
//!   `Err(FsmError::Upstream { status: Some(429) })`) and triggers the bench;
//!   `Err(FsmError::RateLimited)` from `NetClient::send` is a local GCRA wait
//!   or active `Retry-After` penalty and does NOT bench for the day.

use crate::ids::Pid;
use fsm_core::fixture::{request_url, sha256_hex};
use fsm_core::{
    Bytes, CancellationToken, FsmError, HttpRequest, NetCx, Registry, SourceDeps, TransportProfile,
};
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Characters escaped when an id goes into a URL path segment (the WHATWG
/// path set plus `%`, `[`, `]`, `\`, `^`, `|`). `/`, `(`, `)` and `:` stay.
const PATH: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

fn enc(s: &str) -> String {
    utf8_percent_encode(s, PATH).to_string()
}

/// One upstream `fsm-scholar` calls directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Provider {
    OpenAlex,
    SemanticScholar,
    Crossref,
    Arxiv,
    PmcIdconv,
}

impl Provider {
    /// Source id used for attribution (`HttpRequest::source_id`, `sources`).
    pub fn id(self) -> &'static str {
        match self {
            Self::OpenAlex => "openalex",
            Self::SemanticScholar => "semanticscholar",
            Self::Crossref => "crossref",
            Self::Arxiv => "arxiv",
            Self::PmcIdconv => "pmc_idconv",
        }
    }

    /// OpenAlex and S2 bench until UTC midnight on their first 429.
    pub fn benches_on_429(self) -> bool {
        matches!(self, Self::OpenAlex | Self::SemanticScholar)
    }

    /// The provider behind a registered source id (`"semanticscholar"` /
    /// `"s2"` → S2, ...).
    pub fn for_source_id(id: &str) -> Option<Self> {
        match id {
            "openalex" => Some(Self::OpenAlex),
            "semanticscholar" | "s2" => Some(Self::SemanticScholar),
            "crossref" => Some(Self::Crossref),
            "arxiv" => Some(Self::Arxiv),
            "pmc_idconv" => Some(Self::PmcIdconv),
            _ => None,
        }
    }

    /// Shared rate-limit bucket and rate when no manifest for the provider
    /// is registered (the manifests' `limits.host_bucket` / `rps` otherwise).
    fn default_limits(self) -> (&'static str, f64) {
        match self {
            Self::OpenAlex => ("api.openalex.org", 5.0),
            Self::SemanticScholar => ("api.semanticscholar.org", 1.0),
            Self::Crossref => ("api.crossref.org", 5.0),
            Self::Arxiv => ("export.arxiv.org", 0.33),
            Self::PmcIdconv => ("pmc.ncbi.nlm.nih.gov", 3.0),
        }
    }

    fn is_xml(self) -> bool {
        matches!(self, Self::Arxiv)
    }

    fn cache_ttl_s(self) -> u64 {
        match self {
            Self::Crossref => 7 * DAY,
            _ => 30 * DAY,
        }
    }
}

/// A 2xx body worth caching parses as what the provider speaks (safe XML
/// without DTD/entities for arXiv; a JSON object for every JSON API).
fn well_formed(p: Provider, body: &[u8]) -> bool {
    if p.is_xml() {
        std::str::from_utf8(body).is_ok_and(|s| crate::parse::parse_xml_safe(s).is_some())
    } else {
        serde_json::from_slice::<serde_json::Value>(body).is_ok_and(|v| v.is_object())
    }
}

pub(crate) const DAY: u64 = 86_400;
const NOT_FOUND_TTL_S: u64 = DAY;
const MAX_BYTES: u64 = 4 << 20;

/// OpenAlex fields fetched for a work (keeps the record small).
pub(crate) const OPENALEX_SELECT: &str = "id,doi,display_name,publication_year,publication_date,type,cited_by_count,is_retracted,primary_location,best_oa_location,open_access,authorships,abstract_inverted_index,ids";
/// Semantic Scholar fields fetched for a paper.
pub(crate) const S2_FIELDS: &str = "paperId,title,abstract,year,publicationDate,venue,citationCount,influentialCitationCount,isOpenAccess,openAccessPdf,externalIds,authors";
/// Crossref fields of a title query (a published-version candidate).
pub(crate) const CROSSREF_SELECT: &str = "DOI,title,author,issued,published-print,published-online,type,container-title,URL,is-referenced-by-count";

/// The next 00:00:00 UTC strictly after `now` (Unix seconds).
pub fn next_utc_midnight(now: i64) -> i64 {
    (now.div_euclid(DAY as i64) + 1) * DAY as i64
}

/// RFC 3339 rendering of a Unix timestamp, for notes.
pub(crate) fn rfc3339(secs: i64) -> String {
    jiff::Timestamp::from_second(secs).map_or_else(|_| secs.to_string(), |t| t.to_string())
}

/// What one lookup produced.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Fetched {
    Found(Bytes),
    NotFound,
    /// Skipped: benched until this Unix time.
    Benched(i64),
    Failed(String),
}

/// Unix-seconds clock (injectable for tests).
pub(crate) type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

pub(crate) fn system_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
    })
}

/// Bench state shared by every lookup of one [`crate::ScholarCore`].
#[derive(Default)]
pub(crate) struct Bench {
    until: Mutex<HashMap<Provider, i64>>,
    /// Providers whose persisted bench has been read. An async lock held
    /// across the store read, so concurrent first callers wait for it.
    loaded: tokio::sync::Mutex<HashSet<Provider>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn bench_key(p: Provider) -> String {
    format!("scholar:bench:{}", p.id())
}

impl Bench {
    /// `Some(until)` while `p` is benched at `now`.
    pub(crate) async fn until(&self, p: Provider, deps: &SourceDeps, now: i64) -> Option<i64> {
        if !p.benches_on_429() {
            return None;
        }
        {
            let mut loaded = self.loaded.lock().await;
            if !loaded.contains(&p) {
                // Only a successful read counts as loaded: a store error is retried.
                if let Ok(hit) = deps.store.api_cache_get(&bench_key(p)).await {
                    loaded.insert(p);
                    if let Some(t) = hit.and_then(|(body, _, _)| {
                        std::str::from_utf8(&body)
                            .ok()
                            .and_then(|s| s.trim().parse::<i64>().ok())
                    }) {
                        let mut u = lock(&self.until);
                        let e = u.entry(p).or_insert(t);
                        *e = (*e).max(t);
                    }
                }
            }
        }
        lock(&self.until).get(&p).copied().filter(|t| *t > now)
    }

    /// Bench `p` until the next UTC midnight; returns that time.
    pub(crate) async fn bench(&self, p: Provider, deps: &SourceDeps, now: i64) -> i64 {
        let until = next_utc_midnight(now);
        lock(&self.until).insert(p, until);
        let ttl = u64::try_from(until - now).unwrap_or(DAY).max(1);
        // Best effort: a store failure only loses the bench across restarts.
        let _ = deps
            .store
            .api_cache_put(
                &bench_key(p),
                "scholar",
                until.to_string().as_bytes(),
                None,
                None,
                ttl,
            )
            .await;
        until
    }
}

/// Request context of one lookup batch.
#[derive(Clone)]
pub(crate) struct LookupCx {
    pub cancel: CancellationToken,
    pub timeout: Duration,
}

/// Issues lookups for one `ScholarCore`.
pub(crate) struct Http<'a> {
    pub reg: &'a Registry,
    pub deps: &'a SourceDeps,
    pub bench: &'a Bench,
    pub clock: &'a Clock,
}

fn api_key<'k>(deps: &'k SourceDeps, envs: &[&str]) -> Option<&'k str> {
    envs.iter()
        .find_map(|e| deps.cfg.api_keys.get(*e))
        .map(String::as_str)
        .filter(|k| !k.trim().is_empty())
}

/// Env vars carrying an OpenAlex key.
pub(crate) const OPENALEX_KEY_ENVS: [&str; 1] = ["SEARCH_MCP_OPENALEX_API_KEY"];
/// Env vars carrying a Semantic Scholar key (the config table's name first;
/// SPEC.md §6 calls it `SEARCH_MCP_S2_API_KEY`).
pub(crate) const S2_KEY_ENVS: [&str; 2] = [
    "SEARCH_MCP_SEMANTICSCHOLAR_API_KEY",
    "SEARCH_MCP_S2_API_KEY",
];

impl Http<'_> {
    fn now(&self) -> i64 {
        (self.clock)()
    }

    /// Send one GET (cache → bench → network) and classify the outcome.
    pub(crate) async fn fetch(&self, p: Provider, req: HttpRequest, lcx: &LookupCx) -> Fetched {
        let cache_key = request_url(&req).ok().map(|u| {
            format!(
                "scholar:v1:{}:{}",
                p.id(),
                &sha256_hex(u.as_str().as_bytes())[..32]
            )
        });
        if let Some(key) = &cache_key
            && let Ok(Some((body, _, _))) = self.deps.store.api_cache_get(key).await
        {
            return if body.as_ref() == b"null" {
                Fetched::NotFound
            } else {
                Fetched::Found(body)
            };
        }
        if let Some(t) = self.bench.until(p, self.deps, self.now()).await {
            return Fetched::Benched(t);
        }
        let req = req.for_source(p.id()).with_max_bytes(MAX_BYTES);
        let mut ncx = NetCx::new(lcx.cancel.child_token(), lcx.timeout);
        let (bucket, rps) = self.limits(p);
        ncx.host_bucket = Some(bucket);
        ncx.rps = Some(rps);
        let sent = tokio::time::timeout(
            lcx.timeout,
            self.deps.net.send(req, TransportProfile::Plain, &ncx),
        )
        .await;
        let outcome = match sent {
            Err(_) => Fetched::Failed(format!("timed out after {} ms", lcx.timeout.as_millis())),
            // The net layer returns an upstream 429 as a status; its
            // RateLimited error is a LOCAL wait (shared bucket or a
            // Retry-After penalty): not a reason to bench for the day.
            Ok(Err(e @ FsmError::RateLimited { .. })) => {
                Fetched::Failed(format!("local rate limit, not sent ({e})"))
            }
            Ok(Err(FsmError::Upstream {
                status: Some(429), ..
            })) => self.rate_limited(p).await,
            Ok(Err(FsmError::Upstream {
                status: Some(404 | 410),
                ..
            })) => Fetched::NotFound,
            Ok(Err(e)) => Fetched::Failed(e.to_string()),
            Ok(Ok(resp)) => match resp.status {
                429 => self.rate_limited(p).await,
                404 | 410 => Fetched::NotFound,
                s if (200..300).contains(&s) => {
                    if resp.truncated {
                        Fetched::Failed(format!("response truncated at {} bytes", MAX_BYTES))
                    } else if resp.body.iter().all(u8::is_ascii_whitespace) {
                        Fetched::Failed(format!("empty HTTP {s} body"))
                    } else if !well_formed(p, &resp.body) {
                        Fetched::Failed(format!(
                            "malformed {} payload (HTTP {s})",
                            if p.is_xml() { "XML" } else { "JSON" }
                        ))
                    } else {
                        Fetched::Found(resp.body)
                    }
                }
                s => Fetched::Failed(format!("HTTP {s}")),
            },
        };
        if let Some(key) = &cache_key {
            let put = match &outcome {
                Fetched::Found(b) => Some((b.as_ref(), p.cache_ttl_s())),
                Fetched::NotFound => Some((b"null".as_slice(), NOT_FOUND_TTL_S)),
                _ => None,
            };
            if let Some((body, ttl)) = put {
                let _ = self
                    .deps
                    .store
                    .api_cache_put(key, p.id(), body, None, None, ttl)
                    .await;
            }
        }
        outcome
    }

    /// The shared GCRA bucket and rate for `p`: the registered manifest's
    /// (as `ManifestSource` uses them), else the provider defaults.
    fn limits(&self, p: Provider) -> (String, f64) {
        let (host, rps) = p.default_limits();
        match self.reg.get(p.id()) {
            Some(src) => {
                let d = src.decl();
                let bucket = d.limits.host_bucket.clone().unwrap_or_else(|| d.id.clone());
                let rps = if d.limits.rps > 0.0 {
                    d.limits.rps
                } else {
                    rps
                };
                (bucket, rps)
            }
            None => (host.to_owned(), rps),
        }
    }

    async fn rate_limited(&self, p: Provider) -> Fetched {
        if p.benches_on_429() {
            let until = self.bench.bench(p, self.deps, self.now()).await;
            Fetched::Failed(format!(
                "rate limited (HTTP 429); benched until {}",
                rfc3339(until)
            ))
        } else {
            Fetched::Failed("rate limited (HTTP 429)".into())
        }
    }

    fn json(req: HttpRequest) -> HttpRequest {
        req.header("Accept", "application/json")
    }

    /// OpenAlex single-entity lookup (`$0`): DOI, arXiv (via its DataCite
    /// DOI), PMID, PMCID or W-id. `None` for ids OpenAlex cannot take.
    pub(crate) fn openalex_request(&self, pid: &Pid) -> Option<HttpRequest> {
        let path = match pid {
            Pid::Doi(d) => format!("https://doi.org/{}", enc(d)),
            Pid::Arxiv(a) => format!("https://doi.org/10.48550/arXiv.{}", enc(a)),
            Pid::Pmid(p) => format!("pmid:{p}"),
            Pid::Pmcid(p) => format!("pmcid:{p}"),
            Pid::OpenAlex(w) => w.clone(),
            _ => return None,
        };
        let mut req = Self::json(HttpRequest::get(format!(
            "https://api.openalex.org/works/{path}"
        )))
        .query_param("select", OPENALEX_SELECT);
        if let Some(k) = api_key(self.deps, &OPENALEX_KEY_ENVS) {
            req = req.query_param("api_key", k).redact_query_key("api_key");
        }
        Some(req)
    }

    /// Semantic Scholar single-paper lookup (`DOI:`, `ARXIV:`, `PMID:`,
    /// `PMCID:`, paperId, `CorpusId:`).
    pub(crate) fn s2_request(&self, pid: &Pid) -> Option<HttpRequest> {
        let id = match pid {
            Pid::Doi(d) => format!("DOI:{d}"),
            Pid::Arxiv(a) => format!("ARXIV:{a}"),
            Pid::Pmid(p) => format!("PMID:{p}"),
            Pid::Pmcid(p) => format!("PMCID:{}", p.trim_start_matches("PMC")),
            Pid::S2(s) => s.clone(),
            _ => return None,
        };
        let mut req = Self::json(HttpRequest::get(format!(
            "https://api.semanticscholar.org/graph/v1/paper/{}",
            enc(&id)
        )))
        .query_param("fields", S2_FIELDS);
        if let Some(k) = api_key(self.deps, &S2_KEY_ENVS) {
            req = req.header("x-api-key", k).redact_header("x-api-key");
        }
        Some(req)
    }

    fn polite(&self, mut req: HttpRequest) -> HttpRequest {
        if let Some(mail) = self
            .deps
            .cfg
            .contact_email
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
        {
            // Redacted: kept out of cache keys, logs and recorded fixtures.
            req = req
                .query_param("mailto", mail)
                .redact_query_key("mailto")
                .header("User-Agent", format!("free-search-mcp/1.0 (mailto:{mail})"))
                .redact_header("User-Agent");
        }
        req
    }

    /// Crossref `/works/{doi}` (polite pool when `CONTACT_EMAIL` is set).
    pub(crate) fn crossref_request(&self, doi: &str) -> HttpRequest {
        self.polite(Self::json(HttpRequest::get(format!(
            "https://api.crossref.org/works/{}",
            enc(doi)
        ))))
    }

    /// Crossref bibliographic title query for a published version.
    pub(crate) fn crossref_title_request(&self, title: &str, surname: Option<&str>) -> HttpRequest {
        let mut req = Self::json(HttpRequest::get("https://api.crossref.org/works"))
            .query_param("query.bibliographic", title)
            .query_param("rows", "5")
            .query_param(
                "filter",
                "type:journal-article,type:proceedings-article,type:book-chapter",
            )
            .query_param("select", CROSSREF_SELECT);
        if let Some(s) = surname {
            req = req.query_param("query.author", s);
        }
        self.polite(req)
    }

    /// arXiv Atom `id_list` lookup.
    pub(crate) fn arxiv_request(&self, id: &str) -> HttpRequest {
        HttpRequest::get("https://export.arxiv.org/api/query")
            .header("Accept", "application/atom+xml")
            .query_param("id_list", id)
            .query_param("max_results", "1")
    }

    /// PMC idconv for one DOI / PMID / PMCID.
    pub(crate) fn idconv_request(&self, id: &str) -> HttpRequest {
        let mut req = Self::json(HttpRequest::get(
            "https://pmc.ncbi.nlm.nih.gov/tools/idconv/api/v1/articles/",
        ))
        .query_param("ids", id)
        .query_param("format", "json")
        .query_param("tool", "free-search-mcp");
        if let Some(mail) = self
            .deps
            .cfg
            .contact_email
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
        {
            req = req.query_param("email", mail).redact_query_key("email");
        }
        req
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn midnight_is_the_next_utc_day_boundary() {
        // 2026-11-25T13:30:00Z -> 2026-11-26T00:00:00Z
        let now = 1_795_613_400;
        assert_eq!(next_utc_midnight(now), 1_795_651_200);
        assert_eq!(rfc3339(next_utc_midnight(now)), "2026-11-26T00:00:00Z");
        // Exactly at midnight: the NEXT one.
        assert_eq!(next_utc_midnight(1_795_651_200), 1_795_651_200 + 86_400);
    }

    #[test]
    fn path_encoding_keeps_doi_punctuation() {
        assert_eq!(
            enc("10.1016/s0140-6736(20)31180-6"),
            "10.1016/s0140-6736(20)31180-6"
        );
        assert_eq!(enc("10.1002/(sici)1097<x>"), "10.1002/(sici)1097%3Cx%3E");
        assert_eq!(enc("DOI:10.1/a b#c"), "DOI:10.1/a%20b%23c");
    }
}
