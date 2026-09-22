"""Harvard Dataverse — public research-dataset catalogue. Keyless.

  GET https://dataverse.harvard.edu/api/search?q=<q>&type=dataset

The public search endpoint returns dataset records with DOI URLs, descriptions
and publication timestamps. Restricting ``type=dataset`` avoids files and
Dataverse collections sharing the same search index.
"""

from __future__ import annotations

from datetime import UTC, datetime, timedelta
from typing import Any
from urllib.parse import quote_plus

from .base import SearchFilters, SearchResult
from .jsonapi import JsonApiEngine, clip, iso_date

_ENDPOINT = "https://dataverse.harvard.edu/api/search"
_FRESHNESS_DAYS = {"day": 1, "week": 7, "month": 31, "year": 366}


class DataverseEngine(JsonApiEngine):
    """Harvard Dataverse dataset search (keyless JSON API)."""

    name = "dataverse"
    description = "Harvard Dataverse: public research datasets with descriptions and DOI links."
    categories = frozenset({"dataset", "dataset.repository"})

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        n = max(1, min(max_results, 100))
        params = [
            f"q={quote_plus(query)}",
            "type=dataset",
            f"per_page={n}",
            "start=0",
        ]
        if filters and filters.freshness in _FRESHNESS_DAYS:
            since = datetime.now(tz=UTC).date() - timedelta(
                days=_FRESHNESS_DAYS[filters.freshness]
            )
            date_range = quote_plus(f"publicationDate:[{since.isoformat()} TO *]")
            params.extend((f"fq={date_range}", "sort=date", "order=desc"))
        return f"{_ENDPOINT}?{'&'.join(params)}"

    def map_results(self, payload: Any) -> list[SearchResult]:
        if not isinstance(payload, dict):
            return []
        data = payload.get("data")
        items = data.get("items") if isinstance(data, dict) else None
        if not isinstance(items, list):
            return []

        results: list[SearchResult] = []
        for item in items:
            if not isinstance(item, dict):
                continue
            title, url = item.get("name"), item.get("url")
            if not isinstance(title, str) or not title.strip():
                continue
            if not isinstance(url, str) or not url.startswith("http"):
                continue
            date = iso_date(item.get("published_at"))
            results.append(
                SearchResult(
                    title=clip(title, cap=300),
                    url=url,
                    snippet=clip(item.get("description")),
                    engine=self.name,
                    rank=0,
                    published_age=date,
                    published_age_confident=bool(date),
                )
            )
        return results
