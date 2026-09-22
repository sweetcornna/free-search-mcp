"""Federal Register: US rules, proposed rules and notices, newest first.

  GET https://www.federalregister.gov/api/v1/documents.json?conditions[term]=<q>&order=newest

The daily journal of the US federal government. Every published document
has a publication date, a type (Rule, Proposed Rule, Notice, Presidential
Document) and the agencies that issued it, so "what did the agency decide"
questions get the primary record with its date.
"""

from __future__ import annotations

import re
from typing import Any
from urllib.parse import quote_plus

from .base import SearchFilters, SearchResult
from .jsonapi import JsonApiEngine, clip

_API = "https://www.federalregister.gov/api/v1/documents.json"
_FIELDS = "&".join(
    f"fields[]={f}"
    for f in ("title", "publication_date", "html_url", "abstract", "type", "agencies")
)


_US_RULES = re.compile(
    r"federal register|executive order|final rule|proposed rule|\bcfr\b|rulemaking|"
    r"notice of proposed|联邦公报|行政令|美国.*(法规|规则)",
    re.I,
)


class FederalRegisterEngine(JsonApiEngine):
    """Documents published in the US Federal Register."""

    name = "federalregister"
    single_site = True
    categories = frozenset({"gov", "gov.us"})
    impersonate = None
    description = "Federal Register: US federal rules, proposed rules and notices with dates."

    def claims(self, query: str) -> bool:
        return bool(_US_RULES.search(query))

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        n = max(1, min(max_results, 20))
        return (
            f"{_API}?conditions[term]={quote_plus(query)}&order=relevance&per_page={n}&{_FIELDS}"
        )

    def map_results(self, payload: Any) -> list[SearchResult]:
        docs = payload.get("results") if isinstance(payload, dict) else None
        results: list[SearchResult] = []
        for doc in docs or []:
            if not isinstance(doc, dict):
                continue
            title, url = doc.get("title"), doc.get("html_url")
            if not isinstance(title, str) or not isinstance(url, str):
                continue
            when = doc.get("publication_date")
            agencies = [
                a.get("name")
                for a in doc.get("agencies") or []
                if isinstance(a, dict) and isinstance(a.get("name"), str)
            ]
            bits = []
            if isinstance(doc.get("type"), str):
                bits.append(doc["type"])
            if isinstance(when, str):
                bits.append(f"published {when}")
            if agencies:
                bits.append(", ".join(agencies[:3]))
            if isinstance(doc.get("abstract"), str) and doc["abstract"].strip():
                bits.append(clip(doc["abstract"].strip(), cap=300))
            results.append(
                SearchResult(
                    title=title,
                    url=url,
                    snippet=clip(" · ".join(bits), cap=450),
                    engine=self.name,
                    rank=len(results),
                    published_age=when if isinstance(when, str) else None,
                    published_age_confident=isinstance(when, str),
                )
            )
        return results
