# Configuration

All settings are optional environment variables prefixed with `SEARCH_MCP_`.
Inspect resolved values (with secrets redacted) at any time via:

```bash
free-search-mcp config
```

## Precedence & Security Isolation

| Variable Class | 1. Process Env | 2. `./.env` (CWD) | 3. `<config_dir>/.env` | 4. `<config_dir>/config.json` (`0600`) |
|---|---|---|---|---|
| **Ordinary settings** | Applied | Applied | Applied | — |
| **Security-sensitive (`[SEC]`)** | Applied | **Ignored (+ stderr notice)** | Applied | Applied (credentials, `PROXY`, `PROXY_ENGINES`) |

Default `<config_dir>` is `~/.config/search-mcp` (override via process env
`SEARCH_MCP_CONFIG_DIR`). Empty/whitespace values and unresolved `${...}` MCPB
placeholders are treated as unset (`drop_unfilled`).

> **Why `./.env` ignores `[SEC]` variables**: A cloned repository's `.env` must
> never be able to disable SSRF checks, redirect `DOCUMENT_ROOT` or `CONFIG_DIR`,
> inject a proxy, relax `EXEC_RAW_HTTP`, or smuggle credentials.

## Active Settings (`1.0.0`)

### Pools & Routing

| Variable | Default | Meaning |
|---|---|---|
| `SEARCH_MCP_DEFAULT_ENGINES` | `["duckduckgo","bing","yahoojp","seznam"]` | Keyless web default pool (`yahoojp` is unavailable in EEA/UK) |
| `SEARCH_MCP_RESERVE_ENGINES` | `["yahoo","youcom","mwmbl"]` | Hedging reserves used when healthy engines drop below `MIN_HEALTHY_ENGINES` or primary fails |
| `SEARCH_MCP_MIN_HEALTHY_ENGINES` | `3` | Healthy engine threshold before seating reserves |
| `SEARCH_MCP_FRESH_ENGINES` | `["googlenews"]` | Joined automatically for `freshness="day"\|"week"` |
| `SEARCH_MCP_LOCALE_ENGINES` | `{"zh":["so360","quark","baidu"],"ko":["naver","daum"]}` | Joined when the query script matches that language |
| `SEARCH_MCP_MAX_RESULTS_PER_ENGINE` | `10` | Per-engine hit cap |
| `SEARCH_MCP_SCOPE_ENGINE_LIMIT` | `3` | Max scope-native sources added when `scope=` is passed *(legacy alias: `SEARCH_MCP_CATEGORY_ENGINE_LIMIT`)* |
| `SEARCH_MCP_AUTO_ROUTE_ENABLED` | `true` | Auto-route direct-fact claimants on `scope="web"` and `scope="code"` |
| `SEARCH_MCP_CLAIM_ENGINE_LIMIT` | `3` | Max auto-routed claimant sources per query (`< 0` unbounded) |
| `SEARCH_MCP_HEDGE_ENABLED` | `true` | Dispatch reserves after 1.8 s / +800 ms if `< 2` engines answered *(legacy alias: `SEARCH_MCP_RESCUE_ENABLED`)* |

### Health, Limits & Timeouts

| Variable | Default | Meaning |
|---|---|---|
| `SEARCH_MCP_HEALTH_ENABLED` | `true` | Circuit breaker persisted in SQLite `health_state` |
| `SEARCH_MCP_ENGINE_COOLDOWN_SECONDS` | `600` | Initial bench duration (doubles per repeat) |
| `SEARCH_MCP_ENGINE_COOLDOWN_MAX_SECONDS` | `3600` | Maximum bench ceiling |
| `SEARCH_MCP_COHERENCE_GUARD_ENABLED` | `true` | CJK-aware off-topic web decoy guard |
| `SEARCH_MCP_SEARCH_DEADLINE_SECONDS` | `8` | Hard search deadline in seconds (clamped to `(0, 8.0]`) |
| `SEARCH_MCP_REQUEST_TIMEOUT` | `5.0` | Per-engine request timeout (seconds) |
| `SEARCH_MCP_FETCH_TIMEOUT` | `25.0` | Page fetch timeout (seconds) |
| `SEARCH_MCP_RATE_LIMIT_PER_MINUTE` | `30` | Default per-source GCRA rate limit |
| `SEARCH_MCP_FETCH_RATE_LIMIT_PER_MINUTE` | `20` | Shared fetch rate limit |

### Cache & Retraction Watch Sync

| Variable | Default | Meaning |
|---|---|---|
| `SEARCH_MCP_CACHE_DIR` | `~/.cache/search-mcp` | Directory holding `search-mcp.sqlite` (coexists beside v0.18.x `cache.sqlite`) |
| `SEARCH_MCP_CACHE_TTL_SECONDS` | `604800` | Default cache TTL (7 days; recency/news/weather/FX/software/security expire sooner) |
| `SEARCH_MCP_CACHE_MAX_MB` | `512` | SQLite cache size limit in MB (`0` disables eviction) |
| `SEARCH_MCP_OFFLINE_SYNC` | `true` | Lazy ~70 MB Crossref Retraction Watch CSV download on first scholar call (~8–12 MB indexed in SQLite, CSV deleted, 7-day ETag refresh). Set `false` to opt out. |

### Locale, UA & Content

| Variable | Default | Meaning |
|---|---|---|
| `SEARCH_MCP_REGION` | `us-en` | `cc-lang` region hint |
| `SEARCH_MCP_ACCEPT_LANGUAGE` | `en-US,en;q=0.9` | `Accept-Language` header |
| `SEARCH_MCP_SAFESEARCH` | `moderate` | `strict` / `moderate` / `off` |
| `SEARCH_MCP_CONTACT_EMAIL` | *(empty)* | Polite-pool email for Crossref, OpenAlex, NCBI, and Unpaywall OA full text (Unpaywall is skipped when unset) |
| `SEARCH_MCP_USER_AGENT` | *(empty)* | Chrome-profile UA override (empty keeps emulated TLS+header consistency; plain profile always uses `free-search-mcp/1.0`) |
| `SEARCH_MCP_PREWARM` | `true` | Background HTTP/2 connection warmup on startup |
| `SEARCH_MCP_MAX_CONTENT_CHARS` | `50000` | Per-page truncation in `web.fetch` (full text remains cached) |
| `SEARCH_MCP_LOG_LEVEL` | `INFO` | Mapped to `RUST_LOG` (`ERROR`, `WARN`, `INFO`, `DEBUG`, `TRACE`) |

### Transport & Streamable HTTP

| Variable | Default | Meaning |
|---|---|---|
| `SEARCH_MCP_TRANSPORT` | `stdio` | `stdio` or `streamable-http` (CLI `--transport` overrides) |
| `SEARCH_MCP_HTTP_HOST` | `127.0.0.1` | Streamable HTTP bind host (`--host` overrides) |
| `SEARCH_MCP_HTTP_PORT` | `8000` | Streamable HTTP port (`--port` overrides) |
| `SEARCH_MCP_HTTP_PATH` | `/mcp` | Streamable HTTP path (`--path` overrides) |
| `SEARCH_MCP_HTTP_ALLOWED_ORIGINS` **`[SEC]`** | *(empty)* | Extra allowed `Origin` values (comma/space separated; loopback always allowed) |
| `SEARCH_MCP_HTTP_TOKEN` **`[SEC]`** | *(empty)* | Optional Bearer token required for Streamable HTTP requests |
| `SEARCH_MCP_TOOL_CALL_BUDGET` | `0` | Per-process tool call limit (`0` = unlimited; returns non-error summary prompt once exhausted) |

### Sandboxed `exec` Limits & Guardrails

| Variable | Default | Meaning |
|---|---|---|
| `SEARCH_MCP_EXEC_TIMEOUT_S` | `120` | Ceiling on per-call `exec` `timeout_s` (clamped to `1..=120`; env can only lower) |
| `SEARCH_MCP_EXEC_MAX_HOST_CALLS` | `100` | Max Host API calls per `exec` (clamped to `1..=300`) |
| `SEARCH_MCP_EXEC_HEAP_MB` | `96` | QuickJS-NG heap limit in MiB (clamped to `16..=128`) |
| `SEARCH_MCP_EXEC_RAW_HTTP` **`[SEC]`** | `get-post` | `web.request` policy: `get-post` / `get-only` / `off` (auto-downgrades to `get-only` when `DOCUMENT_ROOT` is set unless explicit) |
| `SEARCH_MCP_EXEC_AUTH_HOSTS` **`[SEC]`** | *(empty)* | Host allowlist permitted to receive auth/cookie headers in `web.request` |

### Proxy, SSRF, Documents & Downloads

| Variable | Default | Meaning |
|---|---|---|
| `SEARCH_MCP_CONFIG_DIR` **`[SEC]`** | `~/.config/search-mcp` | Directory for `.env`, `config.json`, and custom `sources.d/*.toml` manifests |
| `SEARCH_MCP_PROXY` **`[SEC]`** | *(empty)* | Outbound proxy (`http://`, `https://`, `socks5://`, `socks5h://`) |
| `SEARCH_MCP_PROXY_ENGINES` **`[SEC]`** | *(empty)* | Limit proxy to specific source IDs (blank = all sources) |
| `SEARCH_MCP_NO_PROXY` | *(empty)* | Comma/space-separated host suffixes bypassing `SEARCH_MCP_PROXY` |
| `SEARCH_MCP_SSRF_RESOLVE_ADDRESSES` **`[SEC]`** | `auto` | `auto` / `always` / `never` — `auto` detects outbound proxies and Fake-IP VPNs (Clash/Surge/sing-box TUN) via canary probes |
| `SEARCH_MCP_ALLOW_PRIVATE_HOSTS` **`[SEC]`** | `false` | Escape hatch disabling private/loopback/metadata IP blocking (leave `false`) |
| `SEARCH_MCP_DOCUMENT_ROOT` **`[SEC]`** | *(empty)* | Opt-in root directory for local `doc.read`/`doc.outline` (taints `exec` against network egress once read) |
| `SEARCH_MCP_DOWNLOAD_ENABLED` **`[SEC]`** | `true` | Enable/disable `web.download` |
| `SEARCH_MCP_DOWNLOAD_DIR` **`[SEC]`** | `<cache_dir>/downloads` | Destination directory for `web.download` |
| `SEARCH_MCP_DOWNLOAD_TTL_HOURS` | `24` | Auto-expiry for downloaded files (`0` keeps forever) |
| `SEARCH_MCP_DOWNLOAD_MAX_MB` | `100` | Per-file download size cap (MB) |
| `SEARCH_MCP_DOWNLOAD_DIR_MAX_MB` **`[SEC]`** | `500` | Total download directory cap (MB) |
| `SEARCH_MCP_MAX_RESPONSE_BYTES` | `25000000` | Default HTTP body cap in bytes (25 MB) |
| `SEARCH_MCP_MAX_PDF_PAGES` | `200` | Maximum PDF pages parsed per call |
| `SEARCH_MCP_MAX_DOCUMENT_CHARS` | `2000000` | Maximum extracted characters per document |

### Answer Agent (`ask` & `search` Synthesis)

See [DELEGATION.md](DELEGATION.md). Defaults leave the answer agent `off` (so `ask` uses keyless `fast` mode with no LLM).

| Variable | Default | Meaning |
|---|---|---|
| `SEARCH_MCP_AGENT_BACKEND` | `off` | `off` \| `api` \| `claude-code` \| `codex` |
| `SEARCH_MCP_AGENT_MODEL` | *(empty)* | Required for `api`; empty = `haiku` (Claude Code) or CLI default (Codex) |
| `SEARCH_MCP_AGENT_API_PROTOCOL` | `openai` | `openai` (`/chat/completions`) or `anthropic` (`/v1/messages`) |
| `SEARCH_MCP_AGENT_API_BASE_URL` | *(empty)* | Base URL for `api` backend (e.g. `http://localhost:11434/v1` for Ollama) |
| `SEARCH_MCP_AGENT_API_KEY` **`[SEC]`** | *(empty)* | Credential for `SEARCH_MCP_AGENT_API_BASE_URL` (if needed) |
| `SEARCH_MCP_AGENT_COMMAND` | `claude` / `codex` | Full CLI binary path when MCP host `PATH` is minimal |
| `SEARCH_MCP_AGENT_CLI_ARGS` | backend default | Override CLI flags; for Codex keep `--ignore-user-config` |
| `SEARCH_MCP_AGENT_DEPTH` | `3` | Pages read before calling the model |
| `SEARCH_MCP_AGENT_READ_SECONDS` | `8` | Shared wall budget for those initial reads |
| `SEARCH_MCP_AGENT_MAX_STEPS` | `1` | Follow-up tool rounds after seed pages (`0` = seed only) |
| `SEARCH_MCP_AGENT_MAX_SOURCE_CHARS` | `6000` | Characters of each page handed to the model |
| `SEARCH_MCP_AGENT_TIMEOUT_SECONDS` | `90` | Deadline for the model step |
| `SEARCH_MCP_AGENT_IN_SEARCH` | `true` | Show the answer agent's synthesis above `search` results too |
| `SEARCH_MCP_AGENT_SEARCH_TIMEOUT_SECONDS` | `45` | Max seconds `search` waits for that answer before returning links alone |

### Agent Lane, `codex` Engine & Opt-In `antigravity` Engine

See [CODEX_SEARCH.md](CODEX_SEARCH.md) and [ANTIGRAVITY_SEARCH.md](ANTIGRAVITY_SEARCH.md) *(read Google's ToS warning before enabling `antigravity`)*.

| Variable | Default | Meaning |
|---|---|---|
| `SEARCH_MCP_AGENT_ENGINES` | `auto` | Agent lane: `auto` = signed-in `codex` only (`antigravity` is opt-in), `off`, or explicit list |
| `SEARCH_MCP_AGENT_LANE_WAIT_SECONDS` | `6` | Max seconds `search` waits for the agent lane from fan-out start |
| `SEARCH_MCP_AGENT_LANE_GRACE_SECONDS` | `2` | Extra grace seconds after keyless engines return results |
| `SEARCH_MCP_CODEX_MODEL` | `latest` | Model for `codex` search (`latest` follows plan catalogue; or pin a model name) |
| `SEARCH_MCP_CODEX_TIMEOUT` | `60` | Seconds per `codex` search |
| `SEARCH_MCP_CODEX_REASONING_EFFORT` | `low` | `low` / `medium` / `high` (used only by `/responses` fallback) |
| `SEARCH_MCP_CODEX_BASE_URL` | `https://chatgpt.com/backend-api/codex` | Codex backend URL |
| `SEARCH_MCP_CODEX_AUTO_SIGNIN` | `true` | Open browser sign-in automatically on first explicit use (stdio desktop only) |
| `SEARCH_MCP_CODEX_SIGNIN_WAIT_SECONDS` | `45` | Seconds one search waits for browser approval |
| `CODEX_HOME` | `~/.codex` | Read-only Codex CLI directory used by `free-search-mcp login codex --use-codex-cli` |
| `SEARCH_MCP_ANTIGRAVITY_MODEL` | `latest` | Model for `antigravity` search (`latest` follows account catalogue) |
| `SEARCH_MCP_ANTIGRAVITY_TIMEOUT` | `60` | Seconds per `antigravity` search |
| `SEARCH_MCP_ANTIGRAVITY_BASE_URLS` | daily sandbox, then prod | JSON list of Cloud Code backends tried in order |
| `SEARCH_MCP_ANTIGRAVITY_VERSION` | `2.1.4` | Antigravity release named in User-Agent |
| `SEARCH_MCP_ANTIGRAVITY_CLIENT_ID` | local install lookup | Antigravity OAuth client ID (for hosts without Antigravity installed) |
| `SEARCH_MCP_ANTIGRAVITY_CLIENT_SECRET` **`[SEC]`** | local install lookup | Antigravity OAuth client secret (set both or neither) |

### Browser (P1 Notice) & Optional Credentials

- `SEARCH_MCP_FETCH_STRATEGY` (`auto` / `http`; `browser` logs a one-time P1
  notice and falls back to `http`) and `SEARCH_MCP_BROWSER_*` are accepted
  without error until P1 CDP ships.
- Optional API keys (`*_API_KEY`, `*_TOKEN`, `*_KEY`, `*_SECRET`, `*_USERNAME`,
  `*_CLIENT_ID`) are all `[SEC]` and documented in [API_KEYS.md](API_KEYS.md).

## Renamed & Removed v0.18.x Variables

- **Honored as aliases (+ 1× stderr notice)**:
  - `SEARCH_MCP_CATEGORY_ENGINE_LIMIT` → `SEARCH_MCP_SCOPE_ENGINE_LIMIT`
  - `SEARCH_MCP_RESCUE_ENABLED` → `SEARCH_MCP_HEDGE_ENABLED`
  - `SEARCH_MCP_RESCUE_ENGINES` → appended to `SEARCH_MCP_RESERVE_ENGINES`
- **Ignored (+ 1× stderr notice)**:
  `SEARCH_MCP_TOOLS`, `SEARCH_MCP_RESCUE_TIMEOUT`,
  `SEARCH_MCP_RESCUE_STAGGER_SECONDS`, `SEARCH_MCP_GOOGLE_CSE_*`,
  `SEARCH_MCP_ADMIN_*`, `SEARCH_MCP_SEARX_INSTANCES`.

## Custom User Manifests

You can add personal declarative TOML manifests in
`~/.config/search-mcp/sources.d/*.toml`. User manifests cannot reuse any
built-in or reserved source ID, cannot reference any built-in source's
`auth.env` variable, and are statically checked to forbid unbounded `jaq`
loops (`while`/`until`/`repeat`/`recurse`).
