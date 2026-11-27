//! Shared implementation for explicitly configured search instances.

use crate::common::{
    CallBudget, effective_domains, effective_freshness, filter_and_truncate_hits, make_hit,
    parse_search_params, retry_after_seconds, search_op, strip_html_and_unescape,
};
use fsm_core::api::Freshness;
use fsm_core::{
    AuthMode, Capability, FsmError, HitKind, HttpRequest, PoolRole, SourceDecl, SourceDeps,
    SourceLimits, SrcCx, SrcOutput, TransportProfile,
};
use serde_json::Value;
use std::collections::HashSet;
use url::Url;

pub(crate) struct InstanceSearch {
    pub(crate) decl: SourceDecl,
    deps: SourceDeps,
    base_url: String,
    key_env: &'static str,
    degoog: bool,
}

impl InstanceSearch {
    pub(crate) fn new(deps: SourceDeps, base_url: String, degoog: bool) -> Self {
        let (id, name, key_env) = if degoog {
            ("degoog", "Degoog", "SEARCH_MCP_DEGOOG_API_KEY")
        } else {
            ("searxng", "SearXNG", "SEARCH_MCP_SEARXNG_TOKEN")
        };
        let mut decl = SourceDecl::new(
            id,
            name,
            "Operator-configured search instance; explicit opt-in only.",
        )
        .with_scopes(&["web"])
        .with_caps(&[Capability::Search])
        .with_op(search_op(
            "Search the configured instance.",
            100,
            &format!("source.call(\"{id}.search\", {{query: \"rust\"}})"),
        ));
        decl.pool_role = PoolRole::Explicit;
        decl.transport = TransportProfile::Plain;
        decl.auth = AuthMode::OptionalKey {
            env: key_env.to_owned(),
            route_requires_key: false,
        };
        decl.limits = SourceLimits::new(1.0, 2, 2, 5000, 21600);
        Self {
            decl,
            deps,
            base_url,
            key_env,
            degoog,
        }
    }

    pub(crate) async fn call(
        &self,
        op: &str,
        params: Value,
        cx: &SrcCx,
    ) -> Result<SrcOutput, FsmError> {
        let (query, limit) = parse_search_params(&self.decl.id, op, &params, 10)?;
        // Only the constructor may select the destination; never accept call-time endpoints or keys.
        let mut url = Url::parse(self.base_url.trim())
            .map_err(|_| FsmError::invalid("invalid configured search instance URL"))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || url.query().is_some()
        {
            return Err(FsmError::invalid(
                "search instance URL must be HTTP(S), without userinfo, query, or fragment",
            ));
        }
        let key = self
            .deps
            .cfg
            .api_key(self.key_env)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if key.is_some() && url.scheme() != "https" {
            return Err(FsmError::invalid(
                "search instance credentials require HTTPS",
            ));
        }
        if key.is_some_and(|s| s.contains('\r') || s.contains('\n')) {
            return Err(FsmError::invalid("invalid search instance credential"));
        }
        let root = url.path().trim_end_matches('/');
        let path = if self.degoog {
            if root.ends_with("/api") {
                format!("{root}/search")
            } else {
                format!("{root}/api/search")
            }
        } else {
            format!("{root}/search")
        };
        url.set_path(&path);
        self.deps.net.check_url_offline(url.as_str())?;
        if query.trim().is_empty() {
            return Ok(SrcOutput::Hits(Vec::new()));
        }
        let mut req = HttpRequest::get(url.as_str())
            .for_source(&self.decl.id)
            .header("Accept", "application/json")
            .query_param("q", query);
        req.follow_redirects = false;
        req = if self.degoog {
            req.query_param("type", "web")
        } else {
            req.query_param("format", "json")
        };
        if let Some(f) = effective_freshness(&params, cx) {
            let time = match f {
                Freshness::Day => Some("day"),
                Freshness::Week => Some("week"),
                Freshness::Month => Some("month"),
                Freshness::Year if self.degoog => Some("year"),
                _ => None,
            };
            if let Some(time) = time {
                req = req.query_param(if self.degoog { "time" } else { "time_range" }, time);
            }
        }
        if let Some(key) = key {
            req = req
                .header("Authorization", format!("Bearer {key}"))
                .redact_header("Authorization");
        }
        let budget = CallBudget::from_cx(cx, &self.decl);
        let ncx = budget.net_cx(cx, &self.decl)?;
        let resp = self
            .deps
            .net
            .send(req, TransportProfile::Plain, &ncx)
            .await?;
        if resp.status == 429 {
            return Err(FsmError::RateLimited {
                source_id: self.decl.id.clone(),
                retry_after_s: retry_after_seconds(&resp),
            });
        }
        // Do not echo upstream bodies: a server can reflect the credential.
        if !resp.is_success() {
            return Err(FsmError::upstream(
                &self.decl.id,
                Some(resp.status),
                "search instance rejected request",
            ));
        }
        let payload: Value = serde_json::from_slice(&resp.body).map_err(|_| {
            FsmError::upstream(
                &self.decl.id,
                Some(resp.status),
                "invalid search instance JSON",
            )
        })?;
        let rows = payload
            .get("results")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                FsmError::upstream(
                    &self.decl.id,
                    Some(resp.status),
                    "missing search results array",
                )
            })?;
        let mut seen = HashSet::new();
        let mut hits = Vec::new();
        for row in rows {
            let Some(raw) = row.get("url").and_then(Value::as_str) else {
                continue;
            };
            let Ok(result_url) = Url::parse(raw.trim()) else {
                continue;
            };
            if !matches!(result_url.scheme(), "http" | "https")
                || result_url.host_str().is_none()
                || !result_url.username().is_empty()
                || result_url.password().is_some()
            {
                continue;
            }
            let result_url = result_url.to_string();
            if !seen.insert(result_url.clone()) {
                continue;
            }
            let title = strip_html_and_unescape(
                row.get("title")
                    .and_then(Value::as_str)
                    .unwrap_or(&result_url),
            );
            let text = if self.degoog {
                row.get("snippet")
                    .and_then(Value::as_str)
                    .or_else(|| row.get("content").and_then(Value::as_str))
            } else {
                row.get("content").and_then(Value::as_str)
            };
            hits.push(make_hit(
                &self.decl.id,
                title,
                result_url,
                strip_html_and_unescape(text.unwrap_or("")),
                None,
                None,
                HitKind::Web,
            ));
        }
        let (include, exclude) = effective_domains(&params, cx);
        Ok(SrcOutput::Hits(filter_and_truncate_hits(
            hits,
            &include,
            &exclude,
            limit.min(100),
        )))
    }
}
