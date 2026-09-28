"""SerpBase (serpbase.dev) — Google SERP via REST API.

SerpBase is a Google Search Results API that returns structured JSON without
scraping, CAPTCHAs, or proxy maintenance. This is a KEYED engine: it requires a
``serpbase_api_key`` secret (read via the keystore, env
``SEARCH_MCP_SERPBASE_API_KEY`` or the admin UI).

Get a key: sign up at https://serpbase.dev (100 free searches, no credit card)
and copy the API key from https://serpbase.dev/dashboard/api-keys.

Request:
  GET https://api.serpbase.dev/google/search?q=<query>&api_key=<key>&num=10
  Query params: q (encoded), api_key, num (clamped 1..100), gl (optional),
                hl (optional)

Response (200) JSON:
  {"organic_results": [{"title", "link", "snippet", "position"}], ...}
  Map: title, url=link, snippet, published_age="" (not provided by API).

Strategy:
  * JSON GET API, so we OVERRIDE search() (the base class only knows how to GET
    an HTML page) and mirror the serper.py / anysearch.py override idioms.
  * Domain/filetype constraints ride in the query via
    base.augment_query_with_operators (Google understands site:/-site:/filetype:).
  * supports_browser_fallback is False — a JSON API that returns nothing or
    malformed data is genuinely empty, so a headless re-render is pointless.

Key handling:
  * A MISSING key raises ValueError with an actionable hint (the aggregator
    surfaces it via its errors map).
  * Any other failure (non-200, network error, malformed JSON) returns ``[]``
    so a flaky endpoint never poisons the aggregator. The key is never logged.
"""

from __future__ import annotations

from typing import Any
from urllib.parse import urlencode

from curl_cffi.requests import AsyncSession
from curl_cffi.requests.exceptions import RequestException

from ..config import settings
from ..httpfetch import IMPERSONATE
from ..keystore import get_secret
from ..net import curl_proxy_kwargs
from .base import (
    Engine,
    SearchFilters,
    SearchResult,
    augment_query_with_operators,
    raise_for_key_error,
)

_ENDPOINT = "https://api.serpbase.dev/google/search"

# SerpBase returns up to 100 organic results per call; we clamp to a reasonable
# page size to keep latency low.
_NUM_MIN = 1
_NUM_MAX = 100


class SerpBaseEngine(Engine):
    """SerpBase (serpbase.dev) Google SERP — keyed, JSON GET."""

    name = "serpbase"
    needs_browser = False
    # JSON API: an empty/malformed response is genuinely empty, so don't waste
    # a Playwright render trying to "recover" it (see Engine.search fallback).
    supports_browser_fallback = False

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        return _ENDPOINT

    def parse(self, html: str) -> list[SearchResult]:
        # Unused on the GET/JSON path (search() is overridden), but the ABC
        # requires it. Never raises.
        return []

    def _map_results(self, payload: Any) -> list[SearchResult]:
        """Map a SerpBase JSON payload into SearchResults. Never raises: any
        structural surprise yields ``[]`` so a malformed response can't poison
        the engine."""
        if not isinstance(payload, dict):
            return []
        items = payload.get("organic_results")
        if not isinstance(items, list):
            return []

        results: list[SearchResult] = []
        for item in items:
            if not isinstance(item, dict):
                continue
            title = _clean(item.get("title"))
            url = (item.get("link") or "").strip()
            if not title or not url:
                continue
            snippet = _clean(item.get("snippet"))
            results.append(
                SearchResult(
                    title=title,
                    url=url,
                    snippet=snippet,
                    engine=self.name,
                    rank=0,
                    published_age="",
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
        key = get_secret("serpbase_api_key")
        if not key:
            raise ValueError(
                "serpbase not configured: add serpbase_api_key in the admin UI "
                "(run: uv run search-mcp-admin) or set SEARCH_MCP_SERPBASE_API_KEY. "
                "Get a free key with 100 searches at https://serpbase.dev"
            )

        # Push domain/filetype constraints into the query via Google operators.
        filetype = "pdf" if filters and filters.category == "pdf" else None
        q = augment_query_with_operators(
            query,
            include_domains=filters.include_domains if filters else None,
            exclude_domains=filters.exclude_domains if filters else None,
            filetype=filetype,
        )
        num = max(_NUM_MIN, min(max_results, _NUM_MAX))
        params: dict[str, Any] = {
            "q": q,
            "api_key": key,
            "num": str(num),
        }

        results: list[SearchResult] = []
        status_code: int | None = None
        try:
            async with AsyncSession(
                impersonate=IMPERSONATE,
                timeout=settings.request_timeout,
                allow_redirects=True,
                **curl_proxy_kwargs(self.name),
            ) as client:
                url = f"{_ENDPOINT}?{urlencode(params)}"
                resp = await client.get(url)
                status_code = resp.status_code
                if resp.status_code == 200:
                    try:
                        payload = resp.json()
                    except Exception:
                        payload = None
                    if payload is not None:
                        results = self._map_results(payload)
        except RequestException:
            results = []
        except Exception:
            results = []

        if not results:
            raise_for_key_error(self.name, status_code)

        return self.finalize_results(results, filters, max_results, diagnostics)


def _clean(value: Any) -> str:
    """Trim a field and strip HTML highlight tags. Tolerant of non-strings (-> "")."""
    if not isinstance(value, str):
        return ""
    s = value.strip()
    for tag in ("<strong>", "</strong>", "<em>", "</em>", "<b>", "</b>"):
        s = s.replace(tag, "")
    return s.strip()
