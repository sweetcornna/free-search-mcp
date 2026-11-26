//! Manifest test vectors: the five `SPEC-contracts.md` §2.2 manifests (M4,
//! Example 1 fixed per A1 §G.4.4), the two A1 §G.4.5 vectors and five real
//! W5 staged manifests (copies; A1 §F fixes applied where noted in each file).
//! Accepted, converted, postcard round-tripped; malformed variants rejected;
//! `crates/manifests/manifest.schema.json` kept current.

use fsm_core::manifest::{
    self, AuthDecl, AuthIn, AuthKind, BodyKind, BodySpec, ConstQueryVal, ManifestDecl, OnEmpty,
    OpAuthOverride, PaginateSpec, PaginateStyle, ParamDecl, ParamLocation, PathEncode, PreAuth,
    PreStepDecl, ResponseFormat,
};
use fsm_core::{
    AuthMode, Capability, HttpMethod, ParamType, PoolRole, ReturnKind, TransportProfile,
};
use serde_json::json;

const EXAMPLES: [(&str, &str); 7] = [
    (
        "clinicaltrials",
        include_str!("manifest_examples/clinicaltrials.toml"),
    ),
    (
        "nih_reporter",
        include_str!("manifest_examples/nih_reporter.toml"),
    ),
    ("arxiv", include_str!("manifest_examples/arxiv.toml")),
    (
        "icore_ranks",
        include_str!("manifest_examples/icore_ranks.toml"),
    ),
    ("nasa_ads", include_str!("manifest_examples/nasa_ads.toml")),
    (
        "epo_ops_a1",
        include_str!("manifest_examples/oauth2_and_per_op_auth.toml"),
    ),
    (
        "anilist_a1",
        include_str!("manifest_examples/graphql_and_html_map.toml"),
    ),
];

const STAGED: [(&str, &str); 5] = [
    (
        "pubmed",
        include_str!("manifest_examples/staged/w5a_pubmed.toml"),
    ),
    (
        "clinicaltrials",
        include_str!("manifest_examples/staged/w5b_clinicaltrials.toml"),
    ),
    (
        "hackernews",
        include_str!("manifest_examples/staged/w5c_hackernews.toml"),
    ),
    (
        "worldbank",
        include_str!("manifest_examples/staged/w5d1_worldbank.toml"),
    ),
    (
        "federalregister",
        include_str!("manifest_examples/staged/w5d2_federalregister.toml"),
    ),
];

fn load(id: &str) -> ManifestDecl {
    let text = EXAMPLES
        .iter()
        .chain(STAGED.iter())
        .find(|(n, _)| *n == id)
        .unwrap()
        .1;
    ManifestDecl::from_toml(text).unwrap()
}

fn expect_problem(m: &ManifestDecl, needle: &str) {
    let p = m.problems();
    assert!(
        p.iter().any(|x| x.contains(needle)),
        "expected a problem containing {needle:?}, got {p:#?}"
    );
    assert!(m.validate().is_err());
}

fn expect_ok(m: &ManifestDecl) {
    assert!(m.problems().is_empty(), "{:#?}", m.problems());
}

#[test]
fn all_examples_and_staged_copies_validate() {
    for (id, text) in EXAMPLES.iter().chain(STAGED.iter()) {
        let m = ManifestDecl::parse_validated(text).unwrap_or_else(|e| panic!("{id}: {e}"));
        assert_eq!(&m.id, id);
        let d = m.to_source_decl();
        assert_eq!(d.ops.len(), m.op.len());
    }
}

#[test]
fn conversion_keeps_document_order_and_semantics() {
    let ct = load("clinicaltrials");
    assert_eq!(ct.op.keys().collect::<Vec<_>>(), ["search", "study"]);
    let search = ct.op.get("search").unwrap();
    assert_eq!(
        search.params.keys().collect::<Vec<_>>(),
        ["query", "status", "limit", "cursor"]
    );
    assert_eq!(
        search.params.get("query").unwrap().wire_name("query"),
        "query.term"
    );
    assert_eq!(
        ct.op.get("study").unwrap().map.as_ref().unwrap().items,
        ".",
        "A1 §G.4.4"
    );
    let d = ct.to_source_decl();
    let op = d.op("search").unwrap();
    assert_eq!(
        op.params
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>(),
        ["query", "status", "limit", "cursor"]
    );
    let limit = op.param("limit").unwrap();
    assert_eq!(
        (limit.ty, limit.default.clone(), limit.min, limit.max),
        (ParamType::Int, Some(json!(10)), Some(1.0), Some(50.0))
    );
    let probe = op.probe.as_ref().unwrap();
    assert_eq!(probe.params, json!({"query": "crispr", "limit": 3}));
    assert_eq!(
        probe.fixture.as_deref(),
        Some("fixtures/academic_domain/clinicaltrials/search/default.json")
    );

    let ads = load("nasa_ads").to_source_decl();
    assert_eq!(
        ads.auth,
        AuthMode::RequiredKey {
            env: "SEARCH_MCP_ADS_API_KEY".into()
        }
    );
    assert!(ads.has_cap(Capability::Graph));
    assert_eq!(ads.transport, TransportProfile::Plain);

    let arxiv = load("arxiv");
    let work_map = arxiv.op.get("work").unwrap().map.as_ref().unwrap();
    let search_work = arxiv
        .op
        .get("search")
        .unwrap()
        .map
        .as_ref()
        .unwrap()
        .work
        .as_deref()
        .unwrap();
    assert_eq!(
        arxiv
            .resolve_ref(work_map.work.as_deref().unwrap())
            .unwrap(),
        search_work
    );

    let icore = load("icore_ranks");
    let html = icore.op.get("rank").unwrap().html.as_ref().unwrap();
    assert_eq!(
        html.fields.keys().collect::<Vec<_>>(),
        ["name", "acronym", "icoreRank"]
    );
}

#[test]
fn a1_vectors_carry_the_new_fields() {
    let epo = load("epo_ops_a1");
    let auth = epo.auth.as_ref().unwrap();
    assert_eq!(auth.kind, AuthKind::Oauth2ClientCredentials);
    assert_eq!(
        (
            auth.effective_in(),
            auth.effective_name(),
            auth.effective_format()
        ),
        (AuthIn::Header, "Authorization", "Bearer {key}")
    );
    assert_eq!(
        auth.credential_envs(),
        ["SEARCH_MCP_EPO_OPS_KEY", "SEARCH_MCP_EPO_OPS_SECRET"]
    );
    let search = epo.op.get("search").unwrap();
    assert_eq!(search.fallback_urls.len(), 1);
    assert!(search.query_jaq.is_some());
    assert_eq!(
        search.params.get("limit").unwrap().location,
        ParamLocation::None
    );
    assert_eq!(search.params.get("extra").unwrap().ty, ParamType::Json);
    assert_eq!(
        epo.op
            .get("biblio")
            .unwrap()
            .params
            .get("ref")
            .unwrap()
            .path_encode(),
        PathEncode::Raw
    );
    let coverage = epo.op.get("coverage").unwrap();
    assert_eq!(coverage.auth, OpAuthOverride::None);
    let d = epo.to_source_decl();
    assert_eq!(
        d.auth,
        AuthMode::RequiredKey {
            env: "SEARCH_MCP_EPO_OPS_KEY,SEARCH_MCP_EPO_OPS_SECRET".into()
        },
        "A1 §B.1 #3"
    );
    assert_eq!(
        d.key_envs(),
        ["SEARCH_MCP_EPO_OPS_KEY", "SEARCH_MCP_EPO_OPS_SECRET"]
    );
    assert_eq!(
        load("nasa_ads").to_source_decl().key_envs(),
        ["SEARCH_MCP_ADS_API_KEY"]
    );
    assert_eq!(d.op("coverage").unwrap().auth, OpAuthOverride::None);

    let ani = load("anilist_a1");
    let body = ani.op.get("search").unwrap().body.as_ref().unwrap();
    assert_eq!(
        (body.kind, body.operation_name.as_deref()),
        (BodyKind::Graphql, Some("Search"))
    );
    assert!(
        body.query
            .as_deref()
            .unwrap()
            .contains("Page(perPage: $perPage)")
    );
    let trending = ani.op.get("trending").unwrap();
    assert!(
        trending.html.is_some() && trending.map.is_some(),
        "html feeds map (A1 §B.4)"
    );

    let fr = load("federalregister");
    let dt = fr
        .op
        .iter()
        .find_map(|(_, o)| o.params.get("doc_type"))
        .unwrap();
    assert_eq!(
        dt.wire_name("doc_type"),
        "conditions[type][]",
        "bracketed wire keys survive"
    );
}

#[test]
fn postcard_round_trip_of_all_vectors() {
    for (id, text) in EXAMPLES.iter().chain(STAGED.iter()) {
        let m = ManifestDecl::from_toml(text).unwrap();
        let bytes = postcard::to_allocvec(&m).unwrap_or_else(|e| panic!("{id}: {e}"));
        let back: ManifestDecl =
            postcard::from_bytes(&bytes).unwrap_or_else(|e| panic!("{id}: {e}"));
        assert_eq!(back, m, "{id}");
        let back: ManifestDecl = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(back, m, "{id} via JSON");
        // The registry view is postcard-safe too.
        let d = m.to_source_decl();
        let back: fsm_core::SourceDecl =
            postcard::from_bytes(&postcard::to_allocvec(&d).unwrap()).unwrap();
        assert_eq!(back, d, "{id} SourceDecl");
    }
}

#[test]
fn malformed_manifests_are_rejected() {
    // Spec rules.
    let mut m = load("arxiv");
    m.id = "ArXiv".into();
    expect_problem(&m, "id \"ArXiv\" must match");
    let mut m = load("arxiv");
    m.scopes.push("paper".into());
    expect_problem(&m, "not a canonical scope id (did you mean \"academic\"?)");
    let mut m = load("arxiv");
    m.op.remove("search");
    expect_problem(&m, "caps contains \"search\" but there is no [op.search]");
    let mut m = load("clinicaltrials");
    m.op.get_mut("search").unwrap().map.as_mut().unwrap().hit = None;
    expect_problem(&m, "map.hit is required (M3)");
    let mut m = load("clinicaltrials");
    let mut pre = PreStepDecl::default();
    pre.url = "https://x.example/pre".into();
    pre.bind = ".x".into();
    m.op.get_mut("search").unwrap().pre = vec![pre.clone(), pre];
    expect_problem(&m, "at most one");
    let mut m = load("arxiv");
    m.op.get_mut("work")
        .unwrap()
        .probe
        .as_mut()
        .unwrap()
        .fixture = Some("fixtures/seed/arxiv/search/default.json".into());
    expect_problem(&m, "belongs to arxiv/work");
    let mut m = load("arxiv");
    m.op.get_mut("work")
        .unwrap()
        .probe
        .as_mut()
        .unwrap()
        .fixture = Some("arxiv/work.json".into());
    expect_problem(&m, "must look like fixtures/<wave>/arxiv/work/<case>.json");

    // Structural rules.
    let mut m = load("arxiv");
    m.tier = 4;
    expect_problem(&m, "tier 4");
    let mut m = load("clinicaltrials");
    m.op.get_mut("study").unwrap().url = "https://clinicaltrials.gov/api/v2/studies/{nctid}".into();
    expect_problem(&m, "undeclared template variable {nctid}");
    let mut m = load("clinicaltrials");
    m.op.get_mut("study").unwrap().url = "https://clinicaltrials.gov/api/v2/studies?k={key}".into();
    expect_problem(&m, "uses {key} but no [auth] key is injected there");
    let mut m = load("nasa_ads");
    m.auth.as_mut().unwrap().format = Some("Bearer".into());
    expect_problem(&m, "must contain {key}");
    let mut m = load("nasa_ads");
    m.auth.as_mut().unwrap().env = Some("ADS_KEY".into());
    expect_problem(&m, "auth.env \"ADS_KEY\" must match");
    let mut m = load("arxiv");
    m.op.get_mut("work").unwrap().map.as_mut().unwrap().work = Some("@ref:op.nope.map.work".into());
    expect_problem(&m, "missing op \"nope\"");
    let mut m = load("arxiv");
    m.op.get_mut("search").unwrap().map.as_mut().unwrap().work = None;
    expect_problem(&m, "returns works but has no map.work");
    let mut m = load("icore_ranks");
    m.op.get_mut("rank").unwrap().response = ResponseFormat::Json;
    expect_problem(&m, "html requires response = \"html\"");
    let mut m = load("nasa_ads");
    m.op.get_mut("search").unwrap().paginate = Some({
        let mut p = PaginateSpec::default();
        p.style = PaginateStyle::Cursor;
        p
    });
    expect_problem(&m, "paginate.param is required");
    let mut m = load("arxiv");
    m.op.get_mut("search")
        .unwrap()
        .params
        .get_mut("sort")
        .unwrap()
        .default = Some(json!("newest"));
    expect_problem(&m, "is not one of enum");
    let mut m = load("arxiv");
    m.op.get_mut("search")
        .unwrap()
        .params
        .get_mut("limit")
        .unwrap()
        .min = Some(100.0);
    expect_problem(&m, "min 100 > max 50");
    let mut m = load("arxiv");
    m.op.get_mut("search")
        .unwrap()
        .params
        .get_mut("limit")
        .unwrap()
        .default = Some(json!("ten"));
    expect_problem(&m, "is not a number");
    let mut m = load("clinicaltrials");
    m.op.get_mut("study")
        .unwrap()
        .probe
        .as_mut()
        .unwrap()
        .params
        .clear();
    expect_problem(&m, "lacks required param \"nct\"");
    let mut m = load("clinicaltrials");
    m.op.get_mut("study")
        .unwrap()
        .probe
        .as_mut()
        .unwrap()
        .params
        .insert("bogus".into(), json!(1));
    expect_problem(&m, "undeclared param \"bogus\"");
    let mut m = load("nih_reporter");
    m.op.get_mut("search").unwrap().method = HttpMethod::Get;
    expect_problem(&m, "body requires method = \"POST\"");
    let mut m = load("icore_ranks");
    m.op.get_mut("rank").unwrap().returns = ReturnKind::Raw;
    expect_problem(&m, "entity caps");
    let mut m = load("arxiv");
    m.claims = Some(Default::default());
    m.claims.as_mut().unwrap().patterns.push("(?<=x)y".into());
    expect_problem(&m, "does not compile");
    let mut m = load("arxiv");
    m.limits.rps = 0.0;
    expect_problem(&m, "limits.rps must be > 0");
}

#[test]
fn a1_auth_kinds_accept_and_reject() {
    // Accept: static key built from the constructor; OAuth2 body style with scope.
    let mut m = load("nasa_ads");
    let mut a = AuthDecl::new_static("SEARCH_MCP_ADS_API_KEY", AuthIn::Header, "Authorization");
    a.format = Some("Bearer {key}".into());
    m.auth = Some(a);
    expect_ok(&m);
    let mut m = load("epo_ops_a1");
    let a = m.auth.as_mut().unwrap();
    a.style = Some(manifest::OAuthStyle::Body);
    a.scope = Some("openid".into());
    a.env_id = Some("SEARCH_MCP_LEGIFRANCE_CLIENT_ID".into());
    a.env_secret = Some("SEARCH_MCP_LEGIFRANCE_CLIENT_SECRET".into());
    expect_ok(&m);
    // Reject.
    let mut m = load("epo_ops_a1");
    m.auth.as_mut().unwrap().token_url = Some("http://ops.epo.org/token".into());
    expect_problem(&m, "must start with https://");
    let mut m = load("epo_ops_a1");
    m.auth.as_mut().unwrap().env_id = None;
    expect_problem(&m, "auth.env_id is required");
    let mut m = load("epo_ops_a1");
    m.auth.as_mut().unwrap().env = Some("SEARCH_MCP_EPO_OPS_KEY".into());
    expect_problem(&m, "auth.env must not be set");
    let mut m = load("nasa_ads");
    m.auth.as_mut().unwrap().token_url = Some("https://x.example/token".into());
    expect_problem(
        &m,
        "auth.token_url is only for kind = \"oauth2_client_credentials\"",
    );
    let mut m = load("nasa_ads");
    m.auth.as_mut().unwrap().location = None;
    expect_problem(&m, "auth.in is required");
    let mut m = load("nasa_ads");
    m.auth.as_mut().unwrap().env = None;
    expect_problem(&m, "auth.env is required");
    let text = EXAMPLES[4].1.replace("in = \"header\"", "in = \"path\"");
    assert!(
        ManifestDecl::from_toml(&text).is_err(),
        "auth.in has no path"
    );
}

#[test]
fn a1_op_and_param_fields_accept_and_reject() {
    // op.auth
    let mut m = load("arxiv");
    m.op.get_mut("search").unwrap().auth = OpAuthOverride::Required;
    expect_problem(&m, "auth = \"required\" needs a top-level [auth]");
    let mut m = load("nasa_ads");
    m.op.get_mut("search").unwrap().auth = OpAuthOverride::Required;
    expect_ok(&m);
    m.op.get_mut("search").unwrap().auth = OpAuthOverride::None;
    m.op.get_mut("search").unwrap().url =
        "https://api.adsabs.harvard.edu/v1/search/query?k={key}".into();
    expect_problem(&m, "uses {key} but no [auth] key is injected there");
    // in = "none"
    let mut m = load("arxiv");
    m.op.get_mut("search")
        .unwrap()
        .params
        .get_mut("limit")
        .unwrap()
        .location = ParamLocation::None;
    expect_problem(
        &m,
        "is in = \"none\" (never transmitted), so it cannot have to",
    );
    m.op.get_mut("search")
        .unwrap()
        .params
        .get_mut("limit")
        .unwrap()
        .to = None;
    expect_ok(&m);
    // type = "json"
    let mut m = load("arxiv");
    let mut p = ParamDecl::new(ParamType::Json, "filters");
    p.location = ParamLocation::Query;
    m.op.get_mut("search").unwrap().params.insert("filters", p);
    expect_problem(&m, "type = \"json\" needs in = \"none\" or in = \"body\"");
    m.op.get_mut("search")
        .unwrap()
        .params
        .get_mut("filters")
        .unwrap()
        .location = ParamLocation::None;
    expect_ok(&m);
    // array
    let mut m = load("arxiv");
    m.op.get_mut("search")
        .unwrap()
        .params
        .get_mut("sort")
        .unwrap()
        .array = Some(manifest::ArrayStyle::Csv);
    expect_problem(
        &m,
        ".array is only for type = \"string_array\" with in = \"query\"",
    );
    let mut p = ParamDecl::new(ParamType::StringArray, "fields");
    p.array = Some(manifest::ArrayStyle::Brackets);
    m.op.get_mut("search")
        .unwrap()
        .params
        .get_mut("sort")
        .unwrap()
        .array = None;
    m.op.get_mut("search").unwrap().params.insert("fields", p);
    expect_ok(&m);
    // encode
    let mut m = load("arxiv");
    m.op.get_mut("search")
        .unwrap()
        .params
        .get_mut("query")
        .unwrap()
        .encode = Some(PathEncode::Raw);
    expect_problem(&m, ".encode is only for in = \"path\"");
    // query_jaq
    let mut m = load("arxiv");
    m.op.get_mut("search").unwrap().query_jaq = Some("  ".into());
    expect_problem(&m, "query_jaq is empty");
    m.op.get_mut("search").unwrap().query_jaq = Some("[[\"start\", \"0\"]]".into());
    expect_ok(&m);
    // fallback_urls
    let mut m = load("clinicaltrials");
    let study = m.op.get_mut("study").unwrap();
    study.fallback_urls = vec!["https://mirror.example.org/api/v2/studies/{nct}".into()];
    expect_ok(&m);
    m.op.get_mut("study")
        .unwrap()
        .fallback_urls
        .push("https://mirror2.example.org/api/v2/studies".into());
    expect_problem(
        &m,
        "fallback_urls[1] must use the same template variables as url",
    );
    m.op.get_mut("study").unwrap().fallback_urls = vec!["ftp://m/{nct}".into(); 4];
    expect_problem(&m, "fallback_urls has 4 entries (max 3)");
    expect_problem(&m, "must be an http(s) URL");
    // const_query arrays and templates
    let mut m = load("arxiv");
    let s = m.op.get_mut("search").unwrap();
    s.const_query
        .insert("fields[]".into(), ConstQueryVal::many(["title", "id"]));
    s.const_query
        .insert("lang".into(), ConstQueryVal::one("{lang}"));
    expect_ok(&m);
    m.op.get_mut("search")
        .unwrap()
        .const_query
        .insert("x".into(), ConstQueryVal::one("{nope}"));
    expect_problem(&m, "const_query.x uses undeclared template variable {nope}");
    assert!(
        ManifestDecl::from_toml(
            &EXAMPLES[2]
                .1
                .replace("sortOrder = \"descending\"", "sortOrder = { a = 1 }")
        )
        .is_err()
    );
    // map.items stream semantics (A1 §B.5)
    let mut m = load("clinicaltrials");
    m.op.get_mut("study").unwrap().map.as_mut().unwrap().items = "[.]".into();
    expect_problem(&m, "emits one array, not a stream of items (A1 §B.5)");
}

#[test]
fn a1_body_and_pre_fields_accept_and_reject() {
    // graphql body rules
    let mut m = load("anilist_a1");
    m.op.get_mut("search").unwrap().body.as_mut().unwrap().query = None;
    expect_problem(&m, "body.kind = \"graphql\" needs a non-empty query");
    let mut m = load("anilist_a1");
    m.op.get_mut("search").unwrap().body.as_mut().unwrap().jaq = Some("{}".into());
    expect_problem(&m, "uses query/variables_jaq, not jaq");
    let mut m = load("nih_reporter");
    m.op.get_mut("search")
        .unwrap()
        .body
        .as_mut()
        .unwrap()
        .operation_name = Some("Q".into());
    expect_problem(&m, "only for body.kind = \"graphql\"");
    let mut m = load("nih_reporter");
    m.op.get_mut("search").unwrap().body = Some(BodySpec::graphql("{ x }", None));
    expect_ok(&m);
    m.op.get_mut("search").unwrap().body = Some(BodySpec::jaq(BodyKind::Sparql, "$p.query"));
    expect_ok(&m);
    // pre.body / pre.auth / pre.on_empty
    let mut m = load("arxiv");
    let mut pre = PreStepDecl::default();
    pre.url = "https://export.arxiv.org/pre".into();
    pre.bind = ".ids | join(\",\")".into();
    pre.params.insert("q".into(), ConstQueryVal::one("{query}"));
    pre.auth = PreAuth::None;
    pre.on_empty = OnEmpty::Continue;
    pre.body = Some(BodySpec::jaq(BodyKind::Form, "{ q: $p.query }"));
    m.op.get_mut("search").unwrap().pre = vec![pre];
    expect_problem(&m, "pre[0].body requires method = \"POST\"");
    m.op.get_mut("search").unwrap().pre[0].method = HttpMethod::Post;
    expect_ok(&m);
    m.op.get_mut("search").unwrap().pre[0]
        .params
        .insert("p".into(), ConstQueryVal::one("{pre}"));
    expect_problem(&m, "uses {pre} without a usable pre step");
    let pubmed = load("pubmed");
    assert_eq!(pubmed.op.get("search").unwrap().pre.len(), 1);
    let ps = pubmed.op.get("search").unwrap();
    assert_eq!(ps.pre[0].on_empty, OnEmpty::Empty, "A1 §F.2 copy");
    assert_eq!(
        ps.params.get("query").unwrap().location,
        ParamLocation::None
    );
    assert_eq!(
        ps.params.get("limit").unwrap().location,
        ParamLocation::None
    );
    let bad = STAGED[0].1.replacen(
        "[[op.search.pre]]",
        "[[op.search.pre]]\non_empty = \"sometimes\"",
        1,
    );
    assert!(ManifestDecl::from_toml(&bad).is_err());
}

#[test]
fn declared_params_shadow_builtin_template_vars() {
    let mut m = load("clinicaltrials");
    let study = m.op.get_mut("study").unwrap();
    study.url = "https://clinicaltrials.gov/api/v2/studies/{nct}/{key}".into();
    let nct = study.params.get("nct").unwrap().clone();
    study.params.insert("key", nct);
    study
        .probe
        .as_mut()
        .unwrap()
        .params
        .insert("key".into(), json!("M.US"));
    expect_ok(&m);
    m.auth = load("nasa_ads").auth;
    expect_problem(&m, "{key} is ambiguous");
    let mut m = load("nasa_ads");
    m.auth.as_mut().unwrap().env = Some("SEARCH_MCP_GEONAMES_USERNAME".into());
    expect_ok(&m);
}

#[test]
fn unknown_fields_are_rejected_at_every_level() {
    let base = EXAMPLES[0].1;
    for (from, to) in [
        ("tier = 1\n", "tier = 1\ncolour = \"red\"\n"),
        (
            "method = \"GET\"\nurl = \"https://clinicaltrials.gov/api/v2/studies\"\n",
            "method = \"GET\"\nurl = \"https://clinicaltrials.gov/api/v2/studies\"\ntimeout = 3\n",
        ),
        (
            "doc = \"Condition or intervention\" }",
            "doc = \"Condition or intervention\", path = \"x\" }",
        ),
        (
            "next_cursor = \".nextPageToken\"",
            "next_cursor = \".nextPageToken\"\nnext = \".x\"",
        ),
        ("cache_ttl_s = 21600 }", "cache_ttl_s = 21600, rpm = 3 }"),
        ("transport = \"plain\"", "transport = \"local\""),
        ("pool_role = \"scope_only\"", "pool_role = \"main\""),
    ] {
        assert!(base.contains(from), "fixture text drifted: {from}");
        assert!(
            ManifestDecl::from_toml(&base.replacen(from, to, 1)).is_err(),
            "accepted: {to}"
        );
    }
    let locale = base.replacen(
        "pool_role = \"scope_only\"",
        "pool_role = { locale = \"zh\" }",
        1,
    );
    assert_eq!(
        ManifestDecl::from_toml(&locale).unwrap().pool_role,
        PoolRole::Locale("zh".into())
    );
    let epo = EXAMPLES[5].1;
    assert!(
        ManifestDecl::from_toml(&epo.replacen(
            "style = \"basic\"",
            "style = \"basic\"\nrefresh = 1",
            1
        ))
        .is_err()
    );
    assert!(
        ManifestDecl::from_toml(&epo.replacen("style = \"basic\"", "style = \"digest\"", 1))
            .is_err()
    );
}

#[test]
fn committed_json_schema_is_current() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../manifests/manifest.schema.json"
    );
    let want = manifest::json_schema_text();
    if std::env::var_os("FSM_BLESS_SCHEMA").is_some() {
        std::fs::write(path, &want).unwrap();
    }
    let have = std::fs::read_to_string(path).unwrap_or_default();
    assert!(
        have == want,
        "{path} is stale; regenerate with FSM_BLESS_SCHEMA=1 cargo test -p fsm-core --test manifest_examples"
    );
    let schema: serde_json::Value = serde_json::from_str(&have).unwrap();
    assert_eq!(schema["title"], "ManifestDecl");
    for def in [
        "AuthDecl",
        "OpSpec",
        "ParamDecl",
        "PreStepDecl",
        "BodySpec",
        "ConstQueryVal",
        "OpAuthOverride",
    ] {
        assert!(schema["$defs"].get(def).is_some(), "schema lacks {def}");
    }
}

#[test]
fn a1_remaining_fields_accept_and_reject() {
    // auth.scope is OAuth2-only.
    let mut m = load("nasa_ads");
    m.auth.as_mut().unwrap().scope = Some("read".into());
    expect_problem(
        &m,
        "auth.scope is only for kind = \"oauth2_client_credentials\"",
    );
    // op.auth takes inherit | none | required.
    let epo = EXAMPLES[5].1;
    assert!(epo.contains("auth = \"none\"\n"));
    for ok in ["inherit", "required", "none"] {
        let t = epo.replacen("auth = \"none\"\n", &format!("auth = \"{ok}\"\n"), 1);
        assert!(ManifestDecl::from_toml(&t).is_ok(), "{ok}");
    }
    assert!(
        ManifestDecl::from_toml(&epo.replacen("auth = \"none\"\n", "auth = \"maybe\"\n", 1))
            .is_err()
    );
    // pre.auth takes inherit | none only.
    let pubmed = STAGED[0].1;
    let with = |v: &str| {
        pubmed.replacen(
            "[[op.search.pre]]\n",
            &format!("[[op.search.pre]]\nauth = \"{v}\"\n"),
            1,
        )
    };
    let m = ManifestDecl::from_toml(&with("none")).unwrap();
    assert_eq!(m.op.get("search").unwrap().pre[0].auth, PreAuth::None);
    expect_ok(&m);
    assert!(ManifestDecl::from_toml(&with("required")).is_err());
}

#[test]
fn a1_html_rules_accept_and_reject() {
    // response = "html" needs [op.<name>.html].
    let mut m = load("icore_ranks");
    m.op.get_mut("rank").unwrap().html = None;
    expect_problem(
        &m,
        "has response = \"html\" but no [op.rank.html] (A1 §B.4)",
    );
    // Extractor grammar: "[<css> ]| <extractor>", or a bare extractor on the item node.
    let mut m = load("anilist_a1");
    let fields = &mut m
        .op
        .get_mut("trending")
        .unwrap()
        .html
        .as_mut()
        .unwrap()
        .fields;
    for (k, v) in [
        ("a", "| text"),
        ("b", "text"),
        ("c", "div.body | html"),
        ("d", "li | texts"),
        ("e", "a[hreflang|=en] | attr(href)"),
        ("f", "img | attrs( data-src )"),
        ("g", "svg | attr(xlink:href)"),
    ] {
        fields.insert(k, v.to_owned());
    }
    expect_ok(&m);
    for bad in [
        "td.title",
        "td | inner_text",
        "a | attr()",
        "a | attr(href",
        "a | Text",
        "td | text()",
    ] {
        let mut m = load("anilist_a1");
        m.op.get_mut("trending")
            .unwrap()
            .html
            .as_mut()
            .unwrap()
            .fields
            .insert("x", bad.to_owned());
        expect_problem(&m, "html.fields.x: unknown extractor");
    }
    // html without map: records of a non-search op only.
    let mut m = load("anilist_a1");
    m.op.get_mut("trending").unwrap().map = None;
    expect_ok(&m);
    m.op.get_mut("trending").unwrap().returns = ReturnKind::Hits;
    expect_problem(&m, "op.trending returns hits but has no map.hit");
    m.op.get_mut("trending").unwrap().returns = ReturnKind::Text;
    expect_problem(
        &m,
        "html without [op.trending.map] is allowed only for returns = \"records\"",
    );
    let mut m = load("anilist_a1");
    let t = m.op.remove("trending").unwrap();
    m.op.get_mut("search").unwrap().html = t.html;
    m.op.get_mut("search").unwrap().response = ResponseFormat::Html;
    m.op.get_mut("search").unwrap().map = None;
    m.op.get_mut("search").unwrap().returns = ReturnKind::Records;
    expect_problem(&m, "op.search returns records, so map.hit is required (M3)");
}

#[test]
fn pre_headers_and_pre_auth_template_rules() {
    let mut m = load("nasa_ads");
    let mut pre = PreStepDecl::default();
    pre.url = "https://api.adsabs.harvard.edu/v1/pre?k={key}".into();
    pre.bind = ".x".into();
    pre.headers.insert("X-Query".into(), "{query}".into());
    m.op.get_mut("search").unwrap().pre = vec![pre];
    expect_ok(&m);
    m.op.get_mut("search").unwrap().pre[0].auth = PreAuth::None;
    expect_problem(
        &m,
        "op.search.pre[0].url uses {key} but no [auth] key is injected there",
    );
    let pre = &mut m.op.get_mut("search").unwrap().pre[0];
    pre.auth = PreAuth::Inherit;
    pre.headers.insert("X-Bad".into(), "{nope}".into());
    expect_problem(
        &m,
        "op.search.pre[0].headers.X-Bad uses undeclared template variable {nope}",
    );
    let pre = &mut m.op.get_mut("search").unwrap().pre[0];
    pre.headers.remove("X-Bad");
    pre.headers.insert("X-Pre".into(), "{pre}".into());
    expect_problem(
        &m,
        "pre[0].headers.X-Pre uses {pre} without a usable pre step",
    );
    // fallback_urls are templated like url.
    let mut m = load("clinicaltrials");
    m.op.get_mut("study").unwrap().fallback_urls =
        vec!["https://m.example.org/{nct}?k={key}".into()];
    expect_problem(
        &m,
        "fallback_urls[0] uses {key} but no [auth] key is injected there",
    );
}
