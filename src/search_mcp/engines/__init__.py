from .antigravity import AntigravityEngine
from .anysearch import AnySearchEngine
from .appstore import AppStoreEngine
from .arxiv import ArxivEngine
from .baidu import BaiduEngine
from .base import (
    Category,
    CategoryGroup,
    Engine,
    SearchFilters,
    SearchResult,
    apply_post_filters,
    apply_post_filters_with_diagnostics,
    category_group,
)
from .bilibili import BilibiliEngine
from .bing import BingEngine
from .brave import BraveEngine
from .brave_api import BraveApiEngine
from .cfets import CfetsEngine
from .cisakev import CisaKevEngine
from .clinicaltrials import ClinicalTrialsEngine
from .cninfo import CninfoEngine
from .codex import CodexEngine
from .coingecko import CoinGeckoEngine
from .crates import CratesEngine
from .crossref import CrossrefEngine
from .dataeuropa import DataEuropaEngine
from .dataverse import DataverseEngine
from .dblp import DblpEngine
from .doaj import DoajEngine
from .dryad import DryadEngine
from .duckduckgo import DuckDuckGoEngine
from .endoflife import EndOfLifeEngine
from .europepmc import EuropePmcEngine
from .federalregister import FederalRegisterEngine
from .figshare import FigshareEngine
from .frankfurter import FrankfurterEngine
from .gdelt import GdeltEngine
from .github import GitHubCodeEngine, GitHubEngine
from .github_releases import GitHubReleasesEngine
from .gleif import GleifEngine
from .google import GoogleEngine
from .google_cse import GoogleCSEEngine
from .googlenews import GoogleNewsEngine
from .govuk import GovUkEngine
from .hackernews import HackerNewsEngine
from .holidays import HolidaysEngine
from .huggingface import HuggingFaceEngine
from .ietf import IetfEngine
from .imf import ImfEngine
from .mdn import MdnEngine
from .mojeek import MojeekEngine
from .npm import NpmEngine
from .nvd import NvdEngine
from .openalex import OpenAlexEngine
from .openlibrary import OpenLibraryEngine
from .openmeteo import OpenMeteoEngine
from .openverse import OpenverseEngine
from .osv import OsvEngine
from .pubmed import PubMedEngine
from .pypi import PyPIEngine
from .rdap import RdapEngine
from .registries import RegistriesEngine
from .searx import SearxEngine
from .sec_edgar import SecEdgarEngine
from .semanticscholar import SemanticScholarEngine
from .serper import SerperEngine
from .serpsearch import SerpSearchEngine
from .so360 import So360Engine
from .sogou import SogouEngine
from .stackexchange import StackExchangeEngine
from .startpage import StartpageEngine
from .tavily import TavilyEngine
from .wdi import WdiEngine
from .wikidata import WikidataEngine
from .wikimedia import WikimediaEngine
from .wikipedia import WikipediaEngine
from .worldbank import WorldBankEngine
from .worldclock import WorldClockEngine
from .yahoofinance import YahooFinanceEngine
from .zbmath import ZbMathEngine
from .zenodo import ZenodoEngine
from .zhihu import ZhihuEngine

ENGINES: dict[str, Engine] = {
    "duckduckgo": DuckDuckGoEngine(),
    "mojeek": MojeekEngine(),
    "searx": SearxEngine(),
    "googlenews": GoogleNewsEngine(),
    "startpage": StartpageEngine(),
    "brave": BraveEngine(),
    "bing": BingEngine(),
    "baidu": BaiduEngine(),
    # Engines added per integration request — all keyless, all opt-in (not in
    # the fast default pool). google/serpsearch scrape Google web SERP;
    # serpsearch is a pure alias of google. anysearch is a JSON REST aggregator
    # (anonymous tier). bilibili is a JSON video-search API. zhihu is
    # browser-rendered + best-effort (Zhihu hard-gates headless clients).
    "google": GoogleEngine(),
    "serpsearch": SerpSearchEngine(),
    "anysearch": AnySearchEngine(),
    "bilibili": BilibiliEngine(),
    "zhihu": ZhihuEngine(),
    # Vertical sources — all keyless JSON/feed APIs. They declare a `categories`
    # set, so `search(category=...)` pulls them in automatically (see
    # aggregator.engines_for_category); they stay OUT of the default pool so
    # ordinary web searches don't pay for a round trip they can't use.
    # Order matters: it decides who wins the category_engine_limit cap.
    # Scholarly sources. `category="paper"` round-robins across the sub-groups
    # (index / preprint / biomed / cs / openaccess / trial), so the order here
    # decides which engine LEADS each sub-group, not which sub-groups run.
    "arxiv": ArxivEngine(),
    "openalex": OpenAlexEngine(),
    "europepmc": EuropePmcEngine(),
    "crossref": CrossrefEngine(),
    "pubmed": PubMedEngine(),
    "semanticscholar": SemanticScholarEngine(),
    "dblp": DblpEngine(),
    "doaj": DoajEngine(),
    "clinicaltrials": ClinicalTrialsEngine(),
    # Mathematics gets its own corpus without changing the deliberate bare
    # paper top three (arxiv, openalex, europepmc). Putting zbMATH after every
    # existing scholarly sub-group keeps that spread stable; callers that need
    # it can narrow directly with `category="paper.math"`.
    "zbmath": ZbMathEngine(),
    "github": GitHubEngine(),
    "stackexchange": StackExchangeEngine(),
    "hackernews": HackerNewsEngine(),
    "wikipedia": WikipediaEngine(),
    "openlibrary": OpenLibraryEngine(),
    "gdelt": GdeltEngine(),
    # Image coverage keeps the broad aggregator first and adds Commons as an
    # independent upstream, so an Openverse outage no longer empties this
    # exclusive category.
    "openverse": OpenverseEngine(),
    "wikimedia": WikimediaEngine(),
    # Dataset order deliberately creates repository / ML / government buckets
    # in that order. Round-robin then spends the default three-source budget on
    # dryad, huggingface and dataeuropa instead of three overlapping
    # repositories; Dryad leads repositories because it is dataset-only and
    # returns the richest complete record in one request.
    "dryad": DryadEngine(),
    "huggingface": HuggingFaceEngine(),
    "dataeuropa": DataEuropaEngine(),
    "dataverse": DataverseEngine(),
    "zenodo": ZenodoEngine(),
    "figshare": FigshareEngine(),
    # Chinese-language web indexes (HTML scrapes, best-effort like zhihu).
    "sogou": SogouEngine(),
    "so360": So360Engine(),
    # Finance. Ordered so `category="finance"` (which round-robins across
    # sub-groups) reaches filings, market data and macro research in that
    # order — the three answer different questions and none substitutes for
    # another. `category="finance.filings"` etc. narrows to one of them.
    "sec_edgar": SecEdgarEngine(),
    "yahoofinance": YahooFinanceEngine(),
    "worldbank": WorldBankEngine(),
    "cninfo": CninfoEngine(),
    "imf": ImfEngine(),
    # Direct-fact sources: registries and reference APIs that answer a factual
    # question with the record itself (a version, a CVE, a rate, a forecast),
    # dated by the publisher. None of them is a web index, so all stay out of
    # the default pool and out of `web`.
    # Software. Bare `software` round-robins across its sub-groups, so the cap
    # of three seats endoflife (lifecycle), github_releases (github) and pypi
    # (python); npm and crates are reached through `software.node` and
    # `software.rust`, or by name.
    "endoflife": EndOfLifeEngine(),
    "github_releases": GitHubReleasesEngine(),
    "pypi": PyPIEngine(),
    "npm": NpmEngine(),
    "crates": CratesEngine(),
    # The other registries in one engine, chosen by the ecosystem named
    # (Maven, RubyGems, Go, Homebrew, Docker Hub, Packagist, NuGet), and the
    # App Store for an iOS app's current version.
    "registries": RegistriesEngine(),
    "appstore": AppStoreEngine(),
    # Security: NVD for a CVE id or recent CVEs, OSV for what touches a
    # package, KEV for whether a CVE is exploited in the wild.
    "nvd": NvdEngine(),
    "osv": OsvEngine(),
    "cisakev": CisaKevEngine(),
    # Reference, weather, documentation, statistics, the calendar.
    "wikidata": WikidataEngine(),
    "rdap": RdapEngine(),
    "openmeteo": OpenMeteoEngine(),
    "mdn": MdnEngine(),
    "ietf": IetfEngine(),
    "wdi": WdiEngine(),
    "holidays": HolidaysEngine(),
    "worldclock": WorldClockEngine(),
    # Rates, entities and crypto sit under `finance.*` sub-groups; registered
    # after the other finance sub-groups so the bare `finance` top three is
    # unchanged. CFETS follows frankfurter so a Chinese 人民币 question gets
    # the ECB fixing and the PBOC central parity, in that order.
    "frankfurter": FrankfurterEngine(),
    "cfets": CfetsEngine(),
    "gleif": GleifEngine(),
    "coingecko": CoinGeckoEngine(),
    # Government journals and portals.
    "federalregister": FederalRegisterEngine(),
    "govuk": GovUkEngine(),
    # API-key engines — opt-in. Configure keys via the admin UI
    # (`uv run search-mcp-admin`) or SEARCH_MCP_*_API_KEY env vars. Each engine
    # raises an actionable error when its key is unset, so it's safe to leave
    # registered while unconfigured (the aggregator surfaces the hint).
    "brave_api": BraveApiEngine(),
    "serper": SerperEngine(),
    "tavily": TavilyEngine(),
    "google_cse": GoogleCSEEngine(),
    # GitHub's code-search endpoint 401s anonymous callers, so unlike the
    # keyless `github` engine above this one needs a token.
    "github_code": GitHubCodeEngine(),
    # Opt-in like the keyed engines above, but enabled by an OAuth sign-in
    # (`search-mcp-login codex`) instead of a key: OpenAI's own web search,
    # the one Codex uses, on the operator's ChatGPT plan.
    "codex": CodexEngine(),
    # The same, on the sign-in of Google's Antigravity IDE: Google Search run
    # by a Gemini model. Google's terms forbid this use of that sign-in; it is
    # enabled only by `search-mcp-login antigravity`, which says so.
    "antigravity": AntigravityEngine(),
}


def source_taxonomy() -> dict[str, dict[str, list[str]]]:
    """`{group: {sub_group_or_"": [engine names]}}`, in registry order.

    DERIVED from `Engine.categories`, never hand-maintained. The hand-written
    engine buckets this replaces had drifted: they advertised `pubmed` for
    `category="paper"` long after `category_engine_limit` stopped it from ever
    running, and never mentioned `openverse` or `zenodo` at all. A table that
    can disagree with the registry eventually will.

    Engines with no declared category are general-web and appear under the
    `"web"` group, which is not a `Category` value — it names the default pool
    for the reader, not a routable filter.

    The `""` sub-group key holds engines that declare the group but no
    sub-group; it renders as the group's own bucket.
    """
    taxonomy: dict[str, dict[str, list[str]]] = {}
    for name, engine in ENGINES.items():
        if not engine.categories:
            taxonomy.setdefault("web", {}).setdefault("", []).append(name)
            continue
        # A sub-group implies its parent group, so every engine already carries
        # the bare group token too. Listing it under BOTH the group bucket and
        # its sub-groups would print it twice; the group bucket is therefore
        # reserved for engines that declare the group and nothing narrower.
        groups = {t for t in engine.categories if "." not in t}
        for group in sorted(groups):
            subs = sorted(
                t.partition(".")[2]
                for t in engine.categories
                if t.startswith(group + ".")
            )
            bucket = taxonomy.setdefault(group, {})
            for sub in subs or [""]:
                bucket.setdefault(sub, []).append(name)
    return taxonomy


def get_engine(name: str) -> Engine:
    key = name.lower().strip()
    if key not in ENGINES:
        raise ValueError(f"Unknown engine: {name!r}. Available: {list(ENGINES)}")
    return ENGINES[key]


__all__ = [
    "ENGINES",
    "Category",
    "CategoryGroup",
    "Engine",
    "SearchFilters",
    "SearchResult",
    "apply_post_filters",
    "apply_post_filters_with_diagnostics",
    "category_group",
    "get_engine",
    "source_taxonomy",
]
