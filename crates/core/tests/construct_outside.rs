//! M1: every public `#[non_exhaustive]` type is constructible from another
//! crate (struct literals are E0639 there), and every shared trait can be
//! implemented and boxed outside `fsm-core`.
//!
//! `every_non_exhaustive_item_is_covered` scans `src/` and fails when a new
//! non-exhaustive type is not named in this file.

use async_trait::async_trait;
use bytes::Bytes;
use fsm_core::config::{EnvDisposition, EnvOrigin, EnvSources, EnvVarSpec};
use fsm_core::fixture::{Exchange, Fixture, FixtureRequest, FixtureResponse, ReplayHit, ReplayNet};
use fsm_core::manifest::{
    ArrayStyle, AuthDecl, AuthIn, AuthKind, AuthModeManifest, BodyKind, BodySpec, ClaimsManifest,
    ConstQueryVal, CsvOpts, HtmlSpec, InstanceDecl, InstanceFileDecl, ManifestDecl,
    ManifestTransport, MapSpec, OAuthStyle, OnEmpty, OpSpec, PaginateSpec, PaginateStyle,
    ParamDecl, ParamLocation, PathEncode, PreAuth, PreStepDecl, ProbeManifest, ResponseFormat,
};
use fsm_core::text::{JsonShrink, TextCut};
use fsm_core::*;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

macro_rules! defaults {
    ($($t:ty),* $(,)?) => {
        $( let _: $t = <$t>::default(); )*
    };
}

#[test]
fn wire_types_construct_outside_the_crate() {
    defaults!(
        HitKind,
        FetchMethod,
        Hit,
        SearchResponse,
        SearchAgentSummary,
        AgentLaneReport,
        AskMode,
        AskPassage,
        AskSource,
        AskResponse,
        PageResult,
        ResearchBrief,
        InlineImageMeta,
        HttpResponse,
        StructuredMeta,
        CompareItem,
        DownloadResult,
        DocSlice,
        DocSection,
        SheetInfo,
        DocOutline,
        WorkIds,
        AuthorRef,
        OaLocation,
        WorkVersion,
        ScholarlyWork,
        SearchMeta,
        WorkResult,
        GraphNotice,
        CitationGraph,
        FulltextSection,
        FulltextResult,
        RetractionNotice,
        SciteTallies,
        IntegrityReport,
        AuthorProfile,
        OrgProfile,
        VenueProfile,
        GrantRecord,
        SourceOutput,
        ProtocolOutput,
        CacheHit,
        CatalogHit,
        CatalogParamDoc,
        CatalogDetail,
    );
    let mut hit = Hit::new("Tokio", "https://tokio.rs");
    hit.snippet = "async runtime".into();
    hit.kind = HitKind::Web;
    let mut resp = SearchResponse::new("rust", "web");
    resp.results.push(hit);
    let _ = SearchAgentSummary::answered("42", "api", "m", 10);
    let _ = AskPassage::new("p", "t", "https://x");
    let _ = AskSource::new("t", "https://x");
    let _ = AskResponse::new("q", "fast");
    let page = PageResult::new("https://tokio.rs", "Tokio", "中文 text", FetchMethod::Http);
    assert_eq!(page.total_chars, 7);
    let _ = ResearchBrief::new("rust", resp);
    let _ = InlineImageMeta::new("https://x/a.png", "image/png", 10, "ab");
    let _ = HttpResponse::new("https://x", 200, "body");
    let _ = StructuredMeta::new("https://x");
    let _ = CompareItem::new("https://x");
    let _ = DownloadResult::new("https://x", "/tmp/x", "application/pdf", 1, "ab");
    let _ = DocSlice::new("a.pdf", "pdf", "text");
    let _ = DocSection::new("Intro", 1, 0);
    let _ = SheetInfo::new("Sheet1", 2, 3);
    let _ = DocOutline::new("a.pdf", "pdf");
    let _ = AuthorRef::new("Ada");
    let _ = OaLocation::new("gold");
    let _ = WorkVersion::new("preprint", "https://arxiv.org/abs/1");
    let work = ScholarlyWork::new("Attention", "https://arxiv.org/abs/1706.03762");
    let _ = SearchMeta::new("attention");
    let _ = WorkResult::new(vec![work.clone()], SearchMeta::new("q"));
    let _ = GraphNotice::new("partial");
    let _ = CitationGraph::new("q", "doi", work.clone());
    let _ = FulltextSection::new("Intro", "text");
    let _ = FulltextResult::new("doi:10.1/x", true);
    let _ = RetractionNotice::new("Retraction", "crossref");
    let _ = SciteTallies::new(1, 2, 3);
    let _ = IntegrityReport::new(Some("10.1/x".into()));
    let _ = AuthorProfile::new("Ada");
    let _ = OrgProfile::new("MIT");
    let _ = VenueProfile::new("ICML");
    let _ = GrantRecord::new("R01", "Title", "NIH", "nih_reporter");
    let _ = SourceOutput::new("arxiv", "search");
    let _ = ProtocolOutput::new("oai_pmh", "https://x", "identify");
    let _ = CacheHit::new("https://x", "T", "s");
    let hit = CatalogHit::new("arxiv.search", "op", "Search arXiv");
    let _ = CatalogParamDoc::new("query", "string", "terms");
    let _ = CatalogDetail::new(hit);
    let _ = WorkIds::default().is_empty();
}

#[test]
fn api_and_config_types_construct_outside_the_crate() {
    defaults!(
        Freshness,
        RenderMode,
        GraphDirection,
        SearchRequest,
        AskOpts,
        FetchOpts,
        RawRequestOpts,
        ResearchOpts,
        ScholarQuery,
        GraphOpts,
        FulltextOpts,
        CiteInput,
        CiteStyle,
        EntityKind,
        CatalogQuery,
        HealthRecord,
        FetchOutcome,
        SsrfResolveMode,
        RawHttpPolicy,
        SafeSearch,
        Config,
        EnvOrigin,
        EnvDisposition,
        EnvVarSpec,
        EnvSources,
        Registry,
    );
    let mut req = SearchRequest::new("rust").with_scope("code");
    req.limit = 5;
    let _ = RawRequestOpts::new("https://x");
    let _ = ScholarQuery::new("attention");
    let _ = CatalogQuery::new("arxiv");
    let _ = HealthRecord::new("bing", "healthy", 0);
    let _ = CiteInput::Id("10.1/x".into());
    let _ = FetchOutcome::Page(PageResult::default());
    let _ = EnvDisposition::Rename { to: "SEARCH_MCP_X" };
    let mut src = EnvSources::new("/home/u");
    src.cwd_dotenv = Some("SEARCH_MCP_REGION=de-de".into());
    let cfg = Config::from_sources(&src).unwrap();
    assert_eq!(cfg.region, "de-de");
    let _ = Config::defaults_for_home(std::path::Path::new("/home/u"));
}

#[test]
fn source_host_net_types_construct_outside_the_crate() {
    defaults!(
        Availability,
        TransportProfile,
        PoolRole,
        AuthMode,
        ParamType,
        ReturnKind,
        Capability,
        SourceLimits,
        Param,
        ProbeSpec,
        OpDecl,
        SourceDecl,
        SrcOutput,
        SrcCx,
        HostFn,
        RenderKind,
        BinaryHandle,
        HostValue,
        CallCx,
        HttpMethod,
        HttpRequest,
        HttpBytes,
        NetCx,
        ScopeDef,
        TextCut,
        JsonShrink,
    );
    let _ = Availability::NeedsKey {
        env: "SEARCH_MCP_X_API_KEY".into(),
    };
    let _ = PoolRole::Locale("zh".into());
    let _ = AuthMode::OptionalKey {
        env: "SEARCH_MCP_X_API_KEY".into(),
        route_requires_key: false,
    };
    let decl = SourceDecl::new("fake", "Fake", "A fake source")
        .with_scopes(&["web"])
        .with_caps(&[Capability::Search])
        .with_op(
            OpDecl::new("search", "Search", ReturnKind::Hits)
                .with_param(Param::new("query", ParamType::String, "terms").required())
                .with_param(
                    Param::new("limit", ParamType::Int, "n")
                        .with_default(json!(10))
                        .with_range(1.0, 50.0),
                )
                .with_param(
                    Param::new("sort", ParamType::String, "s").with_enum(&["relevance", "date"]),
                )
                .with_example("sources.call(\"fake.search\", {query: \"x\"})")
                .with_probe(ProbeSpec::new(json!({"query": "x"}), 1)),
        );
    let mut limits = SourceLimits::new(1.0, 2, 2, 5000, 60);
    limits.host_bucket = Some("fake".into());
    let _ = SrcOutput::Records {
        items: vec![],
        hits: None,
        next_cursor: None,
    };
    let _ = SrcCx::new(CancellationToken::new(), Duration::from_secs(5));
    let _ = SrcCx::from_config(
        &Config::default(),
        CancellationToken::new(),
        Duration::from_secs(5),
    );
    const ROW: HostFn = HostFn::new(
        "web.search",
        "(q: string) => Hit[]",
        "Search the web.",
        "await web.search('x')",
    )
    .with_in_description(true)
    .with_egress(true);
    assert_eq!(ROW.namespace(), "web");
    let _ = BinaryHandle::new("b1", "image/png", Bytes::from_static(b"x"));
    let _ = HostValue::new(json!({})).with_binary(BinaryHandle::default());
    let _ = HostValue::tagged(json!({}), RenderKind::PageResult);
    let cx = CallCx::new("s1", Instant::now() + Duration::from_secs(30));
    assert!(cx.remaining() > Duration::ZERO);
    let _ = HttpRequest::get("https://x")
        .header("accept", "*/*")
        .query_param("q", "1")
        .for_source("fake")
        .with_max_bytes(10);
    let _ = HttpRequest::post("https://x", Bytes::from_static(b"{}"));
    let _ = HttpBytes::new("https://x", 200, vec![], Bytes::new());
    let _ = NetCx::new(CancellationToken::new(), Duration::from_secs(1));
    let _ = scopes::SCOPES.iter().find(|s: &&ScopeDef| s.id == "web");
    let _ = decl;
}

#[test]
fn manifest_and_fixture_types_construct_outside_the_crate() {
    defaults!(
        ManifestTransport,
        ResponseFormat,
        BodyKind,
        ParamLocation,
        AuthModeManifest,
        PaginateStyle,
        ManifestDecl,
        AuthDecl,
        ClaimsManifest,
        OpSpec,
        ParamDecl,
        PreStepDecl,
        MapSpec,
        HtmlSpec,
        PaginateSpec,
        ProbeManifest,
        CsvOpts,
        BodySpec,
        InstanceFileDecl,
        InstanceDecl,
        Fixture,
        Exchange,
        FixtureRequest,
        FixtureResponse,
        ConstQueryVal,
        AuthIn,
        AuthKind,
        OAuthStyle,
        ArrayStyle,
        PathEncode,
        PreAuth,
        OnEmpty,
        OpAuthOverride,
    );
    let _ = ConstQueryVal::many(["a", "b"]);
    let _ = AuthDecl::new_oauth2(
        "https://x.example/token",
        "SEARCH_MCP_X_CLIENT_ID",
        "SEARCH_MCP_X_SECRET",
    );
    let _ = AuthDecl::new_static("SEARCH_MCP_X_API_KEY", AuthIn::Query, "key");
    let _ = BodySpec::graphql("{ x }", None);
    let _ = BodySpec::jaq(BodyKind::Json, "{}");
    let mut m = ManifestDecl::default();
    m.id = "fake".into();
    let mut op = OpSpec::default();
    op.params
        .insert("query", ParamDecl::new(ParamType::String, "terms"));
    m.op.insert("search", op);
    let _ = m.to_source_decl();
    let mut f = Fixture::new("arxiv", "work", "2026-11-25T10:00:00Z");
    f.exchanges.push(Exchange::new(
        FixtureRequest::new("GET", "https://x"),
        FixtureResponse::from_bytes(200, vec![], b"ok"),
    ));
    f.validate().unwrap();
    let net = ReplayNet::from_fixtures(vec![f]).unwrap();
    let hits: Vec<ReplayHit> = net.hits();
    assert!(hits.is_empty() && net.len() == 1);
}

#[test]
fn error_variants_construct_outside_the_crate() {
    let errs = [
        FsmError::invalid("x"),
        FsmError::upstream("bing", Some(503), "unavailable"),
        FsmError::internal("panic", "boom"),
        FsmError::BudgetExceeded("host calls".into()),
    ];
    for e in errs {
        assert!(!e.kind().is_empty());
    }
}

// ---- traits are implementable and object-safe outside the crate ----------

struct FakeNet;
#[async_trait]
impl NetClient for FakeNet {
    async fn send(
        &self,
        _req: HttpRequest,
        _profile: TransportProfile,
        _cx: &NetCx,
    ) -> Result<HttpBytes, FsmError> {
        Ok(HttpBytes::default())
    }
    fn check_url_offline(&self, _url: &str) -> Result<(), FsmError> {
        Ok(())
    }
}

struct FakeStore;
#[async_trait]
impl StoreApi for FakeStore {
    async fn cache_search(&self, _q: &str, _limit: u32) -> Result<Vec<CacheHit>, FsmError> {
        Ok(vec![])
    }
    async fn get_page(
        &self,
        _url: &str,
        _max_age_s: Option<u64>,
    ) -> Result<Option<PageResult>, FsmError> {
        Ok(None)
    }
    async fn put_page(&self, _page: &PageResult, _ttl_s: u64) -> Result<(), FsmError> {
        Ok(())
    }
    async fn kv_put(&self, _s: &str, _k: &str, _v: &Value, _ttl_s: u64) -> Result<(), FsmError> {
        Ok(())
    }
    async fn kv_get(&self, _s: &str, _k: &str) -> Result<Option<Value>, FsmError> {
        Ok(None)
    }
    async fn search_cache_get(
        &self,
        _k: &str,
        _a: Option<u64>,
    ) -> Result<Option<(SearchResponse, u64)>, FsmError> {
        Ok(None)
    }
    async fn search_cache_put(
        &self,
        _k: &str,
        _r: &SearchResponse,
        _t: u64,
    ) -> Result<(), FsmError> {
        Ok(())
    }
    async fn api_cache_get(
        &self,
        _k: &str,
    ) -> Result<Option<(Bytes, Option<String>, Option<String>)>, FsmError> {
        Ok(None)
    }
    async fn api_cache_put(
        &self,
        _k: &str,
        _s: &str,
        _b: &[u8],
        _e: Option<&str>,
        _l: Option<&str>,
        _t: u64,
    ) -> Result<(), FsmError> {
        Ok(())
    }
    async fn scholar_get(&self, _k: &str) -> Result<Option<ScholarlyWork>, FsmError> {
        Ok(None)
    }
    async fn scholar_put(&self, _k: &str, _w: &ScholarlyWork, _t: u64) -> Result<(), FsmError> {
        Ok(())
    }
    async fn retraction_lookup(&self, _d: &str) -> Result<Option<RetractionNotice>, FsmError> {
        Ok(None)
    }
    async fn retractions_replace_batch(
        &self,
        _r: Vec<(String, RetractionNotice)>,
        _e: Option<String>,
    ) -> Result<(), FsmError> {
        Ok(())
    }
    async fn sync_state_get(
        &self,
        _n: &str,
    ) -> Result<Option<(Option<String>, i64, Option<i64>)>, FsmError> {
        Ok(None)
    }
    async fn sync_state_mark(
        &self,
        _n: &str,
        _e: Option<&str>,
        _s: i64,
        _f: Option<i64>,
    ) -> Result<(), FsmError> {
        Ok(())
    }
    async fn health_load_all(&self) -> Result<Vec<HealthRecord>, FsmError> {
        Ok(vec![])
    }
    async fn health_save(&self, _r: &HealthRecord) -> Result<(), FsmError> {
        Ok(())
    }
    async fn download_record(&self, _r: &DownloadResult, _t: u64) -> Result<(), FsmError> {
        Ok(())
    }
}

struct Fake;
#[async_trait]
impl SearchApi for Fake {
    async fn search(&self, req: SearchRequest, _cx: &CallCx) -> Result<SearchResponse, FsmError> {
        Ok(SearchResponse::new(req.query, req.scope))
    }
}
#[async_trait]
impl AskApi for Fake {
    async fn ask(&self, q: &str, _opts: AskOpts, _cx: &CallCx) -> Result<AskResponse, FsmError> {
        Ok(AskResponse::new(q, "fast"))
    }
    async fn answer_search(
        &self,
        _q: &str,
        _search: &SearchResponse,
        _cx: &CallCx,
    ) -> Option<SearchAgentSummary> {
        None
    }
}
#[async_trait]
impl ScholarApi for Fake {
    async fn search(&self, q: ScholarQuery, _cx: &CallCx) -> Result<WorkResult, FsmError> {
        Ok(WorkResult::new(vec![], SearchMeta::new(q.query)))
    }
    async fn work(&self, _id: &str, _cx: &CallCx) -> Result<Option<ScholarlyWork>, FsmError> {
        Ok(None)
    }
    async fn graph(
        &self,
        id: &str,
        _o: GraphOpts,
        _cx: &CallCx,
    ) -> Result<CitationGraph, FsmError> {
        Ok(CitationGraph::new(id, "title", ScholarlyWork::default()))
    }
    async fn fulltext(
        &self,
        id: &str,
        _o: FulltextOpts,
        _cx: &CallCx,
    ) -> Result<FulltextResult, FsmError> {
        Ok(FulltextResult::new(id, false))
    }
    async fn integrity(&self, _d: &str, _cx: &CallCx) -> Result<IntegrityReport, FsmError> {
        Ok(IntegrityReport::new(None))
    }
    async fn cite(&self, _i: CiteInput, _s: CiteStyle, _cx: &CallCx) -> Result<String, FsmError> {
        Ok(String::new())
    }
    async fn entity(
        &self,
        _k: EntityKind,
        _q: &str,
        _o: Value,
        _cx: &CallCx,
    ) -> Result<Value, FsmError> {
        Ok(Value::Null)
    }
}
#[async_trait]
impl FetchApi for Fake {
    async fn fetch(
        &self,
        url: &str,
        _o: FetchOpts,
        _cx: &CallCx,
    ) -> Result<FetchOutcome, FsmError> {
        Ok(FetchOutcome::Page(PageResult::new(
            url,
            "",
            "",
            FetchMethod::Http,
        )))
    }
    async fn fetch_batch(
        &self,
        _u: Vec<String>,
        _o: FetchOpts,
        _m: u8,
        _cx: &CallCx,
    ) -> Result<Vec<PageResult>, FsmError> {
        Ok(vec![])
    }
    async fn request(&self, o: RawRequestOpts, _cx: &CallCx) -> Result<HttpResponse, FsmError> {
        Ok(HttpResponse::new(o.url, 200, ""))
    }
    async fn structured(&self, url: &str, _cx: &CallCx) -> Result<StructuredMeta, FsmError> {
        Ok(StructuredMeta::new(url))
    }
    async fn research(
        &self,
        q: &str,
        _o: ResearchOpts,
        _cx: &CallCx,
    ) -> Result<ResearchBrief, FsmError> {
        Ok(ResearchBrief::new(q, SearchResponse::default()))
    }
    async fn compare(
        &self,
        _q: &str,
        _u: Vec<String>,
        _cx: &CallCx,
    ) -> Result<Vec<CompareItem>, FsmError> {
        Ok(vec![])
    }
    async fn download(&self, url: &str, _cx: &CallCx) -> Result<DownloadResult, FsmError> {
        Ok(DownloadResult::new(url, "", "", 0, ""))
    }
    async fn html_to_md(&self, html: &str) -> Result<String, FsmError> {
        Ok(html.to_owned())
    }
}
#[async_trait]
impl DocApi for Fake {
    async fn read(
        &self,
        s: &str,
        _st: usize,
        _l: Option<usize>,
        _cx: &CallCx,
    ) -> Result<DocSlice, FsmError> {
        Ok(DocSlice::new(s, "txt", ""))
    }
    async fn outline(&self, s: &str, _cx: &CallCx) -> Result<DocOutline, FsmError> {
        Ok(DocOutline::new(s, "txt"))
    }
}
#[async_trait]
impl SourcesApi for Fake {
    async fn call(&self, op_id: &str, _p: Value, _cx: &CallCx) -> Result<SourceOutput, FsmError> {
        Ok(SourceOutput::new(op_id, "search"))
    }
    async fn protocol(
        &self,
        k: &str,
        b: &str,
        op: &str,
        _p: Value,
        _cx: &CallCx,
    ) -> Result<ProtocolOutput, FsmError> {
        Ok(ProtocolOutput::new(k, b, op))
    }
    async fn catalog_search(&self, _q: CatalogQuery) -> Result<Vec<CatalogHit>, FsmError> {
        Ok(vec![])
    }
    async fn catalog_describe(&self, id: &str) -> Result<CatalogDetail, FsmError> {
        Ok(CatalogDetail::new(CatalogHit::new(id, "op", "")))
    }
}
#[async_trait]
impl HostServices for Fake {
    async fn call(&self, _f: &str, args: Value, _cx: &CallCx) -> Result<HostValue, FsmError> {
        Ok(HostValue::new(args))
    }
}

struct FakeSource(SourceDecl);
#[async_trait]
impl Source for FakeSource {
    fn decl(&self) -> &SourceDecl {
        &self.0
    }
    async fn call(&self, _op: &str, _params: Value, _cx: &SrcCx) -> Result<SrcOutput, FsmError> {
        Ok(SrcOutput::Hits(vec![Hit::new("t", "https://x")]))
    }
}

struct FakeProto(Vec<OpDecl>);
#[async_trait]
impl ProtocolAdapter for FakeProto {
    fn kind(&self) -> &'static str {
        "fake_proto"
    }
    fn ops(&self) -> &[OpDecl] {
        &self.0
    }
    async fn call(
        &self,
        base: &str,
        op: &str,
        _p: Value,
        _cx: &SrcCx,
    ) -> Result<ProtocolOutput, FsmError> {
        Ok(ProtocolOutput::new(self.kind(), base, op))
    }
}

fn register_fake(reg: &mut Registry, _deps: &SourceDeps) -> Result<(), FsmError> {
    let decl = SourceDecl::new("fake", "Fake", "A fake source")
        .with_scopes(&["web"])
        .with_caps(&[Capability::Search]);
    reg.register(Arc::new(FakeSource(decl)))?;
    Ok(())
}

#[tokio::test]
async fn traits_assemble_into_services() {
    let cfg = Arc::new(Config::default());
    let store: Arc<dyn StoreApi> = Arc::new(FakeStore);
    let deps = SourceDeps::new(Arc::new(FakeNet), store.clone(), cfg.clone());
    let mut reg = Registry::new();
    let f: RegisterFn = register_fake;
    f(&mut reg, &deps).unwrap();
    reg.register_protocol(Arc::new(FakeProto(vec![OpDecl::new(
        "identify",
        "Identify",
        ReturnKind::Raw,
    )])))
    .unwrap();
    assert_eq!(reg.len(), 1);
    let fake = Arc::new(Fake);
    let services = Services::new(
        fake.clone(),
        fake.clone(),
        fake.clone(),
        fake.clone(),
        fake.clone(),
        store,
        Arc::new(reg),
        cfg,
    );
    let cx = CallCx::default();
    let r = services
        .search
        .search(SearchRequest::new("q"), &cx)
        .await
        .unwrap();
    assert_eq!(r.query, "q");
    let host: Arc<dyn HostServices> = fake;
    assert_eq!(host.call("x", json!(1), &cx).await.unwrap().json, json!(1));
    let src = services.registry.get("fake").unwrap();
    let out = src
        .call("search", json!({}), &SrcCx::default())
        .await
        .unwrap();
    assert!(matches!(out, SrcOutput::Hits(ref h) if h.len() == 1));
}

#[test]
fn every_non_exhaustive_item_is_covered() {
    let this = include_str!("construct_outside.rs");
    let src = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
    let mut files = vec![];
    let mut dirs = vec![std::path::PathBuf::from(src)];
    while let Some(d) = dirs.pop() {
        for e in std::fs::read_dir(d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                dirs.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") {
                files.push(p);
            }
        }
    }
    let mut missing = vec![];
    let mut seen = 0;
    for f in files {
        // Lane-owned src/ext files are exercised by their own crates.
        if f.components().any(|c| c.as_os_str() == "ext") {
            continue;
        }
        let text = std::fs::read_to_string(&f).unwrap();
        let mut rest = text.as_str();
        while let Some(i) = rest.find("#[non_exhaustive]") {
            rest = &rest[i + 17..];
            let Some(j) = rest.find("pub ") else { break };
            let decl = &rest[j + 4..];
            let decl = decl
                .strip_prefix("struct ")
                .or_else(|| decl.strip_prefix("enum "));
            let Some(decl) = decl else { continue };
            let name: String = decl
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            seen += 1;
            let used = this.match_indices(name.as_str()).any(|(k, _)| {
                let before = this[..k]
                    .chars()
                    .next_back()
                    .is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
                let after = this[k + name.len()..]
                    .chars()
                    .next()
                    .is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
                before && after
            });
            if !used {
                missing.push(format!("{} ({})", name, f.display()));
            }
        }
    }
    assert!(seen >= 100, "scanner found only {seen} items");
    assert!(
        missing.is_empty(),
        "not constructed outside the crate: {missing:#?}"
    );
}

/// Review additions: `NullStore`, `ReplayHit::default`, `SrcCx.domains`,
/// root re-exports of `Bytes` / `CancellationToken`.
#[tokio::test]
async fn null_store_and_review_additions() {
    let store: Arc<dyn StoreApi> = Arc::new(NullStore::new());
    assert!(store.get_page("https://x/", None).await.unwrap().is_none());
    assert!(store.health_load_all().await.unwrap().is_empty());
    store.kv_put("", "exec:k", &json!(1), 60).await.unwrap();
    assert!(store.kv_get("", "exec:k").await.unwrap().is_none());
    assert!(store.api_cache_get("k").await.unwrap().is_none());
    let _ = NullStore::default();
    let hit = ReplayHit::default();
    assert!(hit.url.is_empty());
    let mut cx = SrcCx::new(fsm_core::CancellationToken::new(), Duration::from_secs(1));
    cx.domains = vec!["example.org".into(), "-spam.example".into()];
    assert_eq!(cx.domains.len(), 2);
    let req = HttpRequest::post("https://x/", b"{}".to_vec());
    assert_eq!(req.body.as_deref(), Some(&b"{}"[..]));
    let body: fsm_core::Bytes = "x".into();
    assert_eq!(
        HttpBytes::new("https://x/", 200, vec![], body).body.len(),
        1
    );
}
