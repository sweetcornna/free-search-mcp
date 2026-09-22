"""NVD: CVE records from NIST's National Vulnerability Database.

  GET https://services.nvd.nist.gov/rest/json/cves/2.0?keywordSearch=<q>
  GET https://services.nvd.nist.gov/rest/json/cves/2.0?cveId=CVE-2024-6387

A CVE id in the query is looked up directly. Otherwise the keywords are
searched by publication date, newest window first: the API returns oldest
first and has no sort parameter, and a question about a vulnerability is
nearly always about a recent one. A date range may span at most 120 days, so
an empty window is followed by the one before it, three windows at most.
Keyless access allows 5 requests every 30 seconds; the engine's own rate
limit stays under that.
"""

from __future__ import annotations

import re
from datetime import UTC, datetime, timedelta
from typing import Any
from urllib.parse import quote_plus

from .base import SearchFilters, SearchResult
from .facts import iso_day
from .jsonapi import JsonApiEngine, clip

_API = "https://services.nvd.nist.gov/rest/json/cves/2.0"
_CVE_RE = re.compile(r"\bCVE-\d{4}-\d{4,}\b", re.I)
_RANGE_DAYS = 119
_WINDOWS = 3
_STAMP = "%Y-%m-%dT%H:%M:%S.000"


class NvdEngine(JsonApiEngine):
    """CVE lookup and keyword search in the National Vulnerability Database."""

    name = "nvd"
    single_site = True
    categories = frozenset({"security", "security.cve"})
    impersonate = None
    # 5 per 30 s keyless. Three windows in one search fit under this.
    rate_limit_per_minute = 9
    rate_limit_max_wait = 2.0
    description = "NIST NVD: CVE records with CVSS score, publication date and affected products."

    def answers_directly(self, query: str) -> bool:
        # A CVE id names one record; a keyword search returns candidates.
        return bool(_CVE_RE.search(query))

    def claims(self, query: str) -> bool:
        return bool(_CVE_RE.search(query))

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        ids = _CVE_RE.findall(query)
        if ids:
            return f"{_API}?cveId={ids[0].upper()}"
        return self._window_url(query, max_results, datetime.now(UTC))

    @staticmethod
    def _window_url(query: str, max_results: int, end: datetime) -> str:
        n = max(1, min(max_results, 20))
        start = end - timedelta(days=_RANGE_DAYS)
        return (
            f"{_API}?keywordSearch={quote_plus(query)}&resultsPerPage={n}"
            f"&pubStartDate={start.strftime(_STAMP)}&pubEndDate={end.strftime(_STAMP)}"
        )

    async def fetch_results(
        self, query: str, max_results: int, filters: SearchFilters | None
    ) -> list[SearchResult]:
        if _CVE_RE.search(query):
            return self.map_results(await self._get_json(self.build_url(query, max_results)))
        end = datetime.now(UTC)
        for _ in range(_WINDOWS):
            payload = await self._get_json(self._window_url(query, max_results, end))
            if payload is None:
                # A transport failure or a 403 from the rate limit. Asking
                # again would make the second more likely.
                return []
            results = self.map_results(payload)
            if results:
                return results
            end -= timedelta(days=_RANGE_DAYS)
        return []

    def map_results(self, payload: Any) -> list[SearchResult]:
        if not isinstance(payload, dict) or not isinstance(payload.get("vulnerabilities"), list):
            return []
        results: list[SearchResult] = []
        for item in reversed(payload["vulnerabilities"]):
            cve = item.get("cve") if isinstance(item, dict) else None
            if not isinstance(cve, dict) or not isinstance(cve.get("id"), str):
                continue
            cve_id = cve["id"]
            published = iso_day(cve.get("published"))
            description = ""
            for entry in cve.get("descriptions") or []:
                if isinstance(entry, dict) and entry.get("lang") == "en":
                    description = str(entry.get("value") or "")
                    break
            bits = []
            score = self._score(cve.get("metrics"))
            if score:
                bits.append(score)
            if published:
                bits.append(f"published {published}")
            if isinstance(cve.get("vulnStatus"), str):
                bits.append(cve["vulnStatus"])
            if description:
                bits.append(description)
            results.append(
                SearchResult(
                    title=f"{cve_id}" + (f" ({score.split(',')[0]})" if score else ""),
                    url=f"https://nvd.nist.gov/vuln/detail/{cve_id}",
                    snippet=clip(" · ".join(bits)),
                    engine=self.name,
                    rank=0,
                    published_age=published,
                    published_age_confident=bool(published),
                )
            )
        return results

    @staticmethod
    def _score(metrics: Any) -> str:
        if not isinstance(metrics, dict):
            return ""
        for key in ("cvssMetricV40", "cvssMetricV31", "cvssMetricV30", "cvssMetricV2"):
            entries = metrics.get(key)
            if isinstance(entries, list) and entries and isinstance(entries[0], dict):
                data = entries[0].get("cvssData") or {}
                score = data.get("baseScore")
                severity = data.get("baseSeverity") or entries[0].get("baseSeverity") or ""
                if score is not None:
                    return f"CVSS {score} {severity}".strip() + f", {data.get('version', key)}"
        return ""
