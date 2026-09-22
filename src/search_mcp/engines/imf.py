"""IMF DataMapper — macroeconomic indicator series by country. Keyless.

  GET https://www.imf.org/external/datamapper/api/v1/indicators   (catalogue)
  GET https://www.imf.org/external/datamapper/api/v1/countries    (catalogue)
  GET https://www.imf.org/external/datamapper/api/v1/<INDICATOR>  (series)

The complement to `worldbank`: that one searches documents, this one answers
"what IS Vietnam's projected GDP growth" with the numbers, including WEO
forecasts several years out.

Three measured facts drive the implementation:

  * There is NO text search. The catalogue is 132 indicators in ~48 KB, so the
    match is done here with `rapidfuzz` (already a dependency) against each
    indicator's label and description.
  * The country path segment is IGNORED. `/NGDP_RPCH/VNM` returns all 229
    countries, identical byte-for-byte to `/NGDP_RPCH`, so the country filter
    is applied client-side.
  * `periods=` is ignored too, so the year window is trimmed here as well.

Both catalogues are fetched at most once per process — they change on the WEO
release cycle, not per query.
"""

from __future__ import annotations

import re
from typing import Any
from urllib.parse import quote

from rapidfuzz import fuzz, utils

from .base import SearchFilters, SearchResult
from .jsonapi import JsonApiEngine, clip

_API = "https://www.imf.org/external/datamapper/api/v1"
_PROFILE = "https://www.imf.org/external/datamapper/{indicator}@{dataset}/{country}"

# How many recent observations to show. Enough to see a trend and the WEO
# forecast tail without turning a snippet into a table.
_YEARS = 8
# Below this fuzzy score the "match" is noise — an unrelated indicator is worse
# than no result, because a number always looks authoritative.
_MIN_SCORE = 60.0
_MAX_INDICATORS = 3
_MAX_COUNTRIES = 3


class ImfEngine(JsonApiEngine):
    """IMF DataMapper macro indicator lookup (keyless JSON API)."""

    name = "imf"
    description = "IMF DataMapper: GDP, inflation, debt and jobs series, with WEO forecasts."
    categories = frozenset({"finance", "finance.macro"})

    def __init__(self) -> None:
        self._indicators: dict[str, dict[str, Any]] | None = None
        self._countries: dict[str, str] | None = None

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        # No single URL expresses this query — the catalogue drives which series
        # get fetched. Returned for the abstract-method contract and so the
        # engine still has a stable identity in logs.
        return f"{_API}/indicators"

    # -- catalogues --------------------------------------------------------

    async def _load_indicators(self) -> dict[str, dict[str, Any]]:
        if self._indicators is None:
            payload = await self._get_json(f"{_API}/indicators")
            block = payload.get("indicators") if isinstance(payload, dict) else None
            self._indicators = block if isinstance(block, dict) else {}
        return self._indicators

    async def _load_countries(self) -> dict[str, str]:
        if self._countries is None:
            payload = await self._get_json(f"{_API}/countries")
            block = payload.get("countries") if isinstance(payload, dict) else None
            out: dict[str, str] = {}
            if isinstance(block, dict):
                for code, entry in block.items():
                    label = entry.get("label") if isinstance(entry, dict) else None
                    if isinstance(code, str) and isinstance(label, str) and label:
                        out[code] = label
            self._countries = out
        return self._countries

    # -- matching ----------------------------------------------------------

    @classmethod
    def _match_indicators(cls, query: str, catalogue: dict[str, dict[str, Any]]) -> list[str]:
        """Best indicator codes for `query`, strongest first.

        Scores the label heavily and the description lightly: a description
        mentioning "gross domestic product" should not outrank the indicator
        actually CALLED "Real GDP growth".

        `query` must arrive with country words already removed — see
        `_strip_countries`. Leaving them in costs real accuracy: "vietnam real
        gdp growth" scored 57.9 against "Real GDP growth" (the country token
        counts against the ratio) and lost to an unrelated capital-flows
        series; "real gdp growth" scores 100.
        """
        scored: list[tuple[float, float, float, str]] = []
        # Case-SENSITIVE, deliberately, and not the same token set the fuzzy
        # scorer sees. Three of the 132 codes are also ordinary words — `GDP`
        # (Capital Flows' *nominal* GDP), `LP` (Population), `LUR`
        # (Unemployment rate) — so a case-folded shortcut fired on every query
        # containing "gdp" and pinned nominal GDP at 100, beating the 73 that
        # "real gdp growth" scored against the indicator literally called "Real
        # GDP growth". Someone naming a code types it as a code: `NGDP_RPCH`.
        typed = {t.strip(".,") for t in query.split() if t.isupper()}
        for code, entry in catalogue.items():
            if not isinstance(entry, dict):
                continue
            label = str(entry.get("label") or "").strip()
            description = str(entry.get("description") or "")
            score = cls._ratio(query, label)
            if description:
                score = max(score, cls._ratio(query, description) * 0.6)
            # An exact code mention ("NGDP_RPCH") is a certainty, not a guess,
            # so it sorts ahead of everything rather than competing on score.
            exact = 1.0 if code in typed else 0.0
            if score < _MIN_SCORE and not exact:
                continue
            # `token_set_ratio` scores on the INTERSECTION, so every label that
            # merely contains the query's words ties at 100: "real gdp growth"
            # matched "Real Per Capita GDP Growth" and "Real Non-Oil GDP
            # Growth" just as perfectly as "Real GDP growth", and alphabetical
            # order then picked one of the wrong two. `token_sort_ratio`
            # compares the whole strings, so extra words cost — it is the tie
            # break that puts the exactly-named indicator first.
            tight = cls._ratio(query, label, sort=True)
            scored.append((exact, score, tight, code))
        scored.sort(key=lambda row: (-row[0], -row[1], -row[2], row[3]))
        return [code for _exact, _score, _tight, code in scored[:_MAX_INDICATORS]]

    @staticmethod
    def _ratio(query: str, text: str, *, sort: bool = False) -> float:
        """Case- and punctuation-insensitive fuzzy ratio.

        rapidfuzz does no normalization by default, so "real gdp growth" scored
        73 against "Real GDP growth" purely on capitalization — close enough to
        the 60 cutoff that a typed-lowercase query could miss its own exact
        indicator.

        `sort=True` switches to `token_sort_ratio`, which — unlike the default
        `token_set_ratio` — charges for words the label has and the query
        doesn't. See the tie-break in `_match_indicators`.
        """
        scorer = fuzz.token_sort_ratio if sort else fuzz.token_set_ratio
        return float(scorer(query, text, processor=utils.default_process))

    @staticmethod
    def _aliases(label: str) -> list[str]:
        """Names a person would actually type for this country.

        The catalogue's official forms are unusable as-is: a query says
        "china", the catalogue says "China, People's Republic of", so a
        whole-label test never fires. The short form before the first comma is
        the one people use, and it is kept alongside the full label.
        """
        full = label.strip().lower()
        short = full.split(",", 1)[0].strip()
        return [a for a in dict.fromkeys((full, short)) if len(a) >= 4]

    @classmethod
    def _match_countries(cls, query: str, catalogue: dict[str, str]) -> list[str]:
        """ISO3 codes named in the query, by country name or by the code itself.

        Word-boundary matching, not bare substring: "chad" must not be found
        inside "chadwick", and "oman" must not be found inside "romania".
        """
        low = query.lower()
        tokens = {t.strip(".,").upper() for t in query.split()}
        hits: list[tuple[int, str]] = []
        for code, label in catalogue.items():
            if code in tokens:
                hits.append((99, code))
                continue
            for alias in cls._aliases(label):
                if re.search(rf"\b{re.escape(alias)}\b", low):
                    hits.append((len(alias), code))
                    break
        # Longest match wins: "united states" must beat a bare "states", and a
        # specific country must beat an aggregate that merely contains its name.
        hits.sort(key=lambda pair: -pair[0])
        return [code for _length, code in hits[:_MAX_COUNTRIES]]

    @classmethod
    def _strip_countries(cls, query: str, codes: list[str], catalogue: dict[str, str]) -> str:
        """Remove the matched country words so only the metric is scored."""
        out = query
        for code in codes:
            for alias in cls._aliases(catalogue.get(code, "")):
                out = re.sub(rf"\b{re.escape(alias)}\b", " ", out, flags=re.I)
            out = re.sub(rf"\b{re.escape(code)}\b", " ", out, flags=re.I)
        return " ".join(out.split()) or query

    # -- results -----------------------------------------------------------

    async def fetch_results(
        self, query: str, max_results: int, filters: SearchFilters | None
    ) -> list[SearchResult]:
        catalogue = await self._load_indicators()
        if not catalogue:
            return []
        country_catalogue = await self._load_countries()
        countries = self._match_countries(query, country_catalogue)
        metric = self._strip_countries(query, countries, country_catalogue)
        codes = self._match_indicators(metric, catalogue)
        if not codes:
            return []

        results: list[SearchResult] = []
        for code in codes:
            entry = catalogue.get(code) or {}
            if not countries:
                # No country named: describe the indicator rather than picking
                # one arbitrarily. Knowing the series exists and where it lives
                # is the useful answer to "what does the IMF publish on X".
                results.append(self._indicator_result(code, entry, None, {}))
                continue
            series = await self._series(code)
            for iso3 in countries:
                values = series.get(iso3)
                if isinstance(values, dict) and values:
                    results.append(self._indicator_result(code, entry, iso3, values))
        return results

    async def _series(self, code: str) -> dict[str, Any]:
        payload = await self._get_json(f"{_API}/{quote(code, safe='')}")
        values = payload.get("values") if isinstance(payload, dict) else None
        block = values.get(code) if isinstance(values, dict) else None
        return block if isinstance(block, dict) else {}

    def _indicator_result(
        self,
        code: str,
        entry: dict[str, Any],
        iso3: str | None,
        values: dict[str, Any],
    ) -> SearchResult:
        label = " ".join(str(entry.get("label") or code).split())
        unit = str(entry.get("unit") or "").strip()
        dataset = str(entry.get("dataset") or "WEO").strip() or "WEO"
        country_name = (self._countries or {}).get(iso3 or "", "")

        title = f"{label} — {country_name}" if country_name else label
        if unit:
            title = f"{title} ({unit})"

        bits: list[str] = []
        recent = self._recent(values)
        if recent:
            bits.append(" · ".join(f"{year}: {value}" for year, value in recent))
        source = entry.get("source")
        if isinstance(source, str) and source:
            bits.append(source)
        description = entry.get("description")
        if isinstance(description, str) and description:
            bits.append(description)

        return SearchResult(
            title=clip(title, cap=300),
            url=_PROFILE.format(
                indicator=quote(code, safe=""),
                dataset=quote(dataset, safe=""),
                country=quote(iso3 or "", safe=""),
            ).rstrip("/"),
            snippet=clip(" — ".join(bits)),
            engine=self.name,
            rank=0,
            # A WEO vintage is a dataset version, not a publication date for
            # this row, and the series itself runs into forecast years. Nothing
            # here is a date the freshness filter should drop on.
        )

    @staticmethod
    def _recent(values: dict[str, Any]) -> list[tuple[str, Any]]:
        """The last `_YEARS` observations, oldest first, forecasts included."""
        years = sorted(y for y in values if isinstance(y, str) and y.isdigit())
        return [(y, values[y]) for y in years[-_YEARS:]]
