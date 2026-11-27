# free-search-mcp
<!-- mcp-name: io.github.sweetcornna/free-search-mcp -->

https://github.com/user-attachments/assets/354b522a-ef6b-4547-bb07-c80c6268a5e8

<p align="center"><sub>Music: “Ethereal Space (cdk Mix)” by cdk, <a href="https://ccmixter.org/files/cdk/34151">ccmixter.org/files/cdk/34151</a>, licensed <a href="https://creativecommons.org/licenses/by/3.0/">CC BY 3.0</a>.</sub></p>

[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.98-orange.svg)](Cargo.toml)
[![MCP](https://img.shields.io/badge/MCP-2026--07--28-purple.svg)](https://modelcontextprotocol.io/specification/2026-07-28)

`free-search-mcp` 1.0 is a single-binary, local-first Rust Model Context
Protocol server that needs **no API key** and **no signup**. It exposes exactly
**3 tools** (`search`, `exec`, and `ask`, ≤ 6,800 B `tools/list`, ≤ 15 ms cold
start) over **500+ web, academic, and specialist sources**, a first-class
scholarly research subsystem, local document parsing, an FTS5 cache, and
optional agent-search capabilities (answer agent, agent lane, and `codex` /
opt-in `antigravity` engines).

```json
// One `ask` call (no code): direct-answer sources or search + parallel page read in ~8 s
{ "question": "latest stable Rust edition and release date", "freshness": "month" }
```

Or combine scholarly verification in one `exec` turn with `Promise.all`:

```js
async () => {
  const [work, integrity, ft] = await Promise.all([
    scholar.work("arXiv:1706.03762"),
    scholar.integrity("10.48550/arXiv.1706.03762"),
    scholar.fulltext("arXiv:1706.03762", { sections: ["Introduction", "Conclusion"] }),
  ]);
  return { title: work?.title, retracted: integrity.isRetracted, sections: ft.sections };
}
```

No API key, no Python runtime, and no Chromium required.

## Quick start (tri-client plugins)

Claude Code, Codex, and Antigravity (`agy`) are equal first-class targets. All
three register the server under key **`"search"`**, ship all three tools
(`search`, `exec`, `ask`), both skills (`verified-research` and
`scholar-research`), and the `quick-search` agent, pinned to `1.0.1`. You only
need [uv](https://docs.astral.sh/uv/) (which downloads the prebuilt binary
wheel via `uvx`) or a standalone `free-search-mcp` binary.

**Claude Code** (inside a session):

```text
/plugin marketplace add sweetcornna/free-search-mcp
/plugin install free-search@free-search-mcp
```

**Codex** (see [docs/CODEX_SEARCH.md](docs/CODEX_SEARCH.md)):

```bash
codex plugin marketplace add sweetcornna/free-search-mcp
codex plugin add free-search@free-search-mcp
```

*(If `~/.codex/config.toml` already has a `[mcp_servers.search]` entry, it
silently shadows the plugin's server — run `codex mcp remove search` first, and
set `tool_timeout_sec = 150` when registering manually for long `exec` runs.)*

**Antigravity (`agy`)** (see [docs/ANTIGRAVITY_SEARCH.md](docs/ANTIGRAVITY_SEARCH.md)):

```bash
agy plugin install https://github.com/sweetcornna/free-search-mcp/tree/main/plugins/free-search
```

**Hermes Agent** (Agent Plugins v1):

```bash
hermes plugins install sweetcornna/free-search-mcp/plugins/free-search --enable
```

Claude Desktop (binary `.mcpb`), standalone binaries, `uvx`, MCP Registry, and
the ~25 MB distroless Docker image are under [Install](#install).

## Three tools, 500+ sources

Instead of burning ~8,700 tokens on a dozen single-purpose tool schemas, 1.0
always exposes **3 tools** (`tools/list` ≤ 6,800 B, `instructions` ≤ 450 B)
with zero prompts or resource templates:

| Tool | Purpose |
|---|---|
| `ask(question, freshness?, lang?, domains?, mode?, max_tokens?)` | Quick one-call lookup with no code (`mode`: `"auto"` \| `"fast"` \| `"agent"`). `"fast"` needs no LLM: checks direct-answer sources first, else searches and reads top pages in parallel (~8 s deadline) and returns dated passages with sources. `"agent"` uses the configured answer agent (`api` / `claude-code` / `codex`) to write a short sourced answer. `"auto"` uses `"agent"` when configured, else `"fast"`. Also available via `web.ask(...)` in `exec`, CLI `free-search-mcp ask "<question>"`, and HTTP `POST/GET /v1/ask`. |
| `search(query, scope?, sources?, freshness?, domains?, lang?, limit?, max_age_hours?)` | Multi-engine search over live verticals (`web`, `academic`, `news`, `code`, `docs`, `dataset`, `finance`, `security`, `gov`, `media`, `commerce`, `forum`, `reference`, plus dotted sub-scopes and synonyms like `papers`, `github`, `law`, `patents`), optional signed-in agent lane (`codex` / opt-in `antigravity`) and optional answer above results (`SEARCH_MCP_AGENT_IN_SEARCH`), zero-network `scope="catalog"` over 500+ sources & Host APIs, and zero-network local FTS5 `scope="cache"`. |
| `exec(code, max_tokens?, timeout_s?)` | Sandboxed ES2020 JavaScript (QuickJS-NG, zero ambient fs/env/sockets) with typed async Host APIs: `web.*`, `scholar.*`, `doc.*`, `source.*`, `cache.*`, `catalog.*`, and `util.*`. Batch parallel fetches with `Promise.all`, read PDF/DOCX/XLSX/PPTX/EPUB/archives, walk citation graphs, check retractions, fetch lawful OA full text, and call any of 500+ cataloged sources or 18 generic protocols. |

### Host SDK inside `exec`

| Namespace | Functions |
|---|---|
| `web` | `ask`, `search`, `fetch`, `fetchBatch`, `request` (`.body` / `.json`), `structured`, `research`, `compare`, `download` |
| `scholar` | `search`, `work`, `graph`, `fulltext`, `integrity`, `cite` — plus discoverable `author`, `org`, `venue`, `grants` |
| `doc` | `read`, `outline` (isolated PDF worker + Office/EPUB/CSV/tar/zip; local reads opt-in via `SEARCH_MCP_DOCUMENT_ROOT`) |
| `source` | `call(opId, params?)` over 500+ cataloged sources; `protocol(kind, baseUrl, op, params?)` over 18 protocols |
| `cache` | `search`, `getPage`, `put`, `get` (including `cache.get("exec:<id>")` overflow handles) |
| `catalog` | `search`, `describe` (zero-network BM25 + fuzzy search over sources, protocols, and Host APIs) |
| `util` | `sleep`, `htmlToMd`, `estimateTokens`, `dedupe`, `grep`, `pick` |

See [docs/USAGE.md](docs/USAGE.md) for runnable examples,
[docs/DELEGATION.md](docs/DELEGATION.md) for `ask`, host subagents, and
answer-agent backends, and [docs/HOW_IT_WORKS.md](docs/HOW_IT_WORKS.md) for the
architecture, guardrails, agent lane, and ranking details.

## Why 1.0 (Rust)

| Metric | Python `v0.18.2` | Rust `1.0.0` |
|---|---|---|
| MCP tools / `tools/list` size | 11–12 tools / ~35 KB (~8,748 tok) | **3 tools / ≤ 6,800 B** |
| Cold start (`spawn → initialize`) | 552 ms | **≤ 15 ms** |
| Memory (pre-exec idle) | ~85 MB | **≤ 15 MB** |
| Binary / Docker image | ~1.08 GB Docker | **~20 MB binary / ~25 MB distroless** |
| Sources aggregated | 76 engines | **500+ sources** (native + 18 protocols + TOML/`jaq` manifests + optional `codex`/`antigravity` agent lane) |
| Scholarly subsystem | Basic `paper_graph` | Unified `ScholarlyWork`, citation graph, 11-step lawful OA full text, Retraction Watch, offline `hayagriva` citations |

> **Retraction Watch initial sync (~70 MB)**: On the first scholarly call
> (`scope="academic"` or `scholar.*`), `free-search-mcp` lazily downloads the
> Crossref Retraction Watch CSV (~70 MB, CC0 1.0), indexes 6 columns into
> `~/.cache/search-mcp/search-mcp.sqlite` (~8–12 MB), deletes the raw CSV, and
> refreshes via `ETag` every 7 days. Set `SEARCH_MCP_OFFLINE_SYNC=false` to opt
> out and rely on online Crossref checks only.

## Install

The tri-client plugins in [Quick start](#quick-start) are the recommended path:
they pin the server version and bundle the `verified-research` and
`scholar-research` skills plus the `quick-search` agent.

| Where | How |
|---|---|
| **Claude Code** (plugin) | `/plugin marketplace add sweetcornna/free-search-mcp` → `/plugin install free-search@free-search-mcp` |
| **Codex** (plugin) | `codex plugin marketplace add sweetcornna/free-search-mcp` → `codex plugin add free-search@free-search-mcp` |
| **Antigravity (`agy`)** (plugin) | `agy plugin install https://github.com/sweetcornna/free-search-mcp/tree/main/plugins/free-search` |
| **Claude Desktop** (bundle) | Open `free-search-mcp-1.0.1-<platform>.mcpb` from the [latest release](https://github.com/sweetcornna/free-search-mcp/releases/latest) |
| **MCP Registry** | `io.github.sweetcornna/free-search-mcp` |
| **Claude Code** (plain MCP) | `claude mcp add search -s user -- uvx free-search-mcp` |
| **Codex** (plain MCP) | `codex mcp add search -- uvx free-search-mcp` |
| **Antigravity** (plain MCP) | `agy mcp add search uvx free-search-mcp` |
| **Any other MCP client** | stdio command `uvx free-search-mcp` (or local `~/.local/bin/free-search-mcp` / `search-mcp`) |
| **Distroless Docker (~25 MB)** | `docker compose build && docker compose run --rm search-mcp` |

Build and install from a source checkout (installs `~/.local/bin/free-search-mcp`
and the `~/.local/bin/search-mcp` symlink alias):

```bash
curl -LsSf https://raw.githubusercontent.com/sweetcornna/free-search-mcp/main/scripts/install.sh | bash -s -- --client claude-code
```

Serve over Streamable HTTP (loopback-only by default; also exposes
`POST/GET /v1/ask` behind the same token/host guards; set
`SEARCH_MCP_HTTP_TOKEN` before exposing beyond `127.0.0.1`):

```bash
free-search-mcp --transport streamable-http --host 127.0.0.1 --port 8000
```

CLI commands:

```bash
free-search-mcp ask "latest stable tokio version"   # quick sourced lookup from the shell
free-search-mcp login codex                         # optional ChatGPT plan search + agent lane
free-search-mcp login status                        # inspect stored OAuth sign-ins
free-search-mcp config                              # show resolved settings (secrets redacted)
free-search-mcp doctor                              # verify network, SSRF/Fake-IP TUN, cache, probes
free-search-mcp sources                             # list registered sources and availability
free-search-mcp probe                               # probe live source health
free-search-mcp eval-replay                         # run offline ranking replay benchmarks
```

Full install options and client guides:
- [docs/INSTALL.md](docs/INSTALL.md) — all install routes (macOS & Linux)
- [docs/CODEX_SEARCH.md](docs/CODEX_SEARCH.md) — Codex plugin install + optional `codex` ChatGPT engine & agent lane
- [docs/ANTIGRAVITY_SEARCH.md](docs/ANTIGRAVITY_SEARCH.md) — Antigravity (`agy`) plugin install + opt-in `antigravity` engine *(read Google ToS warning first)*
- [docs/AGENT_USAGE.md](docs/AGENT_USAGE.md) — operating rules for agents
- [docs/DELEGATION.md](docs/DELEGATION.md) — `ask`, host subagents, and server-side answer agent backends

## Configuration & optional keys / sign-ins

Nothing is required, and no default, reserve, locale, academic, or vertical
route needs an API key or account. Agents must never ask you for a key or a
sign-in.

Settings use `SEARCH_MCP_*` environment variables, `~/.config/search-mcp/.env`,
or `~/.config/search-mcp/config.json` (`0600`). Security-sensitive variables
(credentials, proxy, SSRF overrides, `HTTP_TOKEN`, `DOCUMENT_ROOT`,
`EXEC_RAW_HTTP`, `AGENT_API_KEY`, `ANTIGRAVITY_CLIENT_SECRET`) are **ignored**
if placed in a working-directory `./.env`.

| Variable | Default | Purpose |
|---|---|---|
| `SEARCH_MCP_PROXY` | *(empty)* | Outbound proxy (`http`/`https`/`socks5`/`socks5h`) |
| `SEARCH_MCP_NO_PROXY` | *(empty)* | Comma-separated host suffixes that bypass proxy |
| `SEARCH_MCP_DEFAULT_ENGINES` | `["duckduckgo","bing","yahoojp","seznam"]` | Default keyless web pool |
| `SEARCH_MCP_RESERVE_ENGINES` | `["yahoo","youcom","mwmbl"]` | Hedging reserves |
| `SEARCH_MCP_AGENT_BACKEND` | `off` | Optional answer agent (`off` \| `api` \| `claude-code` \| `codex`) for `ask` and `search` |
| `SEARCH_MCP_AGENT_ENGINES` | `auto` | Agent lane (`auto` = signed-in `codex` only; `off`; or explicit list) |
| `SEARCH_MCP_CONTACT_EMAIL` | *(empty)* | Optional polite-pool email for Crossref, OpenAlex, NCBI, and Unpaywall OA full text |
| `SEARCH_MCP_OFFLINE_SYNC` | `true` | Lazy ~70 MB Retraction Watch CSV sync (`false` disables) |
| `SEARCH_MCP_CACHE_DIR` | `~/.cache/search-mcp` | Directory for `search-mcp.sqlite` |
| `SEARCH_MCP_HTTP_TOKEN` | *(empty)* | Optional Bearer token for Streamable HTTP and `/v1/ask` |

See [docs/CONFIGURATION.md](docs/CONFIGURATION.md),
[.env.example](.env.example), [docs/API_KEYS.md](docs/API_KEYS.md), and
[docs/PROXY_AND_GATES.md](docs/PROXY_AND_GATES.md).

## Development

```bash
git clone https://github.com/sweetcornna/free-search-mcp.git && cd free-search-mcp
cargo test --workspace --locked --offline
cargo clippy --workspace --all-targets --locked --offline -- -D warnings
cargo deny check --disable-fetch bans licenses sources
```

Running Claude Code inside the checkout picks up `.mcp.json`, which launches
the working tree's Rust server. Release steps live in
[docs/RELEASING.md](docs/RELEASING.md).

## License

MIT. See [LICENSE](LICENSE).
