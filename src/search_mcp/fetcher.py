from __future__ import annotations

import asyncio
import logging
from dataclasses import dataclass
from typing import Any

import httpx
from markdownify import markdownify as html_to_md
from selectolax.parser import HTMLParser

from .browser import pool
from .cache import cache
from .config import settings
from .formatting import estimate_tokens, smart_truncate
from .ratelimit import RateLimiter

log = logging.getLogger(__name__)
fetch_limiter = RateLimiter(settings.fetch_rate_limit_per_minute)


# Tags that contribute no content to a reader-mode view.
_BOILERPLATE = ("script", "style", "noscript", "nav", "header", "footer", "form", "aside", "iframe", "svg")


@dataclass(slots=True)
class FetchResult:
    url: str
    title: str
    content: str
    method: str
    truncated: bool
    tokens_estimated: int = 0

    def to_dict(self) -> dict[str, Any]:
        return {
            "url": self.url,
            "title": self.title,
            "content": self.content,
            "method": self.method,
            "truncated": self.truncated,
            "tokens_estimated": self.tokens_estimated,
        }


def _extract_main_html(html: str) -> tuple[str, str]:
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


def _truncate(text: str) -> tuple[str, bool]:
    return smart_truncate(text, settings.max_content_chars)


async def _http_fetch(url: str) -> tuple[str, str]:
    async with httpx.AsyncClient(
        timeout=settings.fetch_timeout,
        follow_redirects=True,
        headers={
            "User-Agent": settings.user_agent,
            "Accept-Language": settings.accept_language,
            "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        },
    ) as client:
        resp = await client.get(url)
        resp.raise_for_status()
        ctype = resp.headers.get("content-type", "")
        if "html" not in ctype and "xml" not in ctype:
            return "", resp.text
        return "", resp.text


async def fetch_page(
    url: str,
    *,
    render: str = "auto",
    force_refresh: bool = False,
) -> FetchResult:
    if not force_refresh:
        cached = await cache.get_page(url)
        if cached:
            content, truncated = _truncate(cached["content"])
            return FetchResult(
                url=url,
                title=cached.get("title", ""),
                content=content,
                method="cache",
                truncated=truncated,
                tokens_estimated=estimate_tokens(content),
            )

    await fetch_limiter.acquire("fetch")

    method = "http"
    title = ""
    html = ""
    last_err: Exception | None = None

    if render in ("auto", "http"):
        try:
            title, html = await _http_fetch(url)
        except Exception as e:
            last_err = e
            log.info("http fetch failed for %s: %s", url, e)

    needs_browser = render == "browser" or (render == "auto" and (not html or len(html) < 500))
    if needs_browser:
        try:
            title2, html2 = await pool.fetch_html(url)
            title = title2 or title
            html = html2
            method = "browser"
        except Exception as e:
            if not html:
                raise RuntimeError(f"fetch failed for {url}: {e}") from e
            log.warning("browser fallback failed for %s, using http body: %s", url, e)

    if not html:
        raise RuntimeError(f"empty response for {url}: {last_err}")

    extracted_title, main_html = _extract_main_html(html)
    title = title or extracted_title
    md = _to_markdown(main_html)

    await cache.put_page(url, title, md)
    content, truncated = _truncate(md)
    return FetchResult(
        url=url,
        title=title,
        content=content,
        method=method,
        truncated=truncated,
        tokens_estimated=estimate_tokens(content),
    )


async def fetch_many(urls: list[str], render: str = "auto") -> list[FetchResult | dict[str, str]]:
    async def one(u: str):
        try:
            return await fetch_page(u, render=render)
        except Exception as e:
            return {"url": u, "error": str(e)}

    return await asyncio.gather(*(one(u) for u in urls))
