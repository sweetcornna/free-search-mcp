# Antigravity (`agy`): Installing the `free-search` Plugin & the Opt-In `antigravity` Engine

This page covers two separate Antigravity topics in `free-search-mcp` 1.0:

1. **Part 1 — Installing & Using the `free-search` plugin inside Antigravity
   (`agy` CLI & IDE)**: Completely keyless, ToS-safe, first-class plugin setup
   providing **three MCP tools** (`ask`, `search`, `exec`), two skills,
   `rules/AGENTS.md`, and `quick-search` agents.
2. **Part 2 — Optional, at your own risk: Google Search on an Antigravity
   sign-in (the `antigravity` engine)**: Calling Google Search via Antigravity
   OAuth. **Read the account-suspension warning in Part 2 before touching it.**

## 中文速览

- **第一部分：在 Antigravity (`agy`) 中安装 `free-search` 插件（推荐、完全无 key、无封号风险）**：
  ```bash
  agy plugin install https://github.com/sweetcornna/free-search-mcp/tree/main/plugins/free-search
  # 或本地克隆：
  agy plugin install ./plugins/free-search
  ```
  - 验证：`agy plugin validate ./plugins/free-search --strict`。
  - 手动注册 MCP：`agy mcp add search uvx free-search-mcp`（或本地 `~/.local/bin/free-search-mcp`）。
  - 包含：`mcp_config.json`（服务器键 `"search"`，3 个工具 `ask`、`search`、`exec`）、
    `skills/{verified-research,scholar-research}/SKILL.md`、`rules/AGENTS.md`、
    `agents/quick-search.md`（另附 Flash 固定版 `antigravity/agents/quick-search.md`）。
- **第二部分：可选、风险自担：`antigravity` 搜索引擎（用 Antigravity 登录调用 Google 搜索）**：
  - **封号警告**：Google 的 Antigravity 条款明确禁止第三方工具使用 Antigravity OAuth 登录，
    已有账号因此被封（连带 Gemini CLI）。该引擎不会自动弹出登录页，也不会在 `auto` 并行通道中启用；
    仅用你愿意承担封号风险的账号操作。
  - **登录**：`free-search-mcp login antigravity`（或 `uvx free-search-mcp login antigravity`），
    先打印风险警告再打开 Google 授权页。
  - **OAuth 客户端**：仓库不包含客户端 ID/密钥。优先读 `SEARCH_MCP_ANTIGRAVITY_CLIENT_ID` 与
    `SEARCH_MCP_ANTIGRAVITY_CLIENT_SECRET`，未设时从本机 Antigravity 安装包读取；没装 Antigravity
    的机器需手动设置这两项。
  - **使用**：显式传 `sources=["antigravity"]`（或手动设 `SEARCH_MCP_AGENT_ENGINES=codex,antigravity`
    加入并行通道）。SSH 下用 `--no-browser`，把浏览器跳转失败后的 `http://localhost:51121/oauth-callback?code=...`
    完整地址粘回终端即可。
  - **退出登录**：`free-search-mcp login logout antigravity` 删除本地 token；彻底撤销请到 Google
    账号的「第三方应用和服务」里移除 Antigravity。

---

## Part 1: Installing & Using the `free-search` Plugin in Antigravity (`agy`)

Antigravity natively discovers plugins containing `plugin.json`,
`mcp_config.json`, `skills/<name>/SKILL.md`, `rules/AGENTS.md`, and
`agents/<name>.md`.

### 1.1 Install with `agy plugin install` (Recommended)

```bash
agy plugin install https://github.com/sweetcornna/free-search-mcp/tree/main/plugins/free-search
# or from a local clone:
agy plugin install ./plugins/free-search
agy plugin list
```

Validate without installing:

```bash
agy plugin validate ./plugins/free-search --strict
```

Or wire via `.agents/plugins.json` (project) / `~/.gemini/config/plugins.json` (global):

```json
{
  "entries": [
    {
      "path": "/absolute/path/to/free-search-mcp/plugins",
      "include_only": ["free-search"]
    }
  ]
}
```

### 1.2 What Antigravity Loads from `plugins/free-search/`

| Path | Antigravity role |
|---|---|
| `plugin.json` | Plugin marker & metadata (`name: "free-search"`, `version: "1.0.1"`) |
| `mcp_config.json` | Registers the `"search"` stdio MCP server (`uvx free-search-mcp==1.0.1`, exposing `ask`, `search`, `exec`) |
| `skills/verified-research/SKILL.md` | On-demand skill for primary-source fact verification (`ask` for quick lookups; `search` + `exec` for corroborated verification) |
| `skills/scholar-research/SKILL.md` | On-demand skill for academic literature search, citation graphs, Retraction Watch integrity, lawful OA full text, and citations |
| `rules/AGENTS.md` | Concise standing rule (~1.5 KB) enforcing 3-tool discipline (`ask` / `search` / `exec`), primary-page verification, anti-injection, and no-key rules |
| `agents/quick-search.md` | Shared fast lookup subagent (`model: inherit`, uses `ask` first, cross-client valid in both Antigravity and Claude Code) |
| `antigravity/agents/quick-search.md` | Optional standalone Antigravity agent pinned to `model: flash` (copy to `.agents/agents/` or `~/.gemini/config/agents/`) |

### 1.3 Manual MCP Registration (Local Binary or `uvx`)

```bash
agy mcp add search /Users/<user>/.local/bin/free-search-mcp
# or:
agy mcp add search uvx free-search-mcp
agy mcp list
```

Or in `~/.gemini/config/mcp_config.json`:

```json
{
  "mcpServers": {
    "search": {
      "command": "/Users/<user>/.local/bin/free-search-mcp",
      "args": []
    }
  }
}
```

### 1.4 Using `ask`, `search`, and `exec` in Antigravity

- **Quick lookup (`ask`)**: `ask({ question: "tokio graceful shutdown signal example", freshness: "year" })`
- **Discovery / catalog / cache (`search`)**:
  - `search({ query: "flashattention 3", scope: "academic.cs" })`
  - `search({ query: "clinical trials", scope: "catalog" })` *(0 network)*
  - `search({ query: "graceful shutdown", scope: "cache" })` *(0 network)*
- **Multi-step reading, scholar, docs & specialist APIs (`exec`)**:
  ```js
  async () => {
    const hits = await web.search("EU AI Act compliance timeline", { scope: "gov", freshness: "year" });
    const pages = await web.fetchBatch(hits.results.slice(0, 3).map((h) => h.url));
    return pages.map((p) => ({ title: p.title, url: p.url, date: p.publishedDate }));
  }
  ```

---

## Part 2: Optional, At Your Own Risk — Google Search on an Antigravity Sign-In (The `antigravity` Engine)

> **WARNING — READ BEFORE SIGNING IN**: Google's Antigravity terms say:
> *"Using third party software, tools, or services to access the Service (e.g.
> using OpenClaw with Antigravity OAuth) is a breach of this Agreement. Such
> actions may be grounds for suspension or termination of your Antigravity
> and/or Gemini CLI accounts."* Google has suspended accounts for this.
>
> The `antigravity` engine signs in with Antigravity's own OAuth client and
> sends Antigravity's User-Agent. Nothing in `free-search-mcp` needs it: no
> default, reserve, or `scope=` route ever reaches it, it **never** opens a
> sign-in page by itself, and `SEARCH_MCP_AGENT_ENGINES=auto` **never** seats
> it. If you do not accept the account-suspension risk, do not sign in.

### 2.1 Sign In & Sign Out

```bash
free-search-mcp login antigravity            # prints the warning above, then opens Google consent
uvx free-search-mcp login antigravity        # plugin / uvx installs
free-search-mcp login status                 # account, tier, token expiry
free-search-mcp login logout antigravity     # delete local tokens
```

Tokens are saved to `~/.config/search-mcp/oauth/antigravity.json` (`0600`) and
refreshed automatically. Logging out deletes local tokens only; to revoke the
grant itself, remove Antigravity under "Third-party apps and services" in your
Google account.

#### Antigravity's OAuth Client

This repository does **not** embed Antigravity's OAuth client ID or secret.
Sign-in reads `SEARCH_MCP_ANTIGRAVITY_CLIENT_ID` and
`SEARCH_MCP_ANTIGRAVITY_CLIENT_SECRET` if set; otherwise it reads them from the
local Antigravity installation (matching Antigravity's client by SHA-256
fingerprint). On a Mac with Antigravity installed, nothing needs to be set. On
a host without Antigravity, set both variables (values can be copied from
`~/.config/search-mcp/oauth/antigravity.json` on a signed-in machine).

#### Over SSH or Headless Hosts

The callback URI is fixed at `http://localhost:51121/oauth-callback`. Run
`free-search-mcp login antigravity --no-browser`, complete consent in any
browser, and paste the failed `localhost:51121` redirect URL back into the
terminal.

### 2.2 Search Explicitly & Opt-In Agent Lane

```json
{"query": "rust 2024 edition changes", "sources": ["antigravity"], "freshness": "month"}
```

To include `antigravity` in the automatic agent lane alongside `codex` (every
automatic search then spends Antigravity quota and generates Google-visible
traffic), opt in explicitly:

```dotenv
SEARCH_MCP_AGENT_ENGINES=codex,antigravity
```

Because `antigravity` typically takes ~10 s (longer than the 6 s lane wait),
its hits usually merge into the cached result set in the background for a
subsequent identical search.

### 2.3 Settings

| Variable | Default | Meaning |
|---|---|---|
| `SEARCH_MCP_ANTIGRAVITY_MODEL` | `latest` | Model running search (`latest` follows account catalogue: `tieredModelIds.flash`, fallback `webSearchModelIds`) |
| `SEARCH_MCP_ANTIGRAVITY_TIMEOUT` | `60` | Timeout in seconds per `antigravity` search |
| `SEARCH_MCP_ANTIGRAVITY_BASE_URLS` | daily sandbox, then production | JSON list of Cloud Code backends tried in order |
| `SEARCH_MCP_ANTIGRAVITY_VERSION` | `2.1.4` | Antigravity release reported in the User-Agent (update if backend returns 403 license error) |
| `SEARCH_MCP_ANTIGRAVITY_CLIENT_ID` | read from local Antigravity install | OAuth client ID for hosts without Antigravity |
| `SEARCH_MCP_ANTIGRAVITY_CLIENT_SECRET` **`[SEC]`** | read from local Antigravity install | OAuth client secret; set both or neither |

Sign-in and searches route through `SEARCH_MCP_PROXY` when set.

### 2.4 How the `antigravity` Engine Works

1. Sends `POST {base}/v1internal:generateContent` with the `googleSearch` tool
   and Antigravity's User-Agent (`antigravity/<version> <os>/<arch>`), trying
   the daily sandbox host first and production (`cloudcode-pa.googleapis.com`)
   second.
2. Extracts grounded citations using UTF-8 byte offsets in `groundingMetadata`
   and resolves `vertexaisearch.cloud.google.com` redirects in parallel (5 s
   per-link timeout); URLs written only in model text are dropped.
3. Retries once if the model answers from memory without invoking Google
   Search; a second ungrounded reply yields empty results rather than
   hallucinated links.

This is an undocumented backend that Google may change at any time; failures
never affect keyless engines.
