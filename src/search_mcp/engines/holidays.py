"""Public holidays for a country and year.

  GET https://raw.githubusercontent.com/NateScarlet/holiday-cn/master/<year>.json
  GET https://nagerholidays.com/api/v4/Holidays/<CC>/<year>

China's schedule comes from holiday-cn, which parses the State Council's
annual notice and keeps the make-up working days (调休) that no generic
calendar has; the result links the notice itself. Every other country comes
from Nager.Date, which states no rate limit and covers about 200 countries.
The country and the year are read from the question; a Chinese question
with no country means China, and no year means this year.
"""

from __future__ import annotations

import asyncio
import re
from datetime import date
from typing import Any

from .base import SearchFilters, SearchResult
from .facts import COUNTRIES, ZH_COUNTRY, countries_in, query_is_chinese, year_in
from .jsonapi import JsonApiEngine, clip

_CN = "https://raw.githubusercontent.com/NateScarlet/holiday-cn/master/{year}.json"
_NAGER = "https://nagerholidays.com/api/v4/Holidays/{cc}/{year}"
_HOLIDAY_ASK = re.compile(
    r"\b(holidays?|public holidays?|bank holidays?|day off|long weekend)\b|"
    r"假期|假日|放假|节假日|调休|法定假|公众假期|休假安排|放假安排",
    re.I,
)


def _zh_name(iso2: str, fallback: str) -> str:
    for _key, (iso3, code2, _name) in COUNTRIES.items():
        if code2 == iso2 and iso3 in ZH_COUNTRY:
            return ZH_COUNTRY[iso3]
    return fallback


class HolidaysEngine(JsonApiEngine):
    """Public holidays by country and year, with China's make-up working days."""

    name = "holidays"
    single_site = True
    categories = frozenset({"calendar", "calendar.holidays"})
    impersonate = None
    direct_answer = True
    description = "Public holidays by country and year; China from the State Council notice."

    def claims(self, query: str) -> bool:
        return bool(_HOLIDAY_ASK.search(query)) and bool(self._targets(query))

    @staticmethod
    def _targets(query: str) -> list[tuple[str, str, str]]:
        found = countries_in(query)
        if not found and query_is_chinese(query):
            found = [("CHN", "CN", "China")]
        return found

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        year = year_in(query, default=date.today().year)
        targets = self._targets(query) or [("CHN", "CN", "China")]
        return self._url_for(targets[0][1], year)

    @staticmethod
    def _url_for(iso2: str, year: int) -> str:
        return _CN.format(year=year) if iso2 == "CN" else _NAGER.format(cc=iso2, year=year)

    async def fetch_results(
        self, query: str, max_results: int, filters: SearchFilters | None
    ) -> list[SearchResult]:
        targets = self._targets(query)
        if not targets:
            return []
        year = year_in(query, default=date.today().year)
        zh = query_is_chinese(query)
        payloads = await asyncio.gather(
            *(self._get_json(self._url_for(iso2, year)) for _iso3, iso2, _name in targets)
        )
        results: list[SearchResult] = []
        for (_iso3, iso2, name), payload in zip(targets, payloads, strict=True):
            if iso2 == "CN":
                results.extend(self._map_cn(payload, year, zh))
            else:
                results.extend(self._map_nager(payload, iso2, name, year, zh))
        return results

    def map_results(self, payload: Any) -> list[SearchResult]:
        if isinstance(payload, dict) and isinstance(payload.get("days"), list):
            return self._map_cn(payload, int(payload.get("year") or 0), True)
        if isinstance(payload, list) and payload and isinstance(payload[0], dict):
            cc = str(payload[0].get("countryCode") or "")
            year = str(payload[0].get("date") or "")[:4]
            return self._map_nager(payload, cc, cc, int(year or 0), False)
        return []

    def _map_cn(self, payload: Any, year: int, zh: bool) -> list[SearchResult]:
        days = payload.get("days") if isinstance(payload, dict) else None
        if not isinstance(days, list) or not days:
            return []
        # Group consecutive rows by holiday name, keeping off days and
        # make-up working days apart.
        groups: dict[str, dict[str, list[str]]] = {}
        order: list[str] = []
        for row in days:
            if not isinstance(row, dict):
                continue
            name, day = row.get("name"), str(row.get("date") or "")[5:]
            if not isinstance(name, str) or not day:
                continue
            if name not in groups:
                groups[name] = {"off": [], "work": []}
                order.append(name)
            groups[name]["off" if row.get("isOffDay") else "work"].append(day)
        parts = []
        for name in order:
            off, work = groups[name]["off"], groups[name]["work"]
            span = f"{off[0]} to {off[-1]}" if len(off) > 1 else (off[0] if off else "")
            if zh:
                text = f"{name} {span.replace(' to ', '至')}"
                if work:
                    text += "（调休上班 " + ", ".join(work) + "）"
            else:
                text = f"{name} {span}".strip()
                if work:
                    text += " (make-up working days " + ", ".join(work) + ")"
            parts.append(text)
        papers = payload.get("papers") if isinstance(payload, dict) else None
        url = papers[0] if isinstance(papers, list) and papers and isinstance(papers[0], str) else (
            "https://www.gov.cn/zhengce/"
        )
        title = (
            f"{year}年中国法定节假日安排（国务院通知）"
            if zh
            else f"China public holidays {year} (State Council notice)"
        )
        return [
            SearchResult(
                title=title,
                url=url,
                snippet=clip("；".join(parts) if zh else "; ".join(parts), cap=700),
                engine=self.name,
                rank=0,
            )
        ]

    def _map_nager(
        self, payload: Any, iso2: str, name: str, year: int, zh: bool
    ) -> list[SearchResult]:
        if not isinstance(payload, list) or not payload:
            return []
        parts = []
        for row in payload:
            if not isinstance(row, dict) or not isinstance(row.get("date"), str):
                continue
            label = row.get("localName") or row.get("name")
            if not isinstance(label, str):
                continue
            national = row.get("nationalHoliday", row.get("global", True))
            parts.append(f"{row['date'][5:]} {label}" + ("" if national else " (regional)"))
        if not parts:
            return []
        title = (
            f"{year}年{_zh_name(iso2, name)}公众假期" if zh else f"{name} public holidays {year}"
        )
        return [
            SearchResult(
                title=title,
                url=f"https://date.nager.at/PublicHoliday/Country/{iso2}/{year}",
                snippet=clip("; ".join(parts), cap=700),
                engine=self.name,
                rank=0,
            )
        ]
