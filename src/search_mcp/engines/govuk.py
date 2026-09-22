"""GOV.UK search: guidance, policy and statistics pages of the UK government.

  GET https://www.gov.uk/api/search.json?q=<q>&fields=title,link,description,public_timestamp

The same index the site's own search box uses. Each hit carries its
document format (guidance, statistics, news, form) and the timestamp of its
last public update, which is what a "current rules" question needs.
"""

from __future__ import annotations

import re
from typing import Any
from urllib.parse import quote_plus

from .base import SearchFilters, SearchResult
from .jsonapi import JsonApiEngine, clip

_API = "https://www.gov.uk/api/search.json"
_SITE = "https://www.gov.uk"


_UK_GOV = re.compile(
    r"gov\.uk|\bhmrc\b|\bdvla\b|home office|英国(签证|政府|税|移民|海关)|"
    r"(?=.*\b(?:uk|british|britain)\b)(?=.*\b(?:visa|tax|benefit|passport|driving|licence|"
    r"pension|immigration|customs|nhs)\b)",
    re.I | re.S,
)


class GovUkEngine(JsonApiEngine):
    """Pages on GOV.UK, with format and last update."""

    name = "govuk"
    single_site = True
    categories = frozenset({"gov", "gov.uk"})
    impersonate = None
    description = "GOV.UK search: UK government guidance, statistics and news, with update dates."

    def claims(self, query: str) -> bool:
        return bool(_UK_GOV.search(query))

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        n = max(1, min(max_results, 20))
        return (
            f"{_API}?q={quote_plus(query)}&count={n}"
            "&fields=title,link,description,public_timestamp,format,organisations"
        )

    def map_results(self, payload: Any) -> list[SearchResult]:
        docs = payload.get("results") if isinstance(payload, dict) else None
        results: list[SearchResult] = []
        for doc in docs or []:
            if not isinstance(doc, dict):
                continue
            title, link = doc.get("title"), doc.get("link")
            if not isinstance(title, str) or not isinstance(link, str):
                continue
            when = doc.get("public_timestamp")
            day = when[:10] if isinstance(when, str) else None
            orgs = [
                o.get("title")
                for o in doc.get("organisations") or []
                if isinstance(o, dict) and isinstance(o.get("title"), str)
            ]
            bits = []
            if isinstance(doc.get("format"), str):
                bits.append(doc["format"].replace("_", " "))
            if day:
                bits.append(f"updated {day}")
            if orgs:
                bits.append(", ".join(orgs[:2]))
            if isinstance(doc.get("description"), str) and doc["description"].strip():
                bits.append(clip(doc["description"].strip(), cap=300))
            results.append(
                SearchResult(
                    title=title,
                    url=link if link.startswith("http") else _SITE + link,
                    snippet=clip(" · ".join(bits), cap=450),
                    engine=self.name,
                    rank=len(results),
                    published_age=day,
                    published_age_confident=bool(day),
                )
            )
        return results
