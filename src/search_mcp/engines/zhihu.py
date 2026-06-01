"""Best-effort KEYLESS Zhihu (知乎) search via the public search-results page.

Zhihu's real search API (``api/v4/search_v3``) requires login cookies plus an
``x-zse-96`` request signature computed in obfuscated client JS, so there is no
clean keyless path through it. The ONLY no-credential option is to browser-render
the public web search page and scrape whatever result cards it renders.

This engine is OPT-IN / UNRELIABLE — in the same spirit as the baidu/brave
engines — because Zhihu aggressively gates headless clients: it frequently
serves a login wall or an anti-bot interstitial instead of results, even from a
fingerprinted browser. When that happens ``parse()`` legitimately finds no
result cards and returns ``[]``; an empty result set is the HONEST outcome here,
not an error. We therefore always render via the Playwright pool
(``needs_browser = True``) and never raise on a gated/garbage page.

Endpoint hit (GET, browser-rendered):
    https://www.zhihu.com/search?type=content&q=<query>
"""

from __future__ import annotations

from urllib.parse import quote_plus

from .base import (
    Engine,
    SearchFilters,
    SearchResult,
    extract_date_hint,
    parse_html,
    text_of,
)


def _normalize_url(href: str) -> str:
    """Normalise a Zhihu anchor href to an absolute https URL, or return ``""``
    for anything we can't (or shouldn't) link to.

    Cases handled:
      * ``//www.zhihu.com/...``        — protocol-relative -> prefix ``https:``
      * ``/question/..`` / ``/answer/..`` / other root-relative -> prefix host
      * absolute ``https://``          — kept as-is
      * empty / ``javascript:`` / login/anchor links -> dropped (``""``)
    """
    href = (href or "").strip()
    if not href:
        return ""
    low = href.lower()
    # Skip non-navigational / gating hrefs (login modals, JS handlers, anchors).
    if low.startswith(("javascript:", "#", "mailto:")):
        return ""
    if href.startswith("//"):
        return "https:" + href
    if href.startswith("/"):
        return "https://www.zhihu.com" + href
    if low.startswith("https://"):
        return href
    # Bare http or anything else unexpected: don't guess, drop it.
    if low.startswith("http://"):
        return "https://" + href[len("http://") :]
    return ""


class ZhihuEngine(Engine):
    name = "zhihu"
    # Zhihu serves a login wall / anti-bot interstitial to plain HTTP clients,
    # so the only no-key path is a full Playwright render. Even then it is
    # best-effort: a gated page yields zero cards.
    needs_browser = True
    # Best-effort wait target. Zhihu may show a login wall instead of cards, in
    # which case the wait times out and we parse whatever shell was rendered
    # (-> []). The pool treats a wait-selector timeout as non-fatal.
    wait_selector = ".SearchResult-Card"

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        # Zhihu's public web search has no documented stable URL params for
        # freshness / safesearch / region, and its operator support is poor, so
        # we send only the query. Domain/text/category constraints are enforced
        # client-side by the base search() post-filter pass.
        return f"https://www.zhihu.com/search?type=content&q={quote_plus(query)}"

    def parse(self, html: str) -> list[SearchResult]:
        # NEVER raise: a login wall / interstitial legitimately parses to [].
        tree = parse_html(html)
        results: list[SearchResult] = []
        seen: set[str] = set()
        # Try the dedicated search cards first, then fall back to the generic
        # content-item template Zhihu also uses on the results page.
        cards = tree.css("div.SearchResult-Card") or tree.css(".ContentItem")
        for card in cards:
            link = (
                card.css_first("h2 a")
                or card.css_first(".ContentItem-title a")
                or card.css_first("[itemprop=name] a")
            )
            if not link:
                continue
            url = _normalize_url(link.attributes.get("href") or "")
            if not url or url in seen:
                continue
            title = text_of(link)
            if not title:
                continue
            # Snippet: prefer the known excerpt nodes, else the longest text
            # block in the card so we still surface something useful.
            snippet_node = (
                card.css_first(".RichText")
                or card.css_first(".SearchItem-excerpt")
                or card.css_first(".CopyrightRichText-richText")
            )
            snippet = text_of(snippet_node)
            if not snippet:
                # Fallback: the longest text block that does NOT also contain the
                # title heading. Skipping title-bearing nodes is essential —
                # otherwise we'd pick the card's outer wrapper, whose text()
                # concatenates the title plus every other block, polluting the
                # snippet (mirrors google._longest_text_div).
                best = ""
                for node in card.css("p, span, div"):
                    if (
                        node.css_first("h2") is not None
                        or node.css_first(".ContentItem-title") is not None
                        or node.css_first("[itemprop=name]") is not None
                    ):
                        continue
                    t = text_of(node)
                    if len(t) > len(best):
                        best = t
                snippet = best
            seen.add(url)
            result = SearchResult(
                title=title, url=url, snippet=snippet, engine=self.name, rank=0
            )
            hint = extract_date_hint(snippet) or extract_date_hint(title)
            if hint:
                result.published_age = hint
            results.append(result)
        return results
