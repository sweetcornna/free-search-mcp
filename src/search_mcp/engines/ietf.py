"""IETF Datatracker: RFCs by number or title, with status and obsoletion.

  GET https://datatracker.ietf.org/api/v1/doc/document/?name=rfc9110&format=json
  GET https://datatracker.ietf.org/api/v1/doc/document/?title__icontains=<q>&type=rfc

The datatracker is the registry of record for RFCs. A query with an RFC
number gets that document; otherwise titles are matched. Each result names
the standards level and links the HTML text on rfc-editor.org, where the
errata and the "obsoleted by" banner live. The `.html` spelling is the one
web indexes return, so the two sightings merge into one result. The record's
`time` is its last edit, not the publication date, so no date is claimed.
"""

from __future__ import annotations

import re
from typing import Any
from urllib.parse import quote_plus

from .base import SearchFilters, SearchResult
from .facts import _TOKEN_RE, STOP_WORDS
from .jsonapi import JsonApiEngine, clip

_API = "https://datatracker.ietf.org/api/v1/doc/document/"
_RFC_RE = re.compile(r"\brfc[\s-]*(\d{1,5})\b", re.I)
_LEVELS = {
    "std": "Internet Standard",
    "ps": "Proposed Standard",
    "ds": "Draft Standard",
    "bcp": "Best Current Practice",
    "inf": "Informational",
    "exp": "Experimental",
    "hist": "Historic",
    "unkn": "unknown status",
}


_NOT_IN_TITLES = frozenset(
    {"rfc", "rfcs", "ietf", "spec", "specification", "standard", "标准", "规范"}
)


def _title_terms(query: str) -> str:
    words = [
        t
        for t in _TOKEN_RE.findall(query)
        if t.lower() not in STOP_WORDS and t.lower() not in _NOT_IN_TITLES and not t.isdigit()
    ]
    return " ".join(words[:6])


class IetfEngine(JsonApiEngine):
    """RFCs from the IETF Datatracker, by number or title words."""

    name = "ietf"
    single_site = True
    categories = frozenset({"docs", "docs.rfc"})
    impersonate = None
    description = "IETF Datatracker: RFCs by number or title, with standards level and dates."

    def answers_directly(self, query: str) -> bool:
        # An RFC number names one document; title words return candidates.
        return bool(_RFC_RE.search(query))

    def claims(self, query: str) -> bool:
        return bool(_RFC_RE.search(query))

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        numbers = _RFC_RE.findall(query)
        if numbers:
            names = ",".join(f"rfc{int(n)}" for n in numbers[:5])
            return f"{_API}?name__in={names}&format=json"
        terms = _title_terms(query) or query
        n = max(1, min(max_results, 10))
        return f"{_API}?title__icontains={quote_plus(terms)}&type=rfc&format=json&limit={n}"

    def map_results(self, payload: Any) -> list[SearchResult]:
        objects = payload.get("objects") if isinstance(payload, dict) else None
        results: list[SearchResult] = []
        for doc in objects or []:
            if not isinstance(doc, dict):
                continue
            name, title = doc.get("name"), doc.get("title")
            if not isinstance(name, str) or not isinstance(title, str):
                continue
            rfc = doc.get("rfc")
            number = str(rfc) if rfc else (name[3:] if name.lower().startswith("rfc") else "")
            if not number.isdigit():
                continue
            level_key = str(doc.get("std_level") or "").rsplit("/", 2)[-2:-1]
            level = _LEVELS.get(level_key[0], level_key[0]) if level_key else ""
            bits = [f"RFC {number}"]
            if level:
                bits.append(level)
            tags = [str(t).rstrip("/").rsplit("/", 1)[-1] for t in doc.get("tags") or []]
            if "verified-errata" in tags:
                bits.append("has verified errata")
            elif "errata" in tags:
                bits.append("has reported errata")
            if isinstance(doc.get("abstract"), str) and doc["abstract"].strip():
                bits.append(clip(doc["abstract"].strip(), cap=300))
            results.append(
                SearchResult(
                    title=f"RFC {number}: {title}",
                    url=f"https://www.rfc-editor.org/rfc/rfc{number}.html",
                    snippet=clip(" · ".join(bits), cap=420),
                    engine=self.name,
                    rank=len(results),
                )
            )
        return results
