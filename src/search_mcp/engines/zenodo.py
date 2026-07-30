"""Zenodo — CERN's open repository for datasets, software and publications.

  GET https://zenodo.org/api/records?q=<q>&size=<n>

Keyless for search. Records point at a landing page rather than the raw file,
because a Zenodo record is usually several files plus the metadata that
explains them.
"""

from __future__ import annotations

from typing import Any
from urllib.parse import quote_plus

from .base import SearchFilters, SearchResult
from .jsonapi import JsonApiEngine, clip

_ENDPOINT = "https://zenodo.org/api/records"


class ZenodoEngine(JsonApiEngine):
    """Zenodo dataset/software/publication search (keyless JSON API)."""

    name = "zenodo"
    categories = frozenset({"dataset"})
    # Zenodo answers 403 to clients presenting a browser TLS/header
    # fingerprint on its API. Identify honestly instead.
    impersonate = None

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        n = max(1, min(max_results, 100))
        params = [f"q={quote_plus(query)}", f"size={n}"]
        # Zenodo's "newest" sort is a bare `sort=mostrecent`.
        params.append("sort=mostrecent" if filters and filters.freshness else "sort=bestmatch")
        return f"{_ENDPOINT}?{'&'.join(params)}"

    def map_results(self, payload: Any) -> list[SearchResult]:
        if not isinstance(payload, dict):
            return []
        hits = payload.get("hits")
        if not isinstance(hits, dict):
            return []
        items = hits.get("hits")
        if not isinstance(items, list):
            return []

        results: list[SearchResult] = []
        for item in items:
            if not isinstance(item, dict):
                continue
            meta = item.get("metadata")
            meta = meta if isinstance(meta, dict) else {}
            title = meta.get("title")
            if not isinstance(title, str) or not title.strip():
                continue
            url = self._landing(item)
            if not url:
                continue
            date = meta.get("publication_date")
            date = date if isinstance(date, str) else ""
            results.append(
                SearchResult(
                    title=clip(title, cap=300),
                    url=url,
                    snippet=self._snippet(item, meta),
                    engine=self.name,
                    rank=0,
                    published_age=date,
                    published_age_confident=bool(date),
                )
            )
        return results

    @staticmethod
    def _landing(item: dict[str, Any]) -> str:
        """Human landing page, falling back to the DOI."""
        links = item.get("links")
        if isinstance(links, dict):
            for key in ("self_html", "html", "doi"):
                value = links.get(key)
                if isinstance(value, str) and value.startswith("http"):
                    return value
        doi = item.get("doi_url") or item.get("doi")
        if isinstance(doi, str) and doi:
            return doi if doi.startswith("http") else f"https://doi.org/{doi}"
        return ""

    def _snippet(self, item: dict[str, Any], meta: dict[str, Any]) -> str:
        bits: list[str] = []
        rtype = meta.get("resource_type")
        if isinstance(rtype, dict) and isinstance(rtype.get("type"), str):
            bits.append(rtype["type"])
        files = item.get("files")
        if isinstance(files, list) and files:
            bits.append(f"{len(files)} file{'s' if len(files) != 1 else ''}")
        creators = meta.get("creators")
        if isinstance(creators, list):
            names = [
                c["name"] for c in creators
                if isinstance(c, dict) and isinstance(c.get("name"), str)
            ]
            if names:
                bits.append(", ".join(names[:3]) + (" et al." if len(names) > 3 else ""))
        description = meta.get("description")
        if isinstance(description, str) and description:
            import re

            bits.append(re.sub(r"<[^>]+>", " ", description))
        return clip(" — ".join(bits))
