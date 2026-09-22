"""巨潮资讯 (cninfo.com.cn) — official filings for Chinese-listed companies. Keyless.

  POST https://www.cninfo.com.cn/new/hisAnnouncement/query   (form-encoded)

cninfo is the CSRC-designated disclosure portal: every A-share, STAR Market,
ChiNext, NEEQ and Hong Kong dual-listed announcement is published here first.
It is the Chinese counterpart of `sec_edgar`, and the same argument applies —
a web engine can find commentary about a 业绩预告, only this finds the filing.

Two implementation notes:

  * The endpoint is a FORM post that answers with JSON, so this is the one
    engine that uses `JsonApiEngine`'s `form_body` path.
  * `hisAnnouncement/query`, not `fulltextSearch/full`. Measured on 人工智能:
    the announcement query returned 236 hits, the "full text" endpoint zero.
    The latter is erratic — 2,707 hits for 芯片 and none for a comparably
    common term — so it is not used.

Titles come back with `<em>` highlight markup around the matched terms, which
has to go before the text reaches a Markdown renderer.
"""

from __future__ import annotations

import re
from datetime import datetime, timedelta, timezone
from typing import Any

from .base import SearchFilters, SearchResult
from .jsonapi import JsonApiEngine, clip

# cninfo is a Shenzhen/Shanghai exchange service and stamps every timestamp in
# Beijing time. Its epochs are exact midnights there, so rendering them as UTC
# lands at 16:00 the PREVIOUS day and dates every announcement one day early —
# visible in the payload itself, where `adjunctUrl` carries the filing date as
# a path segment that disagreed with `announcementTime` for every hit.
# A fixed offset, not a zoneinfo key: China has observed no DST since 1991, and
# this keeps the engine free of a tzdata dependency.
_CST = timezone(timedelta(hours=8), "CST")

# https on both hosts (verified); plaintext would ship filing URLs the model
# then hands to `fetch` over an unencrypted hop for no reason.
_ENDPOINT = "https://www.cninfo.com.cn/new/hisAnnouncement/query"
# Announcement PDFs are served from the static host, keyed by `adjunctUrl`.
_STATIC = "https://static.cninfo.com.cn/"

_TAG_RE = re.compile(r"<[^>]+>")
_FRESHNESS_DAYS = {"day": 1, "week": 7, "month": 31, "year": 366}


class CninfoEngine(JsonApiEngine):
    """巨潮资讯 A-share / HK announcement search (keyless form-POST JSON API)."""

    name = "cninfo"
    description = "巨潮资讯: official A-share, STAR, ChiNext and HK filings and announcements."
    categories = frozenset({"finance", "finance.filings"})

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        # The query travels in the POST body; the URL is constant. Kept as a
        # real method because `Engine.build_url` is abstract and the aggregator's
        # cache key is built from the request the engine would make.
        return _ENDPOINT

    def _form(self, query: str, max_results: int, filters: SearchFilters | None) -> dict:
        n = max(1, min(max_results, 30))
        form = {
            "pageNum": 1,
            "pageSize": n,
            "column": "szse",
            "tabName": "fulltext",
            "searchkey": query,
            "sortName": "",
            "sortType": "",
            "isHLtitle": "true",
        }
        if filters and filters.freshness:
            days = _FRESHNESS_DAYS.get(filters.freshness)
            if days:
                # Beijing date, matching the dates the server compares against:
                # for the 16 hours a day when UTC is a day behind, a UTC "today"
                # end-bound silently excludes everything filed today.
                end = datetime.now(tz=_CST).date()
                start = end - timedelta(days=days)
                # cninfo's window is a single `start~end` string.
                form["seDate"] = f"{start.isoformat()}~{end.isoformat()}"
        return form

    async def fetch_results(
        self, query: str, max_results: int, filters: SearchFilters | None
    ) -> list[SearchResult]:
        payload = await self._get_json(
            self.build_url(query, max_results, filters),
            method="POST",
            form_body=self._form(query, max_results, filters),
        )
        if payload is None:
            return []
        return self.map_results(payload)

    def map_results(self, payload: Any) -> list[SearchResult]:
        if not isinstance(payload, dict):
            return []
        items = payload.get("announcements")
        if not isinstance(items, list):
            return []

        results: list[SearchResult] = []
        for item in items:
            if not isinstance(item, dict):
                continue
            raw_title = item.get("announcementTitle")
            if not isinstance(raw_title, str) or not raw_title.strip():
                continue
            url = self._document_url(item.get("adjunctUrl"))
            if not url:
                continue
            title = _TAG_RE.sub("", raw_title).strip()
            code = item.get("secCode")
            sec_name = item.get("secName")
            if isinstance(code, str) and isinstance(sec_name, str) and code and sec_name:
                # Prefix the issuer so a bare announcement title ("关于…的公告")
                # is attributable without opening the PDF.
                title = f"{sec_name} ({code}) {title}"
            date = self._announced(item.get("announcementTime"))
            results.append(
                SearchResult(
                    title=clip(title, cap=300),
                    url=url,
                    snippet=self._snippet(item, date),
                    engine=self.name,
                    rank=0,
                    published_age=date,
                    # cninfo stamps the disclosure time; freshness may drop on it.
                    published_age_confident=bool(date),
                )
            )
        return results

    @staticmethod
    def _document_url(adjunct: Any) -> str:
        if not isinstance(adjunct, str) or not adjunct.strip():
            return ""
        return _STATIC + adjunct.lstrip("/")

    @staticmethod
    def _announced(value: Any) -> str:
        """cninfo stamps epoch MILLIseconds in Beijing time; emit `YYYY-MM-DD`."""
        if not isinstance(value, (int, float)) or isinstance(value, bool):
            return ""
        try:
            return datetime.fromtimestamp(value / 1000, tz=_CST).strftime(
                "%Y-%m-%d"
            )
        except (OverflowError, OSError, ValueError):
            return ""

    def _snippet(self, item: dict[str, Any], date: str) -> str:
        bits: list[str] = []
        if date:
            bits.append(f"披露 {date}")
        board = item.get("pageColumn")
        if isinstance(board, str) and board:
            bits.append(board)
        kind = item.get("adjunctType")
        size = item.get("adjunctSize")
        if isinstance(kind, str) and kind:
            bits.append(f"{kind} {size}KB" if isinstance(size, int) else kind)
        return clip(" · ".join(bits))
