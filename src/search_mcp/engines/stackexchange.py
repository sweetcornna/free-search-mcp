"""Stack Exchange — Q&A search across Stack Overflow and sibling sites. Keyless.

  GET https://api.stackexchange.com/2.3/search/advanced?q=<q>&site=stackoverflow

Anonymous callers get 300 requests/day per IP; a registered app key raises the
quota (and is optional here). Two response quirks the parser absorbs:

  * `title` is HTML-escaped (`&quot;...&quot;`), so it needs unescaping before
    it reaches a Markdown renderer.
  * `creation_date` is a Unix epoch integer, not an ISO string.
"""

from __future__ import annotations

import html
from datetime import UTC, datetime
from typing import Any
from urllib.parse import quote_plus

from ..keystore import get_secret
from .base import SearchFilters, SearchResult
from .jsonapi import JsonApiEngine, clip

_ENDPOINT = "https://api.stackexchange.com/2.3/search/advanced"


class StackExchangeEngine(JsonApiEngine):
    """Stack Exchange Q&A search (keyless, optional app key)."""

    name = "stackexchange"
    categories = frozenset({"forum"})

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        n = max(1, min(max_results, 100))
        params = [
            f"q={quote_plus(query)}",
            "site=stackoverflow",
            "order=desc",
            f"pagesize={n}",
            # The API gzips by default; curl_cffi handles the decoding.
            "sort=relevance",
        ]
        if filters and filters.freshness:
            # `activity` surfaces threads that are still being answered, which
            # is what "recent" means for a Q&A site.
            params[-1] = "sort=activity"
        key = get_secret("stackexchange_key")
        if key:
            params.append(f"key={quote_plus(key)}")
        return f"{_ENDPOINT}?{'&'.join(params)}"

    def map_results(self, payload: Any) -> list[SearchResult]:
        if not isinstance(payload, dict):
            return []
        items = payload.get("items")
        if not isinstance(items, list):
            return []

        results: list[SearchResult] = []
        for item in items:
            if not isinstance(item, dict):
                continue
            raw_title = item.get("title")
            url = item.get("link")
            if not isinstance(raw_title, str) or not isinstance(url, str) or not url:
                continue
            date = self._epoch_date(item.get("creation_date"))
            results.append(
                SearchResult(
                    # API titles arrive HTML-escaped; leaving them escaped puts
                    # literal `&quot;` in the rendered result list.
                    title=clip(html.unescape(raw_title), cap=300),
                    url=url,
                    snippet=self._snippet(item),
                    engine=self.name,
                    rank=0,
                    published_age=date,
                    published_age_confident=bool(date),
                )
            )
        return results

    @staticmethod
    def _epoch_date(value: Any) -> str:
        """Unix epoch -> `YYYY-MM-DD`, or "" for anything unparseable."""
        if not isinstance(value, int) or value <= 0:
            return ""
        try:
            return datetime.fromtimestamp(value, tz=UTC).strftime("%Y-%m-%d")
        except (OverflowError, OSError, ValueError):
            return ""

    def _snippet(self, item: dict[str, Any]) -> str:
        bits: list[str] = []
        # Whether the question has an accepted answer is the single most useful
        # signal for "will this actually help me".
        if item.get("is_answered"):
            bits.append("answered")
        for key, label in (("score", "score"), ("answer_count", "answers")):
            value = item.get(key)
            if isinstance(value, int):
                bits.append(f"{label} {value}")
        tags = item.get("tags")
        if isinstance(tags, list):
            names = [t for t in tags if isinstance(t, str)]
            if names:
                bits.append(" ".join(f"[{t}]" for t in names[:5]))
        return clip(" · ".join(bits))
