//! CISA KEV: Known Exploited Vulnerabilities catalogue lookup.
//!
//! Port of `src/search_mcp/engines/cisakev.py`.

use crate::facts::{STOP_WORDS, TOKEN_RE, clip, clip_query};
use crate::http::{
    CallBudget, build_source_decl, encode_query_plus, get_json, hits_output, is_fatal_fanout_error,
    make_hit, parse_search_params,
};
use fsm_core::{
    Availability, Config, FsmError, Hit, PoolRole, ProbeSpec, Source, SourceDecl, SourceDeps,
    SourceLimits, SrcCx, SrcOutput,
};
use regex::Regex;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::Instant;

const FEED_URL: &str =
    "https://www.cisa.gov/sites/default/files/feeds/known_exploited_vulnerabilities.json";
const CATALOG_URL: &str = "https://www.cisa.gov/known-exploited-vulnerabilities-catalog";
const CACHE_TTL: Duration = Duration::from_secs(6 * 3600);
/// The KEV feed is ~1.7+ MB and growing; cap generously at 10 MiB.
const FEED_MAX_BYTES: u64 = 10 << 20;

static CVE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bCVE-\d{4}-\d{4,}\b").expect("kev cve regex"));
static EXPLOITED_ASK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)exploited in the wild|actively exploited|known exploited|\bkev\b|in.the.wild|在野利用|被利用|已被攻击|实际利用",
    )
    .expect("kev exploited_ask regex")
});

#[derive(Default)]
struct KevFeed {
    rows: Vec<Value>,
    version: String,
}

#[derive(Default)]
struct KevCache {
    fetched_at: Option<Instant>,
    feed: Option<Arc<KevFeed>>,
}

pub struct CisaKevEngine {
    decl: SourceDecl,
    deps: SourceDeps,
    cache: RwLock<KevCache>,
}

impl CisaKevEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let decl = build_source_decl(
            "cisakev",
            "CISA KEV",
            "CISA KEV: whether a CVE is known to be exploited, with date added and due date.",
            &["kev", "known exploited", "cisa", "在野利用"],
            CATALOG_URL,
            &["security"],
            PoolRole::DirectClaimant,
            SourceLimits::new(1.0, 2, 2, 8_000, 21_600),
            r#"source.call("cisakev.search", {query: "is CVE-2024-3094 exploited"})"#,
            ProbeSpec::new(json!({"query": "CVE-2024-3094", "limit": 3}), 1),
        );
        Self {
            decl,
            deps,
            cache: RwLock::new(KevCache::default()),
        }
    }

    /// Whether the query names a specific CVE ID (direct-record mode vs keyword mode).
    pub fn answers_directly(query: &str) -> bool {
        CVE_RE.is_match(query)
    }

    async fn load_feed(&self, cx: &SrcCx, budget: &CallBudget) -> Result<Arc<KevFeed>, FsmError> {
        let stale = if !cx.replay_mode {
            let guard = self.cache.read().await;
            if let (Some(at), Some(feed)) = (guard.fetched_at, &guard.feed)
                && at.elapsed() <= CACHE_TTL
                && !feed.rows.is_empty()
            {
                return Ok(Arc::clone(feed));
            }
            guard.feed.clone()
        } else {
            None
        };

        let payload = match get_json(
            &self.deps,
            &self.decl,
            cx,
            budget,
            FEED_URL,
            &[],
            Some(FEED_MAX_BYTES),
        )
        .await
        {
            Ok(p) => p,
            Err(e) if is_fatal_fanout_error(&e) => return Err(e),
            Err(e) => {
                if let Some(prev) = stale {
                    return Ok(prev);
                }
                return Err(e);
            }
        };
        let Some(obj) = payload.as_ref().and_then(Value::as_object) else {
            return Ok(stale.unwrap_or_else(|| Arc::new(KevFeed::default())));
        };
        let rows: Vec<Value> = obj
            .get("vulnerabilities")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter(|v| v.is_object()).cloned().collect())
            .unwrap_or_default();
        if rows.is_empty() {
            return Ok(stale.unwrap_or_else(|| Arc::new(KevFeed::default())));
        }
        let version = obj
            .get("catalogVersion")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let feed = Arc::new(KevFeed { rows, version });
        if !cx.replay_mode {
            let mut guard = self.cache.write().await;
            guard.feed = Some(Arc::clone(&feed));
            guard.fetched_at = Some(Instant::now());
        }
        Ok(feed)
    }

    fn not_listed(&self, cve: &str, version: &str) -> Hit {
        let day = if version.matches('.').count() == 2 {
            version.replace('.', "-")
        } else {
            String::new()
        };
        let title = if !day.is_empty() {
            format!("{cve}: not in CISA's Known Exploited Vulnerabilities catalogue (as of {day})")
        } else {
            format!("{cve}: not in CISA's Known Exploited Vulnerabilities catalogue")
        };
        make_hit(
            &self.decl.id,
            title,
            format!(
                "{CATALOG_URL}?search_api_fulltext={}",
                encode_query_plus(cve)
            ),
            "CISA lists a CVE in KEV when it has evidence of exploitation in the wild; this one is not listed, which means no such evidence has reached CISA, not that the flaw is harmless. Check NVD for the CVSS score and OSV for the fixed version.",
            (!day.is_empty()).then_some(day),
        )
    }

    pub fn map_rows(&self, rows: &[Value]) -> Vec<Hit> {
        let refs: Vec<&Value> = rows.iter().collect();
        self.map_row_refs(&refs)
    }

    fn map_row_refs(&self, rows: &[&Value]) -> Vec<Hit> {
        let mut results = Vec::new();
        for &row in rows {
            let Some(r) = row.as_object() else {
                continue;
            };
            let Some(cve) = r.get("cveID").and_then(Value::as_str) else {
                continue;
            };
            let added: String = r
                .get("dateAdded")
                .and_then(Value::as_str)
                .unwrap_or("")
                .chars()
                .take(10)
                .collect();
            let mut bits: Vec<String> = Vec::new();
            let vp = format!(
                "{} {}",
                r.get("vendorProject").and_then(Value::as_str).unwrap_or(""),
                r.get("product").and_then(Value::as_str).unwrap_or("")
            );
            let vp = vp.trim().to_owned();
            if !vp.is_empty() {
                bits.push(vp);
            }
            if let Some(vname) = r
                .get("vulnerabilityName")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                bits.push(vname.to_owned());
            }
            if !added.is_empty() {
                bits.push(format!("added to KEV {added}"));
            }
            if let Some(due) = r.get("dueDate").and_then(Value::as_str) {
                let due10: String = due.chars().take(10).collect();
                bits.push(format!("US federal remediation due {due10}"));
            }
            if let Some(ransom) = r
                .get("knownRansomwareCampaignUse")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                bits.push(format!("ransomware use: {}", ransom.to_ascii_lowercase()));
            }
            if let Some(action) = r
                .get("requiredAction")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                bits.push(action.to_owned());
            }
            if let Some(desc) = r
                .get("shortDescription")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                bits.push(desc.to_owned());
            }
            results.push(make_hit(
                &self.decl.id,
                format!("{cve}: known exploited in the wild (CISA KEV, added {added})"),
                format!(
                    "{CATALOG_URL}?search_api_fulltext={}",
                    encode_query_plus(cve)
                ),
                clip(&bits.join(" · "), 500),
                (!added.is_empty()).then_some(added),
            ));
        }
        results
    }
}

#[async_trait::async_trait]
impl Source for CisaKevEngine {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    /// Unified claim scale: `1.0` (`>= 0.8`, direct answer) for an explicit
    /// CVE ID; `0.5` (`0.0 < s < 0.8`, relevant non-direct) for exploited-in-the-wild
    /// keywords without a CVE ID; `0.0` otherwise.
    fn claims(&self, query: &str, _lang: Option<&str>) -> f32 {
        let q = clip_query(query);
        if CVE_RE.is_match(q) {
            1.0
        } else if EXPLOITED_ASK.is_match(q) {
            0.5
        } else {
            0.0
        }
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
        let feed = self.load_feed(cx, &budget).await?;
        if feed.rows.is_empty() {
            return Ok(hits_output(Vec::new()));
        }
        let ids: BTreeSet<String> = CVE_RE
            .find_iter(&query)
            .map(|m| m.as_str().to_ascii_uppercase())
            .collect();
        if !ids.is_empty() {
            let mut matched_rows: Vec<&Value> = Vec::new();
            let mut listed: BTreeSet<String> = BTreeSet::new();
            for row in &feed.rows {
                if let Some(cve) = row.get("cveID").and_then(Value::as_str) {
                    let upper = cve.to_ascii_uppercase();
                    if ids.contains(&upper) {
                        listed.insert(upper);
                        matched_rows.push(row);
                    }
                }
            }
            matched_rows.truncate(limit);
            let mut out = self.map_row_refs(&matched_rows);
            for cve in ids.difference(&listed) {
                if out.len() >= limit {
                    break;
                }
                out.push(self.not_listed(cve, &feed.version));
            }
            out.truncate(limit);
            return Ok(hits_output(out));
        }

        let clipped = clip_query(&query);
        let stripped = EXPLOITED_ASK.replace_all(clipped, " ");
        let mut words: Vec<String> = Vec::new();
        for m in TOKEN_RE.find_iter(&stripped) {
            let w = m.as_str().to_ascii_lowercase();
            if !STOP_WORDS.contains(w.as_str()) && w.len() > 2 && !words.contains(&w) {
                words.push(w);
            }
        }
        if words.is_empty() {
            return Ok(hits_output(Vec::new()));
        }
        let mut hits: Vec<&Value> = feed
            .rows
            .iter()
            .filter(|r| {
                let hay = format!(
                    "{} {} {}",
                    r.get("vendorProject").and_then(Value::as_str).unwrap_or(""),
                    r.get("product").and_then(Value::as_str).unwrap_or(""),
                    r.get("vulnerabilityName")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                )
                .to_lowercase();
                words.iter().all(|w| hay.contains(w.as_str()))
            })
            .collect();
        hits.sort_by(|a, b| {
            let da = a.get("dateAdded").and_then(Value::as_str).unwrap_or("");
            let db = b.get("dateAdded").and_then(Value::as_str).unwrap_or("");
            db.cmp(da)
        });
        hits.truncate(limit);
        Ok(hits_output(self.map_row_refs(&hits)))
    }
}
