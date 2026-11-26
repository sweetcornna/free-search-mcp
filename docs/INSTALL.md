# Install and Wire Into Clients

`free-search-mcp` 1.0 ships as a single Rust binary (`free-search-mcp`, with
`search-mcp` symlink/alias) targeting **macOS and Linux** (Windows is P1).
Every install route runs the same keyless 3-tool server (`search`, `exec`, and
`ask`) under server key **`"search"`**.

## 1. Tri-Client Plugins (Recommended)

The plugin pins the server version to `1.0.0` and bundles the `search` MCP
server, both on-demand skills (`verified-research` and `scholar-research`),
the `quick-search` agent, and Antigravity standing rules.

### Claude Code

Inside a session:

```text
/plugin marketplace add sweetcornna/free-search-mcp
/plugin install free-search@free-search-mcp
```

From the shell:

```bash
claude plugin marketplace add sweetcornna/free-search-mcp
claude plugin install free-search@free-search-mcp -s user
```

### Codex

```bash
codex plugin marketplace add sweetcornna/free-search-mcp
codex plugin add free-search@free-search-mcp
```

If `~/.codex/config.toml` has an older `[mcp_servers.search]` entry, it
silently shadows the plugin's server — run `codex mcp remove search` first.
See [CODEX_SEARCH.md](CODEX_SEARCH.md) for setting `tool_timeout_sec = 150`
and copying `plugins/free-search/codex/agents/quick_search.toml` into
`~/.codex/agents/`.

### Antigravity (`agy`)

```bash
agy plugin install https://github.com/sweetcornna/free-search-mcp/tree/main/plugins/free-search
# or from a local clone:
agy plugin install ./plugins/free-search
```

Antigravity ingests `plugin.json`, `mcp_config.json`, `rules/AGENTS.md`,
`skills/{verified-research,scholar-research}/SKILL.md`, and
`agents/quick-search.md`. See [ANTIGRAVITY_SEARCH.md](ANTIGRAVITY_SEARCH.md).

### Hermes Agent (Agent Plugins v1)

```bash
hermes plugins install sweetcornna/free-search-mcp/plugins/free-search --enable
```

---

## 2. Claude Desktop: One-Click Binary Bundle (`.mcpb`)

Each [GitHub Release](https://github.com/sweetcornna/free-search-mcp/releases)
attaches platform-specific `.mcpb` bundles (`free-search-mcp-1.0.0-<platform>.mcpb`)
using `"type": "binary"`. Double-click the bundle or install it via **Settings
→ Extensions**. It embeds the native `free-search-mcp` binary directly (no
Python or `uv` needed) and shows three optional settings: proxy, region, and
cache directory. Empty or unfilled placeholders are stripped automatically by
the binary.

---

## 3. MCP Registry

`free-search-mcp` is published to the official MCP Registry as
`io.github.sweetcornna/free-search-mcp`, resolving to the `maturin` binary
wheel on PyPI run via `uvx free-search-mcp` over stdio.

---

## 4. Standalone Binary, `uvx`, Source Checkout & Docker

### Prebuilt wheel via `uvx` (zero build)

`pyproject.toml` packages the Rust binary with `maturin` (`bindings = "bin"`),
so `uvx` downloads and executes the native binary directly:

```bash
claude mcp add search -s user -- uvx free-search-mcp   # Claude Code
codex mcp add search -- uvx free-search-mcp            # Codex
agy mcp add search uvx free-search-mcp                 # Antigravity
uvx free-search-mcp --help                             # Run directly
```

### Build & install `~/.local/bin/free-search-mcp` (+ `search-mcp` alias)

From a source checkout (requires Rust 1.98 per `rust-toolchain.toml`):

```bash
git clone https://github.com/sweetcornna/free-search-mcp.git
cd free-search-mcp
./scripts/install.sh --client none
```

Or one-line remote install:

```bash
curl -LsSf https://raw.githubusercontent.com/sweetcornna/free-search-mcp/main/scripts/install.sh \
  | bash -s -- --client claude-code
```

Supported `--client` flags: `none` (default), `claude-code`, `claude-desktop`,
`codex`, `antigravity`, `generic`, `add-mcp`, `both`, `all`.

`scripts/install.sh` installs `~/.local/bin/free-search-mcp` via atomic rename
(safely replacing any older `uv` symlink instead of overwriting through it) and
creates `~/.local/bin/search-mcp -> free-search-mcp`.

### Distroless Docker (~25 MB)

```bash
docker compose build
docker compose run --rm search-mcp        # attaches stdio for MCP
```

Or directly:

```bash
docker build -t free-search-mcp .
docker run -i --rm free-search-mcp
```

### Streamable HTTP transport

```bash
free-search-mcp --transport streamable-http --host 127.0.0.1 --port 8000
# Serves http://127.0.0.1:8000/mcp and POST/GET http://127.0.0.1:8000/v1/ask
```

> **Security**: Streamable HTTP binds `127.0.0.1` by default, rejects unknown
> `Origin`/`Host` headers (DNS-rebinding guard), and never adds wildcard hosts
> (`0.0.0.0`) to `allowed_hosts`. If you expose it beyond loopback, set
> `SEARCH_MCP_HTTP_TOKEN` (Bearer auth) and `SEARCH_MCP_HTTP_ALLOWED_ORIGINS`
> or place an authenticating reverse proxy in front.

---

## 5. Manual Client Configurations (Server Key `"search"`)

### Claude Code (working inside a source checkout)

This repo ships a project-level `.mcp.json` using
`cargo run --quiet --locked -p fsm-server --bin free-search-mcp`, so running
`claude` inside the checkout tests your local working tree while coexisting
cleanly with the installed plugin (`plugin:free-search:search`).

### Codex (`~/.codex/config.toml`)

```toml
[mcp_servers.search]
command = "/Users/<user>/.local/bin/free-search-mcp"
args = []
tool_timeout_sec = 150
```

### Antigravity (`~/.gemini/config/mcp_config.json`)

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

### Claude Desktop, Cursor, Cline, Continue & Zed

```json
{
  "mcpServers": {
    "search": {
      "command": "uvx",
      "args": ["free-search-mcp"]
    }
  }
}
```

*(Or replace `"command"` with `"/Users/<user>/.local/bin/free-search-mcp"` and
`"args": []` for a locally built binary.)*

---

## 6. Post-Install Checks & Notes

- **Verify**: Run `free-search-mcp doctor` (checks network, Clash/Fake-IP TUN
  detection, SQLite cache) and `free-search-mcp sources` (lists all registered
  sources).
- **Retraction Watch initial sync (~70 MB)**: On the first scholarly call, the
  server lazily downloads Crossref's ~70 MB Retraction Watch CSV (CC0 1.0),
  indexes it into `~/.cache/search-mcp/search-mcp.sqlite` (~8–12 MB), deletes
  the CSV, and refreshes via `ETag` every 7 days. Disable with
  `SEARCH_MCP_OFFLINE_SYNC=false`.
- **Coexistence with v0.18.x**: 1.0 writes to
  `~/.cache/search-mcp/search-mcp.sqlite` and leaves legacy `cache.sqlite`
  untouched.
- **No Chromium needed in 1.0**: Browser rendering is deferred to P1; all 1.0
  engines and fetchers use the native Rust HTTP + TLS impersonation stack.
