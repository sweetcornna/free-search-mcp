"""Secret/config store for API-key engines + the admin backend.

A tiny, dependency-free layer that holds the API keys the keyed search engines
need (Brave/Serper/Tavily/Google CSE/…). It is the single source of truth shared
by the engines (which read keys) and the admin UI (which writes them).

Design goals — keep the flow simple:
  * **Hot-reload.** Keys live in a JSON file (``<config_dir>/config.json``).
    ``get_secret`` re-reads the file when it changes (mtime cache), so saving in
    the admin UI takes effect on a running server with no restart.
  * **Env override.** ``SEARCH_MCP_<FIELD>`` env vars always win over the file,
    so existing 12-factor / container deployments keep working unchanged.
  * **Safe at rest.** The file is written atomically with ``0600`` perms; secrets
    are never logged.

Precedence for ``get_secret(field)``:  env var  >  JSON file  >  ``None``.

``PROVIDERS`` is the metadata catalogue that drives BOTH the engines and the
admin form (labels, the fields each provider needs, signup URL, the step-by-step
"how to get a key" guide, and the free-tier note).
"""

from __future__ import annotations

import json
import os
import tempfile
from dataclasses import dataclass
from pathlib import Path


# --- paths ------------------------------------------------------------------


def config_dir() -> Path:
    """Directory holding ``config.json``. Override with ``SEARCH_MCP_CONFIG_DIR``;
    defaults to ``~/.config/search-mcp`` (distinct from the cache dir)."""
    env = os.environ.get("SEARCH_MCP_CONFIG_DIR")
    if env:
        return Path(env).expanduser()
    return Path.home() / ".config" / "search-mcp"


def config_path() -> Path:
    return config_dir() / "config.json"


def load_env_file_into_environ(path: str | Path = ".env") -> None:
    """Populate ``os.environ`` with ``SEARCH_MCP_*`` keys from a ``.env`` file.

    ``get_secret`` reads ``os.environ`` (not the ``.env`` file pydantic-settings
    loads for the Settings model), so without this a key that lives only in
    ``.env`` would be invisible to the keyed engines. Call this once from the
    server / admin entry points. Best-effort and idempotent: it never overrides
    an already-set real env var, and never raises (a missing file is a no-op)."""
    try:
        lines = Path(path).read_text(encoding="utf-8").splitlines()
    except OSError:
        return
    for line in lines:
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, _, val = line.partition("=")
        key = key.strip()
        # Strip optional surrounding quotes.
        val = val.strip().strip('"').strip("'")
        if key.startswith("SEARCH_MCP_") and key not in os.environ:
            os.environ[key] = val


# --- file load (mtime-cached for hot reload) --------------------------------

_cache: dict[str, str] = {}
_cache_mtime: float | None = None
_cache_path: str | None = None


def _load_file() -> dict[str, str]:
    """Return the on-disk secrets dict, re-reading only when the file changes.

    Never raises: a missing/corrupt file yields ``{}`` so a bad edit can't take
    the server down."""
    global _cache, _cache_mtime, _cache_path
    path = config_path()
    try:
        mtime = path.stat().st_mtime
    except OSError:
        # File absent -> empty store; reset the cache so a later create is seen.
        _cache, _cache_mtime, _cache_path = {}, None, str(path)
        return {}
    if str(path) == _cache_path and mtime == _cache_mtime:
        return _cache
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
        data = raw.get("secrets", raw) if isinstance(raw, dict) else {}
        # Coerce to str:str, dropping anything malformed.
        clean = {str(k): str(v) for k, v in data.items() if isinstance(v, (str, int, float))}
    except (OSError, ValueError):
        clean = {}
    _cache, _cache_mtime, _cache_path = clean, mtime, str(path)
    return clean


def _env_name(field_key: str) -> str:
    return "SEARCH_MCP_" + field_key.upper()


# --- public read API --------------------------------------------------------


def get_secret(field_key: str) -> str | None:
    """Resolve a secret/config field. Env var beats the JSON file beats ``None``.
    An empty/whitespace value is treated as unset."""
    env = os.environ.get(_env_name(field_key))
    if env and env.strip():
        return env.strip()
    val = _load_file().get(field_key)
    if val and val.strip():
        return val.strip()
    return None


def all_secrets() -> dict[str, str]:
    """The file-backed secrets (NOT env), for the admin UI. Returns a copy."""
    return dict(_load_file())


# --- public write API (used by the admin backend) ---------------------------


def set_secrets(updates: dict[str, str]) -> None:
    """Merge ``updates`` into the store and persist atomically (0600).

    An empty-string value DELETES that field (lets the UI clear a key). Keys not
    present in ``updates`` are left untouched, so submitting a blank field in the
    form never wipes an existing secret."""
    current = dict(_load_file())
    for k, v in updates.items():
        if v is None or str(v).strip() == "":
            current.pop(k, None)
        else:
            current[k] = str(v).strip()
    _write(current)


def delete_secret(field_key: str) -> None:
    current = dict(_load_file())
    if current.pop(field_key, None) is not None:
        _write(current)


def _write(secrets: dict[str, str]) -> None:
    global _cache, _cache_mtime, _cache_path
    d = config_dir()
    d.mkdir(parents=True, exist_ok=True)
    path = config_path()
    payload = json.dumps({"secrets": secrets}, ensure_ascii=False, indent=2)
    # Atomic replace: write to a temp file in the same dir, fix perms, rename.
    fd, tmp = tempfile.mkstemp(dir=str(d), prefix=".config.", suffix=".tmp")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as fh:
            fh.write(payload)
        os.chmod(tmp, 0o600)
        os.replace(tmp, path)
    finally:
        if os.path.exists(tmp):
            os.unlink(tmp)
    # Invalidate cache so the next read reflects the write immediately.
    _cache_mtime = None


# --- provider catalogue (drives engines + admin form) -----------------------


@dataclass(frozen=True)
class ProviderField:
    key: str                 # secret/config key, e.g. "serper_api_key"
    label: str               # human label in the form
    secret: bool = True      # render as a password field, mask in status
    required: bool = True
    placeholder: str = ""


@dataclass(frozen=True)
class Provider:
    id: str                  # provider id == engine name, e.g. "serper"
    label: str               # display name, e.g. "Serper (Google)"
    engine: str              # engine name registered in ENGINES
    fields: list[ProviderField]
    signup_url: str
    free_tier: str           # short note, e.g. "2,500 free queries"
    how_to: list[str]        # numbered steps to obtain a key
    docs_url: str = ""
    optional: bool = False   # engine works without the key (e.g. anysearch)


PROVIDERS: list[Provider] = [
    Provider(
        id="brave_api",
        label="Brave Search API",
        engine="brave_api",
        fields=[ProviderField("brave_api_key", "API key", placeholder="BSA…")],
        signup_url="https://brave.com/search/api/",
        free_tier="2,000 queries/month free",
        how_to=[
            "Open https://brave.com/search/api/ and click 'Get started'.",
            "Sign up / log in and verify your email.",
            "Subscribe to the free 'Data for Search' plan (a card may be required, but the free tier isn't charged).",
            "Open the dashboard → API Keys → copy your subscription token.",
        ],
        docs_url="https://api-dashboard.search.brave.com/app/documentation",
    ),
    Provider(
        id="serper",
        label="Serper (Google)",
        engine="serper",
        fields=[ProviderField("serper_api_key", "API key", placeholder="…")],
        signup_url="https://serper.dev",
        free_tier="2,500 free queries (one-time)",
        how_to=[
            "Open https://serper.dev and sign up (Google login works).",
            "You land on the dashboard with 2,500 free credits.",
            "Copy the API key shown under 'API Key'.",
        ],
        docs_url="https://serper.dev/playground",
    ),
    Provider(
        id="tavily",
        label="Tavily (AI search)",
        engine="tavily",
        fields=[ProviderField("tavily_api_key", "API key", placeholder="tvly-…")],
        signup_url="https://app.tavily.com",
        free_tier="1,000 credits/month free",
        how_to=[
            "Open https://app.tavily.com and sign up.",
            "On the dashboard, find the 'API Keys' section.",
            "Copy your key (it starts with 'tvly-').",
        ],
        docs_url="https://docs.tavily.com",
    ),
    Provider(
        id="google_cse",
        label="Google Custom Search",
        engine="google_cse",
        fields=[
            ProviderField("google_cse_api_key", "API key", placeholder="AIza…"),
            ProviderField("google_cse_cx", "Search engine ID (cx)", secret=False, placeholder="0123…:abcd"),
        ],
        signup_url="https://programmablesearchengine.google.com/",
        free_tier="100 queries/day free",
        how_to=[
            "Create an API key: https://console.cloud.google.com/apis/credentials → 'Create credentials' → 'API key'.",
            "Enable the 'Custom Search API' for that project: https://console.cloud.google.com/apis/library/customsearch.googleapis.com.",
            "Create a search engine at https://programmablesearchengine.google.com/ → set it to 'Search the entire web'.",
            "Copy the 'Search engine ID' (cx) from its control panel. Paste both the API key and the cx here.",
        ],
        docs_url="https://developers.google.com/custom-search/v1/overview",
    ),
    Provider(
        id="anysearch",
        label="AnySearch (optional key)",
        engine="anysearch",
        fields=[ProviderField("anysearch_api_key", "API key (optional)", required=False, placeholder="leave blank for anonymous")],
        signup_url="https://anysearch.com/console/api-keys",
        free_tier="works keyless; a key raises the rate limit/quota",
        how_to=[
            "AnySearch works anonymously with no key (lower limits).",
            "For higher limits, sign up at https://anysearch.com and open Console → API Keys.",
            "Create a key and paste it here.",
        ],
        docs_url="https://www.anysearch.com/docs",
        optional=True,
    ),
]


# Network / proxy config (rendered as its own card in the admin UI). The proxy
# may embed credentials, so it is treated as a secret. Read via net.proxy_url().
NETWORK_FIELDS: list[ProviderField] = [
    ProviderField(
        "proxy",
        "Proxy URL",
        secret=True,
        required=False,
        placeholder="http://user:pass@host:port  ·  socks5://host:port",
    ),
    ProviderField(
        "proxy_engines",
        "Proxy only these engines (optional, comma-separated)",
        secret=False,
        required=False,
        placeholder="blank = all engines · e.g. google bing zhihu",
    ),
]


def provider_by_id(pid: str) -> Provider | None:
    return next((p for p in PROVIDERS if p.id == pid), None)


def is_configured(provider_id: str) -> bool:
    """True when every REQUIRED field of the provider resolves to a value
    (via env or file). Optional-only providers (anysearch) are always True."""
    p = provider_by_id(provider_id)
    if p is None:
        return False
    required = [f for f in p.fields if f.required]
    if not required:
        return True
    return all(get_secret(f.key) is not None for f in required)


def provider_status() -> dict[str, bool]:
    return {p.id: is_configured(p.id) for p in PROVIDERS}


# Exposed for tests that want to reset the hot-reload cache between cases.
def _reset_cache() -> None:  # pragma: no cover - trivial test hook
    global _cache, _cache_mtime, _cache_path
    _cache, _cache_mtime, _cache_path = {}, None, None


__all__ = [
    "Provider",
    "ProviderField",
    "PROVIDERS",
    "all_secrets",
    "config_dir",
    "config_path",
    "delete_secret",
    "get_secret",
    "NETWORK_FIELDS",
    "is_configured",
    "load_env_file_into_environ",
    "provider_by_id",
    "provider_status",
    "set_secrets",
]
