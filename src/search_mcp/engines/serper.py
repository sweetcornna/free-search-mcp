"""Serper (serper.dev) — Google SERP via API key.

Serper proxies Google's search results behind a simple JSON API. This is a
KEYED engine: it requires a ``serper_api_key`` secret (read via the keystore,
env ``SEARCH_MCP_SERPER_API_KEY`` or the admin UI).

Get a key: sign up at https://serper.dev (2,500 free credits one-time) and copy
the API key shown on the dashboard.

Request:
  POST https://google.serper.dev/search
  Headers: {"X-API-KEY": <key>, "Content-Type": "application/json"}
  body: {"q": <augmented query>, "num": <n clamped 10..20>, "gl": "us",
         "hl": "en"[, "tbs": "qdr:d|w|m|y" when freshness is set]}

Response (200) JSON:
  {"organic": [{"title", "link", "snippet", "date"(optional), "position"}], ...}
  Map: title, url=link, snippet, published_age=extract_date_hint(date) or "".

Strategy:
  * JSON POST API, so we OVERRIDE search() (the base class only knows how to GET
    an HTML page) and mirror the anysearch.py / searx.py override idioms.
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
    augment_query_with_operators,
    extract_date_hint,
)


# Pinned at chrome131 to match the rest of the project — keeps the JA3/JA4 +
# HTTP/2 fingerprint consistent with the desktop UA we present elsewhere.
_IMPERSONATE = "chrome131"

_ENDPOINT = "https://google.serper.dev/search"

# Serper's ``num`` (results per page) — clamp into its sane page-size band.
_NUM_MIN = 10
_NUM_MAX = 20

# Our freshness vocabulary -> Google's ``tbs=qdr:*`` recency operator.
_FRESHNESS_QDR = {"day": "d", "week": "w", "month": "m", "year": "y"}


class SerperEngine(Engine):
    """Serper (serper.dev) Google SERP — keyed, JSON POST."""

    name = "serper"
    needs_browser = False
    # JSON API: an empty/malformed response is genuinely empty, so don't waste
    # a Playwright render trying to "recover" it (see Engine.search fallback).
    supports_browser_fallback = False

    # Kept so the abstract contract is satisfied and so callers that compute a
    # cache key from build_url() get a stable string. The endpoint is constant;
    # the query/filters travel in the POST body, not the URL. No secret here.
    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        return _ENDPOINT

    def parse(self, html: str) -> list[SearchResult]:
        # Unused on the POST/JSON path (search() is overridden), but the ABC
        # requires it. Never raises: an HTML/JSON blob arriving here can't be
        # mapped to Serper results, so we simply yield nothing.
        return []

    def _map_results(self, payload: Any) -> list[SearchResult]:
        """Map a Serper JSON payload into SearchResults. Never raises: any
        structural surprise yields ``[]`` so a malformed response can't poison
        the engine."""
        if not isinstance(payload, dict):
            return []
        items = payload.get("organic")
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
            # ``date`` is free text ("2 days ago", "Jan 5, 2024") when present.
            published_age = extract_date_hint(_clean(item.get("date")))
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

    async def search(
        self,
        query: str,
        max_results: int,
        filters: SearchFilters | None = None,
        diagnostics: dict[str, Any] | None = None,
    ) -> list[SearchResult]:
        key = get_secret("serper_api_key")
        if not key:
            raise ValueError(
                "serper not configured: add serper_api_key in the admin UI "
                "(run: uv run search-mcp-admin) or set SEARCH_MCP_SERPER_API_KEY."
            )

        # Push domain/filetype constraints into the query via Google operators —
        # Serper relays them straight to Google.
        filetype = "pdf" if filters and filters.category == "pdf" else None
        q = augment_query_with_operators(
            query,
            include_domains=filters.include_domains if filters else None,
            exclude_domains=filters.exclude_domains if filters else None,
            filetype=filetype,
        )
        body: dict[str, Any] = {
            "q": q,
            "num": max(_NUM_MIN, min(max_results, _NUM_MAX)),
            "gl": "us",
            "hl": "en",
        }
        if filters and filters.freshness:
            qdr = _FRESHNESS_QDR.get(filters.freshness)
            if qdr:
                body["tbs"] = "qdr:" + qdr

        results: list[SearchResult] = []
        try:
            async with AsyncSession(
                impersonate=_IMPERSONATE,
                timeout=settings.request_timeout,
                allow_redirects=True,
                headers={"X-API-KEY": key, "Content-Type": "application/json"},
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


def _clean(value: Any) -> str:
    """Trim a field and strip <strong>/<em> highlight tags Google sometimes
    wraps matched terms in. Tolerant of non-strings (-> "")."""
    if not isinstance(value, str):
        return ""
    s = value.strip()
    for tag in ("<strong>", "</strong>", "<em>", "</em>"):
        s = s.replace(tag, "")
    return s.strip()
