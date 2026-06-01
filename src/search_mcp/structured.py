"""Structured-data extraction (JSON-LD, OpenGraph, Twitter cards, microdata).

The page cache stores the markdown'd body, not the raw HTML, so this module
re-fetches the page directly. We use a plain httpx client (rather than the
heavier curl_cffi/browser stack in fetcher.py) because schema.org-style
metadata is in the initial HTML payload of effectively every site that
publishes it — bot-shields don't usually strip it.
"""
from __future__ import annotations

from typing import Any

import extruct
import httpx
from selectolax.parser import HTMLParser
from w3lib.html import get_base_url

from .config import settings
from .fetcher import (
    _accumulate_capped,
    _check_content_length,
    _resolve_redirect_location,
    _MAX_REDIRECTS,
)
from .url_safety import assert_url_allowed


_SYNTAXES = ["json-ld", "microdata", "opengraph", "rdfa", "microformat"]

# Bare <meta> tag names/properties worth surfacing when no structured data
# exists. Twitter cards / OpenGraph fragments + classic SEO meta + a few
# article hints. Order is preserved in output.
_META_TARGETS: tuple[str, ...] = (
    "description",
    "keywords",
    "author",
    "robots",
    "viewport",
    "theme-color",
    "twitter:card",
    "twitter:title",
    "twitter:description",
    "twitter:image",
    "twitter:site",
    "twitter:creator",
    "article:published_time",
    "article:modified_time",
    "article:author",
    "article:section",
    "article:tag",
)


async def extract_structured(url: str) -> dict[str, Any]:
    """Pull JSON-LD, OpenGraph, Twitter cards, microdata, microformats2 from a page.

    Returns a dict with `url` plus one list per syntax. Empty lists mean the
    site doesn't publish that syntax. When all five lists are empty we add a
    `meta_fallback` dict of bare ``<meta>`` tags (if any) and a `hint`
    explaining why the page produced no structured data.
    """
    # SSRF guard: validate the caller URL before opening a socket.
    assert_url_allowed(url)
    status = 200
    async with httpx.AsyncClient(
        timeout=settings.fetch_timeout,
        # Automatic redirects DISABLED; we follow Location by hand and re-check
        # each hop with assert_url_allowed so a 30x can't reach an internal IP.
        follow_redirects=False,
        headers={"User-Agent": settings.user_agent},
    ) as client:
        current = url
        body = b""
        encoding = "utf-8"
        for _ in range(_MAX_REDIRECTS + 1):
            async with client.stream("GET", current) as resp:
                if resp.status_code in (301, 302, 303, 307, 308):
                    nxt = _resolve_redirect_location(
                        current, resp.headers.get("location")
                    )
                    if not nxt:
                        raise RuntimeError(f"redirect with no Location from {current}")
                    assert_url_allowed(nxt)
                    current = nxt
                    continue
                # DO NOT raise on non-2xx: a 403/503 bot-block still ships an
                # HTML shell we want to run through the meta_fallback/hint path.
                # Only genuine transport errors (httpx.* exceptions from
                # client.stream) propagate. Caps still apply to the shell body.
                status = resp.status_code
                _check_content_length(resp.headers)
                body = await _accumulate_capped(resp.aiter_bytes())
                encoding = resp.encoding or "utf-8"
                break
        else:
            raise RuntimeError(f"too many redirects (>{_MAX_REDIRECTS}) fetching {url}")

    html = body.decode(encoding, errors="replace")
    return extract_structured_from_html(html, url, status=status)


def extract_structured_from_html(
    html: str, url: str, *, status: int = 200
) -> dict[str, Any]:
    """Pure-function variant for unit tests and callers that already have HTML.

    ``status`` is the HTTP status the HTML came back with (200 for the pure
    unit-test path). A non-2xx status is woven into the diagnostic hint so the
    caller can tell "site has no structured data" apart from "site bot-blocked
    us with a 403/503 shell".
    """
    # extruct/w3lib can blow up on pathological HTML. Treat any failure as
    # "no structured data" and fall through to the meta_fallback/hint path
    # rather than letting the exception escape the tool.
    try:
        base_url = get_base_url(html, url)
        data = extruct.extract(
            html,
            base_url=base_url,
            syntaxes=_SYNTAXES,
            uniform=True,
        )
    except Exception:
        data = {}

    result: dict[str, Any] = {
        "url": url,
        "json_ld": data.get("json-ld", []) or [],
        "microdata": data.get("microdata", []) or [],
        "opengraph": data.get("opengraph", []) or [],
        "rdfa": data.get("rdfa", []) or [],
        "microformat": data.get("microformat", []) or [],
    }

    # If extruct found nothing across all five syntaxes, last-ditch: pull
    # bare meta tags and emit a diagnostic hint so callers can tell apart
    # "page genuinely empty" from "we got a bot-block shell".
    if not any(result[k] for k in ("json_ld", "microdata", "opengraph", "rdfa", "microformat")):
        meta = _extract_meta_tags(html)
        result["meta_fallback"] = meta
        hint = (
            "No JSON-LD / OpenGraph / microdata / RDFa / microformats2 found in the "
            "initial HTML. Possible causes: (1) page genuinely has no structured "
            "metadata, (2) data is loaded by JavaScript after the initial HTML "
            "(try fetch with render='browser'), (3) site blocks bots and served "
            "an empty shell. Bare <meta> tags surfaced as `meta_fallback` if any."
        )
        if status >= 400:
            hint += (
                f" The page returned HTTP {status}, so this is very likely a "
                "bot-block/error shell rather than the real content."
            )
        if not meta:
            hint += (
                " No fallback meta tags either — the response was likely a "
                "bot-block shell."
            )
        result["hint"] = hint

    return result


def _extract_meta_tags(html: str) -> dict[str, str]:
    """Pull useful bare ``<meta>`` tags as a fallback.

    Targets common SEO + Twitter-card + article meta. Reads both
    ``name=`` and ``property=`` (some sites mis-attribute, e.g. ``name="og:..."``).
    Only non-empty values are returned, in the order defined by ``_META_TARGETS``.
    """
    if not html:
        return {}
    try:
        tree = HTMLParser(html)
    except Exception:
        return {}

    found: dict[str, str] = {}
    for node in tree.css("meta"):
        attrs = node.attributes
        key = attrs.get("name") or attrs.get("property")
        content = attrs.get("content")
        if not key or not content:
            continue
        key = key.strip().lower()
        content = content.strip()
        if not content:
            continue
        if key in _META_TARGETS and key not in found:
            found[key] = content

    # Preserve _META_TARGETS order in the output dict.
    return {k: found[k] for k in _META_TARGETS if k in found}


__all__ = ["extract_structured", "extract_structured_from_html"]
