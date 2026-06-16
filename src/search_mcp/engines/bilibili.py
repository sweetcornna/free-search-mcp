"""Bilibili web-search JSON engine.

Keyless video search against Bilibili's public web-interface API:
    GET https://api.bilibili.com/x/web-interface/search/all/v2?keyword=...&page=1

The endpoint is anonymous (no login, no API key) but it returns ``code: -412``
("request intercepted") unless the request carries a ``buvid3`` cookie — a
device fingerprint that the real site mints in JS. We don't reverse-engineer
the genuine algorithm; a *synthetic* buvid3 of the shape ``<32 hex>infoc`` is
accepted by this endpoint, so we generate one per request with
``secrets.token_hex(16).upper() + "infoc"`` and send it alongside a
``Referer: https://www.bilibili.com`` header.

Caveats:
  * Results are videos (and some non-video groups we ignore); ``arcurl`` is the
    watch-page URL and may come back protocol-relative ("//www.bilibili.com/..."),
    which we normalise to https.
  * Titles embed ``<em class="keyword">`` highlight tags around the matched
    terms; we strip all HTML so the LLM gets clean text.
  * This is a JSON feed, so an empty/garbage parse is genuinely empty — there is
    no interstitial to recover by rendering, hence ``supports_browser_fallback``
    is False.
"""

from __future__ import annotations

import html as html_lib
import json
import re
import secrets
from datetime import datetime, timezone
from urllib.parse import quote_plus

from curl_cffi.requests import AsyncSession
from curl_cffi.requests.exceptions import RequestException

from ..config import settings
from ..net import curl_proxy_kwargs
from .base import Engine, SearchFilters, SearchResult


_TAG_RE = re.compile(r"<[^>]+>")


def _strip_html(s: str) -> str:
    """Remove HTML tags (e.g. ``<em class="keyword">``) and decode entities."""
    if not s:
        return ""
    no_tags = _TAG_RE.sub(" ", s)
    decoded = html_lib.unescape(no_tags)
    return " ".join(decoded.split())


def _format_pubdate(pubdate: object) -> str:
    """Convert a unix-seconds ``pubdate`` into an ISO date ('2026-04-28').

    Returns "" for falsy/unparseable input — never raises."""
    if not pubdate:
        return ""
    try:
        ts = int(pubdate)
    except (TypeError, ValueError):
        return ""
    try:
        return datetime.fromtimestamp(ts, tz=timezone.utc).strftime("%Y-%m-%d")
    except (OverflowError, OSError, ValueError):
        return ""


class BilibiliEngine(Engine):
    """Bilibili keyless web-search JSON API. No API key, no browser."""

    name = "bilibili"
    needs_browser = False
    # JSON feed: a [] parse is genuinely empty, so skip the pointless
    # captcha-recovery render the base search() would otherwise attempt.
    supports_browser_fallback = False

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        # The "all/v2" endpoint groups results by type (video, bili_user, ...).
        # No native domain/freshness operators here — post-filters handle those.
        return (
            "https://api.bilibili.com/x/web-interface/search/all/v2"
            f"?keyword={quote_plus(query)}&page=1"
        )

    def parse(self, html: str) -> list[SearchResult]:
        results: list[SearchResult] = []
        if not html:
            return results
        try:
            payload = json.loads(html)
        except (json.JSONDecodeError, TypeError):
            return results
        if not isinstance(payload, dict) or payload.get("code") != 0:
            return results
        data = payload.get("data")
        if not isinstance(data, dict):
            return results
        groups = data.get("result")
        if not isinstance(groups, list):
            return results

        for group in groups:
            if not isinstance(group, dict) or group.get("result_type") != "video":
                continue
            items = group.get("data")
            if not isinstance(items, list):
                continue
            for item in items:
                if not isinstance(item, dict):
                    continue
                title = _strip_html(item.get("title") or "")
                url = (item.get("arcurl") or "").strip()
                if url.startswith("//"):
                    url = "https:" + url
                elif url.startswith("http://"):
                    # Bilibili serves https everywhere; upgrade the plain-http
                    # arcurl so we don't hand the LLM a downgraded link.
                    url = "https://" + url[len("http://"):]
                if not title or not url:
                    continue
                snippet = _strip_html(item.get("description") or "")
                published_age = _format_pubdate(item.get("pubdate"))
                results.append(
                    SearchResult(
                        title=title,
                        url=url,
                        snippet=snippet,
                        engine=self.name,
                        rank=0,
                        published_age=published_age,
                        # pubdate is a structured unix-seconds publish time.
                        published_age_confident=bool(published_age),
                    )
                )
        return results

    async def _fetch(self, url: str) -> str:
        """Fetch the JSON feed with a synthetic ``buvid3`` cookie.

        Without the cookie the endpoint replies ``code: -412``. We mint a fresh
        synthetic one per request; the endpoint accepts the shape rather than
        validating a genuine signature. Never raises: on any HTTP error or
        non-200 status we return "" so parse() yields []."""
        buvid3 = secrets.token_hex(16).upper() + "infoc"
        try:
            async with AsyncSession(
                impersonate="chrome131",
                timeout=settings.request_timeout,
                allow_redirects=True,
                headers={
                    "Cookie": f"buvid3={buvid3}",
                    "Referer": "https://www.bilibili.com",
                    "Accept-Language": settings.accept_language,
                },
                **curl_proxy_kwargs(self.name),
            ) as client:
                resp = await client.get(url)
                if resp.status_code != 200:
                    return ""
                return resp.text
        except RequestException:
            return ""
