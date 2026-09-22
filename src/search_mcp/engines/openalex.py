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

# Ask only for the fields `map_results` actually renders. Without `select`
# OpenAlex ships the FULL work record — authorships, concepts, referenced_works,
# counts_by_year — and a 10-result page measured 174 KB against 56 KB with this
# list. Crossref (`select=`) and Open Library (`fields=`) already do the same;
# OpenAlex was the outlier.
_SELECT = ",".join(
    (
        "id",
        "doi",
        "title",
        "display_name",
        "publication_date",
        "cited_by_count",
        "primary_location",
        "abstract_inverted_index",
    )
)


# Comfortably more words than SNIPPET_CAP (400 chars) can hold, so `clip()`
# stays the thing that decides the snippet's length.
_MAX_ABSTRACT_WORDS = 150


class OpenAlexEngine(JsonApiEngine):
    """OpenAlex scholarly works search (keyless JSON API)."""

    name = "openalex"
    description = "OpenAlex: 250M+ scholarly works with abstracts, venues and citation counts."
    categories = frozenset({"paper", "paper.index"})

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        n = max(1, min(max_results, 200))
        params = [f"search={quote_plus(query)}", f"per-page={n}", f"select={_SELECT}"]
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

        Bounded on purpose, and now actually bounded: the result is a SNIPPET,
        so only the leading words can survive `clip()`. Joining the whole
        abstract and then throwing 90% of it away was work done for nothing.
        `_MAX_ABSTRACT_WORDS` is set well above the word count `SNIPPET_CAP`
        chars can hold, so the clip — not this cap — is what decides the text.
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
        ordered = sorted(positions)[:_MAX_ABSTRACT_WORDS]
        return " ".join(positions[i] for i in ordered)
