"""Dryad — curated research datasets with DOI landing pages. Keyless.

  GET https://datadryad.org/api/v2/search?q=<q>&per_page=<n>&page=1

Dryad is dataset-only rather than a mixed publication repository. Search
records include an abstract, authors, a DOI and a publication date, so one
request is enough to produce a useful result card.
"""

from __future__ import annotations

import html
import re
from datetime import UTC, datetime, timedelta
from typing import Any
from urllib.parse import quote_plus

from .base import SearchFilters, SearchResult
from .jsonapi import JsonApiEngine, clip, iso_date

_ENDPOINT = "https://datadryad.org/api/v2/search"
_TAG_RE = re.compile(r"<[^>]+>")
_FRESHNESS_DAYS = {"day": 1, "week": 7, "month": 31, "year": 366}


class DryadEngine(JsonApiEngine):
    """Dryad research-dataset search (keyless JSON API v2)."""

    name = "dryad"
    description = "Dryad: curated research datasets with abstracts, authors and DOI landing pages."
    categories = frozenset({"dataset", "dataset.repository"})

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        n = max(1, min(max_results, 100))
        params = [f"q={quote_plus(query)}", f"per_page={n}", "page=1"]
        if filters and filters.freshness in _FRESHNESS_DAYS:
            since = datetime.now(tz=UTC).date() - timedelta(
                days=_FRESHNESS_DAYS[filters.freshness]
            )
            params.append(f"publishedSince={since.isoformat()}")
        return f"{_ENDPOINT}?{'&'.join(params)}"

    def map_results(self, payload: Any) -> list[SearchResult]:
        if not isinstance(payload, dict):
            return []
        embedded = payload.get("_embedded")
        items = embedded.get("stash:datasets") if isinstance(embedded, dict) else None
        if not isinstance(items, list):
            return []

        results: list[SearchResult] = []
        for item in items:
            if not isinstance(item, dict):
                continue
            title = item.get("title")
            identifier = item.get("identifier")
            if not isinstance(title, str) or not title.strip():
                continue
            if not isinstance(identifier, str) or not identifier.startswith("doi:"):
                continue
            doi = identifier.removeprefix("doi:").strip()
            if not doi:
                continue
            date = iso_date(item.get("publicationDate"))
            results.append(
                SearchResult(
                    title=clip(title, cap=300),
                    url=f"https://doi.org/{doi}",
                    snippet=self._snippet(item),
                    engine=self.name,
                    rank=0,
                    published_age=date,
                    published_age_confident=bool(date),
                )
            )
        return results

    @staticmethod
    def _snippet(item: dict[str, Any]) -> str:
        bits: list[str] = []
        authors = item.get("authors")
        if isinstance(authors, list):
            names = []
            for author in authors:
                if not isinstance(author, dict):
                    continue
                first, last = author.get("firstName"), author.get("lastName")
                name = " ".join(x for x in (first, last) if isinstance(x, str) and x)
                if name:
                    names.append(name)
            if names:
                bits.append(", ".join(names[:3]) + (" et al." if len(names) > 3 else ""))
        abstract = item.get("abstract")
        if isinstance(abstract, str) and abstract:
            bits.append(html.unescape(_TAG_RE.sub(" ", abstract)))
        return clip(" — ".join(bits))
