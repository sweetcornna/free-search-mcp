# Agent usage guide

This guide is for Codex, Claude, Cursor, Cline, Continue, Zed, and any other
agent that can connect to local stdio MCP servers.

## Server contract

`free-search-mcp` is a local stdio MCP server. The portable configuration is:

```json
{
  "mcpServers": {
    "search": {
      "command": "uv",
      "args": ["--directory", "/absolute/path/to/free-search-mcp", "run", "search-mcp"]
    }
  }
}
```

Use an absolute path. The server writes cache and optional API-key settings under
the user's local config/cache directories; it does not need a network API key for
the default engines.

## One-line installs

Codex:

```bash
curl -LsSf https://raw.githubusercontent.com/sweetcornna/free-search-mcp/main/scripts/install.sh | bash -s -- --client codex
```

Claude Code:

```bash
curl -LsSf https://raw.githubusercontent.com/sweetcornna/free-search-mcp/main/scripts/install.sh | bash -s -- --client claude-code
```

Claude Desktop:

```bash
curl -LsSf https://raw.githubusercontent.com/sweetcornna/free-search-mcp/main/scripts/install.sh | bash -s -- --client claude-desktop
```

All first-party targets supported by the installer:

```bash
curl -LsSf https://raw.githubusercontent.com/sweetcornna/free-search-mcp/main/scripts/install.sh | bash -s -- --client all
```

Other agents:

```bash
curl -LsSf https://raw.githubusercontent.com/sweetcornna/free-search-mcp/main/scripts/install.sh | bash -s -- --client generic
```

Agents supported by `add-mcp`:

```bash
curl -LsSf https://raw.githubusercontent.com/sweetcornna/free-search-mcp/main/scripts/install.sh | bash -s -- --client add-mcp
```

`generic` prints the JSON block to paste into agents that accept MCP JSON
directly. `add-mcp` delegates config writing to the community `add-mcp` CLI for
agents it supports.

## Manual registration

Codex:

```bash
codex mcp add search -- uv --directory /absolute/path/to/free-search-mcp run search-mcp
codex mcp list
```

Claude Code:

```bash
claude mcp add search -s user -- uv --directory /absolute/path/to/free-search-mcp run search-mcp
claude mcp list
```

Claude Desktop, Cursor, Cline, Continue, and Zed generally accept the portable
JSON shape above. Put it in the agent's MCP configuration file or settings UI,
then restart the host app if it does not hot-reload MCP servers.

## How agents should use the tools

Use `research` for broad questions where the agent needs a short source-backed
brief. It performs search, fetches top sources, and returns a synthesized
Markdown bundle with source URLs.

Use `search` for targeted discovery, ranking, and when the agent needs to decide
which pages to inspect. Keep `max_results` small unless the task explicitly asks
for broad coverage.

Use `fetch` when the agent already has a URL and needs reader-mode Markdown with
metadata. Use `fetch_batch` when comparing several known URLs.

Use `compare` when a user asks how multiple pages differ, or when a claim should
be checked against two to five known URLs.

Use `read_doc` for PDF, DOCX, HTML, TXT, or Markdown document sources. For local
files, configure `SEARCH_MCP_DOCUMENT_ROOT`; local reads are restricted by
default.

Use `extract_structured` when schema.org, OpenGraph, Twitter card, or microdata
metadata matters more than prose.

Use `cache_search` only for pages previously fetched by this server. Treat it as
local memory, not live web search.

Use `engines` before engine-specific calls if the agent is unsure which engine
names are available.

## Agent operating rules

Prefer the default engines first. They are keyless and fast:
`duckduckgo`, `mojeek`, and `googlenews`.

Use opt-in engines only when the task needs them. Examples: `bilibili` for video
search, `zhihu` for Chinese Q&A, `google` or `serpsearch` for Google-style SERP
coverage, and keyed engines such as `brave_api`, `serper`, `tavily`, or
`google_cse` only after keys are configured.

If a result includes `gated_engines` or `gated_hint`, report the gate honestly.
Do not treat a CAPTCHA, consent wall, or login wall as proof that the web has no
results.

Do not attempt to defeat CAPTCHAs or provider access controls. Use configured
API keys, a legitimate proxy, SearXNG fallback, or another engine.

For factual claims, cite source URLs returned by `research`, `search`, `fetch`,
or `compare`. If sources conflict, preserve the disagreement instead of forcing a
single answer.

For high-stakes or time-sensitive answers, run a live query even if the agent has
prior knowledge. Cache hits are useful context but are not a substitute for a
fresh check.

Do not ask the user for API keys unless the selected engine requires one. The
default server works without keys.

Do not paste secrets into prompts. Configure keys through `search-mcp-admin`, the
saved config file, or environment variables.

## Suggested agent instruction

```text
You have access to the `search` MCP server. Use `research` for broad
source-backed answers, `search` for discovery, `fetch` for known URLs, `compare`
for cross-source checks, and `read_doc` for documents. Cite URLs for factual
claims. Treat gated engines and empty results as diagnostic signals, not final
truth. Use default keyless engines first, and do not bypass CAPTCHAs or access
controls.
```

## Verification

After installation, verify the local server imports:

```bash
uv --directory /absolute/path/to/free-search-mcp run python -c "from search_mcp.aggregator import list_engines; print(list_engines())"
```

Verify the MCP host can see the server:

```bash
codex mcp list
claude mcp list
```

If a host cannot start the server, run the standalone command and inspect stderr:

```bash
uv --directory /absolute/path/to/free-search-mcp run search-mcp
```

For API keys and proxies:

```bash
uv --directory /absolute/path/to/free-search-mcp run search-mcp-admin
```
