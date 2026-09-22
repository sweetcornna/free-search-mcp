"""Frankfurter: the ECB reference exchange rate between two currencies.

  GET https://api.frankfurter.dev/v1/latest?base=USD&symbols=CNY
  GET https://open.er-api.com/v6/latest/TWD          (currencies the ECB lacks)

The European Central Bank publishes one reference rate per currency each
working day around 16:00 CET; Frankfurter serves that table. The result
carries the ECB's date, so an agent can see it is a daily reference and not
a live market quote. A query needs two currencies, as ISO codes or common
names in English or Chinese; an amount in the query is converted. For a
currency the ECB does not publish (TWD, VND, RUB, AED and about 130 more)
the open ExchangeRate-API endpoint answers instead, once a day, and the
result says which source it is; its terms ask for an attribution link,
which the snippet carries.
"""

from __future__ import annotations

import re
from typing import Any

from .base import SearchFilters, SearchResult
from .facts import iso_day
from .jsonapi import JsonApiEngine

_API = "https://api.frankfurter.dev/v1/latest"

# The currencies the ECB publishes. Frankfurter answers 404 for any other code.
CURRENCIES = frozenset(
    {
        "AUD", "BGN", "BRL", "CAD", "CHF", "CNY", "CZK", "DKK", "EUR", "GBP",
        "HKD", "HUF", "IDR", "ILS", "INR", "ISK", "JPY", "KRW", "MXN", "MYR",
        "NOK", "NZD", "PHP", "PLN", "RON", "SEK", "SGD", "THB", "TRY", "USD",
        "ZAR",
    }
)  # fmt: skip

# Codes the open ExchangeRate-API endpoint serves beyond the ECB set. Any
# three-letter word in a question is checked against both sets.
MORE_CURRENCIES = frozenset(
    {
        "TWD", "VND", "RUB", "AED", "SAR", "MOP", "EGP", "PKR", "BDT", "LKR", "NPR", "KWD",
        "QAR", "OMR", "BHD", "JOD", "IQD", "IRR", "KZT", "UZS", "UAH", "BYN", "GEL", "AMD",
        "AZN", "MNT", "KHR", "LAK", "MMK", "ARS", "CLP", "COP", "PEN", "UYU", "BOB", "VES",
        "DOP", "GTQ", "CRC", "JMD", "TTD", "NGN", "KES", "GHS", "TZS", "UGX", "ETB", "MAD",
        "DZD", "TND", "XOF", "XAF", "RSD", "MKD", "BAM", "ALL", "MDL", "LBP", "SYP", "AFN",
        "BTN", "MVR", "FJD", "PGK", "XPF", "BND", "MUR", "SCR", "ZMW", "MZN", "AOA", "BWP",
        "NAD", "CUP", "HTG", "HNL", "NIO", "PAB", "PYG", "SVC", "BZD", "BBD", "BSD", "GYD",
        "SRD", "KGS", "TJS", "TMT", "LYD", "SDG", "SOS", "RWF", "BIF", "MGA", "MWK", "GNF",
        "SLE", "LRD", "GMD", "CVE", "STN", "SZL", "LSL", "ERN", "DJF", "KMF", "MRU", "CDF",
        "XCD", "AWG", "ANG", "KYD", "BMD", "GIP", "FKP", "SHP", "JEP", "GGP", "IMP", "TVD",
        "WST", "TOP", "VUV", "SBD", "KID", "FOK", "ZWL",
    }
)  # fmt: skip

_NAMES = {
    "dollar": "USD", "dollars": "USD", "usd": "USD", "美元": "USD", "美金": "USD",
    "euro": "EUR", "euros": "EUR", "欧元": "EUR",
    "yuan": "CNY", "rmb": "CNY", "renminbi": "CNY", "人民币": "CNY", "元": "CNY",
    "yen": "JPY", "日元": "JPY", "日币": "JPY",
    "pound": "GBP", "pounds": "GBP", "sterling": "GBP", "英镑": "GBP",
    "hong kong dollar": "HKD", "港币": "HKD", "港元": "HKD",
    "franc": "CHF", "francs": "CHF", "瑞郎": "CHF", "瑞士法郎": "CHF",
    "won": "KRW", "韩元": "KRW",
    "rupee": "INR", "rupees": "INR", "卢比": "INR",
    "singapore dollar": "SGD", "新加坡元": "SGD", "新币": "SGD",
    "australian dollar": "AUD", "澳元": "AUD",
    "canadian dollar": "CAD", "加元": "CAD",
    "baht": "THB", "泰铢": "THB",
    "ringgit": "MYR", "林吉特": "MYR",
    "rupiah": "IDR", "印尼盾": "IDR",
    "lira": "TRY", "里拉": "TRY",
    "krona": "SEK", "kronor": "SEK", "瑞典克朗": "SEK",
    "krone": "NOK", "kroner": "NOK", "挪威克朗": "NOK",
    "zloty": "PLN", "兹罗提": "PLN",
    "peso": "MXN", "比索": "MXN",
    "real": "BRL", "reais": "BRL", "雷亚尔": "BRL",
    "rand": "ZAR", "兰特": "ZAR",
    "new zealand dollar": "NZD", "新西兰元": "NZD",
    "台币": "TWD", "新台币": "TWD", "taiwan dollar": "TWD",
    "越南盾": "VND", "dong": "VND",
    "卢布": "RUB", "rouble": "RUB", "ruble": "RUB",
    "迪拉姆": "AED", "dirham": "AED",
    "里亚尔": "SAR", "riyal": "SAR",
    "澳门元": "MOP", "pataca": "MOP",
    "埃及镑": "EGP",
    "格里夫纳": "UAH", "hryvnia": "UAH",
    "菲律宾比索": "PHP",
    "阿根廷比索": "ARS",
    "奈拉": "NGN", "naira": "NGN",
}  # fmt: skip

_AMOUNT_RE = re.compile(r"(?<![\w.])(\d{1,12}(?:[.,]\d{1,4})?)(?:\s*(?:k|万|亿|thousand|million))?")
_CODE_RE = re.compile(r"\b([A-Za-z]{3})\b")


def parse_pair(query: str) -> tuple[str, str, float] | None:
    """(base, quote, amount) from a query, or None when it names fewer than two currencies."""
    # Currencies in the order the question names them: the first is the base
    # ("1万日元等于多少人民币" converts yen into yuan, not the reverse).
    hits: list[tuple[int, str]] = []
    text = query
    for m in _CODE_RE.finditer(query):
        code = m.group(1).upper()
        if code in CURRENCIES or code in MORE_CURRENCIES:
            hits.append((m.start(), code))
    lowered = query.lower()
    # Longer names first so "hong kong dollar" wins over "dollar".
    for name in sorted(_NAMES, key=len, reverse=True):
        at = lowered.find(name)
        if at >= 0:
            if _NAMES[name]:
                hits.append((at, _NAMES[name]))
            lowered = lowered.replace(name, " " * len(name))
    found: list[str] = []
    for _at, code in sorted(hits):
        if code not in found:
            found.append(code)
    if len(found) < 2:
        return None
    amount = 1.0
    m = _AMOUNT_RE.search(text)
    if m:
        try:
            amount = float(m.group(1).replace(",", ""))
        except ValueError:
            amount = 1.0
        tail = text[m.end(1) : m.end(1) + 8].strip().lower()
        if tail.startswith(("k", "thousand")):
            amount *= 1_000
        elif tail.startswith("万"):
            amount *= 10_000
        elif tail.startswith("million"):
            amount *= 1_000_000
        elif tail.startswith("亿"):
            amount *= 100_000_000
    return found[0], found[1], amount


_OPEN_API = "https://open.er-api.com/v6/latest"


def _from_open_api(raw: Any, symbol: str) -> dict[str, Any] | None:
    """Reshape an open.er-api.com answer into Frankfurter's `{base, date, rates}`."""
    if not isinstance(raw, dict) or raw.get("result") != "success":
        return None
    rates = raw.get("rates") if isinstance(raw.get("rates"), dict) else {}
    if symbol not in rates:
        return None
    stamp = raw.get("time_last_update_unix")
    day = iso_day(stamp) if isinstance(stamp, (int, float)) else ""
    return {
        "base": raw.get("base_code"),
        "date": day or str(raw.get("time_last_update_utc") or "")[:16],
        "rates": {symbol: rates[symbol]},
        "_source": "open-er-api",
    }


class FrankfurterEngine(JsonApiEngine):
    """ECB reference rate between the two currencies a query names."""

    name = "frankfurter"
    single_site = True
    categories = frozenset({"finance", "finance.fx"})
    impersonate = None
    direct_answer = True
    description = "ECB daily reference rate between two currencies; others via ExchangeRate-API."

    def claims(self, query: str) -> bool:
        return parse_pair(query) is not None

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        pair = parse_pair(query)
        if pair is None:
            return f"{_API}?base=USD"
        base, symbol, _amount = pair
        return f"{_API}?base={base}&symbols={symbol}"

    async def fetch_results(
        self, query: str, max_results: int, filters: SearchFilters | None
    ) -> list[SearchResult]:
        pair = parse_pair(query)
        if pair is None:
            return []
        base, symbol, amount = pair
        if base in CURRENCIES and symbol in CURRENCIES:
            url = self.build_url(query, max_results, filters)
            payload = await self._get_json(url)
        else:
            url = f"{_OPEN_API}/{base}"
            raw = await self._get_json(url)
            payload = _from_open_api(raw, symbol)
        if not isinstance(payload, dict):
            return []
        payload["_amount"] = amount
        payload["_url"] = url
        return self.map_results(payload)

    def map_results(self, payload: Any) -> list[SearchResult]:
        if not isinstance(payload, dict):
            return []
        base, rates, day = payload.get("base"), payload.get("rates"), payload.get("date")
        if not isinstance(base, str) or not isinstance(rates, dict) or not rates:
            return []
        amount = float(payload.get("_amount") or 1.0)
        results: list[SearchResult] = []
        for symbol, rate in rates.items():
            try:
                rate = float(rate)
            except (TypeError, ValueError):
                continue
            converted = f"{amount:g} {base} = {amount * rate:,.4f} {symbol}" if amount != 1 else ""
            if payload.get("_source") == "open-er-api":
                source = "ExchangeRate-API"
                snippet = (
                    f"1 {base} = {rate:g} {symbol} · daily rate for {day} from the open "
                    "ExchangeRate-API endpoint, updated once a day; not a live market quote · "
                    "Rates By Exchange Rate API https://www.exchangerate-api.com"
                )
            else:
                source = "ECB"
                snippet = (
                    f"1 {base} = {rate:g} {symbol} · ECB reference rate for {day}, published "
                    "once per working day around 16:00 CET; not a live market quote"
                )
            if converted:
                snippet = f"{converted} · {snippet}"
            results.append(
                SearchResult(
                    title=f"{base} to {symbol}: {rate:g} ({source}, {day})",
                    url=str(payload.get("_url") or f"{_API}?base={base}&symbols={symbol}"),
                    snippet=snippet,
                    engine=self.name,
                    rank=len(results),
                    published_age=str(day) if isinstance(day, str) else None,
                    published_age_confident=isinstance(day, str),
                )
            )
        return results
