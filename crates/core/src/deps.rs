//! Dependency injection & the source registry (`SPEC-contracts.md` §1.2).

use crate::api::StoreApi;
use crate::config::Config;
use crate::error::FsmError;
use crate::net::NetClient;
use crate::scopes;
use crate::source::{ProtocolAdapter, Source};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

/// What every source, engine and facade service is built from.
///
/// Holds trait objects, so it has no meaningful `Default`; build it with
/// [`SourceDeps::new`]. Cheap to clone (three `Arc`s).
#[derive(Clone)]
#[non_exhaustive]
pub struct SourceDeps {
    pub net: Arc<dyn NetClient>,
    pub store: Arc<dyn StoreApi>,
    pub cfg: Arc<Config>,
}

impl SourceDeps {
    pub fn new(net: Arc<dyn NetClient>, store: Arc<dyn StoreApi>, cfg: Arc<Config>) -> Self {
        Self { net, store, cfg }
    }
}

impl fmt::Debug for SourceDeps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourceDeps").finish_non_exhaustive()
    }
}

/// Signature of every crate's `register` entry point.
pub type RegisterFn = fn(&mut Registry, &SourceDeps) -> Result<(), FsmError>;

/// All registered sources and protocol adapters, in registration order.
#[derive(Default)]
#[non_exhaustive]
pub struct Registry {
    order: Vec<Arc<dyn Source>>,
    by_id: HashMap<String, Arc<dyn Source>>,
    protocols: Vec<Arc<dyn ProtocolAdapter>>,
    protocol_by_kind: HashMap<String, Arc<dyn ProtocolAdapter>>,
}

impl fmt::Debug for Registry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Registry")
            .field(
                "sources",
                &self
                    .order
                    .iter()
                    .map(|s| s.decl().id.as_str())
                    .collect::<Vec<_>>(),
            )
            .field(
                "protocols",
                &self.protocols.iter().map(|p| p.kind()).collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// `^[a-z][a-z0-9_]{1,39}$` — the id rule shared by sources, manifests,
/// instances and protocol kinds.
pub fn is_valid_id(id: &str) -> bool {
    let b = id.as_bytes();
    (2..=40).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a source. Rejects malformed or duplicate ids and unknown scopes.
    pub fn register(&mut self, src: Arc<dyn Source>) -> Result<(), FsmError> {
        let decl = src.decl();
        if !is_valid_id(&decl.id) {
            return Err(FsmError::invalid(format!(
                "invalid source id '{}' (expected ^[a-z][a-z0-9_]{{1,39}}$)",
                decl.id
            )));
        }
        if self.by_id.contains_key(&decl.id) {
            return Err(FsmError::invalid(format!(
                "duplicate source id '{}'",
                decl.id
            )));
        }
        if let Some(bad) = decl.scopes.iter().find(|s| scopes::get(s).is_none()) {
            return Err(FsmError::invalid(format!(
                "source '{}' declares unknown scope '{bad}'",
                decl.id
            )));
        }
        self.by_id.insert(decl.id.clone(), Arc::clone(&src));
        self.order.push(src);
        Ok(())
    }

    /// Add a protocol adapter. Rejects duplicate kinds.
    pub fn register_protocol(&mut self, adapter: Arc<dyn ProtocolAdapter>) -> Result<(), FsmError> {
        let kind = adapter.kind();
        if !is_valid_id(kind) {
            return Err(FsmError::invalid(format!("invalid protocol kind '{kind}'")));
        }
        if self.protocol_by_kind.contains_key(kind) {
            return Err(FsmError::invalid(format!(
                "duplicate protocol kind '{kind}'"
            )));
        }
        self.protocol_by_kind
            .insert(kind.to_owned(), Arc::clone(&adapter));
        self.protocols.push(adapter);
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn Source>> {
        self.by_id.get(id).cloned()
    }

    pub fn protocol(&self, kind: &str) -> Option<Arc<dyn ProtocolAdapter>> {
        self.protocol_by_kind.get(kind).cloned()
    }

    /// Split `"<source>.<op>"` (or a bare `"<source>"`, meaning op
    /// `"search"`) and look the source up. When the source declares ops,
    /// the op must be one of them.
    pub fn resolve_op<'a>(&self, op_id: &'a str) -> Result<(Arc<dyn Source>, &'a str), FsmError> {
        let op_id = op_id.trim();
        let (id, op) = match op_id.split_once('.') {
            Some((id, op)) => (id, op),
            None => (op_id, "search"),
        };
        let src = self.get(id).ok_or_else(|| {
            FsmError::invalid(format!(
                "unknown source '{id}'; find source ids with catalog.search(...)"
            ))
        })?;
        let ops = &src.decl().ops;
        if !ops.is_empty() && !ops.iter().any(|o| o.name == op) {
            let names: Vec<&str> = ops.iter().map(|o| o.name.as_str()).collect();
            return Err(FsmError::invalid(format!(
                "source '{id}' has no op '{op}'; call one of: {}",
                names
                    .iter()
                    .map(|n| format!("{id}.{n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        Ok((src, op))
    }

    /// Sources declaring `scope` or any of its descendants (`"gov"` also
    /// matches `"gov.law"`), in registration order. Synonyms are resolved
    /// through `core::SCOPES`; an unknown scope yields an empty list.
    pub fn by_scope(&self, scope: &str) -> Vec<Arc<dyn Source>> {
        let Some(def) = scopes::resolve(scope) else {
            return Vec::new();
        };
        self.order
            .iter()
            .filter(|s| s.decl().scopes.iter().any(|d| scopes::is_within(d, def.id)))
            .cloned()
            .collect()
    }

    pub fn all(&self) -> impl Iterator<Item = &Arc<dyn Source>> {
        self.order.iter()
    }

    pub fn protocols(&self) -> impl Iterator<Item = &Arc<dyn ProtocolAdapter>> {
        self.protocols.iter()
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{OpDecl, ReturnKind, SourceDecl, SrcCx, SrcOutput};
    use crate::types::ProtocolOutput;
    use serde_json::Value;

    struct Fake(SourceDecl);

    #[async_trait::async_trait]
    impl Source for Fake {
        fn decl(&self) -> &SourceDecl {
            &self.0
        }
        async fn call(
            &self,
            _op: &str,
            _params: Value,
            _cx: &SrcCx,
        ) -> Result<SrcOutput, FsmError> {
            Ok(SrcOutput::default())
        }
    }

    struct Proto(&'static str);

    #[async_trait::async_trait]
    impl ProtocolAdapter for Proto {
        fn kind(&self) -> &'static str {
            self.0
        }
        fn ops(&self) -> &[OpDecl] {
            &[]
        }
        async fn call(
            &self,
            base_url: &str,
            op: &str,
            _p: Value,
            _cx: &SrcCx,
        ) -> Result<ProtocolOutput, FsmError> {
            Ok(ProtocolOutput::new(self.0, base_url, op))
        }
    }

    fn src(id: &str, scopes: &[&str], ops: &[&str]) -> Arc<dyn Source> {
        let mut d = SourceDecl::new(id, id, "test").with_scopes(scopes);
        for op in ops {
            d = d.with_op(OpDecl::new(*op, "op", ReturnKind::Records));
        }
        Arc::new(Fake(d))
    }

    #[test]
    fn ids_follow_the_rule() {
        assert!(is_valid_id("so360"));
        assert!(is_valid_id("github_code"));
        assert!(!is_valid_id("a"));
        assert!(!is_valid_id("Arxiv"));
        assert!(!is_valid_id("1abc"));
        assert!(!is_valid_id("has-dash"));
        assert!(!is_valid_id(&"a".repeat(41)));
        assert!(is_valid_id(&"a".repeat(40)));
    }

    #[test]
    fn register_rejects_duplicates_and_unknown_scopes() {
        let mut r = Registry::new();
        r.register(src(
            "arxiv",
            &["academic", "academic.preprint"],
            &["search", "work"],
        ))
        .unwrap();
        assert!(r.register(src("arxiv", &["academic"], &[])).is_err());
        assert!(
            r.register(src("bad", &["papers"], &[])).is_err(),
            "synonyms are not canonical ids"
        );
        assert!(r.register(src("Bad", &[], &[])).is_err());
        assert_eq!(r.len(), 1);
        r.register_protocol(Arc::new(Proto("oai_pmh"))).unwrap();
        assert!(r.register_protocol(Arc::new(Proto("oai_pmh"))).is_err());
        assert_eq!(r.protocol("oai_pmh").unwrap().kind(), "oai_pmh");
        assert!(r.protocol("ckan").is_none());
    }

    #[test]
    fn resolve_op_splits_and_validates() {
        let mut r = Registry::new();
        r.register(src(
            "clinicaltrials",
            &["academic.trial"],
            &["search", "study"],
        ))
        .unwrap();
        r.register(src("worldclock", &["reference"], &[])).unwrap();
        let (s, op) = r.resolve_op("clinicaltrials.study").unwrap();
        assert_eq!((s.decl().id.as_str(), op), ("clinicaltrials", "study"));
        assert_eq!(r.resolve_op("clinicaltrials").unwrap().1, "search");
        let err = r
            .resolve_op("clinicaltrials.nope")
            .err()
            .unwrap()
            .to_string();
        assert!(
            err.contains("clinicaltrials.search") && err.contains("clinicaltrials.study"),
            "{err}"
        );
        assert!(r.resolve_op("nosuch.search").is_err());
        assert_eq!(r.resolve_op("worldclock.now").unwrap().1, "now");
    }

    #[test]
    fn by_scope_includes_descendants_and_synonyms_in_order() {
        let mut r = Registry::new();
        r.register(src("pubmed", &["academic.biomed"], &[]))
            .unwrap();
        r.register(src("ddg", &["web"], &[])).unwrap();
        r.register(src("arxiv", &["academic", "academic.preprint"], &[]))
            .unwrap();
        r.register(src("courtlistener", &["gov.law"], &[])).unwrap();
        let ids =
            |v: Vec<Arc<dyn Source>>| v.iter().map(|s| s.decl().id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(r.by_scope("academic")), ["pubmed", "arxiv"]);
        assert_eq!(ids(r.by_scope("papers")), ["pubmed", "arxiv"]);
        assert_eq!(ids(r.by_scope("academic.biomed")), ["pubmed"]);
        assert_eq!(ids(r.by_scope("health")), ["pubmed"]);
        assert_eq!(ids(r.by_scope("gov")), ["courtlistener"]);
        assert!(r.by_scope("nonsense").is_empty());
        assert_eq!(r.all().count(), 4);
        assert!(format!("{r:?}").contains("courtlistener"));
    }
}
