"""arXiv — preprint search over the public Atom API. No key, no quota.

  GET http://export.arxiv.org/api/query?search_query=all:<q>&max_results=<n>

The response is an Atom feed, not JSON, so `fetch_results` is overridden to
parse XML while everything else (session, error boundary, result tail) comes
from `JsonApiEngine`.

Each `<entry>` gives a real publication date in `<published>`, so results are
marked `published_age_confident` and freshness filtering can actually drop
stale hits instead of guessing from snippet text.
"""

from __future__ import annotations

# Stdlib ElementTree, same as googlenews.py's RSS path. It is not the XXE
# hazard the name suggests: CPython's expat binding never resolves external
# entities and rejects internal entity *definitions* outright
# ("ParseError: undefined entity"), which closes both XXE and billion-laughs.
# defusedxml would add a dependency for threats this parser does not have.
import xml.etree.ElementTree as ET
from urllib.parse import quote_plus

from .base import SearchFilters, SearchResult
from .jsonapi import JsonApiEngine, clip, iso_date

_ENDPOINT = "http://export.arxiv.org/api/query"
_NS = {"a": "http://www.w3.org/2005/Atom"}


class ArxivEngine(JsonApiEngine):
    """arXiv preprint search (keyless Atom API)."""

    name = "arxiv"
    categories = frozenset({"paper"})

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        # arXiv rejects max_results > 2000 and treats 0 as "no results".
        n = max(1, min(max_results, 100))
        params = [
            f"search_query=all:{quote_plus(query)}",
            "start=0",
            f"max_results={n}",
        ]
        # No date-range syntax here on purpose: arXiv's submittedDate ranges are
        # brittle to build and the base class already drops stale results using
        # the trustworthy <published> date. Sorting newest-first just makes the
        # result budget more likely to contain something that survives.
        if filters and filters.freshness:
            params.append("sortBy=submittedDate")
            params.append("sortOrder=descending")
        return f"{_ENDPOINT}?{'&'.join(params)}"

    async def fetch_results(
        self, query: str, max_results: int, filters: SearchFilters | None
    ) -> list[SearchResult]:
        body = await self._request_text(self.build_url(query, max_results, filters))
        if not body:
            return []
        return self._parse_feed(body)

    def _parse_feed(self, xml: str) -> list[SearchResult]:
        try:
            root = ET.fromstring(xml)
        except ET.ParseError:
            return []

        results: list[SearchResult] = []
        for entry in root.findall("a:entry", _NS):
            title = clip(entry.findtext("a:title", "", _NS), cap=300)
            url = self._abs_url(entry)
            if not title or not url:
                continue
            published = iso_date(entry.findtext("a:published", "", _NS))
            results.append(
                SearchResult(
                    title=title,
                    url=url,
                    snippet=self._snippet(entry),
                    engine=self.name,
                    rank=0,
                    published_age=published,
                    # <published> is a structured feed field, not scraped text.
                    published_age_confident=bool(published),
                )
            )
        return results

    @staticmethod
    def _abs_url(entry: ET.Element) -> str:
        """Prefer the human abstract page over the PDF.

        Entries carry several <link>s; the text/html one is the /abs/ page. Fall
        back to <id>, which is the same URL in practice.
        """
        for link in entry.findall("a:link", _NS):
            if link.get("type") == "text/html" and link.get("href"):
                return link.get("href", "")
        return (entry.findtext("a:id", "", _NS) or "").strip()

    def _snippet(self, entry: ET.Element) -> str:
        """Abstract, prefixed with the authors when there are any."""
        summary = clip(entry.findtext("a:summary", "", _NS))
        names = [
            n.strip()
            for author in entry.findall("a:author", _NS)
            if (n := author.findtext("a:name", "", _NS))
        ]
        if not names:
            return summary
        shown = ", ".join(names[:3]) + (" et al." if len(names) > 3 else "")
        return clip(f"{shown} — {summary}")
