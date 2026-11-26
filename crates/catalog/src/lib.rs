//! `fsm-catalog` — zero-network catalog search over registered sources, protocol adapters,
//! and the `exec` host API (`SPEC.md` §4, owner: lane `W2c`).
//!
//! Combines Okapi BM25 (`bm25` 2.3) with Snowball English stemming, CJK bigrams, ~90 EN/ZH
//! domain synonym groups, ID/name/alias exact & prefix boosting, and `rapidfuzz` typo recovery.

mod index;
mod synonyms;
mod tokenizer;

use crate::index::{CatalogIndex, build_health_map};
use fsm_core::{CatalogDetail, CatalogHit, CatalogQuery, FsmError, HostFn, Registry, StoreApi};
use std::collections::HashMap;
use std::sync::Arc;

/// Catalog index over the registry and the host API table (delegated to by
/// `fsm-server`'s `SourcesApi`).
pub struct CatalogService {
    reg: Arc<Registry>,
    host_api: &'static [HostFn],
    store: Arc<dyn StoreApi>,
    index: CatalogIndex,
}

impl CatalogService {
    pub fn new(reg: Arc<Registry>, host_api: &'static [HostFn], store: Arc<dyn StoreApi>) -> Self {
        let index = CatalogIndex::build(&reg, host_api);
        Self {
            reg,
            host_api,
            store,
            index,
        }
    }

    async fn load_health(&self) -> HashMap<String, String> {
        let records = self.store.health_load_all().await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "catalog health store load failed; defaulting to healthy");
            Vec::new()
        });
        build_health_map(records)
    }

    pub async fn search(&self, q: CatalogQuery) -> Result<Vec<CatalogHit>, FsmError> {
        let health = self.load_health().await;
        Ok(self.index.search(&q, &health))
    }

    pub async fn describe(&self, id: &str) -> Result<CatalogDetail, FsmError> {
        let health = self.load_health().await;
        self.index.describe(id, &self.reg, &health)
    }

    /// Number of host functions exposed in the catalog.
    pub fn host_api_len(&self) -> usize {
        self.host_api.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synonyms::SynonymMap;
    use crate::tokenizer::CatalogTokenizer;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn catalog_service_is_send_and_sync() {
        assert_send_sync::<CatalogService>();
    }

    #[test]
    fn synonym_table_has_at_least_80_groups() {
        let tok = CatalogTokenizer::new();
        let map = SynonymMap::build(&tok);
        assert!(
            map.group_count() >= 80,
            "expected >= 80 synonym groups, got {}",
            map.group_count()
        );
        let zh_journal = map.expand("期刊");
        assert!(zh_journal.iter().any(|t| t == "journal"), "{zh_journal:?}");
        let seismic = map.expand("seismic");
        assert!(seismic.iter().any(|t| t == "earthquak"), "{seismic:?}");
    }
}
