//! RDAP engine: domain registration, expiry, registrar, and nameservers via IANA bootstrap.

use crate::common::{
    CallBudget, SNIPPET_CAP, clip, is_fatal_fanout_error, iso_day, make_hit, parse_search_params,
    search_op, send_json_keyless,
};
use async_trait::async_trait;
use fsm_core::{
    AuthMode, Capability, FsmError, Hit, HitKind, HttpRequest, PoolRole, ProbeSpec, Source,
    SourceDecl, SourceDeps, SourceLimits, SrcCx, SrcOutput, TransportProfile,
};
use regex::Regex;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

const BOOTSTRAP_URL: &str = "https://data.iana.org/rdap/dns.json";
const BOOTSTRAP_TTL: Duration = Duration::from_secs(24 * 3600);

static DOMAIN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b((?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+(?:[a-z]{2,24}|xn--[a-z0-9-]+))\b",
    )
    .expect("regex")
});

static DOMAIN_ASK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(whois|rdap|registrar|registered|registration|expir(?:es?|y|ation)|domain|name ?servers?|dns)\b|域名|注册商|注册时间|到期|过期|续费|whois",
    )
    .expect("regex")
});

static NOT_DOMAINS: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| ["e.g", "i.e", "vs", "etc"].into_iter().collect());

/// Extract up to `limit` domain names from `query`, ignoring common abbreviations like `e.g`.
pub fn domains_in(query: &str, limit: usize) -> Vec<String> {
    let mut found = Vec::new();
    for cap in DOMAIN_RE.captures_iter(query) {
        let name = cap[1].to_ascii_lowercase().trim_matches('.').to_owned();
        if NOT_DOMAINS.contains(name.as_str()) || found.contains(&name) {
            continue;
        }
        found.push(name);
        if found.len() >= limit {
            break;
        }
    }
    found
}

/// Reduce a hostname to its registrable domain (2 labels, or 3 under common 2-level suffixes).
pub fn registrable_domain(domain: &str) -> String {
    let labels: Vec<&str> = domain.split('.').collect();
    if labels.len() >= 3
        && matches!(
            labels[labels.len() - 2],
            "co" | "com" | "net" | "org" | "gov" | "ac" | "edu"
        )
    {
        labels[labels.len() - 3..].join(".")
    } else if labels.len() >= 2 {
        labels[labels.len() - 2..].join(".")
    } else {
        domain.to_owned()
    }
}

struct BootstrapCache {
    fetched_at: Instant,
    map: HashMap<String, String>,
}

/// RDAP domain lookup engine with IANA DNS bootstrap.
pub struct RdapEngine {
    decl: SourceDecl,
    deps: SourceDeps,
    bootstrap: RwLock<Option<BootstrapCache>>,
}

impl RdapEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let mut decl = SourceDecl::new(
            "rdap",
            "RDAP",
            "RDAP: a domain's registration date, expiry, registrar and name servers.",
        )
        .with_scopes(&["reference"])
        .with_caps(&[Capability::Search, Capability::Lookup])
        .with_op(
            search_op(
                "Look up RDAP domain registration, expiry, registrar, and name servers.",
                5,
                r#"source.call("rdap.search", { query: "when does github.com expire" })"#,
            )
            .with_probe(ProbeSpec::new(
                json!({"query": "when does github.com expire"}),
                1,
            )),
        );
        decl.aliases = vec!["whois".to_owned(), "domain".to_owned(), "域名".to_owned()];
        decl.home_url = Some("https://data.iana.org/rdap/".to_owned());
        decl.auth = AuthMode::None;
        decl.transport = TransportProfile::Plain;
        decl.pool_role = PoolRole::DirectClaimant;
        decl.limits = SourceLimits::new(2.0, 4, 2, 4000, 21_600);
        Self {
            decl,
            deps,
            bootstrap: RwLock::new(None),
        }
    }

    fn parse_bootstrap(payload: &Value) -> HashMap<String, String> {
        let mut table = HashMap::new();
        let Some(services) = payload.get("services").and_then(Value::as_array) else {
            return table;
        };
        for entry in services {
            let Some(pair) = entry.as_array() else {
                continue;
            };
            if pair.len() < 2 {
                continue;
            }
            let (Some(tlds), Some(urls)) = (pair[0].as_array(), pair[1].as_array()) else {
                continue;
            };
            let Some(https_url) = urls
                .iter()
                .filter_map(Value::as_str)
                .find(|u| u.starts_with("https://"))
            else {
                continue;
            };
            let normalized = format!("{}/", https_url.trim_end_matches('/'));
            for t in tlds.iter().filter_map(Value::as_str) {
                table.insert(t.to_ascii_lowercase(), normalized.clone());
            }
        }
        table
    }

    async fn registry_for(
        &self,
        tld: &str,
        cx: &SrcCx,
        budget: &CallBudget,
    ) -> Result<Option<String>, FsmError> {
        // Bypass in-memory cache in replay_mode per A1 §B.9 so replay fixtures are deterministic.
        if !cx.replay_mode {
            let guard = self.bootstrap.read().await;
            if let Some(cache) = guard.as_ref()
                && cache.fetched_at.elapsed() <= BOOTSTRAP_TTL
                && !cache.map.is_empty()
            {
                return Ok(cache.map.get(tld).cloned());
            }
        }

        let req = HttpRequest::get(BOOTSTRAP_URL)
            .header("Accept", "application/json")
            .for_source("rdap");
        let Some(payload) = send_json_keyless(
            self.deps.net.as_ref(),
            req,
            self.decl.transport,
            &self.decl,
            cx,
            budget,
        )
        .await?
        else {
            return Ok(None);
        };

        let table = Self::parse_bootstrap(&payload);
        let found = table.get(tld).cloned();
        if !table.is_empty() && !cx.replay_mode {
            let mut guard = self.bootstrap.write().await;
            *guard = Some(BootstrapCache {
                fetched_at: Instant::now(),
                map: table,
            });
        }
        Ok(found)
    }

    /// Map an RDAP domain JSON record to a hit.
    pub fn map_record(payload: &Value, request_url: Option<&str>) -> Option<Hit> {
        let obj = payload.as_object()?;
        let name = obj
            .get("ldhName")
            .and_then(Value::as_str)?
            .to_ascii_lowercase();

        let mut events: HashMap<String, String> = HashMap::new();
        if let Some(arr) = obj.get("events").and_then(Value::as_array) {
            for ev in arr {
                if let Some(ev_obj) = ev.as_object()
                    && let Some(action) = ev_obj.get("eventAction").and_then(Value::as_str)
                {
                    events.insert(action.to_owned(), iso_day(ev_obj.get("eventDate")));
                }
            }
        }
        let registered = events.get("registration").cloned().unwrap_or_default();
        let expires = events.get("expiration").cloned().unwrap_or_default();
        let changed = events.get("last changed").cloned().unwrap_or_default();

        let mut registrar = String::new();
        if let Some(entities) = obj.get("entities").and_then(Value::as_array) {
            for entity in entities {
                let Some(eobj) = entity.as_object() else {
                    continue;
                };
                let is_registrar = eobj
                    .get("roles")
                    .and_then(Value::as_array)
                    .is_some_and(|roles| roles.iter().any(|r| r.as_str() == Some("registrar")));
                if !is_registrar {
                    continue;
                }
                if let Some(fields) = eobj
                    .get("vcardArray")
                    .and_then(Value::as_array)
                    .and_then(|va| va.get(1))
                    .and_then(Value::as_array)
                {
                    for field in fields {
                        if let Some(arr) = field.as_array()
                            && arr.first().and_then(Value::as_str) == Some("fn")
                            && let Some(val) = arr.get(3).and_then(Value::as_str)
                        {
                            registrar = val.to_owned();
                        }
                    }
                }
                break;
            }
        }

        let servers: Vec<String> = obj
            .get("nameservers")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|ns| {
                        ns.as_object()
                            .and_then(|o| o.get("ldhName"))
                            .and_then(Value::as_str)
                            .map(str::to_ascii_lowercase)
                    })
                    .take(4)
                    .collect()
            })
            .unwrap_or_default();

        let statuses: Vec<&str> = obj
            .get("status")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(Value::as_str).take(4).collect())
            .unwrap_or_default();

        let mut bits = Vec::new();
        if !registered.is_empty() {
            bits.push(format!("registered {registered}"));
        }
        if !expires.is_empty() {
            bits.push(format!("expires {expires}"));
        }
        if !changed.is_empty() {
            bits.push(format!("last changed {changed}"));
        }
        if !registrar.is_empty() {
            bits.push(format!("registrar {registrar}"));
        }
        if !statuses.is_empty() {
            bits.push(format!("status {}", statuses.join(", ")));
        }
        if !servers.is_empty() {
            bits.push(format!("name servers {}", servers.join(", ")));
        }
        bits.push("RDAP record from the registry".to_owned());

        let mut head = if !registered.is_empty() {
            format!("{name}: registered {registered}")
        } else {
            name.clone()
        };
        if !expires.is_empty() {
            head.push_str(&format!(", expires {expires}"));
        }

        let url = request_url
            .filter(|u| !u.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("https://rdap.org/domain/{name}"));

        Some(make_hit(
            "rdap",
            head,
            url,
            clip(&bits.join(" · "), SNIPPET_CAP),
            (!changed.is_empty()).then_some(changed),
            Some("api"),
            HitKind::Record,
        ))
    }
}

#[async_trait]
impl Source for RdapEngine {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    fn claims(&self, query: &str, _lang: Option<&str>) -> f32 {
        if DOMAIN_ASK_RE.is_match(query) && !domains_in(query, 2).is_empty() {
            1.0
        } else {
            0.0
        }
    }

    async fn call(&self, op: &str, params: Value, cx: &SrcCx) -> Result<SrcOutput, FsmError> {
        let (query, limit) = parse_search_params("rdap", op, &params, 5)?;
        let domains = domains_in(query, 2);
        if domains.is_empty() {
            return Ok(SrcOutput::Hits(Vec::new()));
        }
        let budget = CallBudget::from_cx(cx, &self.decl);
        let mut results = Vec::new();
        let mut first_err: Option<FsmError> = None;
        for raw_domain in domains {
            let domain = registrable_domain(&raw_domain);
            let tld = domain.rsplit('.').next().unwrap_or("");
            let base = match self.registry_for(tld, cx, &budget).await {
                Ok(Some(base)) => base,
                Ok(None) => continue,
                Err(e) if is_fatal_fanout_error(&e) => return Err(e),
                Err(e) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                    continue;
                }
            };
            let url = format!("{base}domain/{domain}");
            let req = HttpRequest::get(&url)
                .header("Accept", "application/json")
                .for_source("rdap");
            match send_json_keyless(
                self.deps.net.as_ref(),
                req,
                self.decl.transport,
                &self.decl,
                cx,
                &budget,
            )
            .await
            {
                Ok(Some(payload)) => {
                    if let Some(hit) = Self::map_record(&payload, Some(&url)) {
                        results.push(hit);
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
            && let Some(err) = first_err
        {
            return Err(err);
        }
        results.truncate(limit);
        Ok(SrcOutput::Hits(results))
    }
}
