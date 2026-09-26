"""Google Search through a Gemini model, on the operator's Antigravity sign-in (OPT-IN).

Google's Antigravity IDE signs in with a Google account and runs Gemini models
through the Cloud Code backend (``cloudcode-pa.googleapis.com``), where a
model can run Google Search as a tool. This engine does the same with the same
sign-in, so the searches draw on the account's Antigravity quota.

Google does not allow it. The Antigravity terms call any third-party use of
Antigravity OAuth a breach and name suspension of the Antigravity and Gemini
CLI accounts as the consequence, and the backend licenses only requests that
identify as Antigravity, so this engine sends Antigravity's user agent. It
exists because an operator asked for it on their own account. It is enabled
only by ``search-mcp-login antigravity`` (or the settings page), which says
this before it starts, and unlike `codex` it never opens a sign-in by itself.

One search is one call (read from Antigravity 2.1.4 and checked against the
backend on 2026-09-26)::

    POST {base}/v1internal:generateContent
    {"project", "model", "userAgent": "antigravity", "requestId",
     "request": {"contents": [...], "tools": [{"googleSearch": {}}], ...}}
    -> {"response": {"candidates": [{"content": {"parts": [{"text"}]},
                                      "groundingMetadata": {...}}]}}

The model lists the pages it found, one per line, and ``groundingMetadata``
says which search results each stretch of that text rests on:
``groundingChunks`` are the results (a ``vertexaisearch.cloud.google.com``
redirect and the site's name) and ``groundingSupports`` map byte ranges of the
text to chunk indices. Every result's URL comes from a chunk, resolved to the
page it redirects to. A URL the model wrote itself is never used: in testing it
named hosts that did not exist. A reply without chunks means the model answered
from memory without searching; the search is asked once more, of Antigravity's
own web-search model, and a second such reply yields no results rather than
unsearched text.

The model follows the account's catalogue (``fetchAvailableModels``) unless
``SEARCH_MCP_ANTIGRAVITY_MODEL`` names one: ``tieredModelIds.flash`` is the
flash model Antigravity currently offers, and ``webSearchModelIds`` the model
it runs its own web search on. Both change as Google ships models.

`freshness=` also goes to the search tool as a time range. Every filter is
applied to the results again afterwards, as for any other engine.
"""

from __future__ import annotations

import asyncio
import json
import re
import time
import uuid
from typing import Any
from urllib.parse import urljoin, urlparse

import httpx

from .. import oauth
from ..config import settings
from .base import (
    Engine,
    EngineKeyError,
    SearchFilters,
    SearchResult,
    not_signed_in,
)
from .codex import _FRESHNESS_WORDS, _RECENCY_DAYS, Hit, split_line, to_results

_REDIRECT_HOST = "vertexaisearch.cloud.google.com"
# One redirect lookup. Most took 0.3 to 3 s in testing and a few hung, so a
# lookup past this is dropped rather than waited for.
_LOOKUP_SECONDS = 5.0


class _Unauthorized(Exception):
    """The backend refused the access token; refresh once and retry."""


# --- the request ------------------------------------------------------------------


def build_prompt(query: str, max_results: int, filters: SearchFilters | None) -> str:
    """The whole instruction, kept short on purpose.

    Longer prompts with a strict output format made the model skip the search
    and answer from memory more often (2 in 5 against 0 in 10 for this form).
    """
    wanted = max(1, min(max_results, 10))
    parts = [f"Search the web: {query}."]
    if filters is not None:
        if filters.include_domains:
            parts.append("Only pages from " + ", ".join(filters.include_domains) + ".")
        if filters.exclude_domains:
            parts.append("No pages from " + ", ".join(filters.exclude_domains) + ".")
        if filters.freshness:
            parts.append(f"Only pages published in {_FRESHNESS_WORDS[filters.freshness]}.")
        if filters.category == "news":
            parts.append("Prefer news articles.")
        elif filters.category == "pdf":
            parts.append("Prefer PDF documents.")
    parts.append(
        f"List the {wanted} most relevant pages you found, one per line: title | date as "
        "YYYY-MM-DD or undated | one or two sentences of what the page says."
    )
    return " ".join(parts)


def _rfc3339(epoch: float) -> str:
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch))


def request_body(
    query: str, max_results: int, filters: SearchFilters | None, project: str, model: str
) -> dict[str, Any]:
    search: dict[str, Any] = {}
    if filters is not None and filters.freshness:
        now = time.time()
        search["timeRangeFilter"] = {
            "startTime": _rfc3339(now - _RECENCY_DAYS[filters.freshness] * 86400),
            "endTime": _rfc3339(now),
        }
    return {
        "project": project,
        "model": model,
        "userAgent": "antigravity",
        "requestId": f"agent-{uuid.uuid4()}",
        "request": {
            "contents": [
                {"role": "user",
                 "parts": [{"text": build_prompt(query, max_results, filters)}]}
            ],
            "tools": [{"googleSearch": search}],
            "generationConfig": {"temperature": 0},
            "sessionId": str(uuid.uuid4()),
        },
    }


# --- the reply --------------------------------------------------------------------


def _candidate(data: Any) -> dict[str, Any]:
    response = data.get("response", data) if isinstance(data, dict) else {}
    candidates = response.get("candidates") if isinstance(response, dict) else None
    first = candidates[0] if isinstance(candidates, list) and candidates else None
    return first if isinstance(first, dict) else {}


def _answer(candidate: dict[str, Any]) -> tuple[bytes, dict[int, int]]:
    """The answer as UTF-8, and the byte offset where each of its parts starts.

    A support's offsets count from the start of the part its `partIndex`
    names; without one, from the start of the answer. Thought parts are
    left out of the answer and have no start.
    """
    content = candidate.get("content") if isinstance(candidate.get("content"), dict) else {}
    raw, starts = b"", {}
    for index, part in enumerate(content.get("parts") or []):
        if isinstance(part, dict) and not part.get("thought"):
            starts[index] = len(raw)
            raw += str(part.get("text") or "").encode("utf-8")
    return raw, starts


def _byte_line_bounds(raw: bytes) -> list[tuple[int, int]]:
    """`(start, end)` byte offsets of every non-empty line of `raw`.

    Bytes, not characters: `groundingSupports` count UTF-8 bytes, so a
    character offset drifts on the first non-ASCII character.
    """
    bounds, start = [], 0
    for piece in raw.splitlines(keepends=True):
        end = start + len(piece)
        if piece.strip():
            bounds.append((start, end))
        start = end
    return bounds


def hits_from_reply(data: Any) -> list[Hit]:
    """Grounded pages in the model's order; URLs still the redirects.

    Empty when the reply carries no search results at all.
    """
    candidate = _candidate(data)
    metadata = candidate.get("groundingMetadata")
    metadata = metadata if isinstance(metadata, dict) else {}
    chunks: list[str] = []
    for chunk in metadata.get("groundingChunks") or []:
        web = chunk.get("web") if isinstance(chunk, dict) else None
        uri = web.get("uri") if isinstance(web, dict) else None
        chunks.append(uri if isinstance(uri, str) else "")
    if not any(chunks):
        return []
    raw, part_starts = _answer(candidate)
    supports = []
    for support in metadata.get("groundingSupports") or []:
        if not isinstance(support, dict):
            continue
        segment = support.get("segment") if isinstance(support.get("segment"), dict) else {}
        indices = [i for i in support.get("groundingChunkIndices") or []
                   if isinstance(i, int) and 0 <= i < len(chunks) and chunks[i]]
        start = segment.get("startIndex") or 0
        part = segment.get("partIndex")
        base = 0 if part is None else part_starts.get(part) if isinstance(part, int) else None
        if indices and isinstance(start, int) and base is not None:
            supports.append((base + start, str(segment.get("text") or ""), indices))

    hits: list[Hit] = []
    cited: set[int] = set()
    for start, end in _byte_line_bounds(raw):
        line = raw[start:end].decode("utf-8", errors="replace")
        indices: list[int] = []
        for at, text, chunk_ids in supports:
            # By offset; by the segment's text only when the offset points
            # past the reply, where it cannot be trusted.
            if start <= at < end or (at >= len(raw) and text and text in line):
                indices += [i for i in chunk_ids if i not in indices]
        if not indices:
            continue
        parsed = split_line(line)
        for n, index in enumerate(indices):
            cited.add(index)
            hits.append(
                Hit(
                    url=chunks[index],
                    # The line's title belongs to the first page it cites.
                    title=parsed.title if parsed and n == 0 else "",
                    date=parsed.date if parsed else "",
                    summary=parsed.summary if parsed else "",
                )
            )
    # Results the search returned that no line cites, after the cited ones.
    hits += [Hit(url=uri) for i, uri in enumerate(chunks) if uri and i not in cited]
    return hits


async def resolve_redirects(
    client: httpx.AsyncClient, hits: list[Hit], limit: int
) -> list[Hit]:
    """Replace each grounding redirect with the page it points to.

    A redirect that cannot be resolved is dropped: its address says nothing
    about the page, so no domain filter or deduplication could work on it.
    Only the first `limit` redirects are looked up, all at once, so the wait is
    the slowest lookup (at most `_LOOKUP_SECONDS`); a page past the limit would
    not make the results anyway.
    """
    wanted = list(dict.fromkeys(
        h.url for h in hits if urlparse(h.url).hostname == _REDIRECT_HOST
    ))[:limit]

    async def target(url: str) -> tuple[str, str]:
        try:
            response = await client.head(url, follow_redirects=False, timeout=_LOOKUP_SECONDS)
            if response.status_code == 405:
                response = await client.get(url, follow_redirects=False, timeout=_LOOKUP_SECONDS)
        except httpx.HTTPError:
            return url, ""
        location = response.headers.get("location", "") if response.is_redirect else ""
        return url, urljoin(url, location) if location else ""

    resolved = dict(await asyncio.gather(*(target(url) for url in wanted)))
    out = []
    for hit in hits:
        if urlparse(hit.url).hostname == _REDIRECT_HOST:
            if not resolved.get(hit.url):
                continue
            hit.url = resolved[hit.url]
        out.append(hit)
    return out


# --- errors -----------------------------------------------------------------------


def _error_detail(raw: str) -> tuple[str, str]:
    """`(message, when the quota resets)` from a Google error body."""
    try:
        data = json.loads(raw)
    except ValueError:
        return raw.strip()[:300], ""
    error = data.get("error") if isinstance(data, dict) else None
    if not isinstance(error, dict):
        return raw.strip()[:300], ""
    message = str(error.get("message") or error.get("status") or "")
    resets = ""
    for detail in error.get("details") or []:
        if not isinstance(detail, dict):
            continue
        meta = detail.get("metadata") if isinstance(detail.get("metadata"), dict) else {}
        resets = str(detail.get("retryDelay") or meta.get("quotaResetDelay") or resets)
    return message, resets


def _http_failure(status: int, raw: str, model: str = "") -> Exception:
    """The exception for a non-200 reply that is not an expired token."""
    detail, resets = _error_detail(raw)
    detail = detail.strip()[:300]
    suffix = f": {detail}" if detail else ""
    if status == 429:
        when = f" It resets in {resets}." if resets else ""
        return EngineKeyError(
            f"antigravity: the account's Antigravity quota or rate limit was reached (HTTP "
            f"429{suffix}).{when} Retry later, or continue with the keyless engines, which do "
            "not draw on this quota."
        )
    if status == 403:
        return EngineKeyError(
            f"antigravity: the Cloud Code backend refused this account (HTTP 403{suffix}). "
            "(Operator note: \"no valid license\" is also what the backend says to a client it "
            "does not recognise, so it may have stopped accepting Antigravity "
            f"{settings.antigravity_version}; set SEARCH_MCP_ANTIGRAVITY_VERSION to the current "
            "release. It can also mean Google has restricted the account.)"
        )
    if status in (400, 404):
        hint = ""
        if "model" in detail.lower():
            hint = (f" (Operator note: the search ran on "
                    f"{model or settings.antigravity_model!r}; set "
                    "SEARCH_MCP_ANTIGRAVITY_MODEL to a model in the account's Antigravity "
                    "catalogue, or to latest.)")
        return EngineKeyError(
            f"antigravity: the service refused the request (HTTP {status}{suffix}).{hint}"
        )
    return RuntimeError(f"antigravity: the service answered HTTP {status}{suffix}")


# --- the model --------------------------------------------------------------------

# When the catalogue cannot be read. The newest flash model on 2026-09-26; it
# ran Google Search for 8 test queries out of 8.
_FALLBACK_MODEL = "gemini-3.8-flash-tiered"
_CATALOGUE_TTL = 6 * 3600.0
_FLASH = re.compile(r"^gemini-(\d+(?:\.\d+)?)-flash(?:-[a-z-]+)?$")
# `(expires, (first, second))`, per process.
_latest: dict[str, tuple[float, tuple[str, str]]] = {}


def _first(value: Any) -> str:
    first = value[0] if isinstance(value, list) and value else None
    return first if isinstance(first, str) else ""


def pick_models(catalogue: Any) -> tuple[str, str]:
    """`(first, second)`: the model a search runs on, and the one asked when
    the first answered without searching. `("", "")` when there is nothing to
    choose from.

    The first is the catalogue's current flash model (`tieredModelIds.flash`),
    or failing that the newest recommended `gemini-N-flash` that is not a lite
    or image model. The second is the model Antigravity runs its own web
    search on (`webSearchModelIds`): faster, and it searched where a flash
    model did not, though less often overall (6 queries in 8 against 8).
    """
    data = catalogue if isinstance(catalogue, dict) else {}
    tiers = data.get("tieredModelIds") if isinstance(data.get("tieredModelIds"), dict) else {}
    first = _first(tiers.get("flash"))
    if not first:
        models = data.get("models") if isinstance(data.get("models"), dict) else {}
        flash = []
        for name, info in models.items():
            match = _FLASH.match(name) if isinstance(name, str) else None
            if (match and "lite" not in name and "image" not in name
                    and isinstance(info, dict) and info.get("recommended")):
                flash.append((tuple(int(p) for p in match.group(1).split(".")), name))
        first = max(flash)[1] if flash else ""
    second = _first(data.get("webSearchModelIds")) or first
    return first, second


# --- the engine -------------------------------------------------------------------


class AntigravityEngine(Engine):
    """Google Search run by a Gemini model on the operator's Antigravity sign-in."""

    name = "antigravity"
    description = (
        "Google Search via Gemini on the operator's Antigravity sign-in (breaks Google's terms)"
    )
    needs_browser = False
    supports_browser_fallback = False
    # Every search draws on the account's quota, and a burst is the kind of
    # traffic that gets noticed.
    rate_limit_per_minute = 10

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        return settings.antigravity_base_urls[0].rstrip("/") + "/v1internal:generateContent"

    def parse(self, html: str) -> list[SearchResult]:
        return []

    def is_available(self) -> bool:
        return oauth.is_signed_in("antigravity")

    async def _credential(self, *, force_refresh: bool = False) -> oauth.Credential:
        try:
            return await oauth.credential("antigravity", force_refresh=force_refresh)
        except oauth.NotSignedIn:
            raise not_signed_in(
                self.name,
                account="Google Antigravity",
                alternative=(
                    "omit `engines=` to use the default keyless pool, or call `research` for "
                    "search and reading in one step."
                ),
            ) from None
        except oauth.OAuthError as exc:
            raise EngineKeyError(f"{self.name}: {exc}") from exc

    async def search(
        self,
        query: str,
        max_results: int,
        filters: SearchFilters | None = None,
        diagnostics: dict[str, Any] | None = None,
    ) -> list[SearchResult]:
        cred = await self._credential()
        async with oauth.http_client(self.name, timeout=settings.antigravity_timeout) as client:
            try:
                hits = await self._grounded(client, cred, query, max_results, filters)
            except _Unauthorized:
                cred = await self._credential(force_refresh=True)
                try:
                    hits = await self._grounded(client, cred, query, max_results, filters)
                except _Unauthorized:
                    raise EngineKeyError(
                        "antigravity: the Google sign-in was refused even after a refresh. "
                        "Sign in again with `search-mcp-login antigravity`."
                    ) from None
            # Twice what is asked for: some pages repeat, and filters drop some.
            hits = await resolve_redirects(client, hits, limit=max(2 * max_results, 10))
        return self.finalize_results(to_results(hits, self.name), filters, max_results,
                                     diagnostics)

    async def _grounded(
        self,
        client: httpx.AsyncClient,
        cred: oauth.Credential,
        query: str,
        max_results: int,
        filters: SearchFilters | None,
    ) -> list[Hit]:
        # A model may answer from memory instead of searching; ask once more,
        # with `latest` of the model Antigravity searches with.
        for model in await self._models(client, cred):
            body = request_body(query, max_results, filters, cred.account_id, model)
            hits = hits_from_reply(await self._generate(client, cred, body, model))
            if hits:
                return hits
        return []

    async def _models(
        self, client: httpx.AsyncClient, cred: oauth.Credential
    ) -> tuple[str, str]:
        """The two models `_grounded` asks: the configured one twice, or with
        `latest`, `pick_models` of the catalogue, looked up once per six hours."""
        if settings.antigravity_model.strip().lower() != "latest":
            return settings.antigravity_model, settings.antigravity_model
        now = time.monotonic()
        cached = _latest.get("models")
        if cached is not None and cached[0] > now:
            return cached[1]
        picked = ("", "")
        for base in settings.antigravity_base_urls:
            try:
                response = await client.post(
                    base.rstrip("/") + "/v1internal:fetchAvailableModels",
                    json={"project": cred.account_id},
                    headers=oauth.antigravity_headers(cred.access_token),
                )
                if response.status_code == 200:
                    picked = pick_models(response.json())
            except (httpx.HTTPError, ValueError):
                continue
            if picked[0]:
                break
        if not picked[0]:
            # A failed lookup is tried again in five minutes, not six hours.
            _latest["models"] = (now + 300.0, (_FALLBACK_MODEL, _FALLBACK_MODEL))
            return _FALLBACK_MODEL, _FALLBACK_MODEL
        _latest["models"] = (now + _CATALOGUE_TTL, picked)
        return picked

    async def _generate(
        self, client: httpx.AsyncClient, cred: oauth.Credential, body: dict[str, Any],
        model: str,
    ) -> Any:
        failure: Exception | None = None
        for base in settings.antigravity_base_urls:
            url = base.rstrip("/") + "/v1internal:generateContent"
            try:
                response = await client.post(
                    url, json=body, headers=oauth.antigravity_headers(cred.access_token)
                )
            except httpx.HTTPError as exc:
                failure = RuntimeError(
                    f"antigravity: could not reach {urlparse(url).hostname}: "
                    f"{type(exc).__name__}. If it is blocked on this network, set "
                    "SEARCH_MCP_PROXY."
                )
                continue
            if response.status_code == 401:
                raise _Unauthorized
            if response.status_code == 200:
                try:
                    return response.json()
                except ValueError:
                    failure = RuntimeError("antigravity: the service did not return JSON")
                    continue
            failure = _http_failure(response.status_code, response.text, model)
            # Out of quota or failing here may not mean out of quota or
            # failing on the next host; anything else would repeat there.
            if response.status_code != 429 and response.status_code < 500:
                raise failure
        raise failure or RuntimeError("antigravity: no backend configured")
