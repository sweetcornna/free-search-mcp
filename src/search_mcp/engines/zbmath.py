"""zbMATH Open — mathematics literature and reviews. Keyless.

  GET https://api.zbmath.org/v1/document/_search?search_string=<q>

Unlike the broad DOI indexes, zbMATH is curated for mathematics and exposes
editorial reviews. Its ``datestamp`` is a sentinel in real records, so only the
publication year is surfaced.
"""

from __future__ import annotations

from typing import Any
from urllib.parse import quote_plus

import certifi

from .base import SearchFilters, SearchResult
from .jsonapi import JsonApiEngine, clip

_ENDPOINT = "https://api.zbmath.org/v1/document/_search"


class ZbMathEngine(JsonApiEngine):
    """zbMATH Open mathematics-literature search (keyless JSON API)."""

    name = "zbmath"
    description = "zbMATH Open: curated mathematics literature with reviews and classification."
    categories = frozenset({"paper", "paper.math"})
    # The HTTPS API works with an honest API client but intermittently 502s
    # during curl_cffi's browser-profile handshake. Avoid presenting a browser
    # fingerprint to a machine endpoint that neither needs nor rewards one.
    impersonate = None
    # curl_cffi's bundled CA store lacks the public root anchoring zbMATH's
    # current chain, while certifi carries it and keeps the broader trust store
    # current as roots rotate or are distrusted. Verification stays enabled.
    verify = certifi.where()

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        n = max(1, min(max_results, 100))
        # zbMATH can constrain a calendar year in its query language, but the
        # package's rolling day/week/month windows cannot be represented
        # honestly. The year returned by the mapper remains display-only.
        return (
            f"{_ENDPOINT}?search_string={quote_plus(query)}"
            f"&results_per_page={n}&page=0"
        )

    def map_results(self, payload: Any) -> list[SearchResult]:
        if not isinstance(payload, dict):
            return []
        items = payload.get("result")
        if not isinstance(items, list):
            return []

        results: list[SearchResult] = []
        for item in items:
            if not isinstance(item, dict):
                continue
            title_block = item.get("title")
            title = title_block.get("title") if isinstance(title_block, dict) else None
            url = item.get("zbmath_url")
            if not isinstance(title, str) or not title.strip():
                continue
            if not isinstance(url, str) or not url.startswith("http"):
                continue
            year = item.get("year")
            published = str(year) if isinstance(year, (str, int)) and year else ""
            results.append(
                SearchResult(
                    title=clip(title, cap=300),
                    url=url,
                    snippet=self._snippet(item),
                    engine=self.name,
                    rank=0,
                    # Real records carry datestamp=0001-01-01T00:00:00Z. It is
                    # an indexing sentinel, not a publication date; the year is
                    # useful for display but too imprecise for freshness drops.
                    published_age=published,
                    published_age_confident=False,
                )
            )
        return results

    @staticmethod
    def _snippet(item: dict[str, Any]) -> str:
        contributions = item.get("editorial_contributions")
        if not isinstance(contributions, list):
            return ""
        for contribution in contributions:
            text = contribution.get("text") if isinstance(contribution, dict) else None
            if isinstance(text, str) and text:
                return clip(text)
        return ""
