from .base import (
    Engine,
    SearchFilters,
    SearchResult,
    apply_post_filters,
    apply_post_filters_with_diagnostics,
)
from .anysearch import AnySearchEngine
from .baidu import BaiduEngine
from .bilibili import BilibiliEngine
from .bing import BingEngine
from .brave import BraveEngine
from .brave_api import BraveApiEngine
from .duckduckgo import DuckDuckGoEngine
from .google import GoogleEngine
from .google_cse import GoogleCSEEngine
from .googlenews import GoogleNewsEngine
from .mojeek import MojeekEngine
from .searx import SearxEngine
from .serper import SerperEngine
from .serpsearch import SerpSearchEngine
from .startpage import StartpageEngine
from .tavily import TavilyEngine
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
    # API-key engines — opt-in. Configure keys via the admin UI
    # (`uv run search-mcp-admin`) or SEARCH_MCP_*_API_KEY env vars. Each engine
    # raises an actionable error when its key is unset, so it's safe to leave
    # registered while unconfigured (the aggregator surfaces the hint).
    "brave_api": BraveApiEngine(),
    "serper": SerperEngine(),
    "tavily": TavilyEngine(),
    "google_cse": GoogleCSEEngine(),
}


def get_engine(name: str) -> Engine:
    key = name.lower().strip()
    if key not in ENGINES:
        raise ValueError(f"Unknown engine: {name!r}. Available: {list(ENGINES)}")
    return ENGINES[key]


__all__ = [
    "ENGINES",
    "Engine",
    "SearchFilters",
    "SearchResult",
    "apply_post_filters",
    "apply_post_filters_with_diagnostics",
    "get_engine",
]
