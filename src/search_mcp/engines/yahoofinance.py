"""Yahoo Finance search — resolve a ticker, and the news attached to it. Keyless.

  GET https://query2.finance.yahoo.com/v1/finance/search?q=<q>&quotesCount=&newsCount=

This endpoint is an ENTITY RESOLVER, and treating it as a news search gets you
nonsense. Two measured behaviours shape the whole engine:

  * A multi-word query stops resolving. `q=nvidia` returns
    `["NVDA", "NVHE-U.TO", "NVDX", ...]`; `q=nvidia earnings` returns an empty
    `quotes[]`. So `_entity_query` strips the finance filler words people
    naturally attach to a company name before the request goes out.

  * `news[]` is NOT scoped to the query. Asked about Nvidia it returned, in
    order, an emotional-support-animal ad, a supercar piece, and a Mourinho
    football story. It is a generic markets feed that happens to ride along.
    So news items are kept only when they actually mention the query or the
    resolved instrument — see `_relevant`.

What survives is what the source is genuinely good at: turning a company name
into a symbol, with exchange/sector/industry, plus the coverage that really is
about that company. `fetch` on a quote URL opens the instrument's Yahoo Finance
page; on a news URL, the article.
"""

from __future__ import annotations

from datetime import UTC, datetime
from typing import Any
from urllib.parse import quote_plus

from .base import SearchFilters, SearchResult
from .jsonapi import JsonApiEngine, clip

_ENDPOINT = "https://query2.finance.yahoo.com/v1/finance/search"
_QUOTE = "https://finance.yahoo.com/quote/{symbol}"

# Filler people attach to a company name that stops Yahoo resolving the entity.
# Stripped for the REQUEST only; the original query still decides which news
# items are relevant.
_FILLER = frozenset(
    {
        "earnings", "earning", "stock", "stocks", "share", "shares", "price",
        "prices", "news", "quote", "quotes", "forecast", "outlook", "revenue",
        "guidance", "results", "result", "report", "financials", "dividend",
        "valuation", "analysis", "the", "for", "of", "and",
        "fy", "q1", "q2", "q3", "q4",
        "股价", "财报", "业绩", "股票", "行情", "营收", "预测",
    }
)


def _entity_query(query: str) -> str:
    """Drop finance filler so the resolver sees the entity, not the question.

    Falls back to the original when stripping would leave nothing — a query of
    pure filler has no entity to find, and an empty `q` is not a search.
    """
    kept = [tok for tok in query.split() if tok.strip().lower() not in _FILLER]
    return " ".join(kept) or query


# Dropped from company names when deriving relevance terms — every issuer has
# them, so they would match every headline.
_CORPORATE_SUFFIXES = frozenset(
    {"inc", "inc.", "corp", "corp.", "corporation", "ltd", "ltd.", "plc",
     "holdings", "group", "company", "co", "co.", "sa", "ag", "nv", "the"}
)


class YahooFinanceEngine(JsonApiEngine):
    """Yahoo Finance instrument + market-news search (keyless JSON API)."""

    name = "yahoofinance"
    description = "Yahoo Finance: resolve tickers and pull recent market news for a company."
    categories = frozenset({"finance", "finance.market"})

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        n = max(1, min(max_results, 20))
        # Split the budget: enough quotes to disambiguate a name, the rest to
        # news. A symbol lookup rarely needs more than a handful of candidates.
        quotes = min(n, 6)
        # Ask for more news than we will keep — most of the feed is unrelated
        # and gets filtered out below.
        return (
            f"{_ENDPOINT}?q={quote_plus(_entity_query(query))}"
            f"&quotesCount={quotes}&newsCount={min(n * 3, 20)}"
        )

    async def fetch_results(
        self, query: str, max_results: int, filters: SearchFilters | None
    ) -> list[SearchResult]:
        """Overridden only to keep the ORIGINAL query for relevance filtering.

        `map_results(payload)` cannot see it, and without it there is no way to
        tell a genuine Nvidia story from the generic markets feed riding along
        in the same response.
        """
        payload = await self._get_json(self.build_url(query, max_results, filters))
        if payload is None or not isinstance(payload, dict):
            return []
        quotes = self._quotes(payload.get("quotes"))
        news = self._news(payload.get("news"), query, quotes)
        # Quotes come first because a symbol is usually the prerequisite for
        # everything else — but they must not eat the whole budget. Yahoo
        # returns every listing, ETF and depositary receipt for a name (six
        # rows for "tesla"), which would leave `finalize_results` nothing to
        # truncate but quotes. Half the budget each, and news reclaims any
        # room the quotes do not use.
        cap = max(1, max_results // 2)
        return quotes[:cap] + news + quotes[cap:]

    def map_results(self, payload: Any) -> list[SearchResult]:
        # Reachable only if a caller bypasses fetch_results; without the query
        # there is nothing to filter news against, so quotes only.
        if not isinstance(payload, dict):
            return []
        return self._quotes(payload.get("quotes"))

    def _quotes(self, quotes: Any) -> list[SearchResult]:
        if not isinstance(quotes, list):
            return []
        out: list[SearchResult] = []
        for q in quotes:
            if not isinstance(q, dict):
                continue
            symbol = q.get("symbol")
            if not isinstance(symbol, str) or not symbol.strip():
                continue
            name = q.get("longname") or q.get("shortname") or symbol
            out.append(
                SearchResult(
                    title=clip(f"{symbol} — {name}", cap=300),
                    url=_QUOTE.format(symbol=quote_plus(symbol)),
                    snippet=self._quote_snippet(q),
                    engine=self.name,
                    rank=0,
                )
            )
        return out

    @staticmethod
    def _quote_snippet(q: dict[str, Any]) -> str:
        bits: list[str] = []
        for key in ("typeDisp", "exchDisp", "sectorDisp", "industryDisp"):
            value = q.get(key)
            if isinstance(value, str) and value:
                bits.append(value)
        return clip(" · ".join(bits))

    def _news(
        self, news: Any, query: str, quotes: list[SearchResult]
    ) -> list[SearchResult]:
        if not isinstance(news, list):
            return []
        terms = self._relevance_terms(query, quotes)
        out: list[SearchResult] = []
        for item in news:
            if not isinstance(item, dict):
                continue
            title = item.get("title")
            link = item.get("link")
            if not isinstance(title, str) or not title.strip():
                continue
            if not isinstance(link, str) or not link.startswith("http"):
                continue
            if not self._relevant(title, terms):
                continue
            published = self._published(item.get("providerPublishTime"))
            publisher = item.get("publisher")
            bits = [publisher] if isinstance(publisher, str) and publisher else []
            if published:
                bits.append(published)
            out.append(
                SearchResult(
                    title=clip(title, cap=300),
                    url=link,
                    snippet=clip(" · ".join(bits)),
                    engine=self.name,
                    rank=0,
                    published_age=published,
                    # A real publish timestamp from the API, not a date scraped
                    # out of prose — freshness may drop on it.
                    published_age_confident=bool(published),
                )
            )
        return out

    @staticmethod
    def _relevance_terms(query: str, quotes: list[SearchResult]) -> set[str]:
        """Lower-cased tokens that make a headline about THIS query.

        Built from the query's own content words plus every symbol and company
        word the resolver returned, so "NVDA" in a headline counts as a match
        for a query that only said "nvidia".
        """
        terms = {
            tok.strip(".,;:!?()[]\"'").lower()
            for tok in query.split()
            if len(tok) > 2 and tok.strip().lower() not in _FILLER
        }
        # Every symbol counts — "NVDA" in a headline is about the query that
        # resolved to NVDA. But company WORDS come from the top match only.
        # Yahoo returns leveraged ETFs and income funds alongside the issuer
        # ("Harvest NVIDIA Enhanced High Income Shares ETF"), and harvesting
        # words from those admitted headlines on the strength of "high" —
        # "Down 35% From Its High, Is Sandisk Stock a Bargain" scored as an
        # Nvidia story.
        for quote in quotes:
            symbol, _, _company = quote.title.partition(" — ")
            terms.add(symbol.strip().lower())
        if quotes:
            _symbol, _, company = quotes[0].title.partition(" — ")
            terms.update(
                word.strip(".,").lower()
                for word in company.split()
                if len(word) > 2 and word.strip().lower() not in _CORPORATE_SUFFIXES
            )
        return {t for t in terms if t}

    @staticmethod
    def _relevant(title: str, terms: set[str]) -> bool:
        """Keep a headline only when it names the thing that was asked about.

        With no terms at all — the query resolved to nothing — NOTHING is kept:
        an unresolved query gets the generic markets feed, and passing that off
        as a search result is worse than returning nothing.
        """
        if not terms:
            return False
        low = title.lower()
        return any(term in low for term in terms)

    @staticmethod
    def _published(value: Any) -> str:
        """Unix seconds -> `YYYY-MM-DD`, or "" for anything unparseable."""
        if not isinstance(value, (int, float)) or isinstance(value, bool):
            return ""
        try:
            return datetime.fromtimestamp(value, tz=UTC).strftime("%Y-%m-%d")
        except (OverflowError, OSError, ValueError):
            return ""
