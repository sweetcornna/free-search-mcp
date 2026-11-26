//! Microsoft Bing HTML SERP engine (`www4.bing.com/search`).
//!
//! Port of `src/search_mcp/engines/bing.py`, preserving:
//! - `www4.bing.com` warm-up cookie jar (per-egress, single-flight, 30 min TTL,
//!   60 s failed-warm-up TTL, non-fatal warm-up);
//! - `form=QBRE` and NEVER `count=`;
//! - `ck/a` base64url click-redirect unwrapping;
//! - decoy-page detection + generation-guarded single re-mint + retry once;
//! - second-page pagination (`&first=11`) when `limit > 10` and not a decoy.

use crate::coherence::looks_like_decoy;
use crate::common::{
    CallDeadline, SerpQuery, augment_query_with_operators, bing_safesearch, check_http_status,
    check_wall_if_empty, decode_body, extract_date_hint_now, finalize_hits, make_hit,
    net_cx_with_deadline, quote_plus, region_to_bing_market, search_op_decl, text_of,
};
use crate::cookie_jar::{CookieJar, egress_key, get_with_jar_deadline};
use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use dom_query::Document;
use fsm_core::api::Freshness;
use fsm_core::config::Config;
use fsm_core::source::{
    AuthMode, Capability, PoolRole, Source, SourceDecl, SourceLimits, SrcCx, SrcOutput,
    TransportProfile,
};
use fsm_core::types::{Hit, HitKind};
use fsm_core::{FsmError, SourceDeps};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use url::Url;

pub const WARM_URL: &str = "https://www4.bing.com/";
pub const SEARCH_ENDPOINT: &str = "https://www4.bing.com/search";
pub const JAR_TTL_SECS: f64 = 1800.0;
pub const WARM_FAIL_TTL_SECS: f64 = 60.0;

pub const GATE_MARKERS: &[(&str, &[&str])] = &[];

/// Unwrap a `bing.com/ck/a?...&u=a1<base64url>` click-tracking redirect.
///
/// Only `ck/a` URLs are modified and only valid UTF-8 `http(s)://` targets are
/// accepted; anything else is returned unchanged.
pub fn resolve_bing_url(raw_url: &str) -> String {
    if !raw_url.contains("bing.com/ck/a") {
        return raw_url.to_owned();
    }
    let Ok(parsed) = Url::parse(raw_url) else {
        return raw_url.to_owned();
    };
    let Some((_, encoded)) = parsed.query_pairs().find(|(k, _)| k == "u") else {
        return raw_url.to_owned();
    };
    if encoded.len() < 3 {
        return raw_url.to_owned();
    }
    let payload = encoded[2..].replace('-', "+").replace('_', "/");
    let pad = (4 - (payload.len() % 4)) % 4;
    let padded = format!("{payload}{}", "=".repeat(pad));
    let Ok(bytes) = B64.decode(padded) else {
        return raw_url.to_owned();
    };
    let Ok(decoded) = String::from_utf8(bytes) else {
        return raw_url.to_owned();
    };
    if decoded.starts_with("http://") || decoded.starts_with("https://") {
        decoded
    } else {
        raw_url.to_owned()
    }
}

fn freshness_token(f: Freshness) -> &'static str {
    match f {
        Freshness::Day => r#"ex1:"ez1""#,
        Freshness::Week => r#"ex1:"ez2""#,
        Freshness::Month => r#"ex1:"ez3""#,
        Freshness::Year => r#"ex1:"ez4""#,
        _ => r#"ex1:"ez2""#,
    }
}

/// Build the Bing search URL (`form=QBRE`, never `count=`).
pub fn build_url(q: &SerpQuery) -> String {
    let augmented = augment_query_with_operators(
        &q.query,
        &q.include_domains,
        &q.exclude_domains,
        q.filetype.as_deref(),
    );
    let mut url = format!("{SEARCH_ENDPOINT}?q={}&form=QBRE", quote_plus(&augmented));
    if let Some(f) = q.freshness {
        url.push_str("&filters=");
        url.push_str(&quote_plus(freshness_token(f)));
    }
    url.push_str("&adlt=");
    url.push_str(bing_safesearch(q.safesearch));
    if !q.region.is_empty() {
        url.push_str("&mkt=");
        url.push_str(&quote_plus(&region_to_bing_market(&q.region)));
    }
    url
}

/// Parse Bing SERP HTML into `Hit`s.
pub fn parse(html: &str) -> Vec<Hit> {
    if html.is_empty() {
        return Vec::new();
    }
    let doc = Document::from(html);
    let mut results = Vec::new();
    let mut seen = HashSet::new();

    for li in doc.select("li.b_algo").iter() {
        let link = li.select("h2 a").first();
        if !link.exists() {
            continue;
        }
        let raw_href = link.attr("href").map(|s| s.to_string()).unwrap_or_default();
        let url = resolve_bing_url(&raw_href);
        let title = text_of(&link);
        let snippet_sel = [
            ".b_caption p",
            ".b_lineclamp4",
            ".b_lineclamp2",
            ".b_paractl",
        ]
        .iter()
        .find_map(|s| {
            let sel = li.select(s).first();
            sel.exists().then_some(sel)
        });
        let snippet = snippet_sel.as_ref().map(text_of).unwrap_or_default();
        if url.is_empty() || title.is_empty() || !seen.insert(url.clone()) {
            continue;
        }
        let date = extract_date_hint_now(&snippet).or_else(|| extract_date_hint_now(&title));
        results.push(make_hit(
            "bing",
            HitKind::Web,
            title,
            url,
            snippet,
            date,
            Some("snippet"),
        ));
    }
    results
}

#[derive(Debug, Clone)]
struct CachedJar {
    cookies: CookieJar,
    minted_at: f64,
    generation: u64,
}

impl CachedJar {
    fn is_fresh(&self, now: f64) -> bool {
        let ttl = if self.cookies.is_empty() {
            WARM_FAIL_TTL_SECS
        } else {
            JAR_TTL_SECS
        };
        (now - self.minted_at) < ttl
    }
}

#[derive(Default)]
struct SessionState {
    jars: HashMap<String, CachedJar>,
    minting: HashMap<String, broadcast::Sender<Option<CachedJar>>>,
    generation: u64,
}

pub type ClockFn = Arc<dyn Fn() -> f64 + Send + Sync>;
pub type EgressFn = Arc<dyn Fn(&Config) -> String + Send + Sync>;

pub struct BingEngine {
    decl: SourceDecl,
    deps: SourceDeps,
    sessions: Arc<Mutex<SessionState>>,
    clock: ClockFn,
    egress_fn: EgressFn,
}

impl BingEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let epoch = Instant::now();
        let clock: ClockFn = Arc::new(move || epoch.elapsed().as_secs_f64());
        let egress_fn: EgressFn = Arc::new(|cfg| egress_key(cfg, "bing"));
        Self::with_hooks(deps, clock, egress_fn)
    }

    /// Construct with injectable monotonic clock and egress resolver (for tests).
    pub fn with_hooks(deps: SourceDeps, clock: ClockFn, egress_fn: EgressFn) -> Self {
        let mut decl = SourceDecl::new(
            "bing",
            "Microsoft Bing",
            "Microsoft Bing web results over plain HTTP: broad index, no browser needed.",
        )
        .with_scopes(&["web"])
        .with_caps(&[Capability::Search])
        .with_op(search_op_decl(
            "Search the web via Microsoft Bing.",
            "rust ownership borrowing",
            "bing",
        ));
        decl.home_url = Some("https://www.bing.com".into());
        decl.tier = 2;
        decl.auth = AuthMode::None;
        decl.transport = TransportProfile::EdgePreflight;
        decl.pool_role = PoolRole::Default;
        decl.limits = SourceLimits::new(1.0, 3, 2, 5_000, 21_600);
        Self {
            decl,
            deps,
            sessions: Arc::new(Mutex::new(SessionState::default())),
            clock,
            egress_fn,
        }
    }

    /// Clear all cached cookie jars (for tests).
    pub fn reset_jars(&self) {
        let mut st = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        st.jars.clear();
        st.minting.clear();
        st.generation = 0;
    }

    /// Pre-warm the Bing cookie jar for the current egress.
    pub async fn warm(&self, cx: &SrcCx) {
        let accept_lang = if cx.accept_language.is_empty() {
            self.deps.cfg.accept_language.as_str()
        } else {
            cx.accept_language.as_str()
        };
        let timeout = if cx.timeout.is_zero() {
            std::time::Duration::from_secs_f64(self.deps.cfg.request_timeout_s.max(0.5))
        } else {
            cx.timeout
        };
        let deadline = CallDeadline::from_timeout(timeout);
        let key = (self.egress_fn)(&self.deps.cfg);
        let _ = self.session(&key, accept_lang, &cx.cancel, deadline).await;
    }

    async fn warm_cookies(
        &self,
        accept_language: &str,
        cancel: &CancellationToken,
        deadline: CallDeadline,
    ) -> Result<CookieJar, FsmError> {
        if cancel.is_cancelled() {
            return Err(FsmError::Timeout(0));
        }
        let aux_ncx = net_cx_with_deadline(cancel, deadline, &self.decl.id, None)?;
        let mut jar = CookieJar::new();
        match get_with_jar_deadline(
            self.deps.net.as_ref(),
            &self.decl.id,
            self.decl.transport,
            WARM_URL,
            accept_language,
            &mut jar,
            &aux_ncx,
            deadline,
        )
        .await
        {
            Ok(_) => Ok(jar),
            Err(_) if cancel.is_cancelled() => Err(FsmError::Timeout(0)),
            Err(e) => {
                tracing::debug!(error = %e, "bing session warm-up failed");
                Ok(CookieJar::new())
            }
        }
    }

    async fn session(
        &self,
        key: &str,
        accept_language: &str,
        cancel: &CancellationToken,
        deadline: CallDeadline,
    ) -> Result<CachedJar, FsmError> {
        loop {
            if cancel.is_cancelled() {
                return Err(FsmError::Timeout(0));
            }
            enum Action {
                Fresh(CachedJar),
                Wait(broadcast::Receiver<Option<CachedJar>>),
                Mint(broadcast::Sender<Option<CachedJar>>),
            }

            let now = (self.clock)();
            let action = {
                let mut st = self
                    .sessions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(jar) = st.jars.get(key)
                    && jar.is_fresh(now)
                {
                    Action::Fresh(jar.clone())
                } else if let Some(tx) = st.minting.get(key) {
                    Action::Wait(tx.subscribe())
                } else {
                    let (tx, _) = broadcast::channel(1);
                    st.minting.insert(key.to_owned(), tx.clone());
                    Action::Mint(tx)
                }
            };

            match action {
                Action::Fresh(jar) => return Ok(jar),
                Action::Wait(mut rx) => match rx.recv().await {
                    Ok(Some(jar)) => return Ok(jar),
                    Ok(None) | Err(_) => {
                        if cancel.is_cancelled() {
                            return Err(FsmError::Timeout(0));
                        }
                        continue;
                    }
                },
                Action::Mint(tx) => {
                    struct MintGuard {
                        sessions: Arc<Mutex<SessionState>>,
                        key: String,
                        tx: broadcast::Sender<Option<CachedJar>>,
                        settled: bool,
                    }
                    impl Drop for MintGuard {
                        fn drop(&mut self) {
                            {
                                let mut st = self
                                    .sessions
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                st.minting.remove(&self.key);
                            }
                            if !self.settled {
                                let _ = self.tx.send(None);
                            }
                        }
                    }

                    let mut guard = MintGuard {
                        sessions: Arc::clone(&self.sessions),
                        key: key.to_owned(),
                        tx: tx.clone(),
                        settled: false,
                    };

                    let cookies = self.warm_cookies(accept_language, cancel, deadline).await?;
                    if cancel.is_cancelled() {
                        // Never cache a cancelled warm-up as a failed warm-up (M1).
                        return Err(FsmError::Timeout(0));
                    }
                    let jar = {
                        let mut st = self
                            .sessions
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        st.generation += 1;
                        let jar = CachedJar {
                            cookies,
                            minted_at: (self.clock)(),
                            generation: st.generation,
                        };
                        st.jars.insert(key.to_owned(), jar.clone());
                        jar
                    };
                    guard.settled = true;
                    let _ = tx.send(Some(jar.clone()));
                    drop(guard);
                    return Ok(jar);
                }
            }
        }
    }

    fn invalidate(&self, key: &str, generation: u64) {
        let mut st = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if st.jars.get(key).is_some_and(|j| j.generation == generation) {
            st.jars.remove(key);
        }
    }

    async fn fetch_serp_page(
        &self,
        key: &str,
        url: &str,
        accept_language: &str,
        cancel: &CancellationToken,
        deadline: CallDeadline,
        charge_rps: Option<f64>,
    ) -> Result<(u64, u16, String), FsmError> {
        let jar = self.session(key, accept_language, cancel, deadline).await?;
        let mut req_jar = jar.cookies.clone();
        let req_ncx = net_cx_with_deadline(cancel, deadline, &self.decl.id, charge_rps)?;
        let resp = get_with_jar_deadline(
            self.deps.net.as_ref(),
            &self.decl.id,
            self.decl.transport,
            url,
            accept_language,
            &mut req_jar,
            &req_ncx,
            deadline,
        )
        .await?;
        check_http_status(&self.decl.id, &resp)?;
        Ok((jar.generation, resp.status, decode_body(&resp)))
    }
}

#[async_trait]
impl Source for BingEngine {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    async fn call(&self, op: &str, params: Value, cx: &SrcCx) -> Result<SrcOutput, FsmError> {
        let q = SerpQuery::from_call(&self.decl.id, op, &params, cx, &self.deps.cfg)?;
        let url = build_url(&q);
        let deadline = CallDeadline::from_timeout(q.timeout);
        let key = (self.egress_fn)(&self.deps.cfg);

        // Only the primary SERP page 1 charges the engine's rate limiter;
        // warm-up, decoy re-mint/retry, and page 2 run as auxiliary hops (B1).
        let (mut served_gen, mut status, mut html) = self
            .fetch_serp_page(
                &key,
                &url,
                &q.accept_language,
                &cx.cancel,
                deadline,
                Some(self.decl.limits.rps),
            )
            .await?;
        let mut results = parse(&html);

        if results.is_empty() {
            let checked = check_wall_if_empty(&self.decl.id, status, &html, GATE_MARKERS, results)?;
            return Ok(SrcOutput::Hits(checked));
        }

        let mut is_decoy = looks_like_decoy(&q.query, &results);
        if is_decoy {
            self.invalidate(&key, served_gen);
            tracing::info!(query = %q.query, "bing returned a decoy page; re-minting session");
            let retry = self
                .fetch_serp_page(&key, &url, &q.accept_language, &cx.cancel, deadline, None)
                .await?;
            served_gen = retry.0;
            status = retry.1;
            html = retry.2;
            let _ = served_gen;
            results = parse(&html);
            if results.is_empty() {
                let checked =
                    check_wall_if_empty(&self.decl.id, status, &html, GATE_MARKERS, results)?;
                return Ok(SrcOutput::Hits(checked));
            }
            is_decoy = looks_like_decoy(&q.query, &results);
        }

        if q.limit > 10 && !results.is_empty() && !is_decoy {
            let page2_url = format!("{url}&first=11");
            if let Ok((_, _, html2)) = self
                .fetch_serp_page(
                    &key,
                    &page2_url,
                    &q.accept_language,
                    &cx.cancel,
                    deadline,
                    None,
                )
                .await
            {
                let more = parse(&html2);
                let mut seen: HashSet<String> = results.iter().map(|h| h.url.clone()).collect();
                for hit in more {
                    if seen.insert(hit.url.clone()) {
                        results.push(hit);
                    }
                }
            } else {
                tracing::debug!("bing second page failed; returning page one only");
            }
        }

        let hits = finalize_hits(
            results,
            &q.include_domains,
            &q.exclude_domains,
            q.filetype.as_deref(),
            q.limit,
        );
        Ok(SrcOutput::Hits(hits))
    }
}
