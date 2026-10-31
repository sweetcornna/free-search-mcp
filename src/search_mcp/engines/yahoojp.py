"""Yahoo! JAPAN 検索 (search.yahoo.co.jp) — Google's web index, HTML scrape, no API key.

Yahoo! JAPAN licenses Google's organic results and serves them as plain
server-rendered HTML, which makes it the keyless route to Google's index:
google.com itself answers a plain-HTTP SERP request with a JavaScript shell.
The interface is Japanese but the index is not — results follow the query's
language. Measured 2026-10-31 from a US egress, about 1.5 s a query over plain
HTTP: "python asyncio taskgroup exception handling" returned docs.python.org,
Stack Overflow, Reddit and GeeksforGeeks; "人工智能 最新进展" returned Chinese
sources (36kr, IT之家, 智源社区, Zhihu); `site:`, `-site:` and `filetype:pdf`
were all honoured.

Every organic hit is a `div.sw-Card.Algo` card. Its title anchor
(`a.sw-Card__titleInner`) carries the DIRECT target URL — clicks are tracked
with a `ping` attribute, not a redirect wrapper — and wraps the `h3` title plus
a favicon and breadcrumb, so the title is read from the `h3` alone. The snippet
is `p.sw-Card__summary`. Everything else on the page (Chiebukuro Q&A, news,
image and video carousels, "explore" suggestions, the AI answer panel) is a
sibling `sw-Card` with a different modifier class and is skipped by
construction, as are the site links (`Algo__oslLinks…`) nested inside a card.
No ad was served to this machine in any sample, including a commercial query
("クレジットカード おすすめ"); an ad would arrive as its own card type, not as
`Algo`, and the host check below backs that up.

URL parameters, measured against the unfiltered ranking:

* freshness is `vd=` with h / d / w / m1 / m3 / m6 / y, the values the page's
  own time menu sends. `vd=m` is NOT one of them and silently returns the
  unfiltered ranking, which is why month maps to `m1`.
* `n=` is ignored: a page is about ten results whatever is asked for.
* SafeSearch has no URL parameter; it is a per-browser preference (cookie), so
  Yahoo's default filtering applies.

Snippets lead with Yahoo's own date, "2026/10/1 -" or "5日前 -", which
`extract_date_hint` reads (non-confident, like every snippet date); the native
`vd=` filter is what enforces freshness for this engine.

Yahoo! JAPAN has not served the EEA or the UK since April 2022. From there the
request is answered with a notice page instead of results, which parses to `[]`
and is reported as a `region` gate (see `_GATE_MARKERS`), so the breaker
benches the engine and the hint suggests a proxy.

Best-effort by nature: this parses a page meant for humans, and the selectors
will need revisiting whenever Yahoo! JAPAN reshuffles its markup. An
unparseable page yields `[]` rather than an error, per the keyless-engine
contract.
"""

from __future__ import annotations

from urllib.parse import quote_plus, urlparse

from .base import (
    Engine,
    SearchFilters,
    SearchResult,
    augment_query_with_operators,
    extract_date_hint,
    inline_text,
    parse_html,
)

_ENDPOINT = "https://search.yahoo.co.jp/search"

# The page's own time menu: 1時間以内 h, 24時間以内 d, 1週間以内 w, 1か月以内 m1,
# 1年以内 y. A bare `m` is not accepted (see the module docstring).
_FRESHNESS = {"day": "d", "week": "w", "month": "m1", "year": "y"}

# Yahoo! JAPAN's own search pages and click/ad redirectors. Never a result.
# Other yahoo.co.jp hosts are kept: a Chiebukuro question or a Yahoo! News
# article that Google ranks organically is a real page, not navigation.
_INTERNAL_HOSTS = ("search.yahoo.co.jp", "rdsig.yahoo.co.jp", "ard.yahoo.co.jp")


def _is_internal(url: str) -> bool:
    try:
        host = (urlparse(url).hostname or "").lower()
    except ValueError:
        return True
    return any(host == h or host.endswith("." + h) for h in _INTERNAL_HOSTS)


class YahooJapanEngine(Engine):
    """Yahoo! JAPAN web results (keyless HTML scrape of Google's index)."""

    name = "yahoojp"
    description = (
        "Yahoo! JAPAN: Google's web index with no key, in any language; strongest for Japanese."
    )

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        # Google's operators pass straight through, and they are the only way
        # these filters reach the engine: there are no dedicated URL
        # parameters. Measured: `site:docs.python.org` returned ten
        # docs.python.org pages, `filetype:pdf` returned nine PDFs.
        filetype = "pdf" if filters and filters.category == "pdf" else None
        q = augment_query_with_operators(
            query,
            include_domains=filters.include_domains if filters else None,
            exclude_domains=filters.exclude_domains if filters else None,
            filetype=filetype,
        )
        # `ei=UTF-8` is what the search box sends; without it the query's
        # encoding is left for the server to guess.
        url = f"{_ENDPOINT}?p={quote_plus(q)}&ei=UTF-8"
        if filters and filters.freshness:
            token = _FRESHNESS.get(filters.freshness)
            if token:
                url += f"&vd={token}"
        return url

    def parse(self, html: str) -> list[SearchResult]:
        if not html:
            return []
        tree = parse_html(html)
        results: list[SearchResult] = []
        seen: set[str] = set()

        for card in tree.css("div.sw-Card.Algo"):
            anchor = card.css_first("a.sw-Card__titleInner")
            if anchor is None:
                continue
            url = (anchor.attributes.get("href") or "").strip()
            # The anchor's own text includes the site name and breadcrumb.
            title = inline_text(anchor.css_first("h3"))
            if not title or not url.startswith(("http://", "https://")):
                continue
            if _is_internal(url) or url in seen:
                continue
            seen.add(url)

            snippet = inline_text(card.css_first("p.sw-Card__summary"))
            results.append(
                SearchResult(
                    title=title,
                    url=url,
                    snippet=snippet,
                    engine=self.name,
                    rank=0,
                    # Scraped from arbitrary snippet text, so NOT confident —
                    # a page merely mentioning a date must not be dropped by
                    # the freshness filter.
                    published_age=extract_date_hint(snippet),
                )
            )
        return results
