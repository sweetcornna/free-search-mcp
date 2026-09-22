"""SEC EDGAR full-text search — every US regulatory filing since 2001. Keyless.

  GET https://efts.sec.gov/LATEST/search-index?q=<q>&forms=<types>&...

This is the primary source for anything a US-listed company said on the record:
10-K risk factors, 10-Q results, 8-K material events, proxy statements. A web
engine can only find commentary ABOUT a filing; this searches the filing text.

**The User-Agent matters.** SEC blocks agents whose UA embeds a URL — the
package's usual `free-search-mcp/1.0 (+https://github.com/...)` string is
answered with a 403 while a plain `name email` form gets a 200. The SEC's own
guidance asks for a declarative name plus a contact address, so that is exactly
what `_user_agent()` sends, with `settings.contact_email` filled in when the
operator has set one.

A query that names a ticker is scoped to that issuer. EDGAR's relevance
ranking is otherwise dominated by structured-product pricing supplements —
"NVDA risk factors" led with four ProShares/Investment Managers 497Ks that
merely mention the ticker — while `entityName=NVDA` returns NVIDIA's own 10-Qs.
`entityName` resolves tickers and company names server-side, so no local
ticker table is needed.

Results point at the filing document in the EDGAR archive, so `fetch` and
`read_doc` can open the actual text.
"""

from __future__ import annotations

import re
from datetime import datetime, timedelta
from typing import Any
from urllib.parse import quote_plus

from ..config import settings
from .base import SearchFilters, SearchResult
from .jsonapi import JsonApiEngine, clip

_ENDPOINT = "https://efts.sec.gov/LATEST/search-index"
_ARCHIVE = "https://www.sec.gov/Archives/edgar/data/{cik}/{accession}/{document}"

# EDGAR's date window is opt-in: `startdt`/`enddt` are ignored unless
# `dateRange=custom` is sent alongside them (measured — without it the hit count
# is identical to an unfiltered search).
_FRESHNESS_DAYS = {"day": 1, "week": 7, "month": 31, "year": 366}

# Form types people actually name in a question ("NVDA 10-K risk factors").
# EDGAR's relevance ranking alone is dominated by structured-product pricing
# supplements — measured on "artificial intelligence risk", an unfiltered
# search returned 276 hits led by 8-K and 10-Q boilerplate, while the same
# query with `forms=10-K` returned 20, all of them annual reports. So when the
# query names a form, route it to the `forms` parameter (which filters) instead
# of leaving it in `q` (which merely matches documents that MENTION the form).
#
# Deliberately excludes short, ambiguous codes like `SD`, `ARS` and a bare
# `425`: they collide with ordinary words and numbers in a free-text query.
_FORM_RE = re.compile(
    r"\b("
    r"10-K(?:/A)?|10-Q(?:/A)?|8-K(?:/A)?|20-F|40-F|6-K|"
    r"S-1|S-3|S-4|F-1|11-K|N-CSR|"
    r"424B[0-9]|13F(?:-HR)?|DEF\s?14A|DEFA\s?14A|SC\s?13[DG]"
    r")\b",
    re.I,
)


def _split_forms(query: str) -> tuple[str, list[str]]:
    """Pull form types out of a free-text query.

    Returns `(remaining_query, forms)`. The form tokens are removed from the
    query text so they cannot also act as search terms. A query that is NOTHING
    but form names keeps its original text — `forms=` alone with an empty `q`
    is not a search.
    """
    forms: list[str] = []
    for match in _FORM_RE.finditer(query):
        form = " ".join(match.group(1).split()).upper()
        if form not in forms:
            forms.append(form)
    if not forms:
        return query, []
    stripped = " ".join(_FORM_RE.sub(" ", query).split())
    return (stripped or query), forms


# A ticker as a person writes one: upper case, 1-5 letters, optionally with a
# share-class suffix (`BRK.B`). Case is load-bearing — lower-cased, the ticker
# space is a minefield of ordinary words (`IT`, `ALL`, `ON`, `SO`, `NOW`, `GO`,
# `CAR` are all real tickers), so "all filings on risk" would name three
# unrelated issuers.
_TICKER_RE = re.compile(r"^[A-Z]{1,5}(?:[.-][A-Z]{1,2})?$")

# Upper-cased words that are near-always the abbreviation, not the issuer. Some
# of these ARE tickers (`AI` is C3.ai) — the point is which reading a person
# meant. A wrong guess is not fatal either way: `fetch_results` re-runs the
# search unscoped when the scoped one comes back empty.
_NOT_TICKERS = frozenset(
    {
        "A", "I", "AND", "OR", "THE", "IN", "OF", "TO", "AI", "ML", "US", "USA",
        "UK", "EU", "CEO", "CFO", "CTO", "COO", "IPO", "ESG", "GDP", "SEC",
        "IRS", "FDA", "FTC", "DOJ", "EPS", "ETF", "REIT", "SPAC", "PDF", "API",
        "LLC", "LLP", "PLC", "INC", "LTD", "NYSE", "IT", "ALL", "NEW", "ON",
        "SO", "GO", "NOW", "BY", "FOR", "ARE", "CAN", "HAS", "ITS", "OUT",
    }
)


def _split_ticker(query: str) -> tuple[str, str]:
    """Pull a ticker out of a free-text query. Returns `(query, entity)`.

    The ticker is LEFT in the query text: `entityName` narrows to the issuer
    while `q` still has to match something, and dropping the only distinctive
    term from a query like "NVDA" would leave `q` empty.
    """
    for raw in query.split():
        token = raw.strip(".,;:!?()[]\"'")
        # `original != upper` rejects "Nvda" and "nvda" — see the case note on
        # `_TICKER_RE`.
        if not token or token != token.upper() or token in _NOT_TICKERS:
            continue
        if _TICKER_RE.fullmatch(token):
            return query, token
    return query, ""


def _user_agent() -> str:
    """SEC-acceptable identifier: a name and a contact, and NO URL.

    A UA containing `https://...` is answered with 403 by efts.sec.gov, which is
    why this engine cannot use `jsonapi.USER_AGENT` like its siblings.
    """
    contact = (settings.contact_email or "").strip()
    return f"free-search-mcp {contact}" if contact else "free-search-mcp"


class SecEdgarEngine(JsonApiEngine):
    """SEC EDGAR filing full-text search (keyless JSON API)."""

    name = "sec_edgar"
    description = "SEC EDGAR full-text search over US filings: 10-K, 10-Q, 8-K, proxies."
    categories = frozenset({"finance", "finance.filings"})
    # A browser fingerprint is the wrong thing to present to a government
    # bulk-data endpoint that asks callers to identify themselves.
    impersonate = None

    @property
    def api_headers(self) -> dict[str, str]:  # type: ignore[override]
        return {"User-Agent": _user_agent(), "Accept": "application/json"}

    async def fetch_results(
        self, query: str, max_results: int, filters: SearchFilters | None
    ) -> list[SearchResult]:
        """Scoped search first; fall back to the unscoped one if it is empty.

        The ticker heuristic is a guess, and a wrong guess would otherwise turn
        a query that had answers into one that has none. The retry only costs a
        request in the case that was already going to disappoint.
        """
        payload = await self._get_json(self.build_url(query, max_results, filters))
        results = self.map_results(payload) if payload is not None else []
        if results:
            return results
        _, entity = _split_ticker(_split_forms(query)[0])
        if not entity:
            return results
        payload = await self._get_json(
            self.build_url(query, max_results, filters, scoped=False)
        )
        return self.map_results(payload) if payload is not None else []

    def build_url(
        self,
        query: str,
        max_results: int,
        filters: SearchFilters | None = None,
        *,
        scoped: bool = True,
    ) -> str:
        text, forms = _split_forms(query)
        text, entity = _split_ticker(text)
        params = [f"q={quote_plus(text)}"]
        if entity and scoped:
            params.append(f"entityName={quote_plus(entity)}")
        if forms:
            params.append(f"forms={quote_plus(','.join(forms))}")
        if filters and filters.freshness:
            days = _FRESHNESS_DAYS.get(filters.freshness)
            if days:
                end = datetime.now().date()
                start = end - timedelta(days=days)
                params.append("dateRange=custom")
                params.append(f"startdt={start.isoformat()}")
                params.append(f"enddt={end.isoformat()}")
        return f"{_ENDPOINT}?{'&'.join(params)}"

    def map_results(self, payload: Any) -> list[SearchResult]:
        if not isinstance(payload, dict):
            return []
        hits = payload.get("hits")
        if not isinstance(hits, dict):
            return []
        items = hits.get("hits")
        if not isinstance(items, list):
            return []

        results: list[SearchResult] = []
        for item in items:
            if not isinstance(item, dict):
                continue
            src = item.get("_source")
            if not isinstance(src, dict):
                continue
            url = self._document_url(item.get("_id"), src)
            if not url:
                continue
            title = self._title(src)
            if not title:
                continue
            date = src.get("file_date")
            date = date if isinstance(date, str) else ""
            results.append(
                SearchResult(
                    title=clip(title, cap=300),
                    url=url,
                    snippet=self._snippet(src),
                    engine=self.name,
                    rank=0,
                    published_age=date,
                    # A filing date is a filing date: EDGAR stamps it, so
                    # freshness may be trusted to drop on it.
                    published_age_confident=bool(date),
                )
            )
        return results

    @staticmethod
    def _document_url(doc_id: Any, src: dict[str, Any]) -> str:
        """`_id` is `"<accession>:<filename>"`; the archive path needs the CIK.

        The CIK is stored zero-padded (`"0000829323"`) but the archive path uses
        it unpadded, and the accession number loses its dashes.
        """
        if not isinstance(doc_id, str) or ":" not in doc_id:
            return ""
        accession, _, document = doc_id.partition(":")
        if not accession or not document:
            return ""
        ciks = src.get("ciks")
        cik = ciks[0] if isinstance(ciks, list) and ciks else None
        if not isinstance(cik, str) or not cik.strip("0").isdigit():
            return ""
        return _ARCHIVE.format(
            cik=int(cik),
            accession=accession.replace("-", ""),
            document=document,
        )

    @staticmethod
    def _title(src: dict[str, Any]) -> str:
        """`"<FORM> — <Company (TICKER)>"`, which is how a filing is referred to.

        `display_names` entries look like
        `"Inuvo, Inc.  (INUV)  (CIK 0000829323)"`; the CIK is already in the URL,
        so it is dropped from the title.
        """
        names = src.get("display_names")
        company = ""
        if isinstance(names, list) and names and isinstance(names[0], str):
            company = names[0].split("(CIK")[0].strip()
            company = " ".join(company.split())
            # Multi-listed issuers carry every ticker and warrant class in the
            # display name (JPMorgan lists eight); one is enough to identify
            # the company, and the rest is noise in a result list.
            head, sep, tickers = company.partition("(")
            if sep and "," in tickers:
                company = f"{head.strip()} ({tickers.split(',', 1)[0].strip()})"
        form = src.get("form")
        form = form if isinstance(form, str) else ""
        if form and company:
            return f"{form} — {company}"
        return form or company

    def _snippet(self, src: dict[str, Any]) -> str:
        bits: list[str] = []
        date = src.get("file_date")
        if isinstance(date, str) and date:
            bits.append(f"filed {date}")
        period = src.get("period_ending")
        if isinstance(period, str) and period:
            bits.append(f"period ending {period}")
        desc = src.get("file_description")
        if isinstance(desc, str) and desc:
            bits.append(desc)
        locations = src.get("biz_locations")
        if isinstance(locations, list) and locations and isinstance(locations[0], str):
            bits.append(locations[0])
        return clip(" · ".join(bits))
