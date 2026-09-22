"""Wikidata: the structured record behind a Wikipedia article.

  GET https://www.wikidata.org/w/api.php?action=wbsearchentities&search=<q>
  GET https://www.wikidata.org/w/api.php?action=wbgetentities&ids=<Q...>

An entity search finds the items a query names; a second call reads each
item's description, a few dated facts (inception, population, website,
coordinates) and its Wikipedia article. The result links to the article
when there is one, because that is the page worth reading; the snippet
carries the facts as data, each with the date Wikidata attaches to it.
"""

from __future__ import annotations

import re
from typing import Any
from urllib.parse import quote, quote_plus

from .base import SearchFilters, SearchResult
from .facts import compact_number, query_is_chinese
from .jsonapi import JsonApiEngine, clip

_API = "https://www.wikidata.org/w/api.php"

# Property -> label. Kept to facts whose value is a number, a date, a string
# or coordinates; a property whose value is another item would need a third
# call to become readable.
_FACTS = {
    "P571": "inception",
    "P1082": "population",
    "P2046": "area",
    "P2044": "elevation",
    "P856": "official website",
    "P625": "coordinates",
    "P569": "born",
    "P570": "died",
    "P577": "published",
    "P1128": "employees",
    "P2139": "revenue",
    "P2295": "net profit",
    "P1619": "opened",
    "P2048": "height",
}


# Words that ask for a fact about a thing rather than name the thing. They are
# removed before the entity search, so "Shanghai population" finds Shanghai and
# not the Shanghai population commission.
_FACT_WORDS = re.compile(
    r"\b(population|area|capital|founded|founder|inception|established|height|elevation|"
    r"website|coordinates|location|born|birthday|died|death|age|ceo|revenue|employees|"
    r"headquarters|currency|language|languages|mayor|president|gdp|timezone|what|is|the|of|"
    r"how|many|much|big|old|tall|when|where|who|was|are|does|do|in|a|an|and|for|about|"
    r"wikidata|wikipedia)\b|人口|面积|首都|成立|建立|创立|创始人|海拔|官网|官方网站|坐标|出生|"
    r"生日|逝世|去世|年龄|营收|收入|员工|总部|货币|语言|市长|总统|时区|是什么|是谁|多少|多大|"
    r"多高|什么时候|哪里|哪一年|几年|的|是|有|在|吗|呢|维基数据|维基百科",
    re.I,
)


# The fact words alone, without the function words: these make a question a
# lookup ("Shanghai population", "when was X founded"), the others do not.
_FACT_ASK = re.compile(
    r"\b(population|area|capital|founded|founder|inception|established|height|elevation|"
    r"official website|coordinates|born|birthday|died|ceo|revenue|employees|headquarters|"
    r"currency|mayor|president|gdp|timezone)\b|人口|面积|首都|成立|建立|创立|创始人|海拔|官网|"
    r"官方网站|坐标|出生|生日|逝世|去世|营收|员工|总部|货币|市长|总统|时区",
    re.I,
)


def entity_terms(query: str) -> str:
    """The part of a query that names an entity."""
    text = _FACT_WORDS.sub(" ", query)
    text = re.sub(r"[?？,，。!！]", " ", text)
    return " ".join(text.split())


_UNITS = {
    "Q712226": "km²",
    "Q25343": "m²",
    "Q11573": "m",
    "Q828224": "km",
    "Q174728": "cm",
    "Q11570": "kg",
    "Q4917": "USD",
    "Q4916": "EUR",
    "Q39099": "CNY",
    "Q25224": "GBP",
    "Q8146": "JPY",
    "Q577": "years",
    "Q25235": "h",
    "Q7727": "min",
}


class WikidataEngine(JsonApiEngine):
    """Wikidata entity search with dated structured facts and the Wikipedia link."""

    name = "wikidata"
    single_site = True
    categories = frozenset({"reference"})
    impersonate = None
    direct_answer = True
    description = "Wikidata: structured facts (dates, populations, sites) behind Wikipedia."

    def claims(self, query: str) -> bool:
        return bool(_FACT_ASK.search(query)) and bool(entity_terms(query))

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        lang = "zh" if query_is_chinese(query) else "en"
        n = max(1, min(max_results, 7))
        return (
            f"{_API}?action=wbsearchentities&search={quote_plus(entity_terms(query) or query)}"
            f"&language={lang}"
            f"&uselang={lang}&format=json&limit={n}"
        )

    async def fetch_results(
        self, query: str, max_results: int, filters: SearchFilters | None
    ) -> list[SearchResult]:
        found = await self._get_json(self.build_url(query, max_results, filters))
        hits = found.get("search") if isinstance(found, dict) else None
        ids = [h["id"] for h in hits or [] if isinstance(h, dict) and isinstance(h.get("id"), str)]
        if not ids:
            return []
        lang = "zh" if query_is_chinese(query) else "en"
        detail = await self._get_json(
            f"{_API}?action=wbgetentities&ids={'|'.join(ids)}&props=labels|descriptions|"
            f"sitelinks/urls|claims&languages={lang}|en&sitefilter={lang}wiki|enwiki&format=json"
        )
        entities = detail.get("entities") if isinstance(detail, dict) else None
        if not isinstance(entities, dict):
            return []
        results: list[SearchResult] = []
        for qid in ids:
            results.extend(self._map_entity(qid, entities.get(qid), lang))
        return results

    def map_results(self, payload: Any) -> list[SearchResult]:
        entities = payload.get("entities") if isinstance(payload, dict) else None
        if not isinstance(entities, dict):
            return []
        results: list[SearchResult] = []
        for qid, entity in entities.items():
            results.extend(self._map_entity(qid, entity, "en"))
        return results

    def _map_entity(self, qid: str, entity: Any, lang: str) -> list[SearchResult]:
        if not isinstance(entity, dict) or "missing" in entity:
            return []
        label = _text(entity.get("labels"), lang) or _text(entity.get("labels"), "en")
        if not label:
            return []
        description = _text(entity.get("descriptions"), lang) or _text(
            entity.get("descriptions"), "en"
        )
        sitelinks = entity.get("sitelinks") if isinstance(entity.get("sitelinks"), dict) else {}
        link = sitelinks.get(f"{lang}wiki") or sitelinks.get("enwiki") or {}
        url = link.get("url") if isinstance(link, dict) else None
        if not isinstance(url, str):
            url = f"https://www.wikidata.org/wiki/{quote(qid)}"
        bits = [description] if description else []
        bits.extend(self._facts(entity.get("claims")))
        bits.append(f"Wikidata {qid}")
        return [
            SearchResult(
                title=label,
                url=url,
                snippet=clip(" · ".join(bits), cap=500),
                engine=self.name,
                rank=0,
            )
        ]

    @staticmethod
    def _facts(claims: Any) -> list[str]:
        if not isinstance(claims, dict):
            return []
        facts: list[str] = []
        for prop, name in _FACTS.items():
            statements = claims.get(prop)
            if not isinstance(statements, list) or not statements:
                continue
            # The most recent statement by its "point in time" qualifier, else
            # the preferred or first one.
            chosen = max(statements, key=_point_in_time)
            value = _value(chosen)
            if not value:
                continue
            when = _point_in_time(chosen)
            facts.append(f"{name} {value}" + (f" (as of {when})" if when else ""))
        return facts


def _text(block: Any, lang: str) -> str:
    entry = block.get(lang) if isinstance(block, dict) else None
    value = entry.get("value") if isinstance(entry, dict) else None
    return value if isinstance(value, str) else ""


def _time(value: Any) -> str:
    # Wikidata times look like "+2020-11-01T00:00:00Z" with a precision; day
    # precision is 11, month 10, year 9. Years are zero-padded to four digits
    # and a BCE year carries a minus sign.
    if not isinstance(value, dict) or not isinstance(value.get("time"), str):
        return ""
    raw = value["time"]
    bce = raw.startswith("-")
    stamp = raw.lstrip("+-")[:10]
    year = stamp[:4].lstrip("0") or "0"
    if bce:
        year += " BCE"
    precision = value.get("precision", 11)
    if precision <= 9 or bce:
        return year
    if precision == 10:
        return f"{year}-{stamp[5:7]}"
    return f"{year}-{stamp[5:]}"


def _point_in_time(statement: Any) -> str:
    qualifiers = statement.get("qualifiers") if isinstance(statement, dict) else None
    entries = qualifiers.get("P585") if isinstance(qualifiers, dict) else None
    if isinstance(entries, list) and entries and isinstance(entries[0], dict):
        return _time((entries[0].get("datavalue") or {}).get("value"))
    return ""


def _value(statement: Any) -> str:
    snak = statement.get("mainsnak") if isinstance(statement, dict) else None
    datavalue = snak.get("datavalue") if isinstance(snak, dict) else None
    if not isinstance(datavalue, dict):
        return ""
    kind, value = datavalue.get("type"), datavalue.get("value")
    if kind == "string":
        return value if isinstance(value, str) else ""
    if kind == "time":
        return _time(value)
    if kind == "quantity" and isinstance(value, dict):
        amount = compact_number(str(value.get("amount", "")).lstrip("+"))
        unit = value.get("unit")
        if isinstance(unit, str) and unit != "1":
            # A unit is an item URL. The common ones are named here; any other
            # keeps its id, which is more honest than nothing.
            qid = unit.rsplit("/", 1)[-1]
            return f"{amount} {_UNITS.get(qid, '(' + qid + ')')}"
        return amount
    if kind == "globecoordinate" and isinstance(value, dict):
        try:
            return f"{float(value['latitude']):.4f}, {float(value['longitude']):.4f}"
        except (KeyError, TypeError, ValueError):
            return ""
    return ""
