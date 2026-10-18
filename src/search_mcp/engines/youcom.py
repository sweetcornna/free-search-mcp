"""You.com web search, keyless tier.

You.com serves its agent search endpoint without an API key, rate-limited per
IP (100 searches a day when this was written). We send no key and no
credentials. Suggested in issue #23.

Request:
  GET https://api.you.com/v1/agents/search?query=<q>&count=<n clamped to 1..50>

Response (200) JSON, measured 2026-10-18:
  {"results": {"web": [{"url", "title", "description", "snippets": [...],
                        "page_age" (ISO datetime, optional), "favicon_url",
                        "thumbnail_url"}]},
   "metadata": {"query", "search_uuid", "latency"}}

Strategy:
  * A JSON API, so search() is overridden, as in anysearch.py.
  * ``site:``, ``-site:`` and ``filetype:`` work in the query (measured
    2026-10-18), so domain and pdf filters ride along as operators and are
    still checked afterwards like for every engine.
  * The snippet is ``description``, or the first of ``snippets`` when that is
    empty. published_age is the date part of ``page_age``.
  * supports_browser_fallback is False: an empty JSON answer is really empty.

A 429, any other non-200, a network error or malformed JSON returns ``[]``, so
a flaky endpoint never poisons the aggregator. A refusal is still reported
through ``diagnostics["http_status"]``, because "the daily quota ran out" must
not read as "nothing matched".
"""

from __future__ import annotations

from typing import Any
from urllib.parse import urlencode

from curl_cffi.requests import AsyncSession
from curl_cffi.requests.exceptions import RequestException

from ..config import settings
from ..httpfetch import IMPERSONATE
from ..net import curl_proxy_kwargs
from .base import (
    Engine,
    SearchFilters,
    SearchResult,
    augment_query_with_operators,
)

_ENDPOINT = "https://api.you.com/v1/agents/search"

# The endpoint returned 50 results for count=50 when measured; ask for no more.
_COUNT_MAX = 50

# A SERP snippet is a teaser. The `snippets` fallback can be several long
# passages, so only the first is used, capped.
_SNIPPET_CAP = 400


class YouComEngine(Engine):
    """You.com web search, keyless JSON GET."""

    name = "youcom"
    description = (
        "You.com's own web index, keyless; honours site: and filetype:, "
        "about 100 searches a day per IP."
    )
    needs_browser = False
    supports_browser_fallback = False

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        q = augment_query_with_operators(
            query,
            include_domains=filters.include_domains if filters else None,
            exclude_domains=filters.exclude_domains if filters else None,
            filetype="pdf" if filters and filters.category == "pdf" else None,
        )
        count = max(1, min(max_results, _COUNT_MAX))
        return f"{_ENDPOINT}?{urlencode({'query': q, 'count': count})}"

    def parse(self, html: str) -> list[SearchResult]:
        # Unused: search() is overridden. The ABC requires it.
        return []

    def _map_results(self, payload: Any) -> list[SearchResult]:
        """Map a You.com payload into SearchResults. Never raises."""
        if not isinstance(payload, dict):
            return []
        groups = payload.get("results")
        items = groups.get("web") if isinstance(groups, dict) else None
        if not isinstance(items, list):
            return []

        results: list[SearchResult] = []
        for item in items:
            if not isinstance(item, dict):
                continue
            title = _text(item.get("title"))
            url = _text(item.get("url"))
            if not title or not url.startswith(("http://", "https://")):
                continue
            snippet = _text(item.get("description"))
            if not snippet:
                snippets = item.get("snippets")
                if isinstance(snippets, list) and snippets:
                    snippet = _text(snippets[0])
            if len(snippet) > _SNIPPET_CAP:
                snippet = snippet[:_SNIPPET_CAP].rstrip() + " …"
            page_age = _text(item.get("page_age"))
            published_age = page_age.split("T", 1)[0] if page_age else ""
            results.append(
                SearchResult(
                    title=title,
                    url=url,
                    snippet=snippet,
                    engine=self.name,
                    rank=0,
                    published_age=published_age,
                    # page_age is a structured API field.
                    published_age_confident=bool(published_age),
                )
            )
        return results

    async def search(
        self,
        query: str,
        max_results: int,
        filters: SearchFilters | None = None,
        diagnostics: dict[str, Any] | None = None,
    ) -> list[SearchResult]:
        results: list[SearchResult] = []
        try:
            async with AsyncSession(
                impersonate=IMPERSONATE,
                timeout=settings.request_timeout,
                allow_redirects=True,
                headers={"Accept": "application/json"},
                **curl_proxy_kwargs(self.name),
            ) as client:
                resp = await client.get(self.build_url(query, max_results, filters))
                if resp.status_code == 200:
                    try:
                        payload = resp.json()
                    except Exception:
                        payload = None
                    results = self._map_results(payload)
                elif diagnostics is not None:
                    diagnostics.setdefault("http_status", {})[self.name] = resp.status_code
        except RequestException:
            results = []
        except Exception:
            results = []

        # search() is overridden, so the shared tail (post-filters, diagnostics,
        # ranks) has to be applied here.
        return self.finalize_results(results, filters, max_results, diagnostics)


def _text(value: Any) -> str:
    return value.strip() if isinstance(value, str) else ""
