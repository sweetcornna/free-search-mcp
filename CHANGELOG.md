# Changelog

All notable changes to this project are documented here. The format is loosely
based on [Keep a Changelog](https://keepachangelog.com/), and the project follows
semantic versioning.

## [0.2.0] - 2026-06-01

A large feature release: 9 new search engines (keyless + keyed), a local admin
backend for configuration, and a full set of fixes for provider-gated engines
(proxy, SearXNG fallback, gate diagnostics, Zhihu login).

### Added

**Keyless engines** (no API key, opt-in via `engines=[...]`):
- `google` — Google web SERP scraper (HTTP + browser fallback).
- `serpsearch` — alias of `google` (keyless Google SERP).
- `anysearch` — AnySearch unified-search REST API (anonymous tier; optional key
  lifts limits).
- `bilibili` — Bilibili (哔哩哔哩) video search via the public JSON API.
- `zhihu` — Zhihu (知乎) search, browser-rendered (best-effort; see login below).

**Keyed engines** (dormant until a key is configured):
- `brave_api` (Brave Search API), `serper` (Serper/Google), `tavily` (Tavily AI
  search), `google_cse` (Google Custom Search — needs API key + cx).

**Admin backend & configuration:**
- `search-mcp-admin` — a localhost-only web UI to enter API keys and a proxy,
  with a per-provider "how to get a key" guide, masked inputs, live Save (no
  restart), Test, and Clear. Secrets are stored at `~/.config/search-mcp/config.json`
  (`0600`) and never echoed back to the page.
- `keystore` module: hot-reloaded JSON config with `SEARCH_MCP_*` env override.
- `.env` keys are loaded at server/admin startup.

**Gated-engine fixes (proxy · fallback · diagnostics · login):**
- Optional **proxy** support (`SEARCH_MCP_PROXY` / admin "Network / Proxy" card)
  applied to HTTP engines, the browser pool, and remote fetch/document/structured
  calls. Scope with `SEARCH_MCP_PROXY_ENGINES`. (`http`/`https`/`socks5`.)
- **SearXNG auto-fallback**: `google`/`serpsearch`/`bing` transparently recover
  via the working `searx` meta-search when CAPTCHA-gated; results attributed to
  `searx`.
- **Gate diagnostics**: responses include `gated_engines` + `gated_hint`
  (`captcha`/`consent`/`login`).
- `search-mcp-login` — one-time interactive Zhihu login; cookies persist so
  later headless searches work.
- Transient browser navigation errors are retried once.

**Deploy & docs:**
- One-click `scripts/install.sh`, `Dockerfile` + `docker-compose.yml`,
  project-scoped `.mcp.json`, annotated `.env.example`.
- New docs: `docs/USAGE.md`, `docs/API_KEYS.md`, `docs/PROXY_AND_GATES.md`.

### Fixed
- `anysearch` response mapping (results are nested under `data.results`).

### Notes
- Keyless defaults (`duckduckgo`, `mojeek`, `googlenews`) are unchanged; all new
  engines are opt-in to preserve the fast default-pool latency.
- We deliberately do not attempt to defeat provider CAPTCHAs (ToS); the proxy and
  fallback are the supported ways around datacenter-IP gating.

## [0.1.0]

- Initial release: multi-engine keyless search, smart fetch (httpx → Playwright),
  document reading, FTS5 cache, filters, and LLM-tuned Markdown output.

[0.2.0]: https://github.com/sweetcornna/free-search-mcp/releases/tag/v0.2.0
