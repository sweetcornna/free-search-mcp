# free-search: Claude Code plugin

This plugin is the recommended way to install
[free-search-mcp](https://github.com/sweetcornna/free-search-mcp). One
`/plugin install` gives Claude Code web search, page fetching and document
reading with no API key, no signup and no manual `claude mcp add`. It contains
11 MCP tools, a source-verification skill and a small quick-search agent.

```
/plugin marketplace add sweetcornna/free-search-mcp
/plugin install free-search@free-search-mcp
```

Codex reads the same marketplace, starts the same MCP server, and lists the
skill as `free-search:verified-research` (checked with Codex CLI 0.154). The
agent file here is in Claude Code's format, and `codex/agents/quick_search.toml`
holds the same agent in Codex's:

```bash
codex plugin marketplace add sweetcornna/free-search-mcp
codex plugin add free-search@free-search-mcp
```

If `~/.codex/config.toml` already has a `[mcp_servers.search]` entry, it
silently shadows the plugin's server of the same name. Run
`codex mcp remove search` first.

## What it installs

### The MCP server `search`

The server has 11 tools. `.mcp.json` declares a single stdio server started
with `uvx free-search-mcp==<version>`, so [uv](https://docs.astral.sh/uv/) is
the only prerequisite, and the first launch downloads the package from PyPI.

The version is pinned to this plugin's own version. Installing plugin `X.Y.Z`
always runs package `X.Y.Z`, and `/plugin update free-search` moves you to a
newer server. `tests/test_plugin_manifest.py` keeps the versions (plugin
manifest, `.mcp.json` pin, Python package) in lockstep, and the release
workflow re-checks them against the tag.

### The skill `verified-research`

Without it, agents tend to answer from search snippets: they quote a deadline
or a prize without opening the page, and they do not notice that the page is
from a previous year. The skill is a workflow for that case: search to find the
primary page, fetch it, compare its publish date and the cache age with today,
corroborate with a second independent source, and report what could not be
verified.

### The agent `free-search:quick-search`

The file is `agents/quick-search.md`. The agent takes one question, makes one
`research` call that searches and reads the top pages, and replies with an
answer of one to three sentences, up to five source lines that each carry the
page's date, and a "Not verified" line when something could not be confirmed.
The pages it read stay out of the main conversation. Claude delegates to it
from its description. You can also @-mention it or ask to "use the quick-search
agent".

It is built to return fast. It runs on `haiku`, stops after at most 6 turns
(`maxTurns: 6`), sees four tools (`research`, `search`, `fetch`, `read_doc`)
and loads neither the skill nor `CLAUDE.md` (`omitClaudeMd: true`). The same
tool list makes it read-only, with no shell, no file tools, no `download` and
no other MCP server. It treats text on a fetched page as evidence and does not
follow instructions found there, and it never asks for an API key. A nested
Claude Code session that delegated one version lookup to it finished in 25 s
and cost $0.09 (2026-09-21). For facts someone will act on, run the
`verified-research` skill in the main conversation instead.

Codex documents custom agents as TOML files under `~/.codex/agents/`, and
`codex/agents/quick_search.toml` is this agent in that format. Both files are
generated from one prompt (`search-mcp agent-file claude-code|codex|prompt`),
and a test fails when either drifts from it. With Codex CLI 0.154, `codex exec`
gave no way to select a custom agent by name, so the route that worked there
was the `quick_search` MCP prompt handed to a generic subagent. The main
README's "Delegating a lookup" section has the details, along with the optional
`ask` tool that lets the server dispatch the lookup itself.

## What it costs

The plugin has no hooks, and nothing runs unless the agent calls it.
`claude plugin details` projects about 340 tokens added to every session, about
210 for the skill's description and about 130 for the agent's, so that Claude
knows both exist. The skill's body (about 2.1k tokens) loads only when a
lookup needs verifying, and the agent's prompt (about 560) only when it is
spawned. The tool definitions cost the same as on any other install path.

## Configuration

Nothing is required, and nothing the server selects on its own uses an API key.
Optional settings such as the engine pools and a proxy live in
`~/.config/search-mcp/.env`, as for any other install method, and the plugin
adds no config of its own. See
[Configuration](../../README.md#configuration). Bringing your own key for one
of the five opt-in engines is possible and manual. The end of that section
describes it, and an agent should never ask you for one.

Browser-rendered engines (`brave`, `startpage`, `zhihu`, …) and JS-heavy page
fetches need Chromium once:

```bash
uvx --from free-search-mcp playwright install chromium
```

Without it, HTTP search and fetch keep working, and any call that needs the
browser returns that install command.

## Not using Claude Code or Codex?

Claude Desktop has a one-click `.mcpb` bundle on each GitHub Release, the MCP
Registry entry is `io.github.sweetcornna/free-search-mcp`, and every other
client can run `uvx free-search-mcp` over stdio. See the
[project README](../../README.md#install) and
[docs/AGENT_USAGE.md](../../docs/AGENT_USAGE.md).
