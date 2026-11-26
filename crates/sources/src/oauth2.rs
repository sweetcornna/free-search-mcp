//! OAuth2 `client_credentials` token exchange and in-memory cache (A1 §B.8).
//!
//! Security invariants:
//! - Tokens are cached strictly in process memory (never SQLite, disk, logs, or fixtures).
//! - Per-source single-flight mutex prevents token stampedes.
//! - Proactive skew refresh at `now + expires_in - min(60, expires_in / 5)`.
//! - Eviction helper supports single-retry on HTTP 401.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use fsm_core::manifest::{AuthDecl, OAuthStyle};
use fsm_core::{FsmError, HttpRequest, NetClient, NetCx, TransportProfile};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock};
use url::form_urlencoded;

#[derive(Debug, Clone)]
struct CachedToken {
    access_token: String,
    refresh_at: Instant,
}

#[derive(Default)]
struct PerSourceSlot {
    token: RwLock<Option<CachedToken>>,
    refresh_lock: Mutex<()>,
}

/// Process-local in-memory OAuth2 token cache shared by manifest sources.
#[derive(Default)]
pub struct OAuth2TokenCache {
    slots: RwLock<HashMap<String, Arc<PerSourceSlot>>>,
}

impl std::fmt::Debug for OAuth2TokenCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuth2TokenCache").finish_non_exhaustive()
    }
}

impl OAuth2TokenCache {
    pub fn new() -> Self {
        Self::default()
    }

    async fn slot(&self, source_id: &str) -> Arc<PerSourceSlot> {
        if let Some(s) = self.slots.read().await.get(source_id) {
            return Arc::clone(s);
        }
        let mut guard = self.slots.write().await;
        Arc::clone(
            guard
                .entry(source_id.to_owned())
                .or_insert_with(|| Arc::new(PerSourceSlot::default())),
        )
    }

    /// Compare-and-swap eviction: clears the cached token for `source_id` ONLY if it
    /// still matches `stale_token` (prevents a racing 401 handler from wiping a fresh token).
    pub async fn evict(&self, source_id: &str, stale_token: &str) {
        let slot = self.slot(source_id).await;
        let mut guard = slot.token.write().await;
        if guard
            .as_ref()
            .is_some_and(|cached| cached.access_token == stale_token)
        {
            *guard = None;
        }
    }

    /// Return a fresh `access_token` for `source_id`, using the in-memory cache
    /// unless absent or past its proactive skew-refresh timestamp.
    pub async fn get_token(
        &self,
        source_id: &str,
        auth: &AuthDecl,
        client_id: &str,
        client_secret: &str,
        net: &dyn NetClient,
        ncx: &NetCx,
    ) -> Result<String, FsmError> {
        let slot = self.slot(source_id).await;
        let now = Instant::now();
        if let Some(cached) = slot.token.read().await.as_ref()
            && now < cached.refresh_at
        {
            return Ok(cached.access_token.clone());
        }

        // Single-flight refresh per source.
        let _guard = slot.refresh_lock.lock().await;
        let now = Instant::now();
        if let Some(cached) = slot.token.read().await.as_ref()
            && now < cached.refresh_at
        {
            return Ok(cached.access_token.clone());
        }

        let fetched = fetch_token(source_id, auth, client_id, client_secret, net, ncx).await?;
        let access_token = fetched.access_token.clone();
        *slot.token.write().await = Some(fetched);
        Ok(access_token)
    }
}

async fn fetch_token(
    source_id: &str,
    auth: &AuthDecl,
    client_id: &str,
    client_secret: &str,
    net: &dyn NetClient,
    ncx: &NetCx,
) -> Result<CachedToken, FsmError> {
    let token_url = auth
        .token_url
        .as_deref()
        .ok_or_else(|| FsmError::invalid(format!("{source_id}: oauth2 auth missing token_url")))?;
    net.check_url_offline(token_url)?;

    let body = {
        let mut form = form_urlencoded::Serializer::new(String::new());
        form.append_pair("grant_type", "client_credentials");
        if auth.effective_style() == OAuthStyle::Body {
            form.append_pair("client_id", client_id);
            form.append_pair("client_secret", client_secret);
        }
        if let Some(scope) = auth.scope.as_deref().filter(|s| !s.trim().is_empty()) {
            form.append_pair("scope", scope);
        }
        form.finish()
    };

    let mut req = HttpRequest::post(token_url, body)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .for_source(source_id);
    req.follow_redirects = false;

    if auth.effective_style() == OAuthStyle::Basic {
        let basic = B64.encode(format!("{client_id}:{client_secret}"));
        req = req
            .header("Authorization", format!("Basic {basic}"))
            .redact_header("Authorization");
    }

    let resp = net.send(req, TransportProfile::Plain, ncx).await?;
    if !resp.is_success() {
        let preview: String = String::from_utf8_lossy(&resp.body)
            .chars()
            .take(200)
            .collect();
        return Err(FsmError::upstream(
            source_id,
            Some(resp.status),
            format!(
                "OAuth2 token exchange failed (HTTP {}): {preview}",
                resp.status
            ),
        ));
    }

    let parsed: Value = serde_json::from_slice(&resp.body).map_err(|e| {
        FsmError::upstream(
            source_id,
            Some(resp.status),
            format!("OAuth2 token response invalid JSON: {e}"),
        )
    })?;

    let access_token = parsed
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            FsmError::upstream(
                source_id,
                Some(resp.status),
                "OAuth2 token response missing access_token",
            )
        })?
        .to_owned();

    let expires_in = parsed
        .get("expires_in")
        .and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
        })
        .unwrap_or(300);

    // Proactive skew refresh: now + expires_in - min(60, expires_in / 5).
    let skew = (expires_in / 5).min(60);
    let ttl_s = expires_in.saturating_sub(skew);
    Ok(CachedToken {
        access_token,
        refresh_at: Instant::now() + Duration::from_secs(ttl_s),
    })
}
