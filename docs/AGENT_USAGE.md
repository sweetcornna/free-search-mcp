# Agent usage guide

This guide is for Codex, Claude, Cursor, Cline, Continue, Zed, and any other
agent that can connect to local stdio MCP servers.

## Server contract

`free-search-mcp` is a local stdio MCP server named `search`, with 11 tools. It
needs no API key: every engine a search reaches on its own is keyless, and an
agent should never ask a user for one.

## Install: the plugin first

In Claude Code the plugin ships 11 MCP tools, a source-verification skill and a
small quick-search agent. These are the `search` server pinned to the plugin's
version, the `verified-research` skill, which loads on demand, and the
`free-search:quick-search` agent. Install it with:

```text
/plugin marketplace add sweetcornna/free-search-mcp
/plugin install free-search@free-search-mcp
```

Codex:

```bash
codex plugin marketplace add sweetcornna/free-search-mcp
codex plugin add free-search@free-search-mcp
```

The MCP server and the skill load in both hosts: Codex lists the skill as
`free-search:verified-research` (checked with Codex CLI 0.154). The agent file
is in Claude Code's format. Codex and other hosts get the same agent as a file
or a prompt; see [Delegating a quick lookup](#delegating-a-quick-lookup).

Codex pitfall: a `[mcp_servers.search]` entry already in
`~/.codex/config.toml` silently shadows the plugin's server of the same name.
Remove it with `codex mcp remove search`.

Claude Desktop: install the `free-search-mcp-<version>.mcpb` bundle attached to
each GitHub Release (double-click, or Settings → Extensions). Its settings form
has three optional fields (proxy, region, cache directory) and no key field.

Clients and gateways that install from the MCP Registry: the entry is
`io.github.sweetcornna/free-search-mcp`.

Cursor, Cline, Continue, Zed and anything else that accepts MCP JSON:

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

## Install: from a source checkout

Use this route to work on the code. The portable configuration is:

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

Use an absolute path. The server keeps its cache and settings under the user's
local cache/config directories.

The one-line installers clone the repo, sync dependencies, install Chromium,
run a smoke test and register the client.

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
for broad coverage. **Treat a search result as a lead.** The snippet was written
by a search engine, may be years old, and often describes a different edition
of the page. Open the page before quoting a date, amount, rule or version.

Use `fetch` when the agent already has a URL and needs reader-mode Markdown with
metadata. Use `fetch_batch` when comparing several known URLs. `fetch` prints
`published <date>` or `no publication date found`, and on a cache hit how old
the copy is; pass `max_age_hours` when that is too old for the question.

Use `compare` when a user asks how multiple pages differ, or when a claim should
be checked against two to five known URLs.

Use `read_doc` for PDF, DOCX, HTML, TXT, or Markdown document sources. For local
files, configure `SEARCH_MCP_DOCUMENT_ROOT`; local reads are restricted by
default.

Use `extract_structured` when schema.org, OpenGraph, Twitter card, or microdata
metadata matters more than prose. That metadata is often where a page's publish
date, price or event time lives. Long prose fields are clipped and the result
says so; `fetch` is the tool for the text.

Use `paper_graph` when you have one specific paper; it does not search a topic.
It takes a DOI, an OpenAlex ID, an exact title, or an arXiv reference: a bare
id such as `1706.03762`, `2401.12345v3` or `hep-th/9901001` when it is the
whole input, `arXiv:1706.03762`, an `arxiv.org/abs/…` URL, or a
`10.48550/arXiv.…` DOI. It returns what that paper cites, what cites it
(ordered by how much the field cited those in turn), and any Crossref
retraction or correction notice. Check it before repeating a citation, because
a retracted paper looks exactly like a standing one in a search result.

Use `cache_search` only for pages previously fetched by this server. It
searches local memory and never reaches the live web.

Use `engines` before engine-specific calls if the agent is unsure which engine
names are available.

### Delegating a quick lookup

Delegate when you want a short answer with sources and do not want the page
text in your own context. Read the pages yourself, with `research` or `fetch`,
when a person will act on the fact (a deadline, a price, a rule): the delegate
is a small model reading a few pages.

There are three ways to delegate, and they share one prompt, so the rules on
dates, sources and untrusted page text are the same in each.

The `quick-search` agent. In Claude Code with the plugin it is
`free-search:quick-search` (`plugins/free-search/agents/quick-search.md`).
Claude delegates to it from its description, and a user can @-mention it or
ask to "use the quick-search agent". Give it one question plus any domain,
language or edition constraint. It makes one `research` call, at most two more
reading calls, and replies with an answer of one to three sentences, up to five
source lines with each page's date, and a "Not verified" line when something
could not be confirmed. It runs on `haiku`, stops after at most 6 turns, and
sees four tools (`research`, `search`, `fetch`, `read_doc`), so it has no
shell, no file tools, no `download` and no other MCP server. It does not follow
instructions that appear on fetched pages, and it never asks for an API key.
For a server registered by hand, or for Codex, write the file with
`search-mcp agent-file claude-code` or `search-mcp agent-file codex`.

The `quick_search` prompt. Any MCP client can request it. It returns the same
instructions followed by your question, ready to run inline or to hand to
whatever subagent the host has. With Codex CLI 0.154 this is the route that
worked from `codex exec`: a generic subagent given that text called the search
tools and returned a sourced answer in 87 s, while the custom-agent file could
not be selected by name there.

The `ask` tool. It exists only when the operator set
`SEARCH_MCP_AGENT_BACKEND`, so check the tool list. `ask(question)` has the
server search, read the top pages and get an answer from the model the operator
configured. It returns the answer, the model and the time taken, and the pages
that were read. When that model fails, the result says so and carries the pages
as a `research` brief, so answer from those. Ask one question per call and pass
the source URLs and dates on with the answer. [DELEGATION.md](DELEGATION.md) lists the settings and the measured
timings.

## Agent operating rules

Prefer the default pool: omit `engines=` and let the server choose. The pool is
`duckduckgo`, `bing`, `anysearch` and `mojeek`, all keyless and plain HTTP.
`googlenews` joins on its own for `freshness="day"` / `"week"` or
`category="news"`, `so360` joins when the query is written in Chinese, and the
reserves (`so360`, `brave`, `searx`) are seated while fewer than 3 general
engines are healthy. An engine that hit a wall recently is benched for 10 to 60
minutes and reported under "Benched engines"; naming it in `engines=` forces an
attempt.

Name engines only when the task needs a specific source: `bilibili` for video
search, `so360`/`baidu`/`zhihu` for Chinese content, `brave` or `startpage`
(browser-rendered) for a second opinion. Do not use `google` or `serpsearch`:
they hit a JavaScript wall on every request when last measured.

Never ask for an API key, and never suggest getting one as a fix. The server is
built to work without them, and nothing it selects on its own needs one.
`brave_api`, `serper`, `tavily`, `google_cse` and `github_code` are opt-in
engines: they run only when a call names them and the operator has already set
their own key. If one answers "not configured", the search itself is fine, so
carry on with the keyless engines.

When results are thin, try these in order:

1. Rephrase the query: fewer words, the official name, or the other language.
2. Pass `category=`.
3. Name `engines=["so360","baidu"]` for Chinese content.
4. Name `engines=["brave"]` or `["startpage"]` if the browser is installed.
5. Tell the user a proxy would help, but only when `gated_engines` shows a real
   wall (`captcha`, `consent`, `login`, `javascript`).

Prefer `category=` over naming engines. It routes the query to sources that
natively index that kind of content instead of filtering web results by
hostname. Categories are two levels deep: use a bare group when the query may
benefit from breadth across several sub-groups, and use a dotted sub-group when
the content type is known and the query should stay in that branch. The
category-engine limit (3 by default) still caps how many matching specialists
run.

- `category="paper"` chooses specialists across scholarly sub-groups; narrow
  with `paper.biomed`, `paper.cs`, `paper.preprint`, `paper.openaccess`,
  `paper.trial`, `paper.index`, or `paper.math` when the field is known. The
  bare group selects `arxiv`, `openalex`, and `europepmc`; use `paper.math`
  specifically for `zbmath`.
- `category="finance"` reaches filings, market data and macro research;
  narrow with `finance.filings`, `finance.market` or `finance.macro`.
  `finance.fx` gives the ECB reference rate between two named currencies.
- `category="software"` answers "what is the current version" and "is it
  still supported" from the registries: `endoflife` (release cycles and
  support end dates), `github_releases` (newest release of a repository) and
  `pypi`; `software.node` and `software.rust` reach npm and crates.io. The
  record comes back as a result with the publisher's date, so it can be
  cited without fetching a page.
- `category="security"` returns vulnerability records: `nvd` for a CVE id
  (CVSS, status), `osv` for the advisories touching a package with the fixed
  version. Put the CVE id or the package name in the query.
- `category="reference"` returns the Wikidata item behind a topic with dated
  facts (population, area, inception, website) and the Wikipedia link;
  `"weather"` returns Open-Meteo's current conditions and three-day forecast
  for the place named; `"docs"` searches MDN and the RFC registry;
  `"gov"` searches the US Federal Register and GOV.UK.
- `category="stats"` returns a World Bank indicator for a named country
  (GDP, population, inflation, unemployment, life expectancy, …);
  `"calendar"` returns public holidays for a country and year (China with
  its make-up working days) and the current time in a named city;
  `"finance.crypto"` a coin's spot price; `"finance.entity"` the legal entity
  behind a company name; `"reference.domain"` a domain's registration.
- These record sources join a search on their own when the words of the
  question fit them, so a category is optional: "CVE-2024-3094", "100 usd to
  cny", "上海明天天气", "latest fastapi version", "现在东京几点" each reach the
  right source unasked, and the record leads the list as the `Lead:` line.
  Name the thing plainly: a package, a product, a CVE id, an `owner/repo`, a
  place, a country, two currencies, a coin, a domain.
- `category="github"` reaches GitHub, `"forum"` reaches Stack Exchange and
  Hacker News, and `"news"` reaches Google News and GDELT.
- `category="image"` reaches `openverse` and `wikimedia`. It is an exclusive
  category, so these specialist sources replace the general web pool.
- `category="dataset"` spans repository, ML, and government sub-groups; with
  the default three slots it selects `dryad`, `huggingface`, and `dataeuropa`.
  Narrow with `dataset.repository` for the repository branch (`dryad`,
  `dataverse`, `zenodo`, and `figshare`), `dataset.ml` for `huggingface`, or
  `dataset.gov` for `dataeuropa`. It is also exclusive, so dataset specialists
  replace the web pool and no general web results are added.

Call `engines()` for the live tree with a line on each source; it is derived
from the registry, so it always matches what runs. Passing `engines=` turns the
routing off, so only do it when you specifically want one source.

For non-text resources, `fetch` describes the resource without decoding it: an
image returns its type, size and dimensions. Pass `inline=True` only when you
need to look at the picture, because it is expensive. `download` is a write
operation: it saves to an auto-expiring local directory by default. Use the
returned `saved_path` instead of guessing it. If the operator disabled
downloads, report that refusal and do not try to bypass it.

If a result includes `gated_engines` or `gated_hint`, report the gate. Do not
treat a CAPTCHA, consent wall, or login wall as proof that the web has no
results. The reason `off_topic` is different: that engine answered, but with
results about only the first word of the query, and they were discarded. It is
not a block and a proxy does not change it; the remaining engines' results
stand.

Do not attempt to defeat CAPTCHAs or provider access controls. Use another
keyless engine, the automatic stand-ins, or a legitimate proxy.

Check whether an answer is current before relying on it. The search header
shows `dated: N/M`, and N is usually small. Each result ends with
`dated 2026-09-02` (the engine supplied the date), `2026-09-05 (from snippet
text)` (parsed from prose, weaker) or `undated`, and a source kind (`paper`,
`code`, `forum`, `news`, `government`, `academic`) when the hostname tells.
`(cached 2 days ago · retrieved …)` means the engines were asked at that
earlier time. `freshness=` keeps undated results, so a page listed under
`freshness="week"` is not necessarily from this week; the "Undated results"
note says when that applies. `research` carries the same per-source dates and a
"Dates:" line when sources are undated or far apart.

The workflow has five steps:

1. Search to find URLs.
2. Fetch or research the primary (official) page.
3. Compare its date with today's.
4. Corroborate anything that matters with a second independent source.
5. Say what could not be verified.

With the plugin installed this is the `verified-research` skill. The
`quick-search` agent is the lighter option for a lookup that does not need it.

For factual claims, cite source URLs returned by `research`, `search`, `fetch`,
or `compare`. If sources conflict, report the disagreement and do not force a
single answer.

For high-stakes or time-sensitive answers, run a live query even if the agent has
prior knowledge. A cache hit is useful context, and it does not replace a fresh
check.

Do not paste secrets into prompts. Put a proxy URL with credentials, or a key
an operator chose to add, in environment variables or on the local settings
page (`search-mcp-admin`), and keep it out of the conversation.

## Suggested agent instruction

```text
You have access to the `search` MCP server. Use `research` for broad
source-backed answers, `search` for discovery, `fetch` for known URLs, `compare`
for cross-source checks, `read_doc` for documents, and `paper_graph` to check or
expand a specific paper's citations. Prefer `category=` (e.g. "paper.biomed",
"software", "security", "weather", "finance.filings") over naming engines. Search
snippets are leads, not facts: before relying on a date, amount, rule or
version, fetch the primary page, compare its publish date with today's, and
corroborate it with a second source; say what you could not verify. Cite URLs
for factual claims. Treat gated, benched and off-topic engines and empty
results as diagnostic signals, not final truth. Never ask the user for an API
key, and do not bypass CAPTCHAs or access controls.
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

The local settings page (proxy first; keys are optional and manual):

```bash
uv --directory /absolute/path/to/free-search-mcp run search-mcp-admin
```
