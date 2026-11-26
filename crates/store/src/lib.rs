//! `fsm-store` — SQLite storage layer implementing [`fsm_core::StoreApi`]
//! over `search-mcp.sqlite` (`SPEC.md` §8, `SPEC-contracts.md` §1.4).
//!
//! Design invariants:
//! - Opens `<cache_dir>/search-mcp.sqlite` (`PRAGMA user_version = 1`, WAL,
//!   `busy_timeout = 5000`, `synchronous = NORMAL`). Legacy `cache.sqlite` is
//!   never opened or migrated.
//! - Dedicated single-writer actor thread owns the SQLite connection, sweeps
//!   expired rows, and enforces `cache_max_mb` every 200 writes.
//! - All 10 tables from `SPEC.md` §8 are created: `search_cache`, `pages`,
//!   `pages_fts` (rowid-preserving triggers), `api_cache`, `scholar_works`,
//!   `retractions`, `sync_state`, `exec_kv` (M7 session modes + 2 MiB val +
//!   64 MiB LRU + key regex + no-`exec:` + 168h TTL), `health_state`, `downloads`.
//! - Wire types (`SearchResponse`, `ScholarlyWork`, `Value`, `PageResult`) are
//!   persisted as zstd-compressed JSON (never postcard/bincode).

#![forbid(unsafe_code)]

mod actor;
mod codec;
pub mod downloads;
pub mod fts;
pub mod lock;
mod schema;

use actor::{ActorHandle, Cmd, db_disk_size};
use async_trait::async_trait;
use bytes::Bytes;
use fsm_core::{
    CacheHit, Config, DownloadResult, FsmError, HealthRecord, PageResult, RetractionNotice,
    ScholarlyWork, SearchResponse, StoreApi,
};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{RwLock, oneshot};

pub use fsm_core::ext::w0c::{
    DB_FILENAME, DEFAULT_MAINTAIN_EVERY, EXEC_KV_DEFAULT_TTL_S, EXEC_KV_LRU_MAX_BYTES,
    EXEC_KV_MAX_KEY_LEN, EXEC_KV_MAX_TTL_S, EXEC_KV_MAX_VAL_BYTES, EXEC_KV_OVERFLOW_PREFIX,
    EXEC_KV_OVERFLOW_SESSION, EXEC_KV_SHARED_SESSION, RW_SYNC_LOCK_FILENAME, SCHEMA_USER_VERSION,
    invalid_fts_hint, normalize_doi, validate_kv_key, validate_overflow_kv_key,
    validate_user_kv_key,
};

/// Row returned by [`SqliteStore::downloads_list_active`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadEntry {
    pub id: String,
    pub url: String,
    pub saved_path: String,
    pub media_type: String,
    pub bytes: u64,
    pub sha256: String,
    pub created_at: i64,
    pub expires_at: i64,
}

/// Configuration for [`SqliteStore`].
#[derive(Debug, Clone)]
pub struct StoreConfig {
    /// Full path to the SQLite database file (default: `<cache_dir>/search-mcp.sqlite`).
    pub db_path: PathBuf,
    /// Fallback TTL (seconds) when `put_page` receives `ttl_s = 0` (default: 7 days).
    pub cache_ttl_s: u64,
    /// Maximum DB + WAL footprint in MiB (`0` disables size-cap eviction; default: 512).
    pub cache_max_mb: u64,
    /// Opportunistic maintenance sweep cadence in writes (`0` disables automatic sweeps; default: 200).
    pub maintain_every: u64,
    /// Run one opportunistic maintenance sweep right after opening the DB (off the caller's critical
    /// path, matching Python `cache.py` so short-lived stdio servers still purge expired rows; default: true).
    pub maintain_on_open: bool,
    /// Maximum raw (pre-zstd) JSON size of one `exec_kv` value in bytes (default: 2 MiB).
    pub exec_kv_max_val_bytes: usize,
    /// Maximum total raw JSON bytes retained in `exec_kv` under LRU (default: 64 MiB).
    pub exec_kv_lru_max_bytes: u64,
    /// Default `exec_kv` TTL in seconds when `ttl_s == 0` (default: 24h).
    pub exec_kv_default_ttl_s: u64,
    /// Hard maximum `exec_kv` TTL in seconds (default: 168h / 7d).
    pub exec_kv_max_ttl_s: u64,
}

impl StoreConfig {
    /// Build a [`StoreConfig`] from [`fsm_core::Config`], targeting `<cache_dir>/search-mcp.sqlite`.
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            db_path: cfg.cache_dir.join(DB_FILENAME),
            cache_ttl_s: cfg.cache_ttl_s,
            cache_max_mb: cfg.cache_max_mb,
            maintain_every: DEFAULT_MAINTAIN_EVERY,
            maintain_on_open: true,
            exec_kv_max_val_bytes: EXEC_KV_MAX_VAL_BYTES,
            exec_kv_lru_max_bytes: EXEC_KV_LRU_MAX_BYTES,
            exec_kv_default_ttl_s: EXEC_KV_DEFAULT_TTL_S,
            exec_kv_max_ttl_s: EXEC_KV_MAX_TTL_S,
        }
    }

    /// Build a [`StoreConfig`] pointing at an explicit SQLite file path with standard defaults.
    pub fn at_path(db_path: impl Into<PathBuf>) -> Self {
        Self {
            db_path: db_path.into(),
            cache_ttl_s: 604_800,
            cache_max_mb: 512,
            maintain_every: DEFAULT_MAINTAIN_EVERY,
            maintain_on_open: true,
            exec_kv_max_val_bytes: EXEC_KV_MAX_VAL_BYTES,
            exec_kv_lru_max_bytes: EXEC_KV_LRU_MAX_BYTES,
            exec_kv_default_ttl_s: EXEC_KV_DEFAULT_TTL_S,
            exec_kv_max_ttl_s: EXEC_KV_MAX_TTL_S,
        }
    }
}

struct StoreInner {
    cfg: StoreConfig,
    actor: RwLock<Option<ActorHandle>>,
}

/// Thread-safe, clonable handle to the single-writer SQLite storage actor.
#[derive(Clone)]
pub struct SqliteStore {
    inner: Arc<StoreInner>,
}

impl std::fmt::Debug for SqliteStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteStore")
            .field("db_path", &self.inner.cfg.db_path)
            .finish_non_exhaustive()
    }
}

impl SqliteStore {
    /// Open `<cfg.cache_dir>/search-mcp.sqlite` using settings from [`Config`].
    pub fn open(cfg: &Config) -> Result<Self, FsmError> {
        Self::open_with(StoreConfig::from_config(cfg))
    }

    /// Open a SQLite store at an explicit path with default storage limits.
    pub fn open_path(db_path: impl Into<PathBuf>) -> Result<Self, FsmError> {
        Self::open_with(StoreConfig::at_path(db_path))
    }

    /// Open a SQLite store with a custom [`StoreConfig`].
    pub fn open_with(cfg: StoreConfig) -> Result<Self, FsmError> {
        let handle = ActorHandle::spawn(cfg.clone())?;
        Ok(Self {
            inner: Arc::new(StoreInner {
                cfg,
                actor: RwLock::new(Some(handle)),
            }),
        })
    }

    /// Path to the opened SQLite database file.
    pub fn path(&self) -> &Path {
        &self.inner.cfg.db_path
    }

    /// Current on-disk footprint of the SQLite file plus its `-wal` file in bytes.
    pub fn db_size_bytes(&self) -> u64 {
        db_disk_size(&self.inner.cfg.db_path)
    }

    /// Gracefully shut down the actor thread and close the SQLite connection.
    ///
    /// Safe to call repeatedly; any subsequent operation lazily re-opens the actor.
    pub async fn close(&self) -> Result<(), FsmError> {
        let handle = {
            let mut guard = self.inner.actor.write().await;
            guard.take()
        };
        if let Some(mut handle) = handle {
            let (tx, rx) = oneshot::channel();
            let _ = handle.tx.send(Cmd::Close { reply: tx }).await;
            let _ = rx.await;
            if let Some(join) = handle.thread.take() {
                let _ = tokio::task::spawn_blocking(move || {
                    let _ = join.join();
                })
                .await;
            }
        }
        Ok(())
    }

    /// Convenience helper for user `cache.put(key, value, { ttlHours?, shared? })`.
    ///
    /// Always rejects `exec:` keys (even if `caller_session_id` is empty),
    /// routes `shared = true` to `session_id = "shared"`, and defaults `ttl_s` to 24h.
    pub async fn kv_put_user(
        &self,
        caller_session_id: &str,
        key: &str,
        val: &Value,
        ttl_s: Option<u64>,
        shared: bool,
    ) -> Result<(), FsmError> {
        validate_user_kv_key(key)?;
        let session = if shared {
            EXEC_KV_SHARED_SESSION
        } else {
            caller_session_id
        };
        self.kv_put(session, key, val, ttl_s.unwrap_or(EXEC_KV_DEFAULT_TTL_S))
            .await
    }

    /// Convenience helper for storing an `exec` output overflow handle (`exec:<id>`)
    /// under `session_id = ""` (M7).
    pub async fn kv_put_overflow(
        &self,
        key: &str,
        val: &Value,
        ttl_s: u64,
    ) -> Result<(), FsmError> {
        validate_overflow_kv_key(key)?;
        self.kv_put(EXEC_KV_OVERFLOW_SESSION, key, val, ttl_s).await
    }

    /// Cached page URLs containing `fragment`, most recently fetched first (`cache.py`).
    pub async fn complete_page_urls(
        &self,
        fragment: &str,
        limit: u32,
    ) -> Result<Vec<String>, FsmError> {
        self.call(|reply| Cmd::CompletePageUrls {
            fragment: fragment.to_owned(),
            limit,
            reply,
        })
        .await
    }

    /// Stored `search_cache` keys starting with `prefix`, newest first (`cache.py`).
    pub async fn recent_search_keys(
        &self,
        prefix: &str,
        limit: u32,
    ) -> Result<Vec<String>, FsmError> {
        self.call(|reply| Cmd::RecentSearchKeys {
            prefix: prefix.to_owned(),
            limit,
            reply,
        })
        .await
    }

    /// Non-expired download metadata records from the `downloads` table.
    pub async fn downloads_list_active(&self) -> Result<Vec<DownloadEntry>, FsmError> {
        self.call(|reply| Cmd::DownloadsListActive { reply }).await
    }

    /// Delete expired rows from the `downloads` SQLite table (preserving `expires_at = 0`).
    pub async fn downloads_purge_expired(&self) -> Result<usize, FsmError> {
        self.call(|reply| Cmd::DownloadsPurgeExpired { reply })
            .await
    }

    /// Run a maintenance sweep immediately (purge expired rows, enforce LRU & `cache_max_mb`).
    pub async fn maintain_now(&self) -> Result<(), FsmError> {
        self.call(|reply| Cmd::MaintainNow { reply }).await
    }

    /// Number of maintenance sweeps executed by the active actor instance.
    pub async fn maintain_count(&self) -> Result<u64, FsmError> {
        let tx = self.sender().await?;
        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(Cmd::MaintainCount { reply: reply_tx })
            .await
            .map_err(|_| FsmError::internal("store_actor_closed", "store actor channel closed"))?;
        reply_rx
            .await
            .map_err(|_| FsmError::internal("store_actor_closed", "store actor dropped reply"))
    }

    /// Run SQLite FTS5's built-in `integrity-check` against `pages_fts`.
    pub async fn fts_integrity_check(&self) -> Result<(), FsmError> {
        self.call(|reply| Cmd::FtsIntegrityCheck { reply }).await
    }

    async fn sender(&self) -> Result<tokio::sync::mpsc::Sender<Cmd>, FsmError> {
        {
            let guard = self.inner.actor.read().await;
            if let Some(handle) = guard.as_ref()
                && !handle.tx.is_closed()
            {
                return Ok(handle.tx.clone());
            }
        }
        // Lazily re-open off the async executor so SQLite init/busy-wait never blocks a Tokio worker.
        let mut guard = self.inner.actor.write().await;
        if let Some(handle) = guard.as_ref()
            && !handle.tx.is_closed()
        {
            return Ok(handle.tx.clone());
        }
        let cfg = self.inner.cfg.clone();
        let handle = tokio::task::spawn_blocking(move || ActorHandle::spawn(cfg))
            .await
            .map_err(|e| {
                FsmError::internal("store_actor_spawn", format!("spawn_blocking failed: {e}"))
            })??;
        let tx = handle.tx.clone();
        *guard = Some(handle);
        Ok(tx)
    }

    async fn call<T>(
        &self,
        build: impl FnOnce(oneshot::Sender<Result<T, FsmError>>) -> Cmd,
    ) -> Result<T, FsmError> {
        let tx = self.sender().await?;
        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(build(reply_tx))
            .await
            .map_err(|_| FsmError::internal("store_actor_closed", "store actor channel closed"))?;
        reply_rx
            .await
            .map_err(|_| FsmError::internal("store_actor_closed", "store actor dropped reply"))?
    }
}

#[async_trait]
impl StoreApi for SqliteStore {
    async fn cache_search(&self, fts_query: &str, limit: u32) -> Result<Vec<CacheHit>, FsmError> {
        self.call(|reply| Cmd::CacheSearch {
            fts_query: fts_query.to_owned(),
            limit,
            reply,
        })
        .await
    }

    async fn get_page(
        &self,
        url: &str,
        max_age_s: Option<u64>,
    ) -> Result<Option<PageResult>, FsmError> {
        self.call(|reply| Cmd::GetPage {
            url: url.to_owned(),
            max_age_s,
            reply,
        })
        .await
    }

    async fn put_page(&self, page: &PageResult, ttl_s: u64) -> Result<(), FsmError> {
        self.call(|reply| Cmd::PutPage {
            page: Box::new(page.clone()),
            ttl_s,
            reply,
        })
        .await
    }

    async fn kv_put(
        &self,
        session_id: &str,
        key: &str,
        val: &Value,
        ttl_s: u64,
    ) -> Result<(), FsmError> {
        self.call(|reply| Cmd::KvPut {
            session_id: session_id.to_owned(),
            key: key.to_owned(),
            val: val.clone(),
            ttl_s,
            reply,
        })
        .await
    }

    async fn kv_get(&self, session_id: &str, key: &str) -> Result<Option<Value>, FsmError> {
        self.call(|reply| Cmd::KvGet {
            session_id: session_id.to_owned(),
            key: key.to_owned(),
            reply,
        })
        .await
    }

    async fn search_cache_get(
        &self,
        key: &str,
        max_age_s: Option<u64>,
    ) -> Result<Option<(SearchResponse, u64)>, FsmError> {
        self.call(|reply| Cmd::SearchCacheGet {
            key: key.to_owned(),
            max_age_s,
            reply,
        })
        .await
    }

    async fn search_cache_put(
        &self,
        key: &str,
        resp: &SearchResponse,
        ttl_s: u64,
    ) -> Result<(), FsmError> {
        self.call(|reply| Cmd::SearchCachePut {
            key: key.to_owned(),
            resp: Box::new(resp.clone()),
            ttl_s,
            reply,
        })
        .await
    }

    async fn api_cache_get(
        &self,
        key: &str,
    ) -> Result<Option<(Bytes, Option<String>, Option<String>)>, FsmError> {
        self.call(|reply| Cmd::ApiCacheGet {
            key: key.to_owned(),
            reply,
        })
        .await
    }

    async fn api_cache_put(
        &self,
        key: &str,
        source_id: &str,
        body: &[u8],
        etag: Option<&str>,
        last_mod: Option<&str>,
        ttl_s: u64,
    ) -> Result<(), FsmError> {
        self.call(|reply| Cmd::ApiCachePut {
            key: key.to_owned(),
            source_id: source_id.to_owned(),
            body: body.to_vec(),
            etag: etag.map(str::to_owned),
            last_mod: last_mod.map(str::to_owned),
            ttl_s,
            reply,
        })
        .await
    }

    async fn scholar_get(
        &self,
        canonical_key_or_pid: &str,
    ) -> Result<Option<ScholarlyWork>, FsmError> {
        self.call(|reply| Cmd::ScholarGet {
            canonical_key_or_pid: canonical_key_or_pid.to_owned(),
            reply,
        })
        .await
    }

    async fn scholar_put(
        &self,
        canonical_key: &str,
        work: &ScholarlyWork,
        ttl_s: u64,
    ) -> Result<(), FsmError> {
        self.call(|reply| Cmd::ScholarPut {
            canonical_key: canonical_key.to_owned(),
            work: Box::new(work.clone()),
            ttl_s,
            reply,
        })
        .await
    }

    async fn retraction_lookup(
        &self,
        norm_doi: &str,
    ) -> Result<Option<RetractionNotice>, FsmError> {
        self.call(|reply| Cmd::RetractionLookup {
            norm_doi: norm_doi.to_owned(),
            reply,
        })
        .await
    }

    async fn retractions_replace_batch(
        &self,
        rows: Vec<(String, RetractionNotice)>,
        etag: Option<String>,
    ) -> Result<(), FsmError> {
        self.call(|reply| Cmd::RetractionsReplaceBatch { rows, etag, reply })
            .await
    }

    async fn sync_state_get(
        &self,
        name: &str,
    ) -> Result<Option<(Option<String>, i64, Option<i64>)>, FsmError> {
        self.call(|reply| Cmd::SyncStateGet {
            name: name.to_owned(),
            reply,
        })
        .await
    }

    async fn sync_state_mark(
        &self,
        name: &str,
        etag: Option<&str>,
        started_at: i64,
        finished_at: Option<i64>,
    ) -> Result<(), FsmError> {
        self.call(|reply| Cmd::SyncStateMark {
            name: name.to_owned(),
            etag: etag.map(str::to_owned),
            started_at,
            finished_at,
            reply,
        })
        .await
    }

    async fn health_load_all(&self) -> Result<Vec<HealthRecord>, FsmError> {
        self.call(|reply| Cmd::HealthLoadAll { reply }).await
    }

    async fn health_save(&self, rec: &HealthRecord) -> Result<(), FsmError> {
        self.call(|reply| Cmd::HealthSave {
            rec: Box::new(rec.clone()),
            reply,
        })
        .await
    }

    async fn download_record(&self, rec: &DownloadResult, ttl_s: u64) -> Result<(), FsmError> {
        self.call(|reply| Cmd::DownloadRecord {
            rec: Box::new(rec.clone()),
            ttl_s,
            reply,
        })
        .await
    }
}
