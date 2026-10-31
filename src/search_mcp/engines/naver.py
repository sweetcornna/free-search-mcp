"""Naver (search.naver.com) — Korea's main web index, HTML scrape, no API key.

Naver is where Korean-language pages are best ranked; Korean developer blogs
(velog.io, *.github.io, tistory) and portals surface here that the Western
indexes bury. English queries work too: measured 2026-10-31, "python asyncio
taskgroup" returned 15 web documents (Real Python, Stack Overflow, gihyo.jp)
in ~1.7 s over plain HTTP, with no consent or captcha wall.

Naver no longer has a separate web-documents tab: `where=web`, `where=webkr`
and `ssc=tab.web.all` all render the same unified page as the default
`where=nexearch` (every one reports `data-meta-ssc="tab.nx.all"`). That page
mixes Naver's own verticals into its ranked `urB_*` collections: blog and
cafe posts (`fds-ugc-*` cards), plus the AI briefing, power-link ads, news,
images and "Naver Mate" widgets around them. This engine takes only the web
documents, `div.fds-web-doc-root`. The UGC cards are Naver's blog and cafe
search, which have tabs of their own, and their links do not read over HTTP:
a blog.naver.com post is an iframe shell with no text, a cafe post a script
shell whose URL carries an expiring signed `art=` token. The cost is volume on
general Korean queries, which Naver answers mostly from blogs: "전세 사기 예방
방법" and "삼성전자 반도체 HBM" yielded 2 web documents each, tech queries 10-15.

Inside a document, `.sds-comps-*` / `fds-*` are the design system's semantic
class names (the hashed ones beside them are not used). The title is the
`headline1` text span and the snippet the `body1` one; selecting the spans
rather than their anchors keeps the visually hidden "새 창 열림" ("opens in a
new window") label out of both, and it is stripped again in case the markup
moves. The profile line above the title starts with the document's own date,
"2026.05.24." or "3주 전" (3 weeks ago), which becomes `published_age`.

Measured live 2026-10-31: `site:` and `-site:` are honoured (20 of 20 hits on
github.com); `filetype:pdf` steers toward PDFs rather than filtering (many come
from viewer URLs that do not end in .pdf); `nso=p:1d|1w|1m|1y` is Naver's own
time filter and every dated hit fell inside the window. No URL parameter for
adult filtering was found, so `safesearch` is not forwarded.

Best-effort by nature: this parses a page meant for humans. An unparseable
page yields `[]` rather than an error, per the keyless-engine contract.
"""

from __future__ import annotations

import re
from urllib.parse import quote_plus, urlparse

from .base import (
    Engine,
    SearchFilters,
    SearchResult,
    augment_query_with_operators,
    extract_date_hint,
    parse_html,
)

_ENDPOINT = "https://search.naver.com/search.naver"

# Naver's own period tokens, as its option links send them (`nso=p:1w`).
_FRESHNESS = {"day": "p:1d", "week": "p:1w", "month": "p:1m", "year": "p:1y"}

# Accessibility label inside every result anchor: "opens in a new window".
_NEW_WINDOW = "새 창 열림"

# Ad click redirects. Ads render in their own section, never as a web
# document; this only keeps one out if that ever changes.
_AD_HOSTS = ("ader.naver.com", "ad.search.naver.com")

_ABS_DATE = re.compile(r"^(\d{4})\.(\d{1,2})\.(\d{1,2})\.")
_REL_DATE = re.compile(r"^(\d+)\s*(분|시간|일|주|개월|년)\s*전")
_REL_UNIT = {
    "분": "minute",
    "시간": "hour",
    "일": "day",
    "주": "week",
    "개월": "month",
    "년": "year",
}


def _text(node) -> str:
    """Visible text with no separator at tag boundaries.

    Naver wraps query terms in `<mark>`, often mid-word; a space at each
    boundary printed "asyncio . TaskGroup" and "파이썬 에서는".
    """
    if node is None:
        return ""
    return " ".join(node.text(separator="").replace(_NEW_WINDOW, " ").split())


def _profile_date(text: str) -> str:
    """Normalise the date that opens a document's profile line, else ""."""
    m = _ABS_DATE.match(text)
    if m:
        y, mo, d = (int(g) for g in m.groups())
        return f"{y:04d}-{mo:02d}-{d:02d}"
    m = _REL_DATE.match(text)
    if m:
        n, unit = int(m.group(1)), _REL_UNIT[m.group(2)]
        return f"{n} {unit}{'s' if n != 1 else ''} ago"
    return ""


def _enclosing_anchor(node):
    while node is not None and node.tag != "a":
        node = node.parent
    return node


class NaverEngine(Engine):
    """Naver web-document results (keyless HTML scrape)."""

    name = "naver"
    description = "Naver: Korea's main web index; reach for Korean-language queries and sites."

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        filetype = "pdf" if filters and filters.category == "pdf" else None
        q = augment_query_with_operators(
            query,
            include_domains=filters.include_domains if filters else None,
            exclude_domains=filters.exclude_domains if filters else None,
            filetype=filetype,
        )
        # Naver has no result-count parameter; one page is what it serves.
        url = f"{_ENDPOINT}?where=nexearch&query={quote_plus(q)}"
        if filters and filters.freshness:
            token = _FRESHNESS.get(filters.freshness)
            if token:
                url += f"&sm=tab_opt&nso={quote_plus(token)}"
        return url

    def parse(self, html: str) -> list[SearchResult]:
        if not html:
            return []
        tree = parse_html(html)
        results: list[SearchResult] = []
        seen: set[str] = set()

        for doc in tree.css("div.fds-web-doc-root"):
            head = doc.css_first(".sds-comps-text-type-headline1")
            anchor = _enclosing_anchor(head)
            if anchor is None:
                continue
            url = (anchor.attributes.get("href") or "").strip()
            title = _text(head)
            if not title or not url.startswith(("http://", "https://")):
                continue
            host = (urlparse(url).hostname or "").lower()
            if host in _AD_HOSTS or url in seen:
                continue
            seen.add(url)

            snippet = _text(doc.css_first(".sds-comps-text-type-body1"))
            date = _profile_date(_text(doc.css_first(".sds-comps-profile-info-subtext")))
            results.append(
                SearchResult(
                    title=title,
                    url=url,
                    snippet=snippet,
                    engine=self.name,
                    rank=0,
                    # Scraped from the page, so NOT confident; the absolute
                    # form is display-only under a freshness filter.
                    published_age=date or extract_date_hint(snippet),
                )
            )
        return results
