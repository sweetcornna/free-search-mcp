"""arXiv — preprint search over the public Atom API. No key, no quota.

  GET https://export.arxiv.org/api/query?search_query=<expr>&max_results=<n>

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
import re
import xml.etree.ElementTree as ET
from urllib.parse import quote_plus

from .base import SearchFilters, SearchResult
from .jsonapi import JsonApiEngine, clip, iso_date

# https, not http: export.arxiv.org serves TLS, and every other endpoint in
# this package is https. Plaintext bought a redirect hop and nothing else.
_ENDPOINT = "https://export.arxiv.org/api/query"
_NS = {"a": "http://www.w3.org/2005/Atom"}


# arXiv does not index these, so a term list that includes one matches nothing
# at all (measured: `all:attention AND all:is AND ...` -> 0 results).
_STOPWORDS = frozenset(
    {
        "a", "all", "an", "and", "are", "as", "at", "be", "by", "for", "from", "in", "is",
        "it", "of", "on", "or", "the", "to", "versus", "via", "vs", "with", "you",
    }
)  # fmt: skip
_TERM_RE = re.compile(r"[^\W_][\w.+#-]*")
_MAX_TERMS = 8


def _search_query(query: str) -> str:
    """The `search_query` expression for a free-text query, URL-encoded.

    `all:deep residual learning` does not mean what it looks like. The field
    prefix binds to the first word and the rest are OR-ed, so the engine that
    natively indexes papers — and counts double for `category="paper"` —
    answered "reciprocal rank fusion" with an infrared image-FUSION paper, and
    "deep residual learning for image recognition" with anything containing
    "learning". Measured 2026-09-21, top results per shape:

        all:<words>              off topic (any one word matches)
        all:w1 AND all:w2 ...    on topic; ZERO results if a stopword is kept
        all:"<the query>"        the exact paper for a title; zero otherwise

    So: the exact phrase OR all of the content words. A title finds its paper
    first, a description finds papers about all of its terms, and neither comes
    back empty because of the other. A query that already carries quotes is the
    caller's own expression and is passed through.
    """
    text = " ".join(query.split())
    words = [w for w in _TERM_RE.findall(text) if w.lower() not in _STOPWORDS][:_MAX_TERMS]
    if '"' in text or len(words) < 2:
        return f"all:{quote_plus(text)}"
    phrase = quote_plus(f'"{text}"')
    every_term = "+AND+".join(f"all:{quote_plus(w)}" for w in words)
    return f"all:{phrase}+OR+%28{every_term}%29"


class ArxivEngine(JsonApiEngine):
    """arXiv preprint search (keyless Atom API)."""

    name = "arxiv"
    description = "arXiv preprints in physics, maths, CS and quantitative biology."
    categories = frozenset({"paper", "paper.preprint"})

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        # arXiv rejects max_results > 2000 and treats 0 as "no results".
        n = max(1, min(max_results, 100))
        params = [
            f"search_query={_search_query(query)}",
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
