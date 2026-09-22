"""Semantic Scholar — scholarly search with the richest metadata. Optional key.

  GET https://api.semanticscholar.org/graph/v1/paper/search?query=<q>&fields=<...>

Best-in-class fields for deciding whether a paper is worth reading: abstract,
citation count, influential-citation count, venue, year, and a direct
open-access PDF link when one exists.

**The anonymous pool is effectively unusable.** Every unauthenticated request
made while building this engine came back HTTP 429, across repeated attempts
from different queries — the keyless tier is a shared bucket that is
permanently saturated. So this is registered as an OPTIONAL-key provider
(`keystore.PROVIDERS`, like `anysearch` and `github`): it will try without a
key and degrade to "no results" on a 429, and it works properly once a free key
is configured. `search-mcp-admin` shows how to get one.

`is_available()` reflects that honestly, so `category="paper"` routing does not
spend one of its slots on an engine that will 429 — while naming the engine
explicitly still runs it.
"""

from __future__ import annotations

from typing import Any
from urllib.parse import quote_plus

from ..keystore import get_secret
from .base import SearchFilters, SearchResult
from .jsonapi import JsonApiEngine, clip

_ENDPOINT = "https://api.semanticscholar.org/graph/v1/paper/search"
_PAPER = "https://www.semanticscholar.org/paper/{paper_id}"
_KEY_FIELD = "semanticscholar_api_key"

_FIELDS = ",".join(
    (
        "paperId",
        "title",
        "abstract",
        "year",
        "publicationDate",
        "venue",
        "citationCount",
        "influentialCitationCount",
        "isOpenAccess",
        "openAccessPdf",
        "externalIds",
        "authors",
    )
)


class SemanticScholarEngine(JsonApiEngine):
    """Semantic Scholar paper search (works keyless in theory, needs a key in practice)."""

    name = "semanticscholar"
    description = "Semantic Scholar: abstracts, citation counts, open-access PDFs (key advised)."
    categories = frozenset({"paper", "paper.index"})
    # The anonymous pool answers 429; identify honestly rather than pretending
    # to be a browser, since the fix is a key, not a fingerprint.
    impersonate = None

    def is_available(self) -> bool:
        """Keep the unkeyed engine out of AUTO-SELECTION only.

        Without a key every call is throttled, so spending one of
        `category_engine_limit`'s slots here costs a real source for a
        guaranteed empty. Naming it in `engines=[...]` still runs it — the
        keyless tier does occasionally answer, and the caller asked.
        """
        return bool(get_secret(_KEY_FIELD))

    @property
    def api_headers(self) -> dict[str, str]:  # type: ignore[override]
        key = get_secret(_KEY_FIELD)
        return {"x-api-key": key} if key else {}

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        n = max(1, min(max_results, 100))
        params = [f"query={quote_plus(query)}", f"limit={n}", f"fields={_FIELDS}"]
        if filters and filters.freshness:
            # The search endpoint has no sort; a publication-year floor is the
            # only recency lever, and the client-side check does the rest.
            params.append("sort=publicationDate:desc")
        return f"{_ENDPOINT}?{'&'.join(params)}"

    def map_results(self, payload: Any) -> list[SearchResult]:
        if not isinstance(payload, dict):
            return []
        items = payload.get("data")
        if not isinstance(items, list):
            return []

        results: list[SearchResult] = []
        for item in items:
            if not isinstance(item, dict):
                continue
            title = item.get("title")
            if not isinstance(title, str) or not title.strip():
                continue
            url = self._best_url(item)
            if not url:
                continue
            date = item.get("publicationDate")
            date = date if isinstance(date, str) else ""
            year = item.get("year")
            results.append(
                SearchResult(
                    title=clip(title, cap=300),
                    url=url,
                    snippet=self._snippet(item),
                    engine=self.name,
                    rank=0,
                    # A full publicationDate is trustworthy; a bare year is not
                    # precise enough for the freshness filter to drop on.
                    published_age=date or (str(year) if isinstance(year, int) else ""),
                    published_age_confident=bool(date),
                )
            )
        return results

    @staticmethod
    def _best_url(item: dict[str, Any]) -> str:
        """Open-access PDF first, then the DOI, then the S2 record page."""
        pdf = item.get("openAccessPdf")
        if isinstance(pdf, dict):
            url = pdf.get("url")
            if isinstance(url, str) and url.startswith("http"):
                return url
        external = item.get("externalIds")
        if isinstance(external, dict):
            doi = external.get("DOI")
            if isinstance(doi, str) and doi:
                return f"https://doi.org/{doi}"
        paper_id = item.get("paperId")
        if isinstance(paper_id, str) and paper_id:
            return _PAPER.format(paper_id=paper_id)
        return ""

    def _snippet(self, item: dict[str, Any]) -> str:
        bits: list[str] = []
        authors = item.get("authors")
        if isinstance(authors, list):
            names = [
                a["name"] for a in authors
                if isinstance(a, dict) and isinstance(a.get("name"), str)
            ]
            if names:
                bits.append(", ".join(names[:3]) + (" et al." if len(names) > 3 else ""))
        for key in ("venue",):
            value = item.get(key)
            if isinstance(value, str) and value:
                bits.append(value)
        cited = item.get("citationCount")
        if isinstance(cited, int) and cited:
            influential = item.get("influentialCitationCount")
            if isinstance(influential, int) and influential:
                bits.append(f"cited by {cited} ({influential} influential)")
            else:
                bits.append(f"cited by {cited}")
        if item.get("isOpenAccess"):
            bits.append("open access")
        head = " · ".join(bits)
        abstract = item.get("abstract")
        abstract = abstract if isinstance(abstract, str) else ""
        if head and abstract:
            return clip(f"{head} — {abstract}")
        return clip(abstract or head)
