"""Shared plumbing for keyless JSON / feed API engines.

The base `Engine` knows how to GET an HTML page and parse it. A growing number
of sources are instead plain JSON (or Atom/RSS) APIs, and each one was
re-implementing the same three things: a curl_cffi session with the shared
impersonation and proxy settings, a swallow-everything error boundary, and the
`finalize_results` tail contract.

`JsonApiEngine` owns all three, so a new source is usually just:

    class MyEngine(JsonApiEngine):
        name = "mine"
        categories = frozenset({"paper"})

        def build_url(self, query, max_results, filters=None) -> str:
            return f"https://api.example/search?q={quote_plus(query)}"

        def map_results(self, payload) -> list[SearchResult]:
            ...

Sources needing more than one round trip (PubMed) or a non-JSON body (arXiv's
Atom feed) override `fetch_results` instead.

House rules this enforces for every subclass:
  * **Never raise.** A keyless source that is down, rate-limited, or returning
    nonsense yields `[]`; only the aggregator decides what an empty result
    means. One flaky endpoint must never fail the whole search.
  * **No browser fallback.** An API that returned nothing returned nothing; a
    headless re-render just burns ~1s to get the same empty list.
"""

from __future__ import annotations

import logging
from typing import Any

from curl_cffi.requests import AsyncSession

from ..config import settings
from ..httpfetch import IMPERSONATE
from ..net import curl_proxy_kwargs
from .base import Engine, EngineKeyError, SearchFilters, SearchResult

log = logging.getLogger(__name__)

# A SERP snippet is a teaser, not the page body. Sources that return full
# abstracts (arXiv, Crossref) would otherwise dump multiple KB per result.
SNIPPET_CAP = 400

# Sent instead of a browser fingerprint when `impersonate` is None. Honest and
# contactable, which is what an API operator wants to see in their logs.
USER_AGENT = (
    "free-search-mcp/1.0 (+https://github.com/sweetcornna/free-search-mcp)"
)


def clip(text: Any, cap: int = SNIPPET_CAP) -> str:
    """Normalize any value to a single-line snippet of at most `cap` chars."""
    if not isinstance(text, str):
        return ""
    s = " ".join(text.split())
    if len(s) > cap:
        return s[:cap].rstrip() + " …"
    return s


def iso_date(value: Any) -> str:
    """`YYYY-MM-DD` prefix of an ISO timestamp, or "" if there isn't one.

    Feeding `published_age` a real date is what lets freshness filtering
    actually drop stale hits (see `published_age_confident`).
    """
    if not isinstance(value, str):
        return ""
    s = value.strip()
    if not s:
        return ""
    return s.split("T", 1)[0]


class JsonApiEngine(Engine):
    """Keyless JSON/feed API engine: no browser, no raising, JSON in, results out."""

    needs_browser = False
    supports_browser_fallback = False

    #: Extra request headers. Sources with a usage policy (OpenAlex, Crossref,
    #: NCBI) identify the client here rather than hiding behind the browser
    #: impersonation the HTML scrapers need.
    api_headers: dict[str, str] = {}

    #: TLS/header fingerprint. HTML scrapers need to look like Chrome to get
    #: past bot walls; some JSON APIs do the opposite and reject clients that
    #: claim to be a browser (Zenodo answers 403). Set to None to send an
    #: honest client identifier instead.
    impersonate: str | None = IMPERSONATE

    def parse(self, html: str) -> list[SearchResult]:
        # Required by the ABC but unused: `search()` is overridden and never
        # routes a body through here. Returning [] keeps the never-raise rule
        # if some caller does reach it.
        return []

    # -- transport ---------------------------------------------------------

    async def _request_text(
        self,
        url: str,
        *,
        method: str = "GET",
        json_body: Any | None = None,
        headers: dict[str, str] | None = None,
    ) -> str | None:
        """Return the response body, or None on any failure.

        None means "no answer" and is deliberately indistinguishable from a
        timeout, a 429, or a 500 — callers map all of them to no results.
        """
        merged = {**self.api_headers, **(headers or {})}
        session_kwargs: dict[str, Any] = {}
        if self.impersonate is not None:
            session_kwargs["impersonate"] = self.impersonate
        else:
            merged.setdefault("User-Agent", USER_AGENT)
        try:
            async with AsyncSession(
                timeout=settings.request_timeout,
                allow_redirects=True,
                headers=merged or None,
                **session_kwargs,
                **curl_proxy_kwargs(self.name),
            ) as client:
                if method == "POST":
                    resp = await client.post(url, json=json_body)
                else:
                    resp = await client.get(url)
                if resp.status_code != 200:
                    log.debug("%s: HTTP %s from %s", self.name, resp.status_code, url)
                    return None
                return resp.text
        except Exception:
            log.debug("%s: request failed for %s", self.name, url, exc_info=True)
            return None

    async def _get_json(
        self,
        url: str,
        *,
        method: str = "GET",
        json_body: Any | None = None,
        headers: dict[str, str] | None = None,
    ) -> Any | None:
        """Fetch and JSON-decode. None on transport failure or malformed JSON.

        Sends `Accept: application/json` explicitly. The shared curl_cffi
        session impersonates Chrome, so without this it advertises
        `Accept: text/html,...` — and any API that does content negotiation
        (Django REST Framework's browsable API, for one) answers 200 with an
        HTML page, which then fails to parse as "malformed JSON".
        """
        import json as _json

        merged = {"Accept": "application/json", **(headers or {})}
        body = await self._request_text(
            url, method=method, json_body=json_body, headers=merged
        )
        if body is None:
            return None
        try:
            return _json.loads(body)
        except Exception:
            log.debug("%s: malformed JSON from %s", self.name, url)
            return None

    # -- subclass contract -------------------------------------------------

    def map_results(self, payload: Any) -> list[SearchResult]:
        """Turn a decoded payload into results. Must not raise."""
        raise NotImplementedError

    async def fetch_results(
        self, query: str, max_results: int, filters: SearchFilters | None
    ) -> list[SearchResult]:
        """One GET against `build_url`, mapped by `map_results`.

        Override for multi-request sources or non-JSON bodies.
        """
        payload = await self._get_json(self.build_url(query, max_results, filters))
        if payload is None:
            return []
        return self.map_results(payload)

    async def search(
        self,
        query: str,
        max_results: int,
        filters: SearchFilters | None = None,
        diagnostics: dict[str, Any] | None = None,
    ) -> list[SearchResult]:
        try:
            results = await self.fetch_results(query, max_results, filters)
        except EngineKeyError:
            # Deliberately NOT swallowed. A missing or rejected key is a
            # configuration problem the user can fix, and reporting it as "no
            # results" would read as "nothing matched". The aggregator turns
            # this into a per-engine error hint.
            raise
        except Exception:
            # The never-raise boundary. Subclass mapping bugs and surprise
            # payload shapes both land here and degrade to "this source had
            # nothing", which the aggregator already knows how to report.
            log.debug("%s: search failed", self.name, exc_info=True)
            results = []
        return self.finalize_results(results, filters, max_results, diagnostics)
