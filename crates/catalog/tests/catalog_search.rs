//! Recall, CJK bilingual synonym, ID prefix boost, typo recovery, filtering, OAuth2 auth display,
//! health overlay, and `describe()` tests for `fsm-catalog`.
//!
//! Indexes real promoted manifests & protocol instances from
//! `crates/manifests/{sources,instances}/**/*.toml` (read-only) alongside protocol adapters
//! and sample `HostFn` entries.

use fsm_catalog::CatalogService;
use fsm_core::manifest::{InstanceFileDecl, ManifestDecl};
use fsm_core::{
    AuthMode, CacheHit, Capability, CatalogQuery, DownloadResult, FsmError, HealthRecord, HostFn,
    NullStore, OpAuthOverride, OpDecl, PageResult, Param, ParamType, ProtocolAdapter,
    ProtocolOutput, Registry, RetractionNotice, ReturnKind, ScholarlyWork, SearchResponse, Source,
    SourceDecl, SrcCx, SrcOutput, StoreApi,
};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

struct StubSource(SourceDecl);

#[async_trait::async_trait]
impl Source for StubSource {
    fn decl(&self) -> &SourceDecl {
        &self.0
    }
    async fn call(&self, _op: &str, _params: Value, _cx: &SrcCx) -> Result<SrcOutput, FsmError> {
        Ok(SrcOutput::default())
    }
}

struct StubProto {
    kind: &'static str,
    ops: Vec<OpDecl>,
}

#[async_trait::async_trait]
impl ProtocolAdapter for StubProto {
    fn kind(&self) -> &'static str {
        self.kind
    }
    fn ops(&self) -> &[OpDecl] {
        &self.ops
    }
    async fn call(
        &self,
        base_url: &str,
        op: &str,
        _params: Value,
        _cx: &SrcCx,
    ) -> Result<ProtocolOutput, FsmError> {
        Ok(ProtocolOutput::new(self.kind, base_url, op))
    }
}

static SAMPLE_HOST_API: &[HostFn] = &[
    HostFn::new(
        "web.search",
        "search(q: string, o?: {scope?: string; limit?: number}): Promise<SearchResponse>",
        "Federated keyless web and vertical search.",
        "await web.search(\"rust async\")",
    )
    .with_in_description(true)
    .with_egress(true),
    HostFn::new(
        "web.fetch",
        "fetch(url: string, o?: {render?: string; maxAgeHours?: number}): Promise<PageResult>",
        "Fetch and extract readable Markdown from a web page.",
        "await web.fetch(\"https://example.com\")",
    )
    .with_in_description(true)
    .with_egress(true),
    HostFn::new(
        "scholar.author",
        "author(qOrOrcid: string): Promise<AuthorProfile>",
        "Look up an academic researcher profile by name or ORCID.",
        "await scholar.author(\"0000-0002-1825-0097\")",
    )
    .with_egress(true),
    HostFn::new(
        "scholar.grants",
        "grants(q: string, o?: {sources?: string[]; limit?: number}): Promise<GrantRecord[]>",
        "Search research funding grants across NIH, NSF, CORDIS, and UKRI.",
        "await scholar.grants(\"crispr\")",
    )
    .with_egress(true),
    HostFn::new(
        "util.dedupe",
        "dedupe<T>(items: T[], keyFn?: (x: T) => string): T[]",
        "Remove duplicate items from an array.",
        "util.dedupe(items, x => x.url)",
    )
    .with_prelude_sync(true),
];

fn collect_toml_files(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            collect_toml_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "toml") {
            out.push(path);
        }
    }
}

fn register_sample_protocols(reg: &mut Registry) {
    for (kind, op2) in [
        ("ckan", "package"),
        ("socrata", "rows"),
        ("opendatasoft", "records"),
        ("discourse", "topic"),
        ("wordpress_rest", "post"),
        ("rss_atom", "latest"),
        ("activitypub_fediverse", "nodeinfo"),
        ("shopify_json", "products"),
        ("sitemap_llms", "list"),
    ] {
        let ops = vec![
            OpDecl::new(
                "search",
                format!("Search via {kind} protocol."),
                ReturnKind::Records,
            )
            .with_param(Param::new("query", ParamType::String, "Query").required())
            .with_param(Param::new("limit", ParamType::Int, "Limit").with_default(json!(10))),
            OpDecl::new(
                op2,
                format!("Secondary {op2} op for {kind}."),
                ReturnKind::Records,
            )
            .with_param(Param::new("id", ParamType::String, "Identifier").required()),
        ];
        reg.register_protocol(Arc::new(StubProto { kind, ops }))
            .unwrap();
    }
}

fn load_staged_registry() -> (Registry, usize) {
    let manifests_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("crates/manifests");

    let mut reg = Registry::new();
    register_sample_protocols(&mut reg);

    let mut seen_ids = HashSet::new();
    let mut loaded = 0usize;

    // I7: Scan promoted crates/manifests/{sources,instances},
    // recursively skipping missing/empty directories and deduplicating by source id.
    let mut files = Vec::new();
    for sub in ["sources", "instances"] {
        let dir = manifests_root.join(sub);
        if dir.is_dir() {
            collect_toml_files(&dir, &mut files);
        }
    }

    for path in files {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(manifest) = ManifestDecl::from_toml(&text) {
            let decl = manifest.to_source_decl();
            if seen_ids.insert(decl.id.clone()) && reg.register(Arc::new(StubSource(decl))).is_ok()
            {
                loaded += 1;
            }
        } else if let Ok(inst_file) = InstanceFileDecl::from_toml(&text) {
            let adapter_ops = reg
                .protocol(&inst_file.protocol)
                .map(|p| p.ops().to_vec())
                .unwrap_or_else(|| {
                    vec![
                        OpDecl::new("search", "Search instance", ReturnKind::Records)
                            .with_param(Param::new("query", ParamType::String, "Query").required()),
                    ]
                });
            for inst in inst_file.instance {
                let decl = inst.to_source_decl(&adapter_ops);
                if seen_ids.insert(decl.id.clone())
                    && reg.register(Arc::new(StubSource(decl))).is_ok()
                {
                    loaded += 1;
                }
            }
        }
    }

    // Add a synthetic OAuth2 source and a mixed-auth source to explicitly test key_envs() & OpAuthOverride::None.
    let mut oauth_decl = SourceDecl::new(
        "epo_ops_oauth_test",
        "EPO Open Patent Services (OAuth2)",
        "European Patent Office bibliographic and full-text patent search via OAuth2.",
    )
    .with_scopes(&["gov.patents"])
    .with_caps(&[Capability::Search, Capability::Patent])
    .with_op(
        OpDecl::new("search", "Search European patents", ReturnKind::Records)
            .with_param(Param::new("query", ParamType::String, "CQL patent query").required()),
    );
    oauth_decl.auth = AuthMode::RequiredKey {
        env: "SEARCH_MCP_EPO_OPS_KEY,SEARCH_MCP_EPO_OPS_SECRET".into(),
    };
    reg.register(Arc::new(StubSource(oauth_decl))).unwrap();

    let mut mixed_decl = SourceDecl::new(
        "census_mixed_test",
        "US Census Bureau (Mixed Auth)",
        "US demographic datasets with keyless metadata discovery and keyed data queries.",
    )
    .with_scopes(&["gov", "dataset"])
    .with_caps(&[Capability::Search, Capability::Dataset])
    .with_op(
        OpDecl::new("datasets", "Keyless dataset list", ReturnKind::Records)
            .with_auth(OpAuthOverride::None),
    )
    .with_op(
        OpDecl::new("query", "Keyed table query", ReturnKind::Records)
            .with_auth(OpAuthOverride::Required),
    );
    mixed_decl.auth = AuthMode::RequiredKey {
        env: "SEARCH_MCP_CENSUS_API_KEY".into(),
    };
    reg.register(Arc::new(StubSource(mixed_decl))).unwrap();

    (reg, loaded)
}

struct HealthStore(Vec<HealthRecord>);

#[async_trait::async_trait]
impl StoreApi for HealthStore {
    async fn cache_search(&self, _: &str, _: u32) -> Result<Vec<CacheHit>, FsmError> {
        Ok(vec![])
    }
    async fn get_page(&self, _: &str, _: Option<u64>) -> Result<Option<PageResult>, FsmError> {
        Ok(None)
    }
    async fn put_page(&self, _: &PageResult, _: u64) -> Result<(), FsmError> {
        Ok(())
    }
    async fn kv_put(&self, _: &str, _: &str, _: &Value, _: u64) -> Result<(), FsmError> {
        Ok(())
    }
    async fn kv_get(&self, _: &str, _: &str) -> Result<Option<Value>, FsmError> {
        Ok(None)
    }
    async fn search_cache_get(
        &self,
        _: &str,
        _: Option<u64>,
    ) -> Result<Option<(SearchResponse, u64)>, FsmError> {
        Ok(None)
    }
    async fn search_cache_put(&self, _: &str, _: &SearchResponse, _: u64) -> Result<(), FsmError> {
        Ok(())
    }
    async fn api_cache_get(
        &self,
        _: &str,
    ) -> Result<Option<(fsm_core::Bytes, Option<String>, Option<String>)>, FsmError> {
        Ok(None)
    }
    async fn api_cache_put(
        &self,
        _: &str,
        _: &str,
        _: &[u8],
        _: Option<&str>,
        _: Option<&str>,
        _: u64,
    ) -> Result<(), FsmError> {
        Ok(())
    }
    async fn scholar_get(&self, _: &str) -> Result<Option<ScholarlyWork>, FsmError> {
        Ok(None)
    }
    async fn scholar_put(&self, _: &str, _: &ScholarlyWork, _: u64) -> Result<(), FsmError> {
        Ok(())
    }
    async fn retraction_lookup(&self, _: &str) -> Result<Option<RetractionNotice>, FsmError> {
        Ok(None)
    }
    async fn retractions_replace_batch(
        &self,
        _: Vec<(String, RetractionNotice)>,
        _: Option<String>,
    ) -> Result<(), FsmError> {
        Ok(())
    }
    async fn sync_state_get(
        &self,
        _: &str,
    ) -> Result<Option<(Option<String>, i64, Option<i64>)>, FsmError> {
        Ok(None)
    }
    async fn sync_state_mark(
        &self,
        _: &str,
        _: Option<&str>,
        _: i64,
        _: Option<i64>,
    ) -> Result<(), FsmError> {
        Ok(())
    }
    async fn health_load_all(&self) -> Result<Vec<HealthRecord>, FsmError> {
        Ok(self.0.clone())
    }
    async fn health_save(&self, _: &HealthRecord) -> Result<(), FsmError> {
        Ok(())
    }
    async fn download_record(&self, _: &DownloadResult, _: u64) -> Result<(), FsmError> {
        Ok(())
    }
}

#[tokio::test]
async fn realistic_recall_over_staged_500_source_catalog() {
    let (reg, loaded) = load_staged_registry();
    assert!(
        loaded >= 400,
        "expected >= 400 staged sources, got {loaded}"
    );
    let svc = CatalogService::new(Arc::new(reg), SAMPLE_HOST_API, Arc::new(NullStore::new()));

    // 1. "clinical trials" -> clinicaltrials in top 3
    let hits = svc
        .search(CatalogQuery::new("clinical trials"))
        .await
        .unwrap();
    let ids: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
    assert!(
        ids.iter().take(3).any(|&id| id == "clinicaltrials"),
        "expected clinicaltrials in top 3, got {ids:?}"
    );

    // 2. "patent" & "专利" -> patent sources in top 5
    for q in ["patent", "专利"] {
        let hits = svc.search(CatalogQuery::new(q)).await.unwrap();
        let ids: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
        assert!(
            ids.iter().take(5).any(|id| matches!(
                *id,
                "uspto_odp"
                    | "epo_ops"
                    | "surechembl"
                    | "ipaustralia_patents"
                    | "epo_ops_oauth_test"
            )),
            "query {q:?} missed patent sources: {ids:?}"
        );
    }

    // 3. "seismic" & "地震" -> usgs_earthquakes in top 3
    for q in ["seismic", "地震"] {
        let hits = svc.search(CatalogQuery::new(q)).await.unwrap();
        let ids: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
        assert!(
            ids.iter().take(3).any(|&id| id == "usgs_earthquakes"),
            "query {q:?} expected usgs_earthquakes in top 3, got {ids:?}"
        );
    }

    // 4. "中文 期刊" -> bilingual Chinese academic/research/journal sources in top 10
    let zh_hits = svc.search(CatalogQuery::new("中文 期刊")).await.unwrap();
    let zh_ids: Vec<&str> = zh_hits.iter().map(|h| h.id.as_str()).collect();
    assert!(
        zh_ids.iter().any(|id| matches!(
            *id,
            "cuhk_rdr" | "pku_opendata" | "sciencedb_cn" | "depositar_tw" | "cnki" | "wanfang"
        )),
        "query '中文 期刊' expected Chinese academic/journal source in top 10, got {zh_ids:?}"
    );

    // 5. "CVE" & "漏洞" -> CVE / vulnerability sources in top 5
    for q in ["CVE", "漏洞"] {
        let hits = svc.search(CatalogQuery::new(q)).await.unwrap();
        let ids: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
        assert!(
            ids.iter().take(5).any(|id| matches!(
                *id,
                "nvd" | "mitre_cve_awg" | "ghsa" | "epss" | "shodan_internetdb"
            )),
            "query {q:?} missed CVE sources: {ids:?}"
        );
    }

    // 6. Host API discoverability
    let api_hits = svc
        .search(CatalogQuery::new("orcid researcher"))
        .await
        .unwrap();
    assert!(
        api_hits
            .iter()
            .any(|h| h.id == "scholar.author" && h.kind == "api"),
        "expected scholar.author api hit, got {:?}",
        api_hits
            .iter()
            .map(|h| (&h.id, &h.kind))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn id_prefix_boost_and_typo_recovery_fallback() {
    let (reg, _) = load_staged_registry();
    let svc = CatalogService::new(Arc::new(reg), SAMPLE_HOST_API, Arc::new(NullStore::new()));

    // Exact / prefix boost
    let pref = svc.search(CatalogQuery::new("clin")).await.unwrap();
    assert_eq!(
        pref[0].id,
        "clinicaltrials",
        "got {:?}",
        pref.iter().map(|h| &h.id).collect::<Vec<_>>()
    );

    let web_pref = svc.search(CatalogQuery::new("web.")).await.unwrap();
    assert!(
        web_pref.iter().take(2).all(|h| h.id.starts_with("web.")),
        "expected web.* at top, got {:?}",
        web_pref.iter().map(|h| &h.id).collect::<Vec<_>>()
    );

    // Typo recovery via rapidfuzz fallback
    for (typo, want) in [
        ("clincaltrails", "clinicaltrials"),
        ("opendatasfot", "opendatasoft"),
        ("shopyfi", "shopify_json"),
        ("pubmde", "pubmed"),
        ("clincal trals", "clinicaltrials"),
    ] {
        let hits = svc.search(CatalogQuery::new(typo)).await.unwrap();
        let ids: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
        assert!(
            ids.iter().take(5).any(|&id| id == want),
            "typo {typo:?} failed to recover {want:?}; got {ids:?}"
        );
    }
}

#[tokio::test]
async fn filters_group_capability_keyless_and_oauth2_auth_display() {
    let (reg, _) = load_staged_registry();
    let svc = CatalogService::new(Arc::new(reg), SAMPLE_HOST_API, Arc::new(NullStore::new()));

    // group = "protocol" with empty query lists all registered protocols
    let mut q_proto = CatalogQuery::new("");
    q_proto.group = Some("protocol".into());
    q_proto.limit = 20;
    let protos = svc.search(q_proto).await.unwrap();
    assert_eq!(protos.len(), 9);
    assert!(protos.iter().all(|h| h.kind == "protocol"));

    // group = "api"
    let mut q_api = CatalogQuery::new("");
    q_api.group = Some("api".into());
    let apis = svc.search(q_api).await.unwrap();
    assert_eq!(apis.len(), SAMPLE_HOST_API.len());
    assert!(apis.iter().all(|h| h.kind == "api"));

    // capability = "grants" (plural normalized to "grant")
    let mut q_grant = CatalogQuery::new("research");
    q_grant.capability = Some("grants".into());
    let grants = svc.search(q_grant).await.unwrap();
    assert!(!grants.is_empty());
    assert!(
        grants
            .iter()
            .any(|h| h.id == "nih_reporter" || h.id == "nsf_awards" || h.id == "scholar.grants")
    );

    // keyless_only excludes RequiredKey-only sources (epo_ops_oauth_test) but keeps mixed-auth (census_mixed_test)
    let mut q_pat = CatalogQuery::new("epo_ops_oauth_test");
    q_pat.keyless_only = false;
    let all_pat = svc.search(q_pat).await.unwrap();
    let oauth_hit = all_pat
        .iter()
        .find(|h| h.id == "epo_ops_oauth_test")
        .expect("oauth hit");
    assert!(
        oauth_hit.auth.contains("SEARCH_MCP_EPO_OPS_KEY")
            && oauth_hit.auth.contains("SEARCH_MCP_EPO_OPS_SECRET"),
        "OAuth2 auth should list both env vars: {}",
        oauth_hit.auth
    );

    let mut q_pat_kl = CatalogQuery::new("epo_ops_oauth_test");
    q_pat_kl.keyless_only = true;
    let kl_pat = svc.search(q_pat_kl).await.unwrap();
    assert!(kl_pat.iter().all(|h| h.id != "epo_ops_oauth_test"));

    let mut q_mixed = CatalogQuery::new("census_mixed_test");
    q_mixed.keyless_only = true;
    let mixed = svc.search(q_mixed).await.unwrap();
    assert!(mixed.iter().any(|h| h.id == "census_mixed_test"));
}

#[tokio::test]
async fn describe_sources_ops_protocols_apis_health_and_empty_registry() {
    let (reg, _) = load_staged_registry();
    let store = Arc::new(HealthStore(vec![HealthRecord::new(
        "clinicaltrials",
        "benched",
        1_700_000_000,
    )]));
    let svc = CatalogService::new(Arc::new(reg), SAMPLE_HOST_API, store);

    // 1. Source with live health overlay
    let ct = svc.describe("clinicaltrials").await.unwrap();
    assert_eq!(ct.hit.id, "clinicaltrials");
    assert_eq!(ct.hit.kind, "source");
    assert_eq!(ct.hit.health, "benched");
    assert!(!ct.params.is_empty());
    assert!(ct.scopes.iter().any(|s| s == "academic.trial"));

    // 2. Specific source op (<source>.<op>)
    let ct_study = svc.describe("clinicaltrials.study").await.unwrap();
    assert_eq!(ct_study.hit.id, "clinicaltrials.study");
    assert_eq!(ct_study.hit.health, "benched");
    assert!(
        ct_study
            .params
            .iter()
            .any(|p| p.name == "nct" && p.required)
    );

    // 3. Protocol and protocol op
    let ckan = svc.describe("ckan").await.unwrap();
    assert_eq!(ckan.hit.kind, "protocol");
    let ckan_pkg = svc.describe("ckan.package").await.unwrap();
    assert_eq!(ckan_pkg.hit.id, "ckan.package");
    assert!(ckan_pkg.params.iter().any(|p| p.name == "id"));

    // 4. Host API with parsed signature params
    let wf = svc.describe("web.fetch").await.unwrap();
    assert_eq!(wf.hit.kind, "api");
    assert_eq!(wf.params.len(), 2);
    assert_eq!(wf.params[0].name, "url");
    assert!(wf.params[0].required);
    assert_eq!(wf.params[1].name, "o");
    assert!(!wf.params[1].required);

    // 5. Unknown id suggests close fuzzy match
    let err = svc.describe("clincaltrails").await.unwrap_err().to_string();
    assert!(
        err.contains("clinicaltrials"),
        "expected suggestion in {err:?}"
    );

    // 6. Empty registry & host_api never panics
    let empty = CatalogService::new(Arc::new(Registry::new()), &[], Arc::new(NullStore::new()));
    assert!(
        empty
            .search(CatalogQuery::new("anything"))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        empty
            .search(CatalogQuery::new(""))
            .await
            .unwrap()
            .is_empty()
    );

    // 7. Wire JSON fidelity of CatalogDetail (M2 flat serialization, camelCase)
    let flat_json = serde_json::to_value(&ct).unwrap();
    assert_eq!(flat_json["id"], "clinicaltrials");
    assert_eq!(flat_json["health"], "benched");
    assert!(
        flat_json.get("hit").is_none(),
        "CatalogDetail must flatten hit"
    );
    assert!(flat_json["params"].is_array());
}

#[tokio::test]
async fn scope_synonyms_nfkc_disambiguation_and_latency_sanity() {
    let (mut reg, _) = load_staged_registry();

    // Register a source with the SAME id as a protocol adapter ("ckan") to test disambiguation
    let shadow_src = SourceDecl::new(
        "ckan",
        "CKAN Demo Source",
        "A demo source sharing an id with the ckan protocol adapter.",
    )
    .with_scopes(&["dataset"])
    .with_caps(&[Capability::Search]);
    reg.register(Arc::new(StubSource(shadow_src))).unwrap();

    let svc = CatalogService::new(Arc::new(reg), SAMPLE_HOST_API, Arc::new(NullStore::new()));

    // Bare "ckan" resolves source; "protocol:ckan" explicitly resolves protocol adapter
    assert_eq!(svc.describe("ckan").await.unwrap().hit.kind, "source");
    assert_eq!(
        svc.describe("protocol:ckan").await.unwrap().hit.kind,
        "protocol"
    );

    // Full-width NFKC query + ZH synonym ("ＣＶＥ 漏洞")
    let cve_hits = svc.search(CatalogQuery::new("ＣＶＥ 漏洞")).await.unwrap();
    assert!(
        cve_hits
            .iter()
            .take(5)
            .any(|h| matches!(h.id.as_str(), "nvd" | "mitre_cve_awg" | "ghsa" | "epss")),
        "NFKC CVE query missed: {:?}",
        cve_hits.iter().map(|h| &h.id).collect::<Vec<_>>()
    );

    // Group synonym filtering ("法律" -> gov.law, "papers" -> academic)
    let mut q_law = CatalogQuery::new("");
    q_law.group = Some("法律".into());
    q_law.limit = 20;
    let law_hits = svc.search(q_law).await.unwrap();
    assert!(
        !law_hits.is_empty(),
        "expected gov.law sources for group='法律'"
    );

    let mut q_papers = CatalogQuery::new("researcher");
    q_papers.group = Some("papers".into());
    let paper_hits = svc.search(q_papers).await.unwrap();
    assert!(
        paper_hits.iter().any(|h| h.id == "scholar.author"),
        "group='papers' should include scholar.* APIs too"
    );

    // Limit = 0 defaults to 10; small limit respected even with fuzzy fallback
    let mut q_zero = CatalogQuery::new("patent");
    q_zero.limit = 0;
    assert_eq!(svc.search(q_zero).await.unwrap().len(), 10);

    let mut q_two = CatalogQuery::new("clincaltrails");
    q_two.limit = 2;
    assert!(svc.search(q_two).await.unwrap().len() <= 2);

    // Latency sanity over 50 realistic + typo queries
    let queries = [
        "clinical trials",
        "patent",
        "seismic",
        "中文 期刊",
        "CVE",
        "clincaltrails",
        "weather forecast",
        "opendatasoft",
        "stock filings",
        "web.fetch",
    ];
    let start = std::time::Instant::now();
    for _ in 0..5 {
        for q in queries {
            let res = svc.search(CatalogQuery::new(q)).await.unwrap();
            assert!(!res.is_empty(), "empty results for {q:?}");
        }
    }
    let elapsed = start.elapsed();
    // Generous debug-mode budget (50 queries in < 5s -> < 100ms/query debug; release is < 0.2ms)
    assert!(
        elapsed.as_secs() < 5,
        "50 catalog queries took too long: {elapsed:?}"
    );
}

struct FailingHealthStore;

#[async_trait::async_trait]
impl StoreApi for FailingHealthStore {
    async fn cache_search(&self, _: &str, _: u32) -> Result<Vec<CacheHit>, FsmError> {
        Ok(vec![])
    }
    async fn get_page(&self, _: &str, _: Option<u64>) -> Result<Option<PageResult>, FsmError> {
        Ok(None)
    }
    async fn put_page(&self, _: &PageResult, _: u64) -> Result<(), FsmError> {
        Ok(())
    }
    async fn kv_put(&self, _: &str, _: &str, _: &Value, _: u64) -> Result<(), FsmError> {
        Ok(())
    }
    async fn kv_get(&self, _: &str, _: &str) -> Result<Option<Value>, FsmError> {
        Ok(None)
    }
    async fn search_cache_get(
        &self,
        _: &str,
        _: Option<u64>,
    ) -> Result<Option<(SearchResponse, u64)>, FsmError> {
        Ok(None)
    }
    async fn search_cache_put(&self, _: &str, _: &SearchResponse, _: u64) -> Result<(), FsmError> {
        Ok(())
    }
    async fn api_cache_get(
        &self,
        _: &str,
    ) -> Result<Option<(fsm_core::Bytes, Option<String>, Option<String>)>, FsmError> {
        Ok(None)
    }
    async fn api_cache_put(
        &self,
        _: &str,
        _: &str,
        _: &[u8],
        _: Option<&str>,
        _: Option<&str>,
        _: u64,
    ) -> Result<(), FsmError> {
        Ok(())
    }
    async fn scholar_get(&self, _: &str) -> Result<Option<ScholarlyWork>, FsmError> {
        Ok(None)
    }
    async fn scholar_put(&self, _: &str, _: &ScholarlyWork, _: u64) -> Result<(), FsmError> {
        Ok(())
    }
    async fn retraction_lookup(&self, _: &str) -> Result<Option<RetractionNotice>, FsmError> {
        Ok(None)
    }
    async fn retractions_replace_batch(
        &self,
        _: Vec<(String, RetractionNotice)>,
        _: Option<String>,
    ) -> Result<(), FsmError> {
        Ok(())
    }
    async fn sync_state_get(
        &self,
        _: &str,
    ) -> Result<Option<(Option<String>, i64, Option<i64>)>, FsmError> {
        Ok(None)
    }
    async fn sync_state_mark(
        &self,
        _: &str,
        _: Option<&str>,
        _: i64,
        _: Option<i64>,
    ) -> Result<(), FsmError> {
        Ok(())
    }
    async fn health_load_all(&self) -> Result<Vec<HealthRecord>, FsmError> {
        Err(FsmError::internal("store", "simulated sqlite I/O error"))
    }
    async fn health_save(&self, _: &HealthRecord) -> Result<(), FsmError> {
        Ok(())
    }
    async fn download_record(&self, _: &DownloadResult, _: u64) -> Result<(), FsmError> {
        Ok(())
    }
}

fn build_self_contained_registry() -> Registry {
    let mut reg = Registry::new();
    register_sample_protocols(&mut reg);

    let make = |id: &str,
                name: &str,
                summary: &str,
                aliases: &[&str],
                scopes: &[&str],
                caps: &[Capability]| {
        let mut d = SourceDecl::new(id, name, summary)
            .with_scopes(scopes)
            .with_caps(caps);
        d.aliases = aliases.iter().map(|s| (*s).to_owned()).collect();
        d
    };

    let sources = [
        make(
            "clinicaltrials",
            "ClinicalTrials.gov",
            "Registry and results database of clinical studies and trials worldwide.",
            &["ctgov", "nct"],
            &["academic.trial", "academic.biomed"],
            &[Capability::Search, Capability::Lookup],
        ),
        make(
            "uspto_odp",
            "USPTO Open Data Portal",
            "United States Patent and Trademark Office patent filings and applications.",
            &["uspto", "patents"],
            &["gov.patents"],
            &[Capability::Search, Capability::Patent],
        ),
        make(
            "usgs_earthquakes",
            "USGS Earthquake Hazards",
            "Real-time global earthquake catalog and seismic event feed.",
            &["usgs", "quakes"],
            &["reference.geo", "gov"],
            &[Capability::Search, Capability::Dataset],
        ),
        make(
            "nvd",
            "NIST National Vulnerability Database",
            "CVE security vulnerability advisories and CVSS severity scores.",
            &["nist_nvd", "cve"],
            &["security"],
            &[Capability::Search, Capability::Lookup],
        ),
        make(
            "sciencedb_cn",
            "ScienceDB China",
            "中文科研数据与学术期刊论文开放仓储 (Chinese scientific journal and research data repository).",
            &["科学数据银行"],
            &["academic", "dataset"],
            &[Capability::Search, Capability::Dataset],
        ),
        make(
            "pubmed",
            "PubMed / MEDLINE",
            "Biomedical literature and life sciences journal articles from NCBI.",
            &[],
            &["academic.biomed"],
            &[Capability::Search, Capability::Lookup],
        ),
    ];

    for decl in sources {
        reg.register(Arc::new(StubSource(decl))).unwrap();
    }
    reg
}

/// I7 + Missing Test #13: Self-contained corpus test with zero dependency on `crates/manifests/`.
#[tokio::test]
async fn self_contained_catalog_corpus_without_manifests_dir() {
    let reg = build_self_contained_registry();
    let svc = CatalogService::new(Arc::new(reg), SAMPLE_HOST_API, Arc::new(NullStore::new()));

    // Bilingual EN/ZH synonym recall
    assert_eq!(
        svc.search(CatalogQuery::new("clinical trials"))
            .await
            .unwrap()[0]
            .id,
        "clinicaltrials"
    );
    assert_eq!(
        svc.search(CatalogQuery::new("专利")).await.unwrap()[0].id,
        "uspto_odp"
    );
    assert_eq!(
        svc.search(CatalogQuery::new("地震")).await.unwrap()[0].id,
        "usgs_earthquakes"
    );
    assert_eq!(
        svc.search(CatalogQuery::new("漏洞")).await.unwrap()[0].id,
        "nvd"
    );
    assert!(
        svc.search(CatalogQuery::new("中文 期刊"))
            .await
            .unwrap()
            .iter()
            .take(3)
            .any(|h| h.id == "sciencedb_cn")
    );

    // Prefix & typo recovery
    assert_eq!(
        svc.search(CatalogQuery::new("clin")).await.unwrap()[0].id,
        "clinicaltrials"
    );
    assert_eq!(
        svc.search(CatalogQuery::new("clincaltrails"))
            .await
            .unwrap()[0]
            .id,
        "clinicaltrials"
    );
    assert_eq!(
        svc.search(CatalogQuery::new("opendatasfot")).await.unwrap()[0].id,
        "opendatasoft"
    );
}

/// M5 + Missing Test #13: `health_load_all` failure degrades gracefully to `"ok"` instead of failing catalog search/describe.
#[tokio::test]
async fn catalog_survives_health_store_error() {
    let reg = build_self_contained_registry();
    let svc = CatalogService::new(Arc::new(reg), SAMPLE_HOST_API, Arc::new(FailingHealthStore));

    let hits = svc
        .search(CatalogQuery::new("clinical trials"))
        .await
        .expect("search must succeed when health store fails");
    assert!(!hits.is_empty());
    assert_eq!(hits[0].id, "clinicaltrials");
    assert_eq!(hits[0].health, "ok");

    let detail = svc
        .describe("clinicaltrials")
        .await
        .expect("describe must succeed when health store fails");
    assert_eq!(detail.hit.id, "clinicaltrials");
    assert_eq!(detail.hit.health, "ok");
}

/// Regression test over 30 EN/ZH discovery queries verifying top-5 relevance and zero pollution.
#[tokio::test]
async fn thirty_bilingual_discovery_queries_rank_top_5() {
    let (reg, _) = load_staged_registry();
    static HOST_API_WITH_CATALOG: &[HostFn] = &[
        HostFn::new(
            "catalog.search",
            "search(q: string, o?: {limit?: number}): Promise<CatalogHit[]>",
            "Search the local source, protocol, and API catalog.",
            r#"const hits = await catalog.search("clinical trials", {keylessOnly: true, limit: 5});"#,
        )
        .with_in_description(true),
        HostFn::new(
            "web.search",
            "search(q: string, o?: {scope?: string; limit?: number}): Promise<SearchResponse>",
            "Federated keyless web and vertical search.",
            r#"await web.search("rust async")"#,
        )
        .with_in_description(true)
        .with_egress(true),
        HostFn::new(
            "scholar.author",
            "author(qOrOrcid: string): Promise<AuthorProfile>",
            "Look up an academic researcher profile by name or ORCID.",
            r#"await scholar.author("0000-0002-1825-0097")"#,
        )
        .with_egress(true),
    ];
    let svc = CatalogService::new(
        Arc::new(reg),
        HOST_API_WITH_CATALOG,
        Arc::new(NullStore::new()),
    );

    let cases: &[(&str, &[&str], &[&str])] = &[
        // (query, must_have_in_top_5, must_NOT_have_in_top_5)
        (
            "clinical trials",
            &["clinicaltrials", "isrctn_trials"],
            &["catalog.search"],
        ),
        (
            "专利",
            &["uspto_odp", "epo_ops", "surechembl", "ipaustralia_patents"],
            &["shodan_internetdb", "greynoise_community"],
        ),
        (
            "seismic events",
            &["usgs_earthquakes"],
            &["thesportsdb_free"],
        ),
        (
            "中文 期刊",
            &[
                "ntu_ir_tw",
                "sinica_ir_tw",
                "cuhk_rdr",
                "pku_opendata",
                "sciencedb_cn",
            ],
            &["un_comtrade"],
        ),
        ("CVE", &["nvd", "osv", "mitre_cve_awg", "ghsa"], &[]),
        (
            "museum collections",
            &[
                "met_museum",
                "artic_museum",
                "harvard_art_museums",
                "rijksmuseum_ld",
                "smithsonian_oa",
            ],
            &[],
        ),
        (
            "government spending",
            &["usaspending_gov", "treasury_fiscal"],
            &[],
        ),
        (
            "protein structure",
            &["alphafold_db", "protein_databank", "uniprot"],
            &[],
        ),
        (
            "法律法规",
            &[
                "courtlistener",
                "eurlex_sparql",
                "uk_legislation",
                "ecfr_us_law",
                "legifrance_open",
            ],
            &[],
        ),
        (
            "气象数据",
            &["met_norway", "nws_weather", "noaa_ncei_geoportal"],
            &["cessda_social"],
        ),
        ("earthquakes", &["usgs_earthquakes"], &[]),
        ("drug safety FDA", &["openfda", "fda_press_rss"], &[]),
        (
            "open access preprints",
            &[
                "arxiv",
                "eprint_iacr",
                "hal",
                "biorxiv",
                "medrxiv",
                "psyarxiv",
            ],
            &[],
        ),
        (
            "SEC filings",
            &["sec_xbrl_facts", "eastmoney_cn", "hkex_news"],
            &[],
        ),
        (
            "exchange rates currency",
            &["frankfurter", "cfets", "bis_stats", "ecb_data"],
            &[],
        ),
        (
            "package vulnerabilities",
            &["osv", "deps_dev", "ghsa", "nvd"],
            &[],
        ),
        (
            "research grants funding",
            &[
                "nsf_awards",
                "grants_gov_us",
                "nih_reporter",
                "ukri",
                "cordis",
            ],
            &[],
        ),
        (
            "biomedical literature",
            &["pubmed", "europepmc", "pubtator3", "plos"],
            &[],
        ),
        (
            "archived web pages",
            &["wayback_cdx", "archive_org", "common_crawl"],
            &[],
        ),
        ("space rocket launches", &["launch_library2"], &["up_space"]),
        (
            "satellite imagery earth observation",
            &["copernicus_stac", "nasa_cmr"],
            &[],
        ),
        (
            "macroeconomic GDP inflation",
            &[
                "imf",
                "bls_public",
                "wdi",
                "fred",
                "eurostat",
                "dbnomics_macro",
            ],
            &[],
        ),
        (
            "学术 论文 检索",
            &[
                "crossref",
                "openalex",
                "semanticscholar",
                "openaire",
                "arxiv",
                "europepmc",
                "pubmed",
                "doaj",
            ],
            &[],
        ),
        (
            "临床试验",
            &["clinicaltrials", "isrctn_trials"],
            &["catalog.search"],
        ),
        ("安全漏洞", &["osv", "nvd", "ghsa", "mitre_cve_awg"], &[]),
        (
            "汇率 外汇",
            &["frankfurter", "cfets", "bis_stats", "ecb_data"],
            &[],
        ),
        (
            "公司 企业 注册",
            &["gleif", "uk_companies_house", "fr_entreprises", "edinet_jp"],
            &[],
        ),
        (
            "开源 软件包 依赖",
            &[
                "deps_dev",
                "ecosystems_pkg",
                "gitlab_projects",
                "artifacthub_k8s",
            ],
            &[],
        ),
        (
            "博物馆 艺术 馆藏",
            &[
                "met_museum",
                "artic_museum",
                "harvard_art_museums",
                "rijksmuseum_ld",
                "europeana",
            ],
            &[],
        ),
        (
            "clinical trial protocol",
            &["clinicaltrials", "isrctn_trials"],
            &["catalog.search"],
        ),
    ];

    assert_eq!(cases.len(), 30);
    let mut failures = Vec::new();
    for &(q, expected_any, forbidden) in cases {
        let mut cq = CatalogQuery::new(q);
        cq.limit = 5;
        let hits = svc.search(cq).await.unwrap();
        let top5: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
        if !top5.iter().any(|id| expected_any.contains(id)) {
            failures.push(format!(
                "query {q:?} expected one of {expected_any:?} in top 5, got {top5:?}"
            ));
        }
        for &bad in forbidden {
            if top5.contains(&bad) {
                failures.push(format!("query {q:?} polluted top 5 with {bad:?}: {top5:?}"));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "catalog 30-query failures:\n{}",
        failures.join("\n")
    );
}
