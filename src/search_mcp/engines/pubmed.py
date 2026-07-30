"""PubMed — biomedical literature via NCBI E-utilities. Keyless.

Two round trips, because E-utilities separates search from metadata:

  1. esearch.fcgi?db=pubmed&term=<q>&retmode=json   -> list of PMIDs
  2. esummary.fcgi?db=pubmed&id=<ids>&retmode=json  -> title/date/journal

`fetch_results` is overridden to chain them. NCBI rate-limits anonymous
callers to ~3 requests/second and asks that clients identify themselves; the
aggregator's per-engine limiter (30/min) is already well inside that, and
`SEARCH_MCP_CONTACT_EMAIL` supplies the identification when set.
"""

from __future__ import annotations

from typing import Any
from urllib.parse import quote_plus

from ..config import settings
from .base import SearchFilters, SearchResult
from .jsonapi import JsonApiEngine, clip

_BASE = "https://eutils.ncbi.nlm.nih.gov/entrez/eutils"
_ARTICLE = "https://pubmed.ncbi.nlm.nih.gov/{pmid}/"

# Only the freshness windows PubMed's reldate can express usefully.
_RELDATE_DAYS = {"day": 1, "week": 7, "month": 30, "year": 365}


class PubMedEngine(JsonApiEngine):
    """PubMed biomedical literature search (keyless NCBI E-utilities)."""

    name = "pubmed"
    categories = frozenset({"paper"})

    def _identity(self) -> list[str]:
        """NCBI asks callers to declare a tool name and contact address."""
        params = ["tool=free-search-mcp"]
        if settings.contact_email:
            params.append(f"email={quote_plus(settings.contact_email)}")
        return params

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        n = max(1, min(max_results, 100))
        params = [
            "db=pubmed",
            f"term={quote_plus(query)}",
            "retmode=json",
            f"retmax={n}",
            "sort=relevance",
            *self._identity(),
        ]
        if filters and filters.freshness:
            days = _RELDATE_DAYS.get(filters.freshness)
            if days:
                # Server-side date window: unlike the base class's post-filter,
                # this keeps the result budget from filling with old papers
                # that would only be discarded afterwards.
                params.append(f"datetype=pdat&reldate={days}")
        return f"{_BASE}/esearch.fcgi?{'&'.join(params)}"

    async def fetch_results(
        self, query: str, max_results: int, filters: SearchFilters | None
    ) -> list[SearchResult]:
        found = await self._get_json(self.build_url(query, max_results, filters))
        pmids = self._pmids(found)
        if not pmids:
            return []
        summary = await self._get_json(
            f"{_BASE}/esummary.fcgi?db=pubmed&id={','.join(pmids)}"
            f"&retmode=json&{'&'.join(self._identity())}"
        )
        if summary is None:
            return []
        return self.map_results(summary)

    @staticmethod
    def _pmids(payload: Any) -> list[str]:
        if not isinstance(payload, dict):
            return []
        result = payload.get("esearchresult")
        if not isinstance(result, dict):
            return []
        ids = result.get("idlist")
        if not isinstance(ids, list):
            return []
        # Guard the URL we are about to build: these become path/query data.
        return [i for i in ids if isinstance(i, str) and i.isdigit()]

    def map_results(self, payload: Any) -> list[SearchResult]:
        if not isinstance(payload, dict):
            return []
        result = payload.get("result")
        if not isinstance(result, dict):
            return []
        # `uids` preserves relevance order; iterating the dict would not.
        uids = result.get("uids")
        if not isinstance(uids, list):
            return []

        results: list[SearchResult] = []
        for uid in uids:
            item = result.get(uid) if isinstance(uid, str) else None
            if not isinstance(item, dict):
                continue
            title = clip(item.get("title"), cap=300)
            if not title:
                continue
            results.append(
                SearchResult(
                    title=title,
                    url=_ARTICLE.format(pmid=uid),
                    snippet=self._snippet(item),
                    engine=self.name,
                    rank=0,
                    published_age=self._pub_date(item),
                    published_age_confident=bool(self._pub_date(item)),
                )
            )
        return results

    @staticmethod
    def _pub_date(item: dict[str, Any]) -> str:
        """`sortpubdate` is `2026/07/15 00:00`; `pubdate` is `2026 Aug`.

        Only the former is machine-readable, so a record without it reports no
        date rather than a half-parsed one.
        """
        raw = item.get("sortpubdate")
        if not isinstance(raw, str) or not raw:
            return ""
        date_part = raw.split(" ", 1)[0]
        bits = date_part.split("/")
        if len(bits) != 3 or not all(b.isdigit() for b in bits):
            return ""
        return "-".join(bits)

    def _snippet(self, item: dict[str, Any]) -> str:
        parts: list[str] = []
        authors = item.get("authors")
        if isinstance(authors, list):
            names = [
                a["name"]
                for a in authors
                if isinstance(a, dict) and isinstance(a.get("name"), str)
            ]
            if names:
                parts.append(", ".join(names[:3]) + (" et al." if len(names) > 3 else ""))
        for key in ("source", "fulljournalname"):
            venue = item.get(key)
            if isinstance(venue, str) and venue:
                parts.append(venue)
                break
        return clip(" — ".join(parts))
