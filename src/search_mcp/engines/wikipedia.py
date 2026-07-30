"""Wikipedia — encyclopedia search via the MediaWiki action API. Keyless.

  GET https://<lang>.wikipedia.org/w/api.php?action=query&list=search&...

The language is derived from `settings.region` ('us-en' -> en, 'cn-zh' -> zh),
so a user who already set their region gets their own Wikipedia without a
second knob.

Snippets come back with `<span class="searchmatch">` highlight markup around
the matched terms, which has to be stripped before it reaches a Markdown
renderer.
"""

from __future__ import annotations

import html
import re
from typing import Any
from urllib.parse import quote, quote_plus

from ..config import settings
from .base import SearchFilters, SearchResult
from .jsonapi import JsonApiEngine, clip

_TAG_RE = re.compile(r"<[^>]+>")


def _lang() -> str:
    """`settings.region` is a DDG-style 'cc-lang' token; Wikipedia wants the lang."""
    region = settings.region or ""
    _, _, lang = region.partition("-")
    lang = (lang or "en").strip().lower()
    # Guard the hostname we are about to build: anything that isn't a plain
    # language subtag falls back to English rather than being interpolated.
    return lang if lang.isalpha() and 2 <= len(lang) <= 3 else "en"


class WikipediaEngine(JsonApiEngine):
    """Wikipedia article search (keyless MediaWiki API)."""

    name = "wikipedia"

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        n = max(1, min(max_results, 50))
        return (
            f"https://{_lang()}.wikipedia.org/w/api.php"
            f"?action=query&list=search&format=json"
            f"&srsearch={quote_plus(query)}&srlimit={n}"
        )

    def map_results(self, payload: Any) -> list[SearchResult]:
        if not isinstance(payload, dict):
            return []
        query = payload.get("query")
        if not isinstance(query, dict):
            return []
        hits = query.get("search")
        if not isinstance(hits, list):
            return []

        lang = _lang()
        results: list[SearchResult] = []
        for hit in hits:
            if not isinstance(hit, dict):
                continue
            title = hit.get("title")
            if not isinstance(title, str) or not title:
                continue
            raw = hit.get("snippet")
            snippet = _TAG_RE.sub("", raw) if isinstance(raw, str) else ""
            results.append(
                SearchResult(
                    title=title,
                    # Percent-encode the title into the path; spaces become
                    # underscores per Wikipedia's URL convention.
                    url=f"https://{lang}.wikipedia.org/wiki/"
                    f"{quote(title.replace(' ', '_'), safe='')}",
                    snippet=clip(html.unescape(snippet)),
                    engine=self.name,
                    rank=0,
                )
            )
        return results
