"""crates.io: Rust crates with their current stable version.

  GET https://crates.io/api/v1/crates?q=<q>&per_page=<n>

crates.io asks API clients to identify themselves with a User-Agent and
refuses a browser fingerprint, so this engine sends the project's own.
"""

from __future__ import annotations

from typing import Any
from urllib.parse import quote_plus

from .base import SearchFilters, SearchResult
from .facts import candidate_names, ecosystem_hints, iso_day, version_question
from .jsonapi import USER_AGENT, JsonApiEngine, clip

_API = "https://crates.io/api/v1/crates"


class CratesEngine(JsonApiEngine):
    """crates.io search: Rust crates, their current stable version and last update."""

    name = "crates"
    single_site = True
    categories = frozenset({"software", "software.rust"})
    impersonate = None
    api_headers = {"User-Agent": USER_AGENT}
    direct_answer = True
    description = "crates.io registry: Rust crates, their current stable version and last update."

    def claims(self, query: str) -> bool:
        return (
            version_question(query)
            and "cargo" in ecosystem_hints(query)
            and bool(candidate_names(query))
        )

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        n = max(1, min(max_results, 20))
        terms = " ".join(candidate_names(query, limit=2)) or query
        return f"{_API}?q={quote_plus(terms)}&per_page={n}"

    def map_results(self, payload: Any) -> list[SearchResult]:
        if not isinstance(payload, dict) or not isinstance(payload.get("crates"), list):
            return []
        results: list[SearchResult] = []
        for crate in payload["crates"]:
            if not isinstance(crate, dict):
                continue
            name = crate.get("name")
            version = crate.get("max_stable_version") or crate.get("newest_version")
            if not isinstance(name, str) or not isinstance(version, str):
                continue
            updated = iso_day(crate.get("updated_at"))
            when = f", updated {updated}" if updated else ""
            bits = [f"Current stable version {version}{when}"]
            if isinstance(crate.get("description"), str) and crate["description"]:
                bits.append(crate["description"])
            for key in ("documentation", "repository"):
                if isinstance(crate.get(key), str) and crate[key].startswith("http"):
                    bits.append(f"{key} {crate[key]}")
                    break
            results.append(
                SearchResult(
                    title=f"{name} {version} on crates.io",
                    url=f"https://crates.io/crates/{name}/{version}",
                    snippet=clip(" · ".join(bits)),
                    engine=self.name,
                    rank=0,
                    published_age=updated,
                    published_age_confident=bool(updated),
                )
            )
        return results
