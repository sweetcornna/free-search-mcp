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
from w3lib.html import get_base_url

from .config import settings


_SYNTAXES = ["json-ld", "microdata", "opengraph", "rdfa"]


async def extract_structured(url: str) -> dict[str, Any]:
    """Pull JSON-LD, OpenGraph, Twitter cards, and microdata from a page.

    Returns a dict with `url` plus one list per syntax. Each list contains
    one entry per discovered structured-data block. Empty lists mean the
    site doesn't publish that syntax.
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
    return {
        "url": url,
        "json_ld": data.get("json-ld", []) or [],
        "microdata": data.get("microdata", []) or [],
        "opengraph": data.get("opengraph", []) or [],
        "rdfa": data.get("rdfa", []) or [],
    }


__all__ = ["extract_structured", "extract_structured_from_html"]
