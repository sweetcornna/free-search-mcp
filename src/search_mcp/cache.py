import asyncio
import json
import sqlite3
import time
from typing import Any

import aiosqlite

from .config import settings

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


class Cache:
    def __init__(self) -> None:
        self._path = str(settings.cache_path())
        self._conn_obj: aiosqlite.Connection | None = None
        self._lock = asyncio.Lock()

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
            # missing close() can never hang exit. WAL durability is unaffected
            # because every commit already fsyncs.
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
                await conn.executescript(_SCHEMA)
                await conn.commit()
            except BaseException:
                await conn.close()
                raise
            self._conn_obj = conn
            return conn

    async def close(self) -> None:
        """Close the long-lived connection, if any. Safe to call repeatedly."""
        async with self._lock:
            if self._conn_obj is not None:
                await self._conn_obj.close()
                self._conn_obj = None

    async def get_search(
        self, key: str, max_age_seconds: int | None = None,
    ) -> list[dict[str, Any]] | None:
        conn = await self._conn()
        cur = await conn.execute(
            "SELECT results, created FROM search_cache WHERE cache_key=?",
            (key,),
        )
        row = await cur.fetchone()
        if not row:
            return None
        ttl = max_age_seconds if max_age_seconds is not None else settings.cache_ttl_seconds
        if time.time() - row[1] > ttl:
            return None
        return json.loads(row[0])

    async def put_search(self, key: str, query: str, engines: list[str], results: list[dict[str, Any]]) -> None:
        conn = await self._conn()
        await conn.execute(
            "INSERT OR REPLACE INTO search_cache (cache_key, query, engines, results, created) "
            "VALUES (?, ?, ?, ?, ?)",
            (key, query, ",".join(engines), json.dumps(results, ensure_ascii=False), int(time.time())),
        )
        await conn.commit()

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
        await conn.execute(
            "INSERT OR REPLACE INTO pages (url, title, content, fetched) VALUES (?, ?, ?, ?)",
            (url, title or "", content, int(time.time())),
        )
        await conn.commit()

    async def search_pages(self, query: str, limit: int = 10) -> list[dict[str, Any]]:
        conn = await self._conn()
        try:
            cur = await conn.execute(
                "SELECT url, title, snippet(pages_fts, 2, '[', ']', '...', 16) "
                "FROM pages_fts WHERE pages_fts MATCH ? "
                "ORDER BY bm25(pages_fts) LIMIT ?",
                (query, limit),
            )
            rows = await cur.fetchall()
        except (sqlite3.OperationalError, aiosqlite.Error):
            # Malformed FTS5 MATCH input (e.g. 'a AND', a bare quote, or a
            # 'col:val' phrase against an unknown column) raises a SQLite
            # syntax error. Treat it as "no matches" rather than leaking raw
            # SQLite text to the caller. A friendly hint is added upstream.
            return []
        return [{"url": r[0], "title": r[1], "snippet": r[2]} for r in rows]


cache = Cache()
