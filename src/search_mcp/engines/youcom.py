"""You.com web search, through the free profile of You.com's MCP server.

You.com documents one keyless way in: the MCP server at
``https://api.you.com/mcp?profile=free``, whose ``you-search`` tool allows 100
queries a day with no credentials. The REST search endpoints are paid per call
(an API key, or an HTTP 402 machine payment), so this engine does not use
them. Suggested in issue #23.

Request: one stateless JSON-RPC ``tools/call`` POST (no ``initialize`` needed,
measured 2026-10-18), answered as JSON or as a server-sent event stream:
  {"jsonrpc": "2.0", "id": 1, "method": "tools/call",
   "params": {"name": "you-search",
              "arguments": {"query", "count", "extraction": "none",
                            "freshness"?, "safesearch", "exclude_domains"?},
              "_meta": {"com.you.com/search": {"country"?, "language"?,
                                               "include_domains"?}}}}

The tool's text content is the search JSON:
  {"results": {"web": [...], "news": [...]}, "metadata": {...}}
with items carrying ``url``, ``title``, ``description``, ``snippets`` and an
optional ``page_age``.

Strategy:
  * Domain filters go out as the tool's own fields. It refuses
    ``exclude_domains`` together with ``include_domains`` (an inline ``site:``
    counts as one), so then only the includes are sent and the exclusions are
    left to the post-filter.
    A pdf category rides in the query as ``filetype:pdf``, which it honours.
  * Freshness, SafeSearch and ``SEARCH_MCP_REGION`` are forwarded.
  * Web and news results are both mapped. Only a news item's ``page_age`` is a
    publication time; on a web item it can be a crawl or update time, so it is
    shown but not trusted to drop a result as stale.
  * supports_browser_fallback is False: an empty JSON answer is really empty.

A non-200 returns ``[]`` and is recorded in ``diagnostics["http_status"]``, so
a refusal does not read as "nothing matched". An error the tool reports (a
spent daily quota, a rejected argument) is raised with its text, which the
aggregator shows in its errors map. A network error or malformed reply returns
``[]``.
"""

from __future__ import annotations

import json
from typing import Any

from curl_cffi.requests import AsyncSession
from curl_cffi.requests.exceptions import RequestException

from ..config import settings
from ..httpfetch import IMPERSONATE
from ..net import curl_proxy_kwargs
from .base import Engine, SearchFilters, SearchResult

_ENDPOINT = "https://api.you.com/mcp?profile=free"
_TOOL = "you-search"
_META_KEY = "com.you.com/search"

# The tool caps count at 100 per section; ask for no more than a page needs.
_COUNT_MAX = 50

# A SERP snippet is a teaser. The `snippets` fallback can be several long
# passages, so only the first is used, capped.
_SNIPPET_CAP = 400

# The tool rejects values outside these lists (from its tools/list schema,
# 2026-10-18), so a region it does not know is simply not sent.
_COUNTRIES = frozenset(
    {
        "AR", "AU", "AT", "BE", "BR", "CA", "CL", "DK", "FI", "FR", "DE", "HK", "IN", "ID",
        "IT", "JP", "KR", "MY", "MX", "NL", "NZ", "NO", "CN", "PL", "PT", "PH", "RU", "SA",
        "ZA", "ES", "SE", "CH", "TW", "TR", "GB", "US",
    }
)  # fmt: skip
_LANGUAGES = frozenset(
    {
        "AR", "EU", "BN", "BG", "CA", "ZH-HANS", "ZH-HANT", "HR", "CS", "DA", "NL", "EN",
        "EN-GB", "ET", "FI", "FR", "GL", "DE", "EL", "GU", "HE", "HI", "HU", "IS", "IT",
        "JA", "KN", "KO", "LV", "LT", "MS", "ML", "MR", "NB", "PL", "PT-BR", "PT-PT", "PA",
        "RO", "RU", "SR", "SK", "SL", "ES", "SV", "TA", "TE", "TH", "TR", "UK", "VI",
    }
)  # fmt: skip


def _region() -> dict[str, str]:
    """``settings.region`` ('us-en', 'uk-en', 'tw-zh') as the tool's country
    and language, leaving out whichever it would not accept."""
    cc, _, lang = (settings.region or "").lower().partition("-")
    country = {"uk": "GB"}.get(cc, cc.upper())
    language = lang.upper()
    if language == "ZH":
        language = "ZH-HANT" if country in {"TW", "HK"} else "ZH-HANS"
    elif language == "PT":
        language = "PT-BR" if country == "BR" else "PT-PT"
    elif language == "EN" and country == "GB":
        language = "EN-GB"
    out = {}
    if country in _COUNTRIES:
        out["country"] = country
    if language in _LANGUAGES:
        out["language"] = language
    return out


class YouComError(RuntimeError):
    """The you-search tool answered with an error (a spent quota, a bad argument)."""


class YouComEngine(Engine):
    """You.com web search on the keyless MCP profile."""

    name = "youcom"
    description = (
        "You.com's own web and news index, keyless; about 100 searches a day per IP."
    )
    needs_browser = False
    supports_browser_fallback = False

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        # The endpoint is constant; the query travels in the POST body.
        return _ENDPOINT

    def parse(self, html: str) -> list[SearchResult]:
        # Unused: search() is overridden. The ABC requires it.
        return []

    def _request(
        self, query: str, max_results: int, filters: SearchFilters | None
    ) -> dict[str, Any]:
        """The JSON-RPC body for one search."""
        q = query
        if filters and filters.category == "pdf":
            q = f"{q} filetype:pdf"
        args: dict[str, Any] = {
            "query": q,
            "count": max(1, min(max_results, _COUNT_MAX)),
            # Plain snippets: the default "highlights" adds passages we cap away.
            "extraction": "none",
            "safesearch": settings.safesearch,
        }
        if filters and filters.freshness:
            args["freshness"] = filters.freshness
        meta = _region()
        include = filters.include_domains if filters else None
        exclude = filters.exclude_domains if filters else None
        if include:
            meta["include_domains"] = list(include)
        elif exclude and "site:" not in q:
            # An inline site: counts as an include, which exclude_domains
            # cannot join.
            args["exclude_domains"] = list(exclude)
        return {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": _TOOL, "arguments": args, "_meta": {_META_KEY: meta}},
        }

    def _map_results(self, payload: Any) -> list[SearchResult]:
        """Map the search JSON into SearchResults. Never raises."""
        if not isinstance(payload, dict):
            return []
        groups = payload.get("results")
        if not isinstance(groups, dict):
            return []

        results: list[SearchResult] = []
        for section in ("web", "news"):
            items = groups.get(section)
            if not isinstance(items, list):
                continue
            for item in items:
                result = self._map_item(item, news=section == "news")
                if result is not None:
                    results.append(result)
        return results

    def _map_item(self, item: Any, *, news: bool) -> SearchResult | None:
        if not isinstance(item, dict):
            return None
        title = _text(item.get("title"))
        url = _text(item.get("url"))
        if not title or not url.startswith(("http://", "https://")):
            return None
        snippet = _text(item.get("description"))
        if not snippet:
            snippets = item.get("snippets")
            if isinstance(snippets, list) and snippets:
                snippet = _text(snippets[0])
        if len(snippet) > _SNIPPET_CAP:
            snippet = snippet[:_SNIPPET_CAP].rstrip() + " …"
        page_age = _text(item.get("page_age"))
        published_age = page_age.split("T", 1)[0] if page_age else ""
        return SearchResult(
            title=title,
            url=url,
            snippet=snippet,
            engine=self.name,
            rank=0,
            published_age=published_age,
            published_age_confident=news and bool(published_age),
        )

    async def search(
        self,
        query: str,
        max_results: int,
        filters: SearchFilters | None = None,
        diagnostics: dict[str, Any] | None = None,
    ) -> list[SearchResult]:
        payload: Any = None
        try:
            async with AsyncSession(
                impersonate=IMPERSONATE,
                timeout=settings.request_timeout,
                allow_redirects=True,
                headers={
                    "Content-Type": "application/json",
                    "Accept": "application/json, text/event-stream",
                },
                **curl_proxy_kwargs(self.name),
            ) as client:
                resp = await client.post(
                    _ENDPOINT, json=self._request(query, max_results, filters)
                )
                if resp.status_code == 200:
                    payload = _tool_payload(resp.text)
                elif diagnostics is not None:
                    diagnostics.setdefault("http_status", {})[self.name] = resp.status_code
        except YouComError:
            raise
        except RequestException:
            payload = None
        except Exception:
            payload = None

        results = self._map_results(payload)
        # search() is overridden, so the shared tail (post-filters, diagnostics,
        # ranks) has to be applied here.
        return self.finalize_results(results, filters, max_results, diagnostics)


def _rpc_reply(body: str) -> dict[str, Any] | None:
    """The JSON-RPC reply to our request, from a JSON body or an event stream.
    Notifications in the stream are skipped."""
    lines = [body] if body.lstrip().startswith("{") else [
        line[5:].strip() for line in body.splitlines() if line.startswith("data:")
    ]
    for line in lines:
        try:
            message = json.loads(line)
        except ValueError:
            continue
        if isinstance(message, dict) and ("result" in message or "error" in message):
            return message
    return None


def _tool_payload(body: str) -> Any:
    """The search JSON inside a tools/call reply, or None. Raises YouComError
    when the server or the tool reports an error."""
    reply = _rpc_reply(body)
    if reply is None:
        return None
    if reply.get("error"):
        error = reply["error"]
        message = error.get("message") if isinstance(error, dict) else error
        raise YouComError(f"youcom: {_text(str(message))[:300]}")
    result = reply.get("result")
    if not isinstance(result, dict):
        return None
    content = result.get("content")
    texts = [
        block.get("text", "")
        for block in content if isinstance(block, dict) and block.get("type") == "text"
    ] if isinstance(content, list) else []
    if result.get("isError"):
        raise YouComError(f"youcom: {' '.join(_text(t) for t in texts)[:300]}")
    for text in texts:
        try:
            return json.loads(text)
        except ValueError:
            continue
    return None


def _text(value: Any) -> str:
    return value.strip() if isinstance(value, str) else ""
