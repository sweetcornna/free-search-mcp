"""Tavily AI-search REST API engine (KEYED).

Tavily (https://tavily.com) is an LLM-oriented search API. It requires an API
key — get one free at https://app.tavily.com (the key starts with ``tvly-``);
1,000 credits/month are free. Configure it via the admin UI
(``uv run search-mcp-admin``) or the ``SEARCH_MCP_TAVILY_API_KEY`` env var; the
engine reads it through :func:`keystore.get_secret` under the field
``tavily_api_key``.

Request:
  POST https://api.tavily.com/search
  Authorization: Bearer <key>          # Tavily's current contract — NOT a body field
  Content-Type: application/json
  body: {"query": <q>, "max_results": <n clamped 1..20>,
         "search_depth": "basic", "include_answer": false,
         "include_raw_content": false}
  Tavily supports domain filters NATIVELY, so when ``filters`` carries
  include/exclude domains we forward them as ``include_domains`` /
  ``exclude_domains`` and let the provider narrow server-side (the client-side
  post-filter then re-enforces them, as for every other engine).

Response (200) JSON:
  {"results": [{"title", "url", "content", "score",
                "published_date" (optional ISO)}], "answer": null}
  Map: title, url, snippet = ``content``, published_age = the ``YYYY-MM-DD``
  portion of ``published_date`` when present, else "".

Strategy:
  * This is a JSON POST API, so we OVERRIDE search() (the base class only knows
    how to GET an HTML page) and mirror the anysearch.py / searx.py overrides.
  * A MISSING key is a configuration error, so we RAISE ValueError (the
    aggregator catches it into its per-engine errors map). Any other failure
    — non-200, network error, malformed JSON — returns ``[]`` so a flaky
    endpoint never poisons the aggregator.
  * supports_browser_fallback is False — a JSON API that returns nothing or
    malformed data is genuinely empty, so a headless re-render is pointless.

The API key is NEVER logged, printed, or placed in build_url() (the cache key).
"""

from __future__ import annotations

import html as html_lib
import re
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

_ENDPOINT = "https://api.tavily.com/search"

# keystore field the API key lives under (env: SEARCH_MCP_TAVILY_API_KEY).
_KEY_FIELD = "tavily_api_key"

# Tavily caps max_results at 20 and rejects < 1.
_MAX_RESULTS = 20

_TAG_RE = re.compile(r"<[^>]+>")


def _strip_html(s: str) -> str:
    """Remove highlight tags (e.g. ``<strong>``/``<em>``) and decode entities."""
    if not s:
        return ""
    no_tags = _TAG_RE.sub(" ", s)
    decoded = html_lib.unescape(no_tags)
    return " ".join(decoded.split())


class TavilyEngine(Engine):
    """Tavily AI-search REST API — keyed, JSON POST."""

    name = "tavily"
    needs_browser = False
    # JSON API: an empty/malformed response is genuinely empty, so don't waste
    # a Playwright render trying to "recover" it (see Engine.search fallback).
    supports_browser_fallback = False

    # Kept so the abstract contract is satisfied and so callers that compute a
    # cache key from build_url() get a stable string. The endpoint is constant;
    # the query/filters/key travel in the POST body, NOT the URL — keeping the
    # secret out of any cache key.
    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        return _ENDPOINT

    def parse(self, html: str) -> list[SearchResult]:
        # Unused on the POST/JSON path (search() is overridden), but the ABC
        # requires it. Never raises: an HTML/JSON blob arriving here can't be
        # mapped to Tavily results, so we simply yield nothing.
        return []

    def _map_results(self, payload: Any) -> list[SearchResult]:
        """Map a Tavily JSON payload into SearchResults. Never raises: any
        structural surprise yields ``[]`` so a malformed response can't poison
        the engine."""
        if not isinstance(payload, dict):
            return []
        items = payload.get("results")
        if not isinstance(items, list):
            return []

        results: list[SearchResult] = []
        for item in items:
            if not isinstance(item, dict):
                continue
            title = _strip_html(item.get("title") or "")
            url = (item.get("url") or "").strip()
            if not title or not url:
                continue
            snippet = _strip_html(item.get("content") or "")
            published_age = self._date_portion(item.get("published_date"))
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
    def _date_portion(published_date: Any) -> str:
        """Return the ``YYYY-MM-DD`` prefix of an ISO ``published_date`` string,
        or "" when it's missing/empty/not a string."""
        if not isinstance(published_date, str):
            return ""
        s = published_date.strip()
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
        # A missing key is a configuration error, not a transient failure, so we
        # raise with an actionable hint. The aggregator catches this into its
        # per-engine errors map, surfacing it cleanly to the caller.
        key = get_secret(_KEY_FIELD)
        if not key:
            raise ValueError(
                "tavily not configured: add tavily_api_key in the admin UI "
                "(run: uv run search-mcp-admin) or set SEARCH_MCP_TAVILY_API_KEY."
            )

        # Tavily caps max_results at 20 and rejects < 1; clamp defensively.
        clamped = max(1, min(max_results, _MAX_RESULTS))

        body: dict[str, Any] = {
            "query": query,
            "max_results": clamped,
            "search_depth": "basic",
            "include_answer": False,
            "include_raw_content": False,
        }
        # Tavily supports domain filters natively — forward them when present.
        if filters and filters.include_domains:
            body["include_domains"] = list(filters.include_domains)
        if filters and filters.exclude_domains:
            body["exclude_domains"] = list(filters.exclude_domains)

        results: list[SearchResult] = []
        try:
            async with AsyncSession(
                impersonate=_IMPERSONATE,
                timeout=settings.request_timeout,
                allow_redirects=True,
                headers={
                    # Tavily authenticates via a Bearer token header (the key is
                    # NOT a body field). Sending it in the body would 401.
                    "Authorization": f"Bearer {key}",
                    "Content-Type": "application/json",
                },
                **curl_proxy_kwargs(self.name),
            ) as client:
                resp = await client.post(_ENDPOINT, json=body)
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
