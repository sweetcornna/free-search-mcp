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

from .httpfetch import _decode_body, httpx_client_kwargs, httpx_stream_capped
from .url_safety import assert_url_allowed_async

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


# What this tool is FOR is the small fields: startDate, price, datePublished,
# author, location. What publishers also put in the same blocks is the entire
# article (`articleBody`) — measured at ~6,000 tokens for one blog post, most of
# it a second copy of text `fetch` returns better. Long prose is clipped and
# says so; nothing short is ever touched.
_LONG_TEXT_KEYS = frozenset(
    {"articleBody", "text", "description", "reviewBody", "transcript", "abstract", "content"}
)
_LONG_TEXT_CAP = 500
_XHTML_VOCAB = "http://www.w3.org/1999/xhtml/vocab#"


def _local_name(key: str) -> str:
    """`articleBody` out of `http://schema.org/articleBody` or `og:description`."""
    return key.rsplit("/", 1)[-1].rsplit("#", 1)[-1].rsplit(":", 1)[-1]


def _clip_long_text(node: Any, clipped: list[int]) -> Any:
    if isinstance(node, list):
        return [_clip_long_text(item, clipped) for item in node]
    if not isinstance(node, dict):
        return node
    out: dict[str, Any] = {}
    for key, value in node.items():
        if (
            isinstance(value, str)
            and len(value) > _LONG_TEXT_CAP
            and _local_name(key) in _LONG_TEXT_KEYS
        ):
            clipped.append(len(value))
            out[key] = (
                value[:_LONG_TEXT_CAP].rstrip()
                + f" … [clipped: {len(value)} characters in the page]"
            )
        else:
            out[key] = _clip_long_text(value, clipped)
    return out


def _is_layout_noise(node: Any) -> bool:
    """An RDFa node that only says "this <div> is a navigation landmark".

    ARIA `role` attributes are valid RDFa, so extruct reports one blank node per
    landmark: dozens of `{"@id": "_:N…", "…/xhtml/vocab#role": […]}` entries
    that describe the page's layout and nothing about its content.
    """
    if not isinstance(node, dict):
        return False
    facts = [key for key in node if not key.startswith("@")]
    return bool(facts) and all(key.startswith(_XHTML_VOCAB) for key in facts)


async def extract_structured(url: str) -> dict[str, Any]:
    """Pull JSON-LD, OpenGraph, Twitter cards, microdata, microformats2 from a page.

    Returns a dict with `url` plus one list per syntax. Empty lists mean the
    site doesn't publish that syntax. When all five lists are empty we add a
    `meta_fallback` dict of bare ``<meta>`` tags (if any) and a `hint`
    explaining why the page produced no structured data.
    """
    # SSRF guard: validate the caller URL before opening a socket. The client
    # is constructed HERE (tests monkeypatch httpx.AsyncClient); the shared
    # redirect/caps loop lives in httpfetch.
    await assert_url_allowed_async(url)
    async with httpx.AsyncClient(**httpx_client_kwargs()) as client:
        # raise_for_status=False: a 403/503 bot-block still ships an HTML
        # shell we want to run through the meta_fallback/hint path. Only
        # genuine transport errors propagate. Caps still apply to the shell.
        status, content_type, body = await httpx_stream_capped(
            client, url, raise_for_status=False
        )

    # Honor the declared/sniffed charset (shared with fetcher) so non-UTF-8 pages
    # don't turn schema.org metadata into mojibake.
    html = _decode_body(body, content_type)
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

    clipped: list[int] = []
    rdfa_all = data.get("rdfa", []) or []
    rdfa = [node for node in rdfa_all if not _is_layout_noise(node)]
    result: dict[str, Any] = {
        "url": url,
        "json_ld": _clip_long_text(data.get("json-ld", []) or [], clipped),
        "microdata": _clip_long_text(data.get("microdata", []) or [], clipped),
        "opengraph": _clip_long_text(data.get("opengraph", []) or [], clipped),
        "rdfa": _clip_long_text(rdfa, clipped),
        "microformat": _clip_long_text(data.get("microformat", []) or [], clipped),
    }
    # Said, not done silently: a reader must be able to tell a short
    # `articleBody` from a clipped one, and know where the rest is.
    notes: list[str] = []
    if clipped:
        notes.append(
            f"{len(clipped)} long text field(s) clipped to {_LONG_TEXT_CAP} characters "
            f"({sum(clipped)} in the page), so use `fetch` for the full text"
        )
    if len(rdfa) != len(rdfa_all):
        notes.append(f"{len(rdfa_all) - len(rdfa)} layout-only RDFa node(s) (ARIA roles) omitted")
    if notes:
        result["trimmed"] = "; ".join(notes) + "."

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
                " No fallback meta tags either, so the response was likely a "
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
