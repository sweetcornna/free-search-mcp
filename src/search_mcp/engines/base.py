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
    # Open / preprint repositories + journal portals
    "biorxiv.org",
    "openreview.net",
    "paperswithcode.com",
    "semanticscholar.org",
    "plos.org",
    "ssrn.com",
    "jstor.org",
    "mdpi.com",
    "sciencemag.org",
    "frontiersin.org",
    "wiley.com",
    "tandfonline.com",
)
_FORUM_HOSTS = (
    "reddit.com",
    "news.ycombinator.com",
    "stackoverflow.com",
    "serverfault.com",
    "superuser.com",
    # Whole Stack Exchange network (math.stackexchange.com etc.)
    "stackexchange.com",
    # Other community discussion platforms
    "lobste.rs",
    "tildes.net",
    "lemmy.world",
    "lemmy.ml",
    "discourse.org",
)
# Code-hosting platforms. Kept the _GITHUB_HOSTS name for back-compat with
# tests that import it, but it now covers other public Git forges as well.
_GITHUB_HOSTS = (
    "github.com",
    "gist.github.com",
    "gitlab.com",
    "codeberg.org",
    "bitbucket.org",
    "sourceforge.net",
    "savannah.gnu.org",
    "git.sr.ht",
)
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


# Freshness windows, in days. A result older than the window is "outside" it.
# Generous upper bounds (month=31, year=366) avoid off-by-one over-dropping.
_FRESHNESS_MAX_DAYS = {"day": 1, "week": 7, "month": 31, "year": 366}

# Relative-phrase units -> approximate days. Coarse on purpose: we only need to
# decide in/out of a window, not compute an exact date.
_AGE_UNIT_DAYS = {
    "minute": 1.0 / 1440,
    "hour": 1.0 / 24,
    "day": 1.0,
    "week": 7.0,
    "month": 30.0,
    "year": 365.0,
}

_AGE_REL_RE = re.compile(
    r"\b(\d+)\s*(minute|hour|day|week|month|year)s?\s*ago\b", re.I
)
_AGE_ISO_RE = re.compile(r"\b(\d{4})-(\d{2})-(\d{2})\b")


def _published_age_in_days(published_age: str) -> float | None:
    """Best-effort: convert a ``published_age`` hint into an age in days.

    Handles the two shapes ``published_age`` ever holds (see
    :func:`extract_date_hint` / GoogleNews ``_format_pubdate``):
      * ``"N units ago"`` — relative phrase.
      * ``"YYYY-MM-DD"``  — ISO date.

    Returns ``None`` when the hint is empty or unparseable, which the caller
    treats as "unknown — keep" so we never over-drop.
    """
    if not published_age:
        return None
    s = published_age.strip()

    rel = _AGE_REL_RE.search(s)
    if rel:
        n = int(rel.group(1))
        unit = rel.group(2).lower()
        per = _AGE_UNIT_DAYS.get(unit)
        if per is not None:
            return n * per

    iso = _AGE_ISO_RE.search(s)
    if iso:
        try:
            d = datetime.strptime(iso.group(0), "%Y-%m-%d")
        except ValueError:
            return None
        age = (datetime.now() - d).total_seconds() / 86400.0
        # Future-dated (clock skew / TZ): treat as "now", i.e. age 0.
        return max(age, 0.0)

    return None


def apply_post_filters(
    results: list[SearchResult], filters: SearchFilters | None
) -> list[SearchResult]:
    """Strict client-side filter pass. Engines under-honor URL operators, so
    we re-check domain/category/text constraints here."""
    kept, _ = apply_post_filters_with_diagnostics(results, filters)
    return kept


def apply_post_filters_with_diagnostics(
    results: list[SearchResult], filters: SearchFilters | None
) -> tuple[list[SearchResult], dict[str, int]]:
    """Same logic as :func:`apply_post_filters` but also returns a
    ``drops_by_reason`` mapping so callers can explain *why* a sparse result
    set is sparse.

    Reason keys (only added when count > 0):
      - ``include_domains``
      - ``exclude_domains``
      - ``category_<paper|forum|github|news|pdf|blog>``
      - ``include_text``
      - ``exclude_text``

    Each result is counted against AT MOST ONE reason — the first filter that
    rejects it. This keeps the totals interpretable: ``sum(drops.values()) ==
    len(results) - len(kept)``.
    """
    drops: dict[str, int] = {}
    if filters is None or filters.is_empty():
        return list(results), drops

    inc = [d.lower().lstrip(".") for d in (filters.include_domains or [])]
    exc = [d.lower().lstrip(".") for d in (filters.exclude_domains or [])]
    inc_text = (filters.include_text or "").lower().strip()
    exc_text = (filters.exclude_text or "").lower().strip()

    def _bump(reason: str) -> None:
        drops[reason] = drops.get(reason, 0) + 1

    out: list[SearchResult] = []
    for r in results:
        host = _host(r.url)

        if inc and not _host_matches(host, tuple(inc)):
            _bump("include_domains")
            continue
        if exc and _host_matches(host, tuple(exc)):
            _bump("exclude_domains")
            continue

        if filters.category == "paper" and not _host_matches(host, _PAPER_HOSTS):
            _bump("category_paper")
            continue
        if filters.category == "forum" and not _host_matches(host, _FORUM_HOSTS):
            _bump("category_forum")
            continue
        if filters.category == "github" and not _host_matches(host, _GITHUB_HOSTS):
            _bump("category_github")
            continue
        if filters.category == "news" and not _host_matches(host, _NEWS_HOSTS):
            _bump("category_news")
            continue
        if filters.category == "pdf" and not _strip_query(r.url).lower().endswith(".pdf"):
            _bump("category_pdf")
            continue
        if filters.category == "blog":
            # Blog = "ordinary web page" — exclude obvious non-blog hosts.
            if (
                _host_matches(host, _PAPER_HOSTS)
                or _host_matches(host, _FORUM_HOSTS)
                or _host_matches(host, _GITHUB_HOSTS)
                or _host_matches(host, _NEWS_HOSTS)
            ):
                _bump("category_blog")
                continue

        if inc_text or exc_text:
            haystack = (r.title + " \n " + r.snippet).lower()
            if inc_text and inc_text not in haystack:
                _bump("include_text")
                continue
            if exc_text and exc_text in haystack:
                _bump("exclude_text")
                continue

        # Client-side freshness enforcement. Engines under-honor (baidu omits
        # any freshness param entirely) or silently ignore the freshness URL
        # operator, so re-check here using the parsed publication hint. We only
        # drop results we can PROVE are stale: an empty/unparseable
        # published_age is kept (unknown != old) to avoid over-dropping.
        if filters.freshness is not None:
            age_days = _published_age_in_days(r.published_age)
            if age_days is not None:
                max_days = _FRESHNESS_MAX_DAYS.get(filters.freshness)
                if max_days is not None and age_days > max_days:
                    _bump("freshness")
                    continue

        out.append(r)
    return out, drops


# --- safesearch / region wiring -------------------------------------------
# ``settings.safesearch`` ('strict'|'moderate'|'off') and ``settings.region``
# (a DDG-style 'cc-lang' token, e.g. 'us-en', 'uk-en') are user-facing knobs.
# Each engine spells these differently, so we centralise the per-engine value
# maps here and expose tiny helpers the engines call from build_url.

# DuckDuckGo html endpoint: kp=1 strict, kp=-1 moderate, kp=-2 off.
_DDG_SAFESEARCH = {"strict": "1", "moderate": "-1", "off": "-2"}
# Bing: adlt=strict|moderate|off maps 1:1 to our vocabulary.
_BING_SAFESEARCH = {"strict": "strict", "moderate": "moderate", "off": "off"}
# Brave: safesearch=strict|moderate|off maps 1:1.
_BRAVE_SAFESEARCH = {"strict": "strict", "moderate": "moderate", "off": "off"}
# Mojeek: safe is binary (1 = filter on, 0 = off). Treat strict/moderate as on.
_MOJEEK_SAFESEARCH = {"strict": "1", "moderate": "1", "off": "0"}
# Startpage: family filter is binary (1 = on, 0 = off).
_STARTPAGE_SAFESEARCH = {"strict": "1", "moderate": "1", "off": "0"}


def _region_to_bing_market(region: str) -> str:
    """Turn a 'cc-lang' region token ('us-en', 'uk-en') into a Bing mkt code
    ('en-US', 'en-GB'). Falls back to a sane default on malformed input."""
    if not region or "-" not in region:
        return "en-US"
    cc, _, lang = region.partition("-")
    cc = cc.strip().upper()
    lang = (lang.strip() or "en").lower()
    # Bing uses GB, not UK, for the United Kingdom country code.
    if cc == "UK":
        cc = "GB"
    return f"{lang}-{cc}"


def safesearch_param(engine: str) -> str | None:
    """Return the engine-specific safesearch value for the current setting, or
    ``None`` when the engine has no usable parameter / the map lacks the key."""
    val = settings.safesearch
    table = {
        "duckduckgo": _DDG_SAFESEARCH,
        "bing": _BING_SAFESEARCH,
        "brave": _BRAVE_SAFESEARCH,
        "mojeek": _MOJEEK_SAFESEARCH,
        "startpage": _STARTPAGE_SAFESEARCH,
    }.get(engine)
    if table is None:
        return None
    return table.get(val)


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
    # When parse() yields nothing on the HTTP path, the base search() retries
    # via a Playwright render to recover from interstitial/captcha shells.
    # That recovery only makes sense for HTML engines: an RSS/XML feed that
    # parsed to [] is genuinely empty (or malformed), and re-rendering it in a
    # headless browser just burns ~1s for the same empty result. RSS-backed
    # engines set this False to opt out of the wasted render.
    supports_browser_fallback: bool = True

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
        diagnostics: dict[str, Any] | None = None,
    ) -> list[SearchResult]:
        """Run the engine and return up to ``max_results`` filtered hits.

        When ``diagnostics`` is supplied, populate it in place with:
          * ``raw_per_engine[self.name]``       — pre-filter result count
          * ``after_filter_per_engine[self.name]`` — post-filter count (pre-truncate)
          * ``drops_by_reason``                 — accumulated reason→count map

        The aggregator passes a shared dict so totals merge across engines
        without changing the return signature (back-compat).
        """
        url = self.build_url(query, max_results, filters)
        html = await self._fetch(url)
        results = self.parse(html)
        if (
            not results
            and self.supports_browser_fallback
            and not self.needs_browser
            and settings.fetch_strategy == "auto"
        ):
            # HTTP succeeded but the page was an interstitial/captcha shell.
            _, html = await pool.fetch_html(url, wait_selector=self.wait_selector)
            results = self.parse(html)
        # Client-side post-filter BEFORE truncation, so we don't waste the budget
        # on hits that the engine returned but the user excluded.
        if diagnostics is not None:
            raw_count = len(results)
            filtered, drops = apply_post_filters_with_diagnostics(results, filters)
            diagnostics.setdefault("raw_per_engine", {})[self.name] = raw_count
            diagnostics.setdefault("after_filter_per_engine", {})[self.name] = len(filtered)
            agg = diagnostics.setdefault("drops_by_reason", {})
            for reason, n in drops.items():
                agg[reason] = agg.get(reason, 0) + n
            results = filtered[:max_results]
        else:
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
