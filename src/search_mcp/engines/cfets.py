"""CFETS: the RMB central parity rate published each trading day at 9:15.

  GET https://www.chinamoney.com.cn/r/cms/www/chinamoney/data/fx/ccpr.json

The China Foreign Exchange Trade System publishes the People's Bank of
China's central parity for the yuan against 24 currencies, the official
reference the onshore market trades around. The ECB rate that `frankfurter`
serves is a different fixing on a different day boundary, so a Chinese
question about 人民币 gets both, each labelled. The JPY row is quoted per
100 yen, as CFETS quotes it, and the result says so.
"""

from __future__ import annotations

import re
from datetime import datetime
from typing import Any

from .base import SearchFilters, SearchResult
from .facts import query_is_chinese
from .frankfurter import parse_pair
from .jsonapi import JsonApiEngine, clip

_API = "https://www.chinamoney.com.cn/r/cms/www/chinamoney/data/fx/ccpr.json"
_PAGE = "https://www.chinamoney.com.cn/chinese/bkccpr/"
_PARITY_ASK = re.compile(
    r"中间价|central parity|中国外汇交易中心|cfets|人民币汇率|人民币兑|兑人民币", re.I
)


class CfetsEngine(JsonApiEngine):
    """RMB central parity rates from CFETS, per trading day."""

    name = "cfets"
    single_site = True
    categories = frozenset({"finance", "finance.fx"})
    impersonate = None
    direct_answer = True
    rate_limit_per_minute = 20
    description = "CFETS: the PBOC's daily RMB central parity against 24 currencies."

    def claims(self, query: str) -> bool:
        if _PARITY_ASK.search(query):
            return True
        pair = parse_pair(query)
        return pair is not None and "CNY" in pair[:2] and query_is_chinese(query)

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        return _API

    async def fetch_results(
        self, query: str, max_results: int, filters: SearchFilters | None
    ) -> list[SearchResult]:
        payload = await self._get_json(_API)
        if not isinstance(payload, dict):
            return []
        pair = parse_pair(query)
        wanted = {c for c in pair[:2] if c != "CNY"} if pair else set()
        payload["_wanted"] = sorted(wanted) or ["USD"]
        payload["_amount"] = pair[2] if pair else 1.0
        payload["_base"] = pair[0] if pair else "USD"
        payload["_zh"] = query_is_chinese(query)
        return self.map_results(payload)

    def map_results(self, payload: Any) -> list[SearchResult]:
        if not isinstance(payload, dict) or not isinstance(payload.get("records"), list):
            return []
        wanted = set(payload.get("_wanted") or ["USD"])
        amount = float(payload.get("_amount") or 1.0)
        base = str(payload.get("_base") or "USD")
        zh = bool(payload.get("_zh"))
        data = payload.get("data") if isinstance(payload.get("data"), dict) else {}
        stamp = str(data.get("lastDate") or "")
        day = ""
        try:
            day = datetime.strptime(stamp[:10], "%Y-%m-%d").date().isoformat()
        except ValueError:
            pass
        results: list[SearchResult] = []
        for row in payload["records"]:
            if not isinstance(row, dict):
                continue
            code, pair_name, price = row.get("foreignCName"), row.get("vrtEName"), row.get("price")
            if not isinstance(code, str) or code not in wanted or not isinstance(pair_name, str):
                continue
            try:
                rate = float(str(price).replace(",", ""))
            except (TypeError, ValueError):
                continue
            # "100JPY/CNY" style pairs are quoted per hundred units.
            per = 100.0 if pair_name.startswith("100") else 1.0
            foreign_first = pair_name.replace("100", "").startswith(code)
            unit_rate = rate / per if foreign_first else (1.0 / rate if rate else 0.0)
            if base == "CNY" and unit_rate:
                converted = f"{amount:g} CNY = {amount / unit_rate:,.4f} {code}"
            else:
                converted = f"{amount:g} {code} = {amount * unit_rate:,.4f} CNY"
            quote_line = f"{pair_name} {rate:g}" + (f"（{stamp}）" if zh else f" ({stamp})")
            bits = [
                converted if amount != 1 else f"1 {code} = {unit_rate:.6g} CNY",
                (
                    f"人民币中间价 {quote_line}，中国外汇交易中心每个交易日 9:15 公布，不是实时汇率"
                    if zh
                    else f"RMB central parity {quote_line}, published by CFETS at 9:15 each "
                    "trading day; not a live market quote"
                ),
            ]
            title = (
                f"{code}/CNY 中间价 {unit_rate:.6g}（{day or stamp[:10]}）"
                if zh
                else f"{code} to CNY central parity: {unit_rate:.6g} (CFETS, {day or stamp[:10]})"
            )
            results.append(
                SearchResult(
                    title=title,
                    url=_PAGE,
                    snippet=clip(" · ".join(bits)),
                    engine=self.name,
                    rank=len(results),
                    published_age=day or None,
                    published_age_confident=bool(day),
                )
            )
        return results
