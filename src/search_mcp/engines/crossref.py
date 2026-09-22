"""Crossref — DOI registration metadata for the scholarly literature. Keyless.

  GET https://api.crossref.org/works?query=<q>&rows=<n>&filter=type:journal-article

The type filter is load-bearing, not tidiness: unfiltered, Crossref happily
ranks individual *figures* and other sub-components above the papers that
contain them, because each one has its own DOI. Restricting to article types
is the difference between a useful result list and a list of figure captions.

Crossref records rarely carry abstracts, so the snippet is assembled from
authors, container title, and publisher instead.
"""

from __future__ import annotations

import re
from typing import Any
from urllib.parse import quote_plus

from ..config import settings
from .base import SearchFilters, SearchResult
from .jsonapi import JsonApiEngine, clip

_ENDPOINT = "https://api.crossref.org/works"

# Sub-component DOIs (figures, tables, chapters' sub-parts) crowd out papers.
_TYPES = "journal-article,proceedings-article,posted-content,book-chapter"

# The handful of fields actually used, so Crossref doesn't ship the full record.
_SELECT = "title,URL,abstract,issued,type,author,container-title,publisher"

# Crossref abstracts, when present, are JATS XML fragments.
_TAG_RE = re.compile(r"<[^>]+>")


class CrossrefEngine(JsonApiEngine):
    """Crossref works search (keyless JSON API)."""

    name = "crossref"
    description = "Crossref: the DOI registry; authoritative metadata for published articles."
    categories = frozenset({"paper", "paper.index"})

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        n = max(1, min(max_results, 100))
        params = [
            f"query={quote_plus(query)}",
            f"rows={n}",
            f"filter=type:{_TYPES.replace(',', ',type:')}",
            f"select={quote_plus(_SELECT)}",
        ]
        if settings.contact_email:
            params.append(f"mailto={quote_plus(settings.contact_email)}")
        if filters and filters.freshness:
            params.append("sort=published&order=desc")
        return f"{_ENDPOINT}?{'&'.join(params)}"

    def map_results(self, payload: Any) -> list[SearchResult]:
        if not isinstance(payload, dict):
            return []
        message = payload.get("message")
        if not isinstance(message, dict):
            return []
        items = message.get("items")
        if not isinstance(items, list):
            return []

        results: list[SearchResult] = []
        for item in items:
            if not isinstance(item, dict):
                continue
            title = clip(self._first(item.get("title")), cap=300)
            url = item.get("URL")
            if not title or not isinstance(url, str) or not url:
                continue
            date, date_confident = self._issued_date(item.get("issued"))
            results.append(
                SearchResult(
                    title=title,
                    url=url,
                    snippet=self._snippet(item),
                    engine=self.name,
                    rank=0,
                    published_age=date,
                    published_age_confident=date_confident,
                )
            )
        return results

    @staticmethod
    def _first(value: Any) -> str:
        """Crossref returns `title` and `container-title` as lists of strings."""
        if isinstance(value, list):
            return next((v for v in value if isinstance(v, str) and v), "")
        return value if isinstance(value, str) else ""

    @staticmethod
    def _issued_date(issued: Any) -> tuple[str, bool]:
        """`{"date-parts": [[2024, 8, 3]]}` -> `("2024-08-03", True)`.

        Returns ``(display_date, confident)``. Two rules, both learned from
        bugs:

        * **Truncate at the first ``None``, never filter them out.**
          ``date-parts`` is positional and stops being meaningful at the first
          gap: ``[[2024, None, 5]]`` means "2024, month unknown". Dropping the
          ``None`` collapsed the list to ``[2024, 5]`` and promoted the DAY
          into the month slot, reporting ``2024-05-01`` for a paper with no
          known month.

        * **Only a full Y-M-D is ``confident``.** Year-only records (``[[2024]]``)
          are extremely common; padding them to ``2024-01-01`` and flagging
          them confident let `apply_post_filters` DROP a paper actually issued
          in December under ``freshness="month"``, on the strength of a date
          Crossref never published. Partial dates are now emitted at their real
          precision (``"2024"`` / ``"2024-08"``), which also means
          ``_published_age_in_days`` cannot parse them into a bogus age. Open
          Library takes the same line for its year-only dates.
        """
        if not isinstance(issued, dict):
            return "", False
        parts = issued.get("date-parts")
        if not isinstance(parts, list) or not parts:
            return "", False
        first = parts[0]
        if not isinstance(first, list) or not first:
            return "", False
        nums: list[int] = []
        for p in first[:3]:
            # bool is a subclass of int, so `isinstance(True, int)` is True and
            # a stray boolean would otherwise be formatted as year 1.
            if not isinstance(p, int) or isinstance(p, bool):
                break
            nums.append(p)
        if not nums or not 1 <= nums[0] <= 9999:
            return "", False
        year = f"{nums[0]:04d}"
        if len(nums) < 2 or not 1 <= nums[1] <= 12:
            return year, False
        month = f"{year}-{nums[1]:02d}"
        if len(nums) < 3 or not 1 <= nums[2] <= 31:
            return month, False
        return f"{month}-{nums[2]:02d}", True

    def _snippet(self, item: dict[str, Any]) -> str:
        parts: list[str] = []
        authors = item.get("author")
        if isinstance(authors, list):
            names = [
                " ".join(x for x in (a.get("given"), a.get("family")) if isinstance(x, str))
                for a in authors
                if isinstance(a, dict)
            ]
            names = [n for n in names if n.strip()]
            if names:
                parts.append(", ".join(names[:3]) + (" et al." if len(names) > 3 else ""))
        venue = self._first(item.get("container-title")) or item.get("publisher")
        if isinstance(venue, str) and venue:
            parts.append(venue)
        abstract = item.get("abstract")
        if isinstance(abstract, str) and abstract:
            parts.append(_TAG_RE.sub(" ", abstract))
        return clip(" — ".join(parts))
