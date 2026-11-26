//! Declarative manifest interpreter (`ManifestSource`: `Source` impl with lazy
//! per-source `jaq` compilation and memoization per S14, full A1 §A–§C support).

use crate::csv_parse::csv_to_json;
use crate::html::extract_html;
use crate::jaq_rt::{CompiledFilter, MAX_JAQ_STREAM_ITEMS, compile_filter, compile_user_filter};
use crate::oauth2::OAuth2TokenCache;
use crate::xml::xml_to_json;
use fsm_core::fixture::canonical_json;
use fsm_core::manifest::{
    ArrayStyle, AuthDecl, AuthIn, AuthKind, AuthModeManifest, BodyKind, BodySpec, ClaimsManifest,
    ManifestDecl, OnEmpty, OpAuthOverride, OpSpec, PaginateStyle, ParamDecl, ParamLocation,
    PathEncode, PreAuth, PreStepDecl, ResponseFormat, placeholders,
};
use fsm_core::{
    Availability, Config, Freshness, FsmError, Hit, HitKind, HttpBytes, HttpMethod, HttpRequest,
    NetCx, ParamType, ReturnKind, ScholarlyWork, Source, SourceDecl, SourceDeps, SrcCx, SrcOutput,
};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use url::{Url, form_urlencoded};

/// RFC 3986 `unreserved` = `A-Z a-z 0-9 - . _ ~` (`encode = "segment"`).
const SEGMENT_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// RFC 3986 non-percent `pchar` + `/` (`encode = "raw"`, A1 §B.3: `%` IS encoded as `%25`).
const RAW_PATH_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b'!')
    .remove(b'$')
    .remove(b'&')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')')
    .remove(b'*')
    .remove(b'+')
    .remove(b',')
    .remove(b';')
    .remove(b'=')
    .remove(b':')
    .remove(b'@')
    .remove(b'/');

/// Query value encoding set for inline `{var}` in URL query strings.
const QUERY_VAL_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b',');

struct CompiledBody {
    jaq: Option<CompiledFilter>,
    variables_jaq: Option<CompiledFilter>,
}

struct CompiledPre {
    bind: CompiledFilter,
    body: Option<CompiledBody>,
}

struct CompiledMap {
    items: CompiledFilter,
    next_cursor: Option<CompiledFilter>,
    total: Option<CompiledFilter>,
    error: Option<CompiledFilter>,
    work: Option<CompiledFilter>,
    record: Option<CompiledFilter>,
    hit: Option<CompiledFilter>,
}

struct CompiledOp {
    query_jaq: Option<CompiledFilter>,
    body: Option<CompiledBody>,
    pre: Option<CompiledPre>,
    map: Option<CompiledMap>,
}

struct CompiledManifest {
    ops: HashMap<String, CompiledOp>,
}

struct CompiledClaims {
    patterns: Vec<Regex>,
    /// Word-boundary regexes for ASCII keywords (prevents substring false positives
    /// like `"dom"` claiming `"freedom"`, `"domain"`, or `"random"`).
    ascii_keywords: Vec<Regex>,
    /// Non-ASCII (e.g. CJK) keywords matched as lowercased substrings.
    unicode_keywords: Vec<String>,
}

impl CompiledClaims {
    fn from_manifest(c: &ClaimsManifest) -> Self {
        let patterns = c
            .patterns
            .iter()
            .filter_map(|p| Regex::new(p).ok())
            .collect();
        let mut ascii_keywords = Vec::new();
        let mut unicode_keywords = Vec::new();
        for raw in &c.keywords {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.is_ascii() {
                let pat = format!(r"(?i)\b{}\b", regex::escape(trimmed));
                if let Ok(re) = Regex::new(&pat) {
                    ascii_keywords.push(re);
                }
            } else {
                unicode_keywords.push(trimmed.to_lowercase());
            }
        }
        Self {
            patterns,
            ascii_keywords,
            unicode_keywords,
        }
    }

    fn score(&self, query: &str) -> f32 {
        // R1: regex pattern match scores 1.0 (direct answer); keyword-only match scores 0.5.
        if self.patterns.iter().any(|re| re.is_match(query)) {
            return 1.0;
        }
        if self.ascii_keywords.iter().any(|re| re.is_match(query)) {
            return 0.5;
        }
        if !self.unicode_keywords.is_empty() {
            let lower = query.to_lowercase();
            if self.unicode_keywords.iter().any(|kw| lower.contains(kw)) {
                return 0.5;
            }
        }
        0.0
    }
}

/// Declarative TOML + `jaq` [`Source`] implementation with lazy filter compilation (S14).
pub struct ManifestSource {
    manifest: Arc<ManifestDecl>,
    decl: SourceDecl,
    deps: SourceDeps,
    oauth: Arc<OAuth2TokenCache>,
    claims: Option<CompiledClaims>,
    is_user_override: bool,
    compiled: OnceLock<Result<CompiledManifest, String>>,
}

impl std::fmt::Debug for ManifestSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManifestSource")
            .field("id", &self.decl.id)
            .finish_non_exhaustive()
    }
}

impl ManifestSource {
    /// Construct a manifest source without compiling any `jaq` filters yet (S14).
    pub fn new(manifest: ManifestDecl, deps: SourceDeps, oauth: Arc<OAuth2TokenCache>) -> Self {
        Self::new_inner(manifest, deps, oauth, false)
    }

    /// Construct a user-override manifest source (`sources.d/*.toml`) that enforces
    /// strict DoS linting and strips looping `jaq-core` defs at lazy compilation time.
    pub fn new_user(
        manifest: ManifestDecl,
        deps: SourceDeps,
        oauth: Arc<OAuth2TokenCache>,
    ) -> Self {
        Self::new_inner(manifest, deps, oauth, true)
    }

    fn new_inner(
        manifest: ManifestDecl,
        deps: SourceDeps,
        oauth: Arc<OAuth2TokenCache>,
        is_user_override: bool,
    ) -> Self {
        let decl = manifest.to_source_decl();
        let claims = manifest.claims.as_ref().map(CompiledClaims::from_manifest);
        Self {
            manifest: Arc::new(manifest),
            decl,
            deps,
            oauth,
            claims,
            is_user_override,
            compiled: OnceLock::new(),
        }
    }

    /// Access the underlying declarative manifest.
    pub fn manifest(&self) -> &ManifestDecl {
        &self.manifest
    }

    /// Lazily compile and memoize all `jaq` filters for this source (S14).
    fn compiled(&self) -> Result<&CompiledManifest, FsmError> {
        let res = self
            .compiled
            .get_or_init(|| compile_manifest(&self.manifest, self.is_user_override));
        res.as_ref()
            .map_err(|e| FsmError::invalid(format!("manifest {:?}: {e}", self.manifest.id)))
    }

    fn effective_op_auth(&self, op: &OpSpec) -> EffectiveAuth {
        match op.auth {
            OpAuthOverride::None => EffectiveAuth::None,
            OpAuthOverride::Required => EffectiveAuth::Required,
            OpAuthOverride::Inherit | _ => match &self.manifest.auth {
                None => EffectiveAuth::None,
                Some(a) if a.mode == AuthModeManifest::Required => EffectiveAuth::Required,
                Some(_) => EffectiveAuth::Optional,
            },
        }
    }

    fn has_credentials(&self, cfg: &Config) -> bool {
        let Some(auth) = &self.manifest.auth else {
            return true;
        };
        match auth.kind {
            AuthKind::Static => auth
                .env
                .as_deref()
                .and_then(|e| cfg.api_keys.get(e))
                .is_some_and(|v| !v.is_empty()),
            AuthKind::Oauth2ClientCredentials => {
                let id_ok = auth
                    .env_id
                    .as_deref()
                    .and_then(|e| cfg.api_keys.get(e))
                    .is_some_and(|v| !v.is_empty());
                let sec_ok = auth
                    .env_secret
                    .as_deref()
                    .and_then(|e| cfg.api_keys.get(e))
                    .is_some_and(|v| !v.is_empty());
                id_ok && sec_ok
            }
            _ => false,
        }
    }

    /// Resolve the credential token/key to inject for an op (if any).
    /// Returns `(Option<cred>, used_oauth2)`.
    async fn resolve_credential(
        &self,
        op: &OpSpec,
        cx: &SrcCx,
        ncx: &NetCx,
    ) -> Result<(Option<String>, bool), FsmError> {
        let eff = self.effective_op_auth(op);
        if eff == EffectiveAuth::None {
            return Ok((None, false));
        }
        let Some(auth) = &self.manifest.auth else {
            return Ok((None, false));
        };

        match auth.kind {
            AuthKind::Static => {
                let env_name = auth.env.as_deref().unwrap_or("");
                let key = cx
                    .api_key
                    .clone()
                    .or_else(|| self.deps.cfg.api_keys.get(env_name).cloned())
                    .filter(|v| !v.is_empty());

                match key {
                    Some(k) => Ok((Some(k), false)),
                    None if eff == EffectiveAuth::Required && cx.replay_mode => {
                        // A1 §B.9 #4: synthetic placeholder stripped by ReplayNet redaction.
                        Ok((Some("__replay_key__".to_owned()), false))
                    }
                    None if eff == EffectiveAuth::Required => Err(FsmError::AuthRequired {
                        source_id: self.manifest.id.clone(),
                        env_var: auth.env_display(),
                    }),
                    None => Ok((None, false)),
                }
            }
            AuthKind::Oauth2ClientCredentials => {
                if cx.replay_mode {
                    let has_real = self.has_credentials(&self.deps.cfg);
                    if eff == EffectiveAuth::Required || has_real {
                        // A1 §B.9 #4: bypass live token_url fetch in offline replay.
                        return Ok((Some("__replay_oauth_token__".to_owned()), false));
                    }
                    return Ok((None, false));
                }
                let id_env = auth.env_id.as_deref().unwrap_or("");
                let sec_env = auth.env_secret.as_deref().unwrap_or("");
                let client_id = self.deps.cfg.api_keys.get(id_env).filter(|v| !v.is_empty());
                let client_secret = self
                    .deps
                    .cfg
                    .api_keys
                    .get(sec_env)
                    .filter(|v| !v.is_empty());

                match (client_id, client_secret) {
                    (Some(cid), Some(csec)) => {
                        let tok = self
                            .oauth
                            .get_token(
                                &self.manifest.id,
                                auth,
                                cid,
                                csec,
                                self.deps.net.as_ref(),
                                ncx,
                            )
                            .await?;
                        Ok((Some(tok), true))
                    }
                    _ if eff == EffectiveAuth::Required => Err(FsmError::AuthRequired {
                        source_id: self.manifest.id.clone(),
                        env_var: auth.env_display(),
                    }),
                    _ => Ok((None, false)),
                }
            }
            _ => Ok((None, false)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_pre(
        &self,
        op_name: &str,
        op: &OpSpec,
        pre: &PreStepDecl,
        compiled_pre: &CompiledPre,
        resolved_params: &Map<String, Value>,
        p_val: &Value,
        cfg_val: &Value,
        cred: Option<&str>,
        used_oauth2: bool,
        cx: &SrcCx,
        ncx: &NetCx,
    ) -> Result<(PreOutcome, Option<String>), FsmError> {
        let mut current_cred = cred.map(str::to_owned);
        let mut retried_401 = false;
        let auth_for_pre = if pre.auth == PreAuth::Inherit {
            self.manifest.auth.as_ref()
        } else {
            None
        };

        let resp = loop {
            let pre_cred = if pre.auth == PreAuth::Inherit {
                current_cred.as_deref()
            } else {
                None
            };

            let tpl_ctx = TemplateCx {
                op_params: &op.params,
                resolved: resolved_params,
                pre_val: None,
                key: pre_cred,
                cx,
            };

            let expanded_url = expand_url_template(&pre.url, &tpl_ctx)?;
            verify_same_origin_and_safe(&pre.url, &expanded_url, self.deps.net.as_ref())?;

            let mut req = match pre.method {
                HttpMethod::Get => HttpRequest::get(expanded_url),
                HttpMethod::Head => HttpRequest::head(expanded_url),
                HttpMethod::Post => HttpRequest::post(expanded_url, Vec::<u8>::new()),
                _ => HttpRequest::get(expanded_url),
            }
            .for_source(&self.manifest.id)
            .with_max_bytes(self.deps.cfg.max_response_bytes);

            let mut headers: Vec<(String, String)> = Vec::new();
            if let (Some(body_spec), Some(comp_body)) = (&pre.body, &compiled_pre.body) {
                let (body_bytes, default_hdrs) = build_request_body(
                    body_spec,
                    comp_body,
                    p_val,
                    cfg_val,
                    auth_for_pre,
                    pre_cred,
                    cx.replay_mode,
                    &self.manifest.id,
                )?;
                req.body = Some(body_bytes.into());
                for (k, v) in default_hdrs {
                    set_header_ci(&mut headers, k, v);
                }
            }

            for (hk, hv) in &pre.headers {
                let val = expand_plain_template(hv, &tpl_ctx);
                set_header_ci(&mut headers, hk.clone(), val);
            }

            // Pre query pairs: const_query (sorted) then params (sorted).
            for (k, cv) in &pre.const_query {
                for v in cv.values() {
                    req.query
                        .push((k.clone(), expand_plain_template(v, &tpl_ctx)));
                }
            }
            for (k, cv) in &pre.params {
                for v in cv.values() {
                    if is_unset_single_param_placeholder(v, &op.params, resolved_params) {
                        continue;
                    }
                    req.query
                        .push((k.clone(), expand_plain_template(v, &tpl_ctx)));
                }
            }

            if let (Some(auth), Some(token)) = (auth_for_pre, pre_cred) {
                apply_auth_to_request(auth, token, &mut req, &mut headers);
            }
            req.headers = headers;

            let resp = self
                .deps
                .net
                .send(req, self.manifest.transport.profile(), ncx)
                .await?;

            // M6: single OAuth2 401 CAS-evict + retry on pre.auth == inherit.
            if resp.status == 401
                && pre.auth == PreAuth::Inherit
                && used_oauth2
                && !retried_401
                && !cx.replay_mode
            {
                retried_401 = true;
                if let Some(stale) = current_cred.as_deref() {
                    self.oauth.evict(&self.manifest.id, stale).await;
                }
                let (fresh_cred, _) = self.resolve_credential(op, cx, ncx).await?;
                current_cred = fresh_cred;
                continue;
            }
            break resp;
        };

        if resp.truncated {
            return Err(FsmError::MaxBytesExceeded(self.deps.cfg.max_response_bytes));
        }

        if resp.status == 429 {
            return Err(FsmError::RateLimited {
                source_id: self.manifest.id.clone(),
                retry_after_s: parse_retry_after(&resp),
            });
        }
        if !resp.is_success() {
            let decoded = decode_body(&resp);
            return Err(FsmError::upstream(
                &self.manifest.id,
                Some(resp.status),
                format!(
                    "op.{op_name}.pre HTTP {}: {}",
                    resp.status,
                    first_n_chars(&decoded, 200)
                ),
            ));
        }

        let decoded = decode_body(&resp);
        let root_val = parse_response_body(
            pre.response,
            &decoded,
            None,
            None,
            &self.manifest.id,
            resp.status,
        )?;

        let outputs = compiled_pre
            .bind
            .eval_stream(&root_val, p_val, cfg_val, 1)?;
        let bound_opt = match outputs.into_iter().next() {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if s.is_empty() => None,
            Some(Value::Array(a)) if a.is_empty() => None,
            Some(Value::String(s)) => Some(s),
            Some(Value::Number(n)) => Some(n.to_string()),
            Some(Value::Bool(b)) => Some(b.to_string()),
            Some(other) => Some(other.to_string()),
        };

        let outcome = match bound_opt {
            Some(s) => {
                if s.contains(['\0', '\r', '\n']) {
                    return Err(FsmError::invalid(format!(
                        "{}.{op_name}: pre.bind produced control characters",
                        self.manifest.id
                    )));
                }
                PreOutcome::Bound(s)
            }
            None => match pre.on_empty {
                OnEmpty::Empty => PreOutcome::ShortCircuitEmpty,
                OnEmpty::Error => {
                    return Err(FsmError::upstream(
                        &self.manifest.id,
                        None,
                        "pre-flight bind produced empty result",
                    ));
                }
                OnEmpty::Continue => PreOutcome::Bound(String::new()),
                _ => PreOutcome::ShortCircuitEmpty,
            },
        };
        Ok((outcome, current_cred))
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_main_page(
        &self,
        op: &OpSpec,
        comp_op: &CompiledOp,
        resolved_params: &Map<String, Value>,
        p_val: &Value,
        cfg_val: &Value,
        pre_bound: Option<&str>,
        override_url: Option<&str>,
        cred: Option<&str>,
        used_oauth2: bool,
        cx: &SrcCx,
        ncx: &NetCx,
    ) -> Result<HttpBytes, FsmError> {
        let mut current_cred = cred.map(str::to_owned);
        let mut retried_401 = false;

        loop {
            let candidate_urls: Vec<(&str, bool)> = match override_url {
                Some(u) => vec![(u, false)],
                None => std::iter::once((op.url.as_str(), true))
                    .chain(op.fallback_urls.iter().map(|u| (u.as_str(), true)))
                    .collect(),
            };

            let mut last_err: Option<FsmError> = None;
            let mut got_resp: Option<HttpBytes> = None;

            for (idx, (tmpl_url, is_template)) in candidate_urls.iter().enumerate() {
                if cx.cancel.is_cancelled() {
                    return Err(FsmError::Timeout(0));
                }
                let req = self.build_main_request(
                    op,
                    comp_op,
                    resolved_params,
                    p_val,
                    cfg_val,
                    pre_bound,
                    tmpl_url,
                    *is_template,
                    current_cred.as_deref(),
                    cx,
                )?;

                match self
                    .deps
                    .net
                    .send(req, self.manifest.transport.profile(), ncx)
                    .await
                {
                    Ok(resp) => {
                        let has_more = idx + 1 < candidate_urls.len();
                        if has_more
                            && (resp.status == 408 || resp.status == 429 || resp.status >= 500)
                        {
                            let decoded = decode_body(&resp);
                            last_err = Some(FsmError::upstream(
                                &self.manifest.id,
                                Some(resp.status),
                                first_n_chars(&decoded, 200),
                            ));
                            continue;
                        }
                        got_resp = Some(resp);
                        break;
                    }
                    Err(e) => {
                        let retryable = matches!(
                            e,
                            FsmError::Timeout(_)
                                | FsmError::RateLimited { .. }
                                | FsmError::Upstream {
                                    status: None | Some(500..=599) | Some(408) | Some(429),
                                    ..
                                }
                        );
                        let has_more = idx + 1 < candidate_urls.len();
                        if retryable && has_more {
                            last_err = Some(e);
                            continue;
                        }
                        return Err(e);
                    }
                }
            }

            let resp = match got_resp {
                Some(r) => r,
                None => {
                    return Err(last_err.unwrap_or_else(|| {
                        FsmError::upstream(&self.manifest.id, None, "all candidate URLs failed")
                    }));
                }
            };

            // A1 §B.8 #4 + M6: on HTTP 401 with cached OAuth2 token, CAS-evict and retry ONCE.
            if resp.status == 401 && used_oauth2 && !retried_401 && !cx.replay_mode {
                retried_401 = true;
                if let Some(stale) = current_cred.as_deref() {
                    self.oauth.evict(&self.manifest.id, stale).await;
                }
                let (fresh_cred, _) = self.resolve_credential(op, cx, ncx).await?;
                current_cred = fresh_cred;
                continue;
            }

            return Ok(resp);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn build_main_request(
        &self,
        op: &OpSpec,
        comp_op: &CompiledOp,
        resolved_params: &Map<String, Value>,
        p_val: &Value,
        cfg_val: &Value,
        pre_bound: Option<&str>,
        url_str: &str,
        is_template: bool,
        cred: Option<&str>,
        cx: &SrcCx,
    ) -> Result<HttpRequest, FsmError> {
        let eff_auth = self.effective_op_auth(op);
        let active_auth = if eff_auth != EffectiveAuth::None {
            self.manifest.auth.as_ref()
        } else {
            None
        };

        let tpl_ctx = TemplateCx {
            op_params: &op.params,
            resolved: resolved_params,
            pre_val: pre_bound,
            key: cred,
            cx,
        };

        let expanded_url = if is_template {
            let expanded = expand_url_template(url_str, &tpl_ctx)?;
            verify_same_origin_and_safe(url_str, &expanded, self.deps.net.as_ref())?;
            expanded
        } else {
            verify_same_origin_and_safe(&op.url, url_str, self.deps.net.as_ref())?;
            url_str.to_owned()
        };

        let mut req = match op.method {
            HttpMethod::Get => HttpRequest::get(expanded_url),
            HttpMethod::Head => HttpRequest::head(expanded_url),
            HttpMethod::Post => HttpRequest::post(expanded_url, Vec::<u8>::new()),
            _ => HttpRequest::get(expanded_url),
        }
        .for_source(&self.manifest.id)
        .with_max_bytes(self.deps.cfg.max_response_bytes);

        let mut headers: Vec<(String, String)> = Vec::new();

        // Body serialization (A1 §B.7) sets default Content-Type / Accept before op.headers overrides.
        if let (Some(body_spec), Some(comp_body)) = (&op.body, &comp_op.body) {
            let (body_bytes, default_hdrs) = build_request_body(
                body_spec,
                comp_body,
                p_val,
                cfg_val,
                active_auth,
                cred,
                cx.replay_mode,
                &self.manifest.id,
            )?;
            req.body = Some(body_bytes.into());
            for (k, v) in default_hdrs {
                set_header_ci(&mut headers, k, v);
            }
        }

        // op.headers replace default headers case-insensitively (A1 §B.1 #2).
        for (hk, hv) in &op.headers {
            let val = expand_plain_template(hv, &tpl_ctx);
            set_header_ci(&mut headers, hk.clone(), val);
        }

        // Declared header params.
        for (pname, pdecl) in op.params.iter() {
            if pdecl.location != ParamLocation::Header {
                continue;
            }
            let Some(val) = resolved_params.get(pname).filter(|v| !v.is_null()) else {
                continue;
            };
            let formatted = format_param_scalar(val, pdecl, &tpl_ctx);
            set_header_ci(&mut headers, pdecl.wire_name(pname).to_owned(), formatted);
        }

        // Query parameters (only when using op template URL; LinkHeader follow URLs already carry full query).
        if is_template {
            // 1. const_query (sorted by key — BTreeMap is already sorted).
            for (k, cv) in &op.const_query {
                for v in cv.values() {
                    req.query
                        .push((k.clone(), expand_plain_template(v, &tpl_ctx)));
                }
            }

            // 2. Declared query params (sorted by param name per A1 §B.2 #4).
            let mut query_params: Vec<(&str, &ParamDecl)> = op
                .params
                .iter()
                .filter(|(_, d)| d.location == ParamLocation::Query)
                .collect();
            query_params.sort_by_key(|(name, _)| *name);

            for (pname, pdecl) in query_params {
                let Some(val) = resolved_params.get(pname).filter(|v| !v.is_null()) else {
                    continue;
                };
                let wire_key = pdecl.wire_name(pname);
                if pdecl.ty == ParamType::StringArray {
                    if let Some(arr) = val.as_array() {
                        let strs: Vec<String> = arr
                            .iter()
                            .filter_map(|v| v.as_str().map(str::to_owned))
                            .collect();
                        match pdecl.array_style() {
                            ArrayStyle::Repeat => {
                                for s in strs {
                                    req.query.push((wire_key.to_owned(), s));
                                }
                            }
                            ArrayStyle::Csv => {
                                if !strs.is_empty() {
                                    req.query.push((wire_key.to_owned(), strs.join(",")));
                                }
                            }
                            ArrayStyle::Brackets => {
                                let k = if wire_key.ends_with("[]") {
                                    wire_key.to_owned()
                                } else {
                                    format!("{wire_key}[]")
                                };
                                for s in strs {
                                    req.query.push((k.clone(), s));
                                }
                            }
                            _ => {
                                for s in strs {
                                    req.query.push((wire_key.to_owned(), s));
                                }
                            }
                        }
                    }
                } else {
                    let formatted = format_param_scalar(val, pdecl, &tpl_ctx);
                    req.query.push((wire_key.to_owned(), formatted));
                }
            }

            // 3. query_jaq pairs (A1 §A.4, §B.2 #4).
            if let Some(qj) = &comp_op.query_jaq {
                let out = qj.eval_one(p_val, p_val, cfg_val)?;
                if let Some(val) = out
                    && !val.is_null()
                {
                    let arr = val.as_array().ok_or_else(|| {
                        FsmError::invalid(format!(
                            "{}: query_jaq must return an array of [key, value] pairs",
                            self.manifest.id
                        ))
                    })?;
                    let auth_q_name = active_auth
                        .filter(|a| a.effective_in() == AuthIn::Query)
                        .map(AuthDecl::effective_name);
                    for entry in arr {
                        let pair = entry.as_array().ok_or_else(|| {
                            FsmError::invalid(format!(
                                "{}: query_jaq element must be a 2-element [key, value] array",
                                self.manifest.id
                            ))
                        })?;
                        if pair.len() != 2 {
                            return Err(FsmError::invalid(format!(
                                "{}: query_jaq element must have length 2, got {}",
                                self.manifest.id,
                                pair.len()
                            )));
                        }
                        let k = pair[0].as_str().ok_or_else(|| {
                            FsmError::invalid(format!(
                                "{}: query_jaq pair key must be a string",
                                self.manifest.id
                            ))
                        })?;
                        if let Some(ak) = auth_q_name
                            && k.eq_ignore_ascii_case(ak)
                        {
                            return Err(FsmError::invalid(format!(
                                "{}: query_jaq must not emit auth query parameter {ak:?}",
                                self.manifest.id
                            )));
                        }
                        if pair[1].is_null() {
                            continue;
                        }
                        let v_str = json_scalar_to_string(&pair[1]);
                        req.query.push((k.to_owned(), v_str));
                    }
                }
            }
        }

        // 4. Auth injection (header or query + redaction markers per A1 §B.9).
        if let (Some(auth), Some(token)) = (active_auth, cred) {
            apply_auth_to_request(auth, token, &mut req, &mut headers);
        }

        req.headers = headers;
        Ok(req)
    }

    fn default_hit_kind(&self, op: &OpSpec) -> HitKind {
        if self
            .manifest
            .scopes
            .iter()
            .any(|s| s == "academic" || s.starts_with("academic."))
        {
            HitKind::Academic
        } else if self.manifest.scopes.iter().any(|s| s == "news") {
            HitKind::News
        } else if self.manifest.scopes.iter().any(|s| s == "web") && op.returns == ReturnKind::Hits
        {
            HitKind::Web
        } else {
            HitKind::Record
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EffectiveAuth {
    None,
    Optional,
    Required,
}

enum PreOutcome {
    Bound(String),
    ShortCircuitEmpty,
}

#[async_trait::async_trait]
impl Source for ManifestSource {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    fn claims(&self, query: &str, _lang: Option<&str>) -> f32 {
        self.claims.as_ref().map_or(0.0, |c| c.score(query))
    }

    fn availability(&self, cfg: &Config) -> Availability {
        // A1 §B.1 #3: exact per-op effective auth availability rule.
        let Some(auth) = &self.manifest.auth else {
            return Availability::Available;
        };
        if self.has_credentials(cfg) {
            return Availability::Available;
        }
        let all_required = self
            .manifest
            .op
            .iter()
            .all(|(_, op)| self.effective_op_auth(op) == EffectiveAuth::Required);
        if all_required {
            Availability::NeedsKey {
                env: auth.env_display(),
            }
        } else {
            Availability::Available
        }
    }

    async fn call(&self, op_name: &str, params: Value, cx: &SrcCx) -> Result<SrcOutput, FsmError> {
        let op = self.manifest.op.get(op_name).ok_or_else(|| {
            FsmError::invalid(format!(
                "source {:?} has no op {:?}",
                self.manifest.id, op_name
            ))
        })?;
        let compiled = self.compiled()?;
        let comp_op = compiled.ops.get(op_name).ok_or_else(|| {
            FsmError::internal("manifest_compile", format!("missing compiled op {op_name}"))
        })?;

        // 1. Validate & default params.
        let mut resolved_params =
            validate_and_default_params(&self.manifest.id, op_name, op, &params)?;

        // Build $cfg object (A1 §G.4.3: contact_email, lang, region, freshness).
        let cfg_val = json!({
            "contact_email": cx.contact_email,
            "lang": cx.lang,
            "region": cx.region,
            "freshness": cx.freshness.map(freshness_as_str),
        });

        let timeout = if !cx.timeout.is_zero() {
            cx.timeout
        } else {
            Duration::from_millis(self.manifest.limits.timeout_ms.max(1000))
        };
        let mut ncx = NetCx::new(cx.cancel.clone(), timeout);
        ncx.host_bucket = Some(
            self.manifest
                .limits
                .host_bucket
                .clone()
                .unwrap_or_else(|| self.manifest.id.clone()),
        );
        ncx.rps = Some(self.manifest.limits.rps);

        // 2. Resolve auth credentials.
        let (mut cred, used_oauth2) = self.resolve_credential(op, cx, &ncx).await?;

        // 3. Optional pre-flight step (A1 §B.2).
        let p_val_initial = Value::Object(resolved_params.clone());
        let pre_bound = if let (Some(pre), Some(comp_pre)) = (op.pre.first(), &comp_op.pre) {
            let (outcome, updated_cred) = self
                .execute_pre(
                    op_name,
                    op,
                    pre,
                    comp_pre,
                    &resolved_params,
                    &p_val_initial,
                    &cfg_val,
                    cred.as_deref(),
                    used_oauth2,
                    cx,
                    &ncx,
                )
                .await?;
            cred = updated_cred;
            match outcome {
                PreOutcome::Bound(s) => Some(s),
                PreOutcome::ShortCircuitEmpty => return Ok(empty_output_for_op(op_name, op)),
            }
        } else {
            None
        };

        // 4. Execute main request (and optional multi-page loop when needed).
        let max_pages = op
            .paginate
            .as_ref()
            .and_then(|pg| pg.max_pages)
            .unwrap_or(1)
            .clamp(1, 5);
        let requested_limit: Option<usize> = resolved_params
            .get("limit")
            .and_then(Value::as_u64)
            .or_else(|| params.get("limit").and_then(Value::as_u64))
            .map(|n| n as usize);
        let limit_applies_to_page = op.params.get("limit").is_some_and(|d| {
            d.location != ParamLocation::None
                || op
                    .query_jaq
                    .as_deref()
                    .is_some_and(|q| q.contains("$p.limit"))
                || op.body.as_ref().is_some_and(|b| {
                    b.jaq.as_deref().is_some_and(|j| j.contains("$p.limit"))
                        || b.variables_jaq
                            .as_deref()
                            .is_some_and(|j| j.contains("$p.limit"))
                })
                || op
                    .map
                    .as_ref()
                    .is_some_and(|m| m.items.contains("$p.limit"))
        });
        let wire_page_size: Option<usize> = if limit_applies_to_page {
            resolved_params
                .get("limit")
                .and_then(Value::as_u64)
                .map(|n| n as usize)
        } else {
            None
        };

        let mut all_hits: Vec<Hit> = Vec::new();
        let mut all_works: Vec<ScholarlyWork> = Vec::new();
        let mut all_records: Vec<Value> = Vec::new();
        let mut final_next_cursor: Option<String> = None;
        let mut next_link_url: Option<String> = None;
        let mut seen_cursors: Vec<String> = Vec::new();
        let mut fetched_multi_page = false;

        // Track numeric page / offset cursor across iterations when PaginateStyle is Page or Offset.
        let mut cur_page_num: Option<u64> = None;
        let mut cur_offset_num: Option<u64> = None;

        for page_idx in 0..max_pages {
            if page_idx > 0 {
                fetched_multi_page = true;
            }
            let can_return_partial = page_idx > 0
                && !cx.cancel.is_cancelled()
                && (!all_hits.is_empty() || !all_works.is_empty() || !all_records.is_empty());

            let p_val = Value::Object(resolved_params.clone());
            let resp = match self
                .send_main_page(
                    op,
                    comp_op,
                    &resolved_params,
                    &p_val,
                    &cfg_val,
                    pre_bound.as_deref(),
                    next_link_url.as_deref(),
                    cred.as_deref(),
                    used_oauth2,
                    cx,
                    &ncx,
                )
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    if can_return_partial && !cx.cancel.is_cancelled() {
                        tracing::warn!(
                            source = %self.manifest.id,
                            op = %op_name,
                            page = page_idx + 1,
                            error = %e,
                            "R2: returning partial results after later page failed"
                        );
                        break;
                    }
                    return Err(e);
                }
            };

            if resp.truncated {
                let err = FsmError::MaxBytesExceeded(self.deps.cfg.max_response_bytes);
                if can_return_partial {
                    tracing::warn!(
                        source = %self.manifest.id,
                        op = %op_name,
                        page = page_idx + 1,
                        error = %err,
                        "R2: returning partial results after truncated later page"
                    );
                    break;
                }
                return Err(err);
            }

            if resp.status == 429 {
                let err = FsmError::RateLimited {
                    source_id: self.manifest.id.clone(),
                    retry_after_s: parse_retry_after(&resp),
                };
                if can_return_partial {
                    tracing::warn!(
                        source = %self.manifest.id,
                        op = %op_name,
                        page = page_idx + 1,
                        error = %err,
                        "R2: returning partial results after 429 on later page"
                    );
                    break;
                }
                return Err(err);
            }

            let decoded = decode_body(&resp);

            // Parse body per declared op.response (ignoring Content-Type per A1 §B.1 #1).
            let parsed_res = parse_response_body(
                op.response,
                &decoded,
                op.html.as_ref(),
                op.csv.as_ref(),
                &self.manifest.id,
                resp.status,
            );

            let root_val = match parsed_res {
                Ok(v) => v,
                Err(parse_err) => {
                    let err = if !resp.is_success() {
                        FsmError::upstream(
                            &self.manifest.id,
                            Some(resp.status),
                            first_n_chars(&decoded, 200),
                        )
                    } else {
                        parse_err
                    };
                    if can_return_partial {
                        tracing::warn!(
                            source = %self.manifest.id,
                            op = %op_name,
                            page = page_idx + 1,
                            error = %err,
                            "R2: returning partial results after parse/status error on later page"
                        );
                        break;
                    }
                    return Err(err);
                }
            };

            // A1 §B.1 #1: evaluate map.error BEFORE map.items, even on 4xx/5xx when JSON/XML parsed.
            if let Some(comp_map) = &comp_op.map
                && let Some(err_f) = &comp_map.error
                && let Some(err_val) = err_f.eval_one(&root_val, &p_val, &cfg_val)?
            {
                let msg = match &err_val {
                    Value::Null => None,
                    Value::String(s) if s.trim().is_empty() => None,
                    Value::String(s) => Some(s.clone()),
                    other => Some(other.to_string()),
                };
                if let Some(msg) = msg {
                    let err = FsmError::upstream(&self.manifest.id, Some(resp.status), msg);
                    if can_return_partial {
                        tracing::warn!(
                            source = %self.manifest.id,
                            op = %op_name,
                            page = page_idx + 1,
                            error = %err,
                            "R2: returning partial results after map.error on later page"
                        );
                        break;
                    }
                    return Err(err);
                }
            }

            if !resp.is_success() {
                let err = FsmError::upstream(
                    &self.manifest.id,
                    Some(resp.status),
                    first_n_chars(&decoded, 200),
                );
                if can_return_partial {
                    tracing::warn!(
                        source = %self.manifest.id,
                        op = %op_name,
                        page = page_idx + 1,
                        error = %err,
                        "R2: returning partial results after non-2xx on later page"
                    );
                    break;
                }
                return Err(err);
            }

            // Text / Raw / HTML-without-map short paths (single-page).
            match op.returns {
                ReturnKind::Text if comp_op.map.is_none() => {
                    return Ok(SrcOutput::Text(decoded));
                }
                ReturnKind::Raw if comp_op.map.is_none() => {
                    return Ok(SrcOutput::Raw(root_val));
                }
                ReturnKind::Records if comp_op.map.is_none() => {
                    // A1 §B.4: html without map returns rows directly.
                    let items = match root_val {
                        Value::Array(rows) => rows,
                        other => vec![other],
                    };
                    return Ok(SrcOutput::Records {
                        items,
                        hits: None,
                        next_cursor: None,
                    });
                }
                _ => {}
            }

            let comp_map = comp_op.map.as_ref().ok_or_else(|| {
                FsmError::invalid(format!(
                    "{}.{op_name}: missing [op.{op_name}.map]",
                    self.manifest.id
                ))
            })?;

            // Evaluate map.items as strict stream (A1 §B.5 — NO silent array unwrapping).
            let raw_items: Vec<Value> = comp_map
                .items
                .eval_stream(&root_val, &p_val, &cfg_val, MAX_JAQ_STREAM_ITEMS)?
                .into_iter()
                .filter(|v| !v.is_null())
                .collect();

            let page_count = raw_items.len();

            // Extract optional total count (used by Page/Offset pagination).
            let page_total: Option<usize> = comp_map
                .total
                .as_ref()
                .and_then(|tf| tf.eval_one(&root_val, &p_val, &cfg_val).ok().flatten())
                .and_then(|v| match v {
                    Value::Number(n) => n.as_u64().and_then(|u| usize::try_from(u).ok()),
                    Value::String(s) => s.trim().parse::<usize>().ok(),
                    _ => None,
                });

            // Extract next_cursor from map.next_cursor or Link header (resolving relative RFC 8288 URLs).
            let mut page_next_cursor: Option<String> = None;
            if let Some(nc_f) = &comp_map.next_cursor
                && let Some(nc_val) = nc_f.eval_one(&root_val, &p_val, &cfg_val)?
            {
                let s = match nc_val {
                    Value::Null => None,
                    Value::String(s) if s.trim().is_empty() => None,
                    Value::String(s) => Some(s),
                    Value::Number(n) => Some(n.to_string()),
                    Value::Bool(b) => Some(b.to_string()),
                    _ => None,
                };
                page_next_cursor = s;
            }
            if page_next_cursor.is_none()
                && op
                    .paginate
                    .as_ref()
                    .is_some_and(|pg| pg.style == PaginateStyle::LinkHeader)
                && let Some(link_hdr) = resp.header("link")
                && let Some(raw_next) = parse_link_next(link_hdr)
            {
                let resolved_next = Url::parse(&resp.final_url)
                    .or_else(|_| Url::parse(&op.url))
                    .ok()
                    .and_then(|base| base.join(&raw_next).ok())
                    .map_or(raw_next, |u| u.to_string());
                page_next_cursor = Some(resolved_next);
            }

            // Map each item according to op.returns.
            let default_kind = self.default_hit_kind(op);
            match op.returns {
                ReturnKind::Hits => {
                    let hit_f = comp_map.hit.as_ref().ok_or_else(|| {
                        FsmError::invalid(format!(
                            "{}.{op_name}: missing map.hit",
                            self.manifest.id
                        ))
                    })?;
                    for item in &raw_items {
                        if let Some(v) = hit_f.eval_one(item, &p_val, &cfg_val)?
                            && !v.is_null()
                        {
                            all_hits.push(value_to_hit(v, &self.manifest.id, default_kind)?);
                        }
                    }
                }
                ReturnKind::Works => {
                    let work_f = comp_map.work.as_ref().ok_or_else(|| {
                        FsmError::invalid(format!(
                            "{}.{op_name}: missing map.work",
                            self.manifest.id
                        ))
                    })?;
                    for item in &raw_items {
                        if let Some(v) = work_f.eval_one(item, &p_val, &cfg_val)?
                            && !v.is_null()
                        {
                            all_works.push(value_to_work(v, &self.manifest.id)?);
                        }
                    }
                }
                ReturnKind::Records => {
                    for item in &raw_items {
                        if let Some(rec_f) = &comp_map.record {
                            if let Some(v) = rec_f.eval_one(item, &p_val, &cfg_val)?
                                && !v.is_null()
                            {
                                all_records.push(v);
                            }
                        } else {
                            all_records.push(item.clone());
                        }
                        if let Some(hit_f) = &comp_map.hit
                            && let Some(v) = hit_f.eval_one(item, &p_val, &cfg_val)?
                            && !v.is_null()
                        {
                            all_hits.push(value_to_hit(v, &self.manifest.id, default_kind)?);
                        }
                    }
                }
                ReturnKind::Text => {
                    let mut parts = Vec::new();
                    for item in &raw_items {
                        let v = if let Some(rec_f) = &comp_map.record {
                            rec_f.eval_one(item, &p_val, &cfg_val)?
                        } else {
                            Some(item.clone())
                        };
                        if let Some(val) = v {
                            parts.push(match val {
                                Value::String(s) => s,
                                other => other.to_string(),
                            });
                        }
                    }
                    return Ok(SrcOutput::Text(parts.join("\n")));
                }
                ReturnKind::Raw => {
                    let mut mapped = Vec::new();
                    for item in &raw_items {
                        if let Some(rec_f) = &comp_map.record {
                            if let Some(v) = rec_f.eval_one(item, &p_val, &cfg_val)? {
                                mapped.push(v);
                            }
                        } else {
                            mapped.push(item.clone());
                        }
                    }
                    return Ok(SrcOutput::Raw(Value::Array(mapped)));
                }
                _ => {
                    return Ok(SrcOutput::Raw(root_val));
                }
            }

            final_next_cursor = page_next_cursor.clone();

            // Decide whether to fetch another page.
            let collected_len = match op.returns {
                ReturnKind::Hits => all_hits.len(),
                ReturnKind::Works => all_works.len(),
                ReturnKind::Records => all_records.len(),
                _ => page_count,
            };

            let need_more = requested_limit.is_some_and(|lim| collected_len < lim);
            let Some(pg) = &op.paginate else {
                break;
            };
            if page_idx + 1 >= max_pages || !need_more || page_count == 0 {
                break;
            }

            match pg.style {
                PaginateStyle::Cursor | PaginateStyle::Token => {
                    let Some(next_tok) = page_next_cursor else {
                        break;
                    };
                    if seen_cursors.contains(&next_tok) {
                        break;
                    }
                    seen_cursors.push(next_tok.clone());
                    let Some(param_name) = pg.param.as_deref() else {
                        break;
                    };
                    if let Some((decl_name, _)) = op
                        .params
                        .iter()
                        .find(|(n, d)| *n == param_name || d.wire_name(n) == param_name)
                    {
                        resolved_params.insert(decl_name.to_owned(), Value::String(next_tok));
                    } else {
                        break;
                    }
                }
                PaginateStyle::LinkHeader => {
                    let Some(next_url) = page_next_cursor else {
                        break;
                    };
                    if seen_cursors.contains(&next_url) {
                        break;
                    }
                    seen_cursors.push(next_url.clone());
                    next_link_url = Some(next_url);
                }
                PaginateStyle::Page => {
                    // Advance when this page filled any wire page size (or no wire limit param is sent)
                    // AND server total (when known) exceeds collected_len.
                    let full_page = wire_page_size.is_none_or(|sz| page_count >= sz);
                    let more_available = full_page && page_total.is_none_or(|t| collected_len < t);
                    if !more_available {
                        break;
                    }
                    let Some(param_name) = pg.param.as_deref() else {
                        break;
                    };
                    let Some((decl_name, _)) = op
                        .params
                        .iter()
                        .find(|(n, d)| *n == param_name || d.wire_name(n) == param_name)
                    else {
                        break;
                    };
                    let start = pg.start.unwrap_or(1) as u64;
                    let cur = cur_page_num.unwrap_or_else(|| {
                        resolved_params
                            .get(decl_name)
                            .and_then(Value::as_u64)
                            .unwrap_or(start)
                    });
                    let next = cur.saturating_add(1);
                    cur_page_num = Some(next);
                    resolved_params.insert(decl_name.to_owned(), Value::from(next));
                }
                PaginateStyle::Offset => {
                    let full_page = wire_page_size.is_none_or(|sz| page_count >= sz);
                    let more_available = full_page && page_total.is_none_or(|t| collected_len < t);
                    if !more_available {
                        break;
                    }
                    let Some(param_name) = pg.param.as_deref() else {
                        break;
                    };
                    let Some((decl_name, _)) = op
                        .params
                        .iter()
                        .find(|(n, d)| *n == param_name || d.wire_name(n) == param_name)
                    else {
                        break;
                    };
                    let start = pg.start.unwrap_or(0) as u64;
                    let cur = cur_offset_num.unwrap_or_else(|| {
                        resolved_params
                            .get(decl_name)
                            .and_then(Value::as_u64)
                            .unwrap_or(start)
                    });
                    let next = cur.saturating_add(page_count as u64);
                    cur_offset_num = Some(next);
                    resolved_params.insert(decl_name.to_owned(), Value::from(next));
                }
                _ => {
                    break;
                }
            }
        }

        // I5: never over-return beyond requested `limit` after multi-page fetching.
        if fetched_multi_page && let Some(lim) = requested_limit {
            all_hits.truncate(lim);
            all_works.truncate(lim);
            all_records.truncate(lim);
        }

        match op.returns {
            ReturnKind::Hits => Ok(SrcOutput::Hits(all_hits)),
            ReturnKind::Works => Ok(SrcOutput::Works(all_works)),
            ReturnKind::Records => {
                let hits = if comp_op.map.as_ref().and_then(|m| m.hit.as_ref()).is_some() {
                    Some(all_hits)
                } else {
                    None
                };
                Ok(SrcOutput::Records {
                    items: all_records,
                    hits,
                    next_cursor: final_next_cursor,
                })
            }
            ReturnKind::Text => Ok(SrcOutput::Text(String::new())),
            ReturnKind::Raw | _ => Ok(SrcOutput::Raw(Value::Null)),
        }
    }
}

// ---------------------------------------------------------------------------
// Compilation & validation helpers
// ---------------------------------------------------------------------------

fn compile_manifest(
    manifest: &ManifestDecl,
    is_user_override: bool,
) -> Result<CompiledManifest, String> {
    let compile_expr = |expr: &str| -> Result<CompiledFilter, FsmError> {
        if is_user_override {
            compile_user_filter(expr)
        } else {
            compile_filter(expr)
        }
    };

    let mut ops = HashMap::with_capacity(manifest.op.len());
    for (op_name, op) in manifest.op.iter() {
        let compile_opt =
            |label: &str, expr: Option<&str>| -> Result<Option<CompiledFilter>, String> {
                let Some(raw) = expr else {
                    return Ok(None);
                };
                let resolved = manifest
                    .resolve_ref(raw)
                    .map_err(|e| format!("op.{op_name}.{label}: {e}"))?;
                compile_expr(resolved)
                    .map(Some)
                    .map_err(|e| format!("op.{op_name}.{label}: {e}"))
            };

        let compile_body = |prefix: &str, b: &BodySpec| -> Result<CompiledBody, String> {
            Ok(CompiledBody {
                jaq: compile_opt(&format!("{prefix}.jaq"), b.jaq.as_deref())?,
                variables_jaq: compile_opt(
                    &format!("{prefix}.variables_jaq"),
                    b.variables_jaq.as_deref(),
                )?,
            })
        };

        let query_jaq = compile_opt("query_jaq", op.query_jaq.as_deref())?;
        let body = match &op.body {
            Some(b) => Some(compile_body("body", b)?),
            None => None,
        };
        let pre = match op.pre.first() {
            Some(p) => {
                let bind =
                    compile_expr(&p.bind).map_err(|e| format!("op.{op_name}.pre[0].bind: {e}"))?;
                let pre_body = match &p.body {
                    Some(b) => Some(compile_body("pre[0].body", b)?),
                    None => None,
                };
                Some(CompiledPre {
                    bind,
                    body: pre_body,
                })
            }
            None => None,
        };
        let map = match &op.map {
            Some(m) => {
                let resolved_items = manifest
                    .resolve_ref(&m.items)
                    .map_err(|e| format!("op.{op_name}.map.items: {e}"))?;
                let items = compile_expr(resolved_items)
                    .map_err(|e| format!("op.{op_name}.map.items: {e}"))?;
                Some(CompiledMap {
                    items,
                    next_cursor: compile_opt("map.next_cursor", m.next_cursor.as_deref())?,
                    total: compile_opt("map.total", m.total.as_deref())?,
                    error: compile_opt("map.error", m.error.as_deref())?,
                    work: compile_opt("map.work", m.work.as_deref())?,
                    record: compile_opt("map.record", m.record.as_deref())?,
                    hit: compile_opt("map.hit", m.hit.as_deref())?,
                })
            }
            None => None,
        };

        ops.insert(
            op_name.to_owned(),
            CompiledOp {
                query_jaq,
                body,
                pre,
                map,
            },
        );
    }
    Ok(CompiledManifest { ops })
}

fn validate_and_default_params(
    source_id: &str,
    op_name: &str,
    op: &OpSpec,
    input: &Value,
) -> Result<Map<String, Value>, FsmError> {
    let empty_map = Map::new();
    let in_obj = match input {
        Value::Object(m) => m,
        Value::Null => &empty_map,
        _ => {
            return Err(FsmError::invalid(format!(
                "{source_id}.{op_name}: params must be a JSON object"
            )));
        }
    };

    let mut out = Map::new();
    for (pname, pdecl) in op.params.iter() {
        let supplied = in_obj.get(pname).filter(|v| !v.is_null());
        let val = match supplied {
            Some(v) => {
                if !pdecl.ty.accepts(v) {
                    return Err(FsmError::invalid(format!(
                        "{source_id}.{op_name}: param {pname:?} expected {}, got {v}",
                        pdecl.ty.ts_name()
                    )));
                }
                if let Some(n) = v.as_f64() {
                    if let Some(lo) = pdecl.min
                        && n < lo
                    {
                        return Err(FsmError::invalid(format!(
                            "{source_id}.{op_name}: param {pname:?} value {n} < min {lo}"
                        )));
                    }
                    if let Some(hi) = pdecl.max
                        && n > hi
                    {
                        return Err(FsmError::invalid(format!(
                            "{source_id}.{op_name}: param {pname:?} value {n} > max {hi}"
                        )));
                    }
                }
                if let Some(allowed) = &pdecl.enum_values
                    && let Some(s) = v.as_str()
                    && !allowed.iter().any(|a| a == s)
                {
                    return Err(FsmError::invalid(format!(
                        "{source_id}.{op_name}: param {pname:?} value {s:?} not in enum {allowed:?}"
                    )));
                }
                Some(v.clone())
            }
            None => {
                if let Some(def) = &pdecl.default {
                    Some(def.clone())
                } else if pdecl.required {
                    return Err(FsmError::invalid(format!(
                        "{source_id}.{op_name}: missing required param {pname:?}"
                    )));
                } else {
                    None
                }
            }
        };

        if let Some(v) = val {
            out.insert(pname.to_owned(), v);
        }
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// Template expansion & path encoding guardrails (A1 §B.2, §B.3)
// ---------------------------------------------------------------------------

struct TemplateCx<'a> {
    op_params: &'a fsm_core::manifest::OrderedMap<ParamDecl>,
    resolved: &'a Map<String, Value>,
    pre_val: Option<&'a str>,
    key: Option<&'a str>,
    cx: &'a SrcCx,
}

impl TemplateCx<'_> {
    fn lookup(&self, name: &str) -> String {
        // Declared params shadow builtins (W0a dev 34).
        if self.op_params.contains_key(name) {
            return self
                .resolved
                .get(name)
                .map(json_scalar_to_string)
                .unwrap_or_default();
        }
        match name {
            "pre" => self.pre_val.unwrap_or("").to_owned(),
            "key" => self.key.unwrap_or("").to_owned(),
            "query" => self
                .resolved
                .get("query")
                .map(json_scalar_to_string)
                .unwrap_or_default(),
            "limit" => self
                .resolved
                .get("limit")
                .map(json_scalar_to_string)
                .unwrap_or_default(),
            "cursor" => self
                .resolved
                .get("cursor")
                .map(json_scalar_to_string)
                .unwrap_or_default(),
            "lang" => self
                .resolved
                .get("lang")
                .map(json_scalar_to_string)
                .or_else(|| self.cx.lang.clone())
                .unwrap_or_default(),
            "contact_email" => self.cx.contact_email.clone().unwrap_or_default(),
            _ => String::new(),
        }
    }
}

fn expand_plain_template(tmpl: &str, ctx: &TemplateCx<'_>) -> String {
    if !tmpl.contains('{') {
        return tmpl.to_owned();
    }
    let mut out = String::with_capacity(tmpl.len());
    let mut rest = tmpl;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after_open = &rest[open + 1..];
        if let Some(close) = after_open.find('}') {
            let var = &after_open[..close];
            if is_ident(var) {
                out.push_str(&ctx.lookup(var));
                rest = &after_open[close + 1..];
                continue;
            }
        }
        out.push('{');
        rest = after_open;
    }
    out.push_str(rest);
    out
}

fn expand_url_template(tmpl: &str, ctx: &TemplateCx<'_>) -> Result<String, FsmError> {
    if !tmpl.contains('{') {
        return Ok(tmpl.to_owned());
    }
    let q_pos = tmpl.find('?');
    let mut out = String::with_capacity(tmpl.len());
    let mut pos = 0;

    while let Some(rel_open) = tmpl[pos..].find('{') {
        let open = pos + rel_open;
        out.push_str(&tmpl[pos..open]);
        let after_open = open + 1;
        if let Some(rel_close) = tmpl[after_open..].find('}') {
            let close = after_open + rel_close;
            let var = &tmpl[after_open..close];
            if is_ident(var) {
                let raw = ctx.lookup(var);
                let in_query = q_pos.is_some_and(|q| open > q);
                if in_query {
                    out.push_str(&utf8_percent_encode(&raw, QUERY_VAL_ENCODE_SET).to_string());
                } else {
                    // I1: EVERY placeholder occurring before '?' MUST undergo path-encoding
                    // and traversal/query/fragment injection guardrails (including {pre}).
                    let mode = ctx
                        .op_params
                        .get(var)
                        .filter(|d| d.location == ParamLocation::Path)
                        .map_or(PathEncode::Raw, ParamDecl::path_encode);
                    let encoded = encode_path_param(var, &raw, mode)?;
                    out.push_str(&encoded);
                }
                pos = close + 1;
                continue;
            }
        }
        out.push('{');
        pos = after_open;
    }
    out.push_str(&tmpl[pos..]);
    Ok(out)
}

/// A1 §B.3 path parameter encoding + mandatory traversal & control-char guardrails.
pub fn encode_path_param(name: &str, raw: &str, mode: PathEncode) -> Result<String, FsmError> {
    if raw.contains(['\0', '\r', '\n', '\\']) {
        return Err(FsmError::invalid(format!(
            "path param {name:?} contains forbidden control character or backslash"
        )));
    }
    match mode {
        PathEncode::Segment => Ok(utf8_percent_encode(raw, SEGMENT_ENCODE_SET).to_string()),
        PathEncode::Raw => {
            if raw.starts_with("//") || raw.contains("://") {
                return Err(FsmError::invalid(format!(
                    "path param {name:?} must not start with '//' or contain '://'"
                )));
            }
            if raw.split('/').any(|seg| seg == "." || seg == "..") {
                return Err(FsmError::invalid(format!(
                    "path param {name:?} must not contain '.' or '..' traversal segments"
                )));
            }
            Ok(utf8_percent_encode(raw, RAW_PATH_ENCODE_SET).to_string())
        }
        _ => Ok(utf8_percent_encode(raw, SEGMENT_ENCODE_SET).to_string()),
    }
}

fn verify_same_origin_and_safe(
    template_url: &str,
    expanded_url: &str,
    net: &dyn fsm_core::NetClient,
) -> Result<(), FsmError> {
    // Build a syntactically valid template URL by replacing `{var}` with `"x"`.
    let mut dummy = template_url.to_owned();
    for var in placeholders(template_url) {
        dummy = dummy.replace(&format!("{{{var}}}"), "x");
    }
    let tmpl = Url::parse(&dummy).map_err(|e| {
        FsmError::invalid(format!(
            "manifest URL template {template_url:?} invalid: {e}"
        ))
    })?;
    let final_u = Url::parse(expanded_url)
        .map_err(|e| FsmError::invalid(format!("expanded URL {expanded_url:?} invalid: {e}")))?;

    if final_u.scheme() != tmpl.scheme()
        || final_u.host_str() != tmpl.host_str()
        || final_u.port_or_known_default() != tmpl.port_or_known_default()
    {
        return Err(FsmError::UnsafeUrl(format!(
            "expanded URL {expanded_url:?} changed origin from template {template_url:?}"
        )));
    }

    net.check_url_offline(final_u.as_str())
}

fn is_ident(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && (b[0].is_ascii_alphabetic() || b[0] == b'_')
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

fn is_unset_single_param_placeholder(
    tmpl: &str,
    op_params: &fsm_core::manifest::OrderedMap<ParamDecl>,
    resolved: &Map<String, Value>,
) -> bool {
    let t = tmpl.trim();
    if let Some(inner) = t.strip_prefix('{').and_then(|s| s.strip_suffix('}'))
        && is_ident(inner)
        && op_params.contains_key(inner)
    {
        return resolved.get(inner).is_none_or(Value::is_null);
    }
    false
}

fn format_param_scalar(val: &Value, pdecl: &ParamDecl, ctx: &TemplateCx<'_>) -> String {
    match &pdecl.template {
        Some(tmpl) => expand_plain_template(tmpl, ctx),
        None => json_scalar_to_string(val),
    }
}

fn json_scalar_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Body serialization (A1 §B.7) & Auth injection (A1 §B.9)
// ---------------------------------------------------------------------------

type BuiltBody = (Vec<u8>, Vec<(String, String)>);

#[allow(clippy::too_many_arguments)]
fn build_request_body(
    spec: &BodySpec,
    comp: &CompiledBody,
    p_val: &Value,
    cfg_val: &Value,
    auth: Option<&AuthDecl>,
    cred: Option<&str>,
    replay_mode: bool,
    source_id: &str,
) -> Result<BuiltBody, FsmError> {
    let mut default_headers = vec![(
        "Content-Type".to_owned(),
        spec.kind.content_type().to_owned(),
    )];

    match spec.kind {
        BodyKind::Json => {
            let f = comp
                .jaq
                .as_ref()
                .ok_or_else(|| FsmError::invalid(format!("{source_id}: body.json missing jaq")))?;
            let mut val = f.eval_one(p_val, p_val, cfg_val)?.unwrap_or(Value::Null);
            if !replay_mode
                && let (Some(a), Some(tok)) = (auth, cred)
                && a.effective_in() == AuthIn::Body
                && let Some(obj) = val.as_object_mut()
            {
                let formatted = a.effective_format().replace("{key}", tok);
                obj.insert(a.effective_name().to_owned(), Value::String(formatted));
            }
            let wire = canonical_json(&val);
            Ok((wire.into_bytes(), default_headers))
        }
        BodyKind::Form => {
            let f = comp
                .jaq
                .as_ref()
                .ok_or_else(|| FsmError::invalid(format!("{source_id}: body.form missing jaq")))?;
            let val = f.eval_one(p_val, p_val, cfg_val)?.unwrap_or(Value::Null);
            let obj = val.as_object().ok_or_else(|| {
                FsmError::invalid(format!(
                    "{source_id}: body.kind = \"form\" must evaluate to a JSON object"
                ))
            })?;
            let mut sorted: BTreeMap<String, Value> =
                obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            if !replay_mode
                && let (Some(a), Some(tok)) = (auth, cred)
                && a.effective_in() == AuthIn::Body
            {
                let formatted = a.effective_format().replace("{key}", tok);
                sorted.insert(a.effective_name().to_owned(), Value::String(formatted));
            }
            let mut ser = form_urlencoded::Serializer::new(String::new());
            for (k, v) in sorted {
                match v {
                    Value::Null => {}
                    Value::Array(items) => {
                        for item in items {
                            if !item.is_null() {
                                ser.append_pair(&k, &json_scalar_to_string(&item));
                            }
                        }
                    }
                    other => {
                        ser.append_pair(&k, &json_scalar_to_string(&other));
                    }
                }
            }
            Ok((ser.finish().into_bytes(), default_headers))
        }
        BodyKind::Sparql => {
            default_headers.push((
                "Accept".to_owned(),
                "application/sparql-results+json".to_owned(),
            ));
            let f = comp.jaq.as_ref().ok_or_else(|| {
                FsmError::invalid(format!("{source_id}: body.sparql missing jaq"))
            })?;
            let val = f.eval_one(p_val, p_val, cfg_val)?.unwrap_or(Value::Null);
            let q = val.as_str().ok_or_else(|| {
                FsmError::invalid(format!(
                    "{source_id}: body.kind = \"sparql\" jaq must output a string"
                ))
            })?;
            // A1 §B.7: raw unquoted UTF-8 bytes!
            Ok((q.as_bytes().to_vec(), default_headers))
        }
        BodyKind::Graphql => {
            default_headers.push(("Accept".to_owned(), "application/json".to_owned()));
            let query = spec.query.as_deref().ok_or_else(|| {
                FsmError::invalid(format!("{source_id}: body.graphql missing query"))
            })?;
            let vars = if let Some(vf) = &comp.variables_jaq {
                match vf.eval_one(p_val, p_val, cfg_val)? {
                    None | Some(Value::Null) => Value::Object(Map::new()),
                    Some(obj @ Value::Object(_)) => obj,
                    Some(_) => {
                        return Err(FsmError::invalid(format!(
                            "{source_id}: body.variables_jaq must evaluate to a JSON object"
                        )));
                    }
                }
            } else {
                Value::Object(Map::new())
            };
            let mut payload = Map::new();
            if let Some(op_name) = &spec.operation_name {
                payload.insert("operationName".to_owned(), Value::String(op_name.clone()));
            }
            payload.insert("query".to_owned(), Value::String(query.to_owned()));
            payload.insert("variables".to_owned(), vars);
            let wire = canonical_json(&Value::Object(payload));
            Ok((wire.into_bytes(), default_headers))
        }
        _ => Err(FsmError::invalid(format!(
            "{source_id}: unsupported body kind"
        ))),
    }
}

fn apply_auth_to_request(
    auth: &AuthDecl,
    token: &str,
    req: &mut HttpRequest,
    headers: &mut Vec<(String, String)>,
) {
    let name = auth.effective_name();
    let formatted = auth.effective_format().replace("{key}", token);
    match auth.effective_in() {
        AuthIn::Header => {
            set_header_ci(headers, name.to_owned(), formatted);
            req.redact_headers.push(name.to_owned());
        }
        AuthIn::Query => {
            req.query.push((name.to_owned(), formatted));
            req.redact_query_keys.push(name.to_owned());
        }
        AuthIn::Body | _ => {}
    }
}

fn set_header_ci(headers: &mut Vec<(String, String)>, name: String, value: String) {
    headers.retain(|(k, _)| !k.eq_ignore_ascii_case(&name));
    headers.push((name, value));
}

// ---------------------------------------------------------------------------
// Response decoding, parsing, and output mapping
// ---------------------------------------------------------------------------

fn decode_body(resp: &HttpBytes) -> String {
    if let Some(ct) = resp.header("content-type")
        && let Some(charset) = extract_charset(ct)
        && let Some(enc) = encoding_rs::Encoding::for_label(charset.as_bytes())
    {
        let (cow, _, _) = enc.decode(&resp.body);
        return cow.into_owned();
    }
    String::from_utf8_lossy(&resp.body).into_owned()
}

fn extract_charset(content_type: &str) -> Option<&str> {
    for part in content_type.split(';').skip(1) {
        let part = part.trim();
        if let Some((k, v)) = part.split_once('=')
            && k.trim().eq_ignore_ascii_case("charset")
        {
            return Some(v.trim().trim_matches('"').trim_matches('\''));
        }
    }
    None
}

fn parse_response_body(
    format: ResponseFormat,
    decoded: &str,
    html_spec: Option<&fsm_core::manifest::HtmlSpec>,
    csv_opts: Option<&fsm_core::manifest::CsvOpts>,
    source_id: &str,
    status: u16,
) -> Result<Value, FsmError> {
    match format {
        ResponseFormat::Json => serde_json::from_str(decoded)
            .map_err(|e| FsmError::upstream(source_id, Some(status), format!("invalid JSON: {e}"))),
        ResponseFormat::Xml => xml_to_json(decoded, source_id),
        ResponseFormat::Html => {
            let spec = html_spec.ok_or_else(|| {
                FsmError::invalid(format!(
                    "{source_id}: response = \"html\" requires [op.html]"
                ))
            })?;
            extract_html(decoded, spec, source_id, status)
        }
        ResponseFormat::Csv => csv_to_json(decoded, csv_opts, source_id),
        ResponseFormat::Ndjson => {
            let mut rows = Vec::new();
            for (idx, line) in decoded.lines().enumerate() {
                let t = line.trim();
                if t.is_empty() {
                    continue;
                }
                let val: Value = serde_json::from_str(t).map_err(|e| {
                    FsmError::upstream(
                        source_id,
                        Some(status),
                        format!("invalid NDJSON line {}: {e}", idx + 1),
                    )
                })?;
                rows.push(val);
            }
            Ok(Value::Array(rows))
        }
        ResponseFormat::Text | _ => Ok(Value::String(decoded.to_owned())),
    }
}

fn value_to_hit(v: Value, source_id: &str, default_kind: HitKind) -> Result<Hit, FsmError> {
    let has_explicit_kind = v.get("kind").is_some_and(|k| !k.is_null());
    let mut hit: Hit = serde_json::from_value(v.clone()).map_err(|e| {
        FsmError::upstream(
            source_id,
            None,
            format!("map.hit does not deserialize into Hit: {e} (value: {v})"),
        )
    })?;
    hit.title = hit.title.trim().to_owned();
    hit.snippet = hit.snippet.trim().to_owned();
    if hit.date.as_deref().is_none_or(|d| d.trim().is_empty()) {
        hit.date = None;
        hit.date_source = None;
    }
    if hit.source.is_empty() {
        hit.source = source_id.to_owned();
    }
    if hit.sources.is_empty() {
        hit.sources = vec![source_id.to_owned()];
    }
    if !has_explicit_kind {
        hit.kind = default_kind;
    }
    Ok(hit)
}

fn value_to_work(v: Value, source_id: &str) -> Result<ScholarlyWork, FsmError> {
    let mut work: ScholarlyWork = serde_json::from_value(v.clone()).map_err(|e| {
        FsmError::upstream(
            source_id,
            None,
            format!("map.work does not deserialize into ScholarlyWork: {e} (value: {v})"),
        )
    })?;
    if work.sources.is_empty() {
        work.sources = vec![source_id.to_owned()];
    }
    Ok(work)
}

fn empty_output_for_op(op_name: &str, op: &OpSpec) -> SrcOutput {
    match op.returns {
        ReturnKind::Hits => SrcOutput::Hits(Vec::new()),
        ReturnKind::Works => SrcOutput::Works(Vec::new()),
        ReturnKind::Records => SrcOutput::Records {
            items: Vec::new(),
            hits: (op_name == "search").then(Vec::new),
            next_cursor: None,
        },
        ReturnKind::Text => SrcOutput::Text(String::new()),
        ReturnKind::Raw | _ => SrcOutput::Raw(Value::Null),
    }
}

fn parse_retry_after(resp: &HttpBytes) -> Option<u64> {
    resp.header("retry-after")
        .and_then(|v| v.trim().parse::<u64>().ok())
}

fn parse_link_next(header: &str) -> Option<String> {
    for part in header.split(',') {
        let mut segments = part.split(';');
        let Some(url_seg) = segments.next().map(str::trim) else {
            continue;
        };
        let is_next = segments.any(|s| {
            let s = s.trim();
            s.eq_ignore_ascii_case("rel=\"next\"") || s.eq_ignore_ascii_case("rel=next")
        });
        if is_next && let Some(url) = url_seg.strip_prefix('<').and_then(|s| s.strip_suffix('>')) {
            return Some(url.to_owned());
        }
    }
    None
}

fn freshness_as_str(f: Freshness) -> &'static str {
    match f {
        Freshness::Day => "day",
        Freshness::Week => "week",
        Freshness::Month => "month",
        Freshness::Year => "year",
        _ => "week",
    }
}

fn first_n_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}
