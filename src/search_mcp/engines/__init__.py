from .anysearch import AnySearchEngine
from .arxiv import ArxivEngine
from .baidu import BaiduEngine
from .base import (
    Category,
    Engine,
    SearchFilters,
    SearchResult,
    apply_post_filters,
    apply_post_filters_with_diagnostics,
)
from .bilibili import BilibiliEngine
from .bing import BingEngine
from .brave import BraveEngine
from .brave_api import BraveApiEngine
from .crossref import CrossrefEngine
from .duckduckgo import DuckDuckGoEngine
from .gdelt import GdeltEngine
from .github import GitHubCodeEngine, GitHubEngine
from .google import GoogleEngine
from .google_cse import GoogleCSEEngine
from .googlenews import GoogleNewsEngine
from .hackernews import HackerNewsEngine
from .mojeek import MojeekEngine
from .openalex import OpenAlexEngine
from .openlibrary import OpenLibraryEngine
from .openverse import OpenverseEngine
from .pubmed import PubMedEngine
from .searx import SearxEngine
from .serper import SerperEngine
from .serpsearch import SerpSearchEngine
from .so360 import So360Engine
from .sogou import SogouEngine
from .stackexchange import StackExchangeEngine
from .startpage import StartpageEngine
from .tavily import TavilyEngine
from .wikipedia import WikipediaEngine
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
    "arxiv": ArxivEngine(),
    "openalex": OpenAlexEngine(),
    "crossref": CrossrefEngine(),
    "pubmed": PubMedEngine(),
    "github": GitHubEngine(),
    "stackexchange": StackExchangeEngine(),
    "hackernews": HackerNewsEngine(),
    "wikipedia": WikipediaEngine(),
    "openlibrary": OpenLibraryEngine(),
    "gdelt": GdeltEngine(),
    "openverse": OpenverseEngine(),
    "zenodo": ZenodoEngine(),
    # Chinese-language web indexes (HTML scrapes, best-effort like zhihu).
    "sogou": SogouEngine(),
    "so360": So360Engine(),
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
}


def get_engine(name: str) -> Engine:
    key = name.lower().strip()
    if key not in ENGINES:
        raise ValueError(f"Unknown engine: {name!r}. Available: {list(ENGINES)}")
    return ENGINES[key]


__all__ = [
    "ENGINES",
    "Category",
    "Engine",
    "SearchFilters",
    "SearchResult",
    "apply_post_filters",
    "apply_post_filters_with_diagnostics",
    "get_engine",
]
