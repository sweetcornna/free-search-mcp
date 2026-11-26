//! Multi-ecosystem package registries engine: Maven Central, RubyGems, Go module
//! proxy, Homebrew, Docker Hub, Packagist, and NuGet.
//!
//! Declares `"code"` in `scopes` per M16 so direct-fact claims routing operates on
//! both `scope="web"` and `scope="code"`.

use crate::common::{
    CallBudget, SNIPPET_CAP, clip, encode_path_segment, format_i64_commas, is_fatal_fanout_error,
    iso_day, make_hit, parse_search_params, search_op, send_json_keyless,
};
use crate::facts::{candidate_names, ecosystem_hints, version_question};
use async_trait::async_trait;
use fsm_core::{
    AuthMode, Capability, FsmError, Hit, HitKind, HttpRequest, PoolRole, ProbeSpec, Source,
    SourceDecl, SourceDeps, SourceLimits, SrcCx, SrcOutput, TransportProfile,
};
use futures::future::{BoxFuture, join_all};
use regex::Regex;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::LazyLock;

static GO_MODULE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\b((?:github\.com|gitlab\.com|golang\.org|google\.golang\.org|gopkg\.in|go\.uber\.org|k8s\.io|sigs\.k8s\.io)/[\w./-]+)",
    )
    .expect("regex")
});

static VENDOR_NAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b([\w.-]+)/([\w.-]+)\b").expect("regex"));

static EXTRA_STOP: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "maven",
        "gradle",
        "java",
        "kotlin",
        "jar",
        "gem",
        "ruby",
        "bundler",
        "go",
        "golang",
        "brew",
        "homebrew",
        "formula",
        "docker",
        "dockerhub",
        "image",
        "container",
        "composer",
        "packagist",
        "php",
        "nuget",
        "dotnet",
        ".net",
        "c#",
        "csharp",
        "镜像",
        "容器",
    ]
    .into_iter()
    .collect()
});

/// Extract up to 2 package candidate names excluding ecosystem keywords.
pub fn registry_names(query: &str) -> Vec<String> {
    candidate_names(query, 4)
        .into_iter()
        .filter(|n| !EXTRA_STOP.contains(n.as_str()))
        .take(2)
        .collect()
}

/// Escape Go module paths per proxy.golang.org (`A` -> `!a`).
pub fn escape_go_module(module: &str) -> String {
    let mut out = String::with_capacity(module.len());
    for ch in module.chars() {
        if ch.is_ascii_uppercase() {
            out.push('!');
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// Current version and release date across Maven, RubyGems, Go, Homebrew, Docker Hub,
/// Packagist, and NuGet.
pub struct RegistriesEngine {
    decl: SourceDecl,
    deps: SourceDeps,
}

impl RegistriesEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let mut decl = SourceDecl::new(
            "registries",
            "Package Registries",
            "Maven, RubyGems, Go, Homebrew, Docker Hub, Packagist, NuGet: current version and date.",
        )
        // M16: MUST declare `"code"` in scopes.
        .with_scopes(&["code"])
        .with_caps(&[Capability::Search, Capability::Lookup])
        .with_op(
            search_op(
                "Look up current package versions across Maven, RubyGems, Go, Homebrew, Docker Hub, Packagist, and NuGet.",
                10,
                r#"source.call("registries.search", { query: "guava maven latest version" })"#,
            )
            .with_probe(ProbeSpec::new(
                json!({"query": "guava maven latest version", "limit": 1}),
                1,
            )),
        );
        decl.aliases = vec![
            "maven".to_owned(),
            "rubygems".to_owned(),
            "homebrew".to_owned(),
            "dockerhub".to_owned(),
            "packagist".to_owned(),
            "nuget".to_owned(),
            "pkg.go.dev".to_owned(),
        ];
        decl.home_url = Some("https://central.sonatype.com".to_owned());
        decl.auth = AuthMode::None;
        decl.transport = TransportProfile::Plain;
        decl.pool_role = PoolRole::DirectClaimant;
        decl.limits = SourceLimits::new(2.0, 4, 4, 4000, 21_600);
        Self { decl, deps }
    }

    fn build_hit(title: String, url: String, bits: Vec<String>, day: String) -> Hit {
        let snippet = clip(
            &bits
                .into_iter()
                .filter(|b| !b.is_empty())
                .collect::<Vec<_>>()
                .join(" · "),
            SNIPPET_CAP,
        );
        make_hit(
            "registries",
            title,
            url,
            snippet,
            (!day.is_empty()).then_some(day),
            Some("api"),
            HitKind::Record,
        )
    }

    fn get_req(url: impl Into<String>) -> HttpRequest {
        HttpRequest::get(url)
            .header("Accept", "application/json")
            .for_source("registries")
    }

    /// Map a Maven Central Solr response.
    pub fn map_maven(payload: &Value) -> Vec<Hit> {
        let Some(docs) = payload
            .get("response")
            .and_then(|r| r.get("docs"))
            .and_then(Value::as_array)
        else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for doc in docs {
            let Some(obj) = doc.as_object() else {
                continue;
            };
            let Some(v) = obj.get("latestVersion").and_then(Value::as_str) else {
                continue;
            };
            let g = obj.get("g").and_then(Value::as_str).unwrap_or("");
            let a = obj.get("a").and_then(Value::as_str).unwrap_or("");
            let day = obj
                .get("timestamp")
                .and_then(Value::as_f64)
                .map(|ms| iso_day(Some(&serde_json::json!(ms / 1000.0))))
                .unwrap_or_default();
            let count = obj
                .get("versionCount")
                .map(|c| match c {
                    Value::Number(n) => n.to_string(),
                    Value::String(s) => s.clone(),
                    _ => "?".to_owned(),
                })
                .unwrap_or_else(|| "?".to_owned());
            let head = if day.is_empty() {
                format!("Latest version {v}")
            } else {
                format!("Latest version {v}, released {day}")
            };
            out.push(Self::build_hit(
                format!("{g}:{a} {v} on Maven Central"),
                format!("https://central.sonatype.com/artifact/{g}/{a}/{v}"),
                vec![head, format!("{count} versions published")],
                day,
            ));
            if out.len() >= 2 {
                break;
            }
        }
        out
    }

    /// Map a RubyGems JSON payload.
    pub fn map_rubygems(name: &str, payload: &Value) -> Vec<Hit> {
        let Some(obj) = payload.as_object() else {
            return Vec::new();
        };
        let Some(version) = obj.get("version").and_then(Value::as_str) else {
            return Vec::new();
        };
        let day = iso_day(obj.get("version_created_at"));
        let mut bits = vec![if day.is_empty() {
            format!("Latest version {version}")
        } else {
            format!("Latest version {version}, released {day}")
        }];
        if let Some(info) = obj.get("info").and_then(Value::as_str) {
            bits.push(info.to_owned());
        }
        let home = obj
            .get("homepage_uri")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .or_else(|| obj.get("source_code_uri").and_then(Value::as_str));
        if let Some(home) = home.filter(|h| h.starts_with("http")) {
            bits.push(format!("homepage {home}"));
        }
        let gem_name = obj.get("name").and_then(Value::as_str).unwrap_or(name);
        vec![Self::build_hit(
            format!("{gem_name} {version} on RubyGems"),
            format!("https://rubygems.org/gems/{}", encode_path_segment(name)),
            bits,
            day,
        )]
    }

    /// Map a Go module proxy `/@latest` JSON payload.
    pub fn map_go(module: &str, payload: &Value) -> Vec<Hit> {
        let Some(obj) = payload.as_object() else {
            return Vec::new();
        };
        let Some(version) = obj.get("Version").and_then(Value::as_str) else {
            return Vec::new();
        };
        let day = iso_day(obj.get("Time"));
        let bit = if day.is_empty() {
            format!("Latest version {version}")
        } else {
            format!("Latest version {version}, tagged {day}")
        };
        vec![Self::build_hit(
            format!("{module} {version} (Go module)"),
            format!("https://pkg.go.dev/{module}@{version}"),
            vec![bit],
            day,
        )]
    }

    /// Map a Homebrew formula JSON payload.
    pub fn map_homebrew(name: &str, payload: &Value) -> Vec<Hit> {
        let Some(obj) = payload.as_object() else {
            return Vec::new();
        };
        let Some(stable) = obj
            .get("versions")
            .and_then(|v| v.get("stable"))
            .and_then(Value::as_str)
        else {
            return Vec::new();
        };
        let mut bits = vec![format!("Stable version {stable}")];
        if let Some(desc) = obj.get("desc").and_then(Value::as_str) {
            bits.push(desc.to_owned());
        }
        if let Some(lic) = obj.get("license").and_then(Value::as_str) {
            bits.push(format!("licence {lic}"));
        }
        if let Some(home) = obj.get("homepage").and_then(Value::as_str) {
            bits.push(format!("homepage {home}"));
        }
        let fname = obj.get("name").and_then(Value::as_str).unwrap_or(name);
        vec![Self::build_hit(
            format!("{fname} {stable} on Homebrew"),
            format!(
                "https://formulae.brew.sh/formula/{}",
                encode_path_segment(name)
            ),
            bits,
            String::new(),
        )]
    }

    /// Map a Docker Hub repository JSON payload.
    pub fn map_docker(namespace: &str, repo: &str, payload: &Value) -> Vec<Hit> {
        let Some(obj) = payload.as_object() else {
            return Vec::new();
        };
        if !obj.get("name").is_some_and(Value::is_string) {
            return Vec::new();
        }
        let day = iso_day(obj.get("last_updated"));
        let mut bits = vec![if day.is_empty() {
            "Docker Hub repository".to_owned()
        } else {
            format!("Last pushed {day}")
        }];
        if let Some(desc) = obj
            .get("description")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            bits.push(desc.to_owned());
        }
        if let Some(pulls) = obj.get("pull_count").and_then(Value::as_i64) {
            bits.push(format!("{} pulls", format_i64_commas(pulls)));
        }
        let page = if namespace == "library" {
            format!("https://hub.docker.com/_/{}", encode_path_segment(repo))
        } else {
            format!(
                "https://hub.docker.com/r/{}/{}",
                encode_path_segment(namespace),
                encode_path_segment(repo)
            )
        };
        let label = if namespace == "library" {
            repo.to_owned()
        } else {
            format!("{namespace}/{repo}")
        };
        vec![Self::build_hit(
            format!("{label} on Docker Hub"),
            page,
            bits,
            day,
        )]
    }

    /// Map a Packagist v2 JSON payload.
    pub fn map_packagist(vendor: &str, name: &str, payload: &Value) -> Vec<Hit> {
        let key = format!("{vendor}/{name}");
        let Some(top) = payload
            .get("packages")
            .and_then(|p| p.get(&key))
            .and_then(Value::as_array)
            .and_then(|arr| arr.first())
            .and_then(Value::as_object)
        else {
            return Vec::new();
        };
        let Some(version) = top.get("version").and_then(Value::as_str) else {
            return Vec::new();
        };
        let day = iso_day(top.get("time"));
        let mut bits = vec![if day.is_empty() {
            format!("Latest version {version}")
        } else {
            format!("Latest version {version}, released {day}")
        }];
        if let Some(desc) = top.get("description").and_then(Value::as_str) {
            bits.push(desc.to_owned());
        }
        vec![Self::build_hit(
            format!("{vendor}/{name} {version} on Packagist"),
            format!(
                "https://packagist.org/packages/{}/{}",
                encode_path_segment(vendor),
                encode_path_segment(name)
            ),
            bits,
            day,
        )]
    }

    /// Map a NuGet search query JSON payload.
    pub fn map_nuget(name: &str, payload: &Value) -> Vec<Hit> {
        let Some(data) = payload.get("data").and_then(Value::as_array) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for pkg in data {
            let Some(obj) = pkg.as_object() else {
                continue;
            };
            let Some(version) = obj.get("version").and_then(Value::as_str) else {
                continue;
            };
            let pid = obj.get("id").and_then(Value::as_str).unwrap_or(name);
            let mut bits = vec![format!("Latest version {version}")];
            if let Some(desc) = obj.get("description").and_then(Value::as_str) {
                bits.push(desc.to_owned());
            }
            out.push(Self::build_hit(
                format!("{pid} {version} on NuGet"),
                format!(
                    "https://www.nuget.org/packages/{}/{}",
                    encode_path_segment(pid),
                    encode_path_segment(version)
                ),
                bits,
                String::new(),
            ));
            if out.len() >= 2 {
                break;
            }
        }
        out
    }

    async fn fetch_json(
        &self,
        req: HttpRequest,
        cx: &SrcCx,
        budget: &CallBudget,
    ) -> Result<Option<Value>, FsmError> {
        send_json_keyless(
            self.deps.net.as_ref(),
            req,
            self.decl.transport,
            &self.decl,
            cx,
            budget,
        )
        .await
    }
}

#[async_trait]
impl Source for RegistriesEngine {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    fn claims(&self, query: &str, _lang: Option<&str>) -> f32 {
        let hints = ecosystem_hints(query);
        let has_ours = hints
            .iter()
            .any(|h| !matches!(*h, "pypi" | "npm" | "cargo" | "app"));
        let named = !registry_names(query).is_empty() || GO_MODULE_RE.is_match(query);
        if version_question(query) && has_ours && named {
            1.0
        } else {
            0.0
        }
    }

    async fn call(&self, op: &str, params: Value, cx: &SrcCx) -> Result<SrcOutput, FsmError> {
        let (query, limit) = parse_search_params("registries", op, &params, 10)?;
        if query.trim().is_empty() {
            return Ok(SrcOutput::Hits(Vec::new()));
        }
        let budget = CallBudget::from_cx(cx, &self.decl);
        let hints = ecosystem_hints(query);
        let names = registry_names(query);
        let mut calls: Vec<BoxFuture<'_, Result<Vec<Hit>, FsmError>>> = Vec::new();

        if hints.contains("maven")
            && let Some(name) = names.first().cloned()
        {
            calls.push(Box::pin(async move {
                let req = Self::get_req("https://search.maven.org/solrsearch/select")
                    .query_param("q", name)
                    .query_param("rows", "3")
                    .query_param("wt", "json");
                Ok(self
                    .fetch_json(req, cx, &budget)
                    .await?
                    .map(|p| Self::map_maven(&p))
                    .unwrap_or_default())
            }));
        }

        if hints.contains("rubygems") {
            for name in &names {
                let n = name.clone();
                calls.push(Box::pin(async move {
                    let req = Self::get_req(format!(
                        "https://rubygems.org/api/v1/gems/{}.json",
                        encode_path_segment(&n)
                    ));
                    Ok(self
                        .fetch_json(req, cx, &budget)
                        .await?
                        .map(|p| Self::map_rubygems(&n, &p))
                        .unwrap_or_default())
                }));
            }
        }

        if hints.contains("go")
            && let Some(m) = GO_MODULE_RE.captures(query)
        {
            let module = m[1].trim_end_matches(['/', '.']).to_owned();
            let escaped = escape_go_module(&module);
            calls.push(Box::pin(async move {
                let req = Self::get_req(format!("https://proxy.golang.org/{escaped}/@latest"));
                Ok(self
                    .fetch_json(req, cx, &budget)
                    .await?
                    .map(|p| Self::map_go(&module, &p))
                    .unwrap_or_default())
            }));
        }

        if hints.contains("homebrew") {
            for name in &names {
                let n = name.clone();
                calls.push(Box::pin(async move {
                    let req = Self::get_req(format!(
                        "https://formulae.brew.sh/api/formula/{}.json",
                        encode_path_segment(&n)
                    ));
                    Ok(self
                        .fetch_json(req, cx, &budget)
                        .await?
                        .map(|p| Self::map_homebrew(&n, &p))
                        .unwrap_or_default())
                }));
            }
        }

        if hints.contains("docker") {
            for name in &names {
                let (ns, repo) = match name.rsplit_once('/') {
                    Some((ns, r)) if !ns.is_empty() => (ns.to_owned(), r.to_owned()),
                    _ => ("library".to_owned(), name.clone()),
                };
                calls.push(Box::pin(async move {
                    let req = Self::get_req(format!(
                        "https://hub.docker.com/v2/repositories/{}/{}",
                        encode_path_segment(&ns),
                        encode_path_segment(&repo)
                    ));
                    Ok(self
                        .fetch_json(req, cx, &budget)
                        .await?
                        .map(|p| Self::map_docker(&ns, &repo, &p))
                        .unwrap_or_default())
                }));
            }
        }

        if hints.contains("packagist")
            && let Some(m) = VENDOR_NAME_RE.captures(query)
        {
            let vendor = m[1].to_ascii_lowercase();
            let pkg = m[2].to_ascii_lowercase();
            if !vendor.contains('.') {
                calls.push(Box::pin(async move {
                    let req = Self::get_req(format!(
                        "https://repo.packagist.org/p2/{}/{}.json",
                        encode_path_segment(&vendor),
                        encode_path_segment(&pkg)
                    ));
                    Ok(self
                        .fetch_json(req, cx, &budget)
                        .await?
                        .map(|p| Self::map_packagist(&vendor, &pkg, &p))
                        .unwrap_or_default())
                }));
            }
        }

        if hints.contains("nuget")
            && let Some(name) = names.first().cloned()
        {
            calls.push(Box::pin(async move {
                let req = Self::get_req("https://azuresearch-usnc.nuget.org/query")
                    .query_param("q", &name)
                    .query_param("take", "3");
                Ok(self
                    .fetch_json(req, cx, &budget)
                    .await?
                    .map(|p| Self::map_nuget(&name, &p))
                    .unwrap_or_default())
            }));
        }

        if calls.is_empty() {
            return Ok(SrcOutput::Hits(Vec::new()));
        }

        let mut results = Vec::new();
        let mut first_err: Option<FsmError> = None;
        for bucket in join_all(calls).await {
            match bucket {
                Ok(hits) => results.extend(hits),
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
