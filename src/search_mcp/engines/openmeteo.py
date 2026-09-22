"""Open-Meteo: the forecast for a place named in the query, as data.

  GET https://geocoding-api.open-meteo.com/v1/search?name=<place>
  GET https://api.open-meteo.com/v1/forecast?latitude=..&longitude=..

Weather pages are the classic stale snippet: a search engine's summary of a
forecast page is the forecast the crawler saw. This asks the model provider
for the numbers at request time and says when they were issued. The words
that make a query a weather question (weather, forecast, 天气, 明天) are
removed and the rest is geocoded; the first match wins.
"""

from __future__ import annotations

import re
from typing import Any
from urllib.parse import quote_plus

from .base import SearchFilters, SearchResult
from .facts import query_is_chinese
from .jsonapi import JsonApiEngine, clip

_GEOCODE = "https://geocoding-api.open-meteo.com/v1/search"
_FORECAST = "https://api.open-meteo.com/v1/forecast"

_WEATHER_WORDS = re.compile(
    r"\b(weather|forecast|temperature|temperatures|rain|snow|wind|humidity|today|tomorrow|"
    r"tonight|this week|weekend|in|at|for|the|what|is|will|it|be|like|of|hourly|daily|"
    r"current|now)\b|天气|预报|气温|温度|降雨|下雨|下雪|风力|湿度|今天|明天|后天|今晚|本周|周末|"
    r"未来|几天|一周|怎么样|如何|查|一下|的|吗|现在",
    re.I,
)

# WMO weather interpretation codes, as Open-Meteo documents them.
_CODES = {
    0: ("clear sky", "晴"),
    1: ("mainly clear", "大部晴朗"),
    2: ("partly cloudy", "局部多云"),
    3: ("overcast", "阴"),
    45: ("fog", "雾"),
    48: ("rime fog", "雾（结霜）"),
    51: ("light drizzle", "小毛毛雨"),
    53: ("drizzle", "毛毛雨"),
    55: ("dense drizzle", "大毛毛雨"),
    61: ("light rain", "小雨"),
    63: ("rain", "中雨"),
    65: ("heavy rain", "大雨"),
    66: ("freezing rain", "冻雨"),
    67: ("heavy freezing rain", "强冻雨"),
    71: ("light snow", "小雪"),
    73: ("snow", "中雪"),
    75: ("heavy snow", "大雪"),
    77: ("snow grains", "米雪"),
    80: ("light showers", "小阵雨"),
    81: ("showers", "阵雨"),
    82: ("violent showers", "强阵雨"),
    85: ("snow showers", "阵雪"),
    86: ("heavy snow showers", "强阵雪"),
    95: ("thunderstorm", "雷暴"),
    96: ("thunderstorm with hail", "雷暴伴冰雹"),
    99: ("thunderstorm with heavy hail", "雷暴伴大冰雹"),
}


# The words that make a question a weather question. Narrower than the strip
# list above, which also removes "in", "today" and other filler.
_WEATHER_ASK = re.compile(
    r"\b(weather|forecast|temperature|rain|snow|humidity|wind speed)\b|天气|预报|气温|温度|"
    r"下雨|下雪|湿度|风力",
    re.I,
)


def place_from(query: str) -> str:
    """The query with the weather words removed: what is left names the place."""
    text = _WEATHER_WORDS.sub(" ", query)
    text = re.sub(r"[?？,，。!！]", " ", text)
    return " ".join(text.split())


class OpenMeteoEngine(JsonApiEngine):
    """Current conditions and a three-day forecast for the place the query names."""

    name = "openmeteo"
    single_site = True
    categories = frozenset({"weather"})
    impersonate = None
    direct_answer = True
    description = "Open-Meteo: current conditions and a 3-day forecast for a named place."

    def claims(self, query: str) -> bool:
        return bool(_WEATHER_ASK.search(query)) and bool(place_from(query))

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        lang = "zh" if query_is_chinese(query) else "en"
        return f"{_GEOCODE}?name={quote_plus(place_from(query) or query)}&count=1&language={lang}"

    async def fetch_results(
        self, query: str, max_results: int, filters: SearchFilters | None
    ) -> list[SearchResult]:
        place = place_from(query)
        if not place:
            return []
        geo = await self._get_json(self.build_url(query, max_results, filters))
        hits = geo.get("results") if isinstance(geo, dict) else None
        if not isinstance(hits, list) or not hits or not isinstance(hits[0], dict):
            return []
        spot = hits[0]
        try:
            lat, lon = float(spot["latitude"]), float(spot["longitude"])
        except (KeyError, TypeError, ValueError):
            return []
        url = (
            f"{_FORECAST}?latitude={lat:.4f}&longitude={lon:.4f}"
            "&current=temperature_2m,relative_humidity_2m,weather_code,wind_speed_10m"
            "&daily=weather_code,temperature_2m_max,temperature_2m_min,"
            "precipitation_probability_max,precipitation_sum&timezone=auto&forecast_days=3"
        )
        forecast = await self._get_json(url)
        if not isinstance(forecast, dict):
            return []
        forecast["_place"] = spot
        forecast["_url"] = url
        forecast["_zh"] = query_is_chinese(query)
        return self.map_results(forecast)

    def map_results(self, payload: Any) -> list[SearchResult]:
        if not isinstance(payload, dict):
            return []
        spot = payload.get("_place") or {}
        zh = bool(payload.get("_zh"))
        name = ", ".join(
            str(spot[k]) for k in ("name", "admin1", "country") if isinstance(spot.get(k), str)
        )
        current = payload.get("current") if isinstance(payload.get("current"), dict) else {}
        daily = payload.get("daily") if isinstance(payload.get("daily"), dict) else {}
        issued = str(current.get("time") or "")
        tz = str(payload.get("timezone") or "")
        bits = []
        if current:
            sky = self._sky(current.get("weather_code"), zh)
            bits.append(
                (
                    f"现在 {current.get('temperature_2m')}°C，{sky}，"
                    f"湿度 {current.get('relative_humidity_2m')}%，"
                    f"风速 {current.get('wind_speed_10m')} km/h（{issued} {tz}）"
                )
                if zh
                else (
                    f"Now {current.get('temperature_2m')}°C, {sky}, humidity "
                    f"{current.get('relative_humidity_2m')}%, "
                    f"wind {current.get('wind_speed_10m')} km/h (observed {issued} {tz})"
                )
            )
        days = daily.get("time") if isinstance(daily.get("time"), list) else []
        for i, day in enumerate(days[:3]):
            try:
                sky = self._sky(daily["weather_code"][i], zh)
                hi, lo = daily["temperature_2m_max"][i], daily["temperature_2m_min"][i]
                prob = daily["precipitation_probability_max"][i]
                mm = daily["precipitation_sum"][i]
            except (KeyError, IndexError, TypeError):
                continue
            bits.append(
                f"{day}：{sky}，{lo}到{hi}°C，降水概率 {prob}%，降水量 {mm} mm"
                if zh
                else f"{day}: {sky}, {lo} to {hi}°C, {prob}% chance of precipitation, {mm} mm"
            )
        if not bits:
            return []
        title = (
            f"{name} 天气预报（Open-Meteo）" if zh else f"Weather forecast for {name} (Open-Meteo)"
        )
        return [
            SearchResult(
                title=title,
                url=str(payload.get("_url") or _FORECAST),
                snippet=clip(" · ".join(bits), cap=700),
                engine=self.name,
                rank=0,
                published_age=issued[:10],
                published_age_confident=bool(issued[:10]),
            )
        ]

    @staticmethod
    def _sky(code: Any, zh: bool) -> str:
        try:
            pair = _CODES.get(int(code))
        except (TypeError, ValueError):
            pair = None
        if pair is None:
            return f"code {code}"
        return pair[1] if zh else pair[0]
