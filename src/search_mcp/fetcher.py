from __future__ import annotations

import asyncio
import io
import json
import logging
from dataclasses import dataclass
from typing import Any
from urllib.parse import urlparse

import httpx
import trafilatura
from curl_cffi.requests import AsyncSession
from markdownify import markdownify as html_to_md
from selectolax.parser import HTMLParser

from .browser import pool
from .cache import cache
from .config import settings
from .formatting import estimate_tokens, smart_truncate
from .gnews import is_google_news_url, resolve_google_news_url

# Shared GET plumbing lives in httpfetch. The first three names are
# re-exported for the tests that exercise them through this module
# (fetch_safety, charset); everything else imports from httpfetch directly.
from .httpfetch import (  # noqa: F401
    MaxBytesExceededError,
    _accumulate_capped,
    _decode_body,
    curl_session_kwargs,
    curl_stream_capped,
)
from .ratelimit import RateLimiter
from .url_safety import assert_url_allowed_async

log = logging.getLogger(__name__)
fetch_limiter = RateLimiter(settings.fetch_rate_limit_per_minute)


# Tags that contribute no content to a reader-mode view (fallback path).
_BOILERPLATE = (
    "script", "style", "noscript", "nav", "header",
    "footer", "form", "aside", "iframe", "svg",
)

# Sentinel used to embed metadata JSON inside the cache `title` column without
# touching cache.py's schema. Format:
#   "\x01META\x01" + json + "\x01"
# Old rows lacking the prefix are treated as plain titles (back-compat).
_META_SENTINEL = "\x01META\x01"
_META_SENTINEL_END = "\x01"


@dataclass(slots=True)
class FetchResult:
    url: str
    title: str
    content: str
    method: str
    truncated: bool
    tokens_estimated: int = 0
    author: str = ""
    published_date: str = ""
    sitename: str = ""
    # --- asset fields, set only on the binary/media path -------------------
    # A page has content; an image has properties. These describe the resource
    # itself so a model can decide whether it's worth spending tokens to look
    # at, without the bytes being inlined by default.
    media_type: str = ""
    bytes_size: int = 0
    sha256: str = ""
    width: int | None = None
    height: int | None = None
    # Raw bytes, populated ONLY when the caller asked to inline them. Kept off
    # to_dict() so a JSON-format tool response never carries a megabyte of
    # base64 the caller didn't request.
    data: bytes | None = None
    saved_path: str = ""

    def to_dict(self) -> dict[str, Any]:
        out: dict[str, Any] = {
            "url": self.url,
            "title": self.title,
            "content": self.content,
            "method": self.method,
            "truncated": self.truncated,
            "tokens_estimated": self.tokens_estimated,
            "author": self.author,
            "published_date": self.published_date,
            "sitename": self.sitename,
        }
        if self.media_type:
            out["media_type"] = self.media_type
            out["bytes_size"] = self.bytes_size
            out["sha256"] = self.sha256
            if self.width is not None:
                out["width"] = self.width
                out["height"] = self.height
        if self.saved_path:
            out["saved_path"] = self.saved_path
        return out


def _encode_title_meta(title: str, author: str, date: str, sitename: str) -> str:
    """Pack metadata into the cache title column behind a sentinel prefix."""
    payload = json.dumps(
        {"title": title, "author": author, "date": date, "sitename": sitename},
        ensure_ascii=False,
    )
    return f"{_META_SENTINEL}{payload}{_META_SENTINEL_END}"


def _decode_title_meta(raw: str | None) -> tuple[str, str, str, str]:
    """Inverse of _encode_title_meta. Returns (title, author, date, sitename).

    Backward-compat: rows written before this change have no sentinel and
    contain a plain title string.
    """
    if not raw:
        return "", "", "", ""
    if not raw.startswith(_META_SENTINEL):
        return raw, "", "", ""
    body = raw[len(_META_SENTINEL):]
    if body.endswith(_META_SENTINEL_END):
        body = body[: -len(_META_SENTINEL_END)]
    try:
        data = json.loads(body)
    except json.JSONDecodeError:
        return raw, "", "", ""
    return (
        data.get("title", "") or "",
        data.get("author", "") or "",
        data.get("date", "") or "",
        data.get("sitename", "") or "",
    )


def _extract_main_html(html: str) -> tuple[str, str]:
    """Fallback: naive boilerplate strip + main-region heuristic."""
    tree = HTMLParser(html)
    title = ""
    if tree.css_first("title"):
        title = tree.css_first("title").text(strip=True)
    for tag in _BOILERPLATE:
        for node in tree.css(tag):
            node.decompose()
    main = (
        tree.css_first("article")
        or tree.css_first("main")
        or tree.css_first("[role=main]")
        or tree.css_first("#content")
        or tree.css_first(".content")
        or tree.body
    )
    inner = main.html if main else (tree.body.html if tree.body else html)
    return title, inner or ""


def _to_markdown(html: str) -> str:
    """Fallback HTML->Markdown conversion using markdownify."""
    md = html_to_md(html, heading_style="ATX", bullets="-", strip=["a", "img"])
    lines = [ln.rstrip() for ln in md.splitlines()]
    out: list[str] = []
    blank = 0
    for ln in lines:
        if ln.strip():
            out.append(ln)
            blank = 0
        else:
            blank += 1
            if blank <= 1:
                out.append("")
    return "\n".join(out).strip()


def _extract(html: str, url: str) -> tuple[str, str, str, str, str]:
    """Extract main content + metadata.

    Returns (title, markdown, author, published_date, sitename).
    Falls back to selectolax+markdownify if trafilatura returns nothing.
    """
    title = ""
    author = ""
    date = ""
    sitename = ""

    # Parse the HTML ONCE (trafilatura otherwise re-parses it for both
    # extract_metadata and extract). Both calls accept a pre-parsed
    # lxml.html.HtmlElement; verified that metadata-then-extract on a shared
    # tree yields output identical to the string path. If parsing fails we
    # fall back to passing the raw string, preserving the old behaviour.
    try:
        doc = trafilatura.load_html(html)
    except Exception as e:
        log.debug("trafilatura load_html failed for %s: %s", url, e)
        doc = None
    meta_input = doc if doc is not None else html
    extract_input = doc if doc is not None else html

    try:
        meta = trafilatura.extract_metadata(meta_input)
    except Exception as e:  # extract_metadata can raise on weird inputs
        log.debug("trafilatura metadata failed for %s: %s", url, e)
        meta = None
    if meta is not None:
        title = (getattr(meta, "title", None) or "") or title
        author = getattr(meta, "author", None) or ""
        date = getattr(meta, "date", None) or ""
        sitename = getattr(meta, "sitename", None) or ""

    md = ""
    try:
        md = trafilatura.extract(
            extract_input,
            url=url,
            output_format="markdown",
            include_links=True,
            include_tables=True,
            favor_precision=True,
        ) or ""
    except Exception as e:
        log.info("trafilatura extract failed for %s: %s", url, e)
        md = ""

    if not md.strip():
        # Fallback to the legacy path so we never silently lose content.
        fallback_title, main_html = _extract_main_html(html)
        title = title or fallback_title
        md = _to_markdown(main_html)

    return title, md.strip(), author, date, sitename


def _truncate(text: str) -> tuple[str, bool]:
    return smart_truncate(text, settings.max_content_chars)


async def _http_fetch(url: str) -> tuple[str, str]:
    """SSRF-guarded GET returning ``(content_type, decoded_text)``.

    The redirect/caps/charset loop lives in httpfetch.curl_stream_capped; the
    session is constructed HERE (after the guard, before any socket) so tests
    can keep monkeypatching ``fetcher.AsyncSession``.
    """
    await assert_url_allowed_async(url)
    async with AsyncSession(**curl_session_kwargs()) as client:
        return await curl_stream_capped(client, url)


_DOC_URL_SUFFIXES = (".pdf", ".docx")
_DOC_CTYPES = ("application/pdf", "wordprocessingml", "application/msword")


def _is_document_url(url: str) -> bool:
    """True for URLs whose path ends in a binary-document extension."""
    try:
        path = urlparse(url).path.lower()
    except ValueError:
        return False
    return path.endswith(_DOC_URL_SUFFIXES)


def _is_document_ctype(ctype: str) -> bool:
    c = (ctype or "").lower()
    return any(t in c for t in _DOC_CTYPES)


def _ctype_is_markup(ctype: str) -> bool:
    """True for HTML/XML content-types, or an empty one (HTTP fetch failed /
    server omitted the header) where a browser render may still recover."""
    c = (ctype or "").lower()
    return (not c) or ("html" in c) or ("xml" in c)


# Binary resources that are neither markup nor a text-bearing document. These
# get described (type, size, dimensions, hash) rather than decoded — decoding
# them as text produces a screen of U+FFFD.
_ASSET_CTYPE_PREFIXES = ("image/", "video/", "audio/", "font/")
_ASSET_CTYPES = (
    "application/octet-stream", "application/zip", "application/x-tar",
    "application/gzip", "application/x-7z-compressed", "application/x-rar",
    "application/wasm",
)


def _is_asset_ctype(ctype: str) -> bool:
    c = (ctype or "").lower()
    if not c:
        return False
    # SVG is markup and readable as text, so it is NOT an opaque asset.
    if c.startswith("image/svg"):
        return False
    return c.startswith(_ASSET_CTYPE_PREFIXES) or any(t in c for t in _ASSET_CTYPES)


def _is_asset_url(url: str) -> bool:
    from .documents import _detect_format

    try:
        path = urlparse(url).path
    except ValueError:
        return False
    if path.lower().endswith(".svg"):
        return False
    return _detect_format(path) == "image"


def _image_dimensions(blob: bytes) -> tuple[int | None, int | None]:
    """Read (width, height) from an image header without decoding the pixels.

    Pillow parses only the header on `open`, so this stays cheap even for a
    large photo. Anything unrecognised returns (None, None) rather than
    raising — dimensions are a nicety, not the point of the fetch.
    """
    try:
        from PIL import Image as PILImage

        with PILImage.open(io.BytesIO(blob)) as im:
            return im.width, im.height
    except Exception:
        return None, None


def _human_size(n: int) -> str:
    value = float(n)
    for unit in ("B", "KB", "MB", "GB"):
        if value < 1024 or unit == "GB":
            return f"{value:.0f} {unit}" if unit == "B" else f"{value:.1f} {unit}"
        value /= 1024
    return f"{value:.1f} GB"


async def _fetch_as_asset(
    url: str, *, inline: bool = False, describe_only: bool = True
) -> FetchResult:
    """Describe a binary resource; carry its bytes only if asked.

    `content` is a human/LLM-readable summary rather than the bytes, so an
    ordinary `fetch` of an image costs a few dozen tokens instead of a
    megabyte of base64. `inline=True` additionally attaches the raw bytes for
    the server layer to turn into MCP ImageContent.
    """
    import hashlib

    from .httpfetch import httpx_client_kwargs, httpx_stream_capped

    await assert_url_allowed_async(url)
    async with httpx.AsyncClient(**httpx_client_kwargs()) as client:
        _status, ctype, blob = await httpx_stream_capped(client, url, raise_for_status=True)

    media_type = (ctype or "application/octet-stream").split(";", 1)[0].strip()
    width, height = (None, None)
    if media_type.startswith("image/"):
        width, height = _image_dimensions(blob)

    name = urlparse(url).path.rsplit("/", 1)[-1] or url
    bits = [f"**{name}**", media_type, _human_size(len(blob))]
    if width and height:
        bits.append(f"{width}×{height}px")
    summary = " · ".join(bits)
    if not inline and describe_only and media_type.startswith("image/"):
        summary += (
            "\n\nBytes not included. Call `fetch` again with `inline=True` to "
            "pass the image itself to a vision-capable model."
        )

    return FetchResult(
        url=url,
        title=name,
        content=summary,
        method="asset",
        truncated=False,
        tokens_estimated=estimate_tokens(summary),
        media_type=media_type,
        bytes_size=len(blob),
        sha256=hashlib.sha256(blob).hexdigest(),
        width=width,
        height=height,
        data=blob if inline else None,
    )


async def fetch_bytes(url: str) -> FetchResult:
    """Download a URL as raw bytes, whatever its type.

    Distinct from `fetch_page`, which routes by content-type and returns text
    for anything text-bearing. A download must preserve the FILE — saving
    trafilatura's Markdown rendering of a PDF would not be the PDF.
    """
    return await _fetch_as_asset(url, inline=True, describe_only=False)


async def _fetch_as_document(url: str) -> FetchResult:
    """Parse a binary document (PDF/DOCX) via the document reader and adapt it to
    a FetchResult, so fetch/research return real text instead of decoded bytes.

    Caches the extracted text under the page cache (so repeat fetches + cache
    search work). Lazy-imports documents to avoid a fetcher<->documents cycle.
    """
    from .documents import read_document

    doc = await read_document(url)
    content, soft_trunc = _truncate(doc.content)
    await cache.put_page(url, _encode_title_meta(doc.title, "", "", ""), doc.content)
    return FetchResult(
        url=url,
        title=doc.title,
        content=content,
        method="document",
        truncated=soft_trunc or doc.truncated,
        tokens_estimated=estimate_tokens(content),
    )


async def fetch_page(
    url: str,
    *,
    render: str = "auto",
    force_refresh: bool = False,
    inline: bool = False,
) -> FetchResult:
    # Google News RSS/article links (news.google.com/.../articles/CBM...) are
    # opaque redirect blobs that resolve to an empty JS shell over both HTTP and
    # a headless browser — so fetch/research would otherwise return zero content
    # for every news result. Resolve to the real publisher URL up front (best
    # effort, memoised) so the cache key, fetched body, and returned url are all
    # the publisher's. On failure we keep the original url (no regression).
    if is_google_news_url(url):
        resolved = await resolve_google_news_url(url)
        if resolved:
            url = resolved

    if not force_refresh:
        cached = await cache.get_page(url)
        if cached:
            title, author, date, sitename = _decode_title_meta(cached.get("title"))
            content, truncated = _truncate(cached["content"])
            return FetchResult(
                url=url,
                title=title,
                content=content,
                method="cache",
                truncated=truncated,
                tokens_estimated=estimate_tokens(content),
                author=author,
                published_date=date,
                sitename=sitename,
            )

    # Binary documents (PDF/DOCX) must be parsed by the document reader, not
    # decoded as text (which yields U+FFFD garbage). The URL suffix is known up
    # front, so route before fetching; the content-type check below catches
    # extension-less URLs that still serve a PDF/DOCX.
    if _is_document_url(url):
        return await _fetch_as_document(url)

    # Images and other opaque binaries are described, never decoded as text.
    # Routed by URL first so we don't pay for a text fetch we'd throw away;
    # the content-type check below catches extension-less URLs.
    if _is_asset_url(url):
        return await _fetch_as_asset(url, inline=inline)

    await fetch_limiter.acquire("fetch")

    method = "http"
    title = ""
    html = ""
    ctype = ""
    last_err: Exception | None = None

    if render in ("auto", "http"):
        try:
            ctype, html = await _http_fetch(url)
        except Exception as e:
            last_err = e
            log.info("http fetch failed for %s: %s", url, e)

    # An extension-less URL that nonetheless serves a PDF/DOCX content-type:
    # hand off to the document parser instead of decoding its bytes as text.
    if _is_document_ctype(ctype):
        return await _fetch_as_document(url)

    if _is_asset_ctype(ctype):
        return await _fetch_as_asset(url, inline=inline)

    # The short-body browser fallback is for HTML pages that arrived as an empty
    # JS shell — gate it on the content-type being markup (or unknown), so a
    # small JSON/text API response isn't needlessly re-rendered in Chromium and
    # then mislabelled text/html (which would route it through trafilatura).
    needs_browser = render == "browser" or (
        render == "auto" and _ctype_is_markup(ctype) and (not html or len(html) < 500)
    )
    if needs_browser:
        try:
            title2, html2 = await pool.fetch_html(url)
            title = title2 or title
            html = html2
            ctype = "text/html"  # browser always renders HTML
            method = "browser"
        except Exception as e:
            if not html:
                raise RuntimeError(f"fetch failed for {url}: {e}") from e
            log.warning("browser fallback failed for %s, using http body: %s", url, e)

    if not html:
        raise RuntimeError(f"empty response for {url}: {last_err}")

    # Content-type contract: only HTML/XML payloads go through trafilatura
    # extraction. JSON / plain-text / other content-types are returned VERBATIM
    # (extracting them through trafilatura would mangle or drop the body).
    is_markup = ("html" in ctype) or ("xml" in ctype)
    if is_markup:
        extracted_title, md, author, date, sitename = _extract(html, url)
        title = title or extracted_title
    else:
        md = html  # raw body, verbatim
        author = date = sitename = ""

    await cache.put_page(url, _encode_title_meta(title, author, date, sitename), md)
    content, truncated = _truncate(md)
    return FetchResult(
        url=url,
        title=title,
        content=content,
        method=method,
        truncated=truncated,
        tokens_estimated=estimate_tokens(content),
        author=author,
        published_date=date,
        sitename=sitename,
    )


# The rate limiter throttles requests per minute but says nothing about how
# many coroutines are LIVE at once — an unbounded gather over a big URL list
# would open that many sockets/SSRF lookups simultaneously.
_FETCH_CONCURRENCY = 8


async def fetch_many(urls: list[str], render: str = "auto") -> list[FetchResult | dict[str, str]]:
    sem = asyncio.Semaphore(_FETCH_CONCURRENCY)

    async def one(u: str):
        try:
            async with sem:
                return await fetch_page(u, render=render)
        except Exception as e:
            return {"url": u, "error": str(e)}

    return await asyncio.gather(*(one(u) for u in urls))
