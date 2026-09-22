"""CoinGecko: the price of a cryptocurrency now, in USD and CNY.

  GET https://api.coingecko.com/api/v3/simple/price?ids=bitcoin&vs_currencies=usd,cny
  GET https://api.coingecko.com/api/v3/search?query=<name>

The public endpoint needs no key and shares a per-IP limit, so the engine
allows itself ten calls a minute. The common coins are mapped from their
symbols and Chinese names without a request; anything else goes through
CoinGecko's own search first. The price carries CoinGecko's last-updated
time, and the snippet says it is a spot aggregate, not an exchange quote.
"""

from __future__ import annotations

import re
from datetime import UTC, datetime
from typing import Any
from urllib.parse import quote_plus

from .base import SearchFilters, SearchResult
from .facts import _TOKEN_RE, STOP_WORDS, query_is_chinese
from .jsonapi import JsonApiEngine, clip

_API = "https://api.coingecko.com/api/v3"
_COINS = {
    "btc": "bitcoin", "bitcoin": "bitcoin", "比特币": "bitcoin",
    "eth": "ethereum", "ethereum": "ethereum", "以太坊": "ethereum", "以太币": "ethereum",
    "sol": "solana", "solana": "solana",
    "bnb": "binancecoin", "binance coin": "binancecoin", "币安币": "binancecoin",
    "xrp": "ripple", "ripple": "ripple", "瑞波": "ripple",
    "doge": "dogecoin", "dogecoin": "dogecoin", "狗狗币": "dogecoin",
    "ada": "cardano", "cardano": "cardano",
    "usdt": "tether", "tether": "tether", "泰达币": "tether",
    "usdc": "usd-coin",
    "ton": "the-open-network", "toncoin": "the-open-network",
    "trx": "tron", "tron": "tron", "波场": "tron",
    "dot": "polkadot", "polkadot": "polkadot",
    "avax": "avalanche-2", "avalanche": "avalanche-2",
    "link": "chainlink", "chainlink": "chainlink",
    "ltc": "litecoin", "litecoin": "litecoin", "莱特币": "litecoin",
    "matic": "matic-network", "pol": "matic-network", "polygon": "matic-network",
    "shib": "shiba-inu", "shiba": "shiba-inu",
    "bch": "bitcoin-cash", "bitcoin cash": "bitcoin-cash",
    "xlm": "stellar", "stellar": "stellar",
    "atom": "cosmos", "cosmos": "cosmos",
    "uni": "uniswap", "uniswap": "uniswap",
    "etc": "ethereum-classic",
    "xmr": "monero", "monero": "monero", "门罗币": "monero",
    "near": "near", "apt": "aptos", "aptos": "aptos", "sui": "sui",
    "arb": "arbitrum", "arbitrum": "arbitrum", "op": "optimism", "optimism": "optimism",
    "pepe": "pepe", "wif": "dogwifcoin", "hype": "hyperliquid", "hyperliquid": "hyperliquid",
}  # fmt: skip
_CRYPTO_ASK = re.compile(
    r"\b(crypto|cryptocurrency|coin|token|btc|eth|sol|bnb|xrp|doge|usdt|market cap|"
    r"bitcoin|ethereum|solana|dogecoin|altcoin)\b|币价|加密货币|数字货币|虚拟货币|比特币|以太坊|"
    r"狗狗币|币圈|市值",
    re.I,
)


# id -> (display name, symbol) for the table's coins: the symbol is the
# shortest ASCII key that maps to the id, the name the longest ASCII key.
_LABELS: dict[str, tuple[str, str]] = {}
for _key, _id in _COINS.items():
    if not _key.isascii():
        continue
    name, symbol = _LABELS.get(_id, ("", ""))
    if not symbol or len(_key) < len(symbol):
        symbol = _key
    if not name or len(_key) > len(name):
        name = _key
    _LABELS[_id] = (name, symbol)


_PRICE_WORDS = re.compile(
    r"\b(price|prices|quote|rate|worth|value|today|now|live|usd|cny|dollars?)\b|"
    r"价格|多少钱|行情|报价|今日|现在|美元|人民币",
    re.I,
)


def coins_in(query: str, *, limit: int = 3) -> list[str]:
    """CoinGecko ids for the coins the question names, from the table only."""
    lowered = query.lower()
    found: list[str] = []
    for key in sorted(_COINS, key=len, reverse=True):
        pattern = r"(?<![a-z])" + re.escape(key) + r"(?![a-z])" if key.isascii() else re.escape(key)
        if re.search(pattern, lowered) and _COINS[key] not in found:
            found.append(_COINS[key])
            lowered = re.sub(pattern, " ", lowered)
    return found[:limit]


class CoinGeckoEngine(JsonApiEngine):
    """Spot price, 24h change and market cap of a cryptocurrency."""

    name = "coingecko"
    single_site = True
    categories = frozenset({"finance", "finance.crypto"})
    impersonate = None
    direct_answer = True
    rate_limit_per_minute = 10
    rate_limit_max_wait = 1.0
    description = "CoinGecko: current price, 24h change and market cap of a cryptocurrency."

    def claims(self, query: str) -> bool:
        return bool(_CRYPTO_ASK.search(query)) and bool(
            coins_in(query) or self._unknown_coin_words(query)
        )

    @staticmethod
    def _unknown_coin_words(query: str) -> list[str]:
        text = _PRICE_WORDS.sub(" ", _CRYPTO_ASK.sub(" ", query))
        words = [
            w.lower()
            for w in _TOKEN_RE.findall(text)
            if w.lower() not in STOP_WORDS and not w.isdigit()
        ]
        return words[:2]

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        return _price_url(coins_in(query) or ["bitcoin"])

    async def fetch_results(
        self, query: str, max_results: int, filters: SearchFilters | None
    ) -> list[SearchResult]:
        ids = coins_in(query)
        names: dict[str, tuple[str, str]] = {}
        if not ids:
            words = self._unknown_coin_words(query)
            if not words:
                return []
            found = await self._get_json(f"{_API}/search?query={quote_plus(' '.join(words))}")
            coins = found.get("coins") if isinstance(found, dict) else None
            for coin in (coins or [])[:1]:
                if isinstance(coin, dict) and isinstance(coin.get("id"), str):
                    ids.append(coin["id"])
                    names[coin["id"]] = (str(coin.get("name", "")), str(coin.get("symbol", "")))
            if not ids:
                return []
        prices = await self._get_json(_price_url(ids))
        if not isinstance(prices, dict):
            return []
        prices = {k: v for k, v in prices.items() if k in ids}
        prices["_names"] = names
        prices["_zh"] = query_is_chinese(query)
        return self.map_results(prices)

    def map_results(self, payload: Any) -> list[SearchResult]:
        if not isinstance(payload, dict):
            return []
        names = payload.get("_names") if isinstance(payload.get("_names"), dict) else {}
        zh = bool(payload.get("_zh"))
        results: list[SearchResult] = []
        for coin_id, row in payload.items():
            if coin_id.startswith("_") or not isinstance(row, dict):
                continue
            usd, cny = row.get("usd"), row.get("cny")
            if not isinstance(usd, (int, float)):
                continue
            name, symbol = names.get(coin_id) or _LABELS.get(coin_id) or (
                coin_id.replace("-", " ").title(),
                "",
            )
            name = name.title() if name.islower() else name
            label = f"{name} ({symbol.upper()})" if symbol else name
            stamp = row.get("last_updated_at")
            when = (
                datetime.fromtimestamp(stamp, UTC).strftime("%Y-%m-%d %H:%M UTC")
                if isinstance(stamp, (int, float))
                else ""
            )
            day = when[:10]
            bits = [f"1 {symbol.upper() or name} = {_money(usd)} USD"]
            if isinstance(cny, (int, float)):
                bits[0] += f" = {_money(cny)} CNY"
            change = row.get("usd_24h_change")
            if isinstance(change, (int, float)):
                bits.append(f"24h {change:+.2f}%")
            cap = row.get("usd_market_cap")
            if isinstance(cap, (int, float)) and cap:
                bits.append(f"market cap {_money(cap)} USD")
            bits.append(
                ("CoinGecko 聚合现货价，" + (f"更新于 {when}，" if when else "") + "不是交易所报价")
                if zh
                else "CoinGecko aggregate spot price"
                + (f", updated {when}" if when else "")
                + "; not an exchange quote"
            )
            results.append(
                SearchResult(
                    title=f"{label}: {_money(usd)} USD" + (f" ({when})" if when else ""),
                    url=f"https://www.coingecko.com/en/coins/{coin_id}",
                    snippet=clip(" · ".join(bits)),
                    engine=self.name,
                    rank=len(results),
                    published_age=day or None,
                    published_age_confident=bool(day),
                )
            )
        return results


def _price_url(ids: list[str]) -> str:
    return (
        f"{_API}/simple/price?ids={','.join(ids)}&vs_currencies=usd,cny"
        "&include_24hr_change=true&include_market_cap=true&include_last_updated_at=true"
    )


def _money(value: float) -> str:
    if abs(value) >= 1e9:
        return f"{value / 1e9:,.2f} billion"
    if abs(value) >= 1000:
        return f"{value:,.0f}"
    if abs(value) >= 1:
        return f"{value:,.2f}"
    return f"{value:.6g}"
