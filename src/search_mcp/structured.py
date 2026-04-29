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
    async with httpx.AsyncClient(
        timeout=settings.fetch_timeout,
        follow_redirects=True,
        headers={"User-Agent": settings.user_agent},
    ) as client:
        resp = await client.get(url)
        resp.raise_for_status()
        html = resp.text

    return extract_structured_from_html(html, url)


def extract_structured_from_html(html: str, url: str) -> dict[str, Any]:
    """Pure-function variant for unit tests and callers that already have HTML."""
    base_url = get_base_url(html, url)
    data = extruct.extract(
        html,
        base_url=base_url,
        syntaxes=_SYNTAXES,
        uniform=True,
    )
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
