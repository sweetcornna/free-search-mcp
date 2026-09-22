"""CISA KEV: whether a CVE is known to be exploited in the wild.

  GET https://www.cisa.gov/sites/default/files/feeds/known_exploited_vulnerabilities.json

The Known Exploited Vulnerabilities catalogue is one JSON file of about
1,700 entries (1.7 MB on 2026-09-21), each with the date CISA added it, the
action it requires and the due date for US federal agencies, and whether it
is used in ransomware campaigns. The file is fetched once and kept for six
hours; a CVE id in the question is looked up in it, and other questions
match vendor, product and title words, newest first.
"""

from __future__ import annotations

import re
import time
from typing import Any
from urllib.parse import quote_plus

from .base import SearchFilters, SearchResult
from .facts import _TOKEN_RE, STOP_WORDS
from .jsonapi import JsonApiEngine, clip

_FEED = "https://www.cisa.gov/sites/default/files/feeds/known_exploited_vulnerabilities.json"
_CATALOG = "https://www.cisa.gov/known-exploited-vulnerabilities-catalog"
_CVE_RE = re.compile(r"\bCVE-\d{4}-\d{4,}\b", re.I)
_EXPLOITED_ASK = re.compile(
    r"exploited in the wild|actively exploited|known exploited|\bkev\b|in.the.wild|"
    r"在野利用|被利用|已被攻击|实际利用",
    re.I,
)
_TTL = 6 * 3600.0
_cache: dict[str, Any] = {"at": 0.0, "rows": [], "version": ""}


class CisaKevEngine(JsonApiEngine):
    """Exploited-in-the-wild status of a CVE from CISA's KEV catalogue."""

    name = "cisakev"
    single_site = True
    categories = frozenset({"security", "security.exploited"})
    impersonate = None
    description = "CISA KEV: whether a CVE is known to be exploited, with date added and due date."

    def claims(self, query: str) -> bool:
        return bool(_CVE_RE.search(query)) or bool(_EXPLOITED_ASK.search(query))

    def answers_directly(self, query: str) -> bool:
        return bool(_CVE_RE.search(query))

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        return _FEED

    async def _rows(self) -> list[dict[str, Any]]:
        if time.monotonic() - _cache["at"] > _TTL or not _cache["rows"]:
            payload = await self._get_json(_FEED)
            rows = payload.get("vulnerabilities") if isinstance(payload, dict) else None
            if isinstance(rows, list) and rows:
                _cache["rows"] = [r for r in rows if isinstance(r, dict)]
                _cache["at"] = time.monotonic()
                _cache["version"] = str(payload.get("catalogVersion") or "")
        return _cache["rows"]

    async def fetch_results(
        self, query: str, max_results: int, filters: SearchFilters | None
    ) -> list[SearchResult]:
        rows = await self._rows()
        if not rows:
            return []
        ids = {i.upper() for i in _CVE_RE.findall(query)}
        if ids:
            hits = [r for r in rows if str(r.get("cveID", "")).upper() in ids]
            listed = {str(r.get("cveID", "")).upper() for r in hits}
            # Absence is an answer too: "is it exploited in the wild" is what
            # the question asked, and CISA's list is the reference for it.
            absent = [self._not_listed(cve) for cve in sorted(ids - listed)]
            return self.map_results({"vulnerabilities": hits[:max_results]}) + absent
        words = [
            w.lower()
            for w in _TOKEN_RE.findall(_EXPLOITED_ASK.sub(" ", query))
            if w.lower() not in STOP_WORDS and len(w) > 2
        ]
        if not words:
            return []
        hits = [
            r
            for r in rows
            if all(
                w in f"{r.get('vendorProject', '')} {r.get('product', '')} "
                f"{r.get('vulnerabilityName', '')}".lower()
                for w in words
            )
        ]
        hits.sort(key=lambda r: str(r.get("dateAdded", "")), reverse=True)
        return self.map_results({"vulnerabilities": hits[:max_results]})

    def _not_listed(self, cve: str) -> SearchResult:
        version = _cache.get("version") or ""
        day = version.replace(".", "-") if version.count(".") == 2 else ""
        return SearchResult(
            title=f"{cve}: not in CISA's Known Exploited Vulnerabilities catalogue"
            + (f" (as of {day})" if day else ""),
            url=f"{_CATALOG}?search_api_fulltext={quote_plus(cve)}",
            snippet=(
                "CISA lists a CVE in KEV when it has evidence of exploitation in the wild; "
                "this one is not listed, which means no such evidence has reached CISA, "
                "not that the flaw is harmless. Check NVD for the CVSS score and OSV for "
                "the fixed version."
            ),
            engine=self.name,
            rank=0,
            published_age=day or None,
            published_age_confident=bool(day),
        )

    def map_results(self, payload: Any) -> list[SearchResult]:
        rows = payload.get("vulnerabilities") if isinstance(payload, dict) else None
        results: list[SearchResult] = []
        for row in rows or []:
            if not isinstance(row, dict) or not isinstance(row.get("cveID"), str):
                continue
            cve = row["cveID"]
            added = str(row.get("dateAdded") or "")[:10]
            bits = [f"{row.get('vendorProject', '')} {row.get('product', '')}".strip()]
            if isinstance(row.get("vulnerabilityName"), str):
                bits.append(row["vulnerabilityName"])
            if added:
                bits.append(f"added to KEV {added}")
            if isinstance(row.get("dueDate"), str):
                bits.append(f"US federal remediation due {row['dueDate'][:10]}")
            ransomware = str(row.get("knownRansomwareCampaignUse") or "")
            if ransomware:
                bits.append(f"ransomware use: {ransomware.lower()}")
            if isinstance(row.get("requiredAction"), str):
                bits.append(row["requiredAction"])
            if isinstance(row.get("shortDescription"), str):
                bits.append(row["shortDescription"])
            results.append(
                SearchResult(
                    title=f"{cve}: known exploited in the wild (CISA KEV, added {added})",
                    url=f"{_CATALOG}?search_api_fulltext={quote_plus(cve)}",
                    snippet=clip(" · ".join(b for b in bits if b), cap=500),
                    engine=self.name,
                    rank=len(results),
                    published_age=added or None,
                    published_age_confident=bool(added),
                )
            )
        return results
