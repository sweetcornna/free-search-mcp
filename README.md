# free-search-mcp
<!-- mcp-name: io.github.sweetcornna/free-search-mcp -->

https://github.com/user-attachments/assets/354b522a-ef6b-4547-bb07-c80c6268a5e8

<p align="center"><sub>Music: “Ethereal Space (cdk Mix)” by cdk, <a href="https://ccmixter.org/files/cdk/34151">ccmixter.org/files/cdk/34151</a>, licensed <a href="https://creativecommons.org/licenses/by/3.0/">CC BY 3.0</a>.</sub></p>

[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Python](https://img.shields.io/badge/python-3.11%2B-blue.svg)](pyproject.toml)
[![MCP](https://img.shields.io/badge/MCP-2026--07--28-purple.svg)](https://modelcontextprotocol.io/specification/2026-07-28)

free-search-mcp is a local-first Model Context Protocol server that needs no
API key. It lets any LLM (Claude, GPT, a local Ollama model, …) search the web,
fetch and clean up pages, and read documents, and you never sign up for a
search API.

It combines ideas from several open-source MCP servers in one Python package,
and adds the output shaping for LLMs and the reliability work that each of them
lacked.

```text
research("how does reciprocal rank fusion work", depth=3)
   ↓
# Research brief: how does reciprocal rank fusion work
_engines: duckduckgo, bing, anysearch · sources: 3 · ~3,400 tokens_

## Sources
- [1] Reciprocal rank fusion | Elasticsearch Reference — <https://…>
- [2] Hybrid Search Scoring (RRF) | Microsoft Learn — <https://…>
- [3] RRF explained in 4 mins — Medium — <https://…>

## Documents
…full Markdown bodies of each page, ready for the LLM to read…
```

That was one tool call. It returned three sources with their full text, and no
API key was involved.

## Quick start

You need [uv](https://docs.astral.sh/uv/) and nothing else: no sign-up, no API
key, no clone.

In Claude Code, inside a session:

```text
/plugin marketplace add sweetcornna/free-search-mcp
/plugin install free-search@free-search-mcp
```

In Codex:

```bash
codex plugin marketplace add sweetcornna/free-search-mcp
codex plugin add free-search@free-search-mcp
```

(In Codex, a `[mcp_servers.search]` entry left in `~/.codex/config.toml` by an
earlier `codex mcp add` silently shadows the plugin's server. Remove it with
`codex mcp remove search`.)

The plugin brings the 11 tools, the `verified-research` skill (how to check a
snippet against its page and its date) and, in Claude Code, the
`free-search:quick-search` agent, all pinned to one version.
`/plugin update free-search` moves to the next release.

Optionally, install Chromium once for the browser-rendered engines (`brave`,
`startpage`, `zhihu`, …) and JavaScript-heavy pages. Everything else works
without it, and a call that needs it returns this command as its error:

```bash
uvx --from free-search-mcp playwright install chromium
```

Claude Desktop, other clients, a source checkout and Docker are under
[Install](#install).

## Why this exists

| | Multi-engine | No API key | Smart fallback | PDF/DOCX | FTS5 cache | Filters | Trafilatura | LLM-tuned |
|---|---|---|---|---|---|---|---|---|
| `nickclyde/duckduckgo-mcp-server` | ✗ | ✓ | ✗ | ✗ | ✗ | ✗ | ✗ | ~ |
| `mrkrsl/web-search-mcp` | ✓ | ✓ | ✓ | ✗ | ✗ | ✗ | ✗ | ~ |
| `Aas-ee/open-webSearch` | ✓ | ✓ | ~ | ✗ | ✗ | ✗ | ✗ | ~ |
| `VincentKaufmann/noapi-google-search-mcp` | ✗ | ✓ | ✓ | ✓ | ✓ | ✗ | ✗ | ~ |
| **free-search-mcp** | **✓** | **✓** | **✓** | **✓** | **✓** | **✓** | **✓** | **✓** |

"LLM-tuned" means Markdown-first output with token estimates, errors that name
the next step, and a `research()` that turns a search and its fetches into one
call. [docs/HOW_IT_WORKS.md](docs/HOW_IT_WORKS.md) explains the less obvious
columns.

## Tools (11)

| Tool | Description |
|---|---|
| `search(query, ...filters)` | Parallel multi-engine search, RRF-merged, title-fuzzy + host-canonical deduped, with optional extractive `lead_snippet` |
| `research(question, depth?, ...filters)` | One-shot: search + fetch top N + return Markdown brief |
| `paper_graph(paper, direction?, limit?)` | Walk one paper's citation graph: references, citing works ranked by influence, and Crossref retraction/correction notices. Takes a DOI, an OpenAlex ID, an exact title, or an arXiv reference (`arXiv:1706.03762`, an `arxiv.org/abs/…` URL, a `10.48550/arXiv.…` DOI, or a bare id such as `1706.03762`, `2401.12345v3` or `hep-th/9901001` when it is the whole input) |
| `compare(question, urls=[2..5])` | Concurrent fetch of 2-5 URLs, side-by-side excerpts keyed by question |
| `fetch(url, render?, inline?, ...)` | Fetch any resource: reader-mode Markdown for pages, parsed text for documents, or a description (type/size/dimensions/sha256) for images and binaries. `inline=True` returns the image itself for vision models |
| `fetch_batch(urls, ...)` | Concurrent multi-URL fetch (max 20 per call) |
| `read_doc(source, start?, length?, ...)` | Parse PDF / DOCX / XLSX / PPTX / EPUB / CSV / code / zip-tar / HTML / TXT / MD with pagination |
| `extract_structured(url, ...)` | Pull JSON-LD / OpenGraph / Twitter cards / microdata via extruct. Long prose fields (`articleBody` and the like) are clipped, and the result says so in `trimmed`. This tool returns metadata; use `fetch` for the text |
| `cache_search(query, limit?, ...)` | FTS5 search across previously fetched pages |
| `engines(group?)` | The source tree (group, then sub-group, then engine), one line each. `group` is an enum of the groups that own engines, so a typo fails schema validation |
| `download(url, ...)` | Save a file to `${SEARCH_MCP_CACHE_DIR}/downloads` by default; files auto-delete after 24h. Set `SEARCH_MCP_DOWNLOAD_ENABLED=false` to disable it. |

There are also 5 MCP prompts and 2 resource templates (`cache://page/{url}`,
`cache://search/{query_hash}`). A twelfth tool, `ask`, appears only when an
answer backend is configured (see [Delegating a lookup](#delegating-a-lookup)).

`search` and `research` take these filters:

| Param | Values | Effect |
|---|---|---|
| `freshness` | `day` / `week` / `month` / `year` | Only results from the last N. Results with no date are kept, and `freshness_note` says when that is most of them. `day` / `week` also add `googlenews` |
| `include_domains` | `["python.org", "djangoproject.com"]` | Restrict to these domains |
| `exclude_domains` | `["pinterest.com"]` | Remove these |
| `category` | a group (`paper`, `finance`, `news`, `software`, `security`, `weather`, …) or a sub-group (`paper.biomed`, `finance.filings`, `finance.fx`, `software.python`, `dataset.ml`, …) | Routes to the sources that natively index that kind of thing; the enum in the tool schema lists every value |
| `include_text` | `"async"` | Substring required in title/snippet |
| `exclude_text` | `"beginner"` | Substring forbidden |
| `max_age_hours` | `24` | Accept a cached answer only if it is younger than this. Default 7 days; the tightest of this, the `freshness` window's TTL, the 6-hour news cap and the category's own cap (`weather`, `finance.fx` 1 hour; `software`, `security` 6 hours) wins |

Output is Markdown by default, with provenance and a token estimate in the
header. `format="json"` returns structured data.

### How a search runs

- A search that names no engines asks a small keyless pool: `duckduckgo`,
  `bing`, `anysearch` and `mojeek`. `googlenews` joins when recency is asked
  for, and `so360` when the query is written in Chinese.
- `category=` routes to sources that index that kind of thing (papers,
  filings, datasets, packages, CVEs, …). Record sources such as `pypi`, `nvd`
  or `openmeteo` also join by themselves when the question is one they can
  answer directly.
- An engine that serves a CAPTCHA, a login wall or an off-topic page is
  benched, a reserve takes its seat, and the response says what happened. A
  proxy (`SEARCH_MCP_PROXY`) is the fix for IP gating.
- Every result says how old it is and where its date came from, so the agent
  can tell a lead from a checked fact.
- `engines()` prints the whole source tree. `engines=["so360", "baidu"]` runs
  exactly the engines named.

No engine that a search reaches by itself needs a key. The details, with
measurements, are in [docs/HOW_IT_WORKS.md](docs/HOW_IT_WORKS.md); walls and
proxies in [docs/PROXY_AND_GATES.md](docs/PROXY_AND_GATES.md); a tour of the
tools with examples in [docs/USAGE.md](docs/USAGE.md).

## Install

The plugin in [Quick start](#quick-start) is the recommended path, because the
server is pinned to the plugin's version and updates with it. The other routes
run the same server:

| Where | How |
|---|---|
| Claude Desktop | open `free-search-mcp-<version>.mcpb` from the [latest release](https://github.com/sweetcornna/free-search-mcp/releases/latest) |
| MCP Registry | `io.github.sweetcornna/free-search-mcp` |
| Claude Code, without the plugin | `claude mcp add search -s user -- uvx free-search-mcp` |
| Codex, without the plugin | `codex mcp add search -- uvx free-search-mcp` |
| Hermes Agent (the plugin) | `hermes plugins install sweetcornna/free-search-mcp/plugins/free-search --enable` |
| Any other MCP client | the stdio command `uvx free-search-mcp` |
| Docker | `docker compose build`, then `docker compose run --rm search-mcp` |

A source checkout with Chromium, a smoke test and client registration in one
step:

```bash
curl -LsSf https://raw.githubusercontent.com/sweetcornna/free-search-mcp/main/scripts/install.sh | bash -s -- --client claude-code
```

Over HTTP, `uvx free-search-mcp --transport streamable-http --port 8000`
serves `http://127.0.0.1:8000/mcp`. That endpoint has no authentication and
fetches any URL for whoever reaches it, so keep it on loopback or put an
authenticating proxy in front.

The JSON for Claude Desktop, Cursor, Cline, Continue and Zed, the installer's
options and the plugin's token cost are in [docs/INSTALL.md](docs/INSTALL.md).
Operating rules for agents are in [docs/AGENT_USAGE.md](docs/AGENT_USAGE.md).

## Search on your own account: `codex` and `antigravity`

Two opt-in engines search on an account you sign in with instead of an API
key. Neither is in any pool or route: a search reaches one only when the call
names it, and nothing changes for anyone who never signs in.

| | `codex` | `antigravity` |
|---|---|---|
| Searches with | OpenAI's web search, the one Codex uses | Google Search, run by a Gemini model |
| Account | a ChatGPT plan that includes Codex | a Google account with Antigravity |
| Each search counts against | the plan's Codex usage | the account's Antigravity quota |
| The provider allows it | yes | no, see the warning below |
| Opens a sign-in page by itself | on first use, over stdio on a desktop | never |
| Full guide, with 中文速览 | [docs/CODEX_SEARCH.md](docs/CODEX_SEARCH.md) | [docs/ANTIGRAVITY_SEARCH.md](docs/ANTIGRAVITY_SEARCH.md) |

> Warning: Google's Antigravity terms forbid using its sign-in from third-party
> tools and name suspension of the Antigravity and Gemini CLI accounts as the
> consequence, and Google has suspended accounts for it. Sign in to
> `antigravity` only with an account you accept that risk for.

### 1. Sign in once

Run the sign-in on the machine the server runs on:

```bash
uvx --from free-search-mcp search-mcp-login codex         # the ChatGPT page `codex login` shows
uvx --from free-search-mcp search-mcp-login antigravity   # prints the warning, then Google's consent page
uvx --from free-search-mcp search-mcp-login status        # account, plan, token expiry
```

(`uv run search-mcp-login …` in a source checkout.) The settings page,
`search-mcp-admin`, has the same Sign in / 登录 buttons. Tokens are stored in
`~/.config/search-mcp/oauth/` (`0600`) and refreshed automatically. The plugin,
`uvx` and the Desktop bundle all read that directory, so a running server picks
up a new sign-in without a restart.

- `codex` can skip this step: the first search that names it opens the sign-in
  page and finishes once you approve (stdio on a desktop only;
  `SEARCH_MCP_CODEX_AUTO_SIGNIN=false` turns it off). A machine already signed
  in to the Codex CLI can reuse that sign-in read-only with
  `search-mcp-login codex --use-codex-cli`.
- `antigravity` signs in with Antigravity's own OAuth client, which is not
  shipped in this package. The sign-in reads it from the Antigravity app
  installed on the machine (checked on macOS). Without Antigravity installed,
  set `SEARCH_MCP_ANTIGRAVITY_CLIENT_ID` and
  `SEARCH_MCP_ANTIGRAVITY_CLIENT_SECRET`; a machine that has signed in keeps
  both in `~/.config/search-mcp/oauth/antigravity.json`.

### 2. Name the engine

```python
search("rust 2024 edition changes", engines=["codex"])
research("what changed in python 3.14 asyncio", engines=["antigravity"])
search("rust 2024 edition changes", engines=["codex", "duckduckgo", "bing"])
```

In Claude Code or Codex, ask for it in words ("search this with the codex
engine"). Filters apply as usual, and results are cached like any other
search. Measured on 2026-09-26, a `codex` search took about 3 s and an
`antigravity` search 10 to 20 s.

### 3. Settings (all optional)

| Var | Default | Meaning |
|---|---|---|
| `SEARCH_MCP_CODEX_MODEL`, `SEARCH_MCP_ANTIGRAVITY_MODEL` | `latest` | `latest` follows the account's model catalogue, so a new model is used once it is listed; a model name pins one |
| `SEARCH_MCP_CODEX_TIMEOUT`, `SEARCH_MCP_ANTIGRAVITY_TIMEOUT` | `60` | seconds for one search |
| `SEARCH_MCP_CODEX_AUTO_SIGNIN` | `true` | open the sign-in page on first use |
| `SEARCH_MCP_ANTIGRAVITY_CLIENT_ID`, `SEARCH_MCP_ANTIGRAVITY_CLIENT_SECRET` | read from the Antigravity install | for a machine without Antigravity; set both or neither |
| `SEARCH_MCP_PROXY` | empty | the sign-in and the searches go through it |

### On a server, over SSH, or in Docker

- Sign in with `--no-browser` (`search-mcp-login codex --no-browser`, or
  `antigravity --no-browser`), open the printed address in any browser, and
  approve. The browser then fails to load a `127.0.0.1:1455` or
  `localhost:51121` address; paste that whole address into the terminal.
- Over `streamable-http`, `codex` never opens a sign-in page by itself, since
  it would open on the server. Sign in there with the command above first.
- In Docker, point `SEARCH_MCP_CONFIG_DIR` at a mounted volume so the tokens
  outlive the container, and run the sign-in inside it with `--no-browser`.
  `antigravity` there needs the two client variables.

`search-mcp-login logout codex` or `logout antigravity` deletes the stored
tokens. To revoke Antigravity's access itself, remove it under "Third-party
apps and services" in the Google account.

### When it does not work

| The error says | Do this |
|---|---|
| `codex not configured` / `antigravity not configured` | no sign-in is stored on this machine; run the sign-in above |
| `port 1455 and 1457 … are in use`, or port 51121 | another sign-in (`codex login`, an open sign-in page) holds the port; close it and retry |
| the usage limit, quota or rate limit was reached | wait until the reset time the error gives; the keyless engines still work |
| `needs Antigravity's OAuth client` | install Antigravity, or set the two client variables |
| HTTP 403 `no valid license` from `antigravity` | set `SEARCH_MCP_ANTIGRAVITY_VERSION` to the installed Antigravity release; it can also mean Google restricted the account |
| `has expired or was revoked` | sign in again |

## Optional: your own API key

You do not need one, and agents should never ask for one. Five engines run
only when a call names them and the operator has set their own key:

| Engine | Provider | Key |
|---|---|---|
| `brave_api` | [Brave Search API](https://brave.com/search/api/) | `SEARCH_MCP_BRAVE_API_KEY` |
| `serper` | [Serper](https://serper.dev) | `SEARCH_MCP_SERPER_API_KEY` |
| `tavily` | [Tavily](https://app.tavily.com) | `SEARCH_MCP_TAVILY_API_KEY` |
| `google_cse` | [Google Custom Search](https://programmablesearchengine.google.com/) | `SEARCH_MCP_GOOGLE_CSE_API_KEY` + `SEARCH_MCP_GOOGLE_CSE_CX` |
| `github_code` | [GitHub](https://github.com/settings/tokens) | `SEARCH_MCP_GITHUB_TOKEN` |

Set a key as an environment variable or `.env` line, or on the local settings
page:

```bash
uv run search-mcp-admin        # local settings / 本地设置 — http://127.0.0.1:8765
```

(`uvx --from free-search-mcp search-mcp-admin` for a plugin or uvx install.)
The page is bilingual (中英双语), binds to `127.0.0.1` only, applies a saved
key without a restart, and never shows a stored value again. The walkthrough
per provider is [docs/API_KEYS.md](docs/API_KEYS.md).

## Delegating a lookup

Delegation keeps page text out of the caller's context and returns a short
answer with dated sources. None of it is on by default:

| You want | Use | It needs |
|---|---|---|
| The agent to search and read by itself | the tools, as installed | nothing |
| Your host's own subagents to do the lookup | the `quick-search` agent definition | a host with subagents |
| The server to dispatch through Claude Code | `SEARCH_MCP_AGENT_BACKEND=claude-code` | the `claude` CLI, logged in |
| The server to dispatch through Codex | `SEARCH_MCP_AGENT_BACKEND=codex` | the `codex` CLI, logged in |
| The server to call a model endpoint you choose | `SEARCH_MCP_AGENT_BACKEND=api` | an OpenAI-compatible or Anthropic-compatible URL |
| Your own code to supply the model | `ask(question, answer_with=...)` in Python | nothing else |

The settings, the agent files and measured timings are in
[docs/DELEGATION.md](docs/DELEGATION.md).

## Configuration

Nothing is required. Settings are `SEARCH_MCP_*` variables, read from the
environment first, then `./.env`, then `~/.config/search-mcp/.env`
(`SEARCH_MCP_CONFIG_DIR` moves that directory). The ones people change most:

| Var | Default | Meaning |
|---|---|---|
| `SEARCH_MCP_PROXY` | empty | outbound proxy (`http`, `https` or `socks5`) for the engines, the browser and `fetch` |
| `SEARCH_MCP_PROXY_ENGINES` | empty | proxy only these engines |
| `SEARCH_MCP_DEFAULT_ENGINES` | `["duckduckgo","bing","anysearch","mojeek"]` | the pool a search uses when it names no engines |
| `SEARCH_MCP_REGION` | `us-en` | locale for engines that take one |
| `SEARCH_MCP_CACHE_DIR` | `~/.cache/search-mcp` | search and page cache |
| `SEARCH_MCP_TOOLS` | empty | register only these tools |

Every setting is listed in [docs/CONFIGURATION.md](docs/CONFIGURATION.md) and
documented in [.env.example](.env.example).

## Development

```bash
git clone https://github.com/sweetcornna/free-search-mcp.git && cd free-search-mcp
uv sync && uv run playwright install chromium
uv run pytest -q                              # offline
SEARCH_MCP_TEST_NETWORK=1 uv run pytest -v    # live, hits the real web
```

Running `claude` inside the checkout picks up the repo's `.mcp.json`, which
starts the working tree's server. The architecture is in
[docs/HOW_IT_WORKS.md](docs/HOW_IT_WORKS.md#architecture), and cutting a
release in [docs/RELEASING.md](docs/RELEASING.md).

## Credits

This project builds on:

- [`mrkrsl/web-search-mcp`](https://github.com/mrkrsl/web-search-mcp): the
  httpx-then-Playwright fetch strategy and the multi-engine fallback chain
- [`Aas-ee/open-webSearch`](https://github.com/Aas-ee/open-webSearch):
  multi-engine breadth (Bing/DDG/Baidu/Brave/Startpage)
- [`VincentKaufmann/noapi-google-search-mcp`](https://github.com/VincentKaufmann/noapi-google-search-mcp):
  anti-detection patterns (`navigator.webdriver`, UA, cookies), the SQLite
  FTS5 cache idea, and multi-format `read_document`
- [`nickclyde/duckduckgo-mcp-server`](https://github.com/nickclyde/duckduckgo-mcp-server):
  per-engine rate limiting and LLM-friendly content cleanup
- [Mojeek](https://www.mojeek.com/), an independent search index that doesn't
  gate on User-Agent
- [Model Context Protocol](https://modelcontextprotocol.io/) and the
  [official Python SDK](https://github.com/modelcontextprotocol/python-sdk)

## License

MIT. See [LICENSE](LICENSE).
