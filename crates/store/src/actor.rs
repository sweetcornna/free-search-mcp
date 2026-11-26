//! Single-writer SQLite actor driving all reads, writes, and 200-write maintenance sweeps.

use crate::codec::{
    MAX_API_BLOB_BYTES, MAX_JSON_BLOB_BYTES, PageMetaJson, decode_json_zstd, encode_json_zstd,
    fetch_method_from_str, fetch_method_to_str, format_epoch_iso8601, normalize_arxiv,
    normalize_doi, normalize_openalex, normalize_pmid, now_unix_ms, now_unix_s, zstd_compress,
    zstd_decompress_bounded,
};
use crate::fts::is_bad_match_query;
use crate::schema::init_connection;
use crate::{DownloadEntry, StoreConfig};
use bytes::Bytes;
use fsm_core::ext::w0c::{
    EXEC_KV_OVERFLOW_PREFIX, EXEC_KV_OVERFLOW_SESSION, EXEC_KV_SHARED_SESSION, validate_kv_key,
    validate_overflow_kv_key,
};
use fsm_core::{
    CacheHit, DownloadResult, FsmError, HealthRecord, PageResult, RetractionNotice, ScholarlyWork,
    SearchResponse,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::thread::JoinHandle;
use tokio::sync::{mpsc, oneshot};
use tracing::{error, warn};

const ACTOR_QUEUE_CAPACITY: usize = 4096;

pub(crate) type ApiCacheRow = (Bytes, Option<String>, Option<String>);
pub(crate) type SyncStateRow = (Option<String>, i64, Option<i64>);

/// Handle to the live single-writer SQLite actor thread.
pub(crate) struct ActorHandle {
    pub(crate) tx: mpsc::Sender<Cmd>,
    pub(crate) thread: Option<JoinHandle<()>>,
}

/// Commands dispatched to the single-writer actor.
pub(crate) enum Cmd {
    CacheSearch {
        fts_query: String,
        limit: u32,
        reply: oneshot::Sender<Result<Vec<CacheHit>, FsmError>>,
    },
    GetPage {
        url: String,
        max_age_s: Option<u64>,
        reply: oneshot::Sender<Result<Option<PageResult>, FsmError>>,
    },
    PutPage {
        page: Box<PageResult>,
        ttl_s: u64,
        reply: oneshot::Sender<Result<(), FsmError>>,
    },
    KvPut {
        session_id: String,
        key: String,
        val: Value,
        ttl_s: u64,
        reply: oneshot::Sender<Result<(), FsmError>>,
    },
    KvGet {
        session_id: String,
        key: String,
        reply: oneshot::Sender<Result<Option<Value>, FsmError>>,
    },
    SearchCacheGet {
        key: String,
        max_age_s: Option<u64>,
        reply: oneshot::Sender<Result<Option<(SearchResponse, u64)>, FsmError>>,
    },
    SearchCachePut {
        key: String,
        resp: Box<SearchResponse>,
        ttl_s: u64,
        reply: oneshot::Sender<Result<(), FsmError>>,
    },
    ApiCacheGet {
        key: String,
        reply: oneshot::Sender<Result<Option<ApiCacheRow>, FsmError>>,
    },
    ApiCachePut {
        key: String,
        source_id: String,
        body: Vec<u8>,
        etag: Option<String>,
        last_mod: Option<String>,
        ttl_s: u64,
        reply: oneshot::Sender<Result<(), FsmError>>,
    },
    ScholarGet {
        canonical_key_or_pid: String,
        reply: oneshot::Sender<Result<Option<ScholarlyWork>, FsmError>>,
    },
    ScholarPut {
        canonical_key: String,
        work: Box<ScholarlyWork>,
        ttl_s: u64,
        reply: oneshot::Sender<Result<(), FsmError>>,
    },
    RetractionLookup {
        norm_doi: String,
        reply: oneshot::Sender<Result<Option<RetractionNotice>, FsmError>>,
    },
    RetractionsReplaceBatch {
        rows: Vec<(String, RetractionNotice)>,
        etag: Option<String>,
        reply: oneshot::Sender<Result<(), FsmError>>,
    },
    SyncStateGet {
        name: String,
        reply: oneshot::Sender<Result<Option<SyncStateRow>, FsmError>>,
    },
    SyncStateMark {
        name: String,
        etag: Option<String>,
        started_at: i64,
        finished_at: Option<i64>,
        reply: oneshot::Sender<Result<(), FsmError>>,
    },
    HealthLoadAll {
        reply: oneshot::Sender<Result<Vec<HealthRecord>, FsmError>>,
    },
    HealthSave {
        rec: Box<HealthRecord>,
        reply: oneshot::Sender<Result<(), FsmError>>,
    },
    DownloadRecord {
        rec: Box<DownloadResult>,
        ttl_s: u64,
        reply: oneshot::Sender<Result<(), FsmError>>,
    },
    CompletePageUrls {
        fragment: String,
        limit: u32,
        reply: oneshot::Sender<Result<Vec<String>, FsmError>>,
    },
    RecentSearchKeys {
        prefix: String,
        limit: u32,
        reply: oneshot::Sender<Result<Vec<String>, FsmError>>,
    },
    DownloadsListActive {
        reply: oneshot::Sender<Result<Vec<DownloadEntry>, FsmError>>,
    },
    DownloadsPurgeExpired {
        reply: oneshot::Sender<Result<usize, FsmError>>,
    },
    MaintainNow {
        reply: oneshot::Sender<Result<(), FsmError>>,
    },
    MaintainCount {
        reply: oneshot::Sender<u64>,
    },
    FtsIntegrityCheck {
        reply: oneshot::Sender<Result<(), FsmError>>,
    },
    Close {
        reply: oneshot::Sender<()>,
    },
    #[cfg(test)]
    TestPanic {
        reply: oneshot::Sender<Result<(), FsmError>>,
    },
}

struct ActorState {
    conn: Connection,
    cfg: StoreConfig,
    writes_since_maintain: u64,
    maintain_runs: u64,
    /// Strictly increasing millisecond counter for deterministic `exec_kv` LRU ordering.
    last_kv_ms: i64,
    #[cfg(test)]
    on_maintain_start: Option<std::sync::mpsc::SyncSender<()>>,
}

impl ActorHandle {
    /// Spawn the single-writer actor thread, initializing SQLite before returning.
    pub(crate) fn spawn(cfg: StoreConfig) -> Result<Self, FsmError> {
        if let Some(parent) = cfg.db_path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| {
                FsmError::internal(
                    "store_open_dir",
                    format!("failed to create cache dir {}: {e}", parent.display()),
                )
            })?;
        }

        let (tx, rx) = mpsc::channel::<Cmd>(ACTOR_QUEUE_CAPACITY);
        let (init_tx, init_rx) = std::sync::mpsc::sync_channel::<Result<(), FsmError>>(1);

        let thread = std::thread::Builder::new()
            .name("fsm-store-actor".into())
            .spawn(move || actor_main(cfg, rx, init_tx))
            .map_err(|e| {
                FsmError::internal("store_actor_spawn", format!("failed to spawn actor: {e}"))
            })?;

        match init_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                tx,
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(FsmError::internal(
                    "store_actor_init",
                    "store actor terminated during initialization",
                ))
            }
        }
    }
}

fn actor_main(
    cfg: StoreConfig,
    mut rx: mpsc::Receiver<Cmd>,
    init_tx: std::sync::mpsc::SyncSender<Result<(), FsmError>>,
) {
    let conn = match Connection::open(&cfg.db_path) {
        Ok(c) => c,
        Err(e) => {
            let _ = init_tx.send(Err(FsmError::internal(
                "store_open",
                format!("failed to open {}: {e}", cfg.db_path.display()),
            )));
            return;
        }
    };

    if let Err(e) = init_connection(&conn) {
        let _ = init_tx.send(Err(e));
        return;
    }

    // Seed monotonic LRU clock from any pre-existing `exec_kv` rows.
    let existing_max_kv_ms: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(updated_at), 0) FROM exec_kv",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);

    if init_tx.send(Ok(())).is_err() {
        return;
    }

    let mut state = ActorState {
        conn,
        cfg,
        writes_since_maintain: 0,
        maintain_runs: 0,
        last_kv_ms: existing_max_kv_ms,
        #[cfg(test)]
        on_maintain_start: None,
    };

    // Opportunistic startup sweep (off `open()`'s critical path) so short-lived
    // stdio processes still purge expired rows and enforce `cache_max_mb`.
    if state.cfg.maintain_on_open
        && let Err(e) = state.maintain()
    {
        warn!("store startup maintenance sweep failed: {e}");
    }

    while let Some(cmd) = rx.blocking_recv() {
        let keep_running = match catch_unwind(AssertUnwindSafe(|| dispatch_cmd(&mut state, cmd))) {
            Ok(cont) => cont,
            Err(payload) => {
                error!(
                    "{}: store actor caught panic during command dispatch: {}",
                    state.cfg.db_path.display(),
                    panic_payload_str(&payload)
                );
                let _ = state.conn.execute_batch("ROLLBACK;");
                true
            }
        };
        if !keep_running {
            break;
        }
    }
}

fn panic_payload_str(payload: &Box<dyn Any + Send>) -> &str {
    if let Some(s) = payload.downcast_ref::<&str>() {
        s
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.as_str()
    } else {
        "non-string panic payload"
    }
}

fn dispatch_cmd(state: &mut ActorState, cmd: Cmd) -> bool {
    match cmd {
        Cmd::CacheSearch {
            fts_query,
            limit,
            reply,
        } => {
            let _ = reply.send(state.cache_search(&fts_query, limit));
        }
        Cmd::GetPage {
            url,
            max_age_s,
            reply,
        } => {
            let _ = reply.send(state.get_page(&url, max_age_s));
        }
        Cmd::PutPage { page, ttl_s, reply } => {
            let res = state.put_page(&page, ttl_s);
            let ok = res.is_ok();
            let _ = reply.send(res);
            if ok {
                state.bump_writes();
            }
        }
        Cmd::KvPut {
            session_id,
            key,
            val,
            ttl_s,
            reply,
        } => {
            let res = state.kv_put(&session_id, &key, &val, ttl_s);
            let ok = res.is_ok();
            let _ = reply.send(res);
            if ok {
                state.bump_writes();
            }
        }
        Cmd::KvGet {
            session_id,
            key,
            reply,
        } => {
            let _ = reply.send(state.kv_get(&session_id, &key));
        }
        Cmd::SearchCacheGet {
            key,
            max_age_s,
            reply,
        } => {
            let _ = reply.send(state.search_cache_get(&key, max_age_s));
        }
        Cmd::SearchCachePut {
            key,
            resp,
            ttl_s,
            reply,
        } => {
            let res = state.search_cache_put(&key, &resp, ttl_s);
            let ok = res.is_ok();
            let _ = reply.send(res);
            if ok {
                state.bump_writes();
            }
        }
        Cmd::ApiCacheGet { key, reply } => {
            let _ = reply.send(state.api_cache_get(&key));
        }
        Cmd::ApiCachePut {
            key,
            source_id,
            body,
            etag,
            last_mod,
            ttl_s,
            reply,
        } => {
            let res = state.api_cache_put(
                &key,
                &source_id,
                &body,
                etag.as_deref(),
                last_mod.as_deref(),
                ttl_s,
            );
            let ok = res.is_ok();
            let _ = reply.send(res);
            if ok {
                state.bump_writes();
            }
        }
        Cmd::ScholarGet {
            canonical_key_or_pid,
            reply,
        } => {
            let _ = reply.send(state.scholar_get(&canonical_key_or_pid));
        }
        Cmd::ScholarPut {
            canonical_key,
            work,
            ttl_s,
            reply,
        } => {
            let res = state.scholar_put(&canonical_key, &work, ttl_s);
            let ok = res.is_ok();
            let _ = reply.send(res);
            if ok {
                state.bump_writes();
            }
        }
        Cmd::RetractionLookup { norm_doi, reply } => {
            let _ = reply.send(state.retraction_lookup(&norm_doi));
        }
        Cmd::RetractionsReplaceBatch { rows, etag, reply } => {
            let res = state.retractions_replace_batch(rows, etag.as_deref());
            let ok = res.is_ok();
            let _ = reply.send(res);
            if ok {
                state.bump_writes();
            }
        }
        Cmd::SyncStateGet { name, reply } => {
            let _ = reply.send(state.sync_state_get(&name));
        }
        Cmd::SyncStateMark {
            name,
            etag,
            started_at,
            finished_at,
            reply,
        } => {
            let res = state.sync_state_mark(&name, etag.as_deref(), started_at, finished_at);
            let ok = res.is_ok();
            let _ = reply.send(res);
            if ok {
                state.bump_writes();
            }
        }
        Cmd::HealthLoadAll { reply } => {
            let _ = reply.send(state.health_load_all());
        }
        Cmd::HealthSave { rec, reply } => {
            let res = state.health_save(&rec);
            let ok = res.is_ok();
            let _ = reply.send(res);
            if ok {
                state.bump_writes();
            }
        }
        Cmd::DownloadRecord { rec, ttl_s, reply } => {
            let res = state.download_record(&rec, ttl_s);
            let ok = res.is_ok();
            let _ = reply.send(res);
            if ok {
                state.bump_writes();
            }
        }
        Cmd::CompletePageUrls {
            fragment,
            limit,
            reply,
        } => {
            let _ = reply.send(state.complete_page_urls(&fragment, limit));
        }
        Cmd::RecentSearchKeys {
            prefix,
            limit,
            reply,
        } => {
            let _ = reply.send(state.recent_search_keys(&prefix, limit));
        }
        Cmd::DownloadsListActive { reply } => {
            let _ = reply.send(state.downloads_list_active());
        }
        Cmd::DownloadsPurgeExpired { reply } => {
            let _ = reply.send(state.downloads_purge_expired());
        }
        Cmd::MaintainNow { reply } => {
            let _ = reply.send(state.maintain());
        }
        Cmd::MaintainCount { reply } => {
            let _ = reply.send(state.maintain_runs);
        }
        Cmd::FtsIntegrityCheck { reply } => {
            let _ = reply.send(state.fts_integrity_check());
        }
        Cmd::Close { reply } => {
            let _ = reply.send(());
            return false;
        }
        #[cfg(test)]
        Cmd::TestPanic { reply } => {
            let _ = reply;
            panic!("synthetic actor panic for catch_unwind resilience test");
        }
    }
    true
}

impl ActorState {
    fn next_kv_ms(&mut self) -> i64 {
        let now = now_unix_ms();
        let next = if now > self.last_kv_ms {
            now
        } else {
            self.last_kv_ms.saturating_add(1)
        };
        self.last_kv_ms = next;
        next
    }

    fn bump_writes(&mut self) {
        if self.cfg.maintain_every == 0 {
            return;
        }
        self.writes_since_maintain += 1;
        if self.writes_since_maintain >= self.cfg.maintain_every {
            self.writes_since_maintain = 0;
            if let Err(e) = self.maintain() {
                warn!("store maintenance sweep failed: {e}");
            }
        }
    }

    // -----------------------------------------------------------------------
    // 1. pages & pages_fts
    // -----------------------------------------------------------------------

    fn cache_search(&self, fts_query: &str, limit: u32) -> Result<Vec<CacheHit>, FsmError> {
        if fts_query.trim().is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        self.cache_search_inner(fts_query, limit, false)
    }

    fn cache_search_inner(
        &self,
        fts_query: &str,
        limit: u32,
        retrying: bool,
    ) -> Result<Vec<CacheHit>, FsmError> {
        let now = now_unix_s();
        let run = || -> Result<Vec<CacheHit>, rusqlite::Error> {
            // `LEFT JOIN` with `(p.id IS NULL OR p.expires_at > ?2)` ensures that
            // if an orphaned ghost rowid exists in `pages_fts`, FTS5 evaluates
            // `snippet()` against the missing external-content row and surfaces
            // the desync error so we automatically rebuild and repair the index.
            let mut stmt = self.conn.prepare(
                "SELECT
                    pages_fts.url,
                    pages_fts.title,
                    CASE
                        WHEN instr(snippet(pages_fts, 1, char(1), char(2), '', 16), char(1)) > 0
                            THEN snippet(pages_fts, 1, '[', ']', '...', 16)
                        ELSE snippet(pages_fts, -1, '[', ']', '...', 16)
                    END AS snip,
                    pages_fts.author,
                    p.published_date,
                    pages_fts.sitename,
                    p.id
                 FROM pages_fts
                 LEFT JOIN pages p ON p.id = pages_fts.rowid
                 WHERE pages_fts MATCH ?1 AND (p.id IS NULL OR p.expires_at > ?2)
                 ORDER BY bm25(pages_fts)
                 LIMIT ?3",
            )?;
            let rows = stmt.query_map(params![fts_query, now, i64::from(limit)], |row| {
                let url: String = row.get(0)?;
                let title: String = row.get(1)?;
                let snippet: String = row.get(2)?;
                let author: Option<String> = row.get(3)?;
                let date: Option<String> = row.get(4)?;
                let sitename: Option<String> = row.get(5)?;
                let page_id: Option<i64> = row.get(6)?;
                Ok((page_id, url, title, snippet, author, date, sitename))
            })?;

            let mut hits = Vec::new();
            for row in rows {
                let (page_id, url, title, snippet, author, date, sitename) = row?;
                if page_id.is_none() {
                    continue;
                }
                let mut hit = CacheHit::new(url, title, snippet);
                hit.author = author.filter(|s| !s.is_empty());
                hit.date = date.filter(|s| !s.is_empty());
                hit.sitename = sitename.filter(|s| !s.is_empty());
                hits.push(hit);
            }
            Ok(hits)
        };

        match run() {
            Ok(hits) => Ok(hits),
            Err(err) => {
                let msg = err.to_string();
                if !retrying && !is_bad_match_query(&msg) {
                    warn!(
                        "{}: rebuilding pages_fts index ({msg})",
                        self.cfg.db_path.display()
                    );
                    if let Err(rebuild_err) = self
                        .conn
                        .execute("INSERT INTO pages_fts(pages_fts) VALUES('rebuild')", [])
                    {
                        warn!(
                            "{}: pages_fts rebuild failed ({rebuild_err})",
                            self.cfg.db_path.display()
                        );
                        return Ok(Vec::new());
                    }
                    return self.cache_search_inner(fts_query, limit, true);
                }
                // Malformed FTS5 MATCH input returns empty rather than leaking SQLite text.
                Ok(Vec::new())
            }
        }
    }

    fn get_page(&self, url: &str, max_age_s: Option<u64>) -> Result<Option<PageResult>, FsmError> {
        if max_age_s == Some(0) {
            return Ok(None);
        }
        let row = self
            .conn
            .query_row(
                "SELECT url, title, content, author, published_date, sitename, media_type,
                        method, tokens_estimated, created_at, expires_at, meta_json
                 FROM pages WHERE url = ?1",
                params![url],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, Option<String>>(5)?,
                        r.get::<_, Option<String>>(6)?,
                        r.get::<_, String>(7)?,
                        r.get::<_, i64>(8)?,
                        r.get::<_, i64>(9)?,
                        r.get::<_, i64>(10)?,
                        r.get::<_, Option<String>>(11)?,
                    ))
                },
            )
            .optional()
            .map_err(|e| FsmError::internal("store_get_page", e.to_string()))?;

        let Some((
            url,
            title,
            content,
            author,
            published_date,
            sitename,
            media_type,
            method_str,
            tokens_estimated,
            created_at,
            expires_at,
            meta_json,
        )) = row
        else {
            return Ok(None);
        };

        let now = now_unix_s();
        if expires_at <= now {
            return Ok(None);
        }
        let age_s = now.saturating_sub(created_at).max(0) as u64;
        if let Some(max_age) = max_age_s
            && age_s > max_age
        {
            return Ok(None);
        }

        let meta = meta_json
            .as_deref()
            .and_then(|s| serde_json::from_str::<PageMetaJson>(s).ok());

        let total_chars = meta
            .as_ref()
            .map_or_else(|| content.chars().count(), |m| m.total_chars);
        let method = fetch_method_from_str(&method_str);
        let mut page = PageResult::new(url, title, content, method);
        page.tokens_estimated = tokens_estimated.max(0) as usize;
        page.total_chars = total_chars;
        page.author = author.filter(|s| !s.is_empty());
        page.published_date = published_date.filter(|s| !s.is_empty());
        page.sitename = sitename.filter(|s| !s.is_empty());
        page.media_type = media_type.filter(|s| !s.is_empty());
        page.cache_age_s = Some(age_s);

        if let Some(m) = meta {
            page.truncated = m.truncated;
            page.bytes = m.bytes;
            page.width = m.width;
            page.height = m.height;
            page.sha256 = m.sha256;
            page.retrieved_at = if m.retrieved_at.is_empty() {
                format_epoch_iso8601(created_at)
            } else {
                m.retrieved_at
            };
        } else {
            page.retrieved_at = format_epoch_iso8601(created_at);
        }

        Ok(Some(page))
    }

    fn put_page(&self, page: &PageResult, ttl_s: u64) -> Result<(), FsmError> {
        let now = now_unix_s();
        let effective_ttl = if ttl_s == 0 {
            self.cfg.cache_ttl_s
        } else {
            ttl_s
        };
        let expires_at = now.saturating_add(effective_ttl.min(i64::MAX as u64) as i64);

        let tokens_est = if page.tokens_estimated == 0 && !page.content.is_empty() {
            fsm_core::text::estimate_tokens(&page.content)
        } else {
            page.tokens_estimated
        };
        let total_chars = if page.total_chars == 0 && !page.content.is_empty() {
            page.content.chars().count()
        } else {
            page.total_chars
        };
        let retrieved_at = if page.retrieved_at.is_empty() {
            format_epoch_iso8601(now)
        } else {
            page.retrieved_at.clone()
        };

        let meta = PageMetaJson {
            total_chars,
            truncated: page.truncated,
            bytes: page.bytes,
            width: page.width,
            height: page.height,
            sha256: page.sha256.clone(),
            retrieved_at,
        };
        let meta_json = serde_json::to_string(&meta)
            .map_err(|e| FsmError::internal("store_put_page_meta", e.to_string()))?;

        // Rowid-preserving UPSERT: `ON CONFLICT(url) DO UPDATE` keeps `pages.id`
        // stable and fires `pages_au`, keeping `pages_fts` strictly synchronized.
        self.conn
            .execute(
                "INSERT INTO pages (
                    url, title, content, author, published_date, sitename, media_type,
                    method, tokens_estimated, created_at, expires_at, meta_json
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                 ON CONFLICT(url) DO UPDATE SET
                    title = excluded.title,
                    content = excluded.content,
                    author = excluded.author,
                    published_date = excluded.published_date,
                    sitename = excluded.sitename,
                    media_type = excluded.media_type,
                    method = excluded.method,
                    tokens_estimated = excluded.tokens_estimated,
                    created_at = excluded.created_at,
                    expires_at = excluded.expires_at,
                    meta_json = excluded.meta_json",
                params![
                    page.url,
                    page.title,
                    page.content,
                    page.author,
                    page.published_date,
                    page.sitename,
                    page.media_type,
                    fetch_method_to_str(page.method),
                    tokens_est as i64,
                    now,
                    expires_at,
                    meta_json,
                ],
            )
            .map_err(|e| FsmError::internal("store_put_page", e.to_string()))?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // 2. exec_kv (M7: "" / "shared" / caller session, 2 MiB val, 64 MiB LRU, 168h max TTL)
    // -----------------------------------------------------------------------

    fn kv_put(
        &mut self,
        session_id: &str,
        key: &str,
        val: &Value,
        ttl_s: u64,
    ) -> Result<(), FsmError> {
        // Validate key syntax and M7 `exec:` prefix isolation.
        if key.starts_with(EXEC_KV_OVERFLOW_PREFIX) {
            if session_id != EXEC_KV_OVERFLOW_SESSION {
                return Err(FsmError::InvalidInput(
                    "cache key must not start with 'exec:' (reserved for exec overflow handles)"
                        .into(),
                ));
            }
            validate_overflow_kv_key(key)?;
        } else {
            validate_kv_key(key)?;
        }

        // Pre-zstd raw JSON byte cap (2 MiB default).
        // For `exec:*` partial overflow handles (`Value::String(<first 2 MiB JSON prefix>)`,
        // SPEC §2.5.3), cap the logical string prefix at 2 MiB and enforce a hard ceiling of
        // `2 * max_val_bytes + 2` on the escaped JSON serialization; user keys enforce the
        // strict 2 MiB serialized JSON cap directly.
        let is_overflow = key.starts_with(EXEC_KV_OVERFLOW_PREFIX);
        let raw_json = serde_json::to_vec(val)
            .map_err(|e| FsmError::InvalidInput(format!("value is not serializable JSON: {e}")))?;
        let raw_bytes = raw_json.len();
        let logical_bytes = match (is_overflow, val) {
            (true, Value::String(s)) => s.len(),
            _ => raw_bytes,
        };
        let max_serialized_bytes = if is_overflow {
            self.cfg
                .exec_kv_max_val_bytes
                .saturating_mul(2)
                .saturating_add(2)
        } else {
            self.cfg.exec_kv_max_val_bytes
        };
        if logical_bytes > self.cfg.exec_kv_max_val_bytes || raw_bytes > max_serialized_bytes {
            return Err(FsmError::InvalidInput(format!(
                "exec_kv value ({logical_bytes} logical / {raw_bytes} serialized bytes) exceeds maximum allowed size of {} bytes",
                self.cfg.exec_kv_max_val_bytes
            )));
        }

        let compressed = zstd_compress(&raw_json)?;

        // Enforce TTL default (24h) and hard ceiling (168h).
        let effective_ttl = if ttl_s == 0 {
            self.cfg.exec_kv_default_ttl_s
        } else {
            ttl_s.min(self.cfg.exec_kv_max_ttl_s)
        };
        let now_s = now_unix_s();
        let expires_at = now_s.saturating_add(effective_ttl.min(i64::MAX as u64) as i64);
        let updated_at = self.next_kv_ms();

        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| FsmError::internal("store_kv_tx", e.to_string()))?;

        // Purge expired rows first so expired data never pushes out fresh LRU entries.
        tx.execute("DELETE FROM exec_kv WHERE expires_at <= ?1", params![now_s])
            .map_err(|e| FsmError::internal("store_kv_purge", e.to_string()))?;

        tx.execute(
            "INSERT INTO exec_kv (session_id, key, value_zstd, raw_bytes, expires_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(session_id, key) DO UPDATE SET
                value_zstd = excluded.value_zstd,
                raw_bytes = excluded.raw_bytes,
                expires_at = excluded.expires_at,
                updated_at = excluded.updated_at",
            params![
                session_id,
                key,
                compressed,
                raw_bytes as i64,
                expires_at,
                updated_at,
            ],
        )
        .map_err(|e| FsmError::internal("store_kv_put", e.to_string()))?;

        // Enforce 64 MiB LRU cap over sum(raw_bytes).
        enforce_exec_kv_lru(&tx, self.cfg.exec_kv_lru_max_bytes, session_id, key)?;

        tx.commit()
            .map_err(|e| FsmError::internal("store_kv_commit", e.to_string()))?;
        Ok(())
    }

    fn kv_get(&mut self, session_id: &str, key: &str) -> Result<Option<Value>, FsmError> {
        let now_s = now_unix_s();

        // M7 lookup rules:
        // 1. `exec:*` ignores caller `session_id` and reads from `session_id = ""`.
        // 2. `"shared"` reads strictly from `"shared"`.
        // 3. Any other caller `session_id` reads its own private namespace first,
        //    falling back to `"shared"` so `cache.get(key)` can read `{shared: true}` entries
        //    while private entries remain isolated between distinct sessions.
        let found: Option<(String, Vec<u8>)> = if key.starts_with(EXEC_KV_OVERFLOW_PREFIX) {
            self.conn
                .query_row(
                    "SELECT session_id, value_zstd FROM exec_kv
                     WHERE session_id = ?1 AND key = ?2 AND expires_at > ?3",
                    params![EXEC_KV_OVERFLOW_SESSION, key, now_s],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(|e| FsmError::internal("store_kv_get", e.to_string()))?
        } else if session_id == EXEC_KV_SHARED_SESSION {
            self.conn
                .query_row(
                    "SELECT session_id, value_zstd FROM exec_kv
                     WHERE session_id = ?1 AND key = ?2 AND expires_at > ?3",
                    params![EXEC_KV_SHARED_SESSION, key, now_s],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(|e| FsmError::internal("store_kv_get", e.to_string()))?
        } else {
            self.conn
                .query_row(
                    "SELECT session_id, value_zstd FROM exec_kv
                     WHERE key = ?1 AND session_id IN (?2, ?3) AND expires_at > ?4
                     ORDER BY CASE WHEN session_id = ?2 THEN 0 ELSE 1 END
                     LIMIT 1",
                    params![key, session_id, EXEC_KV_SHARED_SESSION, now_s],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(|e| FsmError::internal("store_kv_get", e.to_string()))?
        };

        let Some((matched_session, compressed)) = found else {
            return Ok(None);
        };

        // Touch `updated_at` for true LRU recency tracking.
        let touched_ms = self.next_kv_ms();
        let _ = self.conn.execute(
            "UPDATE exec_kv SET updated_at = ?1 WHERE session_id = ?2 AND key = ?3",
            params![touched_ms, matched_session, key],
        );

        let max_dec = self
            .cfg
            .exec_kv_max_val_bytes
            .saturating_mul(2)
            .saturating_add(2)
            .max(4 * 1024 * 1024 + 2);
        let value: Value = decode_json_zstd(&compressed, max_dec)?;
        Ok(Some(value))
    }

    // -----------------------------------------------------------------------
    // 3. search_cache
    // -----------------------------------------------------------------------

    fn search_cache_get(
        &self,
        key: &str,
        max_age_s: Option<u64>,
    ) -> Result<Option<(SearchResponse, u64)>, FsmError> {
        if max_age_s == Some(0) {
            return Ok(None);
        }
        let row = self
            .conn
            .query_row(
                "SELECT payload_zstd, created_at, expires_at FROM search_cache WHERE cache_key = ?1",
                params![key],
                |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)),
            )
            .optional()
            .map_err(|e| FsmError::internal("store_search_get", e.to_string()))?;

        let Some((compressed, created_at, expires_at)) = row else {
            return Ok(None);
        };
        let now = now_unix_s();
        if expires_at <= now {
            return Ok(None);
        }
        let age_s = now.saturating_sub(created_at).max(0) as u64;
        if let Some(max_age) = max_age_s
            && age_s > max_age
        {
            return Ok(None);
        }
        let resp: SearchResponse = decode_json_zstd(&compressed, MAX_JSON_BLOB_BYTES)?;
        Ok(Some((resp, age_s)))
    }

    fn search_cache_put(
        &self,
        key: &str,
        resp: &SearchResponse,
        ttl_s: u64,
    ) -> Result<(), FsmError> {
        if ttl_s == 0 {
            self.conn
                .execute(
                    "DELETE FROM search_cache WHERE cache_key = ?1",
                    params![key],
                )
                .map_err(|e| FsmError::internal("store_search_del", e.to_string()))?;
            return Ok(());
        }
        let (compressed, _) = encode_json_zstd(resp)?;
        let now = now_unix_s();
        let expires_at = now.saturating_add(ttl_s.min(i64::MAX as u64) as i64);

        self.conn
            .execute(
                "INSERT INTO search_cache (cache_key, scope, query, payload_zstd, created_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(cache_key) DO UPDATE SET
                    scope = excluded.scope,
                    query = excluded.query,
                    payload_zstd = excluded.payload_zstd,
                    created_at = excluded.created_at,
                    expires_at = excluded.expires_at",
                params![key, resp.scope, resp.query, compressed, now, expires_at],
            )
            .map_err(|e| FsmError::internal("store_search_put", e.to_string()))?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // 4. api_cache
    // -----------------------------------------------------------------------

    fn api_cache_get(&self, key: &str) -> Result<Option<ApiCacheRow>, FsmError> {
        let row = self
            .conn
            .query_row(
                "SELECT payload_zstd, etag, last_modified, expires_at
                 FROM api_cache WHERE cache_key = ?1",
                params![key],
                |r| {
                    Ok((
                        r.get::<_, Vec<u8>>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(|e| FsmError::internal("store_api_get", e.to_string()))?;

        let Some((compressed, etag, last_mod, expires_at)) = row else {
            return Ok(None);
        };
        if expires_at <= now_unix_s() {
            return Ok(None);
        }
        let raw = zstd_decompress_bounded(&compressed, MAX_API_BLOB_BYTES)?;
        Ok(Some((Bytes::from(raw), etag, last_mod)))
    }

    fn api_cache_put(
        &self,
        key: &str,
        source_id: &str,
        body: &[u8],
        etag: Option<&str>,
        last_mod: Option<&str>,
        ttl_s: u64,
    ) -> Result<(), FsmError> {
        if ttl_s == 0 {
            self.conn
                .execute("DELETE FROM api_cache WHERE cache_key = ?1", params![key])
                .map_err(|e| FsmError::internal("store_api_del", e.to_string()))?;
            return Ok(());
        }
        let compressed = zstd_compress(body)?;
        let now = now_unix_s();
        let expires_at = now.saturating_add(ttl_s.min(i64::MAX as u64) as i64);

        self.conn
            .execute(
                "INSERT INTO api_cache (cache_key, source_id, payload_zstd, etag, last_modified, created_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(cache_key) DO UPDATE SET
                    source_id = excluded.source_id,
                    payload_zstd = excluded.payload_zstd,
                    etag = excluded.etag,
                    last_modified = excluded.last_modified,
                    created_at = excluded.created_at,
                    expires_at = excluded.expires_at",
                params![key, source_id, compressed, etag, last_mod, now, expires_at],
            )
            .map_err(|e| FsmError::internal("store_api_put", e.to_string()))?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // 5. scholar_works
    // -----------------------------------------------------------------------

    fn scholar_get(&self, canonical_key_or_pid: &str) -> Result<Option<ScholarlyWork>, FsmError> {
        let query = canonical_key_or_pid.trim();
        if query.is_empty() {
            return Ok(None);
        }
        let now = now_unix_s();

        // Fast path: exact canonical_key match.
        let exact: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT payload_zstd FROM scholar_works
                 WHERE canonical_key = ?1 AND expires_at > ?2",
                params![query, now],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| FsmError::internal("store_scholar_get", e.to_string()))?;

        let blob = if let Some(b) = exact {
            Some(b)
        } else {
            let doi = normalize_doi(query);
            let arxiv = normalize_arxiv(query);
            let pmid = normalize_pmid(query);
            let openalex = normalize_openalex(query);
            self.conn
                .query_row(
                    "SELECT payload_zstd FROM scholar_works
                     WHERE expires_at > ?1 AND (
                         doi = ?2 OR arxiv = ?3 OR pmid = ?4 OR openalex = ?5
                     )
                     ORDER BY updated_at DESC
                     LIMIT 1",
                    params![now, doi, arxiv, pmid, openalex],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| FsmError::internal("store_scholar_get", e.to_string()))?
        };

        let Some(compressed) = blob else {
            return Ok(None);
        };
        let work: ScholarlyWork = decode_json_zstd(&compressed, MAX_JSON_BLOB_BYTES)?;
        Ok(Some(work))
    }

    fn scholar_put(
        &self,
        canonical_key: &str,
        work: &ScholarlyWork,
        ttl_s: u64,
    ) -> Result<(), FsmError> {
        if ttl_s == 0 {
            self.conn
                .execute(
                    "DELETE FROM scholar_works WHERE canonical_key = ?1",
                    params![canonical_key],
                )
                .map_err(|e| FsmError::internal("store_scholar_del", e.to_string()))?;
            return Ok(());
        }
        let (compressed, _) = encode_json_zstd(work)?;
        let now = now_unix_s();
        let expires_at = now.saturating_add(ttl_s.min(i64::MAX as u64) as i64);

        let doi = work.ids.doi.as_deref().and_then(normalize_doi);
        let arxiv = work
            .ids
            .arxiv
            .as_deref()
            .and_then(normalize_arxiv)
            .or_else(|| {
                doi.as_deref()
                    .filter(|d| d.starts_with("10.48550/arxiv."))
                    .and_then(normalize_arxiv)
            });
        let pmid = work.ids.pmid.as_deref().and_then(normalize_pmid);
        let openalex = work.ids.openalex.as_deref().and_then(normalize_openalex);

        self.conn
            .execute(
                "INSERT INTO scholar_works (
                    canonical_key, doi, arxiv, pmid, openalex, payload_zstd, updated_at, expires_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(canonical_key) DO UPDATE SET
                    doi = excluded.doi,
                    arxiv = excluded.arxiv,
                    pmid = excluded.pmid,
                    openalex = excluded.openalex,
                    payload_zstd = excluded.payload_zstd,
                    updated_at = excluded.updated_at,
                    expires_at = excluded.expires_at",
                params![
                    canonical_key,
                    doi,
                    arxiv,
                    pmid,
                    openalex,
                    compressed,
                    now,
                    expires_at,
                ],
            )
            .map_err(|e| FsmError::internal("store_scholar_put", e.to_string()))?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // 6. retractions & sync_state
    // -----------------------------------------------------------------------

    fn retraction_lookup(&self, norm_doi: &str) -> Result<Option<RetractionNotice>, FsmError> {
        let Some(doi) = normalize_doi(norm_doi) else {
            return Ok(None);
        };
        let row = self
            .conn
            .query_row(
                "SELECT retraction_doi, nature, retraction_date, reason, source
                 FROM retractions WHERE original_doi = ?1",
                params![doi],
                |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, String>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(|e| FsmError::internal("store_retraction_lookup", e.to_string()))?;

        let Some((notice_doi, nature, date, reason, source)) = row else {
            return Ok(None);
        };
        let mut notice = RetractionNotice::new(nature, source);
        notice.date = date.filter(|s| !s.is_empty());
        notice.reason = reason.filter(|s| !s.is_empty());
        notice.notice_doi = notice_doi.filter(|s| !s.is_empty());
        Ok(Some(notice))
    }

    fn retractions_replace_batch(
        &mut self,
        rows: Vec<(String, RetractionNotice)>,
        etag: Option<&str>,
    ) -> Result<(), FsmError> {
        let now = now_unix_s();
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| FsmError::internal("store_retractions_tx", e.to_string()))?;

        tx.execute("DELETE FROM retractions", [])
            .map_err(|e| FsmError::internal("store_retractions_clear", e.to_string()))?;

        {
            let mut stmt = tx
                .prepare(
                    "INSERT INTO retractions (
                        original_doi, retraction_doi, nature, retraction_date, reason, urls, source
                     ) VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6)
                     ON CONFLICT(original_doi) DO UPDATE SET
                        retraction_doi = excluded.retraction_doi,
                        nature = excluded.nature,
                        retraction_date = excluded.retraction_date,
                        reason = excluded.reason,
                        source = excluded.source",
                )
                .map_err(|e| FsmError::internal("store_retractions_prep", e.to_string()))?;

            for (raw_doi, notice) in rows {
                let Some(doi) = normalize_doi(&raw_doi) else {
                    continue;
                };
                let source = if notice.source.is_empty() {
                    "retraction_watch"
                } else {
                    notice.source.as_str()
                };
                stmt.execute(params![
                    doi,
                    notice.notice_doi,
                    notice.nature,
                    notice.date,
                    notice.reason,
                    source,
                ])
                .map_err(|e| FsmError::internal("store_retractions_ins", e.to_string()))?;
            }
        }

        // Atomically record completion / ETag in sync_state alongside the replaced rows.
        tx.execute(
            "INSERT INTO sync_state (name, etag, last_modified, started_at, finished_at)
             VALUES ('retraction_watch', ?1, NULL, ?2, ?2)
             ON CONFLICT(name) DO UPDATE SET
                etag = COALESCE(excluded.etag, sync_state.etag),
                finished_at = excluded.finished_at",
            params![etag, now],
        )
        .map_err(|e| FsmError::internal("store_retractions_sync", e.to_string()))?;

        tx.commit()
            .map_err(|e| FsmError::internal("store_retractions_commit", e.to_string()))?;
        Ok(())
    }

    fn sync_state_get(&self, name: &str) -> Result<Option<SyncStateRow>, FsmError> {
        self.conn
            .query_row(
                "SELECT etag, started_at, finished_at FROM sync_state WHERE name = ?1",
                params![name],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(|e| FsmError::internal("store_sync_get", e.to_string()))
    }

    fn sync_state_mark(
        &self,
        name: &str,
        etag: Option<&str>,
        started_at: i64,
        finished_at: Option<i64>,
    ) -> Result<(), FsmError> {
        self.conn
            .execute(
                "INSERT INTO sync_state (name, etag, last_modified, started_at, finished_at)
                 VALUES (?1, ?2, NULL, ?3, ?4)
                 ON CONFLICT(name) DO UPDATE SET
                    etag = excluded.etag,
                    started_at = excluded.started_at,
                    finished_at = excluded.finished_at",
                params![name, etag, started_at, finished_at],
            )
            .map_err(|e| FsmError::internal("store_sync_mark", e.to_string()))?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // 7. health_state
    // -----------------------------------------------------------------------

    fn health_load_all(&self) -> Result<Vec<HealthRecord>, FsmError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT source_id, state, failures, benched_until, last_error, updated_at
                 FROM health_state ORDER BY source_id",
            )
            .map_err(|e| FsmError::internal("store_health_load", e.to_string()))?;

        let rows = stmt
            .query_map([], |r| {
                let source_id: String = r.get(0)?;
                let state: String = r.get(1)?;
                let failures: i64 = r.get(2)?;
                let benched_until: Option<i64> = r.get(3)?;
                let last_error: Option<String> = r.get(4)?;
                let updated_at: i64 = r.get(5)?;
                let mut rec = HealthRecord::new(source_id, state, updated_at);
                rec.failures = failures.clamp(0, i64::from(u32::MAX)) as u32;
                rec.benched_until = benched_until;
                rec.last_error = last_error;
                Ok(rec)
            })
            .map_err(|e| FsmError::internal("store_health_load", e.to_string()))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| FsmError::internal("store_health_load", e.to_string()))
    }

    fn health_save(&self, rec: &HealthRecord) -> Result<(), FsmError> {
        self.conn
            .execute(
                "INSERT INTO health_state (source_id, state, failures, benched_until, last_error, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(source_id) DO UPDATE SET
                    state = excluded.state,
                    failures = excluded.failures,
                    benched_until = excluded.benched_until,
                    last_error = excluded.last_error,
                    updated_at = excluded.updated_at",
                params![
                    rec.source_id,
                    rec.state,
                    i64::from(rec.failures),
                    rec.benched_until,
                    rec.last_error,
                    rec.updated_at,
                ],
            )
            .map_err(|e| FsmError::internal("store_health_save", e.to_string()))?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // 8. downloads
    // -----------------------------------------------------------------------

    fn download_record(&self, rec: &DownloadResult, ttl_s: u64) -> Result<(), FsmError> {
        let now = now_unix_s();
        // `ttl_s == 0` disables expiry (sentinel `expires_at = 0`).
        let expires_at = if ttl_s == 0 {
            0
        } else {
            now.saturating_add(ttl_s.min(i64::MAX as u64) as i64)
        };
        let id = download_id(&rec.saved_path, &rec.sha256);

        self.conn
            .execute(
                "INSERT INTO downloads (id, url, saved_path, media_type, bytes, sha256, created_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(id) DO UPDATE SET
                    url = excluded.url,
                    saved_path = excluded.saved_path,
                    media_type = excluded.media_type,
                    bytes = excluded.bytes,
                    sha256 = excluded.sha256,
                    created_at = excluded.created_at,
                    expires_at = excluded.expires_at",
                params![
                    id,
                    rec.url,
                    rec.saved_path,
                    rec.media_type,
                    rec.bytes.min(i64::MAX as u64) as i64,
                    rec.sha256,
                    now,
                    expires_at,
                ],
            )
            .map_err(|e| FsmError::internal("store_download_record", e.to_string()))?;
        Ok(())
    }

    fn downloads_list_active(&self) -> Result<Vec<DownloadEntry>, FsmError> {
        let now = now_unix_s();
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, url, saved_path, media_type, bytes, sha256, created_at, expires_at
                 FROM downloads
                 WHERE expires_at = 0 OR expires_at > ?1
                 ORDER BY created_at DESC",
            )
            .map_err(|e| FsmError::internal("store_downloads_list", e.to_string()))?;

        let rows = stmt
            .query_map(params![now], |r| {
                Ok(DownloadEntry {
                    id: r.get(0)?,
                    url: r.get(1)?,
                    saved_path: r.get(2)?,
                    media_type: r.get(3)?,
                    bytes: r.get::<_, i64>(4)?.max(0) as u64,
                    sha256: r.get(5)?,
                    created_at: r.get(6)?,
                    expires_at: r.get(7)?,
                })
            })
            .map_err(|e| FsmError::internal("store_downloads_list", e.to_string()))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| FsmError::internal("store_downloads_list", e.to_string()))
    }

    fn downloads_purge_expired(&self) -> Result<usize, FsmError> {
        let now = now_unix_s();
        let n = self
            .conn
            .execute(
                "DELETE FROM downloads WHERE expires_at > 0 AND expires_at <= ?1",
                params![now],
            )
            .map_err(|e| FsmError::internal("store_downloads_purge", e.to_string()))?;
        Ok(n)
    }

    // -----------------------------------------------------------------------
    // 9. Extra helpers ported from cache.py
    // -----------------------------------------------------------------------

    fn complete_page_urls(&self, fragment: &str, limit: u32) -> Result<Vec<String>, FsmError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let now = now_unix_s();
        let escaped = fragment
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("%{escaped}%");
        let mut stmt = self
            .conn
            .prepare(
                "SELECT url FROM pages
                 WHERE url LIKE ?1 ESCAPE '\\' AND expires_at > ?2
                 ORDER BY created_at DESC
                 LIMIT ?3",
            )
            .map_err(|e| FsmError::internal("store_complete_urls", e.to_string()))?;
        let rows = stmt
            .query_map(params![pattern, now, i64::from(limit)], |r| r.get(0))
            .map_err(|e| FsmError::internal("store_complete_urls", e.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| FsmError::internal("store_complete_urls", e.to_string()))
    }

    fn recent_search_keys(&self, prefix: &str, limit: u32) -> Result<Vec<String>, FsmError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let now = now_unix_s();
        let escaped = prefix
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("{escaped}%");
        let mut stmt = self
            .conn
            .prepare(
                "SELECT cache_key FROM search_cache
                 WHERE cache_key LIKE ?1 ESCAPE '\\' AND expires_at > ?2
                 ORDER BY created_at DESC
                 LIMIT ?3",
            )
            .map_err(|e| FsmError::internal("store_recent_keys", e.to_string()))?;
        let rows = stmt
            .query_map(params![pattern, now, i64::from(limit)], |r| r.get(0))
            .map_err(|e| FsmError::internal("store_recent_keys", e.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| FsmError::internal("store_recent_keys", e.to_string()))
    }

    fn fts_integrity_check(&self) -> Result<(), FsmError> {
        self.conn
            .execute(
                "INSERT INTO pages_fts(pages_fts) VALUES('integrity-check')",
                [],
            )
            .map_err(|e| FsmError::internal("store_fts_integrity", e.to_string()))?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // 10. Opportunistic maintenance sweep (TTL purge + LRU + cache_max_mb cap)
    // -----------------------------------------------------------------------

    fn maintain(&mut self) -> Result<(), FsmError> {
        #[cfg(test)]
        if let Some(ref tx) = self.on_maintain_start {
            let _ = tx.send(());
        }
        self.maintain_runs += 1;
        let now = now_unix_s();

        // Step 1: Purge expired rows across all TTL-bearing tables.
        {
            let tx = self
                .conn
                .unchecked_transaction()
                .map_err(|e| FsmError::internal("store_maintain_tx", e.to_string()))?;
            tx.execute(
                "DELETE FROM search_cache WHERE expires_at <= ?1",
                params![now],
            )
            .map_err(|e| FsmError::internal("store_maintain", e.to_string()))?;
            tx.execute("DELETE FROM pages WHERE expires_at <= ?1", params![now])
                .map_err(|e| FsmError::internal("store_maintain", e.to_string()))?;
            tx.execute("DELETE FROM api_cache WHERE expires_at <= ?1", params![now])
                .map_err(|e| FsmError::internal("store_maintain", e.to_string()))?;
            tx.execute(
                "DELETE FROM scholar_works WHERE expires_at <= ?1",
                params![now],
            )
            .map_err(|e| FsmError::internal("store_maintain", e.to_string()))?;
            tx.execute("DELETE FROM exec_kv WHERE expires_at <= ?1", params![now])
                .map_err(|e| FsmError::internal("store_maintain", e.to_string()))?;
            tx.execute(
                "DELETE FROM downloads WHERE expires_at > 0 AND expires_at <= ?1",
                params![now],
            )
            .map_err(|e| FsmError::internal("store_maintain", e.to_string()))?;
            // Belt-and-braces 64 MiB LRU check on exec_kv.
            enforce_exec_kv_lru(&tx, self.cfg.exec_kv_lru_max_bytes, "", "")?;
            tx.commit()
                .map_err(|e| FsmError::internal("store_maintain_commit", e.to_string()))?;
        }

        // Step 2: Enforce overall `cache_max_mb` size cap (0 disables eviction).
        let cap = self.cfg.cache_max_mb.saturating_mul(1024 * 1024);
        if cap == 0 {
            return Ok(());
        }
        let disk_size = db_disk_size(&self.cfg.db_path);
        if disk_size <= cap {
            return Ok(());
        }

        let (mut pages, mut free, mut page_size) = query_storage(&self.conn)?;
        let mut live_size = pages.saturating_sub(free).saturating_mul(page_size);

        // Disposable cache tables evicted oldest-first (never evict retractions/sync_state/health_state).
        const EVICT_TABLES: [(&str, &str); 5] = [
            ("pages", "created_at"),
            ("api_cache", "created_at"),
            ("search_cache", "created_at"),
            ("scholar_works", "updated_at"),
            ("exec_kv", "updated_at"),
        ];

        while live_size > cap {
            let mut dropped: usize = 0;
            for (table, ts_col) in EVICT_TABLES {
                let total: i64 = self
                    .conn
                    .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                    .map_err(|e| FsmError::internal("store_maintain", e.to_string()))?;
                if total <= 0 {
                    continue;
                }
                let target_bytes = (cap as f64) * 0.8;
                let keep = ((total as f64) * target_bytes / (live_size as f64)).floor() as i64;
                let drop_n = (total - keep.max(0)).max(1);
                let del_sql = format!(
                    "DELETE FROM {table} WHERE rowid IN (
                        SELECT rowid FROM {table} ORDER BY {ts_col} ASC, rowid ASC LIMIT ?1
                    )"
                );
                let n = self
                    .conn
                    .execute(&del_sql, params![drop_n])
                    .map_err(|e| FsmError::internal("store_maintain", e.to_string()))?;
                dropped += n;

                let stats = query_storage(&self.conn)?;
                pages = stats.0;
                free = stats.1;
                page_size = stats.2;
                live_size = pages.saturating_sub(free).saturating_mul(page_size);
                if live_size <= cap {
                    break;
                }
            }
            if dropped == 0 {
                // Only schema / non-disposable floor remains.
                break;
            }
        }

        // Compact once if freed pages keep the main DB file above cap.
        if free > 0 && pages.saturating_mul(page_size) > cap {
            self.conn.flush_prepared_statement_cache();
            self.conn
                .execute_batch("VACUUM;")
                .map_err(|e| FsmError::internal("store_vacuum", e.to_string()))?;
        }

        // Reclaim WAL history without dropping valid cache entries.
        let _ = self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
        Ok(())
    }
}

/// Evict least-recently-used rows from `exec_kv` until `SUM(raw_bytes) <= cap_bytes`.
fn enforce_exec_kv_lru(
    tx: &rusqlite::Transaction<'_>,
    cap_bytes: u64,
    keep_session: &str,
    keep_key: &str,
) -> Result<(), FsmError> {
    if cap_bytes == 0 {
        return Ok(());
    }
    let mut total: i64 = tx
        .query_row("SELECT COALESCE(SUM(raw_bytes), 0) FROM exec_kv", [], |r| {
            r.get(0)
        })
        .map_err(|e| FsmError::internal("store_kv_lru", e.to_string()))?;

    let cap = cap_bytes.min(i64::MAX as u64) as i64;
    if total <= cap {
        return Ok(());
    }

    // Fetch LRU candidates oldest first; avoid evicting the just-written entry unless unavoidable.
    let victims: Vec<(String, String, i64)> = {
        let mut stmt = tx
            .prepare(
                "SELECT session_id, key, raw_bytes FROM exec_kv
                 ORDER BY updated_at ASC, rowid ASC",
            )
            .map_err(|e| FsmError::internal("store_kv_lru", e.to_string()))?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(|e| FsmError::internal("store_kv_lru", e.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| FsmError::internal("store_kv_lru", e.to_string()))?
    };

    for (sess, key, bytes) in victims {
        if total <= cap {
            break;
        }
        if !keep_key.is_empty() && sess == keep_session && key == keep_key {
            continue;
        }
        tx.execute(
            "DELETE FROM exec_kv WHERE session_id = ?1 AND key = ?2",
            params![sess, key],
        )
        .map_err(|e| FsmError::internal("store_kv_lru", e.to_string()))?;
        total = total.saturating_sub(bytes);
    }
    Ok(())
}

fn query_storage(conn: &Connection) -> Result<(u64, u64, u64), FsmError> {
    conn.query_row(
        "SELECT page_count, freelist_count, page_size
         FROM pragma_page_count(), pragma_freelist_count(), pragma_page_size()",
        [],
        |r| {
            Ok((
                r.get::<_, i64>(0)?.max(0) as u64,
                r.get::<_, i64>(1)?.max(0) as u64,
                r.get::<_, i64>(2)?.max(0) as u64,
            ))
        },
    )
    .map_err(|e| FsmError::internal("store_pragma_storage", e.to_string()))
}

/// Current on-disk footprint of the SQLite database file plus its WAL file.
pub(crate) fn db_disk_size(db_path: &Path) -> u64 {
    let main_size = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
    let mut wal_os = db_path.as_os_str().to_os_string();
    wal_os.push("-wal");
    let wal_size = std::fs::metadata(Path::new(&wal_os))
        .map(|m| m.len())
        .unwrap_or(0);
    main_size.saturating_add(wal_size)
}

fn download_id(saved_path: &str, sha256: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(saved_path.as_bytes());
    hasher.update([0u8]);
    hasher.update(sha256.as_bytes());
    let digest = hex::encode(hasher.finalize());
    digest[..32].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SqliteStore;
    use fsm_core::{FetchMethod, StoreApi};
    use tempfile::TempDir;

    /// B1 part 2 / Missing test #2: a panic inside one actor command must NOT kill
    /// the actor thread, drop queued commands, or poison subsequent calls.
    #[tokio::test]
    async fn test_actor_panic_does_not_drop_queued_commands_or_poison_store() {
        let dir = TempDir::new().unwrap();
        let mut cfg = StoreConfig::at_path(dir.path().join("search-mcp.sqlite"));
        cfg.maintain_on_open = false;
        let store = SqliteStore::open_with(cfg).unwrap();

        // Hold an external write lock briefly so put_page#0, TestPanic, and put_page#1..=5
        // all queue behind it inside the actor's mpsc channel simultaneously.
        let side = Connection::open(store.path()).unwrap();
        side.execute_batch("BEGIN IMMEDIATE;").unwrap();

        let p0 = {
            let s = store.clone();
            tokio::spawn(async move {
                s.put_page(
                    &PageResult::new("https://example.com/0", "T0", "body0", FetchMethod::Http),
                    3600,
                )
                .await
            })
        };
        // Give p0 a moment to dequeue and block on SQLite busy_timeout, then enqueue panic + 5 writes.
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;

        let panicking = {
            let s = store.clone();
            tokio::spawn(async move { s.call(|reply| Cmd::TestPanic { reply }).await })
        };
        let mut trailing = Vec::new();
        for i in 1..=5 {
            let s = store.clone();
            trailing.push(tokio::spawn(async move {
                s.put_page(
                    &PageResult::new(
                        format!("https://example.com/{i}"),
                        format!("T{i}"),
                        format!("body{i}"),
                        FetchMethod::Http,
                    ),
                    3600,
                )
                .await
            }));
        }

        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        side.execute_batch("COMMIT;").unwrap();
        drop(side);

        // First write succeeded; panicking command errored cleanly without killing the actor;
        // every single queued write after it succeeded!
        p0.await.unwrap().expect("p0 must succeed");
        assert!(
            panicking.await.unwrap().is_err(),
            "panicking command should return Err"
        );
        for (idx, h) in trailing.into_iter().enumerate() {
            h.await
                .unwrap()
                .unwrap_or_else(|e| panic!("queued write #{} dropped after panic: {e}", idx + 1));
        }

        // Subsequent reads confirm all 6 writes landed and the actor is still healthy.
        for i in 0..=5 {
            let got = store
                .get_page(&format!("https://example.com/{i}"), None)
                .await
                .unwrap()
                .unwrap_or_else(|| panic!("page {i} missing"));
            assert_eq!(got.content, format!("body{i}"));
        }
        store.close().await.unwrap();
    }

    /// m2 regression: on the write that triggers a maintenance cadence sweep,
    /// the write caller's reply MUST be sent BEFORE `maintain()` runs.
    #[test]
    fn test_m2_write_reply_sent_before_maintenance_sweep() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("m2.sqlite");
        let conn = Connection::open(&db_path).unwrap();
        init_connection(&conn).unwrap();

        // Rendezvous channel (capacity 0): `maintain()` blocks on `send(())` until
        // the test calls `hook_rx.recv()`. If `dispatch_cmd` ran `maintain()` before
        // sending `reply`, `reply_rx` would never receive before timeout.
        let (hook_tx, hook_rx) = std::sync::mpsc::sync_channel::<()>(0);
        let mut cfg = StoreConfig::at_path(&db_path);
        cfg.maintain_every = 1;
        cfg.maintain_on_open = false;

        let mut state = ActorState {
            conn,
            cfg,
            writes_since_maintain: 0,
            maintain_runs: 0,
            last_kv_ms: 0,
            on_maintain_start: Some(hook_tx),
        };

        let (reply_tx, mut reply_rx) = oneshot::channel();
        let cmd = Cmd::PutPage {
            page: Box::new(PageResult::new(
                "https://example.com/m2",
                "M2",
                "body",
                FetchMethod::Http,
            )),
            ttl_s: 3600,
            reply: reply_tx,
        };

        let worker = std::thread::spawn(move || {
            dispatch_cmd(&mut state, cmd);
            state.maintain_runs
        });

        // Wait up to 1s for the reply BEFORE releasing the maintenance hook.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        let reply = loop {
            match reply_rx.try_recv() {
                Ok(res) => break res,
                Err(oneshot::error::TryRecvError::Empty) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "write reply was blocked behind maintain()!"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                Err(oneshot::error::TryRecvError::Closed) => panic!("reply channel closed"),
            }
        };
        reply.expect("write should succeed before maintenance completes");

        // Now unblock `maintain()` and verify the sweep ran.
        hook_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("maintain() should run right after sending reply");
        assert_eq!(worker.join().unwrap(), 1);
    }
}
