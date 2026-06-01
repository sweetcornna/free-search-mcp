from __future__ import annotations

import asyncio
import json
import logging
from dataclasses import dataclass
from typing import Any

import trafilatura
from curl_cffi.requests import AsyncSession
from markdownify import markdownify as html_to_md
from selectolax.parser import HTMLParser

from .browser import pool
from .cache import cache
from .config import settings
from .formatting import estimate_tokens, smart_truncate
from .ratelimit import RateLimiter
from .url_safety import assert_url_allowed

log = logging.getLogger(__name__)
fetch_limiter = RateLimiter(settings.fetch_rate_limit_per_minute)

# Match the engine fast-path: real Chrome JA3/JA4 + H2 fingerprint so target
# sites don't see "headless client claiming to be Chrome".
_IMPERSONATE = "chrome131"


# Tags that contribute no content to a reader-mode view (fallback path).
_BOILERPLATE = ("script", "style", "noscript", "nav", "header", "footer", "form", "aside", "iframe", "svg")

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

    def to_dict(self) -> dict[str, Any]:
        return {
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


# Cap on manually-followed redirect hops. We disable the HTTP client's
# automatic redirect handling (which would chase a 30x straight to an
# internal IP, bypassing the SSRF guard) and follow Location headers by hand,
# re-validating each hop with assert_url_allowed before connecting.
_MAX_REDIRECTS = 5


class MaxBytesExceededError(RuntimeError):
    """Raised when a response body grows past settings.max_response_bytes."""


def _check_content_length(headers: Any) -> None:
    """Reject up front if the declared Content-Length exceeds the cap.

    Shared by all three remote-GET helpers (fetcher._http_fetch,
    documents._read_remote, structured.extract_structured). A streaming guard
    (_accumulate_capped) still backstops servers that lie or omit the header.
    """
    raw = headers.get("content-length") or headers.get("Content-Length")
    if not raw:
        return
    try:
        declared = int(raw)
    except (TypeError, ValueError):
        return
    cap = settings.max_response_bytes
    if declared > cap:
        raise MaxBytesExceededError(
            f"Response Content-Length {declared} exceeds cap {cap} bytes; refusing to download."
        )


async def _accumulate_capped(aiter: Any) -> bytes:
    """Buffer an async byte-chunk iterator, aborting once it passes the cap.

    The cap is settings.max_response_bytes. Shared across the three remote
    helpers so an oversized (or Content-Length-lying) body never fully buffers
    into memory.
    """
    cap = settings.max_response_bytes
    buf = bytearray()
    async for chunk in aiter:
        if not chunk:
            continue
        buf.extend(chunk)
        if len(buf) > cap:
            raise MaxBytesExceededError(
                f"Response body exceeded cap {cap} bytes while streaming; aborted."
            )
    return bytes(buf)


def _resolve_redirect_location(base_url: str, location: str | None) -> str | None:
    """Resolve a (possibly relative) Location against base_url. None if absent."""
    if not location:
        return None
    from urllib.parse import urljoin

    return urljoin(base_url, location)


async def _http_fetch(url: str) -> tuple[str, str]:
    # SSRF guard: validate the caller URL before we ever open a socket.
    assert_url_allowed(url)
    # No explicit User-Agent: curl_cffi sets one matching the impersonated
    # Chrome build, keeping the UA <-> JA3/H2 fingerprints consistent.
    async with AsyncSession(
        impersonate=_IMPERSONATE,
        timeout=settings.fetch_timeout,
        # Automatic redirects are DISABLED: a 30x could otherwise jump straight
        # to an internal IP, bypassing the per-hop SSRF check below.
        allow_redirects=False,
        headers={
            "Accept-Language": settings.accept_language,
            "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        },
    ) as client:
        current = url
        for _ in range(_MAX_REDIRECTS + 1):
            resp = await client.get(current, stream=True)
            status = resp.status_code
            if status in (301, 302, 303, 307, 308):
                # Drain/close the redirect response without buffering its body.
                await resp.aclose()
                nxt = _resolve_redirect_location(current, resp.headers.get("location"))
                if not nxt:
                    raise RuntimeError(f"redirect with no Location from {current}")
                assert_url_allowed(nxt)  # re-validate EACH hop before following
                current = nxt
                continue
            # Terminal response: enforce caps, then stream the body.
            resp.raise_for_status()
            _check_content_length(resp.headers)
            try:
                body = await _accumulate_capped(resp.aiter_content())
            finally:
                await resp.aclose()
            ctype = resp.headers.get("content-type", "")
            encoding = getattr(resp, "encoding", None) or "utf-8"
            text = body.decode(encoding, errors="replace")
            return ctype, text
        raise RuntimeError(f"too many redirects (>{_MAX_REDIRECTS}) fetching {url}")


async def fetch_page(
    url: str,
    *,
    render: str = "auto",
    force_refresh: bool = False,
) -> FetchResult:
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

    needs_browser = render == "browser" or (render == "auto" and (not html or len(html) < 500))
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


async def fetch_many(urls: list[str], render: str = "auto") -> list[FetchResult | dict[str, str]]:
    async def one(u: str):
        try:
            return await fetch_page(u, render=render)
        except Exception as e:
            return {"url": u, "error": str(e)}

    return await asyncio.gather(*(one(u) for u in urls))
