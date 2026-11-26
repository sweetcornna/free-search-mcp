//! GitHub engines: `github` (repositories + issues, keyless / optional token)
//! and `github_code` (code search, requires `SEARCH_MCP_GITHUB_TOKEN`).

use crate::common::{
    CallBudget, SNIPPET_CAP, clip, effective_freshness, is_fatal_fanout_error, iso_date, make_hit,
    parse_search_params, resolve_optional_key, resolve_required_key, search_op, send_json_keyed,
    send_json_keyless,
};
use async_trait::async_trait;
use fsm_core::{
    AuthMode, Availability, Capability, Config, FsmError, Hit, HitKind, HttpRequest, PoolRole,
    ProbeSpec, Source, SourceDecl, SourceDeps, SourceLimits, SrcCx, SrcOutput, TransportProfile,
};
use serde_json::{Value, json};

pub const GITHUB_API: &str = "https://api.github.com";
pub const GITHUB_ACCEPT: &str = "application/vnd.github+json";
pub const GITHUB_API_VERSION: &str = "2022-11-28";
pub const GITHUB_TOKEN_ENV: &str = "SEARCH_MCP_GITHUB_TOKEN";

/// Build a GitHub REST API GET request with optional redacted Bearer token.
pub fn github_get(url: impl Into<String>, token: Option<&str>, source_id: &str) -> HttpRequest {
    let mut req = HttpRequest::get(url)
        .header("Accept", GITHUB_ACCEPT)
        .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
        .for_source(source_id);
    if let Some(tok) = token.filter(|t| !t.trim().is_empty()) {
        req = req
            .header("Authorization", format!("Bearer {tok}"))
            .redact_header("Authorization");
    }
    req
}

fn github_limits() -> SourceLimits {
    let mut limits = SourceLimits::new(0.5, 2, 2, 4000, 21_600);
    limits.host_bucket = Some("api.github.com".to_owned());
    limits
}

/// Keyless (optional token) GitHub repository and issue search.
pub struct GitHubEngine {
    decl: SourceDecl,
    deps: SourceDeps,
}

impl GitHubEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let mut decl = SourceDecl::new(
            "github",
            "GitHub",
            "GitHub repository and issue search (keyless; optional token raises rate limit).",
        )
        .with_scopes(&["code"])
        .with_caps(&[Capability::Search])
        .with_op(
            search_op(
                "Search GitHub repositories and issues.",
                100,
                r#"source.call("github.search", { query: "model context protocol", limit: 5 })"#,
            )
            .with_probe(ProbeSpec::new(
                json!({"query": "model context protocol", "limit": 3}),
                1,
            )),
        );
        decl.aliases = vec!["gh".to_owned(), "github repos".to_owned()];
        decl.home_url = Some("https://github.com".to_owned());
        decl.auth = AuthMode::OptionalKey {
            env: GITHUB_TOKEN_ENV.to_owned(),
            route_requires_key: false,
        };
        decl.transport = TransportProfile::Plain;
        decl.pool_role = PoolRole::ScopeOnly;
        decl.limits = github_limits();
        Self { decl, deps }
    }

    /// Map a `/search/repositories` JSON response to hits.
    pub fn map_repos(payload: &Value) -> Vec<Hit> {
        let Some(items) = payload.get("items").and_then(Value::as_array) else {
            return Vec::new();
        };
        let mut results = Vec::new();
        for item in items {
            let Some(obj) = item.as_object() else {
                continue;
            };
            let Some(name) = obj.get("full_name").and_then(Value::as_str) else {
                continue;
            };
            let Some(url) = obj
                .get("html_url")
                .and_then(Value::as_str)
                .filter(|u| !u.is_empty())
            else {
                continue;
            };
            let mut bits = Vec::new();
            if let Some(stars) = obj.get("stargazers_count").and_then(Value::as_i64) {
                bits.push(format!("★{stars}"));
            }
            if let Some(lang) = obj
                .get("language")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                bits.push(lang.to_owned());
            }
            if let Some(desc) = obj
                .get("description")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                bits.push(desc.to_owned());
            }
            // pushed_at answers "is this alive?" better than created_at.
            let pushed = iso_date(obj.get("pushed_at"));
            results.push(make_hit(
                "github",
                name.to_owned(),
                url.to_owned(),
                clip(&bits.join(" · "), SNIPPET_CAP),
                (!pushed.is_empty()).then_some(pushed),
                Some("api"),
                HitKind::Vertical,
            ));
        }
        results
    }

    /// Map a `/search/issues` JSON response (issues and pull requests) to hits.
    pub fn map_issues(payload: &Value) -> Vec<Hit> {
        let Some(items) = payload.get("items").and_then(Value::as_array) else {
            return Vec::new();
        };
        let mut results = Vec::new();
        for item in items {
            let Some(obj) = item.as_object() else {
                continue;
            };
            let Some(title) = obj.get("title").and_then(Value::as_str) else {
                continue;
            };
            let Some(url) = obj
                .get("html_url")
                .and_then(Value::as_str)
                .filter(|u| !u.is_empty())
            else {
                continue;
            };
            let mut bits = Vec::new();
            let kind = if obj.get("pull_request").is_some_and(Value::is_object) {
                "PR"
            } else {
                "issue"
            };
            if let Some(state) = obj.get("state").and_then(Value::as_str) {
                bits.push(format!("{kind} {state}"));
            } else {
                bits.push(kind.to_owned());
            }
            if let Some(comments) = obj
                .get("comments")
                .and_then(Value::as_i64)
                .filter(|&c| c > 0)
            {
                bits.push(format!("{comments} comments"));
            }
            if let Some(body) = obj
                .get("body")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                bits.push(body.to_owned());
            }
            let created = iso_date(obj.get("created_at"));
            results.push(make_hit(
                "github",
                clip(title, 300),
                url.to_owned(),
                clip(&bits.join(" · "), SNIPPET_CAP),
                (!created.is_empty()).then_some(created),
                Some("api"),
                HitKind::Vertical,
            ));
        }
        results
    }
}

#[async_trait]
impl Source for GitHubEngine {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    async fn call(&self, op: &str, params: Value, cx: &SrcCx) -> Result<SrcOutput, FsmError> {
        let (query, limit) = parse_search_params("github", op, &params, 10)?;
        if query.trim().is_empty() {
            return Ok(SrcOutput::Hits(Vec::new()));
        }
        let per_page = limit.clamp(1, 100);
        let token = resolve_optional_key(cx, &self.deps.cfg, GITHUB_TOKEN_ENV);
        let freshness = effective_freshness(&params, cx);
        let budget = CallBudget::from_cx(cx, &self.decl);

        let mut repo_req = github_get(
            format!("{GITHUB_API}/search/repositories"),
            token.as_deref(),
            "github",
        )
        .query_param("q", query)
        .query_param("per_page", per_page.to_string());
        if freshness.is_some() {
            repo_req = repo_req
                .query_param("sort", "updated")
                .query_param("order", "desc");
        }

        let mut results = Vec::new();
        let mut first_err: Option<FsmError> = None;
        match send_json_keyless(
            self.deps.net.as_ref(),
            repo_req,
            self.decl.transport,
            &self.decl,
            cx,
            &budget,
        )
        .await
        {
            Ok(Some(payload)) => results.extend(Self::map_repos(&payload)),
            Ok(None) => {}
            Err(e) if is_fatal_fanout_error(&e) => return Err(e),
            Err(e) => first_err = Some(e),
        }

        // Sequential on purpose: skip issues call when repositories fill the budget
        // or when the shared GitHub bucket already returned 429.
        if results.len() < limit && !matches!(first_err, Some(FsmError::RateLimited { .. })) {
            let remaining = (limit - results.len()).clamp(1, 100);
            let issues_req = github_get(
                format!("{GITHUB_API}/search/issues"),
                token.as_deref(),
                "github",
            )
            .query_param("q", query)
            .query_param("per_page", remaining.to_string());
            match send_json_keyless(
                self.deps.net.as_ref(),
                issues_req,
                self.decl.transport,
                &self.decl,
                cx,
                &budget,
            )
            .await
            {
                Ok(Some(payload)) => results.extend(Self::map_issues(&payload)),
                Ok(None) => {}
                Err(e) if is_fatal_fanout_error(&e) => return Err(e),
                Err(e) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
            }
        }

        if results.is_empty()
            && let Some(err) = first_err
        {
            return Err(err);
        }

        results.truncate(limit);
        Ok(SrcOutput::Hits(results))
    }
}

/// Keyed GitHub code-content search (requires `SEARCH_MCP_GITHUB_TOKEN`).
pub struct GitHubCodeEngine {
    decl: SourceDecl,
    deps: SourceDeps,
}

impl GitHubCodeEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let mut decl = SourceDecl::new(
            "github_code",
            "GitHub Code",
            "GitHub code search across public repositories (needs SEARCH_MCP_GITHUB_TOKEN).",
        )
        .with_scopes(&["code"])
        .with_caps(&[Capability::Search])
        .with_op(search_op(
            "Search source code across public GitHub repositories.",
            100,
            r#"source.call("github_code.search", { query: "asyncio.run", limit: 5 })"#,
        ));
        decl.aliases = vec!["gh code".to_owned()];
        decl.home_url = Some("https://github.com/search?type=code".to_owned());
        decl.auth = AuthMode::RequiredKey {
            env: GITHUB_TOKEN_ENV.to_owned(),
        };
        decl.transport = TransportProfile::Plain;
        decl.pool_role = PoolRole::ScopeOnly;
        decl.limits = github_limits();
        Self { decl, deps }
    }

    /// Map a `/search/code` JSON response to hits.
    pub fn map_results(payload: &Value) -> Vec<Hit> {
        let Some(items) = payload.get("items").and_then(Value::as_array) else {
            return Vec::new();
        };
        let mut results = Vec::new();
        for item in items {
            let Some(obj) = item.as_object() else {
                continue;
            };
            let Some(path) = obj.get("path").and_then(Value::as_str) else {
                continue;
            };
            let Some(url) = obj
                .get("html_url")
                .and_then(Value::as_str)
                .filter(|u| !u.is_empty())
            else {
                continue;
            };
            let repo = obj.get("repository").and_then(Value::as_object);
            let repo_name = repo
                .and_then(|r| r.get("full_name"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let desc = repo
                .and_then(|r| r.get("description"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let title = if repo_name.is_empty() {
                path.to_owned()
            } else {
                format!("{repo_name}/{path}")
            };
            results.push(make_hit(
                "github_code",
                title,
                url.to_owned(),
                clip(desc, SNIPPET_CAP),
                None,
                None,
                HitKind::Vertical,
            ));
        }
        results
    }
}

#[async_trait]
impl Source for GitHubCodeEngine {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    fn availability(&self, cfg: &Config) -> Availability {
        if cfg
            .api_key(GITHUB_TOKEN_ENV)
            .is_some_and(|k| !k.trim().is_empty())
        {
            Availability::Available
        } else {
            Availability::NeedsKey {
                env: GITHUB_TOKEN_ENV.to_owned(),
            }
        }
    }

    async fn call(&self, op: &str, params: Value, cx: &SrcCx) -> Result<SrcOutput, FsmError> {
        let (query, limit) = parse_search_params("github_code", op, &params, 10)?;
        let token = resolve_required_key("github_code", GITHUB_TOKEN_ENV, cx, &self.deps.cfg)?;
        if query.trim().is_empty() {
            return Ok(SrcOutput::Hits(Vec::new()));
        }
        let per_page = limit.clamp(1, 100);
        let req = github_get(
            format!("{GITHUB_API}/search/code"),
            Some(&token),
            "github_code",
        )
        .query_param("q", query)
        .query_param("per_page", per_page.to_string());

        let budget = CallBudget::from_cx(cx, &self.decl);
        let payload = send_json_keyed(
            self.deps.net.as_ref(),
            req,
            self.decl.transport,
            &self.decl,
            GITHUB_TOKEN_ENV,
            cx,
            &budget,
        )
        .await?;
        let mut results = Self::map_results(&payload);
        results.truncate(limit);
        Ok(SrcOutput::Hits(results))
    }
}
