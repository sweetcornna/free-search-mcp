//! GitHub Releases engine: latest release tag, date, and notes of a repository.

use crate::common::{
    CallBudget, SNIPPET_CAP, clip, collapse_ws, is_fatal_fanout_error, iso_day, make_hit,
    parse_search_params, resolve_optional_key, search_op, send_json_keyless,
};
use crate::facts::{STOP_WORDS, candidate_names, version_question};
use crate::github::{GITHUB_API, GITHUB_TOKEN_ENV, github_get};
use async_trait::async_trait;
use fsm_core::{
    AuthMode, Capability, FsmError, Hit, HitKind, PoolRole, ProbeSpec, Source, SourceDecl,
    SourceDeps, SourceLimits, SrcCx, SrcOutput, TransportProfile,
};
use futures::future::join_all;
use regex::Regex;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::LazyLock;

static REPO_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b([A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?)/([A-Za-z0-9._-]+)\b").expect("regex")
});

static GITHUB_OR_RELEASE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)github|release").expect("regex"));

static COMMIT_HASH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[0-9a-f]{40}\b").expect("regex"));

static MD_NOISE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[#*`>_\[\]()-]+").expect("regex"));

fn plain_name(name: &str) -> String {
    name.to_ascii_lowercase()
        .chars()
        .filter(|&c| !matches!(c, '-' | '_' | '.'))
        .collect()
}

/// Whether `repo_name` matches one of `candidates` (or their concatenation, e.g. `vs` + `code` -> `vscode`).
pub fn names_repo(repo_name: &str, candidates: &[String]) -> bool {
    let plain = plain_name(repo_name);
    let mut wanted: HashSet<String> = candidates.iter().map(|n| plain_name(n)).collect();
    if candidates.len() > 1 {
        let joined: String = candidates.iter().map(|n| plain_name(n)).collect();
        wanted.insert(joined);
    }
    wanted.contains(&plain)
}

/// Build the repository search `q` term with stop words removed.
pub fn repo_search_terms(query: &str) -> String {
    let kept: Vec<&str> = query
        .split_whitespace()
        .filter(|w| !STOP_WORDS.contains(w.to_ascii_lowercase().as_str()))
        .collect();
    if kept.is_empty() {
        query.to_owned()
    } else {
        kept.join(" ")
    }
}

/// Latest GitHub release of repositories named or described by a query.
pub struct GitHubReleasesEngine {
    decl: SourceDecl,
    deps: SourceDeps,
}

impl GitHubReleasesEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let mut decl = SourceDecl::new(
            "github_releases",
            "GitHub Releases",
            "GitHub releases: the newest release tag, date and notes of a repository.",
        )
        .with_scopes(&["code"])
        .with_caps(&[Capability::Search, Capability::Lookup])
        .with_op(
            search_op(
                "Look up the latest GitHub release for a named repository.",
                10,
                r#"source.call("github_releases.search", { query: "astral-sh/uv latest release" })"#,
            )
            .with_probe(ProbeSpec::new(
                json!({"query": "astral-sh/uv latest release", "limit": 1}),
                1,
            )),
        );
        decl.aliases = vec!["gh releases".to_owned(), "releases".to_owned()];
        decl.home_url = Some("https://github.com".to_owned());
        decl.auth = AuthMode::OptionalKey {
            env: GITHUB_TOKEN_ENV.to_owned(),
            route_requires_key: false,
        };
        decl.transport = TransportProfile::Plain;
        decl.pool_role = PoolRole::DirectClaimant;
        let mut limits = SourceLimits::new(0.5, 2, 2, 4000, 21_600);
        limits.host_bucket = Some("api.github.com".to_owned());
        decl.limits = limits;
        Self { decl, deps }
    }

    /// Map a single `/repos/{owner}/{repo}/releases/latest` JSON payload to a hit.
    pub fn map_release(repo_hint: &str, payload: &Value) -> Option<Hit> {
        let obj = payload.as_object()?;
        let tag = obj.get("tag_name").and_then(Value::as_str)?;
        let url = obj
            .get("html_url")
            .and_then(Value::as_str)
            .filter(|u| u.starts_with("https://github.com/"))?;
        let repo = if !repo_hint.is_empty() {
            repo_hint.to_owned()
        } else {
            let parts: Vec<&str> = url.split('/').collect();
            if parts.len() >= 5 {
                format!("{}/{}", parts[3], parts[4])
            } else {
                String::new()
            }
        };
        let published = iso_day(obj.get("published_at"));
        let mut bits = Vec::new();
        if published.is_empty() {
            bits.push(format!("Latest release {tag}"));
        } else {
            bits.push(format!("Latest release {tag}, published {published}"));
        }
        if obj
            .get("prerelease")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            bits.push("marked as a pre-release".to_owned());
        }
        if let Some(release_name) = obj
            .get("name")
            .and_then(Value::as_str)
            .filter(|n| !n.is_empty() && *n != tag)
        {
            bits.push(release_name.to_owned());
        }
        if let Some(body) = obj
            .get("body")
            .and_then(Value::as_str)
            .filter(|b| !b.trim().is_empty())
        {
            let no_hash = COMMIT_HASH_RE.replace_all(body, "");
            let no_md = MD_NOISE_RE.replace_all(&no_hash, " ");
            let cleaned = collapse_ws(&no_md);
            if !cleaned.is_empty() {
                bits.push(cleaned);
            }
        }
        Some(make_hit(
            "github_releases",
            format!("{repo} {tag}: latest release on GitHub"),
            url.to_owned(),
            clip(&bits.join(" · "), SNIPPET_CAP),
            (!published.is_empty()).then_some(published),
            Some("api"),
            HitKind::Record,
        ))
    }
}

#[async_trait]
impl Source for GitHubReleasesEngine {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    fn claims(&self, query: &str, _lang: Option<&str>) -> f32 {
        if REPO_RE.is_match(query) {
            1.0
        } else if version_question(query) && GITHUB_OR_RELEASE_RE.is_match(query) {
            0.9
        } else {
            0.0
        }
    }

    async fn call(&self, op: &str, params: Value, cx: &SrcCx) -> Result<SrcOutput, FsmError> {
        let (query, limit) = parse_search_params("github_releases", op, &params, 10)?;
        if query.trim().is_empty() {
            return Ok(SrcOutput::Hits(Vec::new()));
        }
        let token = resolve_optional_key(cx, &self.deps.cfg, GITHUB_TOKEN_ENV);
        let budget = CallBudget::from_cx(cx, &self.decl);

        let mut repos: Vec<String> = REPO_RE
            .captures_iter(query)
            .map(|c| format!("{}/{}", &c[1], &c[2]))
            .take(2)
            .collect();

        if repos.is_empty() {
            let names = candidate_names(query, 3);
            if names.is_empty() {
                return Ok(SrcOutput::Hits(Vec::new()));
            }
            let req = github_get(
                format!("{GITHUB_API}/search/repositories"),
                token.as_deref(),
                "github_releases",
            )
            .query_param("q", repo_search_terms(query))
            .query_param("per_page", "3");
            if let Some(found) = send_json_keyless(
                self.deps.net.as_ref(),
                req,
                self.decl.transport,
                &self.decl,
                cx,
                &budget,
            )
            .await?
                && let Some(items) = found.get("items").and_then(Value::as_array)
            {
                for item in items {
                    let Some(obj) = item.as_object() else {
                        continue;
                    };
                    let Some(full_name) = obj.get("full_name").and_then(Value::as_str) else {
                        continue;
                    };
                    let short = obj.get("name").and_then(Value::as_str).unwrap_or("");
                    if names_repo(short, &names) {
                        repos.push(full_name.to_owned());
                        if repos.len() >= 2 {
                            break;
                        }
                    }
                }
            }
        }

        if repos.is_empty() {
            return Ok(SrcOutput::Hits(Vec::new()));
        }

        let futs = repos.iter().map(|repo| {
            let req = github_get(
                format!("{GITHUB_API}/repos/{repo}/releases/latest"),
                token.as_deref(),
                "github_releases",
            );
            async move {
                let res = send_json_keyless(
                    self.deps.net.as_ref(),
                    req,
                    self.decl.transport,
                    &self.decl,
                    cx,
                    &budget,
                )
                .await?;
                Ok::<_, FsmError>((repo.as_str(), res))
            }
        });

        let mut results = Vec::new();
        let mut first_err: Option<FsmError> = None;
        for item in join_all(futs).await {
            match item {
                Ok((repo, Some(payload))) => {
                    if let Some(hit) = Self::map_release(repo, &payload) {
                        results.push(hit);
                    }
                }
                Ok((_, None)) => {}
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
