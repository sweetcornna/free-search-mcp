"""OpenAlex — open index of scholarly works. Keyless, ~100k requests/day.

  GET https://api.openalex.org/works?search=<q>&per-page=<n>

Abstracts arrive as an *inverted index* (`{word: [positions]}`) rather than
text, so `_abstract_text` rebuilds the prose. Supplying a contact email
(`SEARCH_MCP_CONTACT_EMAIL`) moves the caller into OpenAlex's faster "polite
pool"; without one the anonymous pool still works.
"""

from __future__ import annotations

from typing import Any
from urllib.parse import quote_plus

from ..config import settings
from .base import SearchFilters, SearchResult
from .jsonapi import JsonApiEngine, clip

_ENDPOINT = "https://api.openalex.org/works"


class OpenAlexEngine(JsonApiEngine):
    """OpenAlex scholarly works search (keyless JSON API)."""

    name = "openalex"
    categories = frozenset({"paper"})

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        n = max(1, min(max_results, 200))
        params = [f"search={quote_plus(query)}", f"per-page={n}"]
        if settings.contact_email:
            params.append(f"mailto={quote_plus(settings.contact_email)}")
        if filters and filters.freshness:
            # OpenAlex supports a from_publication_date filter; sorting newest
            # first is enough to keep the budget full of results that will
            # survive the base class's freshness check.
            params.append("sort=publication_date:desc")
        return f"{_ENDPOINT}?{'&'.join(params)}"

    def map_results(self, payload: Any) -> list[SearchResult]:
        if not isinstance(payload, dict):
            return []
        items = payload.get("results")
        if not isinstance(items, list):
            return []

        results: list[SearchResult] = []
        for item in items:
            if not isinstance(item, dict):
                continue
            title = clip(item.get("display_name") or item.get("title"), cap=300)
            url = self._best_url(item)
            if not title or not url:
                continue
            date = item.get("publication_date")
            date = date if isinstance(date, str) else ""
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
    def _best_url(item: dict[str, Any]) -> str:
        """Landing page if the record has one, else the DOI, else the OpenAlex id."""
        loc = item.get("primary_location")
        if isinstance(loc, dict):
            landing = loc.get("landing_page_url")
            if isinstance(landing, str) and landing:
                return landing
        for key in ("doi", "id"):
            value = item.get(key)
            if isinstance(value, str) and value.startswith("http"):
                return value
        return ""

    def _snippet(self, item: dict[str, Any]) -> str:
        parts: list[str] = []
        venue = item.get("primary_location")
        if isinstance(venue, dict):
            source = venue.get("source")
            if isinstance(source, dict) and source.get("display_name"):
                parts.append(str(source["display_name"]))
        cited = item.get("cited_by_count")
        if isinstance(cited, int) and cited:
            parts.append(f"cited by {cited}")
        abstract = self._abstract_text(item.get("abstract_inverted_index"))
        head = " · ".join(parts)
        if head and abstract:
            return clip(f"{head} — {abstract}")
        return clip(abstract or head)

    @staticmethod
    def _abstract_text(inverted: Any) -> str:
        """Rebuild prose from OpenAlex's `{word: [positions]}` inverted index.

        Bounded on purpose: only the first `SNIPPET_CAP`-worth of positions
        matter for a snippet, but the index itself can hold a whole abstract,
        so the join happens once over a dict that is already in memory.
        """
        if not isinstance(inverted, dict):
            return ""
        positions: dict[int, str] = {}
        for word, idxs in inverted.items():
            if not isinstance(word, str) or not isinstance(idxs, list):
                continue
            for i in idxs:
                if isinstance(i, int):
                    positions[i] = word
        if not positions:
            return ""
        return " ".join(positions[i] for i in sorted(positions))
