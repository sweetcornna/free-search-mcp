"""GDELT — global news monitoring across languages and countries. Keyless.

  GET https://api.gdeltproject.org/api/v2/doc/doc?query=<q>&mode=ArtList&format=json

GDELT indexes worldwide news in 100+ languages, which is where it complements
`googlenews`: the same query returns outlets Google News never surfaces.

It enforces "one request every 5 seconds" and says so in the body of its 429
response, so the engine declares its own 12/min limit rather than inheriting
the global 30/min default and walking into the wall.
"""

from __future__ import annotations

from typing import Any
from urllib.parse import quote_plus

from .base import SearchFilters, SearchResult
from .jsonapi import JsonApiEngine, clip

_ENDPOINT = "https://api.gdeltproject.org/api/v2/doc/doc"

# GDELT's own timespan tokens.
_TIMESPAN = {"day": "1d", "week": "1w", "month": "1m", "year": "12m"}


class GdeltEngine(JsonApiEngine):
    """GDELT global news search (keyless JSON API)."""

    name = "gdelt"
    categories = frozenset({"news"})
    # GDELT's 429 body says "limit requests to one every 5 seconds" (12/min).
    # We go to one per 10s: observed behavior punishes bursts with a cooldown
    # well past the stated window, and being gentler than a free public API
    # asks costs us nothing here — this engine is opt-in and news-routed, not
    # part of the default pool.
    rate_limit_per_minute = 6
    # Never make a search WAIT on this bucket. A second news query inside the
    # window skips GDELT (reported in diagnostics) instead of adding ~10s to
    # every other engine's results.
    rate_limit_max_wait = 1.0

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        n = max(1, min(max_results, 250))
        params = [
            f"query={quote_plus(query)}",
            "mode=ArtList",
            "format=json",
            f"maxrecords={n}",
            "sort=hybridrel",
        ]
        if filters and filters.freshness:
            span = _TIMESPAN.get(filters.freshness)
            if span:
                params.append(f"timespan={span}")
        return f"{_ENDPOINT}?{'&'.join(params)}"

    def map_results(self, payload: Any) -> list[SearchResult]:
        if not isinstance(payload, dict):
            return []
        articles = payload.get("articles")
        if not isinstance(articles, list):
            return []

        results: list[SearchResult] = []
        for art in articles:
            if not isinstance(art, dict):
                continue
            title = art.get("title")
            url = art.get("url")
            if not isinstance(title, str) or not isinstance(url, str) or not url:
                continue
            date = self._seendate(art.get("seendate"))
            results.append(
                SearchResult(
                    title=clip(title, cap=300),
                    url=url,
                    snippet=self._snippet(art),
                    engine=self.name,
                    rank=0,
                    published_age=date,
                    published_age_confident=bool(date),
                )
            )
        return results

    @staticmethod
    def _seendate(value: Any) -> str:
        """GDELT stamps `20260730T130338Z`; emit `2026-07-30`."""
        if not isinstance(value, str) or len(value) < 8:
            return ""
        head = value[:8]
        if not head.isdigit():
            return ""
        return f"{head[0:4]}-{head[4:6]}-{head[6:8]}"

    def _snippet(self, art: dict[str, Any]) -> str:
        bits: list[str] = []
        for key in ("domain", "sourcecountry", "language"):
            value = art.get(key)
            if isinstance(value, str) and value:
                bits.append(value)
        return clip(" · ".join(bits))
