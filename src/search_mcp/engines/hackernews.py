"""Hacker News — discussion search via the Algolia index. Keyless, generous quota.

  GET https://hn.algolia.com/api/v1/search?query=<q>&hitsPerPage=<n>

Link posts carry the submitted `url`; Ask HN / Show HN text posts carry none,
so those fall back to the HN thread itself. Either way the discussion is worth
reaching, which is the point of indexing HN at all.
"""

from __future__ import annotations

from typing import Any
from urllib.parse import quote_plus

from .base import SearchFilters, SearchResult
from .jsonapi import JsonApiEngine, clip, iso_date

_SEARCH = "https://hn.algolia.com/api/v1/search"
# `search_by_date` is a separate endpoint rather than a sort parameter.
_SEARCH_RECENT = "https://hn.algolia.com/api/v1/search_by_date"
_THREAD = "https://news.ycombinator.com/item?id={object_id}"


class HackerNewsEngine(JsonApiEngine):
    """Hacker News discussion search (keyless Algolia API)."""

    name = "hackernews"
    categories = frozenset({"forum"})

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        n = max(1, min(max_results, 100))
        endpoint = _SEARCH
        if filters and filters.freshness:
            endpoint = _SEARCH_RECENT
        return (
            f"{endpoint}?query={quote_plus(query)}"
            f"&hitsPerPage={n}&tags=story"
        )

    def map_results(self, payload: Any) -> list[SearchResult]:
        if not isinstance(payload, dict):
            return []
        hits = payload.get("hits")
        if not isinstance(hits, list):
            return []

        results: list[SearchResult] = []
        for hit in hits:
            if not isinstance(hit, dict):
                continue
            title = hit.get("title") or hit.get("story_title")
            if not isinstance(title, str) or not title:
                continue
            url = self._url(hit)
            if not url:
                continue
            created = iso_date(hit.get("created_at"))
            results.append(
                SearchResult(
                    title=clip(title, cap=300),
                    url=url,
                    snippet=self._snippet(hit),
                    engine=self.name,
                    rank=0,
                    published_age=created,
                    published_age_confident=bool(created),
                )
            )
        return results

    @staticmethod
    def _url(hit: dict[str, Any]) -> str:
        """Submitted link, or the HN thread for text posts (Ask HN / Show HN)."""
        url = hit.get("url")
        if isinstance(url, str) and url.startswith("http"):
            return url
        object_id = hit.get("objectID")
        if isinstance(object_id, str) and object_id:
            return _THREAD.format(object_id=object_id)
        return ""

    def _snippet(self, hit: dict[str, Any]) -> str:
        bits: list[str] = []
        for key, label in (("points", "points"), ("num_comments", "comments")):
            value = hit.get(key)
            if isinstance(value, int) and value:
                bits.append(f"{value} {label}")
        author = hit.get("author")
        if isinstance(author, str) and author:
            bits.append(f"by {author}")
        # When the result already points at the submitted article, also offer
        # the discussion thread — that is usually the reason to search HN.
        object_id = hit.get("objectID")
        url = hit.get("url")
        if isinstance(object_id, str) and object_id and isinstance(url, str) and url:
            bits.append(f"discussion: {_THREAD.format(object_id=object_id)}")
        return clip(" · ".join(bits))
