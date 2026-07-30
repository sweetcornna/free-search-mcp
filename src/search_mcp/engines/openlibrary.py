"""Open Library — book search over the Internet Archive's catalogue. Keyless.

  GET https://openlibrary.org/search.json?q=<q>&limit=<n>&fields=<...>

`fields` is not an optimisation detail: the default response ships every
edition of every work and runs to megabytes. Asking for the handful of fields
actually rendered keeps a ten-result search in the tens of KB.
"""

from __future__ import annotations

from typing import Any
from urllib.parse import quote_plus

from .base import SearchFilters, SearchResult
from .jsonapi import JsonApiEngine, clip

_ENDPOINT = "https://openlibrary.org/search.json"
_FIELDS = "title,key,author_name,first_publish_year,edition_count,language"


class OpenLibraryEngine(JsonApiEngine):
    """Open Library book search (keyless JSON API)."""

    name = "openlibrary"

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        n = max(1, min(max_results, 100))
        params = [f"q={quote_plus(query)}", f"limit={n}", f"fields={_FIELDS}"]
        if filters and filters.freshness:
            params.append("sort=new")
        return f"{_ENDPOINT}?{'&'.join(params)}"

    def map_results(self, payload: Any) -> list[SearchResult]:
        if not isinstance(payload, dict):
            return []
        docs = payload.get("docs")
        if not isinstance(docs, list):
            return []

        results: list[SearchResult] = []
        for doc in docs:
            if not isinstance(doc, dict):
                continue
            title = doc.get("title")
            key = doc.get("key")
            if not isinstance(title, str) or not title:
                continue
            if not isinstance(key, str) or not key.startswith("/"):
                continue
            year = doc.get("first_publish_year")
            results.append(
                SearchResult(
                    title=clip(title, cap=300),
                    url=f"https://openlibrary.org{key}",
                    snippet=self._snippet(doc),
                    engine=self.name,
                    rank=0,
                    # Only a year is known, so this is not a date the freshness
                    # filter should be allowed to drop results on.
                    published_age=str(year) if isinstance(year, int) else "",
                    published_age_confident=False,
                )
            )
        return results

    def _snippet(self, doc: dict[str, Any]) -> str:
        bits: list[str] = []
        authors = doc.get("author_name")
        if isinstance(authors, list):
            names = [a for a in authors if isinstance(a, str)]
            if names:
                bits.append(", ".join(names[:3]) + (" et al." if len(names) > 3 else ""))
        year = doc.get("first_publish_year")
        if isinstance(year, int):
            bits.append(f"first published {year}")
        editions = doc.get("edition_count")
        if isinstance(editions, int) and editions:
            bits.append(f"{editions} editions")
        return clip(" · ".join(bits))
