//! Protocol instance tables (§3 + A1 §D): the three A1 §G.5.2 vectors and three
//! real W5 staged tables (verbatim copies).

use fsm_core::manifest::{InstanceFileDecl, default_instance_limits};
use fsm_core::{
    Freshness, FsmError, OpDecl, PoolRole, ProtocolAdapter, ProtocolOutput, ReturnKind, SrcCx,
    TransportProfile,
};
use serde_json::{Value, json};
use std::time::Duration;

const VECTORS: [(&str, &str); 6] = [
    ("sru", include_str!("instance_examples/sru.toml")),
    (
        "activitypub_fediverse",
        include_str!("instance_examples/activitypub_fediverse.toml"),
    ),
    (
        "mediawiki",
        include_str!("instance_examples/mediawiki.toml"),
    ),
    (
        "oai_pmh",
        include_str!("manifest_examples/staged/w5e_oai_pmh.toml"),
    ),
    (
        "ckan",
        include_str!("manifest_examples/staged/w5f_ckan.toml"),
    ),
    (
        "mediawiki",
        include_str!("manifest_examples/staged/w5g_mediawiki.toml"),
    ),
];

#[test]
fn vectors_parse_validate_and_round_trip() {
    for (protocol, text) in VECTORS {
        let f = InstanceFileDecl::from_toml(text).unwrap_or_else(|e| panic!("{protocol}: {e}"));
        f.validate().unwrap_or_else(|e| panic!("{protocol}: {e}"));
        assert_eq!(f.protocol, protocol);
        assert!(!f.instance.is_empty());
        let back: InstanceFileDecl =
            postcard::from_bytes(&postcard::to_allocvec(&f).unwrap()).unwrap();
        assert_eq!(back, f, "{protocol} postcard");
        let back: InstanceFileDecl =
            serde_json::from_str(&serde_json::to_string(&f).unwrap()).unwrap();
        assert_eq!(back, f, "{protocol} JSON");
    }
}

#[test]
fn a1_instance_fields() {
    let sru = InstanceFileDecl::from_toml(VECTORS[0].1).unwrap();
    let bnf = &sru.instance[0];
    assert_eq!(bnf.dialect["version"], json!("1.2"));
    assert_eq!(
        bnf.dialect["query_template"],
        json!("bib.title all \"{query}\"")
    );
    assert_eq!(bnf.effective_limits().rps, 1.0);
    assert_eq!(bnf.transport, TransportProfile::Plain);

    let ap = InstanceFileDecl::from_toml(VECTORS[1].1).unwrap();
    assert_eq!(ap.instance[0].dialect["flavor"], json!("lemmy"));
    assert_eq!(
        ap.instance[0].probe.fixture.as_deref(),
        Some("fixtures/w5g/lemmy_world/search/default.json")
    );
    assert_eq!(ap.instance[1].effective_pool_role(), PoolRole::Explicit);

    let mw = InstanceFileDecl::from_toml(VECTORS[2].1).unwrap();
    let commons = &mw.instance[0];
    assert_eq!(commons.default_params["namespace"], json!(6));
    assert_eq!(
        commons.limits.as_ref().unwrap().host_bucket.as_deref(),
        Some("wikimedia.org")
    );
    assert_eq!(
        commons.effective_pool_role(),
        PoolRole::ScopeOnly,
        "search cap ⇒ scope_only by default"
    );
    let ops = vec![
        OpDecl::new("search", "Search", ReturnKind::Hits),
        OpDecl::new("page", "Page", ReturnKind::Records),
    ];
    let d = commons.to_source_decl(&ops);
    assert_eq!(
        d.op("search").unwrap().probe.as_ref().unwrap().params["query"],
        json!("lighthouse")
    );
    assert!(d.op("page").unwrap().probe.is_none());
    assert_eq!(d.limits.host_bucket.as_deref(), Some("wikimedia.org"));
    let staged = InstanceFileDecl::from_toml(VECTORS[5].1).unwrap();
    let bucket = |id: &str| {
        let i = staged.instance.iter().find(|i| i.id == id).unwrap();
        i.effective_limits().host_bucket
    };
    for id in [
        "wiktionary",
        "wikisource",
        "wikibooks",
        "wikispecies",
        "wikinews",
    ] {
        assert_eq!(
            bucket(id).as_deref(),
            Some("wikimedia.org"),
            "A1 §F.9 copy: {id}"
        );
    }
    assert_eq!(bucket("archwiki").as_deref(), Some("wiki.archlinux.org"));
    let l = default_instance_limits("https://lemmy.world/api");
    assert_eq!(
        (l.rps, l.host_bucket.as_deref()),
        (2.0, Some("lemmy.world"))
    );
}

#[test]
fn malformed_instances_are_rejected() {
    let text = VECTORS[0].1;
    // probe is required, and must name an op
    let no_probe: String = text
        .lines()
        .filter(|l| !l.starts_with("probe"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(InstanceFileDecl::from_toml(&no_probe).is_err());
    let mut f = InstanceFileDecl::from_toml(text).unwrap();
    f.instance[0].probe.op = None;
    assert!(
        f.problems()
            .iter()
            .any(|p| p.contains("probe.op must name an adapter op"))
    );
    let mut f = InstanceFileDecl::from_toml(text).unwrap();
    f.instance[0].transport = TransportProfile::Local;
    assert!(
        f.problems()
            .iter()
            .any(|p| p.contains("transport = \"local\""))
    );
    let mut f = InstanceFileDecl::from_toml(text).unwrap();
    f.instance[0].probe.fixture = Some("fixtures/w5e/other/search/default.json".into());
    assert!(
        f.problems()
            .iter()
            .any(|p| p.contains("belongs to bnf_sru/search"))
    );
    let mut f = InstanceFileDecl::from_toml(VECTORS[2].1).unwrap();
    f.instance[1].id = "wikimedia".into();
    assert!(f.problems().iter().any(|p| p.contains("duplicate id")));
    let mut f = InstanceFileDecl::from_toml(text).unwrap();
    f.instance[0].scopes = vec!["preprints".into()];
    assert!(f.validate().is_err());
    let mut f = InstanceFileDecl::from_toml(text).unwrap();
    f.instance[0].base_url = "ftp://x".into();
    assert!(f.validate().is_err());
    assert!(
        InstanceFileDecl::from_toml(&text.replace("tier = 1\n", "tier = 1\nweight = 2\n")).is_err()
    );
    assert!(
        InstanceFileDecl::from_toml(
            &text.replace("pool_role = \"scope_only\"", "pool_role = \"main\"")
        )
        .is_err()
    );
}

#[test]
fn a1_instance_fields_reject() {
    let text = VECTORS[2].1;
    let mut f = InstanceFileDecl::from_toml(text).unwrap();
    f.instance[0].limits.as_mut().unwrap().rps = 0.0;
    assert!(
        f.problems()
            .iter()
            .any(|p| p.contains("limits.rps must be > 0")),
        "{:?}",
        f.problems()
    );
    let mut f = InstanceFileDecl::from_toml(text).unwrap();
    f.instance[0].default_params.insert(" ".into(), json!(1));
    assert!(
        f.problems()
            .iter()
            .any(|p| p.contains("dialect/default_params keys must be non-empty"))
    );
    let mut f = InstanceFileDecl::from_toml(VECTORS[0].1).unwrap();
    f.instance[0].dialect.insert(String::new(), json!("x"));
    assert!(f.validate().is_err());
    assert!(
        InstanceFileDecl::from_toml(&text.replacen(
            "host_bucket = \"wikimedia.org\"",
            "host_bucket = 3",
            1
        ))
        .is_err()
    );
}

struct NoDialect;

#[async_trait::async_trait]
impl ProtocolAdapter for NoDialect {
    fn kind(&self) -> &'static str {
        "plain_proto"
    }
    fn ops(&self) -> &[OpDecl] {
        &[]
    }
    async fn call(
        &self,
        _: &str,
        _: &str,
        _: Value,
        _: &SrcCx,
    ) -> Result<ProtocolOutput, FsmError> {
        Err(FsmError::invalid("not called"))
    }
}

#[test]
fn validate_dialect_default_and_src_cx_fields() {
    let sru = InstanceFileDecl::from_toml(VECTORS[0].1).unwrap();
    assert!(NoDialect.validate_dialect(&Default::default()).is_ok());
    let err = NoDialect
        .validate_dialect(&sru.instance[0].dialect)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains(
            "plain_proto does not accept dialect keys (query_template, record_schema, version)"
        ),
        "{err}"
    );
    let mut cx = SrcCx::new(Default::default(), Duration::from_secs(5));
    assert!(cx.freshness.is_none() && cx.dialect.is_empty() && !cx.replay_mode);
    cx.dialect = sru.instance[0].dialect.clone();
    cx.freshness = Some(Freshness::Week);
    cx.replay_mode = true;
    assert_eq!(cx.dialect["version"], json!("1.2"));
}
