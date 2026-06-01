"""AnySearch unified search REST API engine.

Hits the public AnySearch endpoint (POST https://api.anysearch.com/v1/search)
exposed by the anysearch-ai/anysearch-mcp-server project. We send NO
Authorization header, which selects the anonymous, IP-rate-limited tier — no
API key or credentials of any kind are used.

Request:
  POST https://api.anysearch.com/v1/search
  Content-Type: application/json
  body: {"query": <q>, "max_results": <n clamped to 1..100>}

Response (200) JSON — the live API nests the list under ``data.results``:
  {"code": 0, "message": "success",
   "data": {"results": [{"title", "url", "description", "content", "score",
                         "quality_score", "signal_scores": {...}}]}}
We also tolerate a flat ``{"results": [...]}`` shape, and read ``published_at``
when the item carries it (the live payload often omits it).

Strategy:
  * This is a JSON POST API, so we OVERRIDE search() (the base class only
    knows how to GET an HTML page) and mirror the searx.py override idioms.
  * snippet is the longer of ``description`` / ``content`` (non-empty wins).
  * published_age is the ``YYYY-MM-DD`` date portion of ``published_at`` when
    present, else "".
  * supports_browser_fallback is False — a JSON API that returns nothing or
    malformed data is genuinely empty, so a headless re-render is pointless.

Caveats: the anonymous tier is IP rate-limited; on a 429/5xx, a network
error, or malformed JSON we return ``[]`` rather than raise, so a flaky
endpoint never poisons the aggregator.
"""

from __future__ import annotations

from typing import Any

from curl_cffi.requests import AsyncSession
from curl_cffi.requests.exceptions import RequestException

from ..config import settings
from ..keystore import get_secret
from ..net import curl_proxy_kwargs
from .base import (
    Engine,
    SearchFilters,
    SearchResult,
    apply_post_filters,
    apply_post_filters_with_diagnostics,
)


# Pinned at chrome131 to match the rest of the project — keeps the JA3/JA4 +
# HTTP/2 fingerprint consistent with the desktop UA we present elsewhere.
_IMPERSONATE = "chrome131"

_ENDPOINT = "https://api.anysearch.com/v1/search"


class AnySearchEngine(Engine):
    """AnySearch unified search REST API — anonymous (keyless), JSON POST."""

    name = "anysearch"
    needs_browser = False
    # JSON API: an empty/malformed response is genuinely empty, so don't waste
    # a Playwright render trying to "recover" it (see Engine.search fallback).
    supports_browser_fallback = False

    # Kept so the abstract contract is satisfied and so callers that compute a
    # cache key from build_url() get a stable string. The endpoint is constant;
    # the query/filters travel in the POST body, not the URL.
    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        return _ENDPOINT

    def parse(self, html: str) -> list[SearchResult]:
        # Unused on the POST/JSON path (search() is overridden), but the ABC
        # requires it. Never raises: an HTML/JSON blob arriving here can't be
        # mapped to AnySearch results, so we simply yield nothing.
        return []

    def _map_results(self, payload: Any) -> list[SearchResult]:
        """Map an AnySearch JSON payload into SearchResults. Never raises:
        any structural surprise yields ``[]`` so a malformed response can't
        poison the engine."""
        if not isinstance(payload, dict):
            return []
        # Live API nests results under data.results; tolerate a flat
        # {"results": [...]} as a fallback so a future API change can't break us.
        items = payload.get("results")
        if not isinstance(items, list):
            data = payload.get("data")
            if isinstance(data, dict):
                items = data.get("results")
        if not isinstance(items, list):
            return []

        results: list[SearchResult] = []
        for item in items:
            if not isinstance(item, dict):
                continue
            title = (item.get("title") or "").strip()
            url = (item.get("url") or "").strip()
            if not title or not url:
                continue
            # snippet: the longer of description / content (non-empty wins).
            description = (item.get("description") or "").strip()
            content = (item.get("content") or "").strip()
            snippet = description if len(description) >= len(content) else content
            published_age = self._date_portion(item.get("published_at"))
            results.append(
                SearchResult(
                    title=title,
                    url=url,
                    snippet=snippet,
                    engine=self.name,
                    rank=0,
                    published_age=published_age,
                )
            )
        return results

    @staticmethod
    def _date_portion(published_at: Any) -> str:
        """Return the ``YYYY-MM-DD`` prefix of an ISO ``published_at`` string,
        or "" when it's missing/empty/not a string."""
        if not isinstance(published_at, str):
            return ""
        s = published_at.strip()
        if not s:
            return ""
        # "2024-02-06T00:00:00Z" -> "2024-02-06"; tolerate a bare date too.
        return s.split("T", 1)[0]

    async def search(
        self,
        query: str,
        max_results: int,
        filters: SearchFilters | None = None,
        diagnostics: dict[str, Any] | None = None,
    ) -> list[SearchResult]:
        # AnySearch caps max_results at 100 and rejects < 1; clamp defensively.
        clamped = max(1, min(max_results, 100))

        # Optional key: anonymous works keyless (lower limits); a configured key
        # (admin UI / SEARCH_MCP_ANYSEARCH_API_KEY) raises the rate limit/quota.
        headers = {"Content-Type": "application/json"}
        key = get_secret("anysearch_api_key")
        if key:
            headers["Authorization"] = f"Bearer {key}"

        results: list[SearchResult] = []
        try:
            async with AsyncSession(
                impersonate=_IMPERSONATE,
                timeout=settings.request_timeout,
                allow_redirects=True,
                headers=headers,
                **curl_proxy_kwargs(self.name),
            ) as client:
                resp = await client.post(
                    _ENDPOINT,
                    json={"query": query, "max_results": clamped},
                )
                if resp.status_code == 200:
                    try:
                        payload = resp.json()
                    except Exception:
                        # Malformed JSON: never-raise contract -> empty.
                        payload = None
                    if payload is not None:
                        results = self._map_results(payload)
        except RequestException:
            results = []
        except Exception:
            results = []

        # We override search(), so we must call the post-filter ourselves — the
        # base class only does it on its own code path. Mirror the base class's
        # diagnostics contract so the aggregator's per-engine drop accounting
        # still works.
        if diagnostics is not None:
            raw_count = len(results)
            filtered, drops = apply_post_filters_with_diagnostics(results, filters)
            diagnostics.setdefault("raw_per_engine", {})[self.name] = raw_count
            diagnostics.setdefault("after_filter_per_engine", {})[self.name] = len(filtered)
            agg = diagnostics.setdefault("drops_by_reason", {})
            for reason, n in drops.items():
                agg[reason] = agg.get(reason, 0) + n
            results = filtered[:max_results]
        else:
            results = apply_post_filters(results, filters)[:max_results]
        for i, r in enumerate(results):
            r.rank = i + 1
            r.engine = self.name
        return results
