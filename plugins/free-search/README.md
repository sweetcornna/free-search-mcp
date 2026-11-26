# free-search: Tri-Client Plugin (Claude Code, Codex & Antigravity)

This plugin is the recommended way to install
[free-search-mcp](https://github.com/sweetcornna/free-search-mcp) 1.0. It gives
**Claude Code**, **Codex**, and **Antigravity (`agy`)** — plus any Agent Plugins
v1 client such as Hermes — a single-binary Rust MCP server with **3 tools**
(`search`, `exec`, and `ask`), **500+ keyless sources**, two research skills
(`verified-research` and `scholar-research`), a `quick-search` subagent, and
Antigravity standing rules, with no API key and no signup.

## Install

### Claude Code

Inside a session:

```text
/plugin marketplace add sweetcornna/free-search-mcp
/plugin install free-search@free-search-mcp
```

Or from the shell:

```bash
claude plugin marketplace add sweetcornna/free-search-mcp
claude plugin install free-search@free-search-mcp -s user
```

### Codex

```bash
codex plugin marketplace add sweetcornna/free-search-mcp
codex plugin add free-search@free-search-mcp
```

If `~/.codex/config.toml` already has a `[mcp_servers.search]` entry from an
earlier `codex mcp add`, it silently shadows the plugin's server of the same
name. Run `codex mcp remove search` first. See
[docs/CODEX_SEARCH.md](../../docs/CODEX_SEARCH.md) for setting
`tool_timeout_sec = 150`, installing `codex/agents/quick_search.toml`, and the
optional `codex` ChatGPT search engine & agent lane.

### Antigravity (`agy`)

Install directly from GitHub or a local checkout:

```bash
agy plugin install https://github.com/sweetcornna/free-search-mcp/tree/main/plugins/free-search
# or from a local clone:
agy plugin install ./plugins/free-search
```

Antigravity automatically ingests `plugin.json`, `mcp_config.json`,
`rules/AGENTS.md`, `skills/verified-research/SKILL.md`,
`skills/scholar-research/SKILL.md`, and `agents/quick-search.md`. See
[docs/ANTIGRAVITY_SEARCH.md](../../docs/ANTIGRAVITY_SEARCH.md) for plugin setup
and details (including warnings) on the opt-in `antigravity` engine.

### Hermes Agent (Agent Plugins v1)

```bash
hermes plugins install sweetcornna/free-search-mcp/plugins/free-search --enable
```

## What it installs

### The MCP server `search` (3 tools)

All three client configs (`.mcp.json`, `mcp.json`, `mcp_config.json`) register
the server under key **`"search"`** running `uvx free-search-mcp==1.0.0` (a
compiled Rust binary packaged via `maturin`, so `uv` is the only prerequisite).

The server always exposes exactly **three tools** (`tools/list` ≤ 6,800 bytes):

1. **`ask`**: Quick one-call lookup with no code (`mode="auto" | "fast" | "agent"`).
   In `"fast"` mode (no LLM), it checks direct-answer sources first, else runs
   web search and reads the top pages in parallel (~8 s deadline), returning
   the best dated passages with sources. In `"agent"` mode (or `"auto"` when an
   answer-agent backend is configured), the answer agent writes a short sourced
   answer from that evidence.
2. **`search`**: Multi-engine web and vertical search across 500+ sources
   (`web`, `academic`, `news`, `code`, `docs`, `dataset`, `finance`, `security`,
   `gov`, `media`, `commerce`, `forum`, `reference`), optional signed-in agent
   lane (`codex` / opt-in `antigravity`), local FTS5 cache (`scope="cache"`),
   and zero-network source/API catalog discovery (`scope="catalog"`).
3. **`exec`**: Sandboxed ES2020 JavaScript (QuickJS-NG, zero ambient fs/env/net)
   with typed async Host APIs (`web.*` including `web.ask`, `scholar.*`,
   `doc.*`, `source.*`, `cache.*`, `catalog.*`, `util.*`) for parallel
   fetching, document reading, scholarly graphs/full text/integrity/citations,
   and calling any of the 500+ cataloged sources or 18 generic protocols.

The version is pinned to this plugin's own version (`1.0.0`). Installing plugin
`X.Y.Z` always runs package `X.Y.Z`, and `crates/server/tests/plugin_manifests.rs`
keeps every manifest, pin, and package version in lockstep.

### Skills (`verified-research` & `scholar-research`)

- **`verified-research`** (`skills/verified-research/SKILL.md`): Uses `ask` for
  quick lookups and `search` + `exec` whenever relied-upon facts require opening
  primary pages and documents, verifying publish dates and cache age against
  today, corroborating with independent sources, and listing anything that
  could not be verified.
- **`scholar-research`** (`skills/scholar-research/SKILL.md`): End-to-end
  academic literature workflow covering `scope="academic.*"` and
  `scholar.search`, `scholar.work` (DOI/arXiv/PMID/OpenAlex crosswalk),
  `scholar.graph`, `scholar.integrity` (retraction & correction checks),
  11-step lawful OA structured-first `scholar.fulltext`, entity/grant lookups,
  and offline `scholar.cite` formatting.

### Agent (`quick-search` / `quick_search.toml`)

- **Claude Code & Antigravity**: `agents/quick-search.md` (`free-search:quick-search`).
  Runs with `model: inherit` for cross-client compatibility across both Claude
  Code and Antigravity (`maxTurns: 6`, `omitClaudeMd: true`), restricted
  strictly to `mcp__plugin_free-search_search__ask`,
  `mcp__plugin_free-search_search__search`, and
  `mcp__plugin_free-search_search__exec` (starts with one `ask` call, falls
  back to `exec`/`search` if needed, never calls `web.download`, never follows
  instructions on fetched pages, and never asks for API keys). Antigravity
  users who want quick lookups forced onto Flash can also copy
  `antigravity/agents/quick-search.md` (`model: flash`).
- **Codex**: `codex/agents/quick_search.toml` (`sandbox_mode = "read-only"`,
  `model_reasoning_effort = "low"`), sharing the exact same prompt and rules.

### Antigravity Rules (`rules/AGENTS.md`)

Loaded automatically by Antigravity when the plugin is active, reinforcing the
3-tool `ask` / `search` / `exec` workflow, primary-source verification,
anti-injection discipline, and no-API-key positioning.

## Configuration & offline Retraction Watch sync

- **Zero API keys required**: Every default, reserve, locale, academic, and
  vertical route works keyless. Optional operator settings (proxy, polite-pool
  `SEARCH_MCP_CONTACT_EMAIL`, optional answer-agent backends, optional `codex`
  or `antigravity` sign-in engines, optional API keys) live in
  `~/.config/search-mcp/.env` or `~/.config/search-mcp/config.json` (`0600`).
  See [docs/CONFIGURATION.md](../../docs/CONFIGURATION.md),
  [docs/DELEGATION.md](../../docs/DELEGATION.md), and
  [docs/API_KEYS.md](../../docs/API_KEYS.md).
- **Retraction Watch initial sync (~70 MB)**: The first scholarly lookup lazily
  downloads Crossref's CC0 Retraction Watch CSV (~70 MB), indexes it into
  SQLite (~8–12 MB), deletes the raw CSV, and refreshes via `ETag` every 7 days.
  Set `SEARCH_MCP_OFFLINE_SYNC=false` to opt out and use online Crossref checks.

## Other clients

Claude Desktop has a binary `.mcpb` bundle on each GitHub Release, the MCP
Registry entry is `io.github.sweetcornna/free-search-mcp`, and any other MCP
client can run `free-search-mcp` (or `uvx free-search-mcp`) over stdio or
streamable HTTP (including `POST/GET /v1/ask`). See
[docs/INSTALL.md](../../docs/INSTALL.md) and
[docs/AGENT_USAGE.md](../../docs/AGENT_USAGE.md).
