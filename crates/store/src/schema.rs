//! SQLite schema, pragmas, and idempotent initialization for `search-mcp.sqlite` (`SPEC.md` §8).

use fsm_core::FsmError;
use fsm_core::ext::w0c::SCHEMA_USER_VERSION;
use rusqlite::Connection;

/// All 10 tables, indexes, and rowid-preserving FTS5 triggers for `search-mcp.sqlite`.
///
/// Notes:
/// - `pages.meta_json` (nullable) is an additive column preserving optional `PageResult`
///   fields (`total_chars`, `truncated`, `bytes`, `width`, `height`, `sha256`, `retrieved_at`)
///   so `put_page` -> `get_page` is lossless while remaining 100% compatible with rows
///   carrying only the 12 core `SPEC.md` §8 columns.
/// - `retractions.source` (default `'retraction_watch'`) preserves `RetractionNotice.source`.
/// - `pages_fts` uses `content='pages', content_rowid='id'` with `AFTER INSERT`/`DELETE`/`UPDATE`
///   triggers; `put_page` uses `ON CONFLICT(url) DO UPDATE` so `pages.id` (rowid) stays stable.
pub const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS search_cache (
    cache_key TEXT PRIMARY KEY,
    scope TEXT NOT NULL,
    query TEXT NOT NULL,
    payload_zstd BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_search_cache_exp ON search_cache(expires_at);
CREATE INDEX IF NOT EXISTS idx_search_cache_created ON search_cache(created_at);

CREATE TABLE IF NOT EXISTS pages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    url TEXT UNIQUE NOT NULL,
    title TEXT NOT NULL,
    content TEXT NOT NULL,
    author TEXT,
    published_date TEXT,
    sitename TEXT,
    media_type TEXT,
    method TEXT NOT NULL,
    tokens_estimated INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    meta_json TEXT
);
CREATE INDEX IF NOT EXISTS idx_pages_exp ON pages(expires_at);
CREATE INDEX IF NOT EXISTS idx_pages_created ON pages(created_at);

CREATE VIRTUAL TABLE IF NOT EXISTS pages_fts USING fts5(
    title,
    content,
    author,
    sitename,
    url,
    content='pages',
    content_rowid='id',
    tokenize='unicode61 remove_diacritics 2'
);

CREATE TRIGGER IF NOT EXISTS pages_ai AFTER INSERT ON pages BEGIN
    INSERT INTO pages_fts(rowid, title, content, author, sitename, url)
    VALUES (new.id, new.title, new.content, new.author, new.sitename, new.url);
END;

CREATE TRIGGER IF NOT EXISTS pages_ad AFTER DELETE ON pages BEGIN
    INSERT INTO pages_fts(pages_fts, rowid, title, content, author, sitename, url)
    VALUES ('delete', old.id, old.title, old.content, old.author, old.sitename, old.url);
END;

CREATE TRIGGER IF NOT EXISTS pages_au AFTER UPDATE ON pages BEGIN
    INSERT INTO pages_fts(pages_fts, rowid, title, content, author, sitename, url)
    VALUES ('delete', old.id, old.title, old.content, old.author, old.sitename, old.url);
    INSERT INTO pages_fts(rowid, title, content, author, sitename, url)
    VALUES (new.id, new.title, new.content, new.author, new.sitename, new.url);
END;

CREATE TABLE IF NOT EXISTS api_cache (
    cache_key TEXT PRIMARY KEY,
    source_id TEXT NOT NULL,
    payload_zstd BLOB NOT NULL,
    etag TEXT,
    last_modified TEXT,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_api_cache_exp ON api_cache(expires_at);
CREATE INDEX IF NOT EXISTS idx_api_cache_created ON api_cache(created_at);

CREATE TABLE IF NOT EXISTS scholar_works (
    canonical_key TEXT PRIMARY KEY,
    doi TEXT,
    arxiv TEXT,
    pmid TEXT,
    openalex TEXT,
    payload_zstd BLOB NOT NULL,
    updated_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_scholar_doi ON scholar_works(doi);
CREATE INDEX IF NOT EXISTS idx_scholar_arxiv ON scholar_works(arxiv);
CREATE INDEX IF NOT EXISTS idx_scholar_pmid ON scholar_works(pmid);
CREATE INDEX IF NOT EXISTS idx_scholar_openalex ON scholar_works(openalex);
CREATE INDEX IF NOT EXISTS idx_scholar_exp ON scholar_works(expires_at);

CREATE TABLE IF NOT EXISTS retractions (
    original_doi TEXT PRIMARY KEY,
    retraction_doi TEXT,
    nature TEXT NOT NULL,
    retraction_date TEXT,
    reason TEXT,
    urls TEXT,
    source TEXT NOT NULL DEFAULT 'retraction_watch'
);

CREATE TABLE IF NOT EXISTS sync_state (
    name TEXT PRIMARY KEY,
    etag TEXT,
    last_modified TEXT,
    started_at INTEGER NOT NULL,
    finished_at INTEGER
);

CREATE TABLE IF NOT EXISTS exec_kv (
    session_id TEXT NOT NULL,
    key TEXT NOT NULL,
    value_zstd BLOB NOT NULL,
    raw_bytes INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (session_id, key)
);
CREATE INDEX IF NOT EXISTS idx_exec_kv_exp ON exec_kv(expires_at);
CREATE INDEX IF NOT EXISTS idx_exec_kv_lru ON exec_kv(updated_at);

CREATE TABLE IF NOT EXISTS health_state (
    source_id TEXT PRIMARY KEY,
    state TEXT NOT NULL,
    failures INTEGER NOT NULL,
    benched_until INTEGER,
    last_error TEXT,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS downloads (
    id TEXT PRIMARY KEY,
    url TEXT NOT NULL,
    saved_path TEXT NOT NULL,
    media_type TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    sha256 TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_downloads_exp ON downloads(expires_at);
"#;

/// Configure pragmas, create all 10 tables, and stamp `PRAGMA user_version = 1`.
///
/// Mirrors Python `cache._migrate`: locks with `BEGIN IMMEDIATE` before reading
/// `user_version` so concurrent processes opening the same DB do not race schema
/// initialization or FTS rebuild.
pub fn init_connection(conn: &Connection) -> Result<(), FsmError> {
    conn.busy_timeout(std::time::Duration::from_millis(5000))
        .map_err(|e| FsmError::internal("store_pragma", e.to_string()))?;
    // Switching a fresh file to WAL takes an exclusive lock and SQLite can
    // report SQLITE_BUSY without consulting the busy handler when several
    // processes open the same new database at once; retry with backoff.
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(5000);
    let mut pause = std::time::Duration::from_millis(5);
    loop {
        match conn.execute_batch("PRAGMA journal_mode = WAL;") {
            Ok(()) => break,
            Err(e) if is_busy(&e) && std::time::Instant::now() < deadline => {
                std::thread::sleep(pause);
                pause = (pause * 2).min(std::time::Duration::from_millis(100));
            }
            Err(e) => return Err(FsmError::internal("store_pragma", e.to_string())),
        }
    }
    conn.execute_batch(
        "PRAGMA synchronous = NORMAL;
         PRAGMA temp_store = MEMORY;
         PRAGMA foreign_keys = ON;",
    )
    .map_err(|e| FsmError::internal("store_pragma", e.to_string()))?;

    // Acquire write lock before inspecting/creating schema and user_version.
    conn.execute_batch("BEGIN IMMEDIATE;")
        .map_err(|e| FsmError::internal("store_init_begin", e.to_string()))?;

    let init_res = (|| -> Result<(), FsmError> {
        conn.execute_batch(SCHEMA_SQL)
            .map_err(|e| FsmError::internal("store_schema", e.to_string()))?;

        // Additive column guards for files created externally with minimal §8 columns.
        ensure_column(conn, "pages", "meta_json", "TEXT")?;
        ensure_column(
            conn,
            "retractions",
            "source",
            "TEXT NOT NULL DEFAULT 'retraction_watch'",
        )?;

        let version: u32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(|e| FsmError::internal("store_user_version", e.to_string()))?;

        if version < SCHEMA_USER_VERSION {
            // Rebuilds external-content FTS5 index once per file when user_version == 0
            // (mirrors cache.py's one-time repair gate).
            conn.execute("INSERT INTO pages_fts(pages_fts) VALUES('rebuild')", [])
                .map_err(|e| FsmError::internal("store_fts_rebuild", e.to_string()))?;
            conn.pragma_update(None, "user_version", SCHEMA_USER_VERSION)
                .map_err(|e| FsmError::internal("store_user_version", e.to_string()))?;
        }
        Ok(())
    })();

    match init_res {
        Ok(()) => {
            conn.execute_batch("COMMIT;")
                .map_err(|e| FsmError::internal("store_init_commit", e.to_string()))?;
            Ok(())
        }
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK;");
            Err(err)
        }
    }
}

fn ensure_column(
    conn: &Connection,
    table: &str,
    column: &str,
    col_def: &str,
) -> Result<(), FsmError> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut stmt = conn
        .prepare(&pragma)
        .map_err(|e| FsmError::internal("store_migrate", e.to_string()))?;
    let exists = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| FsmError::internal("store_migrate", e.to_string()))?
        .filter_map(Result::ok)
        .any(|name| name == column);
    if !exists {
        let sql = format!("ALTER TABLE {table} ADD COLUMN {column} {col_def}");
        conn.execute(&sql, [])
            .map_err(|e| FsmError::internal("store_migrate", e.to_string()))?;
    }
    Ok(())
}

fn is_busy(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(f, _)
            if f.code == rusqlite::ErrorCode::DatabaseBusy || f.code == rusqlite::ErrorCode::DatabaseLocked
    )
}
