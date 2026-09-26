# Usage guide

This guide covers the `free-search-mcp` tools, engine selection and filters.
For install and deploy steps, see the [Quick start](#quick-start) below or the
[README](../README.md).

## Quick start

The recommended install is the plugin: 11 MCP tools, a source-verification
skill and a small quick-search agent, pinned to one version, with no API key.
The `search` server and the `verified-research` skill load in Claude Code and
in Codex (checked with Codex CLI 0.154). In Claude Code the
`free-search:quick-search` agent answers one lookup in its own context on
`haiku` and returns a few sentences with dated source URLs. Other hosts get the
same agent as a file or a prompt, and an optional `ask` tool lets the server do
the delegating. The README's
[Delegating a lookup](../README.md#delegating-a-lookup) covers every route.

Claude Code:

```text
/plugin marketplace add sweetcornna/free-search-mcp
/plugin install free-search@free-search-mcp
```

Codex:

```bash
codex plugin marketplace add sweetcornna/free-search-mcp
codex plugin add free-search@free-search-mcp
```

If `~/.codex/config.toml` still has a `[mcp_servers.search]` entry from an
earlier `codex mcp add`, it silently shadows the plugin's server. Run
`codex mcp remove search` first.

From 0.12.0, each GitHub Release carries a one-click `.mcpb` bundle for Claude
Desktop, and the release workflow publishes the server to the MCP Registry as
`io.github.sweetcornna/free-search-mcp`. The [README](../README.md#install)
describes both.

The rest of this section is the source-checkout route, for working on the
code:

```bash
# one-line setup for Claude Code
curl -LsSf https://raw.githubusercontent.com/sweetcornna/free-search-mcp/main/scripts/install.sh | bash -s -- --client claude-code

# one-line setup for Codex
curl -LsSf https://raw.githubusercontent.com/sweetcornna/free-search-mcp/main/scripts/install.sh | bash -s -- --client codex

# local checkout / install only
./scripts/install.sh --client none
```

For Codex, Cursor, Cline, Continue, Zed and generic agent operating rules, see
[AGENT_USAGE.md](AGENT_USAGE.md).

Wire into Claude Code: this repo ships a `.mcp.json`, so running `claude`
inside the project auto-detects the `search` server. To register it globally:

```bash
claude mcp add search -s user -- uv --directory /absolute/path/to/free-search-mcp run search-mcp
```

Wire into Codex:

```bash
codex mcp add search -- uv --directory /absolute/path/to/free-search-mcp run search-mcp
codex mcp list
```

Wire into Claude Desktop by adding this to `claude_desktop_config.json`:

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

Docker (containerized, stdio):

```bash
docker compose build
docker compose run --rm search-mcp
```

## Tools

| Tool | What it does |
|---|---|
| `search(query, engines?, max_results?, ...filters)` | Parallel multi-engine search, merged with RRF and deduplicated, with an optional `lead_snippet`. |
| `research(question, depth?, ...filters)` | Searches, fetches the top N results and returns a Markdown brief in one call. |
| `paper_graph(paper, direction?, limit?)` | One paper's citation graph: what it cites and what cites it (ranked by influence), plus Crossref retraction and correction notices. Accepts a DOI, an OpenAlex ID, an exact title, or an arXiv reference: a bare id such as `1706.03762`, `2401.12345v3` or `hep-th/9901001` when it is the whole input, `arXiv:1706.03762`, an `arxiv.org/abs/…` URL, or a `10.48550/arXiv.…` DOI. |
| `compare(question, urls=[2..5])` | Concurrent fetch of 2 to 5 URLs, with side-by-side excerpts. |
| `fetch(url, render?, inline?, ...)` | Fetch any resource: reader-mode Markdown for pages, parsed text for documents, or a description (type/size/dimensions/sha256) for images and binaries. `inline=True` returns the image itself for a vision model. |
| `fetch_batch(urls, ...)` | Concurrent multi-URL fetch (max 20 per call). |
| `read_doc(source, start?, length?, ...)` | Parse PDF / DOCX / XLSX / PPTX / EPUB / CSV / source code / zip-tar / HTML / TXT / MD with pagination. |
| `extract_structured(url, ...)` | JSON-LD / OpenGraph / Twitter cards / microdata. Long prose fields are clipped and the result says so (`trimmed`); use `fetch` for the text. |
| `cache_search(query, limit?, ...)` | FTS5 search across previously fetched pages. |
| `engines(group?)` | The source tree (group, then sub-group, then engine) with one line of description each. `group` is an enum of the groups that own engines. |
| `download(url, ...)` | Save a file to `${SEARCH_MCP_CACHE_DIR}/downloads` by default; files auto-delete after 24h. Set `SEARCH_MCP_DOWNLOAD_ENABLED=false` to disable it. |

All tools default to `format="markdown"`; pass `format="json"` for structured
output. Markdown arrives as one plain text block. JSON arrives as the
structured content itself, unwrapped, except that a list-valued answer keeps a
`{"result": [...]}` envelope. A truncated `fetch` carries a `cache://page/…`
resource link to the full cached text.

### Reading the output: current? primary?

Search snippets locate sources, and the facts are on the page. The output
reports what it knows about each result's date and origin:

| Signal | Meaning |
|---|---|
| `retrieved_at` | when the engines (or the page) were asked; on a cache hit, the original time |
| `cache_age_seconds` | cache hits only. Markdown: `(cached 2 days ago · retrieved …)` |
| `dated_results` | how many results carry any date, shown in the header as `dated: N/M` |
| `date_source` (per result) | `structured` is shown as `dated 2026-09-02`, `snippet` as `2026-09-05 (from snippet text)`, and `none` as `undated` |
| `source_type` (per result) | `paper` / `code` / `forum` / `news` / `government` / `academic`, from the hostname; never a ranking input |
| `freshness_note` | present when `freshness=` was given and at least half the results are undated, because `freshness` keeps undated results |
| `usage_note` | the one fixed reminder that a snippet is a lead |
| `date_note` (`research`) | how many of the brief's sources could be dated and how far apart they are |

`fetch` prints `published <date>` or `no publication date found`, and the cache
age on a hit. Cached searches expire faster when recency was asked for:
`freshness="day"` after 1 hour, `week` after 6 hours, `month` after 24 hours,
and `category="news"` after 6 hours. `weather` and `finance.fx` answers expire
after 1 hour and `software` and `security` after 6 hours whatever the query
said, because those records move on their own clock. The default is 7 days.

The intended workflow is to search to find URLs, fetch or research the primary
page, check the date, and corroborate what matters.

## Engines

`search` and `research` accept an `engines=[...]` list. Omit it and the server
picks a small pool for you. Every engine a search reaches on its own is keyless
(no API key, no account).

The registry contains 74 engines in total.

The default pool is `duckduckgo`, `bing`, `anysearch` and `mojeek`. All four
are plain HTTP and need no browser.

Two engines join only when the query calls for it:

- `googlenews` joins for `freshness="day"` / `"week"` or `category="news"`. It
  stays out of the default pool because a news feed suits a web search only
  when recency was asked for.
- `so360` joins when the query itself is written in Chinese.

The reserves (`so360`, `brave`, `searx`, in that order) are seated only while
fewer than 3 general engines are healthy.

The circuit breaker benches an engine at once when it serves a wall or an
off-topic page. Errors bench it after 2 in a row, and silent zero-result
answers after 3. The bench lasts 10 minutes, doubling up to 60, and one success
clears it. Naming an engine in `engines=` always runs it. The response lists
`benched_engines` (Markdown: "Benched engines").

When the pool still comes back empty, or nearly empty with gated or erroring
engines, the aggregator runs one bounded rescue pass (the reserves, then
`searx`, then `bing`) and reports it as `rescued_via`.

Engines outside the default pool, reached by name:

| Engine | Source | Notes |
|---|---|---|
| `startpage` | Startpage | browser-rendered, about 5 to 10 s |
| `brave` `baidu` | resp. engines | browser-rendered; intermittently challenge headless clients |
| `searx` | public SearXNG instances | meta-search; public instances often slow |
| `google` | Google web SERP scrape | falls back from HTTP to the browser; hit a JavaScript wall on every request when measured on 2026-09-21, so it is no use as a recovery path |
| `serpsearch` | alias of `google` | identical behavior (all real SERP APIs need a key) |
| `bilibili` | Bilibili (哔哩哔哩) JSON API | keyless video search (synthetic `buvid3` cookie); video results only |
| `zhihu` | Zhihu (知乎) search page | best effort, browser-rendered; Zhihu hard-gates bots, so a login wall or an empty result is common and is reported as such |
| `sogou` | 搜狗 HTML scrape | best effort; returns redirect URLs (`sogou.com/link?url=…`) instead of target URLs |
| `so360` | 360搜索 HTML scrape | returns direct URLs; joins automatically for Chinese queries |
| `wikipedia` | MediaWiki API | language follows `SEARCH_MCP_REGION` |
| `openlibrary` | Open Library | book search |

`anysearch`, the anonymous tier of the
[AnySearch](https://github.com/anysearch-ai/anysearch-mcp-server) REST API, is
in the default pool. It is IP rate-limited, and it ignores `site:` /
`filetype:` operators, so domain filters are applied to its results afterwards.

The scholarly and financial sources (`arxiv`, `openalex`, `crossref`,
`pubmed`, `europepmc`, `dblp`, `doaj`, `clinicaltrials`, `zbmath`,
`sec_edgar`, `yahoofinance`, `cninfo`, `worldbank`, `imf`, `frankfurter`) are
keyless too. Reach them with `category=`; you do not need to name them.
Dataset sources are `dryad`, `dataverse`, `figshare`, `huggingface`,
`dataeuropa`, and `zenodo`; image sources are `openverse` and `wikimedia`.
Twenty-four sources answer with a record rather than a page (`pypi`, `npm`,
`crates`, `registries`, `appstore`, `endoflife`, `github_releases`, `nvd`,
`osv`, `cisakev`, `wikidata`, `rdap`, `openmeteo`, `mdn`, `ietf`, `wdi`,
`holidays`, `worldclock`, `frankfurter`, `cfets`, `gleif`, `coingecko`,
`federalregister`, `govuk`); see
[Direct facts](#direct-facts-records-instead-of-snippets). They join a search
on their own when the question's words fit them, category or not.

### Vertical sources: selected automatically by `category=`

You rarely need to name these sources. They are organised in two levels, group
and sub-group. A bare group widens the search: one specialist per sub-group
joins the pool until the engine cap. A dotted sub-group narrows to that branch,
and the same cap applies.

| `category` | Engines | Notes |
|---|---|---|
| `paper` | one per sub-group | real literature search, structured publication dates |
| `paper.index` | `openalex`, `crossref`, `semanticscholar` | cross-discipline DOI indexes with citation counts |
| `paper.preprint` | `arxiv`, `europepmc` | not peer reviewed, and labelled `PREPRINT` in the snippet |
| `paper.biomed` | `europepmc`, `pubmed` | MEDLINE plus its 40M-record superset |
| `paper.cs` | `dblp` | curated CS bibliography with exact venue, authors and DOI |
| `paper.openaccess` | `doaj`, `europepmc` | full text is free to read, so `read_doc` can open it |
| `paper.trial` | `clinicaltrials` | registered trials, with phase / status / sponsor |
| `paper.math` | `zbmath` | mathematics literature with reviews and classification |
| `finance` | one per sub-group | filings, market data and macro answer different questions |
| `finance.filings` | `sec_edgar`, `cninfo` | US regulatory filings; A-share / HK announcements |
| `finance.market` | `yahoofinance` | ticker resolution + news about the resolved instrument |
| `finance.macro` | `worldbank`, `imf` | World Bank documents; IMF series incl. WEO forecasts |
| `github` | `github` | repos + issues/PRs. `github_code` is opt-in (named explicitly, with your own token) because GitHub rejects anonymous code search |
| `forum` | `stackexchange`, `hackernews` | accepted-answer / score signals |
| `news`, `news.world` | `googlenews`, `gdelt` | GDELT covers 100+ languages; strictly rate-limited, skipped when the limit is hit and never queued |
| `image` | `openverse`, `wikimedia` | CC-licensed/direct file results; Wikimedia also returns attribution and source metadata |
| `dataset` | one per sub-group | datasets, software, and open-data catalogues |
| `dataset.repository` | `dryad`, `dataverse`, `zenodo`, `figshare` | public research-dataset repositories with DOI or landing-page metadata |
| `dataset.ml` | `huggingface` | machine-learning dataset repositories and dataset cards |
| `dataset.gov` | `dataeuropa` | EU and member-state open-data catalogues in one index |
| `software` | one per sub-group | current versions and support windows, from the registries |
| `software.lifecycle` | `endoflife` | endoflife.date: release cycles, latest patch, support end dates (about 480 products) |
| `software.github` | `github_releases` | newest release of a repository: tag, date, notes; `owner/repo` in the query is looked up directly |
| `software.python` | `pypi` | current release, upload date, Python requirement |
| `software.node` | `npm` | current version and publish date |
| `software.rust` | `crates` | current stable version of a crate |
| `software.registry` | `registries` | Maven Central, RubyGems, Go proxy, Homebrew, Docker Hub, Packagist, NuGet; the ecosystem named picks the registry |
| `software.app` | `appstore` | an iOS app's current version, release date and notes |
| `security` | `nvd`, `osv` | vulnerability records |
| `security.cve` | `nvd` | a CVE id gets its NIST record with CVSS; other queries get CVEs whose descriptions mention the words, newest first |
| `security.package` | `osv` | advisories touching a package, with the fixed version (PyPI, npm, crates.io, Go, ...) |
| `security.exploited` | `cisakev` | whether a CVE is in CISA's KEV catalogue; an absent CVE gets a result saying so |
| `reference` | `wikidata`, `rdap` | structured records: the Wikidata item behind an article; a domain's registration |
| `reference.domain` | `rdap` | registration date, expiry, registrar and name servers from the registry's RDAP service |
| `weather` | `openmeteo` | current conditions and a three-day forecast for the named place, fetched now |
| `docs` | `mdn`, `ietf` | reference documentation at the source |
| `docs.web` | `mdn` | MDN Web Docs, English or Chinese |
| `docs.rfc` | `ietf` | RFCs by number or title, with standards level and errata flag |
| `finance.fx` | `frankfurter`, `cfets` | ECB reference rate (other currencies via the open ExchangeRate-API endpoint) and the PBOC's RMB central parity |
| `finance.entity` | `gleif` | legal entity by name: registered name, addresses, jurisdiction, status, LEI |
| `finance.crypto` | `coingecko` | spot price in USD and CNY, 24h change, market cap |
| `stats`, `stats.indicator` | `wdi` | a World Bank indicator for a named country, latest three years |
| `calendar.holidays` | `holidays` | public holidays by country and year; China from the State Council notice with make-up working days |
| `calendar.clock` | `worldclock` | current date and time in a named city or zone, from the server clock |
| `gov` | `federalregister`, `govuk` | official journals and portals |
| `gov.us` | `federalregister` | US rules, proposed rules and notices with date, type and agencies |
| `gov.uk` | `govuk` | GOV.UK guidance, statistics and news with format and last update |

At the default category-engine limit, `category="dataset"` selects `dryad`,
`huggingface` and `dataeuropa`, one source from each dataset sub-group. Use a
dotted token when the sub-group is known: `dataset.repository` for repository
catalogues, `dataset.ml` for Hugging Face, `dataset.gov` for data.europa.eu, or
`paper.math` for zbMATH. A bare group is broader and round-robins across its
sub-groups before the cap truncates; `category="paper"` selects `arxiv`,
`openalex` and `europepmc`, and `category="software"` selects `endoflife`,
`github_releases` and `pypi` (use `software.node` or `software.rust` for npm
and crates.io).

### Direct facts: records instead of snippets

`software`, `security`, `reference`, `weather`, `docs`, `gov`, `stats`,
`calendar`, `finance.fx`, `finance.entity` and `finance.crypto` are served by
sources that return the record itself, dated by its publisher:
`search("latest fastapi version")` gets `fastapi 0.141.1 on PyPI` with the
upload date in the snippet, taken from the registry at request time.

No category is needed. Each record source decides from the words of the
question whether it can answer it (a CVE id, two currencies, a weather word
and a place, a "latest version" phrasing, a country and an indicator), and
the first three that say yes join the default pool for that search
(`SEARCH_MCP_CLAIM_ENGINE_LIMIT`). They run beside the web engines and answer
in well under a second. The response lists them as `auto_routed`. A category
still routes as before; `SEARCH_MCP_AUTO_ROUTE_ENABLED=false` turns the claims
off.

The first result of such a source counts five times a general engine's in
the rank fusion, so one looked-up record outranks the four-engine default
pool agreeing on a page about it, and it becomes the `Lead:` line. `nvd`,
`cisakev` and `ietf` apply that weight only when the query holds a CVE id or
an RFC number; `mdn`, `federalregister` and `govuk` are site searches and take
the ordinary native weight.

The lookups read the words of the question. Package names are the query minus
question words ("latest", "版本", "still supported", ecosystem names), with
`@scope/name` and `owner/repo` kept whole. `openmeteo` geocodes what is left
after removing the weather words (weather, forecast, tomorrow, 天气, 明天).
`frankfurter` needs two currencies as codes or names (美元, 人民币, 日元,
英镑, 港币, 欧元); the first is the base and an amount (`100`, `2.5k`, `1万`)
is converted; the ECB publishes 31 currencies. `wikidata` removes fact words
(population, area, 人口, 面积) and searches for the entity; a Chinese question
gets Chinese labels and the Chinese article. `osv` guesses the ecosystem from
pip, npm, cargo, go and asks PyPI and npm when there is no hint.

```text
search("latest fastapi version")                # pypi and endoflife claim it
search("is python 3.9 still supported")         # endoflife
search("npm express latest version")            # npm
search("rails gem latest version")              # registries (RubyGems)
search("微信 iOS 版最新版本")                       # appstore, Chinese store
search("CVE-2024-3094")                         # nvd, osv, cisakev
search("requests vulnerabilities")              # osv
search("上海 人口")                               # wikidata
search("中国 2025 GDP")                           # wdi and wikidata
search("上海明天天气")                              # openmeteo
search("现在东京几点")                              # worldclock, no request made
search("2026年中国放假安排")                         # holidays, State Council notice
search("1万日元等于多少人民币")                       # frankfurter (ECB) and cfets (PBOC)
search("btc price")                             # coingecko
search("github.com domain expiry")              # rdap
search("Array.prototype.toSorted", category="docs.web")
search("RFC 9110", category="docs.rfc")
search("skilled worker visa salary threshold", category="gov.uk")
```

A search also stops waiting for its slowest engine ten seconds after the
fan-out started, once at least one engine has answered with results
(`SEARCH_MCP_SEARCH_DEADLINE_SECONDS`, `0` to wait for all). Cancelled engines
are listed as `timed_out_engines`.

`image` and `dataset` replace the default pool; the other categories add to it,
capped by `SEARCH_MCP_CATEGORY_ENGINE_LIMIT` (default 3). A web engine cannot
return an image file or a dataset record, so the exclusive categories keep only
specialist sources. There are two image sources and six dataset sources across
three sub-groups, so one outage, rate limit or missed hit does not exhaust
specialist search. Passing `engines=` disables the routing.

Results from an engine that natively indexes the requested category count
double in the rank fusion, so `category=` changes the order of results as well
as which engines run. A specialist is usually the only source returning a given
document, and without the weighting its hit would lose to three general engines
agreeing on a blog post about the topic.

`engines()` prints this table live from the registry, so it cannot drift.

To change the default pool globally, set `SEARCH_MCP_DEFAULT_ENGINES` (a JSON
list) in `.env`.

### Examples

```text
# English web search, default pool
search("reciprocal rank fusion")

# Chinese web: name the Chinese indexes (so360 also joins on its own)
search("RoboMaster 2026 报名时间", engines=["so360", "baidu"])

# Recent news: googlenews joins because recency was asked for
search("uv package manager release", freshness="week")

# Chinese video search on Bilibili
search("python 教程", engines=["bilibili"])

# Mix CJK verticals + general web
search("transformer 架构", engines=["bilibili", "zhihu", "duckduckgo"])

# Restrict to one site — a filter, not an engine choice
search("asyncio task groups", include_domains=["python.org"])
```

> Measured status on 2026-09-21: `duckduckgo`, `bing`, `anysearch` and `so360`
> return live, on-topic results. `mojeek` is captcha-walled on every request,
> so it is benched after one attempt and re-probed later. `google` and
> `serpsearch` hit a JavaScript wall; `startpage` and `sogou` returned nothing.
> `zhihu` frequently hits a login wall and returns empty, which is the expected
> result without a login.

When results are thin, try these in order:

1. Rephrase the query.
2. Pass `category=`.
3. Name `engines=["so360","baidu"]` for Chinese content, or
   `engines=["brave"]` / `["startpage"]` if Chromium is installed.
4. Set a proxy (`SEARCH_MCP_PROXY`) when the diagnostics show a real wall.

Zhihu needs a one-time `search-mcp-login zhihu`. The response reports
`gated_engines` and `gated_hint` when an engine was walled, or when its results
were discarded as `off_topic`. An `off_topic` verdict is not a block, and a
proxy does not change it. `benched_engines` lists engines that were not asked.
See [PROXY_AND_GATES.md](PROXY_AND_GATES.md).

## Filters (search / research)

| Param | Values | Effect |
|---|---|---|
| `freshness` | `day` / `week` / `month` / `year` | only results from the last N; undated results are kept, and `day` / `week` add `googlenews` |
| `include_domains` | `["python.org"]` | restrict to these domains |
| `exclude_domains` | `["pinterest.com"]` | remove these |
| `category` | a group (`news` / `pdf` / `github` / `paper` / `forum` / `blog` / `image` / `dataset` / `finance` / `software` / `security` / `reference` / `weather` / `docs` / `gov` / `stats` / `calendar`) or a sub-group (`paper.biomed`, `finance.filings`, `finance.fx`, `software.python`, `security.cve`, `dataset.ml`, …) | content-type shortcut and routing signal: it sends the query to sources that natively index it (see [Vertical sources](#vertical-sources-selected-automatically-by-category)) |
| `include_text` | `"async"` | substring required in title/snippet |
| `exclude_text` | `"beginner"` | substring forbidden |
| `max_age_hours` | `24` | accept a cached answer only if younger than this (default 7 days; the tightest of this, the `freshness` TTL, the news cap and the category cap wins) |

```text
research("LLM eval frameworks", depth=3, freshness="month", category="paper")
search("kubernetes operators", include_domains=["github.com"], category="github")
search("CRISPR base editing", category="paper.preprint")
search("graph neural network datasets", category="dataset.ml")
search("Riemann hypothesis", category="paper.math")
search("NVDA 10-K risk factors", category="finance.filings")
paper_graph("10.1145/1571941.1572114")               # references + citing works
paper_graph("arXiv:1706.03762")                      # also arxiv.org URLs and 10.48550/arXiv.… DOIs
```

When filters leave 3 or fewer results, the response includes
`filter_diagnostics`, which says which filter to relax.

## Configuration

Copy `.env.example` to `.env` and edit it. Every setting is an env var prefixed
with `SEARCH_MCP_`, and `.env.example` has the full annotated list. The common
ones:

| Var | Default | Meaning |
|---|---|---|
| `SEARCH_MCP_DEFAULT_ENGINES` | `["duckduckgo","bing","anysearch","mojeek"]` | the pool used when a search names no engines (JSON list) |
| `SEARCH_MCP_RESERVE_ENGINES` | `["so360","brave","searx"]` | stand-ins, seated only while fewer than `SEARCH_MCP_MIN_HEALTHY_ENGINES` (default `3`) general engines are healthy |
| `SEARCH_MCP_FRESH_ENGINES` | `["googlenews"]` | joined for `freshness="day"` / `"week"` |
| `SEARCH_MCP_LOCALE_ENGINES` | `{"zh":["so360"]}` | JSON object: engines joined when the query is written in that language |
| `SEARCH_MCP_HEALTH_ENABLED` | `true` | the circuit breaker |
| `SEARCH_MCP_ENGINE_COOLDOWN_SECONDS` / `_MAX_SECONDS` | `600` / `3600` | first bench, and the ceiling it doubles towards |
| `SEARCH_MCP_COHERENCE_GUARD_ENABLED` | `true` | the off-topic bucket guard |
| `SEARCH_MCP_PROXY` / `SEARCH_MCP_PROXY_ENGINES` | *(empty)* | outbound proxy, and an optional list of engines to scope it to |
| `SEARCH_MCP_FETCH_STRATEGY` | `auto` | `auto` / `http` / `browser` |
| `SEARCH_MCP_SAFESEARCH` | `moderate` | `strict` / `moderate` / `off` |
| `SEARCH_MCP_REGION` | `us-en` | `cc-lang` token |
| `SEARCH_MCP_CACHE_TTL_SECONDS` | `604800` | 7 days |
| `SEARCH_MCP_CATEGORY_ENGINE_LIMIT` | `3` | how many category-native engines `category=` may add |
| `SEARCH_MCP_AUTO_ROUTE_ENABLED` | `true` | seat the record sources that claim a question asked without a category |
| `SEARCH_MCP_CLAIM_ENGINE_LIMIT` | `3` | how many claiming record sources may join a search |
| `SEARCH_MCP_SEARCH_DEADLINE_SECONDS` | `10` | wait this long for the slowest engine once one has answered; `0` waits for all |
| `SEARCH_MCP_CONTACT_EMAIL` | *(empty)* | optional; OpenAlex/Crossref/NCBI route identified callers to a faster pool |
| `SEARCH_MCP_DOWNLOAD_ENABLED` | `true` | set `false` to disable local file downloads |
| `SEARCH_MCP_DOWNLOAD_DIR` | `${SEARCH_MCP_CACHE_DIR}/downloads` | optional directory override; unset or blank uses the dynamic default (`/data/downloads` in Docker) |
| `SEARCH_MCP_DOWNLOAD_TTL_HOURS` | `24` | downloaded files are deleted after this; `0` keeps them forever |
| `SEARCH_MCP_DOWNLOAD_MAX_MB` | `100` | second-layer save cap; effective remote cap is the smaller of this and `SEARCH_MCP_MAX_RESPONSE_BYTES` (25,000,000 bytes by default) |
| `SEARCH_MCP_TRANSPORT` | `stdio` | `stdio` / `streamable-http` |
| `SEARCH_MCP_HTTP_HOST` / `_PORT` / `_PATH` | `127.0.0.1` / `8000` / `/mcp` | streamable-http bind settings |
| `SEARCH_MCP_TOOLS` | *(empty)* | comma/space separated allow-list of tool names; empty registers all 11 |
| `SEARCH_MCP_AGENT_BACKEND` | `off` | `api`, `claude-code` or `codex` registers the `ask` tool; the other `SEARCH_MCP_AGENT_*` settings are in the README's [Delegating a lookup](../README.md#delegating-a-lookup) |

### Optional: bring your own key (manual)

Nothing above needs a key, and nothing in the default, reserve, rescue, fresh,
locale or category pools ever uses one. Five engines are **opt-in**: they run
only when a call names them and the operator has set their own key.

| Engine | Provider | Needs |
|---|---|---|
| `brave_api` | Brave Search API | `brave_api_key` |
| `serper` | Serper | `serper_api_key` |
| `tavily` | Tavily | `tavily_api_key` |
| `google_cse` | Google Custom Search | `google_cse_api_key` + `google_cse_cx` |
| `github_code` | GitHub code search | `github_token` (the keyless `github` engine covers repos/issues) |

`anysearch`, `stackexchange` and `semanticscholar` are keyless engines that
accept an optional credential to raise their limits.

Set them as `SEARCH_MCP_<FIELD>` env vars (e.g. `SEARCH_MCP_SERPER_API_KEY`),
or on the local settings page:

```bash
uv run search-mcp-admin     # http://127.0.0.1:8765 — local settings / 本地设置
```

The page is bilingual (中英双语). It opens with the Network / Proxy / 网络 /
代理 card. The provider cards below it have "How to get a key / 如何获取密钥"
steps and Save/Test/Clear buttons, and a saved value applies live. An opt-in
engine named without a key returns an error saying the search itself is fine,
what to use instead, and that the user should not be asked for a key. For a
per-provider walkthrough, see [API_KEYS.md](API_KEYS.md).

### Optional: search on your ChatGPT plan (`codex`)

One more opt-in engine runs on an account sign-in rather than a key. `codex`
runs OpenAI's own web search, the one Codex uses, on the operator's ChatGPT
plan: sign in once with `search-mcp-login codex` (or **Sign in / 登录** on the
settings page), then name it with `engines=["codex"]`. Naming it before any
sign-in opens the ChatGPT sign-in page in the local browser and finishes the
search once it is approved (stdio on a desktop; `SEARCH_MCP_CODEX_AUTO_SIGNIN`). Each search counts
against the plan's Codex usage. It follows the same rules as the key engines:
never selected on its own, and an error rather than results when no sign-in can
be made. See [CODEX_SEARCH.md](CODEX_SEARCH.md), which also explains why there
is no Antigravity equivalent.

## Testing

```bash
# offline (no network) — default
uv run pytest -q

# live network tests (hit the real engines), gated behind an env var
SEARCH_MCP_TEST_NETWORK=1 uv run pytest tests/test_bilibili.py tests/test_anysearch.py -q
```
