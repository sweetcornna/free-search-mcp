"""Resolve opaque Google News article URLs to their real publisher URLs.

Google News RSS items (and the on-site article links) point at
``https://news.google.com/rss/articles/CBM...`` redirect blobs, NOT the
publisher. Those blobs used to embed the target URL in a decodable base64
payload, but the current (2024+) format is an opaque protobuf: fetching the
link over HTTP *or* a headless browser lands on an empty JS shell, so
``fetch`` / ``research`` come back with zero content for every news result.

Google's own web client resolves the link through a private RPC: it reads a
signature (``data-n-a-sg``) + timestamp (``data-n-a-ts``) + article id
(``data-n-a-id``) out of the article page, then POSTs them to the
``batchexecute`` endpoint, which replies with the publisher URL. We replay
exactly that exchange here. Verified working June 2026.

Best-effort by contract: any failure (not a GN url, page shape changed, RPC
error, network blip) returns ``None`` so the caller falls back to the original
URL with no regression. Successful resolutions are memoised for the process
lifetime — the article-page download is ~600 KB, so we never pay it twice.
"""
from __future__ import annotations

import asyncio
import json
import logging
import re
from urllib.parse import quote, urlparse

from curl_cffi.requests import AsyncSession
from curl_cffi.requests.exceptions import RequestException

from .config import settings
from .net import curl_proxy_kwargs

log = logging.getLogger(__name__)

_IMPERSONATE = "chrome131"
_BATCHEXECUTE = "https://news.google.com/_/DotsSplashUi/data/batchexecute"

# Signature / timestamp / id are emitted as attributes on a <c-wiz> in the
# article shell. Order is not guaranteed, so each is matched independently.
_SG_RE = re.compile(r'data-n-a-sg="([^"]+)"')
_TS_RE = re.compile(r'data-n-a-ts="([^"]+)"')
_ID_RE = re.compile(r'data-n-a-id="([^"]+)"')

# Process-lifetime memo: GN article url -> resolved publisher url. Bounded so a
# long-running server can't grow it without limit; news urls are one-shot reads
# so a simple FIFO trim is plenty.
_MEMO: dict[str, str] = {}
_MEMO_MAX = 2048
# Coalesce concurrent resolves of the same url onto one in-flight request.
_INFLIGHT: dict[str, asyncio.Future] = {}


def is_google_news_url(url: str) -> bool:
    """True for the news.google.com article redirect blobs we can resolve."""
    try:
        p = urlparse(url)
    except ValueError:
        return False
    if p.hostname not in ("news.google.com", "www.news.google.com"):
        return False
    # /rss/articles/CBM..., /articles/CBM..., /read/CBM...
    return "/articles/" in p.path or "/read/" in p.path


def _build_freq(article_id: str, ts: str, sig: str) -> str:
    """Construct the ``f.req`` form payload for one ``garturlreq`` call."""
    inner = json.dumps(
        [
            "garturlreq",
            [
                ["X", "X", ["X", "X"], None, None, 1, 1, "US:en", None, 1,
                 None, None, None, None, None, 0, 1],
                "X", "X", 1, [1, 1, 1], 1, 1, None, 0, 0, None, 0,
            ],
            article_id,
            int(ts),
            sig,
        ]
    )
    return "f.req=" + quote(json.dumps([[["Fbv4je", inner, None, "generic"]]]))


def _parse_batchexecute(text: str) -> str | None:
    """Pull the publisher url out of a ``batchexecute`` response.

    Body shape (after the ``)]}'`` XSSI guard):
        [["wrb.fr","Fbv4je","[\\"garturlres\\",\\"<URL>\\",1]",...], ...]
    """
    if not text:
        return None
    body = text.lstrip(")]}'").strip()
    # The array we want is usually on its own line after a length prefix.
    candidate = None
    for line in body.splitlines():
        line = line.strip()
        if line.startswith("[") and "Fbv4je" in line:
            candidate = line
            break
    if candidate is None:
        candidate = body
    try:
        arr = json.loads(candidate)
    except (json.JSONDecodeError, ValueError):
        arr = None
    for row in arr if isinstance(arr, list) else []:
        if isinstance(row, list) and len(row) > 2 and row[1] == "Fbv4je":
            try:
                payload = json.loads(row[2])
            except (json.JSONDecodeError, ValueError, TypeError):
                continue
            if isinstance(payload, list) and len(payload) > 1 and isinstance(payload[1], str):
                url = payload[1]
                if url.startswith("http"):
                    return url
    # Fallback: a pretty-printed/chunked response (array split across lines, or a
    # length-prefix line we couldn't strip) defeats the structured parse above.
    # The garturlres URL is unambiguous in the raw text, so pull it directly.
    m = _GARTURL_RE.search(text)
    return m.group(1) if m else None


# Matches the publisher url inside the escaped ``["garturlres","<url>",...]``
# payload, robust to whitespace/line breaks the structured parser can't follow.
_GARTURL_RE = re.compile(r'garturlres\\?",\\?"\s*(https?://[^"\\]+)')


async def _resolve(url: str) -> str | None:
    try:
        async with AsyncSession(
            impersonate=_IMPERSONATE,
            timeout=settings.request_timeout,
            allow_redirects=True,
            headers={"Accept-Language": settings.accept_language},
            **curl_proxy_kwargs("googlenews"),
        ) as client:
            page = await client.get(url)
            html = page.text or ""
            sg = _SG_RE.search(html)
            ts = _TS_RE.search(html)
            aid = _ID_RE.search(html)
            if not (sg and ts and aid):
                return None
            resp = await client.post(
                _BATCHEXECUTE,
                data=_build_freq(aid.group(1), ts.group(1), sg.group(1)),
                headers={
                    "Content-Type": "application/x-www-form-urlencoded;charset=UTF-8"
                },
            )
            if resp.status_code != 200:
                return None
            return _parse_batchexecute(resp.text)
    except (RequestException, asyncio.TimeoutError):
        return None
    except Exception as e:  # never-raise contract
        log.debug("google news resolve failed for %s: %s", url, e)
        return None


async def resolve_google_news_url(url: str) -> str | None:
    """Resolve a news.google.com article blob to its publisher url.

    Returns the publisher url on success, or ``None`` when ``url`` isn't a
    Google News blob or the resolution failed (caller keeps the original url).
    Memoised + single-flighted across concurrent callers.
    """
    if not is_google_news_url(url):
        return None
    if url in _MEMO:
        return _MEMO[url]

    inflight = _INFLIGHT.get(url)
    if inflight is not None:
        return await inflight

    loop = asyncio.get_running_loop()
    fut: asyncio.Future = loop.create_future()
    _INFLIGHT[url] = fut
    try:
        resolved = await _resolve(url)
        if resolved:
            if len(_MEMO) >= _MEMO_MAX:
                _MEMO.pop(next(iter(_MEMO)), None)
            _MEMO[url] = resolved
        if not fut.done():
            fut.set_result(resolved)
        return resolved
    except BaseException:
        # Includes CancelledError: settle waiters with the best-effort None
        # result rather than orphaning the future (which would hang every
        # coalesced waiter forever) or propagating OUR cancellation into other,
        # independent requests that merely share this in-flight url.
        if not fut.done():
            fut.set_result(None)
        raise
    finally:
        _INFLIGHT.pop(url, None)


__all__ = ["is_google_news_url", "resolve_google_news_url"]
