//! Baidu Search HTML SERP engine (`www.baidu.com/s`).
//!
//! Port of `src/search_mcp/engines/baidu.py` plus per-egress cookie preflight
//! (`D/research/general.md` L66 & `SPEC.md` §7.1):
//! - Warms session cookies via non-fatal single-flight GET to `https://www.baidu.com/`;
//! - Prefers `div[mu]` direct URL over Baidu's opaque `/link?url=` redirector;
//! - Detects `百度安全验证` CAPTCHA walls when parsing yields 0 hits.

use crate::common::{
    CallDeadline, SerpQuery, augment_query_with_operators, check_http_status, check_wall_if_empty,
    decode_body, extract_date_hint_now, finalize_hits, make_hit, net_cx_with_deadline, quote_plus,
    search_op_decl, text_of,
};
use crate::cookie_jar::{CookieJar, egress_key, get_with_jar_deadline};
use async_trait::async_trait;
use dom_query::Document;
use fsm_core::source::{
    AuthMode, Capability, PoolRole, Source, SourceDecl, SourceLimits, SrcCx, SrcOutput,
    TransportProfile,
};
use fsm_core::types::{Hit, HitKind};
use fsm_core::url_canon::{host_matches, host_of};
use fsm_core::{FsmError, SourceDeps};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use url::Url;

pub const WARM_URL: &str = "https://www.baidu.com/";
pub const SEARCH_ENDPOINT: &str = "https://www.baidu.com/s";
const JAR_TTL_SECS: f64 = 1800.0;
const WARM_FAIL_TTL_SECS: f64 = 60.0;

pub const GATE_MARKERS: &[(&str, &[&str])] = &[(
    "captcha",
    &[
        "百度安全验证",
        "wappass.baidu.com",
        "bioc-static.cdn.bcebos.com/pages/ds_svcp.html",
        "seccaptcha",
    ],
)];

/// Build the Baidu search URL (`wd`, `rn=10..=50`).
pub fn build_url(q: &SerpQuery) -> String {
    let rn = q.limit.clamp(10, 50);
    let augmented = augment_query_with_operators(
        &q.query,
        &q.include_domains,
        &q.exclude_domains,
        q.filetype.as_deref(),
    );
    format!("{SEARCH_ENDPOINT}?wd={}&rn={rn}", quote_plus(&augmented))
}

const DROPPED_HOSTS: &[&str] = &["nourl.ubs.baidu.com", "recommend_list.baidu.com"];

fn is_dropped_baidu_url(url: &str) -> bool {
    let host = host_of(url);
    if host.is_empty() || host_matches(&host, DROPPED_HOSTS) {
        return true;
    }
    if host_matches(&host, &["baidu.com"])
        && let Ok(parsed) = Url::parse(url)
        && parsed.path() == "/s"
    {
        return true;
    }
    false
}

/// Parse Baidu SERP HTML (`div.result.c-container, div.result-op.c-container`).
pub fn parse(html: &str) -> Vec<Hit> {
    if html.is_empty() {
        return Vec::new();
    }
    let doc = Document::from(html);
    let mut results = Vec::new();
    let mut seen = HashSet::new();

    for div in doc
        .select("div.result.c-container, div.result-op.c-container")
        .iter()
    {
        let link = {
            let l = div.select("h3.t a").first();
            if l.exists() {
                l
            } else {
                div.select("h3 a").first()
            }
        };
        if !link.exists() {
            continue;
        }
        let url = div
            .attr("mu")
            .filter(|s| !s.trim().is_empty())
            .or_else(|| link.attr("href"))
            .map(|s| s.trim().to_owned())
            .unwrap_or_default();
        if url.is_empty() || is_dropped_baidu_url(&url) {
            continue;
        }
        let title = text_of(&link);
        let snippet_sel = [
            r#"[class*="summary-text"]"#,
            ".c-abstract",
            r#"[class*="content-right"]"#,
            r#"[class*="abstract"]"#,
        ]
        .iter()
        .find_map(|s| {
            let sel = div.select(s).first();
            sel.exists().then_some(sel)
        });
        let snippet = snippet_sel.as_ref().map(text_of).unwrap_or_default();
        if title.is_empty() || !seen.insert(url.clone()) {
            continue;
        }
        let date = extract_date_hint_now(&snippet).or_else(|| extract_date_hint_now(&title));
        results.push(make_hit(
            "baidu",
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
struct BaiduSessionState {
    jars: HashMap<String, CachedJar>,
    minting: HashMap<String, broadcast::Sender<Option<CookieJar>>>,
}

pub struct BaiduEngine {
    decl: SourceDecl,
    deps: SourceDeps,
    sessions: Arc<Mutex<BaiduSessionState>>,
    epoch: Instant,
}

impl BaiduEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let mut decl = SourceDecl::new(
            "baidu",
            "Baidu",
            "百度: the largest Chinese-language web index.",
        )
        .with_scopes(&["web"])
        .with_caps(&[Capability::Search])
        .with_op(search_op_decl(
            "Search Chinese web results via Baidu.",
            "python 异步编程",
            "baidu",
        ));
        decl.aliases = vec!["百度".into()];
        decl.home_url = Some("https://www.baidu.com".into());
        decl.tier = 2;
        decl.auth = AuthMode::None;
        decl.transport = TransportProfile::Chrome;
        decl.pool_role = PoolRole::Locale("zh".into());
        decl.limits = SourceLimits::new(0.5, 2, 2, 5_000, 21_600);
        Self {
            decl,
            deps,
            sessions: Arc::new(Mutex::new(BaiduSessionState::default())),
            epoch: Instant::now(),
        }
    }

    async fn session_jar(
        &self,
        key: &str,
        accept_language: &str,
        cancel: &CancellationToken,
        deadline: CallDeadline,
    ) -> Result<CookieJar, FsmError> {
        loop {
            if cancel.is_cancelled() {
                return Err(FsmError::Timeout(0));
            }
            enum Action {
                Fresh(CookieJar),
                Wait(broadcast::Receiver<Option<CookieJar>>),
                Mint(broadcast::Sender<Option<CookieJar>>),
            }

            let now = self.epoch.elapsed().as_secs_f64();
            let action = {
                let mut st = self
                    .sessions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(jar) = st.jars.get(key)
                    && jar.is_fresh(now)
                {
                    Action::Fresh(jar.cookies.clone())
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
                    struct Guard {
                        sessions: Arc<Mutex<BaiduSessionState>>,
                        key: String,
                        tx: broadcast::Sender<Option<CookieJar>>,
                        settled: bool,
                    }
                    impl Drop for Guard {
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
                    let mut guard = Guard {
                        sessions: Arc::clone(&self.sessions),
                        key: key.to_owned(),
                        tx: tx.clone(),
                        settled: false,
                    };

                    let aux_ncx = net_cx_with_deadline(cancel, deadline, &self.decl.id, None)?;
                    let mut jar = CookieJar::new();
                    if let Err(e) = get_with_jar_deadline(
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
                        if cancel.is_cancelled() {
                            // Never cache a cancelled warm-up as a failed warm-up (M1).
                            return Err(FsmError::Timeout(0));
                        }
                        tracing::debug!(error = %e, "baidu session warm-up failed");
                        jar = CookieJar::new();
                    }

                    if cancel.is_cancelled() {
                        return Err(FsmError::Timeout(0));
                    }

                    {
                        let mut st = self
                            .sessions
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        st.jars.insert(
                            key.to_owned(),
                            CachedJar {
                                cookies: jar.clone(),
                                minted_at: self.epoch.elapsed().as_secs_f64(),
                            },
                        );
                    }
                    guard.settled = true;
                    let _ = tx.send(Some(jar.clone()));
                    drop(guard);
                    return Ok(jar);
                }
            }
        }
    }
}

#[async_trait]
impl Source for BaiduEngine {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    async fn call(&self, op: &str, params: Value, cx: &SrcCx) -> Result<SrcOutput, FsmError> {
        let q = SerpQuery::from_call(&self.decl.id, op, &params, cx, &self.deps.cfg)?;
        let url = build_url(&q);
        let deadline = CallDeadline::from_timeout(q.timeout);
        let key = egress_key(&self.deps.cfg, &self.decl.id);

        let mut jar = self
            .session_jar(&key, &q.accept_language, &cx.cancel, deadline)
            .await?;
        let primary_ncx = net_cx_with_deadline(
            &cx.cancel,
            deadline,
            &self.decl.id,
            Some(self.decl.limits.rps),
        )?;
        let resp = get_with_jar_deadline(
            self.deps.net.as_ref(),
            &self.decl.id,
            self.decl.transport,
            &url,
            &q.accept_language,
            &mut jar,
            &primary_ncx,
            deadline,
        )
        .await?;
        check_http_status(&self.decl.id, &resp)?;
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
