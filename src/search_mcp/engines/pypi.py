"""PyPI: the exact latest release of a Python package, from the registry.

  GET https://pypi.org/pypi/<name>/json

PyPI has no search API any more, so the engine guesses which words of the
query are package names (`facts.candidate_names`) and asks about each; a miss
is a cheap 404. What comes back is the fact a web snippet only paraphrases:
the version, the day it was uploaded, the Python it requires.
"""

from __future__ import annotations

import asyncio
from typing import Any
from urllib.parse import quote

from .base import SearchFilters, SearchResult
from .facts import candidate_names, ecosystem_hints, iso_day, version_question
from .jsonapi import JsonApiEngine, clip

_API = "https://pypi.org/pypi"


class PyPIEngine(JsonApiEngine):
    """Latest release of a Python package, straight from PyPI's JSON API."""

    name = "pypi"
    single_site = True
    categories = frozenset({"software", "software.python"})
    impersonate = None
    direct_answer = True
    description = "PyPI registry: current version, release date and Python requirement."

    def claims(self, query: str) -> bool:
        # A version question with a Python hint, or with no ecosystem named at
        # all: a bare package name is most often a PyPI one.
        hints = ecosystem_hints(query)
        return (
            version_question(query)
            and (not hints or "pypi" in hints)
            and bool(candidate_names(query))
        )

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        names = candidate_names(query, limit=1)
        return f"{_API}/{quote(names[0] if names else query.strip())}/json"

    async def fetch_results(
        self, query: str, max_results: int, filters: SearchFilters | None
    ) -> list[SearchResult]:
        names = candidate_names(query)
        payloads = await asyncio.gather(
            *(self._get_json(f"{_API}/{quote(name)}/json") for name in names)
        )
        results: list[SearchResult] = []
        for payload in payloads:
            results.extend(self.map_results(payload))
        return results

    def map_results(self, payload: Any) -> list[SearchResult]:
        if not isinstance(payload, dict) or not isinstance(payload.get("info"), dict):
            return []
        info = payload["info"]
        name, version = info.get("name"), info.get("version")
        if not isinstance(name, str) or not isinstance(version, str) or not version:
            return []
        uploaded = ""
        files = payload.get("urls")
        if isinstance(files, list) and files and isinstance(files[0], dict):
            uploaded = iso_day(files[0].get("upload_time_iso_8601") or files[0].get("upload_time"))
        bits = [f"Latest release {version}" + (f", uploaded {uploaded}" if uploaded else "")]
        if isinstance(info.get("requires_python"), str) and info["requires_python"]:
            bits.append(f"requires Python {info['requires_python']}")
        if info.get("yanked"):
            bits.append("this release is yanked")
        if isinstance(info.get("summary"), str) and info["summary"]:
            bits.append(info["summary"])
        home = (info.get("project_urls") or {}).get("Homepage") if isinstance(
            info.get("project_urls"), dict
        ) else None
        home = home or info.get("home_page")
        if isinstance(home, str) and home.startswith("http"):
            bits.append(f"homepage {home}")
        return [
            SearchResult(
                title=f"{name} {version} on PyPI",
                url=f"https://pypi.org/project/{name}/{version}/",
                snippet=clip(" · ".join(bits)),
                engine=self.name,
                rank=0,
                published_age=uploaded,
                published_age_confident=bool(uploaded),
            )
        ]
