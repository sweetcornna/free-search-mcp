//! OSV.dev: known vulnerabilities and fixed versions for packages and advisory IDs.
//!
//! Port of `src/search_mcp/engines/osv.py`.

use crate::facts::{TOKEN_RE, candidate_names, clip, clip_query, iso_day};
use crate::http::{
    CallBudget, build_source_decl, encode_path_keep_slash, first_nonempty_str, first_truthy_value,
    get_json_opt, hits_output, is_fatal_fanout_error, make_hit, parse_search_params, post_json,
};
use fsm_core::{
    Availability, Config, FsmError, Hit, PoolRole, ProbeSpec, Source, SourceDecl, SourceDeps,
    SourceLimits, SrcCx, SrcOutput,
};
use futures::future::join_all;
use regex::Regex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::LazyLock;

const API_URL: &str = "https://api.osv.dev/v1/query";

static HINTS: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    HashMap::from([
        ("pypi", "pypi"),
        ("pip", "pypi"),
        ("python", "pypi"),
        ("npm", "npm"),
        ("node", "npm"),
        ("nodejs", "npm"),
        ("javascript", "npm"),
        ("cargo", "cargo"),
        ("crate", "cargo"),
        ("crates", "cargo"),
        ("rust", "cargo"),
        ("go", "golang"),
        ("golang", "golang"),
        ("maven", "maven"),
        ("java", "maven"),
        ("gem", "rubygems"),
        ("ruby", "rubygems"),
        ("rubygems", "rubygems"),
        ("composer", "packagist"),
        ("php", "packagist"),
        ("nuget", "nuget"),
        ("dotnet", "nuget"),
    ])
});

static VULN_ASK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(vulnerabilit(?:y|ies)|advisor(?:y|ies)|cve|security (?:issue|bug|fix)|exploit)\b|漏洞|安全公告|安全问题",
    )
    .expect("osv vuln_ask regex")
});

static ID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:CVE|GHSA|PYSEC|RUSTSEC|GO|OSV)-[0-9A-Za-z-]{4,}\b").expect("osv id regex")
});

/// Upper-case the prefix only: a GHSA suffix is lower-case and case-sensitive.
pub fn canonical_id(vuln_id: &str) -> String {
    let Some((prefix, rest)) = vuln_id.split_once('-') else {
        return vuln_id.to_owned();
    };
    let upper = prefix.to_ascii_uppercase();
    if matches!(upper.as_str(), "CVE" | "PYSEC" | "RUSTSEC" | "GO" | "OSV") {
        vuln_id.to_ascii_uppercase()
    } else {
        format!("{upper}-{rest}")
    }
}

/// Package ecosystems named in `query`, or the default `["pypi", "npm"]`.
pub fn ecosystems_for(query: &str) -> Vec<&'static str> {
    let lower = query.to_lowercase();
    let mut found: Vec<&'static str> = Vec::new();
    for m in TOKEN_RE.find_iter(&lower) {
        if let Some(&eco) = HINTS.get(m.as_str())
            && !found.contains(&eco)
        {
            found.push(eco);
        }
    }
    if found.is_empty() {
        vec!["pypi", "npm"]
    } else {
        found
    }
}

fn capitalize_ascii(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => {
            let mut out = String::with_capacity(s.len());
            for u in first.to_uppercase() {
                out.push(u);
            }
            for c in chars {
                for l in c.to_lowercase() {
                    out.push(l);
                }
            }
            out
        }
    }
}

fn severity_text(vuln: &serde_json::Map<String, Value>) -> String {
    if let Some(sev) = vuln
        .get("database_specific")
        .and_then(Value::as_object)
        .and_then(|db| db.get("severity"))
        .and_then(Value::as_str)
    {
        return format!("{} severity", capitalize_ascii(sev));
    }
    if let Some(list) = vuln.get("severity").and_then(Value::as_array) {
        for entry in list {
            if let Some(obj) = entry.as_object()
                && let Some(score) = obj.get("score").and_then(Value::as_str)
            {
                let ty = obj.get("type").and_then(Value::as_str).unwrap_or("CVSS");
                let short: String = score.chars().take(44).collect();
                return format!("{ty} {short}");
            }
        }
    }
    String::new()
}

fn affected_text(affected: Option<&Value>, package: &str) -> String {
    let Some(entries) = affected.and_then(Value::as_array) else {
        return String::new();
    };
    let wanted = if !package.is_empty() {
        package.rsplit('/').next().unwrap_or("")
    } else {
        ""
    };
    for entry in entries {
        let Some(eobj) = entry.as_object() else {
            continue;
        };
        let pkg = eobj.get("package").and_then(Value::as_object);
        let purl = pkg.and_then(|p| p.get("purl")).and_then(Value::as_str);
        let name = pkg
            .and_then(|p| p.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if !wanted.is_empty() && purl.is_some_and(|p| p != package) && name != wanted {
            continue;
        }
        let eco = pkg
            .and_then(|p| p.get("ecosystem"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let label = format!("{eco} {name}").trim().to_owned();

        let mut fixed: Vec<&str> = Vec::new();
        let mut introduced: Vec<&str> = Vec::new();
        if let Some(ranges) = eobj.get("ranges").and_then(Value::as_array) {
            for r in ranges {
                if let Some(events) = r.get("events").and_then(Value::as_array) {
                    for ev in events {
                        if let Some(f) = ev.get("fixed").and_then(Value::as_str) {
                            fixed.push(f);
                        }
                        if let Some(i) = ev.get("introduced").and_then(Value::as_str) {
                            introduced.push(i);
                        }
                    }
                }
            }
        }
        let mut parts: Vec<String> = Vec::new();
        if !label.is_empty() {
            parts.push(label);
        }
        if let Some(&first_intro) = introduced.first()
            && !first_intro.is_empty()
            && first_intro != "0"
        {
            parts.push(format!("from {first_intro}"));
        }
        if !fixed.is_empty() {
            let take3: Vec<&str> = fixed.into_iter().take(3).collect();
            parts.push(format!("fixed in {}", take3.join(", ")));
        } else {
            parts.push("no fix listed".to_owned());
        }
        return parts.join(" ");
    }
    String::new()
}

pub struct OsvEngine {
    decl: SourceDecl,
    deps: SourceDeps,
}

impl OsvEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let decl = build_source_decl(
            "osv",
            "OSV.dev",
            "OSV.dev: known vulnerabilities and fixed versions for a package (PyPI, npm...).",
            &["ghsa", "pysec", "rustsec", "advisory", "漏洞"],
            "https://osv.dev",
            &["security"],
            PoolRole::DirectClaimant,
            SourceLimits::new(2.0, 4, 2, 5_000, 21_600),
            r#"source.call("osv.search", {query: "requests vulnerabilities"})"#,
            ProbeSpec::new(json!({"query": "requests vulnerabilities", "limit": 3}), 1),
        );
        Self { decl, deps }
    }

    pub fn map_results(&self, payload: &Value) -> Vec<Hit> {
        let vulns = payload
            .get("vulns")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        self.map_many(vulns, "")
    }

    pub fn map_many(&self, vulns: &[Value], package: &str) -> Vec<Hit> {
        let mut out = Vec::new();
        for vuln in vulns {
            let Some(vobj) = vuln.as_object() else {
                continue;
            };
            let Some(vid) = vobj.get("id").and_then(Value::as_str) else {
                continue;
            };
            let aliases: Vec<&str> = vobj
                .get("aliases")
                .and_then(Value::as_array)
                .map(|arr| arr.iter().filter_map(Value::as_str).take(3).collect())
                .unwrap_or_default();
            let sev = severity_text(vobj);
            let when = iso_day(first_truthy_value(vobj, &["published", "modified"]));
            let mut bits: Vec<String> = Vec::new();
            if !sev.is_empty() {
                bits.push(sev);
            }
            if !aliases.is_empty() {
                bits.push(format!("also {}", aliases.join(", ")));
            }
            let aff = affected_text(vobj.get("affected"), package);
            if !aff.is_empty() {
                bits.push(aff);
            }
            if !when.is_empty() {
                bits.push(format!("published {when}"));
            }
            let summary = first_nonempty_str(vobj, &["summary", "details"])
                .unwrap_or("")
                .trim();
            if !summary.is_empty() {
                bits.push(clip(summary, 240));
            }
            let title = if !summary.is_empty() {
                format!("{vid}: {}", clip(summary, 80))
            } else {
                vid.to_owned()
            };
            out.push(make_hit(
                &self.decl.id,
                title,
                format!(
                    "https://osv.dev/vulnerability/{}",
                    encode_path_keep_slash(vid)
                ),
                clip(&bits.join(" · "), 500),
                (!when.is_empty()).then_some(when),
            ));
        }
        out
    }

    fn rank(mut results: Vec<Hit>, max_results: usize) -> Vec<Hit> {
        results.sort_by(|a, b| {
            let da = a.date.as_deref().unwrap_or("");
            let db = b.date.as_deref().unwrap_or("");
            db.cmp(da)
        });
        results.truncate(max_results);
        results
    }
}

#[async_trait::async_trait]
impl Source for OsvEngine {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    fn claims(&self, query: &str, _lang: Option<&str>) -> f32 {
        let q = clip_query(query);
        if ID_RE.is_match(q) {
            return 1.0;
        }
        if VULN_ASK.is_match(q)
            && candidate_names(q, 4)
                .iter()
                .any(|n| !HINTS.contains_key(n.as_str()))
        {
            return 1.0;
        }
        0.0
    }

    fn availability(&self, _cfg: &Config) -> Availability {
        Availability::Available
    }

    async fn call(&self, op: &str, params: Value, cx: &SrcCx) -> Result<SrcOutput, FsmError> {
        let (query, limit) = parse_search_params(&self.decl.id, op, &params, 10, 50)?;
        if query.trim().is_empty() {
            return Ok(hits_output(Vec::new()));
        }
        let budget = CallBudget::from_cx(cx, &self.decl);
        let ids: Vec<String> = ID_RE
            .find_iter(&query)
            .take(2)
            .map(|m| canonical_id(m.as_str()))
            .collect();
        if !ids.is_empty() {
            let fetches = ids.iter().map(|id| {
                let url = format!(
                    "https://api.osv.dev/v1/vulns/{}",
                    encode_path_keep_slash(id)
                );
                get_json_opt(&self.deps, &self.decl, cx, &budget, url, &[], None)
            });
            let payloads = join_all(fetches).await;
            let mut found = Vec::new();
            let mut first_err: Option<FsmError> = None;
            for res in payloads {
                match res {
                    Ok(Some(v)) => {
                        if v.get("id").and_then(Value::as_str).is_some() {
                            found.push(v);
                        }
                    }
                    Ok(None) => {}
                    Err(e) if is_fatal_fanout_error(&e) => return Err(e),
                    Err(e) => {
                        if first_err.is_none() {
                            first_err = Some(e);
                        }
                    }
                }
            }
            let mapped = self.map_many(&found, "");
            if mapped.is_empty()
                && let Some(e) = first_err
            {
                return Err(e);
            }
            return Ok(hits_output(Self::rank(mapped, limit)));
        }

        let names: Vec<String> = candidate_names(&query, 4)
            .into_iter()
            .filter(|n| !HINTS.contains_key(n.as_str()))
            .take(2)
            .collect();
        if names.is_empty() {
            return Ok(hits_output(Vec::new()));
        }
        let ecosystems = ecosystems_for(&query);
        let mut purls = Vec::new();
        for name in &names {
            for &eco in &ecosystems {
                purls.push(format!("pkg:{eco}/{name}"));
            }
        }
        let fetches = purls.iter().map(|purl| {
            let body = json!({"package": {"purl": purl}});
            async move { post_json(&self.deps, &self.decl, cx, &budget, API_URL, &body).await }
        });
        let payloads = join_all(fetches).await;

        let mut results = Vec::new();
        let mut first_err: Option<FsmError> = None;
        for (purl, res) in purls.into_iter().zip(payloads) {
            match res {
                Ok(Some(payload)) => {
                    if let Some(vulns) = payload.get("vulns").and_then(Value::as_array)
                        && !vulns.is_empty()
                    {
                        results.extend(self.map_many(vulns, &purl));
                    }
                }
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
            && let Some(e) = first_err
        {
            return Err(e);
        }
        Ok(hits_output(Self::rank(results, limit)))
    }
}
