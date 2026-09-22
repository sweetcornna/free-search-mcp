"""DBLP — the computer-science bibliography. Keyless.

  GET https://dblp.org/search/publ/api?q=<q>&format=json&h=<n>

Curated rather than crawled, so for CS it is precise where a general index is
noisy: authoritative author names, the real venue (SIGIR, NeurIPS, …), the year,
and a DOI. It indexes bibliographic records, not full text — the payoff is
finding exactly the right paper, not reading it here.

Scope is genuinely computer science only, which is why it is `paper.cs` and not
a general `paper` fallback.

One JSON-from-XML wrinkle: a single-author paper returns `authors.author` as an
OBJECT, a multi-author paper as a LIST. Both are handled.
"""

from __future__ import annotations

from typing import Any
from urllib.parse import quote_plus

from .base import SearchFilters, SearchResult
from .jsonapi import JsonApiEngine, clip

_ENDPOINT = "https://dblp.org/search/publ/api"


class DblpEngine(JsonApiEngine):
    """DBLP computer-science publication search (keyless JSON API)."""

    name = "dblp"
    description = "DBLP: curated computer-science bibliography: exact venues, authors and DOIs."
    categories = frozenset({"paper", "paper.cs"})
    # DBLP punishes bursts: two queries in quick succession during development
    # came back throttled (an empty body, not an error), which the never-raise
    # rule turns into "no results". Stay gentle, and never let a search WAIT on
    # this bucket — a skipped engine is now reported, a slow one is not.
    rate_limit_per_minute = 20
    rate_limit_max_wait = 3.0

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        n = max(1, min(max_results, 100))
        # DBLP has no freshness parameter and no sort; `f`/`h` are the only
        # paging controls. Recency is left to the client-side filter.
        return f"{_ENDPOINT}?q={quote_plus(query)}&format=json&h={n}"

    def map_results(self, payload: Any) -> list[SearchResult]:
        if not isinstance(payload, dict):
            return []
        result = payload.get("result")
        hits = result.get("hits") if isinstance(result, dict) else None
        items = hits.get("hit") if isinstance(hits, dict) else None
        if not isinstance(items, list):
            return []

        results: list[SearchResult] = []
        for item in items:
            info = item.get("info") if isinstance(item, dict) else None
            if not isinstance(info, dict):
                continue
            title = info.get("title")
            if not isinstance(title, str) or not title.strip():
                continue
            url = self._best_url(info)
            if not url:
                continue
            year = info.get("year")
            results.append(
                SearchResult(
                    title=clip(title.rstrip("."), cap=300),
                    url=url,
                    snippet=self._snippet(info),
                    engine=self.name,
                    rank=0,
                    # DBLP records a year and nothing finer, so this is a
                    # display hint the freshness filter must not drop on.
                    published_age=str(year) if isinstance(year, str) and year else "",
                    published_age_confident=False,
                )
            )
        return results

    @staticmethod
    def _best_url(info: dict[str, Any]) -> str:
        """`ee` is the electronic edition — usually a DOI, i.e. the paper itself.

        The DBLP record page is the fallback: still useful (it links every
        version), just one hop further from the text.
        """
        for key in ("ee", "url"):
            value = info.get(key)
            if isinstance(value, str) and value.startswith("http"):
                return value
        doi = info.get("doi")
        if isinstance(doi, str) and doi:
            return f"https://doi.org/{doi}"
        return ""

    @staticmethod
    def _authors(info: dict[str, Any]) -> list[str]:
        block = info.get("authors")
        entries = block.get("author") if isinstance(block, dict) else None
        # A one-author paper serialises as a bare object, not a list.
        if isinstance(entries, dict):
            entries = [entries]
        if not isinstance(entries, list):
            return []
        names = []
        for entry in entries:
            if isinstance(entry, dict) and isinstance(entry.get("text"), str):
                names.append(entry["text"])
            elif isinstance(entry, str):
                names.append(entry)
        return names

    def _snippet(self, info: dict[str, Any]) -> str:
        bits: list[str] = []
        names = self._authors(info)
        if names:
            bits.append(", ".join(names[:3]) + (" et al." if len(names) > 3 else ""))
        for key in ("venue", "year", "type"):
            value = info.get(key)
            if isinstance(value, str) and value:
                bits.append(value)
        if info.get("access") == "open":
            bits.append("open access")
        return clip(" · ".join(bits))
