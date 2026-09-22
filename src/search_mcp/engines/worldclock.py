"""The current time in a named place. No network: the system's zoneinfo.

"What time is it in Tokyo" and "现在北京时间几点" are the questions an
agent answers wrongly most cheaply: its own clock is the cutoff of its
training data, and a web snippet about the time in Tokyo is whatever the
crawler saw. The server has a clock and the IANA zone database, so this
engine answers from them, with the UTC offset and whether daylight saving
is in force. It is registered as an engine so that the answer arrives in
the same list, with the same weight, as any other record.
"""

from __future__ import annotations

import re
from datetime import UTC, datetime
from typing import Any
from zoneinfo import ZoneInfo, ZoneInfoNotFoundError

from .base import SearchFilters, SearchResult
from .facts import query_is_chinese
from .jsonapi import JsonApiEngine

_TIME_ASK = re.compile(
    r"\b(what time|current time|local time|time now|time zone|timezone|utc offset|"
    r"what(?:'s| is) the (?:date|time)|today'?s date|what day is it)\b|"
    r"几点|现在时间|当地时间|北京时间|时区|时差|今天几号|今天是|现在是|什么时候",
    re.I,
)

# Display name -> IANA zone. Chinese and English spellings both key the table.
ZONES: dict[str, tuple[str, str]] = {}


def _add(zone: str, name: str, *aliases: str) -> None:
    for key in (name, *aliases):
        ZONES[key.lower()] = (zone, name)


for _row in (
    ("Asia/Shanghai", "Beijing", "北京", "上海", "shanghai", "china", "中国", "北京时间", "cst",
     "广州", "深圳", "杭州", "成都", "重庆", "武汉", "南京", "西安", "天津"),
    ("Asia/Hong_Kong", "Hong Kong", "香港"),
    ("Asia/Taipei", "Taipei", "台北", "台湾", "taiwan"),
    ("Asia/Tokyo", "Tokyo", "东京", "japan", "日本", "大阪", "osaka", "jst"),
    ("Asia/Seoul", "Seoul", "首尔", "korea", "韩国"),
    ("Asia/Singapore", "Singapore", "新加坡"),
    ("Asia/Bangkok", "Bangkok", "曼谷", "thailand", "泰国", "越南", "vietnam", "hanoi", "河内"),
    ("Asia/Jakarta", "Jakarta", "雅加达", "indonesia", "印尼"),
    ("Asia/Kuala_Lumpur", "Kuala Lumpur", "吉隆坡", "malaysia", "马来西亚"),
    ("Asia/Manila", "Manila", "马尼拉", "philippines", "菲律宾"),
    ("Asia/Kolkata", "New Delhi", "新德里", "india", "印度", "mumbai", "孟买", "ist"),
    ("Asia/Dubai", "Dubai", "迪拜", "uae", "阿联酋", "abu dhabi"),
    ("Asia/Riyadh", "Riyadh", "利雅得", "saudi arabia", "沙特"),
    ("Asia/Tehran", "Tehran", "德黑兰", "iran", "伊朗"),
    ("Asia/Karachi", "Karachi", "卡拉奇", "pakistan", "巴基斯坦"),
    ("Asia/Dhaka", "Dhaka", "达卡", "bangladesh", "孟加拉"),
    ("Asia/Almaty", "Almaty", "阿拉木图", "kazakhstan", "哈萨克斯坦"),
    ("Europe/Moscow", "Moscow", "莫斯科", "russia", "俄罗斯", "msk"),
    ("Europe/Istanbul", "Istanbul", "伊斯坦布尔", "turkey", "土耳其"),
    ("Europe/London", "London", "伦敦", "uk", "britain", "英国", "gmt", "bst"),
    ("Europe/Dublin", "Dublin", "都柏林", "ireland", "爱尔兰"),
    ("Europe/Paris", "Paris", "巴黎", "france", "法国", "cet", "cest"),
    ("Europe/Berlin", "Berlin", "柏林", "germany", "德国", "munich", "慕尼黑", "frankfurt",
     "法兰克福"),
    ("Europe/Amsterdam", "Amsterdam", "阿姆斯特丹", "netherlands", "荷兰"),
    ("Europe/Brussels", "Brussels", "布鲁塞尔", "belgium", "比利时"),
    ("Europe/Zurich", "Zurich", "苏黎世", "switzerland", "瑞士", "geneva", "日内瓦"),
    ("Europe/Madrid", "Madrid", "马德里", "spain", "西班牙", "barcelona", "巴塞罗那"),
    ("Europe/Rome", "Rome", "罗马", "italy", "意大利", "milan", "米兰"),
    ("Europe/Vienna", "Vienna", "维也纳", "austria", "奥地利"),
    ("Europe/Stockholm", "Stockholm", "斯德哥尔摩", "sweden", "瑞典"),
    ("Europe/Oslo", "Oslo", "奥斯陆", "norway", "挪威"),
    ("Europe/Copenhagen", "Copenhagen", "哥本哈根", "denmark", "丹麦"),
    ("Europe/Helsinki", "Helsinki", "赫尔辛基", "finland", "芬兰"),
    ("Europe/Warsaw", "Warsaw", "华沙", "poland", "波兰"),
    ("Europe/Prague", "Prague", "布拉格", "czechia", "捷克"),
    ("Europe/Athens", "Athens", "雅典", "greece", "希腊"),
    ("Europe/Lisbon", "Lisbon", "里斯本", "portugal", "葡萄牙"),
    ("Europe/Kyiv", "Kyiv", "基辅", "ukraine", "乌克兰"),
    ("Africa/Cairo", "Cairo", "开罗", "egypt", "埃及"),
    ("Africa/Johannesburg", "Johannesburg", "约翰内斯堡", "south africa", "南非"),
    ("Africa/Lagos", "Lagos", "拉各斯", "nigeria", "尼日利亚"),
    ("Africa/Nairobi", "Nairobi", "内罗毕", "kenya", "肯尼亚"),
    ("America/New_York", "New York", "纽约", "washington", "华盛顿", "boston", "波士顿", "miami",
     "迈阿密", "eastern time", "美东", "美国东部", "est", "edt"),
    ("America/Chicago", "Chicago", "芝加哥", "houston", "休斯顿", "dallas", "达拉斯",
     "central time", "美中", "cdt"),
    ("America/Denver", "Denver", "丹佛", "mountain time", "mst", "mdt"),
    ("America/Los_Angeles", "Los Angeles", "洛杉矶", "san francisco", "旧金山", "seattle",
     "西雅图", "silicon valley", "硅谷", "pacific time", "美西", "美国西部", "pst", "pdt",
     "california", "加州"),
    ("America/Toronto", "Toronto", "多伦多", "canada", "加拿大", "montreal", "蒙特利尔", "ottawa"),
    ("America/Vancouver", "Vancouver", "温哥华"),
    ("America/Mexico_City", "Mexico City", "墨西哥城", "mexico", "墨西哥"),
    ("America/Sao_Paulo", "São Paulo", "圣保罗", "brazil", "巴西", "rio de janeiro", "里约"),
    ("America/Argentina/Buenos_Aires", "Buenos Aires", "布宜诺斯艾利斯", "argentina", "阿根廷"),
    ("America/Santiago", "Santiago", "圣地亚哥", "chile", "智利"),
    ("America/Bogota", "Bogotá", "波哥大", "colombia", "哥伦比亚"),
    ("America/Lima", "Lima", "利马", "peru", "秘鲁"),
    ("Pacific/Honolulu", "Honolulu", "檀香山", "hawaii", "夏威夷"),
    ("Pacific/Auckland", "Auckland", "奥克兰", "new zealand", "新西兰", "wellington", "惠灵顿"),
    ("Australia/Sydney", "Sydney", "悉尼", "australia", "澳大利亚", "澳洲", "melbourne", "墨尔本",
     "canberra", "堪培拉", "aest", "aedt"),
    ("Australia/Perth", "Perth", "珀斯"),
    ("Australia/Brisbane", "Brisbane", "布里斯班"),
    ("UTC", "UTC", "utc", "gmt+0", "universal time", "协调世界时", "世界时", "格林尼治"),
):
    _add(*_row)

# zone -> the Chinese name, the first CJK alias of its row.
ZH_ZONES: dict[str, str] = {}
for _key, (_zone, _name) in ZONES.items():
    if _zone not in ZH_ZONES and re.search(r"[\u4e00-\u9fff]", _key):
        ZH_ZONES[_zone] = _key

_ZONE_KEYS = sorted(ZONES, key=len, reverse=True)
_IANA = re.compile(r"\b((?:Africa|America|Antarctica|Asia|Atlantic|Australia|Europe|Indian|"
                   r"Pacific)/[A-Za-z_]+(?:/[A-Za-z_]+)?)\b")


def zones_in(query: str, *, limit: int = 3) -> list[tuple[str, str]]:
    """`(zone, display name)` for each place the question names, in order."""
    hits: list[tuple[int, tuple[str, str]]] = []
    taken: set[str] = set()
    for m in _IANA.finditer(query):
        zone = m.group(1)
        try:
            ZoneInfo(zone)
        except ZoneInfoNotFoundError:
            continue
        if zone not in taken:
            taken.add(zone)
            hits.append((m.start(), (zone, zone.rsplit("/", 1)[-1].replace("_", " "))))
    lowered = query.lower()
    for key in _ZONE_KEYS:
        if re.fullmatch(r"[a-z+0-9 ]+", key):
            m2 = re.search(r"(?<![a-z])" + re.escape(key) + r"(?![a-z])", lowered)
            at = m2.start() if m2 else -1
        else:
            at = lowered.find(key)
        if at < 0:
            continue
        zone, name = ZONES[key]
        if zone in taken:
            continue
        taken.add(zone)
        hits.append((at, (zone, name)))
        lowered = lowered[:at] + " " * len(key) + lowered[at + len(key):]
    return [row for _at, row in sorted(hits)][:limit]


class WorldClockEngine(JsonApiEngine):
    """The current date and time in a named place, from the server's clock."""

    name = "worldclock"
    single_site = True
    categories = frozenset({"calendar", "calendar.clock"})
    impersonate = None
    direct_answer = True
    supports_browser_fallback = False
    description = "The current date and time in a named city or zone, from the system clock."

    def claims(self, query: str) -> bool:
        return bool(_TIME_ASK.search(query))

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        zones = zones_in(query) or [("UTC", "UTC")]
        return f"https://time.is/{zones[0][1].replace(' ', '_')}"

    async def fetch_results(
        self, query: str, max_results: int, filters: SearchFilters | None
    ) -> list[SearchResult]:
        zones = zones_in(query)
        if not zones:
            zones = [("Asia/Shanghai", "Beijing")] if query_is_chinese(query) else [("UTC", "UTC")]
        now = datetime.now(UTC)
        return self.map_results({"now": now, "zones": zones, "zh": query_is_chinese(query)})

    def map_results(self, payload: Any) -> list[SearchResult]:
        if not isinstance(payload, dict):
            return []
        now = payload.get("now")
        zones = payload.get("zones") or []
        zh = bool(payload.get("zh"))
        if not isinstance(now, datetime):
            return []
        results: list[SearchResult] = []
        for zone, name in zones:
            try:
                local = now.astimezone(ZoneInfo(zone))
            except ZoneInfoNotFoundError:
                continue
            offset = local.strftime("%z")
            offset = f"UTC{offset[:3]}:{offset[3:]}" if offset else "UTC"
            dst = bool(local.dst())
            weekday = local.strftime("%A")
            if zh:
                days = ["星期一", "星期二", "星期三", "星期四", "星期五", "星期六", "星期日"]
                weekday = days[local.weekday()]
                title = (
                    f"{ZH_ZONES.get(zone, name)}现在时间："
                    f"{local.strftime('%Y-%m-%d %H:%M')}（{zone}）"
                )
                snippet = (
                    f"{local.strftime('%Y年%m月%d日 %H:%M:%S')} {weekday}，{offset}，"
                    f"{'夏令时生效' if dst else '无夏令时'} · "
                    f"协调世界时 {now.strftime('%Y-%m-%d %H:%M:%S')}"
                    " · 来自服务器时钟和 IANA 时区数据库，不是网页"
                )
            else:
                title = f"Current time in {name}: {local.strftime('%Y-%m-%d %H:%M')} ({zone})"
                snippet = (
                    f"{local.strftime('%Y-%m-%d %H:%M:%S')} {weekday}, {offset}, "
                    f"{'daylight saving in force' if dst else 'no daylight saving'} · "
                    f"UTC {now.strftime('%Y-%m-%d %H:%M:%S')} · from the server clock and the "
                    "IANA zone database, not a web page"
                )
            results.append(
                SearchResult(
                    title=title,
                    url=f"https://time.is/{name.replace(' ', '_')}",
                    snippet=snippet,
                    engine=self.name,
                    rank=len(results),
                    published_age=local.date().isoformat(),
                    published_age_confident=True,
                )
            )
        return results
