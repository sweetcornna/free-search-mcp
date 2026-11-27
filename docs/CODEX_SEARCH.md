# Codex: Installing the `free-search` Plugin & Searching on Your ChatGPT Plan (`codex` Engine)

This page covers two separate Codex integrations in `free-search-mcp` 1.0:

1. **Part 1 — Using the `free-search` plugin inside Codex CLI**: Gives Codex
   keyless web search, 500+ specialist sources, academic & retraction
   verification, local document parsing, and sandboxed JS `exec` via exactly
   **three MCP tools** (`ask`, `search`, and `exec`).
2. **Part 2 — Searching on your ChatGPT plan (`codex` engine & agent lane)**:
   Optional OAuth sign-in letting `free-search-mcp` call OpenAI's own web
   search (the same backend Codex uses) against your ChatGPT plan's Codex
   usage, with no API key and no API bill.

You can skip Part 2 completely. Nothing in `free-search-mcp` needs an account,
and no default, reserve, or `scope=` route ever reaches `codex` without a
sign-in.

## 中文速览

- **在 Codex CLI 中安装 `free-search` 插件**：
  ```bash
  codex plugin marketplace add sweetcornna/free-search-mcp
  codex plugin add free-search@free-search-mcp
  ```
  - **避坑（重名遮蔽）**：若 `~/.codex/config.toml` 里曾用 `codex mcp add` 写过
    `[mcp_servers.search]`，它会静默遮蔽插件自带的 `search` 服务器，请先执行
    `codex mcp remove search`。
  - **超时建议**：`exec` 最长支持 120 秒脚本；手动注册时请加上
    `tool_timeout_sec = 150`。
  - **包含内容**：3 个工具（`ask`、`search`、`exec`）、2 个技能（`verified-research`、
    `scholar-research`），以及可选的 Codex 只读子代理文件
    `plugins/free-search/codex/agents/quick_search.toml`。
- **可选：`codex` 搜索引擎（用你自己的 ChatGPT 套餐搜索）**：
  - **用途**：用含 Codex 的 ChatGPT 套餐（Plus / Pro / Business）调用 OpenAI 官方网页搜索，
    不需要 API key，也不产生 API 费用，消耗套餐里的 Codex 额度。
  - **登录**：首次调用 `sources=["codex"]` 而未登录时，桌面 stdio 模式下会自动打开本机浏览器授权页
    （最多等 45 秒；批准后接着完成；不想自动弹出设 `SEARCH_MCP_CODEX_AUTO_SIGNIN=false`）。
    也可以提前运行 `free-search-mcp login codex`（或 `uvx free-search-mcp login codex`）。
  - **复用 Codex CLI 登录**：运行 `free-search-mcp login codex --use-codex-cli`，只读复用
    `$CODEX_HOME/auth.json`（默认 `~/.codex/auth.json`），绝不改写原文件。
  - **SSH 远程机器**：运行 `free-search-mcp login codex --no-browser`；浏览器最终跳转到
    `http://127.0.0.1:1455/auth/callback?code=...` 打不开是正常的，复制地址栏完整 URL 粘回终端回车即可。
  - **Agent 并行通道**：登录后，每次不写 `sources` 的自动搜索都会让 `codex` 与无 key 引擎同时出发，
    结果合并进同一排名；最多等 `SEARCH_MCP_AGENT_LANE_WAIT_SECONDS`（默认 6 秒）且在无 key 引擎出结果后
    最多多等 `SEARCH_MCP_AGENT_LANE_GRACE_SECONDS`（默认 2 秒），来不及的会在后台写入缓存。
    设 `SEARCH_MCP_AGENT_ENGINES=off` 则仅在显式写 `sources=["codex"]` 时运行。
  - **模型与代理**：默认 `SEARCH_MCP_CODEX_MODEL=latest` 跟随套餐模型目录；登录与搜索均走
    `SEARCH_MCP_PROXY`。
  - **Google 对应引擎**：另有一个 `antigravity` 引擎，违反 Google Antigravity 条款，有封号风险
    （连带 Gemini CLI），使用前务必先读 [ANTIGRAVITY_SEARCH.md](ANTIGRAVITY_SEARCH.md)。

---

## Part 1: Installing & Using the `free-search` Plugin in Codex

### 1.1 Install via Codex Plugin Marketplace (Recommended)

```bash
codex plugin marketplace add sweetcornna/free-search-mcp
codex plugin add free-search@free-search-mcp
codex plugin list
```

If you previously registered `search` manually via `codex mcp add search`, that
entry in `~/.codex/config.toml` **silently shadows** the plugin's bundled
`search` server. Remove it first:

```bash
codex mcp remove search
```

What the plugin installs in Codex:
- **MCP server `search`** (`plugins/free-search/.mcp.json` / `mcp.json`): runs
  pinned `uvx free-search-mcp==1.0.1` over stdio, exposing `ask`, `search`, and
  `exec`.
- **Two skills**: `free-search:verified-research` and
  `free-search:scholar-research`.

### 1.2 Manual Registration & `tool_timeout_sec = 150`

If you prefer registering a compiled binary (`~/.local/bin/free-search-mcp`) or
setting `tool_timeout_sec` explicitly in `~/.codex/config.toml`:

```toml
[mcp_servers.search]
command = "/Users/<user>/.local/bin/free-search-mcp"
args = []
tool_timeout_sec = 150
```

*(On Linux, use `/home/<user>/.local/bin/free-search-mcp`. Or run
`codex mcp add search -- uvx free-search-mcp` and add `tool_timeout_sec = 150`.)*

`exec` supports `timeout_s` up to `120` s (emitting `notifications/progress`
every 10 s), so `tool_timeout_sec = 150` gives Codex headroom above the ceiling.

### 1.3 Optional: Install the `quick_search` Custom Agent

```bash
mkdir -p ~/.codex/agents
cp plugins/free-search/codex/agents/quick_search.toml ~/.codex/agents/quick_search.toml
```

`quick_search.toml` sets `sandbox_mode = "read-only"` and
`model_reasoning_effort = "low"`. It starts with one `ask` call, falls back to
`exec` / `search` if needed, never writes files via `web.download`, and returns
a 1–3 sentence sourced answer with dates.

### 1.4 Quick Tool Examples in Codex

- **Quick lookup (no code)**: `ask({ question: "latest stable tokio crate version" })`
- **Discovery / catalog / cache**:
  - `search({ query: "rust 1.98 release notes", freshness: "month" })`
  - `search({ query: "attention is all you need", scope: "academic" })`
  - `search({ query: "patents europe", scope: "catalog" })` *(0 network)*
- **Multi-step reading, scholar, docs & 500+ APIs (`exec`)**:
  ```js
  async () => {
    const [work, integrity] = await Promise.all([
      scholar.work("arXiv:1706.03762"),
      scholar.integrity("10.48550/arXiv.1706.03762"),
    ]);
    return { work, integrity };
  }
  ```

---

## Part 2: Searching on Your ChatGPT Plan (The `codex` Engine)

Once you sign in, `codex` can be called explicitly via `sources: ["codex"]` and
also joins every automatic search as the **agent lane**, fusing OpenAI's web
search into the same ranking as the keyless pool. There is no API key and no
API bill: every search counts against your ChatGPT plan's Codex usage. OpenAI
supports signing in with ChatGPT from third-party tools.

### 2.1 Sign In

The easiest way on a desktop over stdio is to call `search` with
`sources: ["codex"]`: when no sign-in is stored, the server opens the ChatGPT
consent page in your browser and finishes the search as soon as you approve it
(waits up to 45 s, `SEARCH_MCP_CODEX_SIGNIN_WAIT_SECONDS`; consent tab stays
live 10 min; at most one unanswered page per server run; browser output
detached so stdio framing stays clean; disable via
`SEARCH_MCP_CODEX_AUTO_SIGNIN=false`).

To sign in ahead of time:

```bash
free-search-mcp login codex            # local binary install (or `search-mcp login codex`)
uvx free-search-mcp login codex        # plugin / uvx installs
```

Tokens are stored at `~/.config/search-mcp/oauth/codex.json` (`0600`) and
refreshed automatically under a file lock (OpenAI rotates refresh tokens on
every refresh, so locking prevents concurrent servers from invalidating each
other).

```bash
free-search-mcp login status           # signed-in account, plan, token expiry
free-search-mcp login logout codex     # forget stored tokens on this machine
```

#### Over SSH or Headless Hosts

The OAuth callback binds `http://127.0.0.1:1455/auth/callback` (or `1457` if
`1455` is taken). On a remote machine:

```bash
free-search-mcp login codex --no-browser
```

Open the printed URL in any browser and approve it. When the browser fails to
load the `127.0.0.1` callback URL, copy the full URL from the address bar and
paste it back into the terminal.

#### Reusing the Codex CLI's Sign-In (Read-Only)

```bash
free-search-mcp login codex --use-codex-cli                  # reads $CODEX_HOME/auth.json or ~/.codex/auth.json
free-search-mcp login codex --use-codex-cli /path/to/auth.json
```

This stores a read-only link rather than copying tokens. `free-search-mcp`
never refreshes or rewrites Codex CLI's `auth.json` (which would rotate the
CLI's refresh token and sign the CLI out). Running `codex` once refreshes an
expired linked token. For an independent self-refreshing login, run
`free-search-mcp login codex` without `--use-codex-cli`.

### 2.2 Search Explicitly & The Agent Lane

Explicit search:

```json
{"query": "rust 2024 edition changes", "sources": ["codex"], "freshness": "month"}
```

Filters (`freshness`, `domains`) are forwarded to OpenAI's search and re-checked
on the returned hits. Results are cached like any other engine. Naming `codex`
before signing in returns a clear error explaining keyless alternatives without
asking the user for a key or login.

#### The Agent Lane

With a sign-in stored, every automatic search (no `sources` specified, and not
an exclusive `media`/`dataset`/`cache`/`catalog` scope) dispatches `codex` at
the same moment as the keyless pool and fuses its hits into the same RRF
ranking:

- **Wait & grace timing**: Waits at most `SEARCH_MCP_AGENT_LANE_WAIT_SECONDS`
  (`6` s from fan-out start) and at most `SEARCH_MCP_AGENT_LANE_GRACE_SECONDS`
  (`2` s) after the keyless pool has returned hits. When keyless engines find
  nothing, the wait stretches to `SEARCH_MCP_SEARCH_DEADLINE_SECONDS` (`8` s).
- **Background cache merge**: A lane engine still running when the wait expires
  is not cancelled; it finishes in the background and merges into the cached
  result set so a repeat search returns its hits.
- **Rate limit & benching**: The lane never stalls for a rate-limit token (past
  10 searches/min, `codex` sits out that search). A spent usage window or
  refused auth benches it immediately (10 min doubling to 60 min); errors are
  reported under agent-lane diagnostics and never affect keyless results.
- **Disable the lane**: Set `SEARCH_MCP_AGENT_ENGINES=off` so `codex` runs only
  when explicitly named in `sources`.

### 2.3 Settings

| Variable | Default | Meaning |
|---|---|---|
| `SEARCH_MCP_CODEX_MODEL` | `latest` | Model for `codex` search (`latest` follows plan catalogue; or pin a model name) |
| `SEARCH_MCP_CODEX_TIMEOUT` | `60` | Timeout in seconds for one `codex` search |
| `SEARCH_MCP_CODEX_REASONING_EFFORT` | `low` | `low` / `medium` / `high` (used only by `/responses` fallback) |
| `SEARCH_MCP_CODEX_BASE_URL` | `https://chatgpt.com/backend-api/codex` | Codex backend URL |
| `SEARCH_MCP_CODEX_AUTO_SIGNIN` | `true` | Auto-open browser sign-in on first explicit use (stdio desktop only) |
| `SEARCH_MCP_CODEX_SIGNIN_WAIT_SECONDS` | `45` | Seconds one search waits for browser approval |
| `CODEX_HOME` | `~/.codex` | Directory read by `--use-codex-cli` (`auth.json`, read-only) |
| `SEARCH_MCP_AGENT_ENGINES` | `auto` | Agent lane: `auto` (signed-in `codex` only), `off`, or explicit list |
| `SEARCH_MCP_AGENT_LANE_WAIT_SECONDS` | `6` | Max seconds `search` waits for the agent lane |
| `SEARCH_MCP_AGENT_LANE_GRACE_SECONDS` | `2` | Extra grace seconds after keyless engines return results |

Sign-in and searches use `SEARCH_MCP_PROXY` when set.

### 2.4 How the `codex` Engine Works

1. Calls `POST /alpha/search` on the Codex backend for structured search hits
   (title, URL, snippet directly from OpenAI's search).
2. Falls back to `POST /responses` with the hosted `web_search` tool when a
   deployment lacks `/alpha/search`, keeping only URLs attached as citations
   (model-fabricated URLs are dropped).
3. With `SEARCH_MCP_CODEX_MODEL=latest`, checks `GET /models` at most every 6 h
   and selects the newest generation's lightest search-capable non-retired
   model.

This is an undocumented backend that OpenAI may change at any time; if it
breaks, the engine reports an error and keyless engines continue unaffected.
For Google's counterpart (`antigravity`), read the risk warning in
[ANTIGRAVITY_SEARCH.md](ANTIGRAVITY_SEARCH.md) first.
