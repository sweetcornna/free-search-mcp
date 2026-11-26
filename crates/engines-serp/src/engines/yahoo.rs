//! Yahoo Search HTML SERP engine (`search.yahoo.com/search`).
//!
//! Port of `src/search_mcp/engines/yahoo.py`, preserving:
//! - `_bv` / `YBV` cookie handshake jar (per-egress, 12 h TTL, 3 attempts cold /
//!   2 attempts re-mint, single-flight coalescing with cancel/abandoned handling);
//! - `r.search.yahoo.com` AND `rd.search.yahoo.com` `/RU=` unwrapping via
//!   `fsm_core::url_canon::resolve_yahoo_url` (decoded exactly once, Spike N);
//! - ad click filtering (`/rdclks/`, `/cbclk`, `bing.com/aclick`) and internal
//!   Yahoo host filtering (`search.yahoo.com`, `guce.yahoo.com`, `scout.yahoo.com`, ...);
//! - `btf=d|w|m` (omitted for `year`);
//! - EU consent gate marker.

use crate::common::{
    CallDeadline, SerpQuery, augment_query_with_operators, check_wall_if_empty, decode_body,
    extract_date_hint_now, finalize_hits, inline_text, make_hit, net_cx_with_deadline,
    parse_retry_after, quote_plus, search_op_decl,
};
use crate::cookie_jar::{CookieJar, egress_key, get_with_jar_deadline};
use async_trait::async_trait;
use dom_query::Document;
use fsm_core::api::Freshness;
use fsm_core::config::Config;
use fsm_core::net::HttpBytes;
use fsm_core::source::{
    AuthMode, Capability, PoolRole, Source, SourceDecl, SourceLimits, SrcCx, SrcOutput,
    TransportProfile,
};
use fsm_core::types::{Hit, HitKind};
use fsm_core::url_canon::{host_matches, resolve_yahoo_url};
use fsm_core::{FsmError, SourceDeps};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use url::Url;

pub const ENDPOINT: &str = "https://search.yahoo.com/search";
pub const JAR_TTL_SECS: f64 = 12.0 * 3600.0;
pub const HANDSHAKE_ATTEMPTS: usize = 3;
pub const REMINT_ATTEMPTS: usize = 2;

const AD_CLICK_MARKERS: &[&str] = &["/rdclks/", "/cbclk"];

const INTERNAL_HOSTS: &[&str] = &[
    "search.yahoo.com",
    "scout.yahoo.com",
    "shopping.yahoo.com",
    "guce.yahoo.com",
    "consent.yahoo.com",
    "login.yahoo.com",
    "legal.yahoo.com",
    "policies.yahoo.com",
];

pub const GATE_MARKERS: &[(&str, &[&str])] = &[(
    "consent",
    &["consent.yahoo.com/v2/collectconsent?sessionid="],
)];

/// True when `url` is a Yahoo internal page or a Bing Ads click redirect.
pub fn is_dropped(url: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return true;
    };
    let host = parsed.host_str().unwrap_or("").to_ascii_lowercase();
    if host.is_empty() || host_matches(&host, INTERNAL_HOSTS) {
        return true;
    }
    host_matches(&host, &["bing.com"]) && parsed.path().starts_with("/aclick")
}

fn freshness_token(f: Freshness) -> Option<&'static str> {
    match f {
        Freshness::Day => Some("d"),
        Freshness::Week => Some("w"),
        Freshness::Month => Some("m"),
        // `btf=y` is silently ignored by Yahoo; leave year to post-filtering.
        Freshness::Year => None,
        _ => None,
    }
}

/// Build the Yahoo Search URL (`p`, `ei=UTF-8`, optional `btf=d|w|m`).
pub fn build_url(q: &SerpQuery) -> String {
    let augmented = augment_query_with_operators(
        &q.query,
        &q.include_domains,
        &q.exclude_domains,
        q.filetype.as_deref(),
    );
    let mut url = format!("{ENDPOINT}?p={}&ei=UTF-8", quote_plus(&augmented));
    if let Some(token) = q.freshness.and_then(freshness_token) {
        url.push_str("&btf=");
        url.push_str(token);
    }
    url
}

/// Parse Yahoo Search HTML into `Hit`s.
pub fn parse(html: &str) -> Vec<Hit> {
    if html.is_empty() {
        return Vec::new();
    }
    let doc = Document::from(html);
    let mut results = Vec::new();
    let mut seen = HashSet::new();

    for row in doc.select("div.algo").iter() {
        let anchor = {
            let a = row.select("div.compTitle a").first();
            if a.exists() {
                a
            } else {
                row.select("h3 a").first()
            }
        };
        if !anchor.exists() {
            continue;
        }
        let href = anchor
            .attr("href")
            .map(|s| s.to_string())
            .unwrap_or_default();
        if AD_CLICK_MARKERS.iter().any(|m| href.contains(m)) {
            continue;
        }
        let Some(url) = resolve_yahoo_url(&href) else {
            continue;
        };
        let title = inline_text(&row.select("h3").first());
        if title.is_empty() || !(url.starts_with("http://") || url.starts_with("https://")) {
            continue;
        }
        if is_dropped(&url) || !seen.insert(url.clone()) {
            continue;
        }
        let snippet = inline_text(&row.select("div.compText").first());
        let date = extract_date_hint_now(&snippet);
        results.push(make_hit(
            "yahoo",
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
        (now - self.minted_at) < JAR_TTL_SECS
    }
}

#[derive(Debug, Clone)]
enum MintOutcome {
    Minted(CookieJar),
    Failed {
        status: Option<u16>,
        message: String,
    },
    RateLimited(Option<u64>),
    Abandoned,
}

#[derive(Default)]
struct YahooSessionState {
    jars: HashMap<String, CachedJar>,
    minting: HashMap<String, broadcast::Sender<MintOutcome>>,
    generation: u64,
}

pub type ClockFn = Arc<dyn Fn() -> f64 + Send + Sync>;
pub type EgressFn = Arc<dyn Fn(&Config) -> String + Send + Sync>;

pub struct YahooEngine {
    decl: SourceDecl,
    deps: SourceDeps,
    sessions: Arc<Mutex<YahooSessionState>>,
    clock: ClockFn,
    egress_fn: EgressFn,
}

impl YahooEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let epoch = Instant::now();
        let clock: ClockFn = Arc::new(move || epoch.elapsed().as_secs_f64());
        let egress_fn: EgressFn = Arc::new(|cfg| egress_key(cfg, "yahoo"));
        Self::with_hooks(deps, clock, egress_fn)
    }

    pub fn with_hooks(deps: SourceDeps, clock: ClockFn, egress_fn: EgressFn) -> Self {
        let mut decl = SourceDecl::new(
            "yahoo",
            "Yahoo Search",
            "Yahoo Search: Bing's index via Yahoo's own front end; a keyless backup when bing is gated.",
        )
        .with_scopes(&["web"])
        .with_caps(&[Capability::Search])
        .with_op(search_op_decl(
            "Search the web via Yahoo Search.",
            "python asyncio taskgroup",
            "yahoo",
        ));
        decl.home_url = Some("https://search.yahoo.com".into());
        decl.tier = 2;
        decl.auth = AuthMode::None;
        decl.transport = TransportProfile::Chrome;
        decl.pool_role = PoolRole::Reserve;
        decl.limits = SourceLimits::new(0.5, 2, 2, 5_000, 21_600);
        Self {
            decl,
            deps,
            sessions: Arc::new(Mutex::new(YahooSessionState::default())),
            clock,
            egress_fn,
        }
    }

    /// Clear all cached jars (for tests).
    pub fn reset_jars(&self) {
        let mut st = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        st.jars.clear();
        st.minting.clear();
        st.generation = 0;
    }

    /// Inspect cached cookies for an egress (for tests).
    pub fn cached_jar(&self, egress: &str) -> Option<CookieJar> {
        let st = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        st.jars.get(egress).map(|j| j.cookies.clone())
    }

    async fn fetch_with_session(
        &self,
        url: &str,
        egress: &str,
        accept_language: &str,
        cancel: &CancellationToken,
        deadline: CallDeadline,
    ) -> Result<HttpBytes, FsmError> {
        let now = (self.clock)();
        let fresh_jar = {
            let mut st = self
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match st.jars.get(egress) {
                Some(j) if j.is_fresh(now) => Some(j.clone()),
                Some(_) => {
                    st.jars.remove(egress);
                    None
                }
                None => None,
            }
        };

        if let Some(jar) = fresh_jar {
            let mut req_jar = jar.cookies.clone();
            let primary_ncx =
                net_cx_with_deadline(cancel, deadline, &self.decl.id, Some(self.decl.limits.rps))?;
            match get_with_jar_deadline(
                self.deps.net.as_ref(),
                &self.decl.id,
                self.decl.transport,
                url,
                accept_language,
                &mut req_jar,
                &primary_ncx,
                deadline,
            )
            .await
            {
                Ok(resp) if resp.is_success() => return Ok(resp),
                Ok(resp) if resp.status == 429 => {
                    return Err(FsmError::RateLimited {
                        source_id: self.decl.id.clone(),
                        retry_after_s: parse_retry_after(&resp),
                    });
                }
                Ok(resp) if resp.status < 500 => {
                    return Err(FsmError::upstream(
                        &self.decl.id,
                        Some(resp.status),
                        format!("yahoo: HTTP {}", resp.status),
                    ));
                }
                Ok(resp) => {
                    // HTTP 5xx (typically Yahoo's empty 500 rejecting the jar):
                    // drop only this generation and run a short re-mint. This
                    // call already paid its per-call rate token above (B1).
                    tracing::info!(
                        status = resp.status,
                        "yahoo cookie jar rejected; running handshake again"
                    );
                    {
                        let mut st = self
                            .sessions
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if st
                            .jars
                            .get(egress)
                            .is_some_and(|j| j.generation == jar.generation)
                        {
                            st.jars.remove(egress);
                        }
                    }
                    return self
                        .mint(
                            url,
                            egress,
                            accept_language,
                            cancel,
                            deadline,
                            REMINT_ATTEMPTS,
                            false,
                        )
                        .await;
                }
                Err(e) => {
                    // Timeout or network reset: keep the jar, do not handshake.
                    return Err(e);
                }
            }
        }

        self.mint(
            url,
            egress,
            accept_language,
            cancel,
            deadline,
            HANDSHAKE_ATTEMPTS,
            true,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn mint(
        &self,
        url: &str,
        egress: &str,
        accept_language: &str,
        cancel: &CancellationToken,
        deadline: CallDeadline,
        attempts: usize,
        charge_first: bool,
    ) -> Result<HttpBytes, FsmError> {
        loop {
            enum Action {
                Wait(broadcast::Receiver<MintOutcome>),
                Own(broadcast::Sender<MintOutcome>),
            }

            let action = {
                let mut st = self
                    .sessions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(tx) = st.minting.get(egress) {
                    Action::Wait(tx.subscribe())
                } else {
                    let (tx, _) = broadcast::channel(1);
                    st.minting.insert(egress.to_owned(), tx.clone());
                    Action::Own(tx)
                }
            };

            match action {
                Action::Wait(mut rx) => match rx.recv().await {
                    Ok(MintOutcome::Minted(mut jar)) => {
                        let req_ncx = net_cx_with_deadline(
                            cancel,
                            deadline,
                            &self.decl.id,
                            charge_first.then_some(self.decl.limits.rps),
                        )?;
                        let resp = get_with_jar_deadline(
                            self.deps.net.as_ref(),
                            &self.decl.id,
                            self.decl.transport,
                            url,
                            accept_language,
                            &mut jar,
                            &req_ncx,
                            deadline,
                        )
                        .await?;
                        if resp.is_success() {
                            return Ok(resp);
                        }
                        if resp.status == 429 {
                            return Err(FsmError::RateLimited {
                                source_id: self.decl.id.clone(),
                                retry_after_s: parse_retry_after(&resp),
                            });
                        }
                        return Err(FsmError::upstream(
                            &self.decl.id,
                            Some(resp.status),
                            format!("yahoo: HTTP {}", resp.status),
                        ));
                    }
                    Ok(MintOutcome::Failed { status, message }) => {
                        return Err(FsmError::upstream(&self.decl.id, status, message));
                    }
                    Ok(MintOutcome::RateLimited(retry_after_s)) => {
                        return Err(FsmError::RateLimited {
                            source_id: self.decl.id.clone(),
                            retry_after_s,
                        });
                    }
                    Ok(MintOutcome::Abandoned) | Err(_) => {
                        // Owner was cancelled or minted no cookies; take over or wait on next.
                        if cancel.is_cancelled() {
                            return Err(FsmError::Timeout(0));
                        }
                        continue;
                    }
                },
                Action::Own(tx) => {
                    struct MintGuard {
                        sessions: Arc<Mutex<YahooSessionState>>,
                        egress: String,
                        tx: broadcast::Sender<MintOutcome>,
                        settled: bool,
                    }
                    impl Drop for MintGuard {
                        fn drop(&mut self) {
                            {
                                let mut st = self
                                    .sessions
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                st.minting.remove(&self.egress);
                            }
                            if !self.settled {
                                let _ = self.tx.send(MintOutcome::Abandoned);
                            }
                        }
                    }

                    let mut guard = MintGuard {
                        sessions: Arc::clone(&self.sessions),
                        egress: egress.to_owned(),
                        tx: tx.clone(),
                        settled: false,
                    };

                    let res = self
                        .handshake(
                            url,
                            egress,
                            accept_language,
                            cancel,
                            deadline,
                            attempts,
                            charge_first,
                        )
                        .await;
                    guard.settled = true;
                    match &res {
                        Ok((_, Some(jar))) => {
                            let _ = tx.send(MintOutcome::Minted(jar.clone()));
                        }
                        Ok((_, None)) => {
                            let _ = tx.send(MintOutcome::Abandoned);
                        }
                        Err(FsmError::RateLimited { retry_after_s, .. }) => {
                            let _ = tx.send(MintOutcome::RateLimited(*retry_after_s));
                        }
                        Err(FsmError::Timeout(0)) if cancel.is_cancelled() => {
                            let _ = tx.send(MintOutcome::Abandoned);
                        }
                        Err(_) => {
                            let _ = tx.send(MintOutcome::Failed {
                                status: None,
                                message: "yahoo: Yahoo's bot check did not let a concurrent search's cookie handshake through; the other engines are unaffected.".to_owned(),
                            });
                        }
                    }
                    drop(guard);
                    return res.map(|(resp, _)| resp);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn handshake(
        &self,
        url: &str,
        egress: &str,
        accept_language: &str,
        cancel: &CancellationToken,
        deadline: CallDeadline,
        attempts: usize,
        charge_first: bool,
    ) -> Result<(HttpBytes, Option<CookieJar>), FsmError> {
        let mut session_jar = CookieJar::new();
        let mut last_status: Option<u16> = None;

        for attempt in 0..attempts.max(1) {
            // Only the very first hop of a cold call charges the engine rate
            // limit; subsequent handshake attempts and redirect hops are
            // auxiliary (`rps: None`) bounded by the shared per-call deadline (B1/M4).
            let rps = if attempt == 0 && charge_first {
                Some(self.decl.limits.rps)
            } else {
                None
            };
            let hop_ncx = net_cx_with_deadline(cancel, deadline, &self.decl.id, rps)?;
            let resp = get_with_jar_deadline(
                self.deps.net.as_ref(),
                &self.decl.id,
                self.decl.transport,
                url,
                accept_language,
                &mut session_jar,
                &hop_ncx,
                deadline,
            )
            .await?;
            last_status = Some(resp.status);
            if resp.status == 429 {
                return Err(FsmError::RateLimited {
                    source_id: self.decl.id.clone(),
                    retry_after_s: parse_retry_after(&resp),
                });
            }
            if resp.status == 403 {
                break;
            }
            if resp.status == 200 {
                let saved = if !session_jar.is_empty() {
                    let mut st = self
                        .sessions
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    st.generation += 1;
                    let generation = st.generation;
                    st.jars.insert(
                        egress.to_owned(),
                        CachedJar {
                            cookies: session_jar.clone(),
                            minted_at: (self.clock)(),
                            generation,
                        },
                    );
                    Some(session_jar)
                } else {
                    None
                };
                return Ok((resp, saved));
            }
        }

        let status_txt = last_status
            .map(|s| s.to_string())
            .unwrap_or_else(|| "unknown".to_owned());
        Err(FsmError::upstream(
            &self.decl.id,
            last_status,
            format!(
                "yahoo: Yahoo's bot check did not let this request through (its cookie handshake ended in HTTP {status_txt}); the other engines are unaffected."
            ),
        ))
    }
}

#[async_trait]
impl Source for YahooEngine {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    async fn call(&self, op: &str, params: Value, cx: &SrcCx) -> Result<SrcOutput, FsmError> {
        let q = SerpQuery::from_call(&self.decl.id, op, &params, cx, &self.deps.cfg)?;
        let url = build_url(&q);
        let deadline = CallDeadline::from_timeout(q.timeout);
        let egress = (self.egress_fn)(&self.deps.cfg);

        let resp = self
            .fetch_with_session(&url, &egress, &q.accept_language, &cx.cancel, deadline)
            .await?;
        let html = decode_body(&resp);
        let parsed = parse(&html);
        let checked = check_wall_if_empty(&self.decl.id, resp.status, &html, GATE_MARKERS, parsed)?;
        let hits = finalize_hits(
            checked,
            &q.include_domains,
            &q.exclude_domains,
            q.filetype.as_deref(),
            q.limit,
        );
        Ok(SrcOutput::Hits(hits))
    }
}
