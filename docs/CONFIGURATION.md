# Configuration

All settings can be overridden by environment variables prefixed with
`SEARCH_MCP_`. They can live in three places. In order of precedence, highest
first, these are real environment variables, then `./.env` in the launch
directory (source checkouts), then `~/.config/search-mcp/.env` (the stable
location for uvx/PyPI installs; the directory can be changed with
`SEARCH_MCP_CONFIG_DIR`).

Nothing here is required. The setting people most often change is the proxy
(`SEARCH_MCP_PROXY`, see
[docs/PROXY_AND_GATES.md](PROXY_AND_GATES.md)).

Available settings:

| Var | Default | Meaning |
|---|---|---|
| `SEARCH_MCP_DEFAULT_ENGINES` | `["duckduckgo","bing","anysearch","mojeek"]` | JSON list; the pool a search uses when it names no engines |
| `SEARCH_MCP_RESERVE_ENGINES` | `["so360","brave","searx"]` | stand-ins, in order, seated only while fewer than `MIN_HEALTHY_ENGINES` general engines are healthy |
| `SEARCH_MCP_MIN_HEALTHY_ENGINES` | `3` | below this many healthy general engines, reserves are seated |
| `SEARCH_MCP_FRESH_ENGINES` | `["googlenews"]` | joined for `freshness="day"` / `"week"` |
| `SEARCH_MCP_LOCALE_ENGINES` | `{"zh":["so360"]}` | JSON object: engines joined when the QUERY is written in that language |
| `SEARCH_MCP_HEALTH_ENABLED` | `true` | the circuit breaker; `false` asks every engine every time |
| `SEARCH_MCP_ENGINE_COOLDOWN_SECONDS` | `600` | first bench; doubles on each repeat |
| `SEARCH_MCP_ENGINE_COOLDOWN_MAX_SECONDS` | `3600` | ceiling for the doubling |
| `SEARCH_MCP_COHERENCE_GUARD_ENABLED` | `true` | drop a web engine's bucket when it is off-topic and another engine proves the query's words do get echoed |
| `SEARCH_MCP_SEARX_INSTANCES` | *(empty)* | pin known-good SearXNG instance URL(s), comma/space separated; overrides the built-in shortlist |
| `SEARCH_MCP_RESCUE_ENABLED` | `true` | auto-rescue empty searches via rescue engines |
| `SEARCH_MCP_RESCUE_ENGINES` | `["searx","bing"]` | rescue order (JSON list), tried after the reserves |
| `SEARCH_MCP_RESCUE_TIMEOUT` | `10.0` | seconds; cap on the whole rescue pass |
| `SEARCH_MCP_MAX_RESULTS_PER_ENGINE` | `10` | |
| `SEARCH_MCP_RATE_LIMIT_PER_MINUTE` | `30` | per engine |
| `SEARCH_MCP_FETCH_RATE_LIMIT_PER_MINUTE` | `20` | shared `fetch` bucket |
| `SEARCH_MCP_CACHE_DIR` | `~/.cache/search-mcp` | |
| `SEARCH_MCP_CACHE_TTL_SECONDS` | `604800` | 7 days. Searches that asked for recency expire sooner regardless: `freshness=day` 1h, `week` 6h, `month` 24h, `category=news` 6h |
| `SEARCH_MCP_CACHE_MAX_MB` | `512` | size cap on the cache file; `0` disables |
| `SEARCH_MCP_FETCH_STRATEGY` | `auto` | `auto` / `http` / `browser` |
| `SEARCH_MCP_BROWSER_HEADLESS` | `true` | |
| `SEARCH_MCP_BROWSER_POOL_SIZE` | `2` | concurrent pages |
| `SEARCH_MCP_MAX_CONTENT_CHARS` | `50000` | per result truncation |
| `SEARCH_MCP_USER_AGENT` | desktop Chrome UA | used by the httpx (documents) and Playwright paths; the curl_cffi path derives its UA from browser impersonation |
| `SEARCH_MCP_DOWNLOAD_ENABLED` | `true` | set `false` to disable local file downloads |
| `SEARCH_MCP_DOWNLOAD_DIR` | `${SEARCH_MCP_CACHE_DIR}/downloads` | optional directory override; unset or blank uses the dynamic default (`/data/downloads` in Docker) |
| `SEARCH_MCP_DOWNLOAD_TTL_HOURS` | `24` | downloaded files are deleted after this; `0` keeps them forever |
| `SEARCH_MCP_DOWNLOAD_MAX_MB` | `100` | second-layer save cap; effective remote cap is the smaller of this and `SEARCH_MCP_MAX_RESPONSE_BYTES` (25,000,000 bytes by default) |
| `SEARCH_MCP_CATEGORY_ENGINE_LIMIT` | `3` | how many category-native engines `category=` may add |
| `SEARCH_MCP_AUTO_ROUTE_ENABLED` | `true` | without a category, seat the record sources that claim the question (see [Direct facts](HOW_IT_WORKS.md#direct-facts-records-instead-of-snippets)) |
| `SEARCH_MCP_CLAIM_ENGINE_LIMIT` | `3` | how many claiming record sources may join a search |
| `SEARCH_MCP_SEARCH_DEADLINE_SECONDS` | `10` | wait this long for the slowest engine once one has answered; `0` waits for all |
| `SEARCH_MCP_CONTACT_EMAIL` | *(empty)* | optional; routes OpenAlex/Crossref/NCBI into their faster identified-caller pools |
| `SEARCH_MCP_TRANSPORT` | `stdio` | `stdio` / `streamable-http` |
| `SEARCH_MCP_HTTP_HOST` | `127.0.0.1` | streamable-http bind address |
| `SEARCH_MCP_HTTP_PORT` | `8000` | streamable-http port |
| `SEARCH_MCP_HTTP_PATH` | `/mcp` | streamable-http endpoint path |
| `SEARCH_MCP_HTTP_ALLOWED_ORIGINS` | *(empty)* | extra `Origin` values the DNS-rebinding guard accepts, comma/space separated (loopback is always allowed) |
| `SEARCH_MCP_PROXY` | *(empty)* | outbound proxy (`http` / `https` / `socks5`, optional `user:pass@`) for the HTTP engines, the browser and `fetch` |
| `SEARCH_MCP_PROXY_ENGINES` | *(empty)* | route only these engines through the proxy; blank = all |
| `SEARCH_MCP_REGION` | `us-en` | `cc-lang` token; a query's own script still decides the locale engines |
| `SEARCH_MCP_TOOLS` | *(empty)* | comma/space separated allow-list of tool names; empty registers all of them |
| `SEARCH_MCP_TOOL_CALL_BUDGET` | `0` | tool calls this process runs before it answers "budget used up" instead; `0` = no cap. Meant for a server started for one job |
| `SEARCH_MCP_AGENT_*` | off | the optional answer agent; see [DELEGATION.md](DELEGATION.md) |
| `SEARCH_MCP_CODEX_MODEL` | `latest` | model for the opt-in `codex` engine (`latest` follows the plan's catalogue); see [CODEX_SEARCH.md](CODEX_SEARCH.md) |
| `SEARCH_MCP_CODEX_TIMEOUT` | `60` | seconds for one `codex` search |
| `SEARCH_MCP_CODEX_REASONING_EFFORT` | `low` | `low` / `medium` / `high`; used only when the backend has no search endpoint |
| `SEARCH_MCP_CODEX_BASE_URL` | `https://chatgpt.com/backend-api/codex` | the Codex backend |
| `SEARCH_MCP_CODEX_AUTO_SIGNIN` | `true` | named with no sign-in stored, open the sign-in page in the local browser and finish the search once it is approved (stdio on a desktop only) |
| `SEARCH_MCP_CODEX_SIGNIN_WAIT_SECONDS` | `45` | how long one search waits for that approval |
| `SEARCH_MCP_ANTIGRAVITY_MODEL` | `latest` | model for the opt-in `antigravity` engine (`latest` follows the account's catalogue); see [ANTIGRAVITY_SEARCH.md](ANTIGRAVITY_SEARCH.md) |
| `SEARCH_MCP_ANTIGRAVITY_TIMEOUT` | `60` | seconds for one `antigravity` search |
| `SEARCH_MCP_ANTIGRAVITY_BASE_URLS` | the daily sandbox, then production | JSON list of Cloud Code backends, tried in order |
| `SEARCH_MCP_ANTIGRAVITY_VERSION` | `2.1.4` | the Antigravity release the user agent names |
| `SEARCH_MCP_ANTIGRAVITY_CLIENT_ID` | read from the Antigravity install | Antigravity's OAuth client ID, for a machine without Antigravity |
| `SEARCH_MCP_ANTIGRAVITY_CLIENT_SECRET` | read from the Antigravity install | its client secret; set both or neither |
