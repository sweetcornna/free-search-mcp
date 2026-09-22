"""Shared helpers for the fact sources: engines that answer from a registry,
a catalogue or a data API rather than from a web index.

A web engine takes the whole query. A registry takes a name, so these engines
have to pick the name out of "httpx latest version" first. `candidate_names`
does that with a stop list of the words a question about a thing carries along
with the thing. It is deliberately dumb: the engines that use it run only when
a caller asked for their category, and a wrong guess costs one 404.
"""

from __future__ import annotations

import re
from datetime import UTC, datetime

from .base import detect_query_region

# Words a "what version / which release / is it supported" question carries
# that are never the name of the thing asked about. English and Chinese.
STOP_WORDS = frozenset({
    "a", "an", "the", "of", "for", "in", "on", "to", "is", "are", "was", "be", "what", "which",
    "when", "who", "how", "latest", "newest", "current", "stable", "new", "recent", "last",
    "next", "version", "versions", "release", "released", "releases", "update", "updated",
    "upgrade", "changelog", "history", "date", "package", "library", "lib", "module",
    "framework", "tool", "cli", "sdk", "api", "app", "plugin", "python", "pip", "pypi", "node",
    "npm", "js", "javascript", "typescript", "rust", "cargo", "crate", "crates", "go",
    "golang", "java", "maven", "gem", "ruby", "php", "composer", "dotnet", "nuget", "eol",
    "end", "life", "support", "supported", "lifecycle", "security", "vulnerability",
    "vulnerabilities", "cve", "exploit", "patch", "advisory", "install", "download", "still",
    "yet", "does", "do", "did", "has", "have", "with", "and", "or", "not", "any", "there",
    "it", "its", "this", "that", "please", "tell", "me", "find", "check", "look", "up", "最新",
    "版本", "发布", "更新", "发行", "支持", "周期", "漏洞", "安全", "下载", "安装", "什么",
    "哪个", "多少", "是", "的", "了", "吗", "还", "还是", "现在", "目前", "有", "没有", "在",
    "查", "一下", "请", "帮", "我", "要",
})

_TOKEN_RE = re.compile(r"@?[A-Za-z0-9][A-Za-z0-9._@/-]*[A-Za-z0-9]|[A-Za-z0-9]")
_VERSION_LIKE = re.compile(r"^v?\d+(\.\d+)+$")


def candidate_names(query: str, *, limit: int = 3) -> list[str]:
    """Tokens of `query` that could name a package or product, in order.

    Lower-cased, quotes and trailing punctuation dropped, stop words, bare
    numbers and version numbers removed, duplicates removed. `@scope/name`
    and `owner/repo` survive as one token.
    """
    seen: list[str] = []
    for raw in _TOKEN_RE.findall(query):
        token = raw.lower().strip("._-")
        if not token or token in STOP_WORDS or token in seen:
            continue
        if token.isdigit() or _VERSION_LIKE.match(token):
            continue
        seen.append(token)
        if len(seen) >= limit:
            break
    return seen


def query_is_chinese(query: str) -> bool:
    return detect_query_region(query, "").endswith("-zh")


def iso_day(value: object) -> str:
    """`YYYY-MM-DD` from an ISO timestamp, a date, or an epoch in seconds."""
    if isinstance(value, int | float):
        try:
            return datetime.fromtimestamp(float(value), UTC).strftime("%Y-%m-%d")
        except (OverflowError, OSError, ValueError):
            return ""
    if isinstance(value, str) and len(value) >= 10 and value[4] == "-" and value[7] == "-":
        return value[:10]
    return ""


def compact_number(value: object) -> str:
    """`24870895` -> `24,870,895`; anything else unchanged as text."""
    try:
        return f"{int(float(str(value))):,}"
    except (TypeError, ValueError):
        return str(value)


# ---------------------------------------------------------------------------
# Question shapes, for `Engine.claims`. Offline, regex only.
# ---------------------------------------------------------------------------

_VERSION_ASK = re.compile(
    r"\b(latest|newest|current|stable|version|versions|release|released|releases|changelog|"
    r"upgrade|update|updated|eol|end[- ]of[- ]life|lifecycle|supported|support ends?|"
    r"still (?:supported|maintained))\b|最新|版本|发布|更新|升级|停止支持|支持到|支持周期|生命周期|"
    r"还(?:在|有)?(?:维护|支持)",
    re.I,
)

_ECOSYSTEMS = {
    "pypi": re.compile(r"\b(pip|pypi|python|py)\b|python 包|python 库", re.I),
    "npm": re.compile(r"\b(npm|node|nodejs|node\.js|javascript|typescript|js|yarn|pnpm)\b", re.I),
    "cargo": re.compile(r"\b(cargo|crate|crates|rust)\b", re.I),
    "maven": re.compile(r"\b(maven|gradle|java|kotlin|jar)\b", re.I),
    "rubygems": re.compile(r"\b(gem|ruby|rails|bundler)\b", re.I),
    "go": re.compile(r"\b(go|golang)\b|go 模块|go 库", re.I),
    "homebrew": re.compile(r"\b(brew|homebrew|formula)\b", re.I),
    "docker": re.compile(r"\b(docker|dockerhub|image|container)\b|镜像|容器", re.I),
    "packagist": re.compile(r"\b(composer|packagist|php|laravel|symfony)\b", re.I),
    "nuget": re.compile(r"\b(nuget|dotnet|\.net|c#|csharp)\b", re.I),
    # Not a package ecosystem, but a version question about an app belongs
    # to the App Store engine and to none of the registries.
    "app": re.compile(r"\b(app store|ios|iphone|ipad|android|app)\b|应用商店|苹果版|安卓", re.I),
}


def version_question(query: str) -> bool:
    """Whether the question asks which version, release or support state."""
    return bool(_VERSION_ASK.search(query))


def ecosystem_hints(query: str) -> frozenset[str]:
    """Package ecosystems the question names: {"pypi"}, {"npm", "docker"}, ..."""
    return frozenset(name for name, rx in _ECOSYSTEMS.items() if rx.search(query))


# Countries an agent asks about, for the statistics and holiday sources. ISO
# alpha-3 with the alpha-2 the holiday APIs want and an English display name.
# English names and Chinese names are both keys; multi-word names before their
# words. World Bank's own names ("Korea, Rep.") are unrecognisable in a
# question, so aliases carry the load.
COUNTRIES: dict[str, tuple[str, str, str]] = {}


def _add_country(iso3: str, iso2: str, name: str, *aliases: str) -> None:
    for key in (name, *aliases):
        COUNTRIES[key.lower()] = (iso3, iso2, name)


for _row in (
    ("CHN", "CN", "China", "中国", "中华人民共和国", "prc", "mainland china"),
    ("USA", "US", "United States", "美国", "usa", "u.s.", "america", "united states of america"),
    ("JPN", "JP", "Japan", "日本"),
    ("DEU", "DE", "Germany", "德国"),
    ("GBR", "GB", "United Kingdom", "英国", "uk", "britain", "great britain"),
    ("FRA", "FR", "France", "法国"),
    ("IND", "IN", "India", "印度"),
    ("RUS", "RU", "Russia", "俄罗斯", "russian federation"),
    ("BRA", "BR", "Brazil", "巴西"),
    ("KOR", "KR", "South Korea", "韩国", "korea", "republic of korea"),
    ("CAN", "CA", "Canada", "加拿大"),
    ("AUS", "AU", "Australia", "澳大利亚", "澳洲"),
    ("ITA", "IT", "Italy", "意大利"),
    ("ESP", "ES", "Spain", "西班牙"),
    ("MEX", "MX", "Mexico", "墨西哥"),
    ("IDN", "ID", "Indonesia", "印尼", "印度尼西亚"),
    ("TUR", "TR", "Turkey", "土耳其", "türkiye", "turkiye"),
    ("NLD", "NL", "Netherlands", "荷兰", "the netherlands", "holland"),
    ("CHE", "CH", "Switzerland", "瑞士"),
    ("SAU", "SA", "Saudi Arabia", "沙特", "沙特阿拉伯"),
    ("VNM", "VN", "Vietnam", "越南", "viet nam"),
    ("THA", "TH", "Thailand", "泰国"),
    ("SGP", "SG", "Singapore", "新加坡"),
    ("MYS", "MY", "Malaysia", "马来西亚"),
    ("PHL", "PH", "Philippines", "菲律宾", "the philippines"),
    ("PAK", "PK", "Pakistan", "巴基斯坦"),
    ("BGD", "BD", "Bangladesh", "孟加拉国", "孟加拉"),
    ("NGA", "NG", "Nigeria", "尼日利亚"),
    ("EGY", "EG", "Egypt", "埃及"),
    ("ZAF", "ZA", "South Africa", "南非"),
    ("ARG", "AR", "Argentina", "阿根廷"),
    ("POL", "PL", "Poland", "波兰"),
    ("SWE", "SE", "Sweden", "瑞典"),
    ("NOR", "NO", "Norway", "挪威"),
    ("DNK", "DK", "Denmark", "丹麦"),
    ("FIN", "FI", "Finland", "芬兰"),
    ("BEL", "BE", "Belgium", "比利时"),
    ("AUT", "AT", "Austria", "奥地利"),
    ("PRT", "PT", "Portugal", "葡萄牙"),
    ("GRC", "GR", "Greece", "希腊"),
    ("IRL", "IE", "Ireland", "爱尔兰"),
    ("ISR", "IL", "Israel", "以色列"),
    ("IRN", "IR", "Iran", "伊朗"),
    ("IRQ", "IQ", "Iraq", "伊拉克"),
    ("ARE", "AE", "United Arab Emirates", "阿联酋", "uae"),
    ("QAT", "QA", "Qatar", "卡塔尔"),
    ("NZL", "NZ", "New Zealand", "新西兰"),
    ("CHL", "CL", "Chile", "智利"),
    ("COL", "CO", "Colombia", "哥伦比亚"),
    ("PER", "PE", "Peru", "秘鲁"),
    ("UKR", "UA", "Ukraine", "乌克兰"),
    ("CZE", "CZ", "Czechia", "捷克", "czech republic"),
    ("HUN", "HU", "Hungary", "匈牙利"),
    ("ROU", "RO", "Romania", "罗马尼亚"),
    ("KAZ", "KZ", "Kazakhstan", "哈萨克斯坦"),
    ("MNG", "MN", "Mongolia", "蒙古", "蒙古国"),
    ("PRK", "KP", "North Korea", "朝鲜"),
    ("KHM", "KH", "Cambodia", "柬埔寨"),
    ("LAO", "LA", "Laos", "老挝"),
    ("MMR", "MM", "Myanmar", "缅甸"),
    ("NPL", "NP", "Nepal", "尼泊尔"),
    ("LKA", "LK", "Sri Lanka", "斯里兰卡"),
    ("ETH", "ET", "Ethiopia", "埃塞俄比亚"),
    ("KEN", "KE", "Kenya", "肯尼亚"),
    ("MAR", "MA", "Morocco", "摩洛哥"),
    ("HKG", "HK", "Hong Kong", "香港", "hong kong sar"),
    ("MAC", "MO", "Macao", "澳门", "macau"),
    ("WLD", "1W", "World", "世界", "全球", "global"),
    ("EUU", "EU", "European Union", "欧盟", "eu"),
):
    _add_country(*_row)

# iso3 -> the Chinese name, for answers to Chinese questions.
ZH_COUNTRY: dict[str, str] = {}
for _key, (_iso3, _iso2, _name) in COUNTRIES.items():
    if _iso3 not in ZH_COUNTRY and re.search(r"[\u4e00-\u9fff]", _key):
        ZH_COUNTRY[_iso3] = _key

_COUNTRY_KEYS = sorted(COUNTRIES, key=len, reverse=True)
_ASCII_WORD = re.compile(r"^[a-z. ]+$")


def countries_in(query: str, *, limit: int = 2) -> list[tuple[str, str, str]]:
    """`(iso3, iso2, name)` for each country the question names, in order.

    Longer names are matched first so "South Korea" is not read as "Korea"
    and "United States" not as "States". ASCII names match on word
    boundaries, so "uk" does not fire inside "ukraine".
    """
    lowered = query.lower()
    hits: list[tuple[int, tuple[str, str, str]]] = []
    taken: set[str] = set()
    for key in _COUNTRY_KEYS:
        if _ASCII_WORD.match(key):
            m = re.search(r"(?<![a-z])" + re.escape(key) + r"(?![a-z])", lowered)
            at = m.start() if m else -1
        else:
            at = lowered.find(key)
        if at < 0:
            continue
        row = COUNTRIES[key]
        if row[0] in taken:
            continue
        taken.add(row[0])
        hits.append((at, row))
        lowered = lowered[:at] + " " * len(key) + lowered[at + len(key):]
    return [row for _at, row in sorted(hits)][:limit]


_YEAR_RE = re.compile(r"\b(20\d\d)\b|(20\d\d)年")


def year_in(query: str, *, default: int) -> int:
    """The four-digit year the question names, else `default`; 明年 and "next
    year" add one, 去年 and "last year" subtract one."""
    m = _YEAR_RE.search(query)
    if m:
        return int(m.group(1) or m.group(2))
    if re.search(r"明年|next year", query, re.I):
        return default + 1
    if re.search(r"去年|last year", query, re.I):
        return default - 1
    return default
