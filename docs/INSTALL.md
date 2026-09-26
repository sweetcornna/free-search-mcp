# Install and wire into clients

There are four ways to install, listed in the order most people should try
them. All four run the same server, and none of them asks for an API key.

## 1. The plugin (Claude Code and Codex)

```text
/plugin marketplace add sweetcornna/free-search-mcp
/plugin install free-search@free-search-mcp
```

Outside the TUI the same two steps are:

```bash
claude plugin marketplace add sweetcornna/free-search-mcp
claude plugin install free-search@free-search-mcp -s user
```

In Codex:

```bash
codex plugin marketplace add sweetcornna/free-search-mcp
codex plugin add free-search@free-search-mcp
```

A `[mcp_servers.search]` entry already in `~/.codex/config.toml` silently
shadows the plugin's server of the same name, so run `codex mcp remove search`
first. Codex reads the same marketplace file and the plugin's `.mcp.json`, so
it starts the same pinned server.

The marketplace is this repo (`.claude-plugin/marketplace.json`), and the
plugin is `plugins/free-search`. It contains three things. The MCP server and
the skill load in both hosts: Codex lists the skill as
`free-search:verified-research` (checked with Codex CLI 0.154). The agent file
is in Claude Code's format, and Codex gets the same agent another way (see
[DELEGATION.md](DELEGATION.md)).

- The stdio MCP server `search`, started as
  `uvx free-search-mcp==<plugin version>`. Because of that pin the plugin
  version matches the package version: plugin `X.Y.Z` runs package `X.Y.Z`,
  and `/plugin update free-search` is what moves you to a newer server.
- One skill, `verified-research`. Search snippets are only leads, yet agents
  tend to answer from them: they quote a prize, a deadline or a version out of
  a snippet without opening the page or checking what year the page is from.
  The skill is the workflow for that case: open the primary page, read its date
  against today's, corroborate, and say what could not be verified.
- One agent, `free-search:quick-search`
  (`plugins/free-search/agents/quick-search.md`). It takes one question, makes
  one `research` call that searches and reads the top pages, and replies with
  an answer of one to three sentences, up to five source lines that each carry
  the page's date, and a "Not verified" line when something could not be
  confirmed. The pages it read stay out of the main conversation. Claude
  delegates to it from its description, or you can @-mention it or ask to "use
  the quick-search agent". It is built to return fast: it runs on `haiku`,
  stops after at most 6 turns, sees four tools (`research`, `search`, `fetch`,
  `read_doc`) and loads neither the skill nor your `CLAUDE.md`. The same tool
  list makes it read-only, with no shell, no file tools, no `download` and no
  other MCP server. It treats text on fetched pages as evidence and does not
  follow instructions found in it, and it never asks for an API key. A nested
  Claude Code session that delegated one version lookup to it finished in 25 s
  and cost $0.09 (2026-09-21). For facts someone will act on, run the
  `verified-research` skill in the main conversation instead.

The cost: there are no hooks, and nothing runs unless the agent calls it.
`claude plugin details` projects about 340 tokens added to every session, about
210 for the skill's description and about 130 for the agent's, so that Claude
knows both exist. The skill's body (about 2.1k tokens) loads only when a
lookup needs verifying, and the agent's prompt (about 560) only when it is
spawned. The MCP tool definitions cost the same on any install path.

Configuration lives in the same place as for any other install
(`~/.config/search-mcp/.env`). Chromium for the browser-rendered engines is the
one optional follow-up:
`uvx --from free-search-mcp playwright install chromium`.

## 2. Claude Desktop: the one-click bundle

From 0.12.0, every [GitHub Release](https://github.com/sweetcornna/free-search-mcp/releases)
carries `free-search-mcp-<version>.mcpb`. Download it and double-click it, or
open it from Claude Desktop's Settings, under Extensions. Claude Desktop runs
the pinned package through `uv` and shows a settings form with three optional
fields: proxy, region and cache directory. The form has no API-key field
because the server does not use one.

To edit the JSON by hand, see
[Wire into Claude Desktop](#wire-into-claude-desktop).

## 3. MCP Registry

From 0.12.0 the release workflow publishes the server to the official MCP
Registry as `io.github.sweetcornna/free-search-mcp`. A client or gateway that installs
from the registry resolves that name to the PyPI package `free-search-mcp`,
run with `uvx` over stdio. The entry declares only the optional proxy, region
and cache-directory variables.

## 4. Alternatives: uvx, a source checkout, Docker

uvx, registered by hand, runs the same published package as the plugin. It
resolves to whatever PyPI holds at first launch, nothing tells you when a newer
version exists, and you do not get the skill. The quick-search agent is one
command away, as [DELEGATION.md](DELEGATION.md) shows:

```bash
claude mcp add search -s user -- uvx free-search-mcp   # Claude Code
codex mcp add search -- uvx free-search-mcp            # Codex
uvx free-search-mcp                                    # or run the stdio server directly
```

For any other MCP client, point it at the command `uvx free-search-mcp`
(stdio).

A source checkout takes one command. Use it for working on the code, or on a
machine where you want Chromium with its OS deps, a smoke test and client
registration in one step:

```bash
curl -LsSf https://raw.githubusercontent.com/sweetcornna/free-search-mcp/main/scripts/install.sh | bash -s -- --client claude-code
```

The remote installer clones or updates
`~/.local/share/free-search-mcp`, installs `uv` if needed, syncs dependencies,
installs Chromium, smoke-tests the MCP server, then registers it with the
requested client:

```bash
--client claude-code      # Claude Code user-scope config
--client claude-desktop   # claude_desktop_config.json
--client codex            # Codex config
--client generic          # print portable MCP JSON for other agents
--client add-mcp          # delegate to npx add-mcp
--client both             # Claude Code + Claude Desktop
--client all              # Claude Code + Claude Desktop + Codex
--client none             # install only, no client config changes
```

For Codex, Cursor, Cline, Continue, Zed and generic agent guidance, see
[docs/AGENT_USAGE.md](AGENT_USAGE.md).

To work from a local checkout:

```bash
git clone https://github.com/sweetcornna/free-search-mcp.git
cd free-search-mcp
./scripts/install.sh --client none
```

To do the same steps by hand:

```bash
uv sync
uv run playwright install chromium
cp .env.example .env        # optional: customize engines/limits
```

Run as a stand-alone server (stdio transport):

```bash
uv run search-mcp
```

Or serve it over HTTP. MCP revision `2026-07-28` removed protocol-level
sessions, so the HTTP endpoint is stateless: it needs no sticky routing, and
any replica can answer any request:

```bash
uv run search-mcp --transport streamable-http --port 8000   # → http://127.0.0.1:8000/mcp
```

> The HTTP endpoint has **no authentication** and will fetch arbitrary URLs on
> behalf of whoever reaches it. It binds `127.0.0.1` by default; if you change
> `--host`, put an authenticating reverse proxy in front. A DNS-rebinding guard
> rejects unknown `Origin`/`Host` headers; add trusted browser origins with
> `SEARCH_MCP_HTTP_ALLOWED_ORIGINS`. This is a separate process and port from
> the local settings page (`search-mcp-admin`, port 8765), which stays
> loopback-only because it writes your proxy credentials and any keys you chose
> to add.

Docker runs the server containerized over stdio:

```bash
docker compose build
docker compose run --rm search-mcp     # attaches stdio for MCP
```

## Wire into Claude Code

There are three paths, and they are not interchangeable. The first two run a
published release and the third runs your working tree, so choose by what you
are doing.

To use the server, install the plugin. It is one install, Claude Code handles
updates, the server is pinned to the version you installed, and the
`verified-research` skill and the `free-search:quick-search` agent come with
it:

```text
/plugin marketplace add sweetcornna/free-search-mcp
/plugin install free-search@free-search-mcp
```

To use it without the plugin, register it as a plain MCP server. This runs the
same published package, but it resolves to whatever PyPI holds at first launch,
and nothing tells you when a newer version exists:

```bash
claude mcp add search -s user -- uvx free-search-mcp
```

To work on the server, use the project-scoped `.mcp.json` this repo ships.
Running `claude` inside the checkout auto-detects a `search` server started
with `uv run search-mcp`, which is the code in front of you. The file exists
because neither the plugin nor the `uvx` line can show you your own edits. To
reach a checkout from outside it:

```bash
claude mcp add search -s user -- uv --directory /absolute/path/to/free-search-mcp run search-mcp
```

The plugin and the checkout coexist without a name clash, because Claude Code
exposes the plugin's server as `plugin:free-search:search`. Inside this repo
you can have both and still tell which one answered.

## Wire into Codex

The plugin gives you the server, pinned to the plugin's version, and the
`verified-research` skill (for the quick-search agent in Codex, see
[DELEGATION.md](DELEGATION.md)):

```bash
codex plugin marketplace add sweetcornna/free-search-mcp
codex plugin add free-search@free-search-mcp
```

Check `~/.codex/config.toml` afterwards. A `[mcp_servers.search]` block left
over from an earlier `codex mcp add search …` silently shadows the plugin's
server of the same name: Codex starts the old command and says nothing.
`codex mcp remove search` clears it.

Without the plugin, use a plain MCP registration:

```bash
codex mcp add search -- uvx free-search-mcp
codex mcp list
```

(For a source checkout, swap the command for
`uv --directory /absolute/path/to/free-search-mcp run search-mcp`.)

## Wire into Claude Desktop

The one-click route is the `.mcpb` bundle attached to each release; see
[Claude Desktop under Install](#2-claude-desktop-the-one-click-bundle). To
wire it by hand, add this to
`~/Library/Application Support/Claude/claude_desktop_config.json` (macOS) or
the equivalent on your platform:

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

(Source checkout: `"command": "uv", "args": ["--directory",
"/absolute/path/to/free-search-mcp", "run", "search-mcp"]`.)

Restart Claude Desktop. The eleven tools above will appear in the tool
drawer.

## Wire into other clients

The server speaks plain MCP over stdio (or streamable-http, see above). It
implements protocol revision `2026-07-28` and serves every earlier revision
from the same process, so both new and older clients work unchanged:

- Claude Code and Codex: the plugin (above), or
  `claude mcp add search -s user -- uvx free-search-mcp` /
  `codex mcp add search -- uvx free-search-mcp`
- Cursor / Continue / Cline / Zed (use the JSON snippet above)
- Anything that installs from the MCP Registry:
  `io.github.sweetcornna/free-search-mcp`
- Custom Python / TypeScript clients via the official MCP SDK

For agent-specific operating rules, tool-selection guidance, and a reusable
system-prompt snippet, see [docs/AGENT_USAGE.md](AGENT_USAGE.md).

## Installer choice

The plugin is the recommended path because what you installed and what you run
cannot drift. The server is pinned to the plugin's version, upgrading is
`/plugin update` (the other paths re-run `claude mcp add`), and it is the only
path that also delivers the `verified-research` skill and, in Claude Code, the
`quick-search` agent.
The `.mcpb` bundle applies the same idea to Claude Desktop: it is a pinned
package with a settings form, and there is no JSON to edit. The MCP Registry
entry is for clients and gateways that install by registry name.
Running `uvx free-search-mcp` by hand is the fastest way to try the server in
any other client: HTTP engines work immediately, and Chromium is a single
optional follow-up command. `scripts/install.sh` is the full bootstrap for
people who want a source checkout, Chromium with OS deps, a smoke test and
client registration in one step. Generic MCP installers are useful too:
`add-mcp` can write config for many clients at once.
