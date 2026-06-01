from pathlib import Path
from typing import Literal

from pydantic import Field
from pydantic_settings import BaseSettings, SettingsConfigDict

DEFAULT_CACHE_DIR = Path.home() / ".cache" / "search-mcp"


class Settings(BaseSettings):
    model_config = SettingsConfigDict(env_prefix="SEARCH_MCP_", env_file=".env", extra="ignore")

    cache_dir: Path = DEFAULT_CACHE_DIR
    cache_ttl_seconds: int = 60 * 60 * 24 * 7

    # All-HTTP, low-latency default pool. Picked for "consistently fast AND
    # consistently returns results in 2026":
    #   * duckduckgo  — curl_cffi chrome131 fingerprint dodges anomaly 202s
    #   * mojeek      — independent index; intermittently IP-blocked but cheap
    #                   to attempt and falls back fast when it is
    #   * googlenews  — RSS, ~1s, gives news-skewed coverage that complements
    #                   the other two on time-sensitive queries
    # Searx public instances are unreliable (often ≥10s timeouts/empties) and
    # Startpage forces a browser render — both stay opt-in via `engines=`.
    default_engines: list[str] = ["duckduckgo", "mojeek", "googlenews"]
    max_results_per_engine: int = 10

    rate_limit_per_minute: int = Field(default=30, gt=0)
    fetch_rate_limit_per_minute: int = Field(default=20, gt=0)

    request_timeout: float = 15.0
    fetch_timeout: float = 25.0

    user_agent: str = (
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) "
        "AppleWebKit/537.36 (KHTML, like Gecko) "
        "Chrome/131.0.0.0 Safari/537.36"
    )
    accept_language: str = "en-US,en;q=0.9"

    fetch_strategy: Literal["auto", "http", "browser"] = "auto"
    browser_headless: bool = True
    browser_pool_size: int = 2
    max_content_chars: int = 50_000

    safesearch: Literal["strict", "moderate", "off"] = "moderate"
    region: str = "us-en"

    log_level: str = "INFO"

    # --- Safety / sandbox knobs -------------------------------------------
    # SSRF guard escape hatch: when False (default) URLs that resolve to
    # loopback/link-local/private/reserved addresses are rejected.
    allow_private_hosts: bool = False
    # read_doc local-file sandbox root. None (default) DISABLES local file
    # reads entirely — the user opts in by pointing this at a directory.
    document_root: Path | None = None
    # Response-bomb guard: cap on remote response body bytes.
    max_response_bytes: int = 25_000_000
    # Decompression-bomb guard: max PDF pages to parse.
    max_pdf_pages: int = 200
    # Cap on extracted document text (distinct from max_content_chars, which
    # is the fetch-truncation knob for web pages).
    max_document_chars: int = 2_000_000

    def cache_path(self) -> Path:
        self.cache_dir.mkdir(parents=True, exist_ok=True)
        return self.cache_dir / "cache.sqlite"


settings = Settings()
