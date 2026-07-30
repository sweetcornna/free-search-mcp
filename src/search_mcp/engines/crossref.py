"""Crossref — DOI registration metadata for the scholarly literature. Keyless.

  GET https://api.crossref.org/works?query=<q>&rows=<n>&filter=type:journal-article

The type filter is load-bearing, not tidiness: unfiltered, Crossref happily
ranks individual *figures* and other sub-components above the papers that
contain them, because each one has its own DOI. Restricting to article types
is the difference between a useful result list and a list of figure captions.

Crossref records rarely carry abstracts, so the snippet is assembled from
authors, container title, and publisher instead.
"""

from __future__ import annotations

import re
from typing import Any
from urllib.parse import quote_plus

from ..config import settings
from .base import SearchFilters, SearchResult
from .jsonapi import JsonApiEngine, clip

_ENDPOINT = "https://api.crossref.org/works"

# Sub-component DOIs (figures, tables, chapters' sub-parts) crowd out papers.
_TYPES = "journal-article,proceedings-article,posted-content,book-chapter"

# The handful of fields actually used, so Crossref doesn't ship the full record.
_SELECT = "title,URL,abstract,issued,type,author,container-title,publisher"

# Crossref abstracts, when present, are JATS XML fragments.
_TAG_RE = re.compile(r"<[^>]+>")


class CrossrefEngine(JsonApiEngine):
    """Crossref works search (keyless JSON API)."""

    name = "crossref"
    categories = frozenset({"paper"})

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        n = max(1, min(max_results, 100))
        params = [
            f"query={quote_plus(query)}",
            f"rows={n}",
            f"filter=type:{_TYPES.replace(',', ',type:')}",
            f"select={quote_plus(_SELECT)}",
        ]
        if settings.contact_email:
            params.append(f"mailto={quote_plus(settings.contact_email)}")
        if filters and filters.freshness:
            params.append("sort=published&order=desc")
        return f"{_ENDPOINT}?{'&'.join(params)}"

    def map_results(self, payload: Any) -> list[SearchResult]:
        if not isinstance(payload, dict):
            return []
        message = payload.get("message")
        if not isinstance(message, dict):
            return []
        items = message.get("items")
        if not isinstance(items, list):
            return []

        results: list[SearchResult] = []
        for item in items:
            if not isinstance(item, dict):
                continue
            title = clip(self._first(item.get("title")), cap=300)
            url = item.get("URL")
            if not title or not isinstance(url, str) or not url:
                continue
            date = self._issued_date(item.get("issued"))
            results.append(
                SearchResult(
                    title=title,
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
    def _first(value: Any) -> str:
        """Crossref returns `title` and `container-title` as lists of strings."""
        if isinstance(value, list):
            return next((v for v in value if isinstance(v, str) and v), "")
        return value if isinstance(value, str) else ""

    @staticmethod
    def _issued_date(issued: Any) -> str:
        """`{"date-parts": [[2024, 8, 3]]}` -> `2024-08-03`.

        Partial dates are common (`[[2024]]`), and missing ones arrive as
        `[[None]]` rather than an absent key, so every part is checked.
        """
        if not isinstance(issued, dict):
            return ""
        parts = issued.get("date-parts")
        if not isinstance(parts, list) or not parts:
            return ""
        first = parts[0]
        if not isinstance(first, list) or not first:
            return ""
        nums = [p for p in first if isinstance(p, int)]
        if not nums:
            return ""
        year = nums[0]
        month = nums[1] if len(nums) > 1 else 1
        day = nums[2] if len(nums) > 2 else 1
        return f"{year:04d}-{month:02d}-{day:02d}"

    def _snippet(self, item: dict[str, Any]) -> str:
        parts: list[str] = []
        authors = item.get("author")
        if isinstance(authors, list):
            names = [
                " ".join(x for x in (a.get("given"), a.get("family")) if isinstance(x, str))
                for a in authors
                if isinstance(a, dict)
            ]
            names = [n for n in names if n.strip()]
            if names:
                parts.append(", ".join(names[:3]) + (" et al." if len(names) > 3 else ""))
        venue = self._first(item.get("container-title")) or item.get("publisher")
        if isinstance(venue, str) and venue:
            parts.append(venue)
        abstract = item.get("abstract")
        if isinstance(abstract, str) and abstract:
            parts.append(_TAG_RE.sub(" ", abstract))
        return clip(" — ".join(parts))
