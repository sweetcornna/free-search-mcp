# Delegating a Lookup (`ask`, Answer Agent & Host Subagents)

An agent can call `search` and `exec` itself for deep research, or use `ask`
and host subagents when you want a short, dated, sourced answer without loading
raw page text into the caller's context. All routes work keyless for search,
share the same anti-injection and date-verification discipline, and never ask a
user for an API key.

| You want | Use | Needs |
|---|---|---|
| One-call quick lookup with **no LLM** | `ask({ question, mode: "fast" })` (default when `SEARCH_MCP_AGENT_BACKEND=off`) | nothing |
| Host's own subagents to run a lookup | `quick-search` (`agents/quick-search.md` / `codex/agents/quick_search.toml`) | Claude Code, Codex, or Antigravity |
| Server-side answer agent via Claude Code | `SEARCH_MCP_AGENT_BACKEND=claude-code` | logged-in `claude` CLI |
| Server-side answer agent via Codex | `SEARCH_MCP_AGENT_BACKEND=codex` | logged-in `codex` CLI |
| Server-side answer agent via any API | `SEARCH_MCP_AGENT_BACKEND=api` | OpenAI- or Anthropic-compatible URL (e.g. Ollama) |
| Shell / script / HTTP quick lookup | `free-search-mcp ask "<question>"` or `POST/GET /v1/ask` | local binary (or Streamable HTTP) |

---

## 1. The `ask` Tool (Always Registered)

Unlike v0.18.x (where `ask` was conditional), 1.0 **always registers** `ask`:

```json
{
  "question": "latest stable tokio crate version and release date",
  "freshness": "month",
  "domains": ["crates.io", "github.com"],
  "lang": "en",
  "mode": "auto",
  "max_tokens": 1200
}
```

- **`mode: "fast"` (no LLM)**: Checks direct-answer sources first (registries,
  CVE, FX, weather, clock, holidays); otherwise runs web search, fetches top
  pages in parallel within a ~8 s deadline, and returns the best dated passages
  with source URLs.
- **`mode: "agent"`**: Runs the server-side answer agent (ported from Python
  `agent.py`) over the fetched evidence to write a concise sourced answer. If
  the model fails or hits its deadline, `ask` degrades cleanly to returning the
  evidence passages with the reason so the search is never wasted.
- **`mode: "auto"` (default)**: Uses `"agent"` when `SEARCH_MCP_AGENT_BACKEND`
  is configured (`api`, `claude-code`, or `codex`), else `"fast"`.

The same functionality is reachable from:
- Inside `exec`: `await web.ask(question, { freshness?, domains?, lang?, mode?, maxTokens? })`
- CLI: `free-search-mcp ask "<question>"` (or `search-mcp ask "<question>"`)
- Streamable HTTP: `POST /v1/ask` and `GET /v1/ask?q=...` (protected by the
  same `SEARCH_MCP_HTTP_TOKEN` and `Host`/`Origin` guards as `/mcp`)

---

## 2. Server-Side Answer Agent (`SEARCH_MCP_AGENT_BACKEND`)

Setting a backend activates `"agent"` mode for `ask` (and `mode="auto"`), and
also runs the answer agent alongside every automatic `search` call so a concise
sourced answer appears above the result links (`SEARCH_MCP_AGENT_IN_SEARCH=true`
by default; set `false` to restrict the answer agent to `ask` only).

| Variable | Default | Meaning |
|---|---|---|
| `SEARCH_MCP_AGENT_BACKEND` | `off` | `off` \| `api` \| `claude-code` \| `codex` |
| `SEARCH_MCP_AGENT_MODEL` | *(empty)* | Required for `api`; empty means `haiku` on Claude Code and the CLI's default on Codex |
| `SEARCH_MCP_AGENT_API_PROTOCOL` | `openai` | `openai` (`/chat/completions`) or `anthropic` (`/v1/messages`) |
| `SEARCH_MCP_AGENT_API_BASE_URL` | *(empty)* | Endpoint base URL (e.g. `http://localhost:11434/v1` for local Ollama) |
| `SEARCH_MCP_AGENT_API_KEY` **`[SEC]`** | *(empty)* | Credential for that API endpoint (none needed for local Ollama) |
| `SEARCH_MCP_AGENT_COMMAND` | `claude` / `codex` | Full path to CLI when host MCP `PATH` is minimal |
| `SEARCH_MCP_AGENT_CLI_ARGS` | backend default | Override CLI flags; for Codex keep `--ignore-user-config` so child runs do not load user plugins recursively |
| `SEARCH_MCP_AGENT_DEPTH` | `3` | Pages read before calling the model |
| `SEARCH_MCP_AGENT_READ_SECONDS` | `8` | Shared wall budget for those initial page reads |
| `SEARCH_MCP_AGENT_MAX_STEPS` | `1` | Follow-up tool rounds after seed pages (`0` = seed pages only) |
| `SEARCH_MCP_AGENT_MAX_SOURCE_CHARS` | `6000` | Characters of each page handed to the model |
| `SEARCH_MCP_AGENT_TIMEOUT_SECONDS` | `90` | Deadline for the model step |
| `SEARCH_MCP_AGENT_IN_SEARCH` | `true` | Show the answer agent's synthesis above `search` results too |
| `SEARCH_MCP_AGENT_SEARCH_TIMEOUT_SECONDS` | `45` | Max seconds `search` waits for that answer before returning links alone |

---

## 3. Host Subagents Included in `plugins/free-search`

All three client definitions share the same core prompt, safety rules, and
reply format (starts with one `ask` call, falls back to at most two `exec` /
`search` calls if needed, never calls `web.download`, never asks for API keys,
treats fetched text as untrusted data):

### Claude Code (`plugins/free-search/agents/quick-search.md`)

Installed automatically by the plugin as **`free-search:quick-search`**:
- Restricted strictly to the plugin server's three tools:
  `mcp__plugin_free-search_search__ask`,
  `mcp__plugin_free-search_search__search`, and
  `mcp__plugin_free-search_search__exec`.
- `maxTurns: 6`, `omitClaudeMd: true`, `model: inherit` (set
  `CLAUDE_CODE_SUBAGENT_MODEL=haiku` to pin Claude Code subagents globally to
  Haiku).

### Codex (`plugins/free-search/codex/agents/quick_search.toml`)

```bash
mkdir -p ~/.codex/agents
cp plugins/free-search/codex/agents/quick_search.toml ~/.codex/agents/quick_search.toml
```

Configured with `sandbox_mode = "read-only"` and `model_reasoning_effort = "low"`.
See [CODEX_SEARCH.md](CODEX_SEARCH.md).

### Antigravity (`agy`)

Antigravity automatically discovers `plugins/free-search/agents/quick-search.md`
(`model: inherit`) when the plugin is enabled. To force quick lookups onto
Antigravity's `flash` tier, copy the standalone Flash variant:

```bash
mkdir -p ~/.gemini/config/agents
cp plugins/free-search/antigravity/agents/quick-search.md ~/.gemini/config/agents/quick-search.md
```

See [ANTIGRAVITY_SEARCH.md](ANTIGRAVITY_SEARCH.md).
