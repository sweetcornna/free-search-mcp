"""npm: packages on the JavaScript registry, with their current version.

  GET https://registry.npmjs.org/-/v1/search?text=<q>&size=<n>

Unlike PyPI, npm still has a search endpoint, and each hit carries the
published version and its date, so the query goes through unchanged.
"""

from __future__ import annotations

from typing import Any
from urllib.parse import quote_plus

from .base import SearchFilters, SearchResult
from .facts import candidate_names, ecosystem_hints, iso_day, version_question
from .jsonapi import JsonApiEngine, clip

_API = "https://registry.npmjs.org/-/v1/search"


class NpmEngine(JsonApiEngine):
    """npm registry search: package name, current version and its release date."""

    name = "npm"
    single_site = True
    categories = frozenset({"software", "software.node"})
    impersonate = None
    direct_answer = True
    description = "npm registry: JavaScript packages with their current version and release date."

    def claims(self, query: str) -> bool:
        return (
            version_question(query)
            and "npm" in ecosystem_hints(query)
            and bool(candidate_names(query))
        )

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        n = max(1, min(max_results, 20))
        terms = " ".join(candidate_names(query, limit=2)) or query
        return f"{_API}?text={quote_plus(terms)}&size={n}"

    def map_results(self, payload: Any) -> list[SearchResult]:
        if not isinstance(payload, dict) or not isinstance(payload.get("objects"), list):
            return []
        results: list[SearchResult] = []
        for obj in payload["objects"]:
            pkg = obj.get("package") if isinstance(obj, dict) else None
            if not isinstance(pkg, dict):
                continue
            name, version = pkg.get("name"), pkg.get("version")
            if not isinstance(name, str) or not isinstance(version, str):
                continue
            date = iso_day(pkg.get("date"))
            bits = [f"Version {version}" + (f", published {date}" if date else "")]
            if isinstance(pkg.get("description"), str) and pkg["description"]:
                bits.append(pkg["description"])
            links = pkg.get("links") if isinstance(pkg.get("links"), dict) else {}
            for key in ("homepage", "repository"):
                if isinstance(links.get(key), str) and links[key].startswith("http"):
                    bits.append(f"{key} {links[key]}")
                    break
            results.append(
                SearchResult(
                    title=f"{name} {version} on npm",
                    url=f"https://www.npmjs.com/package/{name}/v/{version}",
                    snippet=clip(" · ".join(bits)),
                    engine=self.name,
                    rank=0,
                    published_age=date,
                    published_age_confident=bool(date),
                )
            )
        return results
