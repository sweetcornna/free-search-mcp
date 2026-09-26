# free-search-mcp
<!-- mcp-name: io.github.sweetcornna/free-search-mcp -->

<p align="center">
  <img src="https://raw.githubusercontent.com/sweetcornna/free-search-mcp/main/docs/search.gif" alt="free-search-mcp — one research() call returns a cited Markdown brief, no API key" width="820">
</p>

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

## Quick start: install the plugin (recommended)

You do not need to sign up, get a key or clone the repository. The only
prerequisite is [uv](https://docs.astral.sh/uv/).

In Claude Code, inside a session:

```text
/plugin marketplace add sweetcornna/free-search-mcp
/plugin install free-search@free-search-mcp
```

From a shell it is the same two steps:

```bash
claude plugin marketplace add sweetcornna/free-search-mcp
claude plugin install free-search@free-search-mcp -s user
```

In Codex:

```bash
codex plugin marketplace add sweetcornna/free-search-mcp
codex plugin add free-search@free-search-mcp
```

> **Codex pitfall.** If `~/.codex/config.toml` already has a
> `[mcp_servers.search]` entry from an earlier `codex mcp add`, it shadows the
> plugin's server of the same name. Codex gives no warning and keeps running
> the old one. Remove the entry first: `codex mcp remove search`.

The plugin ships 11 MCP tools, a source-verification skill and a small
quick-search agent. The MCP server `search` is pinned to the plugin's own
version. The skill `verified-research` loads on demand and tells the agent when
a search snippet is not enough and how to check a date. In Claude Code, the
agent `free-search:quick-search` answers one lookup in its own context on
`haiku` and hands back a few sentences with dated source URLs. The server and
the skill load in both hosts: Codex lists the skill as
`free-search:verified-research` (checked with Codex CLI 0.154). Codex and other
hosts get the same agent as a file or a prompt, and an optional `ask` tool lets
the server do the delegating. [Delegating a lookup](#delegating-a-lookup)
covers every route.
`/plugin update free-search` moves all of it to the next release (in Codex, run
`codex plugin marketplace upgrade` and then `codex plugin add` again). The
[plugin section under Install](#1-the-plugin-claude-code-and-codex) has the
details and the token cost.

Nothing else is needed. Every engine that a search reaches on its own works
without an API key, and the first run downloads the package from PyPI.

For Claude Desktop, other MCP clients, a source checkout or Docker, see
[Install](#install).

Optionally, install Chromium once. The browser-rendered engines (`brave`,
`startpage`, `zhihu`, …) and JS-heavy page fetches need it:

```bash
uvx --from free-search-mcp playwright install chromium
```

Without it, HTTP search and fetch keep working, and any call that needs the
browser returns that exact install command as its error.

Configuration is optional and lives in `~/.config/search-mcp/.env`; see
[Configuration](#configuration).

## Why this exists

Existing search MCPs each do one thing well, but you usually want all of it:

| | Multi-engine | No API key | Smart fallback | PDF/DOCX | FTS5 cache | Filters | Trafilatura | LLM-tuned |
|---|---|---|---|---|---|---|---|---|
| `nickclyde/duckduckgo-mcp-server` | ✗ | ✓ | ✗ | ✗ | ✗ | ✗ | ✗ | ~ |
| `mrkrsl/web-search-mcp` | ✓ | ✓ | ✓ | ✗ | ✗ | ✗ | ✗ | ~ |
| `Aas-ee/open-webSearch` | ✓ | ✓ | ~ | ✗ | ✗ | ✗ | ✗ | ~ |
| `VincentKaufmann/noapi-google-search-mcp` | ✗ | ✓ | ✓ | ✓ | ✓ | ✗ | ✗ | ~ |
| **free-search-mcp** | **✓** | **✓** | **✓** | **✓** | **✓** | **✓** | **✓** | **✓** |

"LLM-tuned" means Markdown-first output, token estimates, truncation at
paragraph boundaries, docstrings with "Best for / Not for / Returns / Common
mistakes" sections that the model uses to pick the right tool, error messages
that name a next step, MCP prompts and resource templates, and a one-shot
`research()` that collapses a search and the fetches that follow it into a
single turn.

"Trafilatura" means main content is extracted with
[trafilatura](https://github.com/adbar/trafilatura), which won the Bevendorff
2023 ROUGE benchmark (~0.85 vs ~0.55 for naive boilerplate stripping). Each
fetched page also returns `author`, `published_date`, and `sitename`.

"Filters" means search and research accept `freshness`, `include_domains`,
`exclude_domains`, `category`, `include_text` and `exclude_text`. `category`
also routes the query to sources that natively index that kind of content. A
bare group
(`news`/`pdf`/`github`/`paper`/`forum`/`blog`/`image`/`dataset`/`finance`)
widens the pool, and a dotted sub-group (`paper.biomed`, `paper.math`,
`finance.filings`, `dataset.repository`, `dataset.ml`, `dataset.gov`, …)
narrows it to the sources that index that sub-group. See
[Vertical sources](#vertical-sources-selected-automatically-by-category).

### Anti-detection &amp; resilience

- The HTTP fast path uses [`curl_cffi`](https://github.com/lexiforest/curl_cffi)
  with a real Chrome 131 JA3/JA4 and HTTP/2 fingerprint. DuckDuckGo answers
  plain httpx with its "anomaly 202" rate-limit response, and this fingerprint
  avoids it.
- The Playwright fallback uses `launch_persistent_context` (cookies survive
  restarts on disk), prefers a real installed Chrome (`channel="chrome"`),
  drops the `--no-sandbox` fingerprint marker on macOS, and randomizes the
  viewport per session.
- Bing is queried the way a browser queries it, with a warmed cookie jar and
  the `form=QBRE` request shape. Without them Bing answers HTTP 200 with a decoy
  page about the query's first word. With them the results are on topic, and
  the [off-topic guard](#engines) catches the decoy if it returns.
- Result dedup is title-fuzzy and host-canonical (rapidfuzz
  `token_set_ratio >= 92`, with the host normalized for `www./m./amp.` and
  country TLDs collapsed). This catches `bbc.co.uk` vs `bbc.com` duplicates,
  which URL-only dedup misses. The rank-fusion key also ignores scheme, `www.`,
  default port, fragment and trailing slash, so the `http://` and `https://`
  copies of one page merge into one result, printed with the `https://` URL.
- `search` includes an extractive `lead_snippet`. It picks the top-3 result
  whose snippet contains ≥2 query terms and is ≥80 chars, and renders it as
  `> **Lead:** According to {host}: …`. No LLM is called, and when no snippet
  qualifies no lead is returned.

> This project does not attempt to defeat proof-of-work captchas on Bing or
> Brave, because that would violate their terms of service. When those engines
> challenge a request, the search falls back to other engines.

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

There are also 5 MCP prompts (`Research thoroughly`, `Fact-check claim`,
`Compare sources`, `News brief`, `Quick search`) and 2 resource templates
(`cache://page/{url}`, `cache://search/{query_hash}`).

A twelfth tool, `ask(question, ...)`, is registered only when the operator
configures an answer backend. It returns a short answer with dated sources in
place of the pages. See [Delegating a lookup](#delegating-a-lookup).

### Filters (search / research)

| Param | Values | Effect |
|---|---|---|
| `freshness` | `day` / `week` / `month` / `year` | Only results from the last N. Results with no date are kept, and `freshness_note` says when that is most of them. `day` / `week` also add `googlenews` |
| `include_domains` | `["python.org", "djangoproject.com"]` | Restrict to these domains |
| `exclude_domains` | `["pinterest.com"]` | Remove these |
| `category` | a group (`paper`, `finance`, `news`, `software`, `security`, `weather`, …) or a sub-group (`paper.biomed`, `finance.filings`, `finance.fx`, `software.python`, `dataset.ml`, …) | Routes to the sources that natively index that kind of thing; the enum in the tool schema lists every value |
| `include_text` | `"async"` | Substring required in title/snippet |
| `exclude_text` | `"beginner"` | Substring forbidden |
| `max_age_hours` | `24` | Accept a cached answer only if it is younger than this. Default 7 days; the tightest of this, the `freshness` window's TTL, the 6-hour news cap and the category's own cap (`weather`, `finance.fx` 1 hour; `software`, `security` 6 hours) wins |

All tools default to `format="markdown"`, which is readable, uses ~40% fewer
tokens than JSON, and carries provenance and a token-budget header. Pass
`format="json"` for structured access.

### Is it current, and is it the source?

A search result is a lead. The snippet under it was written by a search
engine, is often months old, and is routinely about a different year's edition
of the same page. It still reads like an answer, and agents tend to answer from
it. Every tool therefore reports what it knows about time and provenance, so
the agent can see how far to trust a result:

```text
# Search: uv 0.9 release notes

_engines: duckduckgo, bing, anysearch_  _results: 3_  _dated: 2/3_
_(cached 2 days ago · retrieved 2026-09-19T08:12:03Z)_

## 1. Release 0.9.0 · astral-sh/uv
…
_duckduckgo, bing_ · score 0.03 · code · dated 2026-09-02

## 2. uv 0.9 is out — what changed
…
_anysearch_ · score 0.02 · 2026-09-05 (from snippet text)

## 3. uv documentation
…
_bing_ · score 0.01 · undated
```

| Signal | Where | Meaning |
|---|---|---|
| `retrieved_at` | `search`, `research`, `fetch` | When the engines (or the page) were asked. For a cache hit this is when the stored answer was first retrieved |
| `cache_age_seconds` | cache hits only | How old the stored answer is; Markdown shows `(cached 2 days ago · retrieved …)` |
| `dated_results` | `search` | How many results carry any date, shown as `dated: N/M` in the header. It is usually a small number |
| `date_source` | each result | `structured` (the engine's API gave a date), `snippet` (parsed out of snippet text, so weaker; shown as `… (from snippet text)`), or `none` (shown as `undated`) |
| `source_type` | each result | `paper` / `code` / `forum` / `news` / `government` / `academic`, from the hostname. It describes the kind of site and does not affect ranking |
| `freshness_note` | `search` with `freshness=` | Present when at least half the results are undated. `freshness` keeps undated results, so appearing in the list does not show that a result is recent |
| `usage_note` | `search` | One fixed line saying that snippets locate sources and the details should be read from the page |
| `date_note` | `research` | How many of the brief's sources could be dated, and how far apart the dates are. Each source keeps its own date and `source_type` |

`fetch` prints `published <date>` when the page declares one and
`no publication date found` when it does not, plus the cache age on a hit.

Cached searches expire sooner when the question was about recency. The default
TTL is seven days, and a seven-day-old answer to `freshness="day"` cannot be
right. So `freshness="day"` results are kept for 1 hour, `week` for 6 hours,
`month` for 24 hours, and `category="news"` for at most 6 hours. The record
categories expire on their own clock whatever the query said: `weather` and
`finance.fx` after 1 hour (a forecast is reissued hourly, the ECB fixes rates
once a working day), `software` and `security` after 6 hours.

These signals support one workflow: search to find URLs, fetch (or research)
the primary page, check its date, and corroborate anything that matters against
a second independent source. The plugin's `verified-research` skill writes that
workflow down for the agent. For a quick lookup that does not need that depth,
the `quick-search` agent reads the pages in a separate context, so they never
enter the main conversation.

### Tool annotations

Every tool declares `readOnlyHint`, `idempotentHint`, and `openWorldHint`
annotations, so MCP clients can label the tools and restrict elevated actions.

### Protocol notes

The SDK is pinned to `mcp>=2.2,<2.3`. The server implements protocol revision
`2026-07-28` and serves the earlier handshake-era revisions from the same
process, so new and old clients both work unchanged. What a client sees:

- Markdown output is one plain text block, with nothing else attached.
- `format="json"` returns the dict itself as structured content, with no
  `{"result": …}` wrapper around it. A tool whose answer is a list keeps the
  `{"result": [...]}` envelope, because structured content must be an object.
- Tools do not advertise an `outputSchema`. The shape depends on `format`, and
  a schema that holds for only one of the two modes makes strict clients reject
  the other.
- A truncated `fetch` carries a `cache://page/…` resource link to the full
  cached text (clients on revisions older than `2025-06-18` do not get the
  link block). `search` in JSON mode links its `cache://search/…` row the same
  way.
- Completions are served for prompt arguments (`category`, `depth`, `since`)
  and for both `cache://` resource templates, so a client can offer the URLs
  and searches that are in the cache.
- Failures come back as tool errors whose text names a next step; they are not
  raised as protocol errors. A refused URL, an unknown engine or an unreadable
  document produces a message the agent can read and act on, and the message
  never includes internal details.

### Engines

The registry holds 74 engines. A search that names none of them runs a small
pool chosen for it, and the pool depends on which engines are healthy at that
moment.

The default pool is `duckduckgo`, `bing`, `anysearch` and `mojeek`. All four
use plain HTTP and need no browser.

Two engines join it when the query calls for them:

- `googlenews` joins for `freshness="day"` / `"week"`, or `category="news"`.
  It answers any query, programming questions included, with ten news
  headlines, which suits a web search only when recency was asked for. Its
  opaque `news.google.com` redirect links are resolved to the publisher's URL,
  so the same story from a web engine merges with it and is listed once.
- `so360` joins when the query itself is written in Chinese. The language is
  detected from the script of the query, and `SEARCH_MCP_REGION` plays no part.
  In our measurement it returned the organiser's own site and the university
  notice for a Chinese query, neither of which the western indexes had.

Reserves (`so360`, `brave`, `searx`, in that order) are seated only while
fewer than 3 general engines are healthy. A pool that is one engine short is
left alone, because a fourth engine that needs a browser render would add
seconds to every search. A reserve that needs Chromium is skipped while
Chromium is known to be missing.

The circuit breaker benches an engine at once when it serves a wall (CAPTCHA,
consent, login, a JS shell) or an off-topic page. An engine that errors is
benched after 2 failures in a row, and one that silently returns nothing after
3. The bench lasts 10 minutes and doubles on each repeat up to 60, and one
success clears it. A benched engine is not asked, so a dead engine slows down
only the search that discovers it, and the bench is written to
`<cache_dir>/engine_health.json` so that a new process does not discover it
again (measured: 8.5 s for the first process, 4.0 s for the next). Naming an
engine in `engines=[...]` runs it even while it is benched. The response reports `benched_engines` (Markdown:
"Benched engines") with the reason, the retry time and which reserve is
standing in.

The off-topic guard exists because Bing can answer HTTP 200 with ten
well-formed results about the first word of the query, such as the Steam page
for "rust ownership borrowing". Left alone, rank fusion interleaves those with
real answers. The guard drops a web engine's whole bucket when almost none of
it mentions the rest of the query and another engine's results show that those
words do get echoed. The engine is reported under `gated_engines` with reason
`off_topic`. This is not a block, so a proxy does not change it. More in
[docs/PROXY_AND_GATES.md](docs/PROXY_AND_GATES.md).

The rescue pass runs when a search still comes back empty, or nearly empty
with gated or erroring engines. The aggregator makes one bounded recovery
attempt through the reserves and then `searx` and `bing`, in that order,
skipping anything that already ran or is benched, and reports it as
`rescued_via`. A wall on the default engines then costs time but still returns
results.

None of this needs an API key. No engine in the default, reserve, rescue,
fresh, locale or category pools requires one, and
`tests/test_no_key_positioning.py` fails the build if a keyed engine is added
to any of them.

Status as measured on 2026-09-21:

- `duckduckgo`, `bing`, `anysearch`, `so360` and `brave` (browser) answer on
  topic.
- `mojeek` serves a captcha on every request. It stays in the default list for
  its independent index, whose results differ usefully from the others when it
  answers. In practice it is benched after one attempt and re-probed every 10
  to 60 minutes.
- `google` / `serpsearch` serve a JavaScript wall on every request. They are
  not a recovery path, so do not reach for `engines=["google"]` when results
  are thin.
- `startpage` and `sogou` returned nothing.

By name only (all keyless):
- `startpage` is browser-rendered (~5-10s/query).
- `brave` and `baidu` are browser-rendered and intermittently challenge
  headless clients.
- `searx` is a meta-search proxy over public SearXNG instances. Most public
  instances are slow or unreliable in 2026; pin a good one with
  `SEARCH_MCP_SEARX_INSTANCES`.
- `google` is a keyless scrape of the Google web SERP (HTTP first, with a
  Playwright fallback when Google serves a JS or consent shell). `serpsearch`
  is an alias of `google`: all dedicated "SERP APIs" require a key, so the only
  keyless SERP is a direct scrape. See the status note above.
- `anysearch` is in the default pool. It calls the anonymous (keyless) tier of
  the [AnySearch](https://github.com/anysearch-ai/anysearch-mcp-server)
  unified-search REST API, and one HTTP call returns fused, re-ranked results.
  The tier is rate-limited per IP, and a 429 or 5xx degrades to an empty
  result. It does not honour `site:` / `filetype:` operators, so domain filters
  are applied to its results afterwards.
- `bilibili` is keyless Bilibili (哔哩哔哩) video search through the public
  `web-interface/search/all/v2` JSON API (synthetic `buvid3` cookie, no
  login). It returns video results only.
- `zhihu` is best-effort keyless Zhihu (知乎) search. Zhihu's
  `api/v4/search_v3` needs login cookies and `x-zse-96` signing, so the only
  no-key path is rendering the public search page in a browser. Zhihu blocks
  headless clients, so a login wall or an empty result is common. Treat it like
  `baidu`/`brave`.
- `sogou` and `so360` are Chinese web indexes, scraped from HTML. `sogou`
  returns redirect URLs (`sogou.com/link?url=…`) in place of target URLs, and
  the blob can only be resolved by following it. `fetch` handles that, but
  host-based `category` filtering discards them. `baidu` and `so360` return
  direct URLs.
- `wikipedia` is encyclopedia search; its language follows
  `SEARCH_MCP_REGION`.
- `openlibrary` is book search over the Internet Archive catalogue.

Scholarly and financial sources are keyless too, and are normally reached
through `category=`. The scholarly ones are `arxiv`, `openalex`, `crossref`,
`pubmed`, `europepmc`, `dblp`, `doaj`, `clinicaltrials` and `zbmath`; the
financial ones are `sec_edgar`, `yahoofinance`, `cninfo`, `worldbank`, `imf`
and `frankfurter`. Dataset sources are `dryad`, `dataverse`, `figshare`,
`huggingface`, `dataeuropa` and `zenodo`, and the image sources are
`openverse` and `wikimedia`. `semanticscholar` is registered alongside the
scholarly sources, but its anonymous pool answers 429 in practice, so it stays
out of automatic routing. Naming it still runs it.

A further twenty-four sources answer with a record rather than a page:
`pypi`, `npm`, `crates`, `registries`, `appstore`, `endoflife`,
`github_releases`, `nvd`, `osv`, `cisakev`, `wikidata`, `rdap`, `openmeteo`,
`mdn`, `ietf`, `wdi`, `holidays`, `worldclock`, `frankfurter`, `cfets`,
`gleif`, `coingecko`, `federalregister` and `govuk`. They are described under
[Direct facts](#direct-facts-records-instead-of-snippets) below.

> Engines outside the pools above run only when you name them per call, for
> example `engines=["bilibili", "so360"]`, so the default search stays a
> handful of fast HTTP requests. To change the pools themselves, see
> `SEARCH_MCP_DEFAULT_ENGINES` and the settings next to it under
> [Configuration](#configuration).

### Vertical sources (selected automatically by `category`)

These sources index something a general web engine can't. You normally don't
name them, because passing `category=` to `search`/`research` routes to them.
Sources are organised as groups with sub-groups. A bare group widens the
search: one specialist per sub-group joins the web pool until the engine cap is
reached. A dotted sub-group narrows to that branch, and the same cap still
applies.

| `category` | Engines | Why it matters |
|---|---|---|
| `paper` | one per sub-group, below | Searches the literature itself, where a web engine could only filter its results by hostname |
| `paper.index` | `openalex`, `crossref`, `semanticscholar` | Cross-discipline DOI indexes with citation counts |
| `paper.preprint` | `arxiv`, `europepmc` (`SRC:"PPR"`) | Work that has not been peer reviewed yet, flagged as such |
| `paper.biomed` | `europepmc`, `pubmed` | MEDLINE and its 40M-record superset, with open-access full text |
| `paper.cs` | `dblp` | Curated CS bibliography: exact venues, authors, DOIs |
| `paper.openaccess` | `doaj`, `europepmc` | Every hit is free to read in full, so `read_doc` can open it |
| `paper.trial` | `clinicaltrials` | Registered human trials, which hold evidence the literature has not caught up with |
| `paper.math` | `zbmath` | Mathematics literature with reviews and classification |
| `finance` | one per sub-group, below | No general engine indexes filings, quotes or macro series |
| `finance.filings` | `sec_edgar`, `cninfo` | Regulatory filings in full text (US, and A-share/HK) |
| `finance.market` | `yahoofinance` | Ticker resolution plus market news for the resolved instrument |
| `finance.macro` | `worldbank`, `imf` | World Bank research and IMF DataMapper series, WEO forecasts included |
| `github` | `github` (repos + issues/PRs) | Real repository metadata, stars, last push. `github_code` is opt-in: GitHub rejects anonymous code search, so it runs only when named and only with your own token |
| `forum` | `stackexchange`, `hackernews` | Accepted-answer and score signals |
| `news` / `news.world` | `googlenews`, `gdelt` | GDELT covers 100+ languages Google News never surfaces |
| `image` | `openverse`, `wikimedia` | Openly-licensed images; results are direct file URLs, with Wikimedia attribution and source metadata |
| `dataset` | one per sub-group, below | Datasets, software and open-data catalogues |
| `dataset.repository` | `dryad`, `dataverse`, `zenodo`, `figshare` | Research datasets in public repositories, with DOI or landing-page metadata |
| `dataset.ml` | `huggingface` | Machine-learning dataset repositories and dataset cards |
| `dataset.gov` | `dataeuropa` | EU and member-state open-data catalogues in one index |
| `software` | one per sub-group, below | Current versions and support windows from the registries themselves, dated by the publisher |
| `software.lifecycle` | `endoflife` | endoflife.date: release cycles, latest patch and support end dates for about 480 products (Python, Ubuntu, Node.js, PostgreSQL, Kubernetes, ...) |
| `software.github` | `github_releases` | The newest release of a repository, with tag, date and notes; `owner/repo` in the query is looked up directly |
| `software.python` | `pypi` | Current release, upload date and Python requirement of a PyPI package |
| `software.node` | `npm` | Current version and publish date of an npm package |
| `software.rust` | `crates` | Current stable version of a crate |
| `software.registry` | `registries` | Maven Central, RubyGems, the Go proxy, Homebrew, Docker Hub, Packagist and NuGet in one engine; the ecosystem named in the question picks the registry, none named asks nothing |
| `software.app` | `appstore` | An iOS app's current version, release date and notes; a Chinese question searches the Chinese store |
| `security` | `nvd`, `osv` | Vulnerability records instead of blog posts about them |
| `security.cve` | `nvd` | A CVE id gets its NIST record with CVSS score and status; other queries get the CVEs whose descriptions mention the words, newest first |
| `security.package` | `osv` | Advisories that touch a package, with the fixed version, across PyPI, npm, crates.io, Go and more |
| `security.exploited` | `cisakev` | Whether a CVE is in CISA's Known Exploited Vulnerabilities catalogue, with the date added and the due date; a CVE that is not listed gets a result saying so, dated by the catalogue version |
| `reference` | `wikidata`, `rdap` | Structured records: the Wikidata item behind a Wikipedia article, and a domain's registration |
| `reference.domain` | `rdap` | A domain's registration date, expiry, registrar and name servers from the registry's own RDAP service, found through IANA's bootstrap file |
| `weather` | `openmeteo` | Current conditions and a three-day forecast for the place named in the query, fetched at request time |
| `docs` | `mdn`, `ietf` | Reference documentation searched at the source |
| `docs.web` | `mdn` | MDN Web Docs (HTML, CSS, JavaScript, Web APIs, HTTP), in English or Chinese |
| `docs.rfc` | `ietf` | RFCs from the IETF Datatracker by number or title, with standards level and errata flag |
| `finance.fx` | `frankfurter`, `cfets` | The ECB reference rate between two currencies (about 130 other currencies through the open ExchangeRate-API endpoint), and the PBOC's RMB central parity from CFETS; converted for an amount in the query |
| `finance.entity` | `gleif` | The legal entity behind a company name: registered name, addresses, jurisdiction, status, LEI; Chinese legal names as written |
| `finance.crypto` | `coingecko` | A cryptocurrency's spot price in USD and CNY, 24h change and market cap |
| `stats` / `stats.indicator` | `wdi` | A World Bank indicator (GDP, population, inflation, unemployment, life expectancy and 17 more) for a named country, latest three years, with the revision date |
| `calendar` | `holidays`, `worldclock` | Dates and times |
| `calendar.holidays` | `holidays` | Public holidays by country and year; China from the State Council notice (with the make-up working days), elsewhere from Nager.Date |
| `calendar.clock` | `worldclock` | The current date and time in a named city or zone, from the server clock and the IANA database; no request is made |
| `gov` | `federalregister`, `govuk` | Official journals and portals |
| `gov.us` | `federalregister` | US rules, proposed rules and notices with publication date, type and agencies |
| `gov.uk` | `govuk` | GOV.UK guidance, statistics and news with document format and last update |

At the default category-engine limit, `category="dataset"` selects
`dryad`, `huggingface`, and `dataeuropa`, one source from each dataset
sub-group. Use `dataset.repository`, `dataset.ml`, or `dataset.gov` when the
sub-group is known. `category="paper"` selects `arxiv`, `openalex`, and
`europepmc`, and `paper.math` reaches `zbmath` explicitly.

`category="software"` selects `endoflife`, `github_releases` and `pypi`; use
`software.node`, `software.rust`, `software.registry` or `software.app` for
npm, crates.io, the other registries and the App Store, or ask without a
category and let the question's words pick the source.

`image` and `dataset` replace the default pool. A web engine can't return an
image file or a dataset record, so mixing it in only crowds out the specialist
sources that can. `image` has two sources and `dataset` has six across three
sub-groups, so an outage, a rate limit or a missed hit still leaves the
category with a specialist fallback. The other groups augment the web pool,
capped by `SEARCH_MCP_CATEGORY_ENGINE_LIMIT` (default 3). That cap is smaller
than most groups, so a bare group round-robins across its sub-groups before
truncating: `category="paper"` spends its three slots on three different
corpora, where a plain truncation would pick three overlapping DOI indexes.

Results from an engine that natively indexes the requested category count
double in the rank fusion. Without the doubling, `category=` barely affects the
order: a specialist is usually the only source returning a given document, so
its hit loses to three general engines agreeing on a blog post about the
topic.

### Direct facts: records instead of snippets

The groups `software`, `security`, `reference`, `weather`, `docs`, `gov`,
`stats`, `calendar` and the sub-groups `finance.fx`, `finance.entity` and
`finance.crypto` are served by sources that answer with the record itself.
Asked `search("latest fastapi version")`, `pypi` returns one result whose
title is `fastapi 0.141.1 on PyPI` and whose snippet says when that release
was uploaded and which Python it needs, taken from the registry at request
time. The web engines return the PyPI project page too, with whatever version
the crawler saw.

**No category is needed.** Each record source declares, offline and from the
words of the question alone, whether it can answer it (`Engine.claims`): a
CVE id, two currency names, a weather word plus a place, a "latest version"
phrasing, a country plus an indicator. When a search names no category and
no engines, the first three sources that claim the question join the default
pool (`SEARCH_MCP_CLAIM_ENGINE_LIMIT`). They answer in well under a second
and run in parallel with the web engines, so the search is no slower for
them; a false claim costs one fast request that returns nothing. The
response lists them under `auto_routed`, and the markdown says "Record
sources: … answered this question directly". Measured 2026-09-22 on eleven
questions asked without a category ("latest fastapi version", "CVE-2024-3094",
"上海明天天气", "1万日元等于多少人民币", "现在东京几点", "2026年中国放假安排",
"中国 2025 GDP", "btc price", "微信 iOS 版最新版本", "github.com domain expiry"),
the record led in every one, in 2.0 to 7.2 s; "python asyncio tutorial" drew
no claim and ran as before. `category=` still works and still routes, and
`SEARCH_MCP_AUTO_ROUTE_ENABLED=false` turns the claims off.

The first result of such a source counts five times a general engine's in the
rank fusion when it claimed the question or its category was asked for. The
rule: one looked-up record outranks the whole four-engine default pool
agreeing on a page about it, because the page's snippet is a crawl-time
summary and the record is the publisher's own dated value. Later results from
the same source (other candidate names, older advisories) count as ordinary
native hits. Three sources have a lookup mode and a search mode and apply the
weight only to the first: `nvd` and `cisakev` when the query holds a CVE id,
`ietf` when it holds an RFC number. Measured 2026-09-21 on eleven category
probes: with the ordinary doubling the PyPI record ranked below three snippets
of the PyPI project page and Open-Meteo's numbers ranked third behind two
weather portals; with the weight both lead. When a record and a web result are
the same page (`coingecko.com/en/coins/bitcoin` found by DuckDuckGo too), the
merged result keeps the record's title and snippet. A record that won the top
rank is also the `Lead:` line, without the usual test that the snippet echoes
the question: "10000 JPY = 425.70 CNY" answers "1万日元等于多少人民币" without
sharing a word with it.

Each record carries its publisher's date as `published_age`, so the `dated`
count and the per-result `dated …` marker apply. Open-Meteo's date is the
observation time, Frankfurter's is the ECB's fixing date and CFETS's the
trading day, and each snippet says the rate is a daily reference rather than a
live quote. The IETF record claims no date, because the Datatracker's `time`
is the record's last edit. The clock's date is the local date in the zone
asked about.

These sources read the words of the question, so they are sensitive to
phrasing in ways a web engine is not:

- Package names are the query's words minus question words ("latest", "版本",
  "still supported", ecosystem names) and version numbers. `@scope/name` and
  `owner/repo` survive as one token. With no ecosystem named a bare name is
  asked of PyPI and endoflife.date; "npm", "crate", "maven", "gem", "go",
  "brew", "docker", "composer" and "nuget" pick the registry. `github_releases`
  accepts a searched repository only when its name matches one of those
  words, and claims a question only when a repository or GitHub is named,
  because the unauthenticated API allows 60 requests an hour.
- `openmeteo` removes the weather words (weather, forecast, tomorrow, 天气,
  明天) and geocodes what is left; the first match wins.
- `frankfurter` needs two currencies as ISO codes or common names in English
  or Chinese (美元, 人民币, 日元, 英镑, 港币, 欧元, 新台币, 卢布, …); the first
  named is the base, and an amount (`100`, `2.5k`, `1万`) is converted. A pair
  outside the ECB's 31 currencies goes to the open ExchangeRate-API endpoint,
  and the result says so. `cfets` joins for a Chinese question about 人民币 or
  any question about the 中间价.
- `wikidata` removes the fact words (population, area, founded, 人口, 面积)
  and searches for the entity that remains; a Chinese question searches
  Chinese labels and links the Chinese article. `wdi` needs a country it knows
  (about 70, in English or Chinese) and an indicator it knows; "Shanghai
  population" is Wikidata's, "China population" is both.
- `holidays` needs a country and takes the year from the question (明年 and
  "next year" count); a Chinese question with no country means China.
  `worldclock` needs a city, country or zone it knows (about 60 cities); a
  Chinese question with none means Beijing.
- `osv` guesses the ecosystem from words like pip, npm, cargo or go, and asks
  PyPI and npm when there is no hint. `cisakev` keeps the 1.7 MB catalogue in
  memory for six hours and answers from it.
- `rdap` needs a domain and a registration word (expire, registrar, whois,
  域名, 到期); `gleif` a company name and an entity word (legal entity, LEI,
  法人, 注册地); `coingecko` a coin it knows by symbol or name (30 in the
  table, the rest through CoinGecko's search) and a price word; `appstore`
  an app name and an App Store word (iOS 版, app store).

All twenty-four are keyless JSON APIs (the clock makes no request at all) and
stay out of the default pool. Three of them are site searches rather than
lookups (`mdn`, `federalregister`, `govuk`); they take the ordinary native
weight. Candidates for the next round, probed on 2026-09-21 with their limits
and China coverage, are listed in [docs/DATA_SOURCES.md](docs/DATA_SOURCES.md).

### The search deadline

A search returns when every engine has answered or, once at least one engine
has answered with results, when `SEARCH_MCP_SEARCH_DEADLINE_SECONDS` (10) have
passed since the fan-out started. Engines still running are cancelled and
listed under `timed_out_engines` with a hint; their slot counts as an error,
so the breaker benches an engine that keeps timing out. Measured 2026-09-21:
the pool answers in 2 to 4 s and the tail was one browser-rendered engine at
15 s or more. A cancelled engine may be inside a browser render whose teardown
takes seconds; the search waits one second for it and moves on. When nothing
has answered by the deadline the search keeps waiting, because an empty answer
delivered on time is worth less than a late one. `0` disables the deadline.

`engines()` prints this tree live from the registry. The tree is derived from
what each engine declares, so it cannot drift from what runs.

Naming engines explicitly (`engines=[...]`) turns the routing off.

Sources that publish a stricter rate limit than our default declare it
themselves. When such a source's bucket is empty it is skipped, because search
is a parallel fan-out and queueing behind one slow source would add that delay
to every other engine's results.

### When an engine is gated (proxy · stand-ins · login)

Some engines are blocked by the provider: Google serves a JavaScript or
CAPTCHA wall, Mojeek serves a captcha, and Zhihu needs a login. We don't defeat
CAPTCHAs (ToS). The server handles a blocked engine in these ways:

- Stand-ins are automatic. A walled engine is benched and a reserve takes its
  seat. If the search still comes back empty, one bounded rescue pass runs (see
  [Engines](#engines)). Results are attributed to the engine that produced
  them.
- A proxy is the fix for IP gating. Set `SEARCH_MCP_PROXY`
  (`http`/`https`/`socks5`, optional `user:pass@`). It routes the HTTP engines,
  the browser, and `fetch` through a non-blocked IP. Scope it with
  `SEARCH_MCP_PROXY_ENGINES="google zhihu"`.
- The response carries diagnostics. `gated_engines` and `gated_hint` say which
  engine hit what (`captcha` / `consent` / `login` / `javascript`, or
  `off_topic` for a discarded decoy page) and how it was handled.
  `benched_engines` and `benched_hint` say which engines were not asked at all,
  and for how long.
- For Zhihu, log in once: run `uv run search-mcp-login zhihu`. A browser opens,
  you log in, the cookies persist, and `zhihu` search then works. This requires
  a desktop session. (`search-mcp-login codex` is a different sign-in: see
  [search on your ChatGPT plan](#optional-search-on-your-chatgpt-plan-codex).)

When results are thin, the remedies are, in order: rephrase the query, pass
`category=`, name `engines=["so360","baidu"]` for Chinese content or
`engines=["brave"]` / `["startpage"]` if Chromium is installed, and use a proxy
if the diagnostics show a real wall. An agent should not reach for an API key
as the fix; see
[Optional: bring your own key](#optional-bring-your-own-key-manual).

The full guide is [docs/PROXY_AND_GATES.md](docs/PROXY_AND_GATES.md).

> `brave` and `baidu` gate headless browsers after a handful of calls (PoW
> CAPTCHAs, "something went wrong" pages, redirect wrappers). Name them only
> when the default pool can't find what you need.

### Sparse-result diagnostics

When filters drop results so aggressively that ≤3 are returned, the
response includes `filter_diagnostics` so the LLM knows which setting to
relax. Example for `category="forum" + exclude_text="beginner"`:

```text
⚠️ **Filter diagnostics** (results were sparse)
Raw results: 20 across 3 engines → 0 after filters.
Top drops: category_forum (20).
Hint: Filters dropped 20 of 20 raw results. Most were excluded by
category=forum. Try widening or removing one filter.
```

## Install

There are four ways to install, listed in the order most people should try
them. All four run the same server, and none of them asks for an API key.

### 1. The plugin (Claude Code and Codex)

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
[Delegating a lookup](#delegating-a-lookup)).

- The stdio MCP server `search`, started as
  `uvx free-search-mcp==<plugin version>`. Because of that pin the plugin
  version matches the package version: plugin `0.12.0` runs package `0.12.0`,
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

### 2. Claude Desktop: the one-click bundle

From 0.12.0, every [GitHub Release](https://github.com/sweetcornna/free-search-mcp/releases)
carries `free-search-mcp-<version>.mcpb`. Download it and double-click it, or
open it from Claude Desktop's Settings, under Extensions. Claude Desktop runs
the pinned package through `uv` and shows a settings form with three optional
fields: proxy, region and cache directory. The form has no API-key field
because the server does not use one.

To edit the JSON by hand, see
[Wire into Claude Desktop](#wire-into-claude-desktop).

### 3. MCP Registry

From 0.12.0 the release workflow publishes the server to the official MCP
Registry as `io.github.sweetcornna/free-search-mcp`. A client or gateway that installs
from the registry resolves that name to the PyPI package `free-search-mcp`,
run with `uvx` over stdio. The entry declares only the optional proxy, region
and cache-directory variables.

### 4. Alternatives: uvx, a source checkout, Docker

uvx, registered by hand, runs the same published package as the plugin. It
resolves to whatever PyPI holds at first launch, nothing tells you when a newer
version exists, and you do not get the skill. The quick-search agent is one
command away, as [Delegating a lookup](#delegating-a-lookup) shows:

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
[docs/AGENT_USAGE.md](docs/AGENT_USAGE.md).

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

### Config & env vars

All settings are env vars prefixed with `SEARCH_MCP_`. Copy `.env.example` to
`.env` and edit it. The file documents every setting, including the engine
pools (`SEARCH_MCP_DEFAULT_ENGINES` and the reserve / fresh / locale lists).
The full table is under [Configuration](#configuration), and there is a
[usage guide](docs/USAGE.md).

### Tests

```bash
uv run pytest -q                              # offline (default, no network)
SEARCH_MCP_TEST_NETWORK=1 uv run pytest -v    # live tests, hit the real web
```

### Releasing

[docs/RELEASING.md](docs/RELEASING.md) covers cutting a version: every file the
version lives in (including the plugin's pinned package, the registry entry and
the desktop bundle), the tag, PyPI, the GitHub Release, the MCP Registry, and
what to verify afterwards.

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
[Delegating a lookup](#delegating-a-lookup)):

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

### Wire into other clients

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
system-prompt snippet, see [docs/AGENT_USAGE.md](docs/AGENT_USAGE.md).

### Installer choice

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

## Delegating a lookup

An agent can call the tools itself, and for most work that is the right shape.
Delegation is for a narrower case: you want a short answer with sources, and
you want the page text kept out of the caller's context. None of it is on by
default. Every route uses one prompt (`search_mcp.agent.HOST_AGENT_PROMPT`), so
the rules about dates, sources and untrusted page text hold whichever you pick.
Search needs no key on any of them.

| You want | Use | It needs |
|---|---|---|
| The agent to search and read by itself | the tools, as installed | nothing |
| Your host's own subagents to do the lookup | the `quick-search` agent definition | a host with subagents |
| The server to dispatch through Claude Code | `SEARCH_MCP_AGENT_BACKEND=claude-code` | the `claude` CLI, logged in |
| The server to dispatch through Codex | `SEARCH_MCP_AGENT_BACKEND=codex` | the `codex` CLI, logged in |
| The server to call a model endpoint you choose | `SEARCH_MCP_AGENT_BACKEND=api` | an OpenAI-compatible or Anthropic-compatible URL |
| Your own code to supply the model | `ask(question, answer_with=...)` in Python | nothing else |

### With the host's own subagents

In Claude Code the plugin installs `free-search:quick-search`. For a server you
registered by hand, write the agent file yourself:

```bash
uvx free-search-mcp agent-file claude-code > ~/.claude/agents/quick-search.md
```

`--tool-prefix` sets how the host names the server's tools. The default,
`mcp__search__`, matches `claude mcp add search ...`.

For Codex, the same command writes a custom-agent file in the format Codex
documents (`name`, `description`, `developer_instructions`, a read-only sandbox
and low reasoning effort):

```bash
mkdir -p ~/.codex/agents
uvx free-search-mcp agent-file codex > ~/.codex/agents/quick_search.toml
```

One caveat, measured with Codex CLI 0.154 on 2026-09-21: `codex exec` offered
its `spawn_agent` tool without a parameter for choosing an agent by name, so
the file could not be selected from there. Handing the same instructions to a
generic subagent did work in that version. The subagent called the search tools
and returned a sourced answer in 87 s. The `quick_search` MCP prompt returns
that text plus your question for any MCP client, and
`uvx free-search-mcp agent-file prompt` prints it for a platform that has a
system-prompt field and no agent files.

### With the `ask` tool

Set a backend and the server registers a twelfth tool,
`ask(question, freshness, include_domains, category)`. The server first runs
one `research` call, with no model involved, and puts the pages in the model's
first message, so most questions take one model call. The model may then spend
`SEARCH_MCP_AGENT_MAX_STEPS` calls on `search`, `fetch` or `read_doc`, and has
no other tool. When the model fails or the deadline passes, `ask` returns the
pages as a `research` brief together with the reason, so the search is not
wasted. A bad configuration is reported as an error that names the setting.

| Variable | Default | Meaning |
|---|---|---|
| `SEARCH_MCP_AGENT_BACKEND` | `off` | `api`, `claude-code` or `codex` |
| `SEARCH_MCP_AGENT_MODEL` | empty | Required for `api`. Empty means `haiku` on Claude Code and the CLI's own default on Codex |
| `SEARCH_MCP_AGENT_API_PROTOCOL` | `openai` | `openai` (`/chat/completions`) or `anthropic` (`/v1/messages`) |
| `SEARCH_MCP_AGENT_API_BASE_URL` | empty | For `openai`, the URL ending in `/v1`. A local Ollama is `http://localhost:11434/v1` |
| `SEARCH_MCP_AGENT_API_KEY` | empty | The credential of that model endpoint, when it has one. A local endpoint has none |
| `SEARCH_MCP_AGENT_COMMAND` | `claude` / `codex` | Full path of the CLI. Hosts often start MCP servers with a short `PATH` |
| `SEARCH_MCP_AGENT_CLI_ARGS` | backend default | Extra CLI flags, replacing the default. For Codex the default is `--ignore-user-config -c model_reasoning_effort="low"`. If your model provider lives in `config.toml`, keep `--ignore-user-config` and name the provider with `-c` overrides: without that flag the child also loads your own Codex plugins and MCP servers |
| `SEARCH_MCP_AGENT_DEPTH` | `3` | Pages the server reads before the model is called |
| `SEARCH_MCP_AGENT_READ_SECONDS` | `8` | How long those reads may take together. A page still loading after that is left out and its search snippet is used |
| `SEARCH_MCP_AGENT_MAX_STEPS` | `1` | Tool rounds the model may spend afterwards. `0` answers from the first pages only |
| `SEARCH_MCP_AGENT_MAX_SOURCE_CHARS` | `6000` | Characters of each page given to the model |
| `SEARCH_MCP_AGENT_TIMEOUT_SECONDS` | `90` | Deadline for the model part |

The CLI backends start the child with its built-in tools off and with a
three-tool copy of this server. That copy enforces the step limit itself, has
local file reads switched off, and never sees the model endpoint's key. Claude Code runs with extended thinking off, which took the same answer
from 13.7 s to 5.2 s. Codex runs in a read-only sandbox, in an empty directory,
with its own web search disabled so that the answer rests on the pages the
server reports.

Measured on the development machine on 2026-09-21, for the model part only.
The search part took 8 to 9 s cold and nothing when cached. One slow site used
to stretch it: a Baidu Baike page took 25.7 s to fail while the two pages that
held the answer arrived in 0.5 s and 2.1 s, which is why the reads now share a
time budget.

| Backend | First pages were enough | One follow-up call |
|---|---|---|
| `api`, local Ollama `qwen3:1.7b`, no key | 6.4 s | not measured |
| `claude-code`, `haiku` | 4.8 s | 10 to 12 s |
| `codex`, default model, low effort | 13 s | 21 to 24 s |

A small model reads a few pages quickly and can misread them. The tool's own
description tells the calling agent to read the primary page itself for
anything a person will act on.

### Embedding in a service

- Any MCP client can connect over stdio, or over HTTP with
  `--transport streamable-http`.
- `SEARCH_MCP_TOOLS=search,fetch` registers only the named tools, which keeps
  the tool schemas in your model's context to the ones you use.
- `search-mcp ask "question" --json` is the one-shot form for a service that
  would rather start a process. It exits 0 with an answer, 1 when the model
  failed and the pages were printed instead, and 2 when no backend is configured.
- In Python, pass your own model and no setting is needed:

```python
from search_mcp.agent import ask

async def my_model(instructions: str, evidence: str) -> str:
    return await my_llm.complete(system=instructions, user=evidence)

result = await ask("When does registration close?", answer_with=my_model)
print(result["answer"], result["sources"])
```

## Configuration

All settings can be overridden by environment variables prefixed with
`SEARCH_MCP_`. They can live in three places. In order of precedence, highest
first, these are real environment variables, then `./.env` in the launch
directory (source checkouts), then `~/.config/search-mcp/.env` (the stable
location for uvx/PyPI installs; the directory can be changed with
`SEARCH_MCP_CONFIG_DIR`).

Nothing here is required. The setting people most often change is the proxy
(`SEARCH_MCP_PROXY`, see
[docs/PROXY_AND_GATES.md](docs/PROXY_AND_GATES.md)).

Available settings:

| Var | Default | Meaning |
|---|---|---|
| `SEARCH_MCP_DEFAULT_ENGINES` | `["duckduckgo","bing","anysearch","mojeek"]` | JSON list; the pool a search uses when it names no engines |
| `SEARCH_MCP_RESERVE_ENGINES` | `["so360","brave","searx"]` | stand-ins, in order, seated only while fewer than `MIN_HEALTHY_ENGINES` general engines are healthy |
| `SEARCH_MCP_MIN_HEALTHY_ENGINES` | `3` | below this many healthy general engines, reserves are seated |
| `SEARCH_MCP_FRESH_ENGINES` | `["googlenews"]` | joined for `freshness="day"` / `"week"` |
| `SEARCH_MCP_LOCALE_ENGINES` | `{"zh":["so360"]}` | JSON object: engines joined when the QUERY is written in that language |
| `SEARCH_MCP_HEALTH_ENABLED` | `true` | the circuit breaker; `false` asks every engine every time |
| `SEARCH_MCP_ENGINE_COOLDOWN_SECONDS` | `600` | first bench; doubles on each repeat |
| `SEARCH_MCP_ENGINE_COOLDOWN_MAX_SECONDS` | `3600` | ceiling for the doubling |
| `SEARCH_MCP_COHERENCE_GUARD_ENABLED` | `true` | drop a web engine's bucket when it is off-topic and another engine proves the query's words do get echoed |
| `SEARCH_MCP_SEARX_INSTANCES` | *(empty)* | pin known-good SearXNG instance URL(s), comma/space separated; overrides the built-in shortlist |
| `SEARCH_MCP_RESCUE_ENABLED` | `true` | auto-rescue empty searches via rescue engines |
| `SEARCH_MCP_RESCUE_ENGINES` | `["searx","bing"]` | rescue order (JSON list), tried after the reserves |
| `SEARCH_MCP_RESCUE_TIMEOUT` | `10.0` | seconds; cap on the whole rescue pass |
| `SEARCH_MCP_MAX_RESULTS_PER_ENGINE` | `10` | |
| `SEARCH_MCP_RATE_LIMIT_PER_MINUTE` | `30` | per engine |
| `SEARCH_MCP_FETCH_RATE_LIMIT_PER_MINUTE` | `20` | shared `fetch` bucket |
| `SEARCH_MCP_CACHE_DIR` | `~/.cache/search-mcp` | |
| `SEARCH_MCP_CACHE_TTL_SECONDS` | `604800` | 7 days. Searches that asked for recency expire sooner regardless: `freshness=day` 1h, `week` 6h, `month` 24h, `category=news` 6h |
| `SEARCH_MCP_CACHE_MAX_MB` | `512` | size cap on the cache file; `0` disables |
| `SEARCH_MCP_FETCH_STRATEGY` | `auto` | `auto` / `http` / `browser` |
| `SEARCH_MCP_BROWSER_HEADLESS` | `true` | |
| `SEARCH_MCP_BROWSER_POOL_SIZE` | `2` | concurrent pages |
| `SEARCH_MCP_MAX_CONTENT_CHARS` | `50000` | per result truncation |
| `SEARCH_MCP_USER_AGENT` | desktop Chrome UA | used by the httpx (documents) and Playwright paths; the curl_cffi path derives its UA from browser impersonation |
| `SEARCH_MCP_DOWNLOAD_ENABLED` | `true` | set `false` to disable local file downloads |
| `SEARCH_MCP_DOWNLOAD_DIR` | `${SEARCH_MCP_CACHE_DIR}/downloads` | optional directory override; unset or blank uses the dynamic default (`/data/downloads` in Docker) |
| `SEARCH_MCP_DOWNLOAD_TTL_HOURS` | `24` | downloaded files are deleted after this; `0` keeps them forever |
| `SEARCH_MCP_DOWNLOAD_MAX_MB` | `100` | second-layer save cap; effective remote cap is the smaller of this and `SEARCH_MCP_MAX_RESPONSE_BYTES` (25,000,000 bytes by default) |
| `SEARCH_MCP_CATEGORY_ENGINE_LIMIT` | `3` | how many category-native engines `category=` may add |
| `SEARCH_MCP_AUTO_ROUTE_ENABLED` | `true` | without a category, seat the record sources that claim the question (see [Direct facts](#direct-facts-records-instead-of-snippets)) |
| `SEARCH_MCP_CLAIM_ENGINE_LIMIT` | `3` | how many claiming record sources may join a search |
| `SEARCH_MCP_SEARCH_DEADLINE_SECONDS` | `10` | wait this long for the slowest engine once one has answered; `0` waits for all |
| `SEARCH_MCP_CONTACT_EMAIL` | *(empty)* | optional; routes OpenAlex/Crossref/NCBI into their faster identified-caller pools |
| `SEARCH_MCP_TRANSPORT` | `stdio` | `stdio` / `streamable-http` |
| `SEARCH_MCP_HTTP_HOST` | `127.0.0.1` | streamable-http bind address |
| `SEARCH_MCP_HTTP_PORT` | `8000` | streamable-http port |
| `SEARCH_MCP_HTTP_PATH` | `/mcp` | streamable-http endpoint path |
| `SEARCH_MCP_HTTP_ALLOWED_ORIGINS` | *(empty)* | extra `Origin` values the DNS-rebinding guard accepts, comma/space separated (loopback is always allowed) |
| `SEARCH_MCP_PROXY` | *(empty)* | outbound proxy (`http` / `https` / `socks5`, optional `user:pass@`) for the HTTP engines, the browser and `fetch` |
| `SEARCH_MCP_PROXY_ENGINES` | *(empty)* | route only these engines through the proxy; blank = all |
| `SEARCH_MCP_REGION` | `us-en` | `cc-lang` token; a query's own script still decides the locale engines |
| `SEARCH_MCP_TOOLS` | *(empty)* | comma/space separated allow-list of tool names; empty registers all of them |
| `SEARCH_MCP_TOOL_CALL_BUDGET` | `0` | tool calls this process runs before it answers "budget used up" instead; `0` = no cap. Meant for a server started for one job |
| `SEARCH_MCP_AGENT_*` | off | the optional answer agent; see [Delegating a lookup](#delegating-a-lookup) |
| `SEARCH_MCP_CODEX_MODEL` | `gpt-6-luna` | model for the opt-in `codex` engine; see [search on your ChatGPT plan](#optional-search-on-your-chatgpt-plan-codex) |
| `SEARCH_MCP_CODEX_TIMEOUT` | `60` | seconds for one `codex` search |
| `SEARCH_MCP_CODEX_REASONING_EFFORT` | `low` | `low` / `medium` / `high`; used only when the backend has no search endpoint |
| `SEARCH_MCP_CODEX_BASE_URL` | `https://chatgpt.com/backend-api/codex` | the Codex backend |

### Optional: bring your own key (manual)

You do not need this section. It is here because a few people already pay for
a search API and want to point this server at it.

Five engines are **opt-in**: `brave_api`, `serper`, `tavily`, `google_cse` and
`github_code`. They are in no pool and no `category=` route. One runs only when
a call names it (`engines=["serper"]`) and the operator of this machine has set
their own key for it. Named without a key, the engine returns an error that
says the search itself is fine, names the keyless alternative, and tells the
agent not to ask the user for a key. Agents should never ask for one, because
the server is built to work without.

| Engine | Provider | What the key is |
|---|---|---|
| `brave_api` | [Brave Search API](https://brave.com/search/api/) | `SEARCH_MCP_BRAVE_API_KEY` |
| `serper` | [Serper](https://serper.dev) | `SEARCH_MCP_SERPER_API_KEY` |
| `tavily` | [Tavily](https://app.tavily.com) | `SEARCH_MCP_TAVILY_API_KEY` |
| `google_cse` | [Google Custom Search](https://programmablesearchengine.google.com/) | `SEARCH_MCP_GOOGLE_CSE_API_KEY` + `SEARCH_MCP_GOOGLE_CSE_CX` |
| `github_code` | [GitHub](https://github.com/settings/tokens) | `SEARCH_MCP_GITHUB_TOKEN`. GitHub rejects anonymous code search; the keyless `github` engine covers repos and issues |

Three keyless engines accept an optional credential that only raises their
limits: `anysearch`, `stackexchange`, and `semanticscholar` (whose anonymous
pool answers 429 in practice, so it joins `category="paper"` routing only once
a free key is set).

Set a key either as an environment variable / `.env` line (these win), or on
the local settings page:

```bash
uv run search-mcp-admin        # local settings / 本地设置 — http://127.0.0.1:8765
```

(`uvx --from free-search-mcp search-mcp-admin` for a plugin or uvx install.)
The page is bilingual (中英双语), binds to `127.0.0.1` only, and opens with the
**Network / Proxy / 网络 / 代理** card. The provider cards below it have masked
fields with **Save / 保存** (applies live, with no server restart),
**Test / 测试** and **Clear / 清除**. Values are written to
`~/.config/search-mcp/config.json` (`0600`) and never echoed back to the page.
Set `SEARCH_MCP_ADMIN_NO_BROWSER=1` to stop it opening a browser tab.
The per-provider walkthrough is [docs/API_KEYS.md](docs/API_KEYS.md).

### Optional: search on your ChatGPT plan (`codex`)

You do not need this either. If you already have a ChatGPT plan that includes
Codex, the opt-in `codex` engine runs OpenAI's own web search, the one Codex
uses, on that plan. There is no API key and no API bill; each search counts
against the plan's Codex usage. OpenAI supports signing in with ChatGPT from
third-party tools.

```bash
uvx --from free-search-mcp search-mcp-login codex   # browser sign-in, the same page as `codex login`
uvx --from free-search-mcp search-mcp-login status
```

(`uv run search-mcp-login codex` in a source checkout, or **Sign in / 登录** on
the settings page.) Then name it: `search("...", engines=["codex"])`.

- The tokens are stored at `~/.config/search-mcp/oauth/codex.json` (`0600`)
  and refreshed automatically.
- Already signed in to the Codex CLI? `search-mcp-login codex --use-codex-cli`
  links to its `auth.json` read-only instead.
- On a remote machine, use `--no-browser`, sign in anywhere, and paste the
  address the browser lands on (`http://127.0.0.1:1455/auth/callback?code=…`)
  back into the terminal.
- It is opt-in exactly like the key engines: in no pool and no route, and
  named before a sign-in it returns an error, not results. The sign-in and the
  searches honour `SEARCH_MCP_PROXY`.

There is no Antigravity equivalent. Google's Antigravity terms call any
third-party use of Antigravity OAuth a breach and Google suspends accounts for
it, and the Gemini API's search-grounding terms forbid collecting and caching
the links. The full guide, with a Chinese summary (中文速览), is
[docs/CODEX_SEARCH.md](docs/CODEX_SEARCH.md).

## Architecture

```
   ┌─────────────────────────────────────────────────────┐
   │  MCP server (stdio | streamable-http)               │
   │  tools: search / research / compare / paper_graph / │
   │         fetch / fetch_batch / read_doc / download / │
   │         extract_structured / cache_search / engines │
   └────────────┬────────────────────────────────────────┘
                │
   ┌────────────▼────────────┐  ┌────────────────────────┐
   │  aggregator             │  │  fetcher               │
   │  - parallel engines     │  │  - HTTP fast path      │
   │  - reciprocal rank      │  │  - playwright fallback │
   │    fusion               │  │  - markdownify         │
   │  - search cache (FTS5)  │  │  - page cache (FTS5)   │
   │  - health-aware pool    │  │  - fetched_at / dates  │
   │  - off-topic guard      │  │                        │
   └────┬────────────────────┘  └────────────┬───────────┘
        │                                    │
   ┌────▼─────────────────┐  ┌──────────────▼─────────────┐
   │  engines/            │  │  browser pool              │
   │   duckduckgo.py      │  │   - persistent context     │
   │   mojeek.py          │  │   - stealth init script    │
   │   searx.py           │  │   - shared cookies         │
   │   startpage.py (opt) │  │   - semaphore-bounded pages│
   │   brave.py     (opt) │  └────────────────────────────┘
   │   bing.py            │
   │   baidu.py     (opt) │
   │   google.py    (opt) │
   │   serpsearch.py(opt) │
   │   anysearch.py       │
   │   bilibili.py  (opt) │
   │   zhihu.py     (opt) │
   │   sogou.py     (opt) │
   │   so360.py  (reserve)│
   │   arxiv/openalex/    │
   │   crossref/pubmed    │
   │   github/stackexch.  │
   │   hackernews/gdelt   │
   │   wikipedia/openlib. │
   │   openverse/zenodo   │
   │   dryad/dataverse    │
   │   figshare           │
   │   huggingface        │
   │   dataeuropa         │
   │   wikimedia/zbmath   │
   └──────────────────────┘

   ┌────────────────────────────┐    ┌──────────────────┐
   │  documents/                │    │  ratelimit       │
   │   pypdf, python-docx,      │    │   token bucket   │
   │   markdownify              │    │   per engine     │
   └────────────────────────────┘    └──────────────────┘

   ┌────────────────────────────┐    ┌──────────────────┐
   │  formatting                │    │  research        │
   │   token estimate           │    │   composed       │
   │   smart truncation         │    │   workflow       │
   │   markdown renderers       │    │                  │
   └────────────────────────────┘    └──────────────────┘
```

### Engine adapter pattern

Each engine in `src/search_mcp/engines/` implements:

```python
class Engine:
    name: str
    needs_browser: bool          # Force Playwright?
    wait_selector: str | None    # CSS to wait for in browser mode

    def build_url(self, query: str, max_results: int) -> str: ...
    def parse(self, html: str) -> list[SearchResult]: ...
```

The base class handles transport (an HTTP request first, with a Playwright
fallback), rate limiting, and the case where HTTP returns a captcha shell
instead of results (it retries through the browser).

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
