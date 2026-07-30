"""Openverse — search for openly-licensed images. Keyless.

  GET https://api.openverse.org/v1/images/?q=<q>&page_size=<n>

Openverse aggregates Wikimedia Commons, Flickr, museums and others, so it
covers image search without needing a separate Commons engine.

Every result carries a licence, and the snippet leads with it: an image search
that doesn't say what you're allowed to do with the picture is a trap.
`url` is the image file itself, so a result can be handed straight to
`fetch(inline=True)` to actually look at it.
"""

from __future__ import annotations

from typing import Any
from urllib.parse import quote_plus

from ..config import settings
from .base import SearchFilters, SearchResult
from .jsonapi import JsonApiEngine, clip

_ENDPOINT = "https://api.openverse.org/v1/images/"


class OpenverseEngine(JsonApiEngine):
    """Openly-licensed image search (keyless JSON API)."""

    name = "openverse"
    categories = frozenset({"image"})

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        n = max(1, min(max_results, 100))
        # `format=json` in the query string, not just an Accept header: this is
        # Django REST Framework, and header-based negotiation does not survive
        # curl_cffi's browser impersonation reliably.
        params = [f"q={quote_plus(query)}", f"page_size={n}", "format=json"]
        if settings.safesearch != "off":
            params.append("mature=false")
        return f"{_ENDPOINT}?{'&'.join(params)}"

    def map_results(self, payload: Any) -> list[SearchResult]:
        if not isinstance(payload, dict):
            return []
        items = payload.get("results")
        if not isinstance(items, list):
            return []

        results: list[SearchResult] = []
        for item in items:
            if not isinstance(item, dict):
                continue
            # The direct file URL, so `fetch(inline=True)` works on it as-is.
            url = item.get("url")
            if not isinstance(url, str) or not url.startswith("http"):
                continue
            title = item.get("title")
            if not isinstance(title, str) or not title.strip():
                title = url.rsplit("/", 1)[-1]
            results.append(
                SearchResult(
                    title=clip(title, cap=300),
                    url=url,
                    snippet=self._snippet(item),
                    engine=self.name,
                    rank=0,
                )
            )
        return results

    def _snippet(self, item: dict[str, Any]) -> str:
        bits: list[str] = []
        # Licence first: it decides whether the image is usable at all.
        licence = item.get("license")
        if isinstance(licence, str) and licence:
            version = item.get("license_version")
            bits.append(
                f"CC {licence.upper()} {version}" if isinstance(version, str) and version
                else f"CC {licence.upper()}"
            )
        creator = item.get("creator")
        if isinstance(creator, str) and creator:
            bits.append(f"by {creator}")
        width, height = item.get("width"), item.get("height")
        if isinstance(width, int) and isinstance(height, int):
            bits.append(f"{width}×{height}px")
        source = item.get("foreign_landing_url")
        if isinstance(source, str) and source:
            bits.append(f"source: {source}")
        return clip(" · ".join(bits))
