from __future__ import annotations

import abc
import re
from dataclasses import asdict, dataclass, field
from datetime import datetime
from typing import Any, Literal
from urllib.parse import urlparse

from curl_cffi.requests import AsyncSession
from curl_cffi.requests.exceptions import RequestException
from selectolax.parser import HTMLParser

from ..browser import pool
from ..config import settings


# Pinned at chrome131 to match the desktop UA we send elsewhere. curl_cffi
# uses this token to set the JA3/JA4 + HTTP/2 SETTINGS fingerprint that real
# Chrome would emit, defeating naive headless detection (DDG anomaly page).
_IMPERSONATE = "chrome131"


Freshness = Literal["day", "week", "month", "year"]
Category = Literal["news", "pdf", "github", "paper", "forum", "blog"]


# Date-extraction patterns. Order matters: relative phrases ("2 days ago")
# are more LLM-friendly than reverse-engineering an ISO date from a vague
# "Apr 28" with no year, so we try them first.
_REL_RE = re.compile(
    r"\b(\d+)\s*(minute|hour|day|week|month|year)s?\s*ago\b",
    re.I,
)
# "Apr 28, 2026" or "Apr 28" (year optional, but we only normalise when present)
_ABS_RE_1 = re.compile(
    r"\b(Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)[a-z]*\s+(\d{1,2})(?:,\s*(\d{4}))?\b"
)
# ISO date 2024-12-01
_ABS_RE_2 = re.compile(r"\b(\d{4})-(\d{2})-(\d{2})\b")
# US/EU short date 12/01/2024 or 1/2/24
_ABS_RE_3 = re.compile(r"\b(\d{1,2})/(\d{1,2})/(\d{2,4})\b")


def extract_date_hint(text: str) -> str:
    """Return a normalised date string if one is present in ``text``.

    Output forms:
      * ``"YYYY-MM-DD"`` — when the input contains an unambiguous absolute date.
      * ``"N units ago"`` — when the input contains a relative phrase, lower-cased.
      * ``""``           — when nothing date-like was found.

    Best-effort: never raises, never guesses years for partial dates, and
    deliberately ignores ``"Today"`` / ``"Yesterday"`` because correct
    interpretation needs the engine's timezone, which we don't have.
    """
    if not text:
        return ""

    # Relative phrases beat absolute dates: they're shorter, self-describing,
    # and don't need timezone disambiguation.
    rel = _REL_RE.search(text)
    if rel:
        n, unit = rel.group(1), rel.group(2).lower()
        return f"{n} {unit}{'s' if int(n) != 1 else ''} ago"

    # ISO date wins next — least ambiguous.
    iso = _ABS_RE_2.search(text)
    if iso:
        try:
            d = datetime.strptime(iso.group(0), "%Y-%m-%d")
            return d.strftime("%Y-%m-%d")
        except ValueError:
            pass

    # "Apr 28, 2026" — only normalise when year is present.
    abs1 = _ABS_RE_1.search(text)
    if abs1 and abs1.group(3):
        raw = f"{abs1.group(1)} {abs1.group(2)}, {abs1.group(3)}"
        for fmt in ("%b %d, %Y", "%B %d, %Y"):
            try:
                d = datetime.strptime(raw, fmt)
                return d.strftime("%Y-%m-%d")
            except ValueError:
                continue

    # Numeric short date — try a few orderings, prefer m/d/Y (US/most engines).
    short = _ABS_RE_3.search(text)
    if short:
        raw = short.group(0)
        for fmt in ("%m/%d/%Y", "%m/%d/%y", "%d/%m/%Y", "%d/%m/%y"):
            try:
                d = datetime.strptime(raw, fmt)
                # Sanity: reject obviously bogus years (e.g. version numbers)
                if 1990 <= d.year <= datetime.now().year + 1:
                    return d.strftime("%Y-%m-%d")
            except ValueError:
                continue

    return ""


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
    # Human-readable publication hint pulled from the snippet/title, e.g.
    # ``"2 days ago"`` or ``"2026-04-28"``. Empty when no date was detected.
    # Surfaced to the LLM so date-sensitive queries don't require fetching
    # every URL just to check freshness.
    published_age: str = ""

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
        # curl_cffi sets the User-Agent matching the impersonated browser, so
        # we deliberately do NOT pass our own UA here — sending a mismatched UA
        # would re-introduce the very fingerprint discrepancy DDG checks for.
        try:
            async with AsyncSession(
                impersonate=_IMPERSONATE,
                timeout=settings.request_timeout,
                allow_redirects=True,
                headers={
                    "Accept-Language": settings.accept_language,
                    "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
                },
            ) as client:
                resp = await client.get(url)
                resp.raise_for_status()
                return resp.text
        except RequestException:
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
