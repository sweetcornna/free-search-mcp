"""Wikimedia Commons — freely licensed media through the MediaWiki API.

  GET https://commons.wikimedia.org/w/api.php?action=query&generator=search

One generator request returns the direct file URL plus description, creator,
licence and dimensions. That keeps attribution beside the image and avoids
scraping Commons file pages.
"""

from __future__ import annotations

import html
import re
from typing import Any
from urllib.parse import quote_plus

from .base import SearchFilters, SearchResult
from .jsonapi import JsonApiEngine, clip, iso_date

_ENDPOINT = "https://commons.wikimedia.org/w/api.php"
_TAG_RE = re.compile(r"<[^>]+>")


def _metadata_value(metadata: Any, key: str) -> str:
    block = metadata.get(key) if isinstance(metadata, dict) else None
    value = block.get("value") if isinstance(block, dict) else None
    if not isinstance(value, str):
        return ""
    return html.unescape(_TAG_RE.sub(" ", value)).strip()


class WikimediaEngine(JsonApiEngine):
    """Wikimedia Commons image search (keyless MediaWiki API)."""

    name = "wikimedia"
    description = "Wikimedia Commons: freely licensed images with attribution and source metadata."
    categories = frozenset({"image"})
    # Wikimedia asks API clients to identify themselves; browser impersonation
    # would hide the contactable User-Agent the shared API path already sends.
    impersonate = None

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        n = max(1, min(max_results, 50))
        return (
            f"{_ENDPOINT}?action=query&generator=search"
            f"&gsrsearch={quote_plus(query)}&gsrnamespace=6&gsrlimit={n}"
            "&prop=imageinfo&iiprop=url%7Cextmetadata%7Csize%7Cmime%7Ctimestamp"
            "&iiurlwidth=500&format=json"
        )

    def map_results(self, payload: Any) -> list[SearchResult]:
        if not isinstance(payload, dict):
            return []
        query = payload.get("query")
        pages = query.get("pages") if isinstance(query, dict) else None
        if not isinstance(pages, dict):
            return []

        results: list[SearchResult] = []
        for page in pages.values():
            if not isinstance(page, dict):
                continue
            title = page.get("title")
            infos = page.get("imageinfo")
            info = infos[0] if isinstance(infos, list) and infos else None
            if not isinstance(title, str) or not title.strip():
                continue
            title = title.removeprefix("File:").strip()
            if not title:
                continue
            if not isinstance(info, dict):
                continue
            url = info.get("url")
            if not isinstance(url, str) or not url.startswith("http"):
                continue
            date = iso_date(info.get("timestamp"))
            results.append(
                SearchResult(
                    title=clip(title, cap=300),
                    url=url,
                    snippet=self._snippet(info),
                    engine=self.name,
                    rank=0,
                    published_age=date,
                    published_age_confident=bool(date),
                )
            )
        return results

    @staticmethod
    def _snippet(info: dict[str, Any]) -> str:
        metadata = info.get("extmetadata")
        bits: list[str] = []
        licence = _metadata_value(metadata, "LicenseShortName")
        if licence:
            bits.append(licence)
        artist = _metadata_value(metadata, "Artist")
        if artist:
            bits.append(f"by {artist}")
        width, height = info.get("width"), info.get("height")
        if isinstance(width, int) and isinstance(height, int):
            bits.append(f"{width}×{height}px")
        description = _metadata_value(metadata, "ImageDescription")
        head = " · ".join(bits)
        if head and description:
            return clip(f"{head} — {description}")
        return clip(description or head)
