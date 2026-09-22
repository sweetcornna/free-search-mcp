"""figshare — dataset search across public figshare repositories. Keyless.

  POST https://api.figshare.com/v2/articles/search

The collection GET endpoint silently ignores search text. The documented POST
search is therefore load-bearing, as is ``item_type=3``: without it the same
index returns papers, posters and other non-dataset records.
"""

from __future__ import annotations

import html
import re
from datetime import UTC, datetime, timedelta
from typing import Any

from .base import SearchFilters, SearchResult
from .jsonapi import JsonApiEngine, clip, iso_date

_ENDPOINT = "https://api.figshare.com/v2/articles/search"
_TAG_RE = re.compile(r"<[^>]+>")
_FRESHNESS_DAYS = {"day": 1, "week": 7, "month": 31, "year": 366}


class FigshareEngine(JsonApiEngine):
    """figshare public-dataset search (keyless JSON API)."""

    name = "figshare"
    description = "figshare: public datasets across institutional and subject repositories."
    categories = frozenset({"dataset", "dataset.repository"})

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        return _ENDPOINT

    def _body(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> dict[str, Any]:
        body: dict[str, Any] = {
            "search_for": query,
            "item_type": 3,
            "limit": max(1, min(max_results, 100)),
        }
        if filters and filters.freshness in _FRESHNESS_DAYS:
            since = datetime.now(tz=UTC).date() - timedelta(
                days=_FRESHNESS_DAYS[filters.freshness]
            )
            body.update(
                published_since=since.isoformat(),
                order="published_date",
                order_direction="desc",
            )
        return body

    async def fetch_results(
        self, query: str, max_results: int, filters: SearchFilters | None
    ) -> list[SearchResult]:
        payload = await self._get_json(
            self.build_url(query, max_results, filters),
            method="POST",
            json_body=self._body(query, max_results, filters),
        )
        if payload is None:
            return []
        return self.map_results(payload)

    def map_results(self, payload: Any) -> list[SearchResult]:
        if not isinstance(payload, list):
            return []

        results: list[SearchResult] = []
        for item in payload:
            if not isinstance(item, dict):
                continue
            raw_title = item.get("title")
            if not isinstance(raw_title, str):
                continue
            title = html.unescape(_TAG_RE.sub(" ", raw_title)).strip()
            url = self._best_url(item)
            if not title or not url:
                continue
            date = iso_date(item.get("published_date"))
            kind = item.get("defined_type_name")
            doi = item.get("doi")
            bits = [kind] if isinstance(kind, str) and kind else []
            if isinstance(doi, str) and doi:
                bits.append(f"DOI: {doi}")
            results.append(
                SearchResult(
                    title=clip(title, cap=300),
                    url=url,
                    snippet=clip(" · ".join(bits)),
                    engine=self.name,
                    rank=0,
                    published_age=date,
                    published_age_confident=bool(date),
                )
            )
        return results

    @staticmethod
    def _best_url(item: dict[str, Any]) -> str:
        public = item.get("url_public_html")
        if isinstance(public, str) and public.startswith("http"):
            return public
        doi = item.get("doi")
        if isinstance(doi, str) and doi:
            return f"https://doi.org/{doi}"
        api = item.get("url")
        if isinstance(api, str) and api.startswith("http"):
            return api
        return ""
