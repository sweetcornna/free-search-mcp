"""GLEIF: the legal entity behind a company name, by LEI.

  GET https://api.gleif.org/api/v1/lei-records?filter[fulltext]=<name>&page[size]=3

The Global Legal Entity Identifier Foundation publishes the registered name,
legal and headquarters addresses, jurisdiction, status and registration
number of every entity that holds an LEI, refreshed daily. Coverage is
entities that took an LEI, so financial institutions, listed companies and
their subsidiaries are in and a corner shop is not. Chinese legal names are
searchable as written. Rate limit stated in the docs: 60 requests a minute.
"""

from __future__ import annotations

import re
from typing import Any
from urllib.parse import quote_plus

from .base import SearchFilters, SearchResult
from .facts import iso_day
from .jsonapi import JsonApiEngine, clip

_API = "https://api.gleif.org/api/v1/lei-records"
_ENTITY_ASK = re.compile(
    r"\b(lei|legal entity|legal name|registered (?:name|office|address)|incorporated|"
    r"jurisdiction|parent company|subsidiary|company registration|registration number)\b|"
    r"法人|法定名称|注册地|注册地址|注册号|母公司|子公司|公司主体|工商登记|营业执照",
    re.I,
)
_STRIP = re.compile(
    r"\b(lei|legal entity|legal name|registered (?:name|office|address)|incorporated|"
    r"jurisdiction|parent company|subsidiary|company registration|registration number|of|the|"
    r"what|is|who|where|which|for|company|corporation|corp|inc|ltd|limited)\b|"
    r"法人|法定名称|注册地址|注册地|注册号|母公司|子公司|公司主体|工商登记|营业执照|的|是|查|"
    r"哪里|什么|谁|公司",
    re.I,
)
_LEI = re.compile(r"\b[A-Z0-9]{18}[0-9]{2}\b")


def entity_name(query: str) -> str:
    text = _STRIP.sub(" ", query)
    text = re.sub(r"[?？,，。!！]", " ", text)
    return " ".join(text.split())


class GleifEngine(JsonApiEngine):
    """Legal entity records from the GLEIF LEI index."""

    name = "gleif"
    single_site = True
    categories = frozenset({"finance", "finance.entity"})
    impersonate = None
    direct_answer = True
    rate_limit_per_minute = 30
    description = "GLEIF: legal entity name, addresses, jurisdiction and status by LEI."

    def claims(self, query: str) -> bool:
        return bool(_LEI.search(query)) or (
            bool(_ENTITY_ASK.search(query)) and bool(entity_name(query))
        )

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        n = max(1, min(max_results, 5))
        lei = _LEI.search(query)
        if lei:
            return f"{_API}?filter%5Blei%5D={lei.group(0)}&page%5Bsize%5D=1"
        name = entity_name(query) or query
        # Full-text search tokenises CJK names into single characters and
        # returns unrelated companies; the legal-name filter matches the
        # name as written.
        cjk = re.search(r"[\u3040-\u30ff\u3400-\u9fff]", name)
        field = "entity.legalName" if cjk else "fulltext"
        return f"{_API}?filter%5B{field}%5D={quote_plus(name)}&page%5Bsize%5D={n}"

    def map_results(self, payload: Any) -> list[SearchResult]:
        data = payload.get("data") if isinstance(payload, dict) else None
        meta = payload.get("meta") if isinstance(payload, dict) else None
        published = ""
        if isinstance(meta, dict) and isinstance(meta.get("goldenCopy"), dict):
            published = iso_day(meta["goldenCopy"].get("publishDate"))
        results: list[SearchResult] = []
        for row in data or []:
            attrs = row.get("attributes") if isinstance(row, dict) else None
            entity = attrs.get("entity") if isinstance(attrs, dict) else None
            if not isinstance(entity, dict) or not isinstance(attrs.get("lei"), str):
                continue
            legal = (entity.get("legalName") or {}).get("name")
            if not isinstance(legal, str):
                continue
            lei = attrs["lei"]
            bits = [f"LEI {lei}"]
            status = entity.get("status")
            if isinstance(status, str):
                bits.append(status.lower())
            jurisdiction = entity.get("jurisdiction")
            if isinstance(jurisdiction, str):
                bits.append(f"jurisdiction {jurisdiction}")
            address = entity.get("legalAddress")
            if isinstance(address, dict):
                place = ", ".join(
                    str(address[k])
                    for k in ("city", "region", "country")
                    if isinstance(address.get(k), str) and address[k]
                )
                if place:
                    bits.append(f"legal address {place}")
            registered = entity.get("registeredAs")
            if isinstance(registered, str) and registered:
                bits.append(f"registered as {registered}")
            created = iso_day(entity.get("creationDate"))
            if created:
                bits.append(f"created {created}")
            others = [
                o.get("name")
                for o in entity.get("otherNames") or []
                if isinstance(o, dict) and isinstance(o.get("name"), str)
            ]
            if others:
                bits.append("also " + "; ".join(others[:2]))
            if published:
                bits.append(f"GLEIF data of {published}")
            results.append(
                SearchResult(
                    title=f"{legal} ({lei})",
                    url=f"https://search.gleif.org/#/record/{lei}",
                    snippet=clip(" · ".join(bits)),
                    engine=self.name,
                    rank=len(results),
                    published_age=published or None,
                    published_age_confident=bool(published),
                )
            )
        return results
