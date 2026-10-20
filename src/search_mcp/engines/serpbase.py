"""SerpBase (serpbase.dev) — Google Web Search via a hosted REST API.

SerpBase is a hosted search-and-data API: one key covers 39 endpoints across
Google/DuckDuckGo search, Maps, the big storefronts (Amazon, Walmart, eBay,
Flipkart, Myntra, Ajio, Apple), social (Reddit, X, TikTok, YouTube) and plain
web scrape/crawl. This engine uses the Google Web Search product only, so the
package still gets structured JSON without scraping, CAPTCHAs or proxies.

This is a KEYED engine: it requires a ``serpbase_api_key`` secret (read via the
keystore, env ``SEARCH_MCP_SERPBASE_API_KEY`` or the admin UI).

Get a key: sign up at https://serpbase.dev (100 free credits, no credit card)
and create a key at https://serpbase.dev/api-keys.

Request:
  POST https://api.serpbase.dev/google/search
  Headers: {"X-API-Key": <key>, "Content-Type": "application/json"}
  body: {"q": <augmented query>, "num": <n clamped 1..100>, "gl": "us",
         "hl": "en"}

Response (200) JSON:
  {"status": 0, "request_id": "...", "elapsed_ms": 812, "credits_charged": 1,
   "organic": [{"title", "link", "snippet", "position"}], ...}
  Map: title, url=link, snippet, published_age="" (not provided by this path).

Strategy:
  * JSON POST API, so we OVERRIDE search() (the base class only knows how to GET
    an HTML page) and mirror the serper.py / anysearch.py override idioms.
  * Domain/filetype constraints ride in the query via
    base.augment_query_with_operators (Google understands site:/-site:/filetype:).
  * supports_browser_fallback is False — a JSON API that returns nothing or
    malformed data is genuinely empty, so a headless re-render is pointless.

Key handling — the one thing that differs from other keyed engines:
  * SerpBase answers **HTTP 200 for failures too**. Success is the *envelope*
    field ``status == 0``; a rejected key is a 200 whose body says
    ``{"status": 1001, ...}``. So the HTTP status can't be trusted for auth
    errors and the envelope code is mapped here (``_raise_for_envelope``).
  * A MISSING key raises via ``key_not_configured`` (the aggregator surfaces it
    in its errors map).
  * Auth/quota envelope codes raise an actionable error; transient ones
    (1500/1502/1503/1504) return ``[]`` so a flaky endpoint never poisons the
    aggregator. The key is never logged.
"""

from __future__ import annotations

from typing import Any

from curl_cffi.requests import AsyncSession
from curl_cffi.requests.exceptions import RequestException

from ..config import settings
from ..httpfetch import IMPERSONATE
from ..keystore import get_secret
from ..net import curl_proxy_kwargs
from .base import (
    Engine,
    EngineKeyError,
    SearchFilters,
    SearchResult,
    augment_query_with_operators,
    key_not_configured,
    raise_for_key_error,
)

_ENDPOINT = "https://api.serpbase.dev/google/search"

# SerpBase returns up to 100 organic results per call; we clamp to a reasonable
# page size to keep latency low. Credits are billed per request (1 for Google
# Web Search), not per result, so the page size does not change the cost.
_NUM_MIN = 1
_NUM_MAX = 100

# Envelope codes (not HTTP statuses) that mean "this credential cannot work".
_AUTH_ENVELOPE = 1001       # UNAUTHORIZED — key missing, revoked or mistyped
_FORBIDDEN_ENVELOPE = 1003  # FORBIDDEN — e.g. product paused or plan gate
_QUOTA_ENVELOPE = 1020      # INSUFFICIENT_CREDITS
_RATE_ENVELOPE = 1029       # RATE_LIMITED — safe to retry later


class SerpBaseEngine(Engine):
    """SerpBase (serpbase.dev) Google Web Search — keyed, JSON POST."""

    name = "serpbase"
    description = "SerpBase: Google results through a hosted search-and-data API."
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
        # mapped to SerpBase results, so we simply yield nothing.
        return []

    def _map_results(self, payload: Any) -> list[SearchResult]:
        """Map a SerpBase payload into SearchResults. Never raises: any
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
            raise key_not_configured(
                "serpbase",
                alternative=(
                    "omit `engines=` to use the default keyless pool, which already covers "
                    "the web."
                ),
                env="SEARCH_MCP_SERPBASE_API_KEY",
                field="serpbase_api_key",
            )

        # Push domain/filetype constraints into the query via Google operators —
        # SerpBase relays them straight to Google.
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

        results: list[SearchResult] = []
        status_code: int | None = None
        try:
            async with AsyncSession(
                impersonate=IMPERSONATE,
                timeout=settings.request_timeout,
                allow_redirects=True,
                headers={"X-API-Key": key, "Content-Type": "application/json"},
                **curl_proxy_kwargs(self.name),
            ) as client:
                resp = await client.post(_ENDPOINT, json=body)
                status_code = resp.status_code
                if resp.status_code == 200:
                    try:
                        payload = resp.json()
                    except Exception:
                        # Malformed JSON: never-raise contract -> empty.
                        payload = None
                    if payload is not None:
                        # HTTP 200 is not success here; the envelope decides.
                        _raise_for_envelope(self.name, payload)
                        results = self._map_results(payload)
        except EngineKeyError:
            # The envelope already said exactly what is wrong (bad key, no
            # credits, rate limited) — let it through untouched.
            raise
        except RequestException:
            results = []
        except Exception:
            results = []

        # A configured key rejected at the transport layer (401/403/422/429)
        # raises an actionable error instead of a confusing silent empty.
        if not results:
            raise_for_key_error(self.name, status_code)

        # We override search(), so the shared tail contract (post-filter +
        # diagnostics + rank) must be applied explicitly — see
        # Engine.finalize_results.
        return self.finalize_results(results, filters, max_results, diagnostics)


def _raise_for_envelope(engine: str, payload: Any) -> None:
    """Map SerpBase's business ``status`` onto an actionable error.

    SerpBase returns HTTP 200 for failures, so an auth problem looks like
    ``{"status": 1001}`` rather than a 401. Only auth/quota codes raise:
    transient codes (1500 internal, 1502 upstream failed, 1503 unavailable,
    1504 timeout) stay silent so a flaky API never poisons the aggregator, and
    failed requests are not billed.
    """
    if not isinstance(payload, dict):
        return
    code = payload.get("status")
    if not isinstance(code, int) or code == 0:
        return
    if code == _AUTH_ENVELOPE:
        raise EngineKeyError(
            f"{engine}: the configured credential was rejected (status 1001). It may be "
            "expired, revoked or mistyped. This affects only this opt-in engine; the keyless "
            "engines are unaffected, so continue with them. (Operator note: check the "
            "value in `search-mcp-admin` or the SEARCH_MCP_SERPBASE_API_KEY environment "
            "variable.)"
        )
    if code == _FORBIDDEN_ENVELOPE:
        raise EngineKeyError(
            f"{engine}: the API refused this request (status 1003) — the key's plan does not "
            "cover it, or the product is paused. The keyless engines are unaffected."
        )
    if code == _QUOTA_ENVELOPE:
        raise EngineKeyError(
            f"{engine}: out of credits (status 1020). Credits are prepaid and never expire, so "
            "top up at https://serpbase.dev/pricing — or continue with the keyless engines, "
            "which have no quota."
        )
    if code == _RATE_ENVELOPE:
        raise EngineKeyError(
            f"{engine}: rate limit reached (status 1029). Retry later, or continue with the "
            "keyless engines, which are not subject to this limit."
        )


def _clean(value: Any) -> str:
    """Trim a field and strip <strong>/<em> highlight tags Google sometimes
    wraps matched terms in. Tolerant of non-strings (-> "")."""
    if not isinstance(value, str):
        return ""
    s = value.strip()
    for tag in ("<strong>", "</strong>", "<em>", "</em>", "<b>", "</b>"):
        s = s.replace(tag, "")
    return s.strip()
