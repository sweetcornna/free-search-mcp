"""data.europa.eu — the EU's harvested open-data catalogue. Keyless.

  GET https://data.europa.eu/api/hub/search/search?q=<q>&limit=<n>&page=0

Records are multilingual maps and may point to distributions hosted by the
original publisher. The stable catalogue resource stays the result URL; it
preserves the metadata and the complete distribution list.
"""

from __future__ import annotations

import html
import re
from typing import Any
from urllib.parse import quote_plus

from .base import SearchFilters, SearchResult
from .jsonapi import JsonApiEngine, clip, iso_date

_ENDPOINT = "https://data.europa.eu/api/hub/search/search"
_TAG_RE = re.compile(r"<[^>]+>")


def _language_value(value: Any) -> str:
    if isinstance(value, str):
        return value
    if not isinstance(value, dict):
        return ""
    english = value.get("en")
    if isinstance(english, str) and english:
        return english
    return next((text for text in value.values() if isinstance(text, str) and text), "")


class DataEuropaEngine(JsonApiEngine):
    """data.europa.eu public-sector dataset search (keyless JSON API)."""

    name = "dataeuropa"
    description = "data.europa.eu: EU and member-state open-data catalogues in one index."
    categories = frozenset({"dataset", "dataset.gov"})

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        n = max(1, min(max_results, 100))
        return f"{_ENDPOINT}?q={quote_plus(query)}&limit={n}&page=0"

    def map_results(self, payload: Any) -> list[SearchResult]:
        if not isinstance(payload, dict):
            return []
        result = payload.get("result")
        items = result.get("results") if isinstance(result, dict) else None
        if not isinstance(items, list):
            return []

        results: list[SearchResult] = []
        for item in items:
            if not isinstance(item, dict):
                continue
            title = _language_value(item.get("title")).strip()
            url = item.get("resource")
            if not title or not isinstance(url, str) or not url.startswith("http"):
                continue
            raw_description = _language_value(item.get("description"))
            description = html.unescape(_TAG_RE.sub(" ", raw_description))
            record = item.get("catalog_record")
            record = record if isinstance(record, dict) else {}
            # `catalog.modified` dates the upstream catalogue itself. The
            # per-result `catalog_record.modified` is the relevant freshness
            # signal; confusing the two makes every hit share one portal date.
            date = iso_date(record.get("modified") or record.get("issued"))
            results.append(
                SearchResult(
                    title=clip(title, cap=300),
                    url=url,
                    snippet=clip(description),
                    engine=self.name,
                    rank=0,
                    published_age=date,
                    published_age_confident=bool(date),
                )
            )
        return results
