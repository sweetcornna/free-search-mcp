from __future__ import annotations

import abc
from dataclasses import asdict, dataclass, field
from typing import Any, Literal
from urllib.parse import urlparse

import httpx
from selectolax.parser import HTMLParser

from ..browser import pool
from ..config import settings


Freshness = Literal["day", "week", "month", "year"]
Category = Literal["news", "pdf", "github", "paper", "forum", "blog"]


# Host whitelists for category filtering when the engine has no native flag.
# Match-by-suffix so subdomains (e.g. www.arxiv.org) count.
_PAPER_HOSTS = (
    "arxiv.org",
    "acm.org",
    "springer.com",
    "ieee.org",
    "nature.com",
    "sciencedirect.com",
)
_FORUM_HOSTS = (
    "reddit.com",
    "news.ycombinator.com",
    "stackoverflow.com",
    "serverfault.com",
    "superuser.com",
)
_GITHUB_HOSTS = ("github.com", "gist.github.com")
# Major news outlets — used by category="news" since the default engine pool
# (DDG/Mojeek/Startpage) has no native news flag, so this filter would
# otherwise be a no-op.
_NEWS_HOSTS = (
    "reuters.com", "apnews.com", "bbc.com", "bbc.co.uk", "nytimes.com",
    "washingtonpost.com", "theguardian.com", "cnn.com", "nbcnews.com",
    "abcnews.go.com", "cbsnews.com", "foxnews.com", "npr.org",
    "bloomberg.com", "ft.com", "wsj.com", "economist.com", "cnbc.com",
    "axios.com", "politico.com", "thehill.com", "aljazeera.com",
    "techcrunch.com", "theverge.com", "arstechnica.com", "wired.com",
    "venturebeat.com", "engadget.com", "9to5mac.com", "9to5google.com",
    "theinformation.com", "businessinsider.com", "forbes.com",
    "news.google.com",  # Google News RSS items live here
)


@dataclass(slots=True)
class SearchFilters:
    """LLM-friendly filter set passed from the aggregator to each engine.
    Fields default to None / empty so callers can omit any subset."""

    freshness: Freshness | None = None
    include_domains: list[str] = field(default_factory=list)
    exclude_domains: list[str] = field(default_factory=list)
    category: Category | None = None
    include_text: str | None = None
    exclude_text: str | None = None

    def is_empty(self) -> bool:
        return (
            self.freshness is None
            and not self.include_domains
            and not self.exclude_domains
            and self.category is None
            and not self.include_text
            and not self.exclude_text
        )


@dataclass(slots=True)
class SearchResult:
    title: str
    url: str
    snippet: str
    engine: str
    rank: int

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


def _host(url: str) -> str:
    try:
        return (urlparse(url).hostname or "").lower()
    except ValueError:
        return ""


def _host_matches(host: str, suffixes: tuple[str, ...] | list[str]) -> bool:
    if not host:
        return False
    return any(host == s or host.endswith("." + s) for s in suffixes)


def _strip_query(url: str) -> str:
    return url.split("?", 1)[0].split("#", 1)[0]


def apply_post_filters(
    results: list[SearchResult], filters: SearchFilters | None
) -> list[SearchResult]:
    """Strict client-side filter pass. Engines under-honor URL operators, so
    we re-check domain/category/text constraints here."""
    if filters is None or filters.is_empty():
        return results

    inc = [d.lower().lstrip(".") for d in (filters.include_domains or [])]
    exc = [d.lower().lstrip(".") for d in (filters.exclude_domains or [])]
    inc_text = (filters.include_text or "").lower().strip()
    exc_text = (filters.exclude_text or "").lower().strip()

    out: list[SearchResult] = []
    for r in results:
        host = _host(r.url)

        if inc and not _host_matches(host, tuple(inc)):
            continue
        if exc and _host_matches(host, tuple(exc)):
            continue

        if filters.category == "paper" and not _host_matches(host, _PAPER_HOSTS):
            continue
        if filters.category == "forum" and not _host_matches(host, _FORUM_HOSTS):
            continue
        if filters.category == "github" and not _host_matches(host, _GITHUB_HOSTS):
            continue
        if filters.category == "news" and not _host_matches(host, _NEWS_HOSTS):
            continue
        if filters.category == "pdf" and not _strip_query(r.url).lower().endswith(".pdf"):
            continue
        if filters.category == "blog":
            # Blog = "ordinary web page" — exclude obvious non-blog hosts.
            if (
                _host_matches(host, _PAPER_HOSTS)
                or _host_matches(host, _FORUM_HOSTS)
                or _host_matches(host, _GITHUB_HOSTS)
                or _host_matches(host, _NEWS_HOSTS)
            ):
                continue

        if inc_text or exc_text:
            haystack = (r.title + " \n " + r.snippet).lower()
            if inc_text and inc_text not in haystack:
                continue
            if exc_text and exc_text in haystack:
                continue

        out.append(r)
    return out


def augment_query_with_operators(
    query: str,
    *,
    include_domains: list[str] | None = None,
    exclude_domains: list[str] | None = None,
    filetype: str | None = None,
) -> str:
    """Append `site:` / `-site:` / `filetype:` operators to a free-text query.
    These are universally understood by every engine we target, even when the
    engine has no dedicated URL parameter for the same constraint."""
    parts: list[str] = [query]
    if include_domains:
        if len(include_domains) == 1:
            parts.append(f"site:{include_domains[0]}")
        else:
            joined = " OR ".join(f"site:{d}" for d in include_domains)
            parts.append(f"({joined})")
    if exclude_domains:
        for d in exclude_domains:
            parts.append(f"-site:{d}")
    if filetype:
        parts.append(f"filetype:{filetype}")
    return " ".join(parts)


class Engine(abc.ABC):
    name: str
    needs_browser: bool = False
    wait_selector: str | None = None

    @abc.abstractmethod
    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str: ...

    @abc.abstractmethod
    def parse(self, html: str) -> list[SearchResult]: ...

    async def search(
        self,
        query: str,
        max_results: int,
        filters: SearchFilters | None = None,
    ) -> list[SearchResult]:
        url = self.build_url(query, max_results, filters)
        html = await self._fetch(url)
        results = self.parse(html)
        if not results and not self.needs_browser and settings.fetch_strategy == "auto":
            # HTTP succeeded but the page was an interstitial/captcha shell.
            _, html = await pool.fetch_html(url, wait_selector=self.wait_selector)
            results = self.parse(html)
        # Client-side post-filter BEFORE truncation, so we don't waste the budget
        # on hits that the engine returned but the user excluded.
        results = apply_post_filters(results, filters)[:max_results]
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
