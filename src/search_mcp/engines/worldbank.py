"""World Bank Documents & Reports — development and macro research. Keyless.

  GET https://search.worldbank.org/api/v3/wds?format=json&qterm=<q>&rows=<n>

The Bank publishes country economic updates, macro monitoring briefs, poverty
assessments and sector studies here, and this endpoint is a real full-text
search over all of them — the primary-source counterpart to reading a news
story about a country's economy.

Deliberately NOT the indicator API. `api.worldbank.org/v2/indicator` has no
text search at all (`?search=gdp` is accepted and ignored — it returns the same
first page of all 29,544 indicators), so a keyword engine cannot be built on
it. Numeric series are `imf`'s job; this engine returns documents.

`fl=` is not an optimisation detail: the default response ships every indexed
field including full abstracts for every hit.
"""

from __future__ import annotations

from typing import Any
from urllib.parse import quote_plus

from .base import SearchFilters, SearchResult
from .jsonapi import JsonApiEngine, clip, iso_date

_ENDPOINT = "https://search.worldbank.org/api/v3/wds"
_FIELDS = "docdt,display_title,pdfurl,txturl,url,count,docty,owner,abstracts"


class WorldBankEngine(JsonApiEngine):
    """World Bank Documents & Reports search (keyless JSON API)."""

    name = "worldbank"
    description = "World Bank reports: country economic updates, macro briefs, sector studies."
    categories = frozenset({"finance", "finance.macro"})

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        n = max(1, min(max_results, 50))
        params = [
            "format=json",
            f"qterm={quote_plus(query)}",
            f"rows={n}",
            f"fl={_FIELDS}",
        ]
        if filters and filters.freshness:
            # The index sorts by relevance by default; newest-first is the only
            # freshness lever it exposes.
            params.append("srt=docdt&order=desc")
        return f"{_ENDPOINT}?{'&'.join(params)}"

    def map_results(self, payload: Any) -> list[SearchResult]:
        if not isinstance(payload, dict):
            return []
        documents = payload.get("documents")
        if not isinstance(documents, dict):
            return []

        results: list[SearchResult] = []
        for key, doc in documents.items():
            # The `documents` map carries a "facets" sibling alongside the hits.
            if key == "facets" or not isinstance(doc, dict):
                continue
            title = doc.get("display_title")
            if not isinstance(title, str) or not title.strip():
                continue
            url = self._best_url(doc)
            if not url:
                continue
            date = iso_date(doc.get("docdt"))
            results.append(
                SearchResult(
                    title=clip(title, cap=300),
                    url=url,
                    snippet=self._snippet(doc),
                    engine=self.name,
                    rank=0,
                    published_age=date,
                    # A publication date from the catalogue record, not prose.
                    published_age_confident=bool(date),
                )
            )
        return results

    @staticmethod
    def _best_url(doc: dict[str, Any]) -> str:
        """Landing page first — it links the PDF, the text and the metadata.

        The PDF is the fallback rather than the default so `fetch` gets a page
        it can render cheaply instead of a multi-megabyte download.
        """
        for key in ("url", "pdfurl", "txturl"):
            value = doc.get(key)
            if isinstance(value, str) and value.startswith("http"):
                # The catalogue still emits http:// links; the same host serves
                # https, and the model should not be handed a plaintext URL.
                return "https://" + value.split("://", 1)[1]
        return ""

    def _snippet(self, doc: dict[str, Any]) -> str:
        bits: list[str] = []
        for key in ("docty", "count"):
            value = doc.get(key)
            if isinstance(value, str) and value:
                bits.append(value)
        head = " · ".join(bits)
        abstracts = doc.get("abstracts")
        abstract = ""
        if isinstance(abstracts, dict):
            # The key really is "cdata!" — an XML artifact of the source index.
            raw = abstracts.get("cdata!")
            if isinstance(raw, str):
                abstract = raw
        if head and abstract:
            return clip(f"{head} — {abstract}")
        return clip(abstract or head)
