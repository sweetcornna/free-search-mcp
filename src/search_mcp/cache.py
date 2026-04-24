import json
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
        self._initialized = False

    async def _conn(self) -> aiosqlite.Connection:
        conn = await aiosqlite.connect(self._path)
        if not self._initialized:
            await conn.executescript(_SCHEMA)
            await conn.commit()
            self._initialized = True
        return conn

    async def get_search(
        self, key: str, max_age_seconds: int | None = None,
    ) -> list[dict[str, Any]] | None:
        conn = await self._conn()
        try:
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
        finally:
            await conn.close()

    async def put_search(self, key: str, query: str, engines: list[str], results: list[dict[str, Any]]) -> None:
        conn = await self._conn()
        try:
            await conn.execute(
                "INSERT OR REPLACE INTO search_cache (cache_key, query, engines, results, created) "
                "VALUES (?, ?, ?, ?, ?)",
                (key, query, ",".join(engines), json.dumps(results, ensure_ascii=False), int(time.time())),
            )
            await conn.commit()
        finally:
            await conn.close()

    async def get_page(
        self, url: str, max_age_seconds: int | None = None,
    ) -> dict[str, Any] | None:
        conn = await self._conn()
        try:
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
        finally:
            await conn.close()

    async def put_page(self, url: str, title: str | None, content: str) -> None:
        conn = await self._conn()
        try:
            await conn.execute(
                "INSERT OR REPLACE INTO pages (url, title, content, fetched) VALUES (?, ?, ?, ?)",
                (url, title or "", content, int(time.time())),
            )
            await conn.commit()
        finally:
            await conn.close()

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
            return [{"url": r[0], "title": r[1], "snippet": r[2]} for r in rows]
        finally:
            await conn.close()


cache = Cache()
