"""App Store: the current version of an iOS app and its release notes.

  GET https://itunes.apple.com/search?term=<name>&country=cn&entity=software&limit=3

Apple's lookup service needs no key and returns the version, the date it
was released, the notes, the seller and the minimum iOS version. The
storefront follows the question's language: a Chinese question searches the
Chinese store, where 微信 and 支付宝 are listed under their own names.
"""

from __future__ import annotations

import re
from typing import Any
from urllib.parse import quote_plus

from .base import SearchFilters, SearchResult
from .facts import iso_day, query_is_chinese, version_question
from .jsonapi import JsonApiEngine, clip

_API = "https://itunes.apple.com/search"
_APP_ASK = re.compile(
    r"\b(app store|ios app|iphone app|ipad app|app version|ios version of)\b|"
    r"app\s*store|苹果版|ios 版|iphone 版|应用商店|app 版本|应用版本",
    re.I,
)
_STRIP = re.compile(
    r"苹果版|ios ?版|iphone ?版|ipad ?版|应用商店|应用|版本|最新|更新|发布|了什么|"
    r"的|是|什么|多少|现在|目前|了|吗|"
    r"\b(app store|ios app|iphone app|ipad app|app version|ios version of|app|ios|iphone|ipad|"
    r"apple|version|latest|newest|current|release|released|update|updated|what|is|the|of|on)\b",
    re.I,
)


def app_name(query: str) -> str:
    text = _STRIP.sub(" ", query)
    text = re.sub(r"[?？,，。!！]", " ", text)
    return " ".join(text.split())


class AppStoreEngine(JsonApiEngine):
    """Current version, release date and notes of an iOS app."""

    name = "appstore"
    single_site = True
    categories = frozenset({"software", "software.app"})
    impersonate = None
    direct_answer = True
    rate_limit_per_minute = 15
    description = "Apple App Store: current version, release date and notes of an iOS app."

    def claims(self, query: str) -> bool:
        return bool(_APP_ASK.search(query)) and version_question(query) and bool(app_name(query))

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        country = "cn" if query_is_chinese(query) else "us"
        n = max(1, min(max_results, 5))
        term = app_name(query) or query
        return f"{_API}?term={quote_plus(term)}&country={country}&entity=software&limit={n}"

    async def fetch_results(
        self, query: str, max_results: int, filters: SearchFilters | None
    ) -> list[SearchResult]:
        payload = await self._get_json(self.build_url(query, max_results, filters))
        if isinstance(payload, dict):
            payload["_term"] = app_name(query)
        return self.map_results(payload)

    def map_results(self, payload: Any) -> list[SearchResult]:
        rows = payload.get("results") if isinstance(payload, dict) else None
        rows = [r for r in rows or [] if isinstance(r, dict)]
        # The store's relevance order puts a publisher's other apps first at
        # times; an app whose name is the searched term leads.
        term = str(payload.get("_term") or "").lower() if isinstance(payload, dict) else ""
        if term:

            def _named(row: dict[str, Any]) -> int:
                return 0 if str(row.get("trackName", "")).lower().startswith(term) else 1

            rows.sort(key=_named)
        results: list[SearchResult] = []
        for row in rows:
            if not isinstance(row, dict):
                continue
            name, version, url = row.get("trackName"), row.get("version"), row.get("trackViewUrl")
            if not isinstance(name, str) or not isinstance(version, str):
                continue
            if not isinstance(url, str):
                url = f"https://apps.apple.com/app/id{row.get('trackId', '')}"
            day = iso_day(row.get("currentVersionReleaseDate"))
            bits = [f"Version {version}" + (f", released {day}" if day else "")]
            if isinstance(row.get("sellerName"), str):
                bits.append(f"by {row['sellerName']}")
            if isinstance(row.get("minimumOsVersion"), str):
                bits.append(f"requires iOS {row['minimumOsVersion']}")
            if isinstance(row.get("formattedPrice"), str):
                bits.append(row["formattedPrice"])
            rating = row.get("averageUserRating")
            if isinstance(rating, (int, float)):
                bits.append(f"rating {rating:.1f}")
            if isinstance(row.get("releaseNotes"), str) and row["releaseNotes"].strip():
                bits.append(" ".join(row["releaseNotes"].split()))
            results.append(
                SearchResult(
                    title=f"{name} {version} on the App Store",
                    url=url.split("?")[0],
                    snippet=clip(" · ".join(bits), cap=450),
                    engine=self.name,
                    rank=len(results),
                    published_age=day or None,
                    published_age_confident=bool(day),
                )
            )
        return results
