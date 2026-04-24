from .base import Engine, SearchFilters, SearchResult, apply_post_filters
from .baidu import BaiduEngine
from .bing import BingEngine
from .brave import BraveEngine
from .duckduckgo import DuckDuckGoEngine
from .mojeek import MojeekEngine
from .startpage import StartpageEngine

ENGINES: dict[str, Engine] = {
    "duckduckgo": DuckDuckGoEngine(),
    "mojeek": MojeekEngine(),
    "startpage": StartpageEngine(),
    "brave": BraveEngine(),
    "bing": BingEngine(),
    "baidu": BaiduEngine(),
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
    "get_engine",
]
