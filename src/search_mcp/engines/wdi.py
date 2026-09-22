"""World Bank indicators: one number for one country, with its year.

  GET https://api.worldbank.org/v2/country/CHN/indicator/NY.GDP.MKTP.CD?format=json&mrv=3

The World Development Indicators are the reference series for GDP,
population, inflation, unemployment and the rest, revised on a published
date that the response carries. The indicator and the country are read from
the question; both must be recognised, or nothing is asked. The existing
`worldbank` engine searches the Bank's documents; this one reads its data.
"""

from __future__ import annotations

import asyncio
import re
from typing import Any

from .base import SearchFilters, SearchResult
from .facts import ZH_COUNTRY, compact_number, countries_in, iso_day, query_is_chinese
from .jsonapi import JsonApiEngine, clip

_API = "https://api.worldbank.org/v2/country/{iso3}/indicator/{code}?format=json&mrv=3"

# code -> (label, unit hint, pattern). Order matters: the first match wins
# when a question fits several, so the specific ones come first.
_INDICATORS: list[tuple[str, str, str, re.Pattern[str]]] = [
    ("NY.GDP.PCAP.CD", "GDP per capita", "current US$",
     re.compile(r"gdp per capita|per capita gdp|人均 ?gdp|人均国内生产总值|人均产值", re.I)),
    ("NY.GDP.MKTP.KD.ZG", "GDP growth", "annual %",
     re.compile(r"gdp growth|growth rate|经济增速|经济增长率|gdp ?增速|gdp ?增长", re.I)),
    ("NY.GDP.MKTP.CD", "GDP", "current US$",
     re.compile(r"\bgdp\b|gross domestic product|国内生产总值|经济总量", re.I)),
    ("SP.POP.TOTL", "Population", "people",
     re.compile(r"\bpopulation\b|人口(?!普查)", re.I)),
    ("FP.CPI.TOTL.ZG", "Inflation, consumer prices", "annual %",
     re.compile(r"\binflation\b|\bcpi\b|通胀|通货膨胀|物价", re.I)),
    ("SL.UEM.TOTL.ZS", "Unemployment", "% of labour force",
     re.compile(r"unemployment|失业率|失业", re.I)),
    ("SP.DYN.LE00.IN", "Life expectancy at birth", "years",
     re.compile(r"life expectancy|预期寿命|人均寿命", re.I)),
    ("SP.DYN.TFRT.IN", "Fertility rate", "births per woman",
     re.compile(r"fertility|生育率|总和生育率", re.I)),
    ("SP.DYN.CBRT.IN", "Birth rate", "per 1,000 people",
     re.compile(r"birth rate|出生率", re.I)),
    ("NE.EXP.GNFS.CD", "Exports of goods and services", "current US$",
     re.compile(r"\bexports?\b|出口(额|总额)?", re.I)),
    ("NE.IMP.GNFS.CD", "Imports of goods and services", "current US$",
     re.compile(r"\bimports?\b|进口(额|总额)?", re.I)),
    ("GC.DOD.TOTL.GD.ZS", "Central government debt", "% of GDP",
     re.compile(r"government debt|public debt|debt to gdp|政府债务|国债.*占比|负债率", re.I)),
    ("FI.RES.TOTL.CD", "Total reserves", "current US$",
     re.compile(r"foreign (exchange )?reserves|外汇储备", re.I)),
    ("BX.KLT.DINV.CD.WD", "Foreign direct investment, net inflows", "current US$",
     re.compile(r"\bfdi\b|foreign direct investment|外商直接投资|外资流入", re.I)),
    ("IT.NET.USER.ZS", "Internet users", "% of population",
     re.compile(r"internet users|internet penetration|互联网普及率|网民", re.I)),
    ("SP.URB.TOTL.IN.ZS", "Urban population", "% of total",
     re.compile(r"urbani[sz]ation|urban population|城镇化率|城市化率", re.I)),
    ("AG.LND.TOTL.K2", "Land area", "sq. km",
     re.compile(r"land area|国土面积|陆地面积", re.I)),
    ("MS.MIL.XPND.GD.ZS", "Military expenditure", "% of GDP",
     re.compile(r"military (spending|expenditure)|defen[cs]e (spending|budget)|军费|国防开支",
                re.I)),
    ("SE.ADT.LITR.ZS", "Literacy rate, adult", "% of people 15+",
     re.compile(r"literacy|识字率", re.I)),
    ("SH.XPD.CHEX.GD.ZS", "Current health expenditure", "% of GDP",
     re.compile(r"health (spending|expenditure)|医疗支出|卫生支出", re.I)),
    ("EG.USE.ELEC.KH.PC", "Electric power consumption", "kWh per capita",
     re.compile(r"electricity consumption|power consumption|用电量", re.I)),
    ("EN.GHG.CO2.PC.CE.AR5", "CO2 emissions per capita", "t CO2e",
     re.compile(r"co2|carbon emissions|碳排放", re.I)),
]  # fmt: skip


def indicators_in(query: str, *, limit: int = 2) -> list[tuple[str, str, str]]:
    """`(code, label, unit)` for each indicator the question names."""
    found = [(code, label, unit) for code, label, unit, rx in _INDICATORS if rx.search(query)]
    return found[:limit]


class WdiEngine(JsonApiEngine):
    """A World Development Indicator for a named country, latest years."""

    name = "wdi"
    single_site = True
    categories = frozenset({"stats", "stats.indicator"})
    impersonate = None
    direct_answer = True
    description = "World Bank indicators: GDP, population, inflation and more, by country and year."

    def claims(self, query: str) -> bool:
        return bool(indicators_in(query)) and bool(countries_in(query))

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        countries = countries_in(query) or [("WLD", "1W", "World")]
        indicators = indicators_in(query) or [("NY.GDP.MKTP.CD", "GDP", "current US$")]
        return _API.format(iso3=countries[0][0], code=indicators[0][0])

    async def fetch_results(
        self, query: str, max_results: int, filters: SearchFilters | None
    ) -> list[SearchResult]:
        countries = countries_in(query)
        indicators = indicators_in(query)
        if not countries or not indicators:
            return []
        pairs = [(c, i) for c in countries for i in indicators][:4]
        payloads = await asyncio.gather(
            *(self._get_json(_API.format(iso3=c[0], code=i[0])) for c, i in pairs)
        )
        zh = query_is_chinese(query)
        results: list[SearchResult] = []
        for (country, indicator), payload in zip(pairs, payloads, strict=True):
            results.extend(self._map(payload, country, indicator, zh))
        return results

    def map_results(self, payload: Any) -> list[SearchResult]:
        rows = payload[1] if isinstance(payload, list) and len(payload) > 1 else None
        if not isinstance(rows, list) or not rows or not isinstance(rows[0], dict):
            return []
        first = rows[0]
        code = (first.get("indicator") or {}).get("id", "")
        label = (first.get("indicator") or {}).get("value", code)
        iso3 = str(first.get("countryiso3code") or "")
        name = (first.get("country") or {}).get("value", iso3)
        return self._map(payload, (iso3, "", name), (code, label, ""), False)

    def _map(
        self,
        payload: Any,
        country: tuple[str, str, str],
        indicator: tuple[str, str, str],
        zh: bool,
    ) -> list[SearchResult]:
        if not isinstance(payload, list) or len(payload) < 2 or not isinstance(payload[1], list):
            return []
        meta = payload[0] if isinstance(payload[0], dict) else {}
        rows = [r for r in payload[1] if isinstance(r, dict) and r.get("value") is not None]
        if not rows:
            return []
        iso3, _iso2, name = country
        code, label, unit = indicator
        wb_label = (rows[0].get("indicator") or {}).get("value")
        if isinstance(wb_label, str) and wb_label:
            label = wb_label
        latest = rows[0]
        values = "; ".join(f"{r.get('date')}: {_fmt(r.get('value'))}" for r in rows[:3])
        updated = iso_day(meta.get("lastupdated"))
        iso2 = str(latest.get("country", {}).get("id") or _iso2 or "")
        bits = [
            f"{name}, {label}: {values}",
            f"World Bank WDI series {code}" + (f", data revised {updated}" if updated else ""),
        ]
        if zh:
            bits[0] = f"{ZH_COUNTRY.get(iso3, name)}（{label}）：{values}"
        return [
            SearchResult(
                title=f"{name} {label} {latest.get('date')}: {_fmt(latest.get('value'))}",
                url=f"https://data.worldbank.org/indicator/{code}?locations={iso2 or iso3}",
                snippet=clip(" · ".join(bits)),
                engine=self.name,
                rank=0,
                published_age=updated or None,
                published_age_confident=bool(updated),
            )
        ]


def _fmt(value: Any) -> str:
    try:
        number = float(value)
    except (TypeError, ValueError):
        return str(value)
    if abs(number) >= 1e12:
        return f"{number / 1e12:.3g} trillion"
    if abs(number) >= 1e9:
        return f"{number / 1e9:.3g} billion"
    if abs(number) >= 1e6:
        return f"{number / 1e6:.3g} million"
    if number == int(number) and abs(number) >= 1000:
        return compact_number(int(number))
    return f"{number:.4g}"
