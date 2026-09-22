"""MDN Web Docs search: the reference page for a web platform API or CSS feature.

  GET https://developer.mozilla.org/api/v1/search?q=<q>&locale=en-US

MDN's own search index, with the page summary as written by its editors.
A Chinese query is sent to the zh-CN locale, whose pages are translations of
the same references with English fallbacks where none exists.
"""

from __future__ import annotations

import re
from typing import Any
from urllib.parse import quote_plus

from .base import SearchFilters, SearchResult
from .facts import query_is_chinese
from .jsonapi import JsonApiEngine, clip

_API = "https://developer.mozilla.org/api/v1/search"
_SITE = "https://developer.mozilla.org"


_WEB_PLATFORM = re.compile(
    r"\w+\.prototype\.\w+|\b(css|html|javascript|typescript|dom|web ?api|webgl|webrtc|"
    r"service worker|indexeddb|websocket|http header|fetch api|mdn)\b|前端|浏览器 api",
    re.I,
)


class MdnEngine(JsonApiEngine):
    """MDN Web Docs reference pages (HTML, CSS, JavaScript, Web APIs, HTTP)."""

    name = "mdn"
    single_site = True
    categories = frozenset({"docs", "docs.web"})
    impersonate = None
    description = "MDN Web Docs: reference pages for HTML, CSS, JavaScript, Web APIs and HTTP."

    def claims(self, query: str) -> bool:
        return bool(_WEB_PLATFORM.search(query))

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        locale = "zh-CN" if query_is_chinese(query) else "en-US"
        n = max(1, min(max_results, 10))
        return f"{_API}?q={quote_plus(query)}&locale={locale}&size={n}"

    def map_results(self, payload: Any) -> list[SearchResult]:
        docs = payload.get("documents") if isinstance(payload, dict) else None
        results: list[SearchResult] = []
        for doc in docs or []:
            if not isinstance(doc, dict):
                continue
            title, path = doc.get("title"), doc.get("mdn_url")
            if not isinstance(title, str) or not isinstance(path, str):
                continue
            summary = doc.get("summary") if isinstance(doc.get("summary"), str) else ""
            results.append(
                SearchResult(
                    title=title,
                    url=path if path.startswith("http") else _SITE + path,
                    snippet=clip(summary, cap=400),
                    engine=self.name,
                    rank=len(results),
                )
            )
        return results
