"""Direct-fact sources (offline): registries and reference APIs that answer
with the record itself.

Fixtures are trimmed copies of real responses captured 2026-09-21. Tests call
the pure mappers, the query parsers and `fetch_results` with `_get_json`
stubbed, so nothing here touches the network.
"""

from __future__ import annotations

import pytest

from search_mcp.aggregator import _merge, engines_for_category
from search_mcp.engines import ENGINES, get_engine
from search_mcp.engines.base import SearchResult
from search_mcp.engines.endoflife import EndOfLifeEngine, _index
from search_mcp.engines.facts import candidate_names, compact_number, iso_day
from search_mcp.engines.frankfurter import FrankfurterEngine, parse_pair
from search_mcp.engines.github_releases import GitHubReleasesEngine, _names_repo
from search_mcp.engines.ietf import IetfEngine, _title_terms
from search_mcp.engines.npm import NpmEngine
from search_mcp.engines.nvd import NvdEngine
from search_mcp.engines.openmeteo import OpenMeteoEngine, place_from
from search_mcp.engines.osv import OsvEngine, _canonical_id, ecosystems_for
from search_mcp.engines.pypi import PyPIEngine
from search_mcp.engines.wikidata import WikidataEngine, entity_terms

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.

FACT_ENGINES = {
    "endoflife": "software",
    "github_releases": "software",
    "pypi": "software",
    "npm": "software",
    "crates": "software",
    "registries": "software",
    "appstore": "software",
    "nvd": "security",
    "osv": "security",
    "cisakev": "security",
    "wikidata": "reference",
    "rdap": "reference",
    "openmeteo": "weather",
    "mdn": "docs",
    "ietf": "docs",
    "wdi": "stats",
    "holidays": "calendar",
    "worldclock": "calendar",
    "frankfurter": "finance",
    "cfets": "finance",
    "gleif": "finance",
    "coingecko": "finance",
    "federalregister": "gov",
    "govuk": "gov",
}


# ---------------------------------------------------------------------------
# Registry contract
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(("name", "group"), sorted(FACT_ENGINES.items()))
def test_registered_under_its_group_and_out_of_the_default_pool(name, group):
    from search_mcp.config import Settings

    engine = get_engine(name)
    assert engine.name == name
    assert group in engine.categories
    assert engine.single_site, "a record lookup is not a web index"
    settings = Settings()
    assert name not in settings.default_engines
    assert name not in settings.reserve_engines
    assert name not in settings.rescue_engines


def test_bare_software_seats_lifecycle_github_and_python():
    """Registry order decides who wins the three seats; npm and crates are
    reached through their sub-groups."""
    assert engines_for_category("software") == ["endoflife", "github_releases", "pypi"]
    assert engines_for_category("software.node") == ["npm"]
    assert engines_for_category("software.rust") == ["crates"]


def test_the_small_groups_run_every_member():
    assert engines_for_category("security") == ["nvd", "osv", "cisakev"]
    assert engines_for_category("docs") == ["mdn", "ietf"]
    assert engines_for_category("gov") == ["federalregister", "govuk"]
    assert engines_for_category("reference") == ["wikidata", "rdap"]
    assert engines_for_category("calendar") == ["holidays", "worldclock"]
    assert engines_for_category("stats") == ["wdi"]


def test_fx_is_reached_by_its_sub_group_and_leaves_bare_finance_alone():
    assert engines_for_category("finance.fx") == ["frankfurter", "cfets"]
    assert engines_for_category("finance.entity") == ["gleif"]
    assert engines_for_category("finance.crypto") == ["coingecko"]
    assert engines_for_category("finance") == ["sec_edgar", "yahoofinance", "worldbank"]


# ---------------------------------------------------------------------------
# Shared query helpers
# ---------------------------------------------------------------------------


def test_candidate_names_drop_question_words_and_keep_scoped_names():
    assert candidate_names("latest fastapi version") == ["fastapi"]
    assert candidate_names("requests 库最新版本") == ["requests"]
    assert candidate_names("@types/node and astral-sh/uv release") == [
        "@types/node",
        "astral-sh/uv",
    ]
    assert candidate_names("is python 3.9 still supported") == []
    assert candidate_names("requests 2.32 changelog") == ["requests"]
    assert candidate_names("what is the latest version") == []


def test_iso_day_and_compact_number():
    assert iso_day("2026-07-29T14:02:11.123456Z") == "2026-07-29"
    assert iso_day(1_700_000_000) == "2023-11-14"
    assert iso_day(None) == ""
    assert compact_number("24870895") == "24,870,895"
    assert compact_number("6341") == "6,341"


# ---------------------------------------------------------------------------
# PyPI
# ---------------------------------------------------------------------------

_PYPI = {
    "info": {
        "name": "fastapi",
        "version": "0.141.1",
        "summary": "FastAPI framework, high performance, easy to learn",
        "requires_python": ">=3.10",
        "project_urls": {"Homepage": "https://github.com/fastapi/fastapi"},
    },
    "urls": [{"upload_time_iso_8601": "2026-07-29T10:11:12.000000Z"}],
}


def test_pypi_maps_the_current_release_with_its_upload_date():
    (r,) = PyPIEngine().map_results(_PYPI)
    assert r.title == "fastapi 0.141.1 on PyPI"
    assert r.url == "https://pypi.org/project/fastapi/0.141.1/"
    assert "Latest release 0.141.1, uploaded 2026-07-29" in r.snippet
    assert "requires Python >=3.10" in r.snippet
    assert r.published_age == "2026-07-29" and r.published_age_confident


async def test_pypi_asks_for_each_candidate_name_and_skips_404s(monkeypatch):
    engine = PyPIEngine()
    asked: list[str] = []

    async def fake(url, **kw):
        asked.append(url)
        return _PYPI if "/fastapi/" in url else None

    monkeypatch.setattr(engine, "_get_json", fake)
    out = await engine.fetch_results("fastapi vs starlette latest version", 5, None)
    assert asked == [
        "https://pypi.org/pypi/fastapi/json",
        "https://pypi.org/pypi/vs/json",
        "https://pypi.org/pypi/starlette/json",
    ]
    assert [r.title for r in out] == ["fastapi 0.141.1 on PyPI"]


def test_pypi_rejects_payloads_without_a_version():
    assert PyPIEngine().map_results({"info": {"name": "x"}}) == []
    assert PyPIEngine().map_results({"message": "Not Found"}) == []
    assert PyPIEngine().map_results(None) == []


# ---------------------------------------------------------------------------
# npm and crates.io
# ---------------------------------------------------------------------------


def test_npm_searches_for_the_package_name_not_the_question():
    url = NpmEngine().build_url("latest express version", 5)
    assert "text=express&" in url and "latest" not in url


def test_npm_maps_search_objects():
    payload = {
        "objects": [
            {
                "package": {
                    "name": "express",
                    "version": "5.2.1",
                    "date": "2025-12-01T20:00:00.000Z",
                    "description": "Fast, unopinionated, minimalist web framework",
                    "links": {"homepage": "https://expressjs.com/"},
                }
            },
            {"package": {"name": "broken"}},
        ]
    }
    (r,) = NpmEngine().map_results(payload)
    assert r.url == "https://www.npmjs.com/package/express/v/5.2.1"
    assert "Version 5.2.1, published 2025-12-01" in r.snippet
    assert r.published_age == "2025-12-01"


def test_crates_uses_the_stable_version_and_an_honest_user_agent():
    engine = get_engine("crates")
    assert engine.impersonate is None
    assert "User-Agent" in engine.api_headers
    payload = {
        "crates": [
            {
                "name": "tokio",
                "max_stable_version": "1.53.1",
                "newest_version": "1.54.0-alpha.1",
                "updated_at": "2026-07-20T09:00:00.000000+00:00",
                "description": "An event-driven, non-blocking I/O platform",
                "repository": "https://github.com/tokio-rs/tokio",
            }
        ]
    }
    (r,) = engine.map_results(payload)
    assert r.title == "tokio 1.53.1 on crates.io"
    assert r.url == "https://crates.io/crates/tokio/1.53.1"
    assert r.published_age == "2026-07-20"


# ---------------------------------------------------------------------------
# endoflife.date
# ---------------------------------------------------------------------------

_EOL_PRODUCTS = {
    "result": [
        {"name": "python", "label": "Python", "aliases": []},
        {"name": "ubuntu", "label": "Ubuntu", "aliases": ["ubuntu-linux"]},
        {"name": "redhat", "label": "Red Hat Enterprise Linux", "aliases": ["rhel", "red hat"]},
        {"name": "go", "label": "Go", "aliases": ["golang"]},
    ]
}
_EOL_PYTHON = {
    "result": {
        "name": "python",
        "label": "Python",
        "releases": [
            {
                "name": "3.14",
                "isEol": False,
                "eolFrom": "2030-10-31",
                "latest": {"name": "3.14.7", "date": "2026-08-05"},
            },
            {"name": "3.13", "isEol": False, "eolFrom": "2029-10-31"},
            {"name": "3.12", "isEol": False, "eolFrom": "2028-10-31"},
            {"name": "3.11", "isEol": False, "eolFrom": "2027-10-31"},
            {"name": "3.10", "isEol": False, "eolFrom": "2026-10-31"},
            {"name": "3.9", "isEol": True, "eolFrom": "2025-10-31"},
        ],
    }
}


def test_endoflife_index_maps_every_spelling_to_the_api_name():
    index = _index(_EOL_PRODUCTS["result"])
    assert index["python"] == "python"
    assert index["rhel"] == "redhat"
    assert index["redhat"] == "redhat"
    assert index["golang"] == "go"


def test_endoflife_matches_products_named_in_the_question_including_go_and_python():
    """The catalogue is its own filter: "python" and "go" are products here,
    not stop words."""
    index = _index(_EOL_PRODUCTS["result"])
    engine = EndOfLifeEngine()
    assert engine._matches("is python 3.9 still supported", index) == ["python"]
    assert engine._matches("red hat 9 end of life", index) == ["redhat"]
    assert engine._matches("when does go 1.22 stop getting fixes", index) == ["go"]
    assert engine._matches("latest kubernetes release", index) == []


async def test_endoflife_puts_the_asked_cycle_first_and_dates_the_result(monkeypatch):
    engine = EndOfLifeEngine()
    from search_mcp.engines import endoflife

    monkeypatch.setattr(endoflife, "_catalogue", {"at": 0.0, "index": {}})

    async def fake(url, **kw):
        return _EOL_PYTHON if url.endswith("/python") else _EOL_PRODUCTS

    monkeypatch.setattr(engine, "_get_json", fake)
    (r,) = await engine.fetch_results("is python 3.9 still supported", 5, None)
    assert r.url == "https://endoflife.date/python"
    assert r.snippet.startswith("Latest release 3.14.7 on 2026-08-05 (cycle 3.14)")
    assert "Cycles: 3.9: end of life since 2025-10-31; 3.14: supported until 2030-10-31" in r.snippet
    assert r.published_age == "2026-08-05" and r.published_age_confident


# ---------------------------------------------------------------------------
# GitHub releases
# ---------------------------------------------------------------------------

_RELEASE = {
    "tag_name": "0.12.17",
    "name": "0.12.17",
    "html_url": "https://github.com/astral-sh/uv/releases/tag/0.12.17",
    "published_at": "2026-09-18T18:00:00Z",
    "prerelease": False,
    "body": "## Release Notes\n- Reject unsupported Git archive paths (#21780)\n"
    "d2b135cfb7a3582b9eb515756b9166bcb9521f4a",
}


def test_names_repo_requires_the_query_to_name_the_repository():
    assert _names_repo({"name": "zod"}, ["zod"])
    assert _names_repo({"name": "vscode"}, ["vs", "code"])
    assert _names_repo({"name": "scikit-learn"}, ["scikit_learn"])
    assert not _names_repo({"name": "Transcrypt"}, ["3.9"])
    assert not _names_repo({"name": "awesome-zod"}, ["zod"])


async def test_github_releases_looks_an_owner_repo_up_directly(monkeypatch):
    engine = GitHubReleasesEngine()
    asked: list[str] = []

    async def fake(url, **kw):
        asked.append(url)
        return _RELEASE

    monkeypatch.setattr(engine, "_get_json", fake)
    (r,) = await engine.fetch_results("latest release of astral-sh/uv", 5, None)
    assert asked == ["https://api.github.com/repos/astral-sh/uv/releases/latest"]
    assert r.title == "astral-sh/uv 0.12.17: latest release on GitHub"
    assert r.published_age == "2026-09-18"
    # Markdown and commit hashes are stripped from the notes.
    assert "##" not in r.snippet and "d2b135cf" not in r.snippet
    assert "Reject unsupported Git archive paths" in r.snippet


async def test_github_releases_searches_then_rejects_repositories_the_query_did_not_name(
    monkeypatch,
):
    engine = GitHubReleasesEngine()
    asked: list[str] = []

    async def fake(url, **kw):
        asked.append(url)
        if "/search/repositories" in url:
            return {
                "items": [
                    {"full_name": "TranscryptOrg/Transcrypt", "name": "Transcrypt"},
                    {"full_name": "colinhacks/zod", "name": "zod"},
                ]
            }
        return _RELEASE

    monkeypatch.setattr(engine, "_get_json", fake)
    out = await engine.fetch_results("zod latest release", 5, None)
    assert asked[1:] == ["https://api.github.com/repos/colinhacks/zod/releases/latest"]
    assert len(out) == 1
    # A version number alone names no repository, so nothing is searched.
    asked.clear()
    assert await engine.fetch_results("is python 3.9 still supported", 5, None) == []
    assert asked == []


# ---------------------------------------------------------------------------
# NVD and OSV
# ---------------------------------------------------------------------------

_NVD = {
    "vulnerabilities": [
        {
            "cve": {
                "id": "CVE-2024-3094",
                "published": "2024-03-29T17:15:21.150",
                "vulnStatus": "Modified",
                "descriptions": [
                    {"lang": "es", "value": "Se descubrió código malicioso"},
                    {"lang": "en", "value": "Malicious code was discovered in xz 5.6.0."},
                ],
                "metrics": {
                    "cvssMetricV31": [
                        {"cvssData": {"version": "3.1", "baseScore": 10.0, "baseSeverity": "CRITICAL"}}
                    ]
                },
            }
        }
    ]
}


def test_nvd_maps_a_cve_with_its_score_and_english_description():
    (r,) = NvdEngine().map_results(_NVD)
    assert r.title == "CVE-2024-3094 (CVSS 10.0 CRITICAL)"
    assert r.url == "https://nvd.nist.gov/vuln/detail/CVE-2024-3094"
    assert "published 2024-03-29" in r.snippet and "xz 5.6.0" in r.snippet
    assert "Se descubrió" not in r.snippet


def test_nvd_answers_directly_only_for_a_cve_id():
    engine = NvdEngine()
    assert engine.answers_directly("CVE-2024-3094 details")
    assert not engine.answers_directly("openssl vulnerability")
    assert "cveId=CVE-2024-3094" in engine.build_url("what is CVE-2024-3094", 5)
    assert "keywordSearch=openssl" in engine.build_url("openssl", 5)


def test_osv_guesses_the_ecosystem_and_keeps_ghsa_ids_case_sensitive():
    assert ecosystems_for("requests pip vulnerabilities") == ("pypi",)
    assert ecosystems_for("lodash npm") == ("npm",)
    assert ecosystems_for("tokio") == ("pypi", "npm")
    assert _canonical_id("ghsa-9wx4-h78v-vm56") == "GHSA-9wx4-h78v-vm56"
    assert _canonical_id("cve-2024-3094") == "CVE-2024-3094"


_OSV_VULN = {
    "id": "PYSEC-2026-1872",
    "summary": "Requests vulnerable to .netrc credentials leak via malicious URLs",
    "aliases": ["CVE-2024-47081", "GHSA-9hjg-9r4m-mvj7"],
    "published": "2026-07-07T00:00:00Z",
    "affected": [
        {
            "package": {"ecosystem": "PyPI", "name": "requests", "purl": "pkg:pypi/requests"},
            "ranges": [{"type": "ECOSYSTEM", "events": [{"introduced": "0"}, {"fixed": "2.32.4"}]}],
        }
    ],
}


async def test_osv_posts_a_purl_per_ecosystem_and_leads_with_the_newest_advisory(monkeypatch):
    engine = OsvEngine()
    posted: list[str] = []

    async def fake(url, method="GET", json_body=None, **kw):
        posted.append(json_body["package"]["purl"])
        if json_body["package"]["purl"] != "pkg:pypi/requests":
            return {}
        older = dict(_OSV_VULN, id="PYSEC-2020-1", published="2020-01-01T00:00:00Z")
        return {"vulns": [older, _OSV_VULN]}

    monkeypatch.setattr(engine, "_get_json", fake)
    out = await engine.fetch_results("requests vulnerabilities", 5, None)
    assert posted == ["pkg:pypi/requests", "pkg:npm/requests"]
    assert [r.title.split(":")[0] for r in out] == ["PYSEC-2026-1872", "PYSEC-2020-1"]
    top = out[0]
    assert top.url == "https://osv.dev/vulnerability/PYSEC-2026-1872"
    assert "PyPI requests fixed in 2.32.4" in top.snippet
    assert "also CVE-2024-47081, GHSA-9hjg-9r4m-mvj7" in top.snippet
    assert top.published_age == "2026-07-07" and top.rank == 0


async def test_osv_looks_an_advisory_id_up_directly(monkeypatch):
    engine = OsvEngine()
    asked: list[str] = []

    async def fake(url, **kw):
        asked.append(url)
        return _OSV_VULN

    monkeypatch.setattr(engine, "_get_json", fake)
    (r,) = await engine.fetch_results("GHSA-9hjg-9r4m-mvj7", 5, None)
    assert asked == ["https://api.osv.dev/v1/vulns/GHSA-9hjg-9r4m-mvj7"]
    assert r.title.startswith("PYSEC-2026-1872")


# ---------------------------------------------------------------------------
# Wikidata
# ---------------------------------------------------------------------------

_ENTITY = {
    "entities": {
        "Q8686": {
            "labels": {"en": {"value": "Shanghai"}, "zh": {"value": "上海市"}},
            "descriptions": {"en": {"value": "provincial-level municipality in China"}},
            "sitelinks": {
                "enwiki": {"url": "https://en.wikipedia.org/wiki/Shanghai"},
                "zhwiki": {"url": "https://zh.wikipedia.org/wiki/上海市"},
            },
            "claims": {
                "P1082": [
                    {
                        "mainsnak": {
                            "datavalue": {
                                "type": "quantity",
                                "value": {"amount": "+24870895", "unit": "1"},
                            }
                        },
                        "qualifiers": {
                            "P585": [
                                {
                                    "datavalue": {
                                        "value": {"time": "+2020-11-01T00:00:00Z", "precision": 9}
                                    }
                                }
                            ]
                        },
                    },
                    {
                        "mainsnak": {
                            "datavalue": {
                                "type": "quantity",
                                "value": {"amount": "+23019148", "unit": "1"},
                            }
                        },
                        "qualifiers": {
                            "P585": [
                                {
                                    "datavalue": {
                                        "value": {"time": "+2010-11-01T00:00:00Z", "precision": 9}
                                    }
                                }
                            ]
                        },
                    },
                ],
                "P2046": [
                    {
                        "mainsnak": {
                            "datavalue": {
                                "type": "quantity",
                                "value": {
                                    "amount": "+6341",
                                    "unit": "http://www.wikidata.org/entity/Q712226",
                                },
                            }
                        }
                    }
                ],
                "P571": [
                    {
                        "mainsnak": {
                            "datavalue": {
                                "type": "time",
                                "value": {"time": "+0751-00-00T00:00:00Z", "precision": 9},
                            }
                        }
                    }
                ],
                "P856": [
                    {
                        "mainsnak": {
                            "datavalue": {"type": "string", "value": "https://shanghai.gov.cn"}
                        }
                    }
                ],
            },
        }
    }
}


def test_entity_terms_strip_the_fact_asked_about():
    assert entity_terms("Shanghai population") == "Shanghai"
    assert entity_terms("上海 人口") == "上海"
    assert entity_terms("when was the Eiffel Tower built") == "Eiffel Tower built"
    assert entity_terms("population") == ""


async def test_wikidata_reports_the_newest_dated_fact_and_links_wikipedia(monkeypatch):
    engine = WikidataEngine()
    asked: list[str] = []

    async def fake(url, **kw):
        asked.append(url)
        if "wbsearchentities" in url:
            return {"search": [{"id": "Q8686"}]}
        return _ENTITY

    monkeypatch.setattr(engine, "_get_json", fake)
    (r,) = await engine.fetch_results("Shanghai population", 5, None)
    assert "search=Shanghai&language=en" in asked[0]
    assert "ids=Q8686" in asked[1]
    assert r.title == "Shanghai"
    assert r.url == "https://en.wikipedia.org/wiki/Shanghai"
    assert "population 24,870,895 (as of 2020)" in r.snippet
    assert "23,019,148" not in r.snippet
    assert "area 6,341 km²" in r.snippet
    assert "inception 751" in r.snippet
    assert "official website https://shanghai.gov.cn" in r.snippet
    assert r.snippet.endswith("Wikidata Q8686")


async def test_wikidata_prefers_the_chinese_article_for_a_chinese_question(monkeypatch):
    engine = WikidataEngine()

    async def fake(url, **kw):
        if "wbsearchentities" in url:
            assert "language=zh" in url
            return {"search": [{"id": "Q8686"}]}
        return _ENTITY

    monkeypatch.setattr(engine, "_get_json", fake)
    (r,) = await engine.fetch_results("上海 人口", 5, None)
    assert r.title == "上海市"
    assert r.url == "https://zh.wikipedia.org/wiki/上海市"


def test_wikidata_falls_back_to_the_item_page_without_a_sitelink():
    engine = WikidataEngine()
    payload = {"entities": {"Q1": {"labels": {"en": {"value": "Thing"}}}}}
    (r,) = engine.map_results(payload)
    assert r.url == "https://www.wikidata.org/wiki/Q1"
    assert engine.map_results({"entities": {"Q1": {"missing": ""}}}) == []


# ---------------------------------------------------------------------------
# Open-Meteo
# ---------------------------------------------------------------------------

_FORECAST = {
    "timezone": "Asia/Shanghai",
    "current": {
        "time": "2026-09-22T10:15",
        "temperature_2m": 27.6,
        "relative_humidity_2m": 57,
        "weather_code": 2,
        "wind_speed_10m": 5.8,
    },
    "daily": {
        "time": ["2026-09-22", "2026-09-23"],
        "weather_code": [3, 51],
        "temperature_2m_max": [29.6, 29.3],
        "temperature_2m_min": [21.9, 22.1],
        "precipitation_probability_max": [6, 30],
        "precipitation_sum": [0.0, 0.3],
    },
}


def test_place_from_strips_the_weather_words_in_both_languages():
    assert place_from("上海明天天气") == "上海"
    assert place_from("weather in Berlin tomorrow") == "Berlin"
    assert place_from("what is the weather like in New York this week?") == "New York"
    assert place_from("天气预报") == ""


async def test_openmeteo_geocodes_then_reports_numbers_with_their_issue_time(monkeypatch):
    engine = OpenMeteoEngine()
    asked: list[str] = []

    async def fake(url, **kw):
        asked.append(url)
        if "geocoding-api" in url:
            return {
                "results": [
                    {"name": "上海", "admin1": "上海市", "country": "中国",
                     "latitude": 31.22222, "longitude": 121.45806}
                ]
            }
        return _FORECAST

    monkeypatch.setattr(engine, "_get_json", fake)
    (r,) = await engine.fetch_results("上海明天天气", 5, None)
    assert "name=%E4%B8%8A%E6%B5%B7&count=1&language=zh" in asked[0]
    assert "latitude=31.2222&longitude=121.4581" in asked[1]
    assert r.title == "上海, 上海市, 中国 天气预报（Open-Meteo）"
    assert r.snippet.startswith("现在 27.6°C，局部多云，湿度 57%，风速 5.8 km/h（2026-09-22T10:15 Asia/Shanghai）")
    assert "2026-09-23：小毛毛雨，22.1到29.3°C，降水概率 30%，降水量 0.3 mm" in r.snippet
    assert r.url == asked[1]
    assert r.published_age == "2026-09-22" and r.published_age_confident


async def test_openmeteo_gives_up_quietly_when_the_place_is_unknown(monkeypatch):
    engine = OpenMeteoEngine()

    async def fake(url, **kw):
        return {"generationtime_ms": 0.5}

    monkeypatch.setattr(engine, "_get_json", fake)
    assert await engine.fetch_results("weather in Nowhereville", 5, None) == []
    assert await engine.fetch_results("weather forecast", 5, None) == []


def test_openmeteo_english_snippet():
    payload = dict(_FORECAST, _place={"name": "Berlin", "country": "Germany"}, _zh=False)
    (r,) = OpenMeteoEngine().map_results(payload)
    assert r.title == "Weather forecast for Berlin, Germany (Open-Meteo)"
    assert "Now 27.6°C, partly cloudy, humidity 57%" in r.snippet
    assert "2026-09-22: overcast, 21.9 to 29.6°C, 6% chance of precipitation, 0.0 mm" in r.snippet


# ---------------------------------------------------------------------------
# MDN and IETF
# ---------------------------------------------------------------------------


def test_mdn_picks_the_locale_from_the_query_and_maps_documents():
    engine = get_engine("mdn")
    assert "locale=en-US" in engine.build_url("Array.prototype.toSorted", 5)
    assert "locale=zh-CN" in engine.build_url("css 容器查询", 5)
    payload = {
        "documents": [
            {
                "title": "Array.prototype.toSorted()",
                "mdn_url": "/en-US/docs/Web/JavaScript/Reference/Global_Objects/Array/toSorted",
                "summary": "The toSorted() method is the copying version of sort().",
            },
            {"title": "no url"},
        ]
    }
    (r,) = engine.map_results(payload)
    assert r.url.startswith("https://developer.mozilla.org/en-US/docs/Web/JavaScript/")
    assert r.snippet.startswith("The toSorted() method")


def test_ietf_looks_numbers_up_and_searches_titles_without_the_word_rfc():
    engine = IetfEngine()
    assert "name__in=rfc9110,rfc9111" in engine.build_url("RFC 9110 and rfc-9111", 5)
    assert "title__icontains=http+semantics&type=rfc" in engine.build_url("http semantics rfc", 5)
    assert _title_terms("the QUIC transport protocol standard") == "QUIC transport protocol"
    assert engine.answers_directly("rfc 9110")
    assert not engine.answers_directly("http semantics")


def test_ietf_maps_a_document_with_its_standards_level_and_no_false_date():
    payload = {
        "objects": [
            {
                "name": "rfc9110",
                "rfc": "9110",
                "title": "HTTP Semantics",
                "std_level": "/api/v1/name/stdlevelname/std/",
                "tags": ["/api/v1/name/doctagname/errata/", "/api/v1/name/doctagname/verified-errata/"],
                "time": "2026-05-20T15:43:39Z",
                "abstract": "The Hypertext Transfer Protocol (HTTP) is a stateless protocol.",
            },
            {"name": "draft-ietf-httpbis-semantics", "title": "HTTP Semantics"},
        ]
    }
    (r,) = IetfEngine().map_results(payload)
    assert r.title == "RFC 9110: HTTP Semantics"
    assert r.url == "https://www.rfc-editor.org/rfc/rfc9110.html"
    assert r.snippet.startswith("RFC 9110 · Internet Standard · has verified errata")
    assert not r.published_age, "the record's time is its last edit, not publication"


# ---------------------------------------------------------------------------
# Frankfurter
# ---------------------------------------------------------------------------


def test_parse_pair_reads_codes_names_and_amounts_in_question_order():
    assert parse_pair("100 usd to cny") == ("USD", "CNY", 100.0)
    assert parse_pair("1万日元等于多少人民币") == ("JPY", "CNY", 10000.0)
    assert parse_pair("euro to hong kong dollar") == ("EUR", "HKD", 1.0)
    assert parse_pair("2.5k GBP in USD") == ("GBP", "USD", 2500.0)
    assert parse_pair("usd exchange rate") is None
    assert parse_pair("新台币 兑 美元") == ("TWD", "USD", 1.0), "served by the fallback source"
    assert parse_pair("the cat sat") is None, "three-letter words are not currencies"


async def test_frankfurter_converts_and_labels_the_rate_as_a_daily_reference(monkeypatch):
    engine = FrankfurterEngine()
    asked: list[str] = []

    async def fake(url, **kw):
        asked.append(url)
        return {"amount": 1.0, "base": "USD", "date": "2026-09-21", "rates": {"CNY": 6.6954}}

    monkeypatch.setattr(engine, "_get_json", fake)
    (r,) = await engine.fetch_results("100 usd to cny", 5, None)
    assert asked == ["https://api.frankfurter.dev/v1/latest?base=USD&symbols=CNY"]
    assert r.title == "USD to CNY: 6.6954 (ECB, 2026-09-21)"
    assert r.snippet.startswith("100 USD = 669.5400 CNY · 1 USD = 6.6954 CNY · ECB reference rate for 2026-09-21")
    assert "not a live market quote" in r.snippet
    assert r.published_age == "2026-09-21" and r.published_age_confident


async def test_frankfurter_does_nothing_without_two_currencies(monkeypatch):
    engine = FrankfurterEngine()

    async def fake(url, **kw):
        raise AssertionError("no request expected")

    monkeypatch.setattr(engine, "_get_json", fake)
    assert await engine.fetch_results("dollar exchange rate today", 5, None) == []


# ---------------------------------------------------------------------------
# Federal Register and GOV.UK
# ---------------------------------------------------------------------------


def test_federal_register_maps_type_date_and_agencies():
    engine = get_engine("federalregister")
    assert "conditions[term]=AI+executive+order" in engine.build_url("AI executive order", 5)
    payload = {
        "results": [
            {
                "title": "Request for Comments on Managing Misuse Risk",
                "type": "Notice",
                "publication_date": "2025-01-15",
                "html_url": "https://www.federalregister.gov/documents/2025/01/15/2025-00698/x",
                "abstract": "The U.S. Artificial Intelligence Safety Institute requests comments.",
                "agencies": [{"name": "Commerce Department"}, {"name": "NIST"}],
            }
        ]
    }
    (r,) = engine.map_results(payload)
    assert r.snippet.startswith("Notice · published 2025-01-15 · Commerce Department, NIST")
    assert r.published_age == "2025-01-15" and r.published_age_confident


def test_govuk_maps_format_update_and_absolute_links():
    engine = get_engine("govuk")
    payload = {
        "results": [
            {
                "title": "Skilled Worker visa: going rates",
                "link": "/government/publications/skilled-worker-visa-going-rates",
                "description": "Going rates for eligible occupation codes.",
                "public_timestamp": "2026-04-09T09:30:00.000+01:00",
                "format": "detailed_guide",
                "organisations": [{"title": "UK Visas and Immigration"}],
            }
        ]
    }
    (r,) = engine.map_results(payload)
    assert r.url == "https://www.gov.uk/government/publications/skilled-worker-visa-going-rates"
    assert r.snippet.startswith("detailed guide · updated 2026-04-09 · UK Visas and Immigration")
    assert r.published_age == "2026-04-09"


# ---------------------------------------------------------------------------
# Ranking: a looked-up record leads its category
# ---------------------------------------------------------------------------


def _hit(engine: str, url: str, rank: int = 1) -> SearchResult:
    return SearchResult(title=url, url=url, snippet="", engine=engine, rank=rank)


def test_a_direct_answer_outranks_the_whole_default_pool_agreeing():
    portal = "https://www.weather.com.cn/weather/101020100.shtml"
    buckets = [
        [_hit(e, portal)] for e in ("duckduckgo", "bing", "anysearch", "mojeek")
    ] + [[_hit("openmeteo", "https://api.open-meteo.com/v1/forecast?latitude=31.22")]]
    ranked = _merge(buckets, 5, "weather", "上海明天天气")
    assert ranked[0]["url"].startswith("https://api.open-meteo.com/")
    assert ranked[0]["score"] > ranked[1]["score"]


def test_only_the_first_result_of_a_direct_engine_gets_the_weight():
    consensus = "https://security.snyk.io/package/pip/requests"
    buckets = [[_hit(e, consensus)] for e in ("duckduckgo", "bing", "anysearch")]
    buckets.append(
        [
            _hit("osv", "https://osv.dev/vulnerability/PYSEC-2026-1872", 1),
            _hit("osv", "https://osv.dev/vulnerability/PYSEC-2020-1", 2),
        ]
    )
    urls = [r["url"] for r in _merge(buckets, 5, "security", "requests vulnerabilities")]
    assert urls[0].endswith("PYSEC-2026-1872")
    assert urls[1] == consensus, "the second advisory is an ordinary native hit"


def test_nvd_keyword_candidates_do_not_outrank_a_consensus():
    consensus = "https://app.opencve.io/cve/?product=requests"
    buckets = [[_hit(e, consensus)] for e in ("duckduckgo", "bing", "anysearch")]
    buckets.append([_hit("nvd", "https://nvd.nist.gov/vuln/detail/CVE-2026-16870")])
    urls = [r["url"] for r in _merge(buckets, 5, "security", "requests vulnerabilities")]
    assert urls[0] == consensus
    urls = [r["url"] for r in _merge(buckets, 5, "security", "CVE-2026-16870")]
    assert urls[0].endswith("CVE-2026-16870"), "an id names a record"


def test_without_a_category_the_weight_goes_to_the_engine_that_claimed_the_question():
    consensus = "https://wise.com/us/currency-converter/usd-to-cny-rate"
    buckets = [[_hit(e, consensus)] for e in ("duckduckgo", "bing", "anysearch")]
    buckets.append([_hit("frankfurter", "https://api.frankfurter.dev/v1/latest?base=USD")])
    # frankfurter claims "100 usd to cny", so it leads with or without the category.
    assert _merge(buckets, 5, None, "100 usd to cny")[0]["url"].startswith(
        "https://api.frankfurter.dev/"
    )
    assert _merge(buckets, 5, "finance.fx", "100 usd to cny")[0]["url"].startswith(
        "https://api.frankfurter.dev/"
    )
    # A record engine that did not claim the question is an ordinary bucket.
    buckets[-1] = [_hit("pypi", "https://pypi.org/project/usd/1.0/")]
    assert _merge(buckets, 5, None, "100 usd to cny")[0]["url"] == consensus


def test_every_fact_engine_is_honest_about_which_results_are_records():
    """The record engines set the flag; the site searches (MDN, GOV.UK, the
    Federal Register) rank documents and must not."""
    records = {"pypi", "npm", "crates", "endoflife", "github_releases", "osv", "wikidata",
               "openmeteo", "frankfurter", "registries", "appstore", "rdap", "wdi", "holidays",
               "worldclock", "cfets", "gleif", "coingecko"}
    for name in FACT_ENGINES:
        engine = ENGINES[name]
        if name in records:
            assert engine.direct_answer, name
        elif name in ("nvd", "ietf", "cisakev"):
            assert not engine.direct_answer and engine.answers_directly("CVE-2024-3094 rfc 9110")
        else:
            assert not engine.direct_answer and not engine.answers_directly(name), name


def test_a_top_ranked_record_becomes_the_lead_without_echoing_the_question():
    from search_mcp.aggregator import _lead_snippet

    results = [
        {
            "url": "https://api.frankfurter.dev/v1/latest?base=JPY&symbols=CNY",
            "snippet": "10000 JPY = 425.7000 CNY · 1 JPY = 0.04257 CNY · ECB reference rate",
            "engines": ["frankfurter"],
        },
        {
            "url": "https://themoneyconverter.com/JPY/CNY",
            "snippet": "JPY 日元 国家 日本 " * 10 + "CNY 人民币 国家 中国",
            "engines": ["bing", "duckduckgo"],
        },
    ]
    lead = _lead_snippet("1万日元等于多少人民币", results)
    assert lead.startswith("According to api.frankfurter.dev: 10000 JPY = 425.7000 CNY")
    # A web page on top still has to echo the question.
    lead = _lead_snippet("1万日元等于多少人民币", results[::-1])
    assert lead.startswith("According to themoneyconverter.com:")
    # A keyword-mode NVD candidate on top is not a record, so the term test applies.
    results[0]["engines"] = ["nvd"]
    assert _lead_snippet("1万日元等于多少人民币", results).startswith(
        "According to themoneyconverter.com:"
    )


def test_record_categories_cap_the_cache_on_their_own_clock():
    from search_mcp.aggregator import _read_ttl

    assert _read_ttl(None, "weather", None) == 3600
    assert _read_ttl(None, "finance.fx", None) == 3600
    assert _read_ttl(None, "finance", None) is None, "bare finance keeps the default"
    assert _read_ttl(None, "software.python", None) == 6 * 3600
    assert _read_ttl(None, "security", None) == 6 * 3600
    assert _read_ttl(None, "reference", None) is None
    assert _read_ttl("day", "software", None) == 3600, "the tightest limit wins"
    assert _read_ttl(None, "weather", 600) == 600


async def test_frankfurter_falls_back_to_the_open_endpoint_for_non_ecb_currencies(monkeypatch):
    engine = FrankfurterEngine()
    asked: list[str] = []

    async def fake(url, **kw):
        asked.append(url)
        return {
            "result": "success",
            "base_code": "TWD",
            "time_last_update_unix": 1790035351,
            "time_last_update_utc": "Tue, 22 Sep 2026 00:02:31 +0000",
            "rates": {"USD": 0.031495, "CNY": 0.2105},
        }

    monkeypatch.setattr(engine, "_get_json", fake)
    (r,) = await engine.fetch_results("1000 TWD to CNY", 5, None)
    assert asked == ["https://open.er-api.com/v6/latest/TWD"]
    assert r.title == "TWD to CNY: 0.2105 (ExchangeRate-API, 2026-09-22)"
    assert r.snippet.startswith("1000 TWD = 210.5000 CNY · 1 TWD = 0.2105 CNY · daily rate for 2026-09-22")
    assert "Rates By Exchange Rate API https://www.exchangerate-api.com" in r.snippet
    assert r.published_age == "2026-09-22"
