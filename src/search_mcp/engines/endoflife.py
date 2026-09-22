"""endoflife.date: release cycles, latest versions and support dates for about
480 products (languages, databases, operating systems, frameworks).

  GET https://endoflife.date/api/v1/products            the catalogue
  GET https://endoflife.date/api/v1/products/<name>     one product

"Is Python 3.11 still supported" and "what is the current Node LTS" are
questions a web snippet answers with last year's page. This source keeps
the dates as data. The catalogue is fetched once a day per process and
matched against the words of the query, so "node.js" finds `nodejs` through
the product's aliases.
"""

from __future__ import annotations

import asyncio
import re
import time
from typing import Any

from .base import SearchFilters, SearchResult
from .facts import ecosystem_hints, iso_day, version_question
from .jsonapi import JsonApiEngine, clip

_API = "https://endoflife.date/api/v1/products"
_CATALOGUE_TTL = 24 * 3600.0

_catalogue: dict[str, Any] = {"at": 0.0, "index": {}}
_WORD_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9.+#-]*")
_VERSION_RE = re.compile(r"\b\d+(?:\.\d+)*\b")


def _key(text: str) -> str:
    return re.sub(r"[^a-z0-9+#]", "", text.lower())


def _index(products: list[Any]) -> dict[str, str]:
    """Every spelling a product goes by, mapped to its API name."""
    index: dict[str, str] = {}
    for product in products:
        if not isinstance(product, dict) or not isinstance(product.get("name"), str):
            continue
        name = product["name"]
        spellings = [name, product.get("label")]
        aliases = product.get("aliases")
        if isinstance(aliases, list):
            spellings.extend(aliases)
        for spelling in spellings:
            if isinstance(spelling, str) and spelling:
                index.setdefault(_key(spelling), name)
    return index


class EndOfLifeEngine(JsonApiEngine):
    """endoflife.date: latest release, active cycles and support end dates."""

    name = "endoflife"
    single_site = True
    categories = frozenset({"software", "software.lifecycle"})
    impersonate = None
    direct_answer = True
    description = "endoflife.date: latest version, release cycles and support end dates."

    def claims(self, query: str) -> bool:
        # Any version or support question: the catalogue decides whether a
        # product is named, and a miss costs one cached lookup.
        return version_question(query) and not ecosystem_hints(query) - {"pypi", "npm", "go"}

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        return _API

    async def _catalogue_index(self) -> dict[str, str]:
        if time.monotonic() - _catalogue["at"] > _CATALOGUE_TTL or not _catalogue["index"]:
            payload = await self._get_json(_API)
            products = payload.get("result") if isinstance(payload, dict) else None
            if isinstance(products, list) and products:
                _catalogue["index"] = _index(products)
                _catalogue["at"] = time.monotonic()
        return _catalogue["index"]

    def _matches(self, query: str, index: dict[str, str]) -> list[str]:
        # The index holds only product spellings, so it is its own filter: a
        # stop list would wrongly drop "python", "go" and "rust", which are
        # products here. Two-word spans first ("red hat", "ubuntu lts").
        words = [w for w in _WORD_RE.findall(query) if len(w) > 1]
        found: list[str] = []
        for span in (2, 1):
            for i in range(len(words) - span + 1):
                key = _key("".join(words[i : i + span]))
                name = index.get(key)
                if name and name not in found:
                    found.append(name)
        return found[:3]

    async def fetch_results(
        self, query: str, max_results: int, filters: SearchFilters | None
    ) -> list[SearchResult]:
        index = await self._catalogue_index()
        names = self._matches(query, index)
        if not names:
            return []
        payloads = await asyncio.gather(*(self._get_json(f"{_API}/{name}") for name in names))
        results: list[SearchResult] = []
        for payload in payloads:
            if isinstance(payload, dict):
                payload["_query"] = query
            results.extend(self.map_results(payload))
        return results

    def map_results(self, payload: Any) -> list[SearchResult]:
        product = payload.get("result") if isinstance(payload, dict) else None
        if not isinstance(product, dict) or not isinstance(product.get("name"), str):
            return []
        releases = [r for r in product.get("releases") or [] if isinstance(r, dict)]
        if not releases:
            return []
        label = product.get("label") if isinstance(product.get("label"), str) else product["name"]
        newest = releases[0]
        latest = newest.get("latest") if isinstance(newest.get("latest"), dict) else {}
        latest_name = latest.get("name") if isinstance(latest.get("name"), str) else ""
        latest_date = iso_day(latest.get("date"))
        bits = []
        if latest_name:
            bits.append(
                f"Latest release {latest_name}" + (f" on {latest_date}" if latest_date else "")
                + f" (cycle {newest.get('name')})"
            )
        wanted = set(_VERSION_RE.findall(str(payload.get("_query") or "")))
        bits.append(self._cycles(releases, wanted))
        return [
            SearchResult(
                title=f"{label}: release cycles and support dates",
                url=f"https://endoflife.date/{product['name']}",
                snippet=clip(" · ".join(b for b in bits if b), cap=600),
                engine=self.name,
                rank=0,
                published_age=latest_date,
                published_age_confident=bool(latest_date),
            )
        ]

    @staticmethod
    def _cycles(releases: list[dict[str, Any]], wanted: set[str]) -> str:
        # A cycle the question names ("python 3.9", "ubuntu 22.04") goes first,
        # so it survives the snippet cap even when it is old.
        asked = [r for r in releases if str(r.get("name")) in wanted]
        rest = [r for r in releases if r not in asked]
        parts = []
        for release in (asked + rest)[:5]:
            name = release.get("name")
            if release.get("isEol"):
                state = "end of life"
                if isinstance(release.get("eolFrom"), str):
                    state += f" since {release['eolFrom']}"
            elif isinstance(release.get("eolFrom"), str):
                state = f"supported until {release['eolFrom']}"
            elif release.get("isEol") is False:
                state = "supported"
            else:
                state = "support end not published"
            if release.get("isLts"):
                state = "LTS, " + state
            parts.append(f"{name}: {state}")
        return "Cycles: " + "; ".join(parts)
