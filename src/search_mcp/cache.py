import asyncio
import json
import logging
import os
import sqlite3
import time
from typing import Any

import aiosqlite

from .config import settings

log = logging.getLogger(__name__)

_SCHEMA = """
CREATE TABLE IF NOT EXISTS search_cache (
    cache_key TEXT PRIMARY KEY,
    query     TEXT NOT NULL,
    engines   TEXT NOT NULL,
    results   TEXT NOT NULL,
    created   INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS pages (
    url      TEXT PRIMARY KEY,
    title    TEXT,
    content  TEXT NOT NULL,
    fetched  INTEGER NOT NULL
);

-- Maintenance (TTL purge + oldest-first eviction) filters and sorts on the
-- timestamps; without these, every pass full-scans the blob-heavy tables.
CREATE INDEX IF NOT EXISTS pages_fetched_idx ON pages(fetched);
CREATE INDEX IF NOT EXISTS search_cache_created_idx ON search_cache(created);

CREATE VIRTUAL TABLE IF NOT EXISTS pages_fts USING fts5(
    url UNINDEXED,
    title,
    content,
    content='pages',
    content_rowid='rowid',
    tokenize='unicode61 remove_diacritics 2'
);

CREATE TRIGGER IF NOT EXISTS pages_ai AFTER INSERT ON pages BEGIN
    INSERT INTO pages_fts(rowid, url, title, content)
    VALUES (new.rowid, new.url, new.title, new.content);
END;

CREATE TRIGGER IF NOT EXISTS pages_ad AFTER DELETE ON pages BEGIN
    INSERT INTO pages_fts(pages_fts, rowid, url, title, content)
    VALUES ('delete', old.rowid, old.url, old.title, old.content);
END;

CREATE TRIGGER IF NOT EXISTS pages_au AFTER UPDATE ON pages BEGIN
    INSERT INTO pages_fts(pages_fts, rowid, url, title, content)
    VALUES ('delete', old.rowid, old.url, old.title, old.content);
    INSERT INTO pages_fts(rowid, url, title, content)
    VALUES (new.rowid, new.url, new.title, new.content);
END;
"""


# A malformed MATCH query is the caller's input and must never trigger a repair.
# Everything else that SQLite can raise here is treated as a possible index
# desync, because the wording of that one is not portable: external-content
# FTS5 reads column values back from `pages` by rowid, and a posting for a row
# that is gone surfaces as `fts5: missing row N from content table …` on
# SQLite 3.42+ but as a bare SQLITE_CORRUPT_VTAB ("database disk image is
# malformed") on older builds — which is what the distro SQLite behind some
# CI interpreters still reports. Matching on the corruption wording therefore
# fails silently on exactly the installs most likely to hold a poisoned cache.
# Classifying the safe-to-ignore case instead is version-independent, and the
# cost of being wrong is one bounded rebuild.
_BAD_MATCH_MARKERS = (
    "syntax error",
    "no such column",
    "unknown special query",
    "unterminated string",
)


def _is_bad_match_query(exc: BaseException) -> bool:
    """True when the MATCH expression itself is invalid, e.g. `a AND` or `x:1`."""
    text = str(exc).lower()
    return any(marker in text for marker in _BAD_MATCH_MARKERS)


# Bumped when an existing cache FILE needs work that `CREATE TABLE IF NOT
# EXISTS` cannot do. 1 = rebuild `pages_fts`, whose index was corrupted by the
# `INSERT OR REPLACE` in the old `put_page` (see the comment there).
_USER_VERSION = 1


class Cache:
    # Opportunistic maintenance cadence: once at connection init, then every
    # N writes. No background tasks, so the stdio server lifecycle stays simple.
    _MAINTAIN_EVERY = 200

    def __init__(self) -> None:
        self._path = str(settings.cache_path())
        self._conn_obj: aiosqlite.Connection | None = None
        self._lock = asyncio.Lock()
        self._writes_since_maintain = 0
        # Reference to the in-flight background maintenance task (if any) so
        # it isn't garbage-collected mid-run and never overlaps itself.
        self._maintain_task: asyncio.Task | None = None

    async def _conn(self) -> aiosqlite.Connection:
        """Return the single long-lived connection, creating it once.

        The connection is opened (a single background thread) and the schema +
        pragmas are applied exactly once, under a lock so that concurrent first
        callers don't race the initialization or end up with two connections.
        """
        if self._conn_obj is not None:
            return self._conn_obj
        async with self._lock:
            # Re-check inside the lock: another coroutine may have initialized
            # the connection while we were waiting to acquire it.
            if self._conn_obj is not None:
                return self._conn_obj
            conn = aiosqlite.connect(self._path)
            # aiosqlite drives SQLite on a private, non-daemon worker thread.
            # This connection is long-lived and may never be explicitly closed
            # (interpreter exit, or a test runner reusing the module singleton
            # across event loops), and a live non-daemon thread blocks process
            # shutdown forever. Mark the worker daemon BEFORE it starts so a
            # missing close() can never hang exit. The cache holds disposable
            # data: with WAL + synchronous=NORMAL an abrupt exit can lose the
            # last few commits but never corrupts the file.
            worker = getattr(conn, "_thread", None)
            if worker is not None:
                worker.daemon = True
            conn = await conn
            try:
                # WAL lets readers and a writer proceed concurrently; the
                # busy_timeout makes a contended writer wait instead of
                # immediately raising 'database is locked'.
                await conn.execute("PRAGMA journal_mode=WAL")
                await conn.execute("PRAGMA busy_timeout=5000")
                # NORMAL is the documented safe default under WAL: fsync at
                # checkpoints instead of every commit, so each put_page /
                # put_search stops paying a per-write fsync.
                await conn.execute("PRAGMA synchronous=NORMAL")
                await conn.execute("PRAGMA temp_store=MEMORY")
                await conn.executescript(_SCHEMA)
                await self._migrate(conn)
                await conn.commit()
            except BaseException:
                await conn.close()
                raise
            self._conn_obj = conn
            # Maintenance runs OFF the caller's critical path: a large cache's
            # VACUUM must never stall the first tool call of a session behind
            # the init lock. aiosqlite serializes statements on its worker
            # thread, so the background task can safely share the connection.
            self._spawn_maintenance(conn)
            return conn

    def _spawn_maintenance(self, conn: aiosqlite.Connection) -> None:
        """Fire-and-forget a maintenance pass; at most one in flight."""
        if self._maintain_task is not None and not self._maintain_task.done():
            return
        self._maintain_task = asyncio.get_running_loop().create_task(
            self._maintain(conn)
        )

    def _db_size(self) -> int:
        """Current on-disk footprint: main db file + WAL (best-effort)."""
        size = 0
        for suffix in ("", "-wal"):
            try:
                size += os.path.getsize(self._path + suffix)
            except OSError:
                pass
        return size

    async def _maintain(self, conn: aiosqlite.Connection) -> None:
        """Purge expired rows, then enforce the size cap. Never raises.

        DELETEs alone never shrink a SQLite file (freed pages go to the
        freelist), so when the file exceeds cache_max_mb we drop the oldest
        ``pages`` rows down to a size-proportional target and run ONE VACUUM
        (+WAL truncate) to actually return the space. ``pages`` dominates the
        footprint; ``search_cache`` rows are small and age out via TTL.
        """
        try:
            cutoff = int(time.time()) - settings.cache_ttl_seconds
            await conn.execute("DELETE FROM search_cache WHERE created < ?", (cutoff,))
            await conn.execute("DELETE FROM pages WHERE fetched < ?", (cutoff,))
            await conn.commit()

            cap = settings.cache_max_mb * 1024 * 1024
            size = self._db_size()
            if cap <= 0 or size <= cap:
                return
            cur = await conn.execute("SELECT COUNT(*) FROM pages")
            row = await cur.fetchone()
            total = row[0] if row else 0
            if total:
                # Keep the newest rows that fit ~80% of the cap, assuming size
                # scales with row count. The FTS delete-trigger keeps pages_fts
                # in sync.
                keep = int(total * (cap * 0.8) / size)
                drop = max(1, total - keep)
                await conn.execute(
                    "DELETE FROM pages WHERE rowid IN "
                    "(SELECT rowid FROM pages ORDER BY fetched ASC LIMIT ?)",
                    (drop,),
                )
                await conn.commit()
            await conn.execute("VACUUM")
            await conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")
        except Exception as e:
            # Maintenance is best-effort housekeeping — a failure (locked db,
            # disk error) must never take down the actual read/write path.
            log.warning("cache maintenance failed: %s", e)

    async def _bump_writes(self, conn: aiosqlite.Connection) -> None:
        self._writes_since_maintain += 1
        if self._writes_since_maintain >= self._MAINTAIN_EVERY:
            self._writes_since_maintain = 0
            self._spawn_maintenance(conn)

    async def close(self) -> None:
        """Close the long-lived connection, if any. Safe to call repeatedly."""
        async with self._lock:
            if self._maintain_task is not None and not self._maintain_task.done():
                self._maintain_task.cancel()
                try:
                    await self._maintain_task
                except asyncio.CancelledError:
                    # Swallow only OUR cancellation of the maintenance task;
                    # if close() itself is being cancelled the task won't have
                    # reached the cancelled state — propagate in that case.
                    if not self._maintain_task.cancelled():
                        raise
                except Exception:
                    pass
            self._maintain_task = None
            if self._conn_obj is not None:
                await self._conn_obj.close()
                self._conn_obj = None

    @staticmethod
    async def _migrate(conn: aiosqlite.Connection) -> None:
        """Additive column migrations. Idempotent, cheap, never destructive.

        `CREATE TABLE IF NOT EXISTS` cannot add a column to a table that already
        exists, so a schema that grew after a user's cache file was created has
        to be reconciled here. SQLite's `ADD COLUMN` is O(1) and backfills NULL,
        which is the correct value for every pre-existing row: we genuinely do
        not know the provenance of a result cached before we started recording
        it.
        """
        cur = await conn.execute("PRAGMA table_info(search_cache)")
        columns = {row[1] for row in await cur.fetchall()}
        if "meta" not in columns:
            await conn.execute("ALTER TABLE search_cache ADD COLUMN meta TEXT")

        # One-time repair of caches written before `put_page` stopped using
        # `INSERT OR REPLACE`. Those files carry orphaned `pages_fts` postings
        # that make every `cache_search` raise, so the fix above only helps
        # rows written from now on — the existing index still has to be
        # rebuilt. `user_version` gates it to exactly once per file; the
        # rebuild is O(corpus) and must not run on every start.
        cur = await conn.execute("PRAGMA user_version")
        row = await cur.fetchone()
        if (row[0] if row else 0) < _USER_VERSION:
            await conn.execute("INSERT INTO pages_fts(pages_fts) VALUES('rebuild')")
            await conn.execute(f"PRAGMA user_version={_USER_VERSION}")

    async def get_search(
        self, key: str, max_age_seconds: int | None = None,
    ) -> tuple[list[dict[str, Any]], dict[str, Any]] | None:
        """Cached results plus the provenance recorded alongside them.

        Returns `(results, meta)`; `meta` is `{}` for rows written before the
        `meta` column existed. Provenance is part of the ANSWER, not a
        statistic about the run that produced it: a result set that only exists
        because a captcha-walled engine was rescued via searx must still say so
        on the cache-hit path, or the second identical query silently
        re-labels a recovered result set as a normal one.
        """
        conn = await self._conn()
        cur = await conn.execute(
            "SELECT results, created, meta FROM search_cache WHERE cache_key=?",
            (key,),
        )
        row = await cur.fetchone()
        if not row:
            return None
        ttl = max_age_seconds if max_age_seconds is not None else settings.cache_ttl_seconds
        if time.time() - row[1] > ttl:
            return None
        meta: dict[str, Any] = {}
        if row[2]:
            try:
                loaded = json.loads(row[2])
            except ValueError:
                loaded = None
            if isinstance(loaded, dict):
                meta = loaded
        # When the row was written — so a hit can say how old it is. Derived
        # from the row on every read, never stored inside `meta`.
        meta["cached_at"] = row[1]
        return json.loads(row[0]), meta

    async def put_search(
        self,
        key: str,
        query: str,
        engines: list[str],
        results: list[dict[str, Any]],
        meta: dict[str, Any] | None = None,
    ) -> None:
        conn = await self._conn()
        await conn.execute(
            "INSERT OR REPLACE INTO search_cache "
            "(cache_key, query, engines, results, created, meta) "
            "VALUES (?, ?, ?, ?, ?, ?)",
            (
                key,
                query,
                ",".join(engines),
                json.dumps(results, ensure_ascii=False),
                int(time.time()),
                json.dumps(meta, ensure_ascii=False) if meta else None,
            ),
        )
        await conn.commit()
        await self._bump_writes(conn)

    async def get_page(
        self, url: str, max_age_seconds: int | None = None,
    ) -> dict[str, Any] | None:
        conn = await self._conn()
        cur = await conn.execute(
            "SELECT title, content, fetched FROM pages WHERE url=?",
            (url,),
        )
        row = await cur.fetchone()
        if not row:
            return None
        ttl = max_age_seconds if max_age_seconds is not None else settings.cache_ttl_seconds
        if time.time() - row[2] > ttl:
            return None
        return {"url": url, "title": row[0], "content": row[1], "fetched": row[2]}

    async def put_page(self, url: str, title: str | None, content: str) -> None:
        conn = await self._conn()
        # UPSERT, deliberately NOT `INSERT OR REPLACE`. REPLACE resolves the
        # url conflict by DELETING the old row and inserting a new one with a
        # NEW rowid — and SQLite fires DELETE triggers on that path only when
        # `recursive_triggers` is on, which it is not by default. So every
        # re-fetch of a URL left the old rowid's postings orphaned in
        # `pages_fts` while adding postings for the new one. External-content
        # FTS5 reads column values back from `pages` by rowid, so the first
        # orphan turned EVERY `cache_search` into
        # `fts5: missing row N from content table` — swallowed by the
        # malformed-query handler below and reported as "no cached pages
        # match". `DO UPDATE` keeps the rowid and fires `pages_au`, which
        # replaces exactly the one FTS row that changed.
        await conn.execute(
            "INSERT INTO pages (url, title, content, fetched) VALUES (?, ?, ?, ?) "
            "ON CONFLICT(url) DO UPDATE SET "
            "title=excluded.title, content=excluded.content, fetched=excluded.fetched",
            (url, title or "", content, int(time.time())),
        )
        await conn.commit()
        await self._bump_writes(conn)

    async def complete_page_urls(self, fragment: str, limit: int = 50) -> list[str]:
        """Cached page URLs containing `fragment`, most recently fetched first.

        For MCP completion of `cache://page/{url}`. LIKE with the wildcards
        escaped: a fragment is text somebody typed, not a pattern.
        """
        needle = fragment.replace("\\", "\\\\").replace("%", "\\%").replace("_", "\\_")
        conn = await self._conn()
        cur = await conn.execute(
            "SELECT url FROM pages WHERE url LIKE ? ESCAPE '\\' ORDER BY fetched DESC LIMIT ?",
            (f"%{needle}%", limit),
        )
        return [row[0] for row in await cur.fetchall()]

    async def recent_search_keys(self, prefix: str = "", limit: int = 50) -> list[str]:
        """Cache keys of stored searches starting with `prefix`, newest first."""
        conn = await self._conn()
        cur = await conn.execute(
            "SELECT cache_key FROM search_cache WHERE cache_key LIKE ? "
            "ORDER BY created DESC LIMIT ?",
            (f"{prefix}%", limit),
        )
        return [row[0] for row in await cur.fetchall()]

    async def search_pages(
        self, query: str, limit: int = 10, *, _retrying: bool = False
    ) -> list[dict[str, Any]]:
        conn = await self._conn()
        try:
            cur = await conn.execute(
                "SELECT url, title, snippet(pages_fts, 2, '[', ']', '...', 16) "
                "FROM pages_fts WHERE pages_fts MATCH ? "
                "ORDER BY bm25(pages_fts) LIMIT ?",
                (query, limit),
            )
            rows = await cur.fetchall()
        except (sqlite3.OperationalError, aiosqlite.Error) as exc:
            if not _retrying and not _is_bad_match_query(exc):
                # Not the caller's query: most likely the index points at rows
                # the content table no longer has. Rebuilding is the documented
                # repair and is cheap relative to having the feature silently
                # return nothing, which is what the blanket handler below did
                # for every query against a desynced index.
                log.warning("%s: rebuilding FTS index (%s)", self._path, exc)
                try:
                    await conn.execute("INSERT INTO pages_fts(pages_fts) VALUES('rebuild')")
                    await conn.commit()
                except (sqlite3.OperationalError, aiosqlite.Error) as rebuild_exc:
                    # Damage a rebuild cannot undo. Still not the caller's
                    # problem to see as a traceback.
                    log.warning("%s: FTS rebuild failed (%s)", self._path, rebuild_exc)
                    return []
                return await self.search_pages(query, limit, _retrying=True)
            # Malformed FTS5 MATCH input (e.g. 'a AND', a bare quote, or a
            # 'col:val' phrase against an unknown column) raises a SQLite
            # syntax error. Treat it as "no matches" rather than leaking raw
            # SQLite text to the caller. A friendly hint is added upstream.
            return []
        return [{"url": r[0], "title": r[1], "snippet": r[2]} for r in rows]


cache = Cache()
