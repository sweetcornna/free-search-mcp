"""Hugging Face Hub — public machine-learning dataset repositories. Keyless.

  GET https://huggingface.co/api/datasets?search=<q>&limit=<n>

Search results name repositories rather than individual files. The API does
not return a human URL, so it is derived from the observed repository ``id``.
"""

from __future__ import annotations

from typing import Any
from urllib.parse import quote_plus

from .base import SearchFilters, SearchResult
from .jsonapi import JsonApiEngine, clip, iso_date

_ENDPOINT = "https://huggingface.co/api/datasets"


class HuggingFaceEngine(JsonApiEngine):
    """Hugging Face Hub dataset-repository search (keyless JSON API)."""

    name = "huggingface"
    description = "Hugging Face Hub: machine-learning dataset repositories and dataset cards."
    categories = frozenset({"dataset", "dataset.ml"})

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        n = max(1, min(max_results, 100))
        return f"{_ENDPOINT}?search={quote_plus(query)}&limit={n}"

    def map_results(self, payload: Any) -> list[SearchResult]:
        if not isinstance(payload, list):
            return []

        results: list[SearchResult] = []
        for item in payload:
            if not isinstance(item, dict):
                continue
            repo_id = item.get("id")
            if not isinstance(repo_id, str) or not repo_id.strip():
                continue
            date = iso_date(item.get("lastModified"))
            results.append(
                SearchResult(
                    title=clip(repo_id, cap=300),
                    url=f"https://huggingface.co/datasets/{repo_id}",
                    snippet=clip(item.get("description")),
                    engine=self.name,
                    rank=0,
                    # The Hub exposes no publication date; lastModified is the
                    # only structured freshness signal and is labelled as such
                    # by the field rather than inferred from prose.
                    published_age=date,
                    published_age_confident=bool(date),
                )
            )
        return results
