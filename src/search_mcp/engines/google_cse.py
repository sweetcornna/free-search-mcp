"""Google Custom Search JSON API engine (keyed).

Hits the Google Custom Search JSON API
(GET https://www.googleapis.com/customsearch/v1). This is a KEYED engine: it
needs TWO config fields, both obtained from the Google Cloud / Programmable
Search consoles:

  * ``google_cse_api_key`` — a Google Cloud API key with the "Custom Search API"
    enabled. Create one at https://console.cloud.google.com/apis/credentials and
    enable the API at
    https://console.cloud.google.com/apis/library/customsearch.googleapis.com.
  * ``google_cse_cx`` — the "Search engine ID" (cx) of a Programmable Search
    Engine configured to "Search the entire web". Create it at
    https://programmablesearchengine.google.com/ and copy the cx from its
    control panel.

Set both via the admin UI (run: ``uv run search-mcp-admin``) or the env vars
``SEARCH_MCP_GOOGLE_CSE_API_KEY`` / ``SEARCH_MCP_GOOGLE_CSE_CX``.

Request:
  GET https://www.googleapis.com/customsearch/v1
  params: key, cx, q, num (1..10 — CSE hard cap), hl, gl, safe, dateRestrict

Response (200) JSON:
  {"items": [{"title", "link", "snippet", "displayLink", ...}]}
  The ``items`` key is OMITTED entirely when there are zero results -> [].

Strategy:
  * This is a JSON GET API, so we OVERRIDE search() (the base class only knows
    how to GET an HTML page) and mirror the anysearch.py / searx.py idioms.
  * num is clamped to 1..10 because CSE rejects num > 10.
  * published_age is derived from the snippet via base.extract_date_hint, since
    CSE does not return a structured publish date in the basic response.
  * supports_browser_fallback is False — a JSON API that returns nothing or
    malformed data is genuinely empty, so a headless re-render is pointless.

Key handling: a MISSING required key RAISES ValueError (the aggregator turns
that into an actionable errors-map entry). Any http/network/parse failure
returns ``[]`` rather than raising, so a flaky endpoint never poisons the
aggregator. The key/cx are never logged.
"""

from __future__ import annotations

from typing import Any
from urllib.parse import quote_plus

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
    raise_for_key_error,
)


# Pinned at chrome131 to match the rest of the project — keeps the JA3/JA4 +
# HTTP/2 fingerprint consistent with the desktop UA we present elsewhere.
_IMPERSONATE = "chrome131"

_ENDPOINT = "https://www.googleapis.com/customsearch/v1"

# CSE hard cap: ``num`` must be in 1..10 or the API rejects the request.
_MAX_NUM = 10

# Our freshness vocabulary -> the CSE ``dateRestrict`` token.
_DATE_RESTRICT = {"day": "d1", "week": "w1", "month": "m1", "year": "y1"}


class GoogleCSEEngine(Engine):
    """Google Custom Search JSON API — keyed (api_key + cx), JSON GET."""

    name = "google_cse"
    needs_browser = False
    # JSON API: an empty/malformed response is genuinely empty, so don't waste
    # a Playwright render trying to "recover" it (see Engine.search fallback).
    supports_browser_fallback = False

    # Kept so the abstract contract is satisfied and so callers that compute a
    # cache key from build_url() get a stable string. No secret appears in it:
    # the key/cx travel as query params at request time, not in the cache key.
    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        return f"{_ENDPOINT}?q={quote_plus(query)}"

    def parse(self, html: str) -> list[SearchResult]:
        # Unused on the GET/JSON path (search() is overridden), but the ABC
        # requires it. Never raises: an HTML/JSON blob arriving here can't be
        # mapped to CSE results, so we simply yield nothing.
        return []

    def _map_results(self, payload: Any) -> list[SearchResult]:
        """Map a CSE JSON payload into SearchResults. Never raises: any
        structural surprise yields ``[]`` so a malformed response can't poison
        the engine. The ``items`` key is absent on zero results -> []."""
        if not isinstance(payload, dict):
            return []
        items = payload.get("items")
        if not isinstance(items, list):
            return []

        results: list[SearchResult] = []
        for item in items:
            if not isinstance(item, dict):
                continue
            title = self._strip_highlights(item.get("title"))
            url = (item.get("link") or "").strip()
            if not title or not url:
                continue
            snippet = self._strip_highlights(item.get("snippet"))
            published_age = extract_date_hint(snippet)
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
    def _strip_highlights(text: Any) -> str:
        """Strip <strong>/<em> highlight tags CSE sprinkles into title/snippet
        and collapse whitespace. Non-strings / missing -> ""."""
        if not isinstance(text, str):
            return ""
        s = (
            text.replace("<strong>", "")
            .replace("</strong>", "")
            .replace("<em>", "")
            .replace("</em>", "")
            .replace("<b>", "")
            .replace("</b>", "")
        )
        return " ".join(s.split())

    async def search(
        self,
        query: str,
        max_results: int,
        filters: SearchFilters | None = None,
        diagnostics: dict[str, Any] | None = None,
    ) -> list[SearchResult]:
        # Two required fields — fail loud (the aggregator surfaces this as an
        # actionable errors-map entry) rather than silently returning nothing.
        key = get_secret("google_cse_api_key")
        cx = get_secret("google_cse_cx")
        if not key or not cx:
            raise ValueError(
                "google_cse not configured: add google_cse_api_key AND "
                "google_cse_cx in the admin UI (uv run search-mcp-admin) or set "
                "SEARCH_MCP_GOOGLE_CSE_API_KEY / SEARCH_MCP_GOOGLE_CSE_CX."
            )

        # CSE rejects num > 10 (and < 1); clamp defensively.
        num = max(1, min(max_results, _MAX_NUM))

        # Fold include/exclude domains + a pdf filetype into the query as
        # operators — universally understood, and CSE has no dedicated params.
        filetype = "pdf" if (filters and filters.category == "pdf") else None
        q = augment_query_with_operators(
            query,
            include_domains=filters.include_domains if filters else None,
            exclude_domains=filters.exclude_domains if filters else None,
            filetype=filetype,
        )

        params: dict[str, str | int] = {
            "key": key,
            "cx": cx,
            "q": q,
            "num": num,
            "hl": "en",
            "gl": "us",
            "safe": "active"
            if settings.safesearch in ("strict", "moderate")
            else "off",
        }
        if filters and filters.freshness:
            restrict = _DATE_RESTRICT.get(filters.freshness)
            if restrict:
                params["dateRestrict"] = restrict

        results: list[SearchResult] = []
        status_code: int | None = None
        try:
            async with AsyncSession(
                impersonate=_IMPERSONATE,
                timeout=settings.request_timeout,
                allow_redirects=True,
                headers={"Accept": "application/json"},
                **curl_proxy_kwargs(self.name),
            ) as client:
                resp = await client.get(_ENDPOINT, params=params)
                status_code = resp.status_code
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

        # A configured-but-rejected key (401/403/422) or a quota hit (429) raises
        # an actionable error instead of returning a confusing silent empty.
        if not results:
            raise_for_key_error(self.name, status_code)

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
