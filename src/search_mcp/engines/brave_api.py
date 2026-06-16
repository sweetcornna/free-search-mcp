"""Brave Search API engine (OFFICIAL keyed API).

This is the *official* Brave Search REST API — NOT the keyless ``brave``
scrape engine that hits search.brave.com. It requires a subscription token
(field ``brave_api_key``), sent in the ``X-Subscription-Token`` header.

Get a key:
  1. Open https://brave.com/search/api/ and click "Get started".
  2. Sign up / log in, verify your email.
  3. Subscribe to the free "Data for Search" plan (2,000 queries/month free).
  4. Dashboard -> API Keys -> copy the subscription token.
  Paste it in the admin UI (run: uv run search-mcp-admin) or export
  SEARCH_MCP_BRAVE_API_KEY.

Request:
  GET https://api.search.brave.com/res/v1/web/search
  Headers: Accept: application/json, Accept-Encoding: gzip,
           X-Subscription-Token: <key>
  Query params: q, count (1..20), country, safesearch, freshness.

Response (200) JSON:
  {"web": {"results": [{"title", "url", "description",
                        "page_age"(ISO datetime, optional),
                        "age"(str, optional)}]}}
  Some payloads omit "web" entirely -> we return [].

Strategy:
  * This is a JSON GET API, so we OVERRIDE search() (the base class only
    knows how to GET + parse an HTML page) and mirror the anysearch.py /
    searx.py override idioms.
  * A REQUIRED missing key RAISES ValueError (the aggregator catches engine
    exceptions into an errors map, so the hint surfaces cleanly). Any other
    failure (http/network/parse) returns [] so a flaky API never poisons the
    aggregator.
  * description carries <strong> highlight tags around matched terms; we
    strip them. published_age is the YYYY-MM-DD portion of page_age when
    present, else a date hint extracted from age/description.
  * The subscription token is never logged, printed, or placed in build_url
    (the cache key) — it only travels in the request header.
"""

from __future__ import annotations

import re
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

_ENDPOINT = "https://api.search.brave.com/res/v1/web/search"

# The provider caps count at 20 and rejects < 1; clamp defensively.
_MAX_COUNT = 20

# safesearch maps 1:1 to our vocabulary.
_SAFESEARCH = {"strict": "strict", "moderate": "moderate", "off": "off"}

# Our freshness vocabulary -> Brave's "freshness" param codes.
_FRESHNESS = {"day": "pd", "week": "pw", "month": "pm", "year": "py"}

# Strip <strong>/<em> (and their closing forms) highlight tags Brave wraps
# around matched terms in titles/snippets. Tolerant of attributes.
_TAG_RE = re.compile(r"</?(?:strong|em)\b[^>]*>", re.I)


def _strip_tags(text: str) -> str:
    """Remove <strong>/<em> highlight tags and collapse whitespace. Never
    raises; a non-str yields ""."""
    if not isinstance(text, str):
        return ""
    return " ".join(_TAG_RE.sub("", text).split())


class BraveApiEngine(Engine):
    """Brave Search API — official, keyed (X-Subscription-Token), JSON GET."""

    name = "brave_api"
    needs_browser = False
    # JSON API: an empty/malformed response is genuinely empty, so don't waste
    # a Playwright render trying to "recover" it (see Engine.search fallback).
    supports_browser_fallback = False

    # Kept so the abstract contract is satisfied and so callers that compute a
    # cache key from build_url() get a stable string. The secret travels in the
    # request header, never in the URL.
    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        q = self._augmented_query(query, filters)
        return f"{_ENDPOINT}?q={quote_plus(q)}"

    def parse(self, html: str) -> list[SearchResult]:
        # Unused on the GET/JSON path (search() is overridden), but the ABC
        # requires it. Never raises: an HTML/JSON blob arriving here can't be
        # mapped to Brave API results, so we simply yield nothing.
        return []

    @staticmethod
    def _augmented_query(query: str, filters: SearchFilters | None) -> str:
        filetype = None
        if filters and filters.category == "pdf":
            filetype = "pdf"
        return augment_query_with_operators(
            query,
            include_domains=filters.include_domains if filters else None,
            exclude_domains=filters.exclude_domains if filters else None,
            filetype=filetype,
        )

    @staticmethod
    def _country() -> str:
        """settings.region is a 'cc-lang' token ('us-en'); Brave wants the
        2-letter country uppercased ('US'). Fall back to 'US' on bad input."""
        region = settings.region or ""
        cc = region.split("-", 1)[0].strip().upper()
        return cc or "US"

    def _map_results(self, payload: Any) -> list[SearchResult]:
        """Map a Brave API JSON payload into SearchResults. Never raises: any
        structural surprise yields ``[]`` so a malformed response can't poison
        the engine. Payloads that omit ``web`` are genuinely empty."""
        if not isinstance(payload, dict):
            return []
        web = payload.get("web")
        if not isinstance(web, dict):
            return []
        items = web.get("results")
        if not isinstance(items, list):
            return []

        results: list[SearchResult] = []
        for item in items:
            if not isinstance(item, dict):
                continue
            title = _strip_tags(item.get("title") or "")
            url = (item.get("url") or "").strip()
            if not title or not url:
                continue
            snippet = _strip_tags(item.get("description") or "")
            published_age = self._published_age(item)
            results.append(
                SearchResult(
                    title=title,
                    url=url,
                    snippet=snippet,
                    engine=self.name,
                    rank=0,
                    published_age=published_age,
                    # page_age/age are structured API date fields.
                    published_age_confident=bool(published_age),
                )
            )
        return results

    @staticmethod
    def _published_age(item: dict[str, Any]) -> str:
        """Prefer the ISO ``page_age`` (-> YYYY-MM-DD); otherwise derive a hint
        from the free-text ``age`` ("3 days ago") or the description."""
        page_age = item.get("page_age")
        if isinstance(page_age, str) and page_age.strip():
            # "2024-02-06T00:00:00Z" -> "2024-02-06"; tolerate a bare date too.
            return page_age.strip().split("T", 1)[0]
        age = item.get("age")
        if isinstance(age, str) and age.strip():
            hint = extract_date_hint(age)
            if hint:
                return hint
        description = item.get("description")
        if isinstance(description, str) and description:
            return extract_date_hint(description)
        return ""

    async def search(
        self,
        query: str,
        max_results: int,
        filters: SearchFilters | None = None,
        diagnostics: dict[str, Any] | None = None,
    ) -> list[SearchResult]:
        key = get_secret("brave_api_key")
        if not key:
            raise ValueError(
                "brave_api not configured: add brave_api_key in the admin UI "
                "(run: uv run search-mcp-admin) or set SEARCH_MCP_BRAVE_API_KEY."
            )

        q = self._augmented_query(query, filters)
        params: dict[str, Any] = {
            "q": q,
            "count": max(1, min(max_results, _MAX_COUNT)),
            "country": self._country(),
        }
        safesearch = _SAFESEARCH.get(settings.safesearch)
        if safesearch is not None:
            params["safesearch"] = safesearch
        if filters and filters.freshness:
            fresh = _FRESHNESS.get(filters.freshness)
            if fresh is not None:
                params["freshness"] = fresh

        results: list[SearchResult] = []
        status_code: int | None = None
        try:
            async with AsyncSession(
                impersonate=_IMPERSONATE,
                timeout=settings.request_timeout,
                allow_redirects=True,
                headers={
                    "Accept": "application/json",
                    "Accept-Encoding": "gzip",
                    "X-Subscription-Token": key,
                },
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
