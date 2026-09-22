"""DOAJ — the Directory of Open Access Journals. Keyless.

  GET https://doaj.org/api/search/articles/<q>?pageSize=<n>

Every hit is an article a reader can open for free, which is the whole point:
a `paper` search that returns paywalled abstracts costs the caller a fetch that
cannot succeed. Cross-disciplinary, ~10M articles from vetted OA journals.

The query lives in the URL PATH, not the query string, so it must be
percent-encoded with `safe=""` — a slash in the query would otherwise change
the endpoint being called.
"""

from __future__ import annotations

from typing import Any
from urllib.parse import quote

from .base import SearchFilters, SearchResult
from .jsonapi import JsonApiEngine, clip

_ENDPOINT = "https://doaj.org/api/search/articles"
_ARTICLE = "https://doaj.org/article/{id}"


class DoajEngine(JsonApiEngine):
    """DOAJ open-access article search (keyless JSON API)."""

    name = "doaj"
    description = "DOAJ: open-access journal articles; every hit is free to read in full."
    categories = frozenset({"paper", "paper.openaccess"})

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        n = max(1, min(max_results, 100))
        # safe="" so a "/" in the query cannot escape into the path.
        params = [f"pageSize={n}"]
        if filters and filters.freshness:
            params.append("sort=created_date:desc")
        return f"{_ENDPOINT}/{quote(query, safe='')}?{'&'.join(params)}"

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
            bib = item.get("bibjson")
            bib = bib if isinstance(bib, dict) else {}
            title = bib.get("title")
            if not isinstance(title, str) or not title.strip():
                continue
            url = self._best_url(item, bib)
            if not url:
                continue
            results.append(
                SearchResult(
                    title=clip(" ".join(title.split()), cap=300),
                    url=url,
                    snippet=self._snippet(bib),
                    engine=self.name,
                    rank=0,
                    # DOAJ records year (and sometimes month), never a full
                    # date — display only, never trusted for dropping.
                    published_age=self._published(bib),
                    published_age_confident=False,
                )
            )
        return results

    @staticmethod
    def _best_url(item: dict[str, Any], bib: dict[str, Any]) -> str:
        """The publisher's full text first — that is what makes DOAJ useful."""
        links = bib.get("link")
        if isinstance(links, list):
            for link in links:
                if not isinstance(link, dict):
                    continue
                if link.get("type") != "fulltext":
                    continue
                url = link.get("url")
                if isinstance(url, str) and url.startswith("http"):
                    return url
        for ident in bib.get("identifier") or []:
            if isinstance(ident, dict) and ident.get("type") == "doi":
                doi = ident.get("id")
                if isinstance(doi, str) and doi:
                    return f"https://doi.org/{doi}"
        article_id = item.get("id")
        if isinstance(article_id, str) and article_id:
            return _ARTICLE.format(id=article_id)
        return ""

    @staticmethod
    def _published(bib: dict[str, Any]) -> str:
        year = bib.get("year")
        if not isinstance(year, str) or not year.isdigit():
            return ""
        month = bib.get("month")
        if isinstance(month, str) and month.isdigit() and 1 <= int(month) <= 12:
            return f"{year}-{int(month):02d}"
        return year

    def _snippet(self, bib: dict[str, Any]) -> str:
        bits: list[str] = []
        authors = bib.get("author")
        if isinstance(authors, list):
            names = [
                a["name"] for a in authors
                if isinstance(a, dict) and isinstance(a.get("name"), str)
            ]
            if names:
                bits.append(", ".join(names[:3]) + (" et al." if len(names) > 3 else ""))
        journal = bib.get("journal")
        if isinstance(journal, dict) and isinstance(journal.get("title"), str):
            bits.append(journal["title"])
        head = " · ".join(bits)
        abstract = bib.get("abstract")
        abstract = abstract if isinstance(abstract, str) else ""
        if head and abstract:
            return clip(f"{head} — {abstract}")
        return clip(abstract or head)
