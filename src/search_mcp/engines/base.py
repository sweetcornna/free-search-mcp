from __future__ import annotations

import abc
from dataclasses import asdict, dataclass
from typing import Any

import httpx
from selectolax.parser import HTMLParser

from ..browser import pool
from ..config import settings


@dataclass(slots=True)
class SearchResult:
    title: str
    url: str
    snippet: str
    engine: str
    rank: int

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


class Engine(abc.ABC):
    name: str
    needs_browser: bool = False
    wait_selector: str | None = None

    @abc.abstractmethod
    def build_url(self, query: str, max_results: int) -> str: ...

    @abc.abstractmethod
    def parse(self, html: str) -> list[SearchResult]: ...

    async def search(self, query: str, max_results: int) -> list[SearchResult]:
        url = self.build_url(query, max_results)
        html = await self._fetch(url)
        results = self.parse(html)[:max_results]
        if not results and not self.needs_browser and settings.fetch_strategy == "auto":
            # HTTP succeeded but the page was an interstitial/captcha shell.
            _, html = await pool.fetch_html(url, wait_selector=self.wait_selector)
            results = self.parse(html)[:max_results]
        for i, r in enumerate(results):
            r.rank = i + 1
            r.engine = self.name
        return results

    async def _fetch(self, url: str) -> str:
        if self.needs_browser or settings.fetch_strategy == "browser":
            _, html = await pool.fetch_html(url, wait_selector=self.wait_selector)
            return html
        try:
            async with httpx.AsyncClient(
                timeout=settings.request_timeout,
                follow_redirects=True,
                headers={
                    "User-Agent": settings.user_agent,
                    "Accept-Language": settings.accept_language,
                    "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
                },
                http2=False,
            ) as client:
                resp = await client.get(url)
                resp.raise_for_status()
                return resp.text
        except (httpx.HTTPError, httpx.HTTPStatusError):
            if settings.fetch_strategy == "http":
                raise
            _, html = await pool.fetch_html(url, wait_selector=self.wait_selector)
            return html


def text_of(node) -> str:
    if node is None:
        return ""
    return " ".join(node.text(separator=" ", strip=True).split())


def parse_html(html: str) -> HTMLParser:
    return HTMLParser(html)
