from __future__ import annotations

import abc
import asyncio
import random
import re
from dataclasses import asdict, dataclass, field
from datetime import datetime
from typing import Any, Literal
from urllib.parse import urlparse

from curl_cffi.requests import AsyncSession
from curl_cffi.requests.exceptions import RequestException, Timeout
from selectolax.parser import HTMLParser

from ..browser import BrowserUnavailableError, pool
from ..config import settings

# Shared with the fetch path so search and fetch traffic always present the
# SAME browser fingerprint — a drift between the two is exactly the
# inconsistency naive headless detection (DDG anomaly page) looks for.
from ..httpfetch import IMPERSONATE
from ..net import curl_proxy_kwargs

Freshness = Literal["day", "week", "month", "year"]
Category = Literal["news", "pdf", "github", "paper", "forum", "blog", "image", "dataset"]


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
    "medrxiv.org",
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
    # Resolvers and indexes the keyless paper engines actually emit. Crossref
    # and OpenAlex return DOI links, PubMed returns NCBI links; without these
    # a category="paper" search discards its own best sources.
    "doi.org",
    "ncbi.nlm.nih.gov",
    "europepmc.org",
    "osf.io",
    "zenodo.org",
    "dblp.org",
    "researchgate.net",
    "aclanthology.org",
    "mlr.press",
    "neurips.cc",
    "openaccess.thecvf.com",
    # Non-Anglophone scholarly portals — same standing as the above.
    "cnki.net",
    "ci.nii.ac.jp",
    "j-stage.jst.go.jp",
    "hal.science",
    "scielo.org",
    "cambridge.org",
    "oup.com",
    "sagepub.com",
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
    # Non-Anglophone Q&A / discussion sites. zhihu is a registered engine, so
    # leaving it out meant `engines=["zhihu"], category="forum"` dropped every
    # result the engine returned.
    "zhihu.com",
    "v2ex.com",
    "segmentfault.com",
    "juejin.cn",
    "teratail.com",
    "qiita.com",
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
# Known news outlets — used by category="news" since the general web pool
# (DDG/Mojeek/Bing) has no native news flag, so this filter would otherwise be
# a no-op.
#
# This list is a FLOOR, not a definition of "news". There are tens of thousands
# of news outlets worldwide and no hand-maintained tuple will ever hold them;
# `_is_news_host` below therefore also accepts the `news.<domain>` naming
# convention, and results from engines that natively index news skip the
# hostname check entirely (see `apply_post_filters_with_diagnostics`).
_NEWS_HOSTS = (
    # Wire services and English-language majors
    "reuters.com", "apnews.com", "afp.com", "bbc.com", "bbc.co.uk",
    "nytimes.com", "washingtonpost.com", "theguardian.com", "cnn.com",
    "nbcnews.com", "abcnews.go.com", "cbsnews.com", "foxnews.com", "npr.org",
    "bloomberg.com", "ft.com", "wsj.com", "economist.com", "cnbc.com",
    "axios.com", "politico.com", "thehill.com", "aljazeera.com",
    "latimes.com", "usatoday.com", "nypost.com", "newsweek.com", "time.com",
    "theatlantic.com", "newyorker.com", "semafor.com", "independent.co.uk",
    "telegraph.co.uk", "sky.com", "cbc.ca", "abc.net.au", "smh.com.au",
    "straitstimes.com", "channelnewsasia.com", "scmp.com", "japantimes.co.jp",
    "thehindu.com", "timesofindia.indiatimes.com", "ndtv.com",
    # Tech press
    "techcrunch.com", "theverge.com", "arstechnica.com", "wired.com",
    "venturebeat.com", "engadget.com", "9to5mac.com", "9to5google.com",
    "theinformation.com", "businessinsider.com", "forbes.com",
    "theregister.com", "zdnet.com", "cnet.com", "techradar.com",
    "tomshardware.com", "restofworld.org", "thenextweb.com", "infoq.com",
    # Chinese-language outlets and tech press
    "xinhuanet.com", "people.com.cn", "chinadaily.com.cn", "cctv.com",
    "thepaper.cn", "caixin.com", "yicai.com", "jiemian.com", "sina.com.cn",
    "163.com", "sohu.com", "qq.com", "ifeng.com", "guancha.cn",
    "36kr.com", "jiqizhixin.com", "qbitai.com", "ithome.com", "cnbeta.com.tw",
    "udn.com", "ltn.com.tw", "chinatimes.com", "cna.com.tw", "hk01.com",
    # Other non-Anglophone majors
    "nikkei.com", "asahi.com", "yomiuri.co.jp", "nhk.or.jp", "mainichi.jp",
    "chosun.com", "joongang.co.kr", "hani.co.kr", "yna.co.kr",
    "lemonde.fr", "lefigaro.fr", "liberation.fr", "spiegel.de", "zeit.de",
    "faz.net", "welt.de", "sueddeutsche.de", "elpais.com", "elmundo.es",
    "corriere.it", "repubblica.it", "folha.uol.com.br", "globo.com",
    "tass.com", "themoscowtimes.com", "haaretz.com", "timesofisrael.com",
    "news.google.com",  # Google News RSS items live here
)

# Hosts that name themselves as news: `news.sina.com.cn`, `news.mydrivers.com`,
# `newsroom.example.co.jp`. Cheap, convention-based, and deliberately narrow —
# a bare "news"/"times"/"post" substring test would match postgresql.org and
# newsletter.example.com.
_NEWS_HOST_PREFIXES = ("news.", "newsroom.")
_NEWS_HOST_SUFFIXES = (".press", "news.com", "news.net", "news.cn", "news.org")


def _is_news_host(host: str) -> bool:
    """Allowlist membership OR the `news.<domain>` naming convention."""
    if _host_matches(host, _NEWS_HOSTS):
        return True
    return host.startswith(_NEWS_HOST_PREFIXES) or host.endswith(_NEWS_HOST_SUFFIXES)


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
    # True when ``published_age`` came from a STRUCTURED source (RSS pubDate, an
    # API date field) rather than a date scraped out of arbitrary snippet text.
    # Only confident absolute dates are trusted to DROP a result under a
    # freshness filter — otherwise a fresh page that merely *mentions* an old
    # date ("...founded in 2009...") would be wrongly dropped. Relative "N ago"
    # phrases are always trusted regardless of this flag. Internal-only: excluded
    # from to_dict() so it never leaks into tool output or the cache.
    published_age_confident: bool = False

    def to_dict(self) -> dict[str, Any]:
        d = asdict(self)
        d.pop("published_age_confident", None)
        return d


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
    results: list[SearchResult],
    filters: SearchFilters | None,
    *,
    native_category: bool = False,
) -> list[SearchResult]:
    """Strict client-side filter pass. Engines under-honor URL operators, so
    we re-check domain/category/text constraints here."""
    kept, _ = apply_post_filters_with_diagnostics(
        results, filters, native_category=native_category
    )
    return kept


def apply_post_filters_with_diagnostics(
    results: list[SearchResult],
    filters: SearchFilters | None,
    *,
    native_category: bool = False,
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

    ``native_category`` says these results came from an engine that natively
    indexes the requested category (``filters.category in engine.categories``).
    The hostname allowlists exist only to approximate a category for GENERAL web
    engines that have no such flag; re-applying them to a source that indexes
    the category by definition is not a safety net, it is data loss. GDELT is
    the clearest case: it indexes news in 100+ languages, and every non-Western
    outlet it returned used to be discarded by an Anglophone hostname tuple.
    Domain, text and freshness filters still apply — only the category-by-host
    guess is skipped.
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

        # `category="pdf"` is checked against the URL itself, not a hostname
        # guess, so it stays authoritative even for a native engine.
        if filters.category == "pdf" and not _strip_query(r.url).lower().endswith(".pdf"):
            _bump("category_pdf")
            continue
        if not native_category:
            if filters.category == "paper" and not _host_matches(host, _PAPER_HOSTS):
                _bump("category_paper")
                continue
            if filters.category == "forum" and not _host_matches(host, _FORUM_HOSTS):
                _bump("category_forum")
                continue
            if filters.category == "github" and not _host_matches(host, _GITHUB_HOSTS):
                _bump("category_github")
                continue
            if filters.category == "news" and not _is_news_host(host):
                _bump("category_news")
                continue
            if filters.category == "blog" and (
                # Blog = "ordinary web page" — exclude obvious non-blog hosts.
                _host_matches(host, _PAPER_HOSTS)
                or _host_matches(host, _FORUM_HOSTS)
                or _host_matches(host, _GITHUB_HOSTS)
                or _is_news_host(host)
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
                # Only DROP on a date we trust: a relative "N ago" phrase (an
                # explicit recency claim) or a date from a structured source
                # (RSS/API, published_age_confident). An absolute date scraped
                # from arbitrary snippet text is display-only — a fresh page that
                # merely mentions an old year must not be dropped.
                trusted = bool(
                    r.published_age_confident or _AGE_REL_RE.search(r.published_age)
                )
                max_days = _FRESHNESS_MAX_DAYS.get(filters.freshness)
                if trusted and max_days is not None and age_days > max_days:
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


# Google News editions are keyed by written script, not just language: the
# Simplified and Traditional Chinese editions are separate feeds and neither
# answers to a bare `zh`.
_GOOGLE_NEWS_SCRIPT = {
    ("zh", "TW"): "zh-Hant",
    ("zh", "HK"): "zh-Hant",
    ("zh", "MO"): "zh-Hant",
    ("zh", "CN"): "zh-Hans",
    ("zh", "SG"): "zh-Hans",
}


def region_to_google_params(region: str) -> tuple[str, str]:
    """Turn a 'cc-lang' region token into Google's ``(hl, gl)`` pair.

    'cn-zh' -> ('zh-CN', 'CN'). Falls back to US English on malformed input.
    """
    if not region or "-" not in region:
        return "en-US", "US"
    cc, _, lang = region.partition("-")
    cc = cc.strip().upper()
    lang = (lang.strip() or "en").lower()
    if cc == "UK":
        cc = "GB"
    if not (cc.isalpha() and len(cc) == 2 and lang.isalpha()):
        return "en-US", "US"
    return f"{lang}-{cc}", cc


def region_to_google_news_ceid(region: str) -> str:
    """Google News edition token, e.g. 'US:en', 'CN:zh-Hans', 'DE:de'.

    The News RSS endpoint is edition-scoped rather than merely
    language-hinted: asking the US/English edition for a Chinese query returns
    an EMPTY feed, not a translated or degraded one. Measured on
    'AI 新闻 最新进展': 0 items on US:en, 35 items on CN:zh-Hans.
    """
    hl, gl = region_to_google_params(region)
    lang = hl.partition("-")[0]
    return f"{gl}:{_GOOGLE_NEWS_SCRIPT.get((lang, gl), lang)}"


# --- query-script -> edition -----------------------------------------------
# Search backends are edition-scoped, and the edition is picked from settings
# rather than from the query, so a query written in a script the configured
# edition does not cover gets served badly or not at all. Measured against the
# Google News US/English edition (item counts, same query in each language):
#
#     Chinese   0 vs 35    <- empty feed; indistinguishable from an outage
#     Thai     12 vs 100
#     Hebrew   31 vs 100
#     Arabic   49 vs 100
#     Greek    69 vs 100
#     Russian 100 vs 100   <- already fine, native edition no worse
#     German/French/Spanish/Vietnamese/Turkish: 100 vs 100
#
# So this is a general quality fix (every non-Latin script gains or ties), of
# which Chinese is the pathological case. Latin-script languages are left on
# the configured edition: the measurements show nothing to gain, and script
# alone cannot tell German from English anyway — separating those needs real
# language ID, which is a dependency this project does not carry.
#
# Detection is by Unicode script: dependency-free, deterministic, and it covers
# every language whose writing system is distinctive, not a hand-picked few.
_SCRIPT_RANGES: tuple[tuple[str, tuple[tuple[int, int], ...]], ...] = (
    ("hangul", ((0x1100, 0x11FF), (0xA960, 0xA97F), (0xAC00, 0xD7A3))),
    ("kana", ((0x3040, 0x309F), (0x30A0, 0x30FF), (0x31F0, 0x31FF))),
    ("han", ((0x3400, 0x4DBF), (0x4E00, 0x9FFF), (0xF900, 0xFAFF))),
    ("cyrillic", ((0x0400, 0x04FF), (0x0500, 0x052F))),
    ("greek", ((0x0370, 0x03FF), (0x1F00, 0x1FFF))),
    ("hebrew", ((0x0590, 0x05FF),)),
    ("arabic", ((0x0600, 0x06FF), (0x0750, 0x077F), (0x08A0, 0x08FF))),
    ("thai", ((0x0E00, 0x0E7F),)),
    ("lao", ((0x0E80, 0x0EFF),)),
    ("khmer", ((0x1780, 0x17FF),)),
    ("myanmar", ((0x1000, 0x109F),)),
    ("devanagari", ((0x0900, 0x097F),)),
    ("bengali", ((0x0980, 0x09FF),)),
    ("gurmukhi", ((0x0A00, 0x0A7F),)),
    ("gujarati", ((0x0A80, 0x0AFF),)),
    ("tamil", ((0x0B80, 0x0BFF),)),
    ("telugu", ((0x0C00, 0x0C7F),)),
    ("kannada", ((0x0C80, 0x0CFF),)),
    ("malayalam", ((0x0D00, 0x0D7F),)),
    ("sinhala", ((0x0D80, 0x0DFF),)),
    ("georgian", ((0x10A0, 0x10FF), (0x1C90, 0x1CBF))),
    ("armenian", ((0x0530, 0x058F),)),
    ("ethiopic", ((0x1200, 0x137F),)),
)

# One region per script. Where a script serves several languages we take the
# largest by online news volume; a user who wants another sets SEARCH_MCP_REGION
# explicitly and keeps it (see the `configured` check below).
#
# Measured item counts from the Google News edition each row selects (vs. the
# US/English edition it replaces): zh 100/0, ko 102/102, ja 100/36, ru 100/100,
# el 100/69, he 100/31, ar 100/49, th 100/12, hi 100/100, bn 100/7, ta 100/6,
# te 100/6, gu 33/1, ml 29/0, pa 1/0, si 1/1, ka 1/1, am 1/1.
#
# fa, ur, km, my, kn and hy return 0 from EVERY url form tried (bare `hl=fa`,
# `hl=fa-IR`, alternate country codes, no locale at all) while an Arabic
# control returned 100 on the same harness — Google News does not run those
# editions. They are mapped anyway because the mapping is correct and costs
# nothing; do not re-test them expecting a url-form fix.
_SCRIPT_REGION = {
    "han": "cn-zh", "kana": "jp-ja", "hangul": "kr-ko",
    "cyrillic": "ru-ru", "greek": "gr-el", "hebrew": "il-he",
    "arabic": "eg-ar", "thai": "th-th", "lao": "la-lo",
    "khmer": "kh-km", "myanmar": "mm-my", "devanagari": "in-hi",
    "bengali": "in-bn", "gurmukhi": "in-pa", "gujarati": "in-gu",
    "tamil": "in-ta", "telugu": "in-te", "kannada": "in-kn",
    "malayalam": "in-ml", "sinhala": "lk-si", "georgian": "ge-ka",
    "armenian": "am-hy", "ethiopic": "et-am",
}

# Persian and Urdu are written in Arabic script but are separate editions.
# These letters do not occur in Arabic proper, so their presence is decisive.
_PERSIAN_CHARS = frozenset("پچژگک")
_URDU_CHARS = frozenset("ٹڈڑںھے")

# Share of alphabetic characters that must belong to one non-Latin script
# before we switch editions. Real queries mix in Latin tokens ("AI 新闻",
# "GPT-4 نموذج"), so a majority test would miss them; 30% is high enough that
# an English query with a stray glyph does not trip it.
_SCRIPT_SWITCH_RATIO = 0.3


def detect_query_region(query: str, configured: str) -> str:
    """Region token to use for ``query``, given the configured default.

    Returns ``configured`` unchanged for Latin-script queries and whenever the
    configured region already speaks the detected language — an operator who
    set ``tw-zh`` keeps Traditional Chinese rather than being rewritten to
    ``cn-zh``.
    """
    counts: dict[str, int] = {}
    letters = 0
    for ch in query:
        if not ch.isalpha():
            continue
        letters += 1
        o = ord(ch)
        for script, ranges in _SCRIPT_RANGES:
            if any(lo <= o <= hi for lo, hi in ranges):
                counts[script] = counts.get(script, 0) + 1
                break
    if not letters or not counts:
        return configured

    # Kana and Hangul are exclusive to Japanese and Korean, while Han is shared
    # with both. Japanese prose is largely kanji, so a plain "most frequent
    # script" vote reads 人工知能ニュース as Chinese (4 Han vs 4 kana, Han wins
    # the tie) and sends it to the Chinese edition — measured at 36 items
    # against 100 from the Japanese one. Presence of the exclusive script
    # decides; the ratio gate then applies to the CJK block as a whole so
    # "AI ニュース" still qualifies.
    cjk = counts.get("han", 0) + counts.get("kana", 0) + counts.get("hangul", 0)
    if counts.get("kana") or counts.get("hangul"):
        if cjk / letters < _SCRIPT_SWITCH_RATIO:
            return configured
        script = "hangul" if counts.get("hangul") else "kana"
        n = cjk
    else:
        script, n = max(counts.items(), key=lambda kv: kv[1])
    if n / letters < _SCRIPT_SWITCH_RATIO:
        return configured

    if script == "arabic":
        chars = set(query)
        if chars & _URDU_CHARS:
            detected = "pk-ur"
        elif chars & _PERSIAN_CHARS:
            detected = "ir-fa"
        else:
            detected = _SCRIPT_REGION["arabic"]
    else:
        detected = _SCRIPT_REGION.get(script, "")
    if not detected:
        return configured

    conf_lang = (configured or "").partition("-")[2].strip().lower()
    if conf_lang and conf_lang == detected.partition("-")[2]:
        return configured
    return detected


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


# --- gate detection --------------------------------------------------------
# Substrings that mark a "gated" response — a CAPTCHA / consent / login wall
# served INSTEAD of results. Lets engines turn a silent empty into an honest,
# actionable reason, and lets gated SERP engines trigger a searx fallback.
# Lower-cased haystack; order = priority (captcha > consent > login).
_GATE_MARKERS: tuple[tuple[str, tuple[str, ...]], ...] = (
    (
        "captcha",
        (
            "/sorry/index",          # Google "/sorry/" interstitial
            "unusual traffic",
            "/recaptcha/",
            "g-recaptcha",
            "h-captcha",
            "captcha-delivery",
            "px-captcha",
            "are you a robot",
            "verify you are a human",
            # DuckDuckGo's anomaly/challenge page (HTTP 202) never contains the
            # literal "captcha" — it shows an "anomaly-modal" asking the user to
            # "select all squares containing a duck". Without these markers a
            # gated DDG (the #1 default engine) returns a silent empty.
            "anomaly-modal",
            "made by a human",
            # Mojeek serves an ALTCHA proof-of-work challenge titled "Captcha"
            # whose markup shares none of the markers above, so a captcha-walled
            # Mojeek — one of the four DEFAULT engines — was reported as a
            # silent empty and sent the user chasing an IP block instead.
            "captcha-wrap",
            "altcha",
            "verification required",
            "complete the challenge",
        ),
    ),
    (
        # Not a challenge to solve — a JS-only shell served instead of results.
        # Google answers a plain-HTTP SERP request with a 200 whose body is
        # `table,div,span,p{display:none}` plus "Please click here if you are
        # not redirected", which parses to zero results and matches no captcha
        # marker, so it was indistinguishable from "this query found nothing".
        # Named to read correctly in the aggregator's "<engine> was
        # <reason>-gated" hint.
        "javascript",
        (
            "if you are not redirected",
            "please click here if you are not",
            "enable javascript to continue",
            "javascript is required",
        ),
    ),
    (
        "consent",
        (
            "consent.google.com",
            "consent.youtube.com",
            "before you continue",
            "consent.bing.com",
        ),
    ),
    (
        "login",
        (
            "请登录",                 # zhihu / generic CN login wall
            "登录知乎",
            "signflow",              # zhihu login modal class
            "sign in to continue",
            "you must log in",
            "please log in to continue",
        ),
    ),
)


class EngineKeyError(ValueError):
    """A keyed engine cannot run: its key is missing, rejected, or out of quota.

    Subclasses ValueError so existing `except ValueError` handling keeps
    working, but exists as its own type so the keyless never-raise boundary
    (see `JsonApiEngine.search`) can let *this* through while still swallowing
    transport failures and malformed payloads. Without the distinction, an
    unconfigured keyed engine reports "no results" — which reads as "nothing
    matched" rather than "you need to add a token".
    """


def raise_for_key_error(engine: str, status: int | None) -> None:
    """Turn an auth/quota HTTP status from a keyed engine into an actionable
    error, so a bad/expired key surfaces a hint instead of a silent empty.

    Called by the keyed engines AFTER their try/except, only when they got no
    results — so a healthy 200 never trips it. The aggregator catches the raised
    ValueError into its per-engine ``errors`` map. Network/transport failures
    (status is None) stay silent, preserving the "a flaky API never poisons the
    aggregator" contract; only an explicit auth/quota status raises.
    """
    if status in (401, 403):
        raise EngineKeyError(
            f"{engine}: the API key was rejected (HTTP {status}). Verify it in the "
            "admin UI (uv run search-mcp-admin) or the SEARCH_MCP_*_API_KEY env var."
        )
    if status == 422:
        raise EngineKeyError(
            f"{engine}: the API rejected the request (HTTP 422) — usually an "
            "invalid key or malformed parameters."
        )
    if status == 429:
        raise EngineKeyError(
            f"{engine}: rate limit / quota exceeded (HTTP 429). Slow down or raise "
            "the plan's limit."
        )


# --- transient-failure retry -------------------------------------------------
# ONE bounded retry for the keyless HTTP path. Deliberately narrow:
#   * connection errors (reset/refused) — fast failures, worth a single retry
#   * 429/5xx — transient server states; honor Retry-After up to a small cap
#   * timeouts — NEVER retried: a request_timeout expiry means the engine is
#     slow-dead and retrying doubles the whole search's latency envelope
# Happy path pays zero extra cost; a failing engine adds at most ~3s inside
# the aggregator's parallel gather.
_RETRYABLE_STATUSES = frozenset({429, 500, 502, 503, 504})
_RETRY_AFTER_CAP = 3.0
_MAX_ATTEMPTS = 2


def _is_retryable_status(status: int) -> bool:
    return status in _RETRYABLE_STATUSES


def _retry_after_seconds(headers: Any) -> float | None:
    """Parse a ``Retry-After`` header's delta-seconds form.

    The HTTP-date form is deliberately unsupported — with a 3s cap it could
    only ever clamp to the cap anyway. Returns ``None`` when the header is
    absent, negative, or unparseable.
    """
    try:
        raw = headers.get("Retry-After") if headers else None
    except Exception:
        return None
    if not raw:
        return None
    try:
        val = float(str(raw).strip())
    except ValueError:
        return None
    return val if val >= 0 else None


def detect_gate(html: str) -> str | None:
    """Best-effort: classify a page as a gate (``"captcha"``/``"consent"``/
    ``"login"``/``"javascript"``) when it carries a known wall marker, else
    ``None``.

    Used to (a) surface an honest reason instead of a silent empty result set,
    and (b) let gated SERP engines fall back to a working meta-search. Never
    raises; a normal results page returns ``None``."""
    if not html:
        return None
    low = html.lower()
    for reason, markers in _GATE_MARKERS:
        if any(m in low for m in markers):
            return reason
    return None


class Engine(abc.ABC):
    name: str
    needs_browser: bool = False
    wait_selector: str | None = None
    # Categories this engine natively indexes, from the `Category` literal
    # above. Empty (the default) means "general web" — the engine has no
    # special claim on any category and is only used when asked for by name or
    # via the default pool.
    #
    # A non-empty set is a routing signal, not a filter: `aggregate_search`
    # pulls these engines in when a caller asks for that category, because
    # asking a general web engine for `category="paper"` can only ever filter
    # its results by hostname, whereas arXiv actually indexes papers.
    categories: frozenset[str] = frozenset()
    # Per-engine override for the aggregator's rate limiter, when the source
    # documents a stricter rule than settings.rate_limit_per_minute. None means
    # "use the global default". Declared here so the number lives next to the
    # engine that has to obey it rather than in a table somewhere else.
    rate_limit_per_minute: int | None = None
    # How long the aggregator will wait for this engine's rate-limit token
    # before giving up and skipping it for this search. None means "wait as
    # long as it takes", which is right for a 30/min engine (sub-second) and
    # wrong for a source limited to one request every few seconds: search runs
    # in a parallel fan-out, so queueing behind a slow bucket would add that
    # delay to the WHOLE search. Skipping costs one source; waiting costs the
    # user's latency on every result.
    rate_limit_max_wait: float | None = None
    # TLS/JA3 + header fingerprint for this engine's HTTP requests. None means
    # "use the shared default" (httpfetch.IMPERSONATE).
    #
    # Worth overriding when a SERP is operated by a browser vendor and the
    # matching browser is the least surprising client it can see: Google gets a
    # current Chrome, Bing gets Edge. curl_cffi sends the whole coherent
    # profile — JA3, HTTP/2 SETTINGS, header order, User-Agent and the sec-ch-ua
    # client hints — so these stay mutually consistent, which a hand-set
    # User-Agent alone would not.
    impersonate: str | None = None
    # When parse() yields nothing on the HTTP path, the base search() retries
    # via a Playwright render to recover from interstitial/captcha shells.
    # That recovery only makes sense for HTML engines: an RSS/XML feed that
    # parsed to [] is genuinely empty (or malformed), and re-rendering it in a
    # headless browser just burns ~1s for the same empty result. RSS-backed
    # engines set this False to opt out of the wasted render.
    supports_browser_fallback: bool = True

    def is_available(self) -> bool:
        """Whether this engine can run right now, for AUTO-SELECTION only.

        Category routing consults this so an unconfigured keyed engine is not
        silently added to a pool it can only fail in. It deliberately does NOT
        gate an explicit `engines=[...]` request: someone who names an engine
        should get the actionable "your key is missing" error, not silence.
        """
        return True

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
            try:
                _, html = await pool.fetch_html(url, wait_selector=self.wait_selector)
                results = self.parse(html)
            except BrowserUnavailableError:
                # No Chromium installed: keep the (empty) HTTP outcome and let
                # the aggregator surface ONE actionable install hint instead of
                # a per-engine stack trace.
                if diagnostics is not None:
                    diagnostics.setdefault("gated", {})[self.name] = "browser_unavailable"
        # When we got nothing, check whether the page was a gate (CAPTCHA /
        # consent / login wall) and record an honest reason so the aggregator
        # can explain the empty result instead of silently dropping the engine.
        # setdefault on the ENGINE key too: a browser_unavailable reason
        # recorded above must not be clobbered by the gate classification of
        # the very shell the browser render was supposed to get past.
        if not results and diagnostics is not None:
            reason = detect_gate(html)
            if reason:
                diagnostics.setdefault("gated", {}).setdefault(self.name, reason)
        return self.finalize_results(results, filters, max_results, diagnostics)

    def finalize_results(
        self,
        results: list[SearchResult],
        filters: SearchFilters | None,
        max_results: int,
        diagnostics: dict[str, Any] | None = None,
    ) -> list[SearchResult]:
        """Post-filter, record diagnostics, truncate, and stamp rank/engine.

        Every ``search()`` implementation must end with exactly this contract —
        the base HTML path above AND each keyed JSON engine that overrides
        ``search()`` — so it lives in one place instead of being mirrored by
        hand in every override. Post-filtering happens BEFORE truncation, so
        the result budget is never wasted on hits the user excluded.

        An engine that declares the requested category in ``self.categories``
        is trusted for it: `categories` is documented above as a routing
        signal, and re-filtering a native source's output through the hostname
        allowlist contradicted that — it discarded 8 of 8 Crossref hits on
        `category="paper"` because Crossref emits doi.org links.
        """
        native = bool(filters and filters.category in self.categories)
        if diagnostics is not None:
            raw_count = len(results)
            filtered, drops = apply_post_filters_with_diagnostics(
                results, filters, native_category=native
            )
            diagnostics.setdefault("raw_per_engine", {})[self.name] = raw_count
            diagnostics.setdefault("after_filter_per_engine", {})[self.name] = len(filtered)
            agg = diagnostics.setdefault("drops_by_reason", {})
            for reason, n in drops.items():
                agg[reason] = agg.get(reason, 0) + n
            results = filtered[:max_results]
        else:
            results = apply_post_filters(
                results, filters, native_category=native
            )[:max_results]
        for i, r in enumerate(results):
            r.rank = i + 1
            r.engine = self.name
        return results

    async def _fetch(self, url: str) -> str:
        if self.needs_browser or settings.fetch_strategy == "browser":
            _, html = await pool.fetch_html(url, wait_selector=self.wait_selector)
            return html
        try:
            return await self._http_get(url)
        except RequestException as http_err:
            if settings.fetch_strategy == "http":
                raise
            try:
                _, html = await pool.fetch_html(url, wait_selector=self.wait_selector)
            except BrowserUnavailableError:
                # The browser can't rescue this and its absence is not the
                # cause — surface the real network error, not an install hint.
                raise http_err from None
            return html

    async def _http_get(self, url: str) -> str:
        """One HTTP GET with at most one retry for transient failures.

        Retry policy lives in the module-level ``_RETRYABLE_STATUSES`` /
        ``_retry_after_seconds`` helpers; timeouts are never retried. The
        session is reused across attempts so a retry doesn't re-pay the TLS
        handshake.
        """
        # curl_cffi sets the User-Agent matching the impersonated browser, so
        # we deliberately do NOT pass our own UA here — sending a mismatched UA
        # would re-introduce the very fingerprint discrepancy DDG checks for.
        async with AsyncSession(
            impersonate=self.impersonate or IMPERSONATE,
            timeout=settings.request_timeout,
            allow_redirects=True,
            headers={
                "Accept-Language": settings.accept_language,
                "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            },
            **curl_proxy_kwargs(self.name),
        ) as client:
            for attempt in range(_MAX_ATTEMPTS):
                last = attempt + 1 >= _MAX_ATTEMPTS
                try:
                    resp = await client.get(url)
                except Timeout:
                    raise  # slow-dead engine: retrying doubles search latency
                except RequestException:
                    if last:
                        raise
                    await asyncio.sleep(random.uniform(0.4, 0.8))
                    continue
                if not last and _is_retryable_status(resp.status_code):
                    delay = _retry_after_seconds(resp.headers)
                    if delay is not None and delay > _RETRY_AFTER_CAP:
                        # The server named a window our cap can't honor — a
                        # capped-sleep retry is a guaranteed second rejection,
                        # so fail now instead of burning ~3s + a round-trip.
                        resp.raise_for_status()
                    await asyncio.sleep(delay if delay is not None else 0.6)
                    continue
                resp.raise_for_status()
                return resp.text
        raise RequestException(f"{self.name}: retries exhausted for {url}")  # unreachable


def text_of(node) -> str:
    if node is None:
        return ""
    return " ".join(node.text(separator=" ", strip=True).split())


def parse_html(html: str) -> HTMLParser:
    return HTMLParser(html)
