//! Replay fixture format (`SPEC-contracts.md` §4, M5, A1 §G.6) and the
//! offline [`ReplayNet`] client (A1 §B.9), re-exported by `fsm-net` as
//! `fsm_net::replay::ReplayNet`.
//!
//! Unlike the wire types, absent values serialize as explicit `null`
//! (the recorded files show `"bodySha256": null`, `"bodyB64": null`).
//!
//! Recording rules (A1 §G.6): `request.url` and `request.headersSubset` omit
//! every redacted credential query key and header; `bodySha256` is the
//! SHA-256 of the exact request body bytes (compact sorted-key JSON for
//! `json`/`graphql` bodies, raw query text for `sparql`). Use
//! [`FixtureRequest::from_http`] so recording and replay agree.
//!
//! Exchange counts (A1 §G.6.3): `opensearch` fixtures hold two exchanges,
//! `[0]` the OSDD XML and `[1]` the expanded feed (§E #3); an
//! `activitypub_fediverse` instance with an explicit `dialect.flavor` records
//! a single data exchange (§E #16).

use crate::error::FsmError;
use crate::net::{HttpBytes, HttpRequest, NetClient, NetCx};
use crate::source::TransportProfile;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use url::Url;

/// Current fixture format version.
pub const FIXTURE_VERSION: u32 = 1;

/// One recorded fixture file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[non_exhaustive]
pub struct Fixture {
    pub version: u32,
    pub source: String,
    pub op: String,
    /// RFC 3339 UTC.
    pub recorded_at: String,
    pub exchanges: Vec<Exchange>,
}

impl Default for Fixture {
    fn default() -> Self {
        Self {
            version: FIXTURE_VERSION,
            source: String::new(),
            op: String::new(),
            recorded_at: String::new(),
            exchanges: Vec::new(),
        }
    }
}

/// One request/response pair.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[non_exhaustive]
pub struct Exchange {
    pub request: FixtureRequest,
    pub response: FixtureResponse,
}

/// The matched part of a recorded request.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[non_exhaustive]
pub struct FixtureRequest {
    /// `GET` | `POST` | `HEAD`.
    pub method: String,
    pub url: String,
    /// Lower-cased names of the headers that matter for matching.
    #[serde(default)]
    pub headers_subset: BTreeMap<String, String>,
    /// Hex SHA-256 of the request body, when there was one.
    #[serde(default)]
    pub body_sha256: Option<String>,
}

/// A recorded response; exactly one of `body_text` / `body_b64` is set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[non_exhaustive]
pub struct FixtureResponse {
    pub status: u16,
    /// `[name, value]` pairs in received order.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub body_text: Option<String>,
    #[serde(default)]
    pub body_b64: Option<String>,
}

impl Default for FixtureResponse {
    fn default() -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body_text: Some(String::new()),
            body_b64: None,
        }
    }
}

impl FixtureRequest {
    pub fn new(method: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            method: method.into(),
            url: url.into(),
            ..Self::default()
        }
    }

    /// The recordable form of an outgoing request: full URL (with
    /// `req.query` merged) minus redacted query keys, the headers named in
    /// `keep_headers` minus redacted ones (names lower-cased), and the body hash.
    pub fn from_http(req: &HttpRequest, keep_headers: &[&str]) -> Self {
        let url = request_url(req).map_or_else(|_| req.url.clone(), |u| u.to_string());
        let headers_subset = req
            .headers
            .iter()
            .filter(|(k, _)| {
                keep_headers.iter().any(|h| h.eq_ignore_ascii_case(k)) && !req.is_redacted_header(k)
            })
            .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
            .collect();
        Self {
            method: req.method.as_str().to_owned(),
            url,
            headers_subset,
            body_sha256: req.body.as_ref().map(|b| sha256_hex(b)),
        }
    }
}

/// Compact JSON with every object's keys sorted recursively (A1 §B.7: the wire
/// body of `json`/`graphql` requests and the `bodySha256` input). Needed
/// because the workspace enables `serde_json/preserve_order`, so plain
/// `serde_json::to_string` keeps insertion order.
pub fn canonical_json(value: &serde_json::Value) -> String {
    fn sorted(v: &serde_json::Value) -> serde_json::Value {
        match v {
            serde_json::Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                serde_json::Value::Object(
                    keys.into_iter()
                        .map(|k| (k.clone(), sorted(&m[k])))
                        .collect(),
                )
            }
            serde_json::Value::Array(a) => serde_json::Value::Array(a.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    sorted(value).to_string()
}

/// Lower-case hex SHA-256 (the `bodySha256` encoding).
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// `req.url` with `req.query` appended and redacted query keys removed.
pub fn request_url(req: &HttpRequest) -> Result<Url, FsmError> {
    let mut url = Url::parse(&req.url)
        .map_err(|e| FsmError::invalid(format!("invalid URL {:?}: {e}", req.url)))?;
    if !req.query.is_empty() {
        let mut q = url.query_pairs_mut();
        for (k, v) in &req.query {
            q.append_pair(k, v);
        }
    }
    strip_query_keys(&mut url, |k| req.is_redacted_query_key(k));
    Ok(url)
}

fn strip_query_keys(url: &mut Url, drop: impl Fn(&str) -> bool) {
    if url.query().is_none() {
        return;
    }
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| !drop(k))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if kept.len() == url.query_pairs().count() {
        return;
    }
    if kept.is_empty() {
        url.set_query(None);
    } else {
        url.query_pairs_mut().clear().extend_pairs(kept);
    }
}

impl FixtureResponse {
    /// Store `body` as text when it is UTF-8, else as base64.
    pub fn from_bytes(status: u16, headers: Vec<(String, String)>, body: &[u8]) -> Self {
        match std::str::from_utf8(body) {
            Ok(text) => Self {
                status,
                headers,
                body_text: Some(text.to_owned()),
                body_b64: None,
            },
            Err(_) => Self {
                status,
                headers,
                body_text: None,
                body_b64: Some(B64.encode(body)),
            },
        }
    }

    /// The body bytes.
    pub fn body_bytes(&self) -> Result<Vec<u8>, FsmError> {
        match (&self.body_text, &self.body_b64) {
            (Some(t), None) => Ok(t.as_bytes().to_vec()),
            (None, Some(b)) => B64
                .decode(b)
                .map_err(|e| FsmError::invalid(format!("fixture bodyB64: {e}"))),
            _ => Err(FsmError::invalid(
                "fixture response must set exactly one of bodyText / bodyB64",
            )),
        }
    }

    /// First header value with this name (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

impl Exchange {
    pub fn new(request: FixtureRequest, response: FixtureResponse) -> Self {
        Self { request, response }
    }
}

impl Fixture {
    pub fn new(
        source: impl Into<String>,
        op: impl Into<String>,
        recorded_at: impl Into<String>,
    ) -> Self {
        Self {
            source: source.into(),
            op: op.into(),
            recorded_at: recorded_at.into(),
            ..Self::default()
        }
    }

    /// Parse and validate a fixture file.
    pub fn from_json(text: &str) -> Result<Self, FsmError> {
        let f: Self = serde_json::from_str(text)
            .map_err(|e| FsmError::invalid(format!("fixture JSON: {e}")))?;
        f.validate()?;
        Ok(f)
    }

    /// Pretty JSON with a trailing newline (the on-disk form).
    pub fn to_json(&self) -> String {
        let mut s = serde_json::to_string_pretty(self).unwrap_or_default();
        s.push('\n');
        s
    }

    /// Version 1, at least one exchange, exactly one body per response, a valid base64 body.
    pub fn validate(&self) -> Result<(), FsmError> {
        if self.version != FIXTURE_VERSION {
            return Err(FsmError::invalid(format!(
                "fixture version {} (expected {FIXTURE_VERSION})",
                self.version
            )));
        }
        if self.exchanges.is_empty() {
            return Err(FsmError::invalid("fixture has no exchanges"));
        }
        for (i, x) in self.exchanges.iter().enumerate() {
            x.response
                .body_bytes()
                .map_err(|e| FsmError::invalid(format!("exchange {i}: {e}")))?;
            if x.request.url.is_empty() || x.request.method.is_empty() {
                return Err(FsmError::invalid(format!(
                    "exchange {i}: request needs method and url"
                )));
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// ReplayNet
// ---------------------------------------------------------------------------

/// One served request, for assertions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReplayHit {
    pub method: String,
    /// The request URL as matched (query merged, redacted keys removed).
    pub url: String,
    /// `<source>/<op>` of the fixture that answered.
    pub fixture: String,
    /// Index of the exchange inside that fixture.
    pub exchange: usize,
}

struct Entry {
    fixture: String,
    exchange: usize,
    method: String,
    url: Url,
    query: Vec<(String, String)>,
    headers: BTreeMap<String, String>,
    body_sha256: Option<String>,
    response: FixtureResponse,
}

#[derive(Default)]
struct ReplayState {
    served: Vec<usize>,
    hits: Vec<ReplayHit>,
    unmatched: Vec<String>,
}

/// Offline [`NetClient`] that answers only from recorded fixtures (§4, A1 §B.9).
///
/// Matching: method (case-insensitive); scheme, host, port and path exactly;
/// query pairs as an order-insensitive multiset (the request's `url` and
/// `query` merged); every `headersSubset` entry present in the request
/// (header names case-insensitive, values exact); `bodySha256`, when
/// recorded, equal to the SHA-256 of the request body. Query keys and headers
/// listed in `HttpRequest::redact_*` are ignored on both sides. When several
/// exchanges match, the first not yet served wins (so multi-exchange fixtures
/// are served in order); once all are served the last one repeats. An
/// unmatched request fails with `FsmError::Internal { kind: "replay_unmatched" }`
/// naming the request and the closest recorded exchange, and is listed by
/// [`unmatched`](Self::unmatched). A `max_bytes` cap truncates the body and
/// sets `HttpBytes::truncated`.
pub struct ReplayNet {
    entries: Vec<Entry>,
    state: Mutex<ReplayState>,
}

impl std::fmt::Debug for ReplayNet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplayNet")
            .field("exchanges", &self.entries.len())
            .finish_non_exhaustive()
    }
}

fn sorted_pairs(url: &Url) -> Vec<(String, String)> {
    let mut q: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    q.sort();
    q
}

impl ReplayNet {
    /// Serve these fixtures (validated; load order = match priority).
    pub fn from_fixtures(fixtures: Vec<Fixture>) -> Result<Self, FsmError> {
        let mut entries = Vec::new();
        for f in fixtures {
            f.validate()?;
            let label = format!("{}/{}", f.source, f.op);
            for (i, x) in f.exchanges.into_iter().enumerate() {
                let url = Url::parse(&x.request.url).map_err(|e| {
                    FsmError::invalid(format!(
                        "fixture {label} exchange {i}: invalid URL {:?}: {e}",
                        x.request.url
                    ))
                })?;
                entries.push(Entry {
                    fixture: label.clone(),
                    exchange: i,
                    method: x.request.method.to_ascii_uppercase(),
                    query: sorted_pairs(&url),
                    url,
                    headers: x
                        .request
                        .headers_subset
                        .into_iter()
                        .map(|(k, v)| (k.to_ascii_lowercase(), v))
                        .collect(),
                    body_sha256: x.request.body_sha256.map(|s| s.to_ascii_lowercase()),
                    response: x.response,
                });
            }
        }
        let served = vec![0; entries.len()];
        Ok(Self {
            entries,
            state: Mutex::new(ReplayState {
                served,
                ..ReplayState::default()
            }),
        })
    }

    /// Serve one fixture file.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, FsmError> {
        Self::from_fixtures(vec![read_fixture(path.as_ref())?])
    }

    /// Serve every `*.json` fixture under `dir` (recursively, in path order).
    pub fn from_dir(dir: impl AsRef<Path>) -> Result<Self, FsmError> {
        let dir = dir.as_ref();
        let mut files = Vec::new();
        collect_json(dir, &mut files)?;
        if files.is_empty() {
            return Err(FsmError::invalid(format!(
                "no *.json fixtures under {}",
                dir.display()
            )));
        }
        files.sort();
        Self::from_fixtures(
            files
                .iter()
                .map(|p| read_fixture(p))
                .collect::<Result<_, _>>()?,
        )
    }

    /// Number of recorded exchanges.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Requests served so far, in order.
    pub fn hits(&self) -> Vec<ReplayHit> {
        self.lock().hits.clone()
    }

    /// Requests that matched nothing (`"METHOD url"`), in order.
    pub fn unmatched(&self) -> Vec<String> {
        self.lock().unmatched.clone()
    }

    /// Recorded exchanges never served (`"<source>/<op>#<i>"`).
    pub fn unused(&self) -> Vec<String> {
        let st = self.lock();
        self.entries
            .iter()
            .zip(&st.served)
            .filter(|(_, n)| **n == 0)
            .map(|(e, _)| format!("{}#{}", e.fixture, e.exchange))
            .collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ReplayState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Differences between a request and an entry (empty = match).
    fn diff(
        &self,
        e: &Entry,
        req: &HttpRequest,
        url: &Url,
        query: &[(String, String)],
        body: &Option<String>,
    ) -> Vec<String> {
        let mut d = Vec::new();
        if !e.method.eq_ignore_ascii_case(req.method.as_str()) {
            d.push(format!(
                "method {} vs recorded {}",
                req.method.as_str(),
                e.method
            ));
        }
        if e.url.scheme() != url.scheme()
            || e.url.host_str() != url.host_str()
            || e.url.port_or_known_default() != url.port_or_known_default()
        {
            d.push(format!(
                "origin {} vs recorded {}",
                url.origin().ascii_serialization(),
                e.url.origin().ascii_serialization()
            ));
        }
        if e.url.path() != url.path() {
            d.push(format!("path {} vs recorded {}", url.path(), e.url.path()));
        }
        let recorded: Vec<&(String, String)> = e
            .query
            .iter()
            .filter(|(k, _)| !req.is_redacted_query_key(k))
            .collect();
        // Multiset difference: both sides are sorted, duplicates count.
        let (mut missing, mut extra) = (Vec::new(), Vec::new());
        let (mut i, mut j) = (0, 0);
        while i < recorded.len() || j < query.len() {
            match (recorded.get(i), query.get(j)) {
                (Some(r), Some(q)) if *r == q => {
                    i += 1;
                    j += 1;
                }
                (Some(r), Some(q)) if *r < q => {
                    missing.push(format!("{}={}", r.0, r.1));
                    i += 1;
                }
                (Some(r), None) => {
                    missing.push(format!("{}={}", r.0, r.1));
                    i += 1;
                }
                (_, Some(q)) => {
                    extra.push(format!("{}={}", q.0, q.1));
                    j += 1;
                }
                (None, None) => break,
            }
        }
        if !missing.is_empty() || !extra.is_empty() {
            d.push(format!(
                "query missing [{}] unexpected [{}]",
                missing.join("&"),
                extra.join("&")
            ));
        }
        for (name, want) in &e.headers {
            if req.is_redacted_header(name) {
                continue;
            }
            match req
                .headers
                .iter()
                .rfind(|(k, _)| k.eq_ignore_ascii_case(name))
            {
                Some((_, v)) if v == want => {}
                Some((_, v)) => d.push(format!("header {name}: {v:?} vs recorded {want:?}")),
                None => d.push(format!("header {name} missing (recorded {want:?})")),
            }
        }
        if let Some(want) = &e.body_sha256
            && body.as_deref() != Some(want.as_str())
        {
            d.push(format!(
                "body sha256 {} vs recorded {want}",
                body.as_deref().unwrap_or("<none>")
            ));
        }
        d
    }

    fn serve(&self, req: &HttpRequest) -> Result<HttpBytes, FsmError> {
        let url = request_url(req)?;
        let query: Vec<(String, String)> = sorted_pairs(&url);
        let body = Some(sha256_hex(req.body.as_deref().unwrap_or_default()));
        let diffs: Vec<Vec<String>> = self
            .entries
            .iter()
            .map(|e| self.diff(e, req, &url, &query, &body))
            .collect();
        let mut st = self.lock();
        let matching: Vec<usize> = (0..self.entries.len())
            .filter(|i| diffs[*i].is_empty())
            .collect();
        let Some(&idx) = matching
            .iter()
            .find(|i| st.served[**i] == 0)
            .or(matching.last())
        else {
            let what = format!("{} {url}", req.method.as_str());
            st.unmatched.push(what.clone());
            let closest = (0..self.entries.len()).min_by_key(|i| (diffs[*i].len(), *i));
            let hint = closest.map_or_else(
                || "no fixtures are loaded".to_owned(),
                |i| {
                    let e = &self.entries[i];
                    format!(
                        "closest: {}#{} {} {} — differs in: {}",
                        e.fixture,
                        e.exchange,
                        e.method,
                        e.url,
                        diffs[i].join("; ")
                    )
                },
            );
            return Err(FsmError::Internal {
                kind: "replay_unmatched",
                message: format!("ReplayNet: no recorded exchange for {what}; {hint}"),
            });
        };
        st.served[idx] += 1;
        let e = &self.entries[idx];
        st.hits.push(ReplayHit {
            method: e.method.clone(),
            url: url.to_string(),
            fixture: e.fixture.clone(),
            exchange: e.exchange,
        });
        drop(st);
        let mut bytes = e.response.body_bytes()?;
        let mut truncated = false;
        if let Some(max) = req.max_bytes
            && bytes.len() as u64 > max
        {
            bytes.truncate(usize::try_from(max).unwrap_or(usize::MAX));
            truncated = true;
        }
        let mut out = HttpBytes::new(
            url.to_string(),
            e.response.status,
            e.response.headers.clone(),
            bytes,
        );
        out.truncated = truncated;
        Ok(out)
    }
}

fn read_fixture(path: &Path) -> Result<Fixture, FsmError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| FsmError::invalid(format!("cannot read fixture {}: {e}", path.display())))?;
    Fixture::from_json(&text).map_err(|e| FsmError::invalid(format!("{}: {e}", path.display())))
}

fn collect_json(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), FsmError> {
    let rd = std::fs::read_dir(dir)
        .map_err(|e| FsmError::invalid(format!("cannot read {}: {e}", dir.display())))?;
    for entry in rd {
        let path = entry
            .map_err(|e| FsmError::invalid(format!("cannot read {}: {e}", dir.display())))?
            .path();
        if path.is_dir() {
            collect_json(&path, out)?;
        } else if path.extension().is_some_and(|x| x == "json") {
            out.push(path);
        }
    }
    Ok(())
}

#[async_trait::async_trait]
impl NetClient for ReplayNet {
    async fn send(
        &self,
        req: HttpRequest,
        _profile: TransportProfile,
        cx: &NetCx,
    ) -> Result<HttpBytes, FsmError> {
        // No FsmError::Cancelled exists; a cancelled call ends as Timeout(0).
        if cx.cancel.is_cancelled() {
            return Err(FsmError::Timeout(0));
        }
        self.serve(&req)
    }

    /// Replay performs no SSRF policy of its own: any http(s) URL is accepted.
    fn check_url_offline(&self, url: &str) -> Result<(), FsmError> {
        match Url::parse(url) {
            Ok(u) if matches!(u.scheme(), "http" | "https") => Ok(()),
            _ => Err(FsmError::UnsafeUrl(url.to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC_EXAMPLE: &str = r#"{
      "version": 1,
      "source": "arxiv",
      "op": "work",
      "recordedAt": "2026-11-25T10:00:00Z",
      "exchanges": [
        {
          "request": {
            "method": "GET",
            "url": "https://export.arxiv.org/api/query?id_list=1706.03762",
            "headersSubset": {"accept": "application/atom+xml"},
            "bodySha256": null
          },
          "response": {
            "status": 200,
            "headers": [["content-type", "application/atom+xml; charset=UTF-8"]],
            "bodyText": "<?xml version=\"1.0\"?>...",
            "bodyB64": null
          }
        }
      ]
    }"#;

    #[test]
    fn spec_example_parses_and_round_trips() {
        let f = Fixture::from_json(SPEC_EXAMPLE).unwrap();
        assert_eq!((f.source.as_str(), f.op.as_str()), ("arxiv", "work"));
        let r = &f.exchanges[0].response;
        assert_eq!(
            r.header("Content-Type"),
            Some("application/atom+xml; charset=UTF-8")
        );
        assert!(r.body_bytes().unwrap().starts_with(b"<?xml"));
        let text = f.to_json();
        assert!(text.contains("\"bodyB64\": null") && text.contains("\"bodySha256\": null"));
        assert_eq!(Fixture::from_json(&text).unwrap(), f);
    }

    #[test]
    fn exactly_one_body() {
        let bin = FixtureResponse::from_bytes(200, vec![], &[0xff, 0x00, 0x10]);
        assert!(bin.body_text.is_none());
        assert_eq!(bin.body_bytes().unwrap(), vec![0xff, 0x00, 0x10]);
        let txt = FixtureResponse::from_bytes(200, vec![], "héllo".as_bytes());
        assert_eq!(txt.body_text.as_deref(), Some("héllo"));
        let both = SPEC_EXAMPLE.replace("\"bodyB64\": null", "\"bodyB64\": \"AA==\"");
        assert!(Fixture::from_json(&both).is_err());
        let neither = SPEC_EXAMPLE.replace(
            "\"bodyText\": \"<?xml version=\\\"1.0\\\"?>...\"",
            "\"bodyText\": null",
        );
        assert!(Fixture::from_json(&neither).is_err(), "{neither}");
        assert!(
            Fixture::from_json(&SPEC_EXAMPLE.replace("\"version\": 1", "\"version\": 2")).is_err()
        );
        assert!(
            Fixture::from_json(
                &SPEC_EXAMPLE.replace("\"op\": \"work\"", "\"op\": \"work\", \"extra\": 1")
            )
            .is_err()
        );
    }
}
