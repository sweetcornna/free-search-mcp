"""Europe PMC — life-sciences literature, preprints included. Keyless.

  GET https://www.ebi.ac.uk/europepmc/webservices/rest/search?query=<q>&format=json

Broader than `pubmed` on the same corpus: 40M+ records covering MEDLINE plus
preprint servers (bioRxiv, medRxiv, Research Square, …), patents and agricola,
with abstracts, citation counts and — crucially — direct open-access full-text
links, so a hit can be handed straight to `read_doc`.

It also serves two sub-groups no other registered engine can:

  * `paper.preprint` — `(SRC:"PPR")` restricts to preprints. bioRxiv's own API
    only pages by DATE and has no keyword search at all, so this is the way to
    search preprints; measured at 13,531 hits for a query that returns 228,334
    across the whole index.
  * `paper.openaccess` — `(OPEN_ACCESS:"Y")` restricts to papers whose full
    text is free to read.

Both are applied from `filters.category_token`, which carries the exact token
the caller asked for (`SearchFilters.category` has already been reduced to the
bare group by then).
"""

from __future__ import annotations

from datetime import UTC, datetime, timedelta
from typing import Any
from urllib.parse import quote_plus

from .base import SearchFilters, SearchResult
from .jsonapi import JsonApiEngine, clip

_ENDPOINT = "https://www.ebi.ac.uk/europepmc/webservices/rest/search"
_ARTICLE = "https://europepmc.org/article/{source}/{id}"

_FRESHNESS_DAYS = {"day": 1, "week": 7, "month": 31, "year": 366}

# Extra query clauses per sub-group. Europe PMC's own field syntax, so the
# restriction happens in the index rather than by discarding results here.
_SUBGROUP_CLAUSE = {
    "paper.preprint": '(SRC:"PPR")',
    "paper.openaccess": '(OPEN_ACCESS:"Y")',
}


class EuropePmcEngine(JsonApiEngine):
    """Europe PMC literature search (keyless JSON API)."""

    name = "europepmc"
    description = "Europe PMC: 40M+ life-science papers and preprints, with open-access full text."
    categories = frozenset(
        {"paper", "paper.biomed", "paper.preprint", "paper.openaccess"}
    )

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        n = max(1, min(max_results, 100))
        clauses = [query]
        token = getattr(filters, "category_token", None) if filters else None
        clause = _SUBGROUP_CLAUSE.get(token or "")
        if clause:
            clauses.insert(0, clause)
        if filters and filters.freshness:
            days = _FRESHNESS_DAYS.get(filters.freshness)
            if days:
                end = datetime.now(tz=UTC).date()
                start = end - timedelta(days=days)
                clauses.append(
                    f"(FIRST_PDATE:[{start.isoformat()} TO {end.isoformat()}])"
                )
        return (
            f"{_ENDPOINT}?query={quote_plus(' AND '.join(clauses))}"
            f"&format=json&pageSize={n}&resultType=core"
        )

    def map_results(self, payload: Any) -> list[SearchResult]:
        if not isinstance(payload, dict):
            return []
        block = payload.get("resultList")
        if not isinstance(block, dict):
            return []
        items = block.get("result")
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
            date = item.get("firstPublicationDate")
            date = date if isinstance(date, str) else ""
            results.append(
                SearchResult(
                    title=clip(title.rstrip("."), cap=300),
                    url=url,
                    snippet=self._snippet(item),
                    engine=self.name,
                    rank=0,
                    published_age=date,
                    # A catalogue publication date, not a date scraped from prose.
                    published_age_confident=bool(date),
                )
            )
        return results

    @staticmethod
    def _best_url(item: dict[str, Any]) -> str:
        """Free full text if there is any, else the Europe PMC record.

        An open-access HTML link is the most useful thing to hand a model:
        `read_doc` can open it. The PDF variant is skipped in favour of HTML —
        same content, far cheaper to fetch.
        """
        urls = item.get("fullTextUrlList")
        candidates = urls.get("fullTextUrl") if isinstance(urls, dict) else None
        if isinstance(candidates, list):
            for style in ("html", "pdf"):
                for entry in candidates:
                    if not isinstance(entry, dict):
                        continue
                    if entry.get("availabilityCode") != "OA":
                        continue
                    if entry.get("documentStyle") != style:
                        continue
                    url = entry.get("url")
                    if isinstance(url, str) and url.startswith("http"):
                        return url
        source = item.get("source")
        ident = item.get("id")
        if isinstance(source, str) and isinstance(ident, str) and source and ident:
            return _ARTICLE.format(source=source, id=ident)
        doi = item.get("doi")
        if isinstance(doi, str) and doi:
            return f"https://doi.org/{doi}"
        return ""

    def _snippet(self, item: dict[str, Any]) -> str:
        bits: list[str] = []
        if item.get("source") == "PPR":
            # A preprint has not been peer reviewed, and the model must be able
            # to say so without opening the record.
            bits.append("PREPRINT")
        authors = item.get("authorString")
        if isinstance(authors, str) and authors:
            bits.append(authors)
        journal = item.get("journalInfo")
        if isinstance(journal, dict):
            title = journal.get("journal")
            name = title.get("title") if isinstance(title, dict) else None
            if isinstance(name, str) and name:
                bits.append(name)
        cited = item.get("citedByCount")
        if isinstance(cited, int) and cited:
            bits.append(f"cited by {cited}")
        if item.get("isOpenAccess") == "Y":
            bits.append("open access")
        head = " · ".join(bits)
        abstract = item.get("abstractText")
        abstract = abstract if isinstance(abstract, str) else ""
        if head and abstract:
            return clip(f"{head} — {abstract}")
        return clip(abstract or head)
