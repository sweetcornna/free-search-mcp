"""Claims routing, the search deadline, and the second batch of record engines
(offline). Fixtures are trimmed copies of real responses captured 2026-09-22.
"""

from __future__ import annotations

import asyncio
from datetime import UTC, datetime

import pytest

from search_mcp import aggregator
from search_mcp.aggregator import _fan_out, _merge, _nominal_pool, claimants
from search_mcp.config import settings
from search_mcp.engines import ENGINES
from search_mcp.engines.appstore import AppStoreEngine, app_name
from search_mcp.engines.base import SearchResult
from search_mcp.engines.cfets import CfetsEngine
from search_mcp.engines.cisakev import CisaKevEngine
from search_mcp.engines.coingecko import CoinGeckoEngine, coins_in
from search_mcp.engines.facts import (
    countries_in,
    ecosystem_hints,
    version_question,
    year_in,
)
from search_mcp.engines.gleif import GleifEngine, entity_name
from search_mcp.engines.holidays import HolidaysEngine
from search_mcp.engines.rdap import RdapEngine, domains_in
from search_mcp.engines.registries import RegistriesEngine
from search_mcp.engines.wdi import WdiEngine, indicators_in
from search_mcp.engines.worldclock import WorldClockEngine, zones_in

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.


# ---------------------------------------------------------------------------
# Claims: who answers what, without a category
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    ("query", "expected"),
    [
        ("latest fastapi version", ["endoflife", "pypi"]),
        ("npm express latest version", ["endoflife", "npm"]),
        ("tokio crate latest version", ["crates"]),
        ("guava maven latest version", ["registries"]),
        ("微信 iOS 版最新版本", ["appstore"]),
        ("astral-sh/uv latest release", ["endoflife", "github_releases", "pypi"]),
        ("CVE-2024-3094", ["nvd", "osv", "cisakev"]),
        ("requests vulnerabilities", ["osv"]),
        ("zyxel exploited in the wild", ["cisakev"]),
        ("上海明天天气", ["openmeteo"]),
        ("Shanghai population", ["wikidata"]),
        ("japan population", ["wikidata", "wdi"]),
        ("example.com domain expiry", ["rdap"]),
        ("Array.prototype.toSorted", ["mdn"]),
        ("RFC 9110", ["ietf"]),
        ("中国 2025 GDP", ["wikidata", "wdi"]),
        ("2026年中国放假安排", ["holidays"]),
        ("现在北京时间几点", ["worldclock"]),
        ("100 usd to cny", ["frankfurter"]),
        ("1万日元等于多少人民币", ["frankfurter", "cfets"]),
        ("人民币中间价", ["cfets"]),
        ("alibaba group legal entity LEI", ["gleif"]),
        ("btc price", ["coingecko"]),
        ("executive order on AI", ["federalregister"]),
        ("UK passport renewal fee", ["govuk"]),
        ("how to cook rice", []),
        ("python asyncio tutorial", []),
        ("what is fastapi", []),
        ("", []),
    ],
)
def test_claimants_by_question_shape(query, expected):
    assert claimants(query) == expected


def test_claimants_are_capped_and_switchable(monkeypatch):
    monkeypatch.setattr(settings, "claim_engine_limit", 1)
    assert claimants("CVE-2024-3094") == ["nvd"]
    monkeypatch.setattr(settings, "auto_route_enabled", False)
    assert claimants("CVE-2024-3094") == []


def test_claimants_join_the_nominal_pool_only_without_a_category():
    pool = _nominal_pool("CVE-2024-3094", None, None)
    assert pool[: len(settings.default_engines)] == list(settings.default_engines)
    assert pool[len(settings.default_engines):] == ["nvd", "osv", "cisakev"]
    assert "cisakev" not in _nominal_pool("CVE-2024-3094", "paper", None)
    assert _nominal_pool("how to cook rice", None, None) == list(settings.default_engines)


def test_a_claim_test_that_raises_does_not_break_the_search(monkeypatch):
    monkeypatch.setattr(
        ENGINES["nvd"], "claims", lambda q: (_ for _ in ()).throw(RuntimeError("boom"))
    )
    assert "nvd" not in claimants("CVE-2024-3094")
    assert "osv" in claimants("CVE-2024-3094")


def test_a_claimed_record_leads_and_keeps_its_title_over_the_page_title():
    page = "https://www.coingecko.com/en/coins/bitcoin"
    buckets = [
        [SearchResult(title="Bitcoin Price: BTC/USD Live", url=page, snippet="", engine=e, rank=1)]
        for e in ("duckduckgo", "bing")
    ]
    buckets.append(
        [SearchResult(title="Bitcoin (BTC): 85,485 USD", url=page, snippet="1 BTC = 85,485 USD",
                      engine="coingecko", rank=1)]
    )
    (top, *_rest) = _merge(buckets, 5, None, "btc price")
    assert top["title"] == "Bitcoin (BTC): 85,485 USD"
    assert top["snippet"] == "1 BTC = 85,485 USD"
    assert sorted(top["engines"]) == ["bing", "coingecko", "duckduckgo"]


# ---------------------------------------------------------------------------
# The fan-out deadline
# ---------------------------------------------------------------------------


def _hit(name: str) -> SearchResult:
    return SearchResult(title=name, url=f"https://{name}.example/", snippet="", engine=name, rank=1)


async def test_slow_engines_are_cancelled_once_someone_answered(monkeypatch):
    monkeypatch.setattr(settings, "search_deadline_seconds", 0.2)
    cancelled: list[str] = []

    async def run(name):
        if name == "slow":
            try:
                await asyncio.sleep(5)
            except asyncio.CancelledError:
                cancelled.append(name)
                raise
        return name, [_hit(name)]

    diagnostics: dict = {}
    results = await _fan_out(run, ["fast", "slow"], diagnostics)
    assert diagnostics["timed_out"] == ["slow"]
    assert cancelled == ["slow"]
    assert [name for name, _ in results] == ["fast", "slow"]
    assert isinstance(results[1][1], TimeoutError)
    assert "0.2s search deadline" in str(results[1][1])


async def test_the_deadline_waits_when_nothing_has_answered_yet(monkeypatch):
    monkeypatch.setattr(settings, "search_deadline_seconds", 0.05)

    async def run(name):
        await asyncio.sleep(0.15)
        return name, [_hit(name)]

    diagnostics: dict = {}
    results = await _fan_out(run, ["a", "b"], diagnostics)
    assert "timed_out" not in diagnostics
    assert all(isinstance(res, list) and res for _, res in results)


async def test_an_empty_answer_does_not_count_as_answered(monkeypatch):
    monkeypatch.setattr(settings, "search_deadline_seconds", 0.05)

    async def run(name):
        if name == "empty":
            return name, []
        await asyncio.sleep(0.15)
        return name, [_hit(name)]

    diagnostics: dict = {}
    results = await _fan_out(run, ["empty", "late"], diagnostics)
    assert "timed_out" not in diagnostics
    assert results[1][1] and results[1][1][0].engine == "late"


async def test_a_zero_deadline_waits_for_everyone(monkeypatch):
    monkeypatch.setattr(settings, "search_deadline_seconds", 0)

    async def run(name):
        await asyncio.sleep(0.05 if name == "slow" else 0)
        return name, [_hit(name)]

    diagnostics: dict = {}
    results = await _fan_out(run, ["fast", "slow"], diagnostics)
    assert "timed_out" not in diagnostics and len(results) == 2


async def test_timed_out_engines_reach_the_payload_and_the_breaker(monkeypatch, tmp_path):
    monkeypatch.setattr(settings, "search_deadline_seconds", 0.1)
    monkeypatch.setattr(settings, "default_engines", ["fast", "slow"])
    monkeypatch.setattr(settings, "cache_dir", tmp_path)

    class Fast:
        name = "fast"
        categories = frozenset()
        single_site = False
        direct_answer = False

        def claims(self, q):
            return False

        def answers_directly(self, q):
            return False

        def is_available(self):
            return True

        async def search(self, query, n, filters, diagnostics=None):
            return [_hit("fast")]

    class Slow(Fast):
        name = "slow"

        async def search(self, query, n, filters, diagnostics=None):
            await asyncio.sleep(5)
            return [_hit("slow")]

    fakes = {"fast": Fast(), "slow": Slow()}
    monkeypatch.setattr(aggregator, "ENGINES", fakes)
    monkeypatch.setattr(aggregator, "get_engine", lambda name: fakes[name])
    payload = await aggregator.aggregate_search("anything", use_cache=False)
    assert payload["timed_out_engines"] == ["slow"]
    assert "0.1s search deadline" in payload["timed_out_hint"]
    assert payload["errors"] == {"slow": "no answer within the 0.1s search deadline"}
    assert [r["engines"] for r in payload["results"]] == [["fast"]]


# ---------------------------------------------------------------------------
# Shared helpers
# ---------------------------------------------------------------------------


def test_question_shapes():
    assert version_question("latest fastapi version")
    assert version_question("python 3.9 还在支持吗")
    assert not version_question("what is fastapi")
    assert ecosystem_hints("go module gin latest") == {"go"}
    assert ecosystem_hints("docker nginx image") == {"docker"}
    assert ecosystem_hints("微信 iOS 版") == {"app"}
    assert countries_in("中国和美国的GDP") == [("CHN", "CN", "China"), ("USA", "US", "United States")]
    assert countries_in("uk vs ukraine")[0][0] == "GBR"
    assert countries_in("south korea") == [("KOR", "KR", "South Korea")]
    assert year_in("明年放假安排", default=2026) == 2027
    assert year_in("2025年假期", default=2026) == 2025
    assert year_in("假期", default=2026) == 2026


# ---------------------------------------------------------------------------
# Registries
# ---------------------------------------------------------------------------


async def test_registries_ask_only_the_registry_the_question_names(monkeypatch):
    engine = RegistriesEngine()
    asked: list[str] = []

    async def fake(url, **kw):
        asked.append(url)
        if "rubygems" in url:
            return {"name": "rails", "version": "8.1.3.1",
                    "version_created_at": "2026-07-29T15:02:41.060Z",
                    "info": "Ruby on Rails is a full-stack web framework.",
                    "homepage_uri": "https://rubyonrails.org"}
        if "proxy.golang.org" in url:
            return {"Version": "v1.12.0", "Time": "2026-02-28T10:10:09Z"}
        return None

    monkeypatch.setattr(engine, "_get_json", fake)
    (r,) = await engine.fetch_results("rails gem latest version", 5, None)
    assert asked == ["https://rubygems.org/api/v1/gems/rails.json"]
    assert r.title == "rails 8.1.3.1 on RubyGems" and r.published_age == "2026-07-29"
    asked.clear()
    (r,) = await engine.fetch_results("github.com/gin-gonic/gin go module latest version", 5, None)
    assert asked == ["https://proxy.golang.org/github.com/gin-gonic/gin/@latest"]
    assert r.url == "https://pkg.go.dev/github.com/gin-gonic/gin@v1.12.0"
    asked.clear()
    assert await engine.fetch_results("latest fastapi version", 5, None) == []
    assert asked == [], "no ecosystem named, nothing asked"


def test_registries_maps_maven_docker_and_packagist():
    engine = RegistriesEngine()
    (r,) = engine.map_results(
        {"response": {"docs": [{"g": "com.google.guava", "a": "guava",
                                "latestVersion": "33.4.8-jre", "timestamp": 1744651522422,
                                "versionCount": 150}]}}
    )
    assert r.title == "com.google.guava:guava 33.4.8-jre on Maven Central"
    assert r.published_age == "2025-04-14"
    assert engine.claims("guava maven latest version")
    assert not engine.claims("guava latest version")
    assert engine.claims("docker nginx image latest")


# ---------------------------------------------------------------------------
# App Store
# ---------------------------------------------------------------------------


def test_app_store_extracts_the_app_name_and_puts_it_first():
    assert app_name("微信 iOS 版最新版本") == "微信"
    assert app_name("latest version of the Notion iOS app") == "Notion"
    engine = AppStoreEngine()
    assert "country=cn" in engine.build_url("微信 iOS 版最新版本", 5)
    assert "country=us" in engine.build_url("Notion iOS app version", 5)
    payload = {
        "_term": "微信",
        "results": [
            {"trackName": "秒剪", "version": "4.0.15", "trackViewUrl": "https://apps.apple.com/cn/app/id1",
             "currentVersionReleaseDate": "2026-09-18T00:00:00Z", "sellerName": "Tencent"},
            {"trackName": "微信", "version": "8.0.78", "trackViewUrl": "https://apps.apple.com/cn/app/id414478124?uo=4",
             "currentVersionReleaseDate": "2026-09-08T00:00:00Z", "sellerName": "Tencent",
             "minimumOsVersion": "15.0", "formattedPrice": "免费", "averageUserRating": 4.1,
             "releaseNotes": "本次更新：\\n- 解决了一些已知问题。"},
        ],
    }
    first, second = engine.map_results(payload)
    assert first.title == "微信 8.0.78 on the App Store"
    assert first.url == "https://apps.apple.com/cn/app/id414478124"
    assert "Version 8.0.78, released 2026-09-08 · by Tencent · requires iOS 15.0 · 免费" in first.snippet
    assert second.title.startswith("秒剪")


# ---------------------------------------------------------------------------
# CISA KEV
# ---------------------------------------------------------------------------

_KEV = {
    "catalogVersion": "2026.09.21",
    "vulnerabilities": [
        {"cveID": "CVE-2026-7273", "vendorProject": "Zyxel", "product": "GS1900 Series Switches",
         "vulnerabilityName": "Zyxel GS1900 Series Switches Stack-Based Buffer Overflow Vulnerability",
         "dateAdded": "2026-09-21", "dueDate": "2026-09-24", "knownRansomwareCampaignUse": "Unknown",
         "requiredAction": "Apply mitigations per vendor instructions.",
         "shortDescription": "Zyxel GS1900 switches contain a stack-based buffer overflow."},
        {"cveID": "CVE-2024-40891", "vendorProject": "Zyxel", "product": "DSL CPE Devices",
         "vulnerabilityName": "Zyxel DSL CPE OS Command Injection Vulnerability",
         "dateAdded": "2025-02-11", "dueDate": "2025-03-04", "knownRansomwareCampaignUse": "Unknown"},
    ],
}


async def test_kev_looks_ids_up_reports_absence_and_searches_words(monkeypatch):
    from search_mcp.engines import cisakev

    monkeypatch.setattr(cisakev, "_cache", {"at": 0.0, "rows": [], "version": ""})
    engine = CisaKevEngine()
    calls: list[str] = []

    async def fake(url, **kw):
        calls.append(url)
        return _KEV

    monkeypatch.setattr(engine, "_get_json", fake)
    (hit,) = await engine.fetch_results("is CVE-2026-7273 exploited", 5, None)
    assert hit.title == "CVE-2026-7273: known exploited in the wild (CISA KEV, added 2026-09-21)"
    assert "US federal remediation due 2026-09-24" in hit.snippet
    (absent,) = await engine.fetch_results("CVE-2024-3094", 5, None)
    assert absent.title == (
        "CVE-2024-3094: not in CISA's Known Exploited Vulnerabilities catalogue (as of 2026-09-21)"
    )
    assert "not that the flaw is harmless" in absent.snippet
    hits = await engine.fetch_results("zyxel exploited in the wild", 5, None)
    assert [h.title.split(":")[0] for h in hits] == ["CVE-2026-7273", "CVE-2024-40891"]
    assert len(calls) == 1, "the catalogue is fetched once and kept"
    assert engine.answers_directly("CVE-2024-3094") and not engine.answers_directly("zyxel exploited")


# ---------------------------------------------------------------------------
# RDAP
# ---------------------------------------------------------------------------


async def test_rdap_bootstraps_to_the_registry_and_reads_the_events(monkeypatch):
    from search_mcp.engines import rdap

    monkeypatch.setattr(rdap, "_bootstrap", {"at": 0.0, "map": {}})
    engine = RdapEngine()
    asked: list[str] = []

    async def fake(url, **kw):
        asked.append(url)
        if url.endswith("dns.json"):
            return {"services": [[["com", "net"], ["https://rdap.verisign.com/com/v1/"]],
                                 [["kg"], ["http://rdap.cctld.kg/"]]]}
        return {
            "ldhName": "GITHUB.COM",
            "status": ["client delete prohibited", "client transfer prohibited"],
            "events": [{"eventAction": "registration", "eventDate": "2007-10-09T18:20:50Z"},
                       {"eventAction": "expiration", "eventDate": "2028-10-09T18:20:50Z"},
                       {"eventAction": "last changed", "eventDate": "2026-09-07T09:01:08Z"}],
            "entities": [{"roles": ["registrar"],
                          "vcardArray": ["vcard", [["version", {}, "text", "4.0"],
                                                   ["fn", {}, "text", "MarkMonitor Inc."]]]}],
            "nameservers": [{"ldhName": "DNS1.P08.NSONE.NET"}, {"ldhName": "NS-1283.AWSDNS-32.ORG"}],
        }

    monkeypatch.setattr(engine, "_get_json", fake)
    assert domains_in("when does www.github.com expire, e.g. soon?") == ["www.github.com"]
    (r,) = await engine.fetch_results("when does www.github.com expire", 5, None)
    assert asked == ["https://data.iana.org/rdap/dns.json",
                     "https://rdap.verisign.com/com/v1/domain/github.com"]
    assert r.title == "github.com: registered 2007-10-09, expires 2028-10-09"
    assert "registrar MarkMonitor Inc." in r.snippet
    assert "name servers dns1.p08.nsone.net, ns-1283.awsdns-32.org" in r.snippet
    assert r.published_age == "2026-09-07"
    assert await engine.fetch_results("example.kg registrar", 5, None) == [], "http-only registry"


# ---------------------------------------------------------------------------
# World Bank indicators
# ---------------------------------------------------------------------------

_WDI = [
    {"page": 1, "pages": 1, "per_page": 50, "total": 3, "sourceid": "2", "lastupdated": "2026-07-13"},
    [
        {"indicator": {"id": "NY.GDP.MKTP.CD", "value": "GDP (current US$)"},
         "country": {"id": "CN", "value": "China"}, "countryiso3code": "CHN",
         "date": "2025", "value": 19498039388042.6},
        {"indicator": {"id": "NY.GDP.MKTP.CD", "value": "GDP (current US$)"},
         "country": {"id": "CN", "value": "China"}, "countryiso3code": "CHN",
         "date": "2024", "value": 18729668435848},
        {"indicator": {"id": "NY.GDP.MKTP.CD", "value": "GDP (current US$)"},
         "country": {"id": "CN", "value": "China"}, "countryiso3code": "CHN",
         "date": "2023", "value": None},
    ],
]


async def test_wdi_reads_indicator_and_country_from_the_question(monkeypatch):
    engine = WdiEngine()
    asked: list[str] = []

    async def fake(url, **kw):
        asked.append(url)
        return _WDI

    monkeypatch.setattr(engine, "_get_json", fake)
    assert indicators_in("中国 2025 GDP") == [("NY.GDP.MKTP.CD", "GDP", "current US$")]
    assert indicators_in("japan population and unemployment") == [
        ("SP.POP.TOTL", "Population", "people"),
        ("SL.UEM.TOTL.ZS", "Unemployment", "% of labour force"),
    ]
    assert indicators_in("gdp per capita of germany")[0][0] == "NY.GDP.PCAP.CD"
    (r,) = await engine.fetch_results("中国 2025 GDP", 5, None)
    assert asked == [
        "https://api.worldbank.org/v2/country/CHN/indicator/NY.GDP.MKTP.CD?format=json&mrv=3"
    ]
    assert r.title == "China GDP (current US$) 2025: 19.5 trillion"
    assert r.snippet.startswith("中国（GDP (current US$)）：2025: 19.5 trillion; 2024: 18.7 trillion")
    assert "data revised 2026-07-13" in r.snippet
    assert r.url == "https://data.worldbank.org/indicator/NY.GDP.MKTP.CD?locations=CN"
    assert r.published_age == "2026-07-13"
    assert await engine.fetch_results("gdp of nowhere", 5, None) == []
    assert await engine.fetch_results("china weather", 5, None) == []


# ---------------------------------------------------------------------------
# Holidays and the clock
# ---------------------------------------------------------------------------


async def test_holidays_china_from_the_state_council_notice(monkeypatch):
    engine = HolidaysEngine()
    asked: list[str] = []

    async def fake(url, **kw):
        asked.append(url)
        return {
            "year": 2026,
            "papers": ["https://www.gov.cn/zhengce/zhengceku/202511/content_7047091.htm"],
            "days": [
                {"name": "元旦", "date": "2026-01-01", "isOffDay": True},
                {"name": "元旦", "date": "2026-01-02", "isOffDay": True},
                {"name": "元旦", "date": "2026-01-03", "isOffDay": True},
                {"name": "元旦", "date": "2026-01-04", "isOffDay": False},
                {"name": "春节", "date": "2026-02-14", "isOffDay": False},
                {"name": "春节", "date": "2026-02-15", "isOffDay": True},
                {"name": "春节", "date": "2026-02-23", "isOffDay": True},
            ],
        }

    monkeypatch.setattr(engine, "_get_json", fake)
    (r,) = await engine.fetch_results("2026年放假安排", 5, None)
    assert asked == ["https://raw.githubusercontent.com/NateScarlet/holiday-cn/master/2026.json"]
    assert r.title == "2026年中国法定节假日安排（国务院通知）"
    assert r.url == "https://www.gov.cn/zhengce/zhengceku/202511/content_7047091.htm"
    assert r.snippet == "元旦 01-01至01-03（调休上班 01-04）；春节 02-15至02-23（调休上班 02-14）"
    assert engine.claims("2026年放假安排") and not engine.claims("holidays")


async def test_holidays_elsewhere_from_nager(monkeypatch):
    engine = HolidaysEngine()
    asked: list[str] = []

    async def fake(url, **kw):
        asked.append(url)
        return [
            {"date": "2026-01-01", "name": "New Year's Day", "countryCode": "DE", "nationalHoliday": True},
            {"date": "2026-01-06", "name": "Epiphany", "countryCode": "DE", "nationalHoliday": False},
        ]

    monkeypatch.setattr(engine, "_get_json", fake)
    (r,) = await engine.fetch_results("germany public holidays next year", 5, None)
    year = datetime.now(UTC).year + 1
    assert asked == [f"https://nagerholidays.com/api/v4/Holidays/DE/{year}"]
    assert r.title == f"Germany public holidays {year}"
    assert r.snippet == "01-01 New Year's Day; 01-06 Epiphany (regional)"
    assert r.url == f"https://date.nager.at/PublicHoliday/Country/DE/{year}"


def test_worldclock_answers_from_the_zone_database_without_a_request():
    engine = WorldClockEngine()
    assert zones_in("what time is it in Tokyo and New York") == [
        ("Asia/Tokyo", "Tokyo"), ("America/New_York", "New York")
    ]
    assert zones_in("现在北京时间几点") == [("Asia/Shanghai", "Beijing")]
    assert zones_in("time in Europe/Berlin") == [("Europe/Berlin", "Berlin")]
    now = datetime(2026, 9, 22, 3, 33, 33, tzinfo=UTC)
    tokyo, new_york = engine.map_results(
        {"now": now, "zones": zones_in("Tokyo and New York"), "zh": False}
    )
    assert tokyo.title == "Current time in Tokyo: 2026-09-22 12:33 (Asia/Tokyo)"
    assert "UTC+09:00, no daylight saving" in tokyo.snippet
    assert new_york.title == "Current time in New York: 2026-09-21 23:33 (America/New_York)"
    assert "UTC-04:00, daylight saving in force" in new_york.snippet
    assert new_york.published_age == "2026-09-21"
    (beijing,) = engine.map_results({"now": now, "zones": zones_in("北京"), "zh": True})
    assert beijing.title == "北京现在时间：2026-09-22 11:33（Asia/Shanghai）"
    assert "星期二" in beijing.snippet


# ---------------------------------------------------------------------------
# CFETS, GLEIF, CoinGecko
# ---------------------------------------------------------------------------

_CFETS = {
    "data": {"lastDate": "2026-09-22 9:15"},
    "records": [
        {"vrtEName": "USD/CNY", "foreignCName": "USD", "price": "6.7459"},
        {"vrtEName": "100JPY/CNY", "foreignCName": "JPY", "price": "4.2794"},
        {"vrtEName": "CNY/MOP", "foreignCName": "MOP", "price": "1.1913"},
    ],
}


async def test_cfets_reads_the_central_parity_and_per_hundred_pairs(monkeypatch):
    engine = CfetsEngine()

    async def fake(url, **kw):
        return dict(_CFETS)

    monkeypatch.setattr(engine, "_get_json", fake)
    (r,) = await engine.fetch_results("人民币中间价", 5, None)
    assert r.title == "USD/CNY 中间价 6.7459（2026-09-22）"
    assert r.published_age == "2026-09-22" and r.published_age_confident
    (r,) = await engine.fetch_results("1万日元等于多少人民币", 5, None)
    assert r.snippet.startswith("10000 JPY = 427.9400 CNY · 人民币中间价 100JPY/CNY 4.2794（2026-09-22 9:15）")
    (r,) = await engine.fetch_results("100 cny to mop", 5, None)
    assert r.snippet.startswith("100 CNY = 119.1300 MOP")
    assert engine.claims("1万日元等于多少人民币") and not engine.claims("100 usd to cny")


def test_gleif_uses_the_legal_name_filter_for_cjk_and_maps_records():
    engine = GleifEngine()
    assert entity_name("alibaba group legal entity LEI") == "alibaba group"
    assert "filter%5Bfulltext%5D=alibaba+group" in engine.build_url("alibaba group legal entity LEI", 3)
    assert "filter%5Bentity.legalName%5D=%E9%98%BF%E9%87%8C%E5%B7%B4%E5%B7%B4" in engine.build_url(
        "阿里巴巴 法人", 3
    )
    payload = {
        "meta": {"goldenCopy": {"publishDate": "2026-09-21T16:00:00Z"}},
        "data": [
            {"attributes": {"lei": "254900KD4P900ITKSU65", "entity": {
                "legalName": {"name": "Alibaba Group Services Limited"},
                "otherNames": [{"name": "Alibaba.com Group Services Limited"}],
                "legalAddress": {"city": "Hong Kong", "country": "HK"},
                "jurisdiction": "HK", "status": "ACTIVE", "registeredAs": "37473977",
                "creationDate": "2006-12-19T00:00:00Z"}}}
        ],
    }
    (r,) = engine.map_results(payload)
    assert r.title == "Alibaba Group Services Limited (254900KD4P900ITKSU65)"
    assert r.url == "https://search.gleif.org/#/record/254900KD4P900ITKSU65"
    assert r.snippet.startswith(
        "LEI 254900KD4P900ITKSU65 · active · jurisdiction HK · legal address Hong Kong, HK"
    )
    assert r.published_age == "2026-09-21"


async def test_coingecko_maps_symbols_and_searches_unknown_coins(monkeypatch):
    engine = CoinGeckoEngine()
    assert coins_in("btc and 以太坊 price") == ["bitcoin", "ethereum"]
    asked: list[str] = []

    async def fake(url, **kw):
        asked.append(url)
        if "/search?" in url:
            return {"coins": [{"id": "worldcoin-wld", "name": "Worldcoin", "symbol": "WLD"}]}
        return {"worldcoin-wld": {"usd": 0.92, "cny": 6.16, "usd_24h_change": -0.47,
                                  "usd_market_cap": 1_600_000_000, "last_updated_at": 1790048000}}

    monkeypatch.setattr(engine, "_get_json", fake)
    (r,) = await engine.fetch_results("worldcoin token price", 5, None)
    assert asked[0] == "https://api.coingecko.com/api/v3/search?query=worldcoin"
    assert "ids=worldcoin-wld&vs_currencies=usd,cny" in asked[1]
    assert r.title == "Worldcoin (WLD): 0.92 USD (2026-09-22 03:33 UTC)"
    assert "1 WLD = 0.92 USD = 6.16 CNY · 24h -0.47% · market cap 1.60 billion USD" in r.snippet
    assert r.published_age == "2026-09-22"
    (r,) = engine.map_results({"bitcoin": {"usd": 85485, "cny": 572421}})
    assert r.title == "Bitcoin (BTC): 85,485 USD"
