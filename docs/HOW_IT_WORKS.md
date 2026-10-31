# How free-search-mcp works

The README says what the server does and how to install it. This page is the
detail behind it: why it is built this way, what each response tells the agent,
how the engines are chosen and handled, and how the code is laid out.

## Why it is built this way

The README compares this server with four others. Four of its columns need a
word of explanation.

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

## Anti-detection &amp; resilience

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

## Is it current, and is it the source?

A search result is a lead. The snippet under it was written by a search
engine, is often months old, and is routinely about a different year's edition
of the same page. It still reads like an answer, and agents tend to answer from
it. Every tool therefore reports what it knows about time and provenance, so
the agent can see how far to trust a result:

```text
# Search: uv 0.9 release notes

_engines: duckduckgo, bing, yahoojp, anysearch_  _results: 3_  _dated: 2/3_
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

## Tool annotations

Every tool declares `readOnlyHint`, `idempotentHint`, and `openWorldHint`
annotations, so MCP clients can label the tools and restrict elevated actions.

## Protocol notes

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

## Engines

The registry holds 76 engines. A search that names none of them runs a small
pool chosen for it, and the pool depends on which engines are healthy at that
moment.

The default pool is `duckduckgo`, `bing`, `yahoojp` and `anysearch`. All four
use plain HTTP and need no browser. `yahoojp` is Yahoo! JAPAN, which licenses
Google's organic results and serves them as plain HTML, so it is the keyless
route to Google's index (google.com itself answers a plain request with a
JavaScript shell). The interface is Japanese, the index is not: measured on
2026-10-31 it answered English, Chinese and Japanese queries in 0.8 to 1.5 s
and put docs.python.org, Stack Overflow and the Federal Reserve's own release
on top where the other engines led with blogs. The scrapers that only ever served a CAPTCHA, a
JavaScript shell or nothing at all (`mojeek`, `google`, `serpsearch`,
`startpage`, `searx`, `sogou`) were removed from the registry, because leaving
them in the pool only spent a wall's latency on the first search of every
process.

These join it when the query calls for them:

- `googlenews` joins for `freshness="day"` / `"week"`, or `category="news"`.
  It answers any query, programming questions included, with ten news
  headlines, which suits a web search only when recency was asked for. Its
  opaque `news.google.com` redirect links are resolved to the publisher's URL,
  so the same story from a web engine merges with it and is listed once.
- `so360` and `quark` join when the query itself is written in Chinese, and
  `naver` when it is written in Korean. The language is detected from the
  script of the query, and `SEARCH_MCP_REGION` plays no part. In our
  measurement `so360` returned the organiser's own site and the university
  notice for a Chinese query, neither of which the western indexes had, and
  `quark` (Alibaba's Shenma index) added CSDN, Tencent Cloud and php.cn pages.
  A Japanese query needs no extra engine: `yahoojp` is already in the pool.

Reserves (`yahoo`, `seznam`, `so360`, `brave`, in that order) are seated only
while fewer than 3 general engines are healthy. The default pool is four wide,
so it can lose one engine before a reserve is seated. `yahoo` is Bing's index
through Yahoo's own front end, which stayed on topic when `bing` served its
decoy page; `seznam` is an independent crawler. A reserve that needs Chromium
is skipped while Chromium is known to be missing.

The circuit breaker benches an engine at once when it serves a wall (CAPTCHA,
consent, login, a JS shell, a region block) or an off-topic page. An engine that errors is
benched after 2 failures in a row, and one that silently returns nothing after
3. The bench lasts 10 minutes and doubles on each repeat up to 60, and one
success clears it. A benched engine is not asked, so a dead engine slows down
only the search that discovers it, and the bench is written to
`<cache_dir>/engine_health.json` so that a new process does not discover it
again (measured: 8.5 s for the first process, 4.0 s for the next). Naming an
engine in `engines=[...]` runs it even while it is benched. The response reports `benched_engines` (Markdown:
"Benched engines") with the reason, the retry time and which reserve is
standing in.

A wall on the HTTP path alone is not a reason to bench an engine whose browser
render gets past it. When an engine's HTTP answer parses to nothing and the
render that follows finds results, the next 15 minutes of searches go to the
browser first and skip the HTTP request. Measured on 2026-10-22, DuckDuckGo
served its captcha to every HTTP request from one IP, and each one cost 1.1 to
2.3 s before the render. The verdict is written to
`<cache_dir>/http_walls.json`. A render that finds nothing or fails lifts it,
and after the 15 minutes the next search tries HTTP first again. A server that starts within a day of such a verdict launches the
browser in the background, so the first search does not wait for it
(`SEARCH_MCP_PREWARM`).

Engine requests reuse their connections. Each engine used to open a new
session per request and pay DNS, TCP and TLS each time; measured through a
fake-IP tunnel, a reused connection answered 0.7 to 0.9 s sooner (Bing 1.64 s
against 0.70 s). Cookies still last only for the request block that received
them. `fetch` downloads its body through a session of its own and closes it
when it is done, because closing is the one way to stop a transfer it has
given up on (over the size cap, an error status, a timeout).

The off-topic guard exists because Bing can answer HTTP 200 with ten
well-formed results about the first word of the query, such as the Steam page
for "rust ownership borrowing". Left alone, rank fusion interleaves those with
real answers. The guard drops a web engine's whole bucket when almost none of
it mentions the rest of the query and another engine's results show that those
words do get echoed. The engine is reported under `gated_engines` with reason
`off_topic`. This is not a block, so a proxy does not change it. More in
[docs/PROXY_AND_GATES.md](PROXY_AND_GATES.md).

The rescue pass runs when a search still comes back empty, or nearly empty
with gated or erroring engines. The aggregator makes one bounded recovery
attempt through the reserves and then `youcom` and `baidu`, skipping anything
that already ran or is benched, and reports it as `rescued_via`. A wall on the
default engines then costs time but still returns results.

The pass is a staggered race, in the manner of hedged requests. The first
candidate is asked at once, each next one `SEARCH_MCP_RESCUE_STAGGER_SECONDS`
(1) later or as soon as the one before it has failed, and the first usable
answer wins and cancels the rest; the whole pass fits in
`SEARCH_MCP_RESCUE_TIMEOUT` (10). At most two candidates hold a place at a
time, so an ordinary slow reserve is hedged by one more request rather than
by the whole list; one that has run past its share of the pass (two fifths
of the timeout, and never under two staggers) keeps running but stops
holding its place, so two hung reserves cannot starve a fast third. Until 0.18 it asked one candidate at a time,
each in an equal slice of the timeout, so a slow first candidate spent its
slice before a fast second one was asked. A candidate that answers inside the
first stagger still spares the others a request. When nothing keyless
answered and the [agent lane](#the-agent-lane) is still searching, the rescue
starts beside the lane wait instead of after it.

None of this needs an API key. No engine in the default, reserve, rescue,
fresh, locale or category pools requires one, and
`tests/test_no_key_positioning.py` fails the build if a keyed engine is added
to any of them.

Status as measured on 2026-10-29, and on 2026-10-31 for the engines added in
0.18:

- `duckduckgo`, `bing`, `anysearch`, `so360`, `baidu`, `youcom`, `googlenews`
  and `brave` (browser) answer on topic, and so do `yahoojp`, `yahoo`,
  `seznam`, `quark` and `naver`.
- `mojeek`, `google` / `serpsearch`, `startpage`, `searx` and `sogou` no longer
  have an engine module. They were measured serving a CAPTCHA, a JavaScript
  wall or an empty page on every request, so no pool, route or explicit name
  can reach them.
- `zhihu` is browser-rendered and login-gated; it works after
  `search-mcp-login zhihu` and returns empty otherwise.

Other keyless engines, and how each behaves:
- `brave` and `baidu` are browser-rendered and intermittently challenge
  headless clients.
- `yahoojp` (default pool) passes `site:`, `-site:` and `filetype:pdf` on to
  Google and filters by time with `vd=`. Yahoo! JAPAN has not served the EEA
  or the UK since April 2022; from there its notice page is reported as a
  `region` gate and the engine is benched for the maximum hour at once. A
  proxy elsewhere is the fix, or, for a server that stays in Europe, leaving
  it out of `SEARCH_MCP_DEFAULT_ENGINES`.
- `yahoo` scrapes search.yahoo.com and unwraps its `r.search.yahoo.com`
  click redirects. Yahoo answers a cookieless client with a bot-check
  handshake that often ends in an empty HTTP 500, so the cookies of one
  completed handshake are kept per proxy egress for 12 hours (measured: 5 of
  5 searches through without a redirect, against 3 of 8 with a fresh session
  each). Its EU consent page is a `consent` gate.
- `seznam` reads Seznam.cz's own crawler index from the JSON state embedded in
  the page rather than its per-build class names, which also holds all ten
  organic hits where the HTML rendered three. It honours `site:` and
  `filetype:` and has no time filter.
- `quark` (Quark / 神马) honours no search operators: a `site:` query returns
  its "no results" page, so domain and PDF filters are applied afterwards. It
  walls a burst (the fourth request within 6 s) with Alibaba's x5sec page,
  which is recognised as a captcha, so it is limited to 10 searches a minute
  and 2 at once, and a search skips it rather than queue.
- `naver` takes Naver's web documents only. Its blog and cafe cards are left
  out (blog pages are empty frames over HTTP and cafe links carry expiring
  tokens), so a query Naver answers mostly from those returns few results.
- `anysearch` is in the default pool. It calls the anonymous (keyless) tier of
  the [AnySearch](https://github.com/anysearch-ai/anysearch-mcp-server)
  unified-search REST API, and one HTTP call returns fused, re-ranked results.
  The tier is rate-limited per IP, and a 429 or 5xx degrades to an empty
  result. It does not honour `site:` / `filetype:` operators, so domain filters
  are applied to its results afterwards.
- `youcom` calls the `you-search` tool on the free profile of You.com's MCP
  server (`https://api.you.com/mcp?profile=free`), the keyless way in that
  You.com documents; its REST search endpoints are paid per call. It has its
  own web and news index and receives the domain filters, freshness,
  SafeSearch and region. The profile allows 100 searches a day per IP, so it is
  a rescue source rather than a default. A spent quota or a refusal is reported
  as such, not as an empty result.
- `bilibili` is keyless Bilibili (哔哩哔哩) video search through the public
  `web-interface/search/all/v2` JSON API (synthetic `buvid3` cookie, no
  login). It returns video results only.
- `zhihu` is best-effort keyless Zhihu (知乎) search. Zhihu's
  `api/v4/search_v3` needs login cookies and `x-zse-96` signing, so the only
  no-key path is rendering the public search page in a browser. Zhihu blocks
  headless clients, so a login wall or an empty result is common. Treat it like
  `baidu`/`brave`.
- `so360` is a Chinese web index scraped from HTML; `baidu` is the other.
  Both return direct URLs.
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
> [Configuration](CONFIGURATION.md).

## Vertical sources (selected automatically by `category`)

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

## Direct facts: records instead of snippets

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
rule: one looked-up record outranks the whole three-engine default pool
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
and China coverage, are listed in [docs/DATA_SOURCES.md](DATA_SOURCES.md).

## The search deadline

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

## The agent lane

An agent engine searches with a language model on the operator's own account:
`codex` runs OpenAI's web search on a ChatGPT plan, `antigravity` Google Search
through Gemini. Neither needs a key, both need a sign-in, and neither sits in a
pool or a route. Until 0.18 one ran only when a call named it, and naming it
switched the keyless pool, its breaker, its routing and its rescue off for
that call.

The agent lane runs them beside the pool instead. A search that names no
engines (and is not an `image` or `dataset` search) asks every signed-in agent
engine that `SEARCH_MCP_AGENT_ENGINES` seats at the same moment as the keyless
engines, puts each answer through the same off-topic guard, and fuses it into
the same reciprocal-rank ranking. A page OpenAI's search and DuckDuckGo both
found outranks a page only one of them found, and a lane bucket can be the
witness that exposes a keyless engine's decoy page.

- `auto` (the default) seats `codex` once it is signed in. `antigravity` is
  seated only when the setting names it, because Google's terms forbid this
  use of its sign-in. Without a sign-in nothing is seated and no sign-in page
  opens, so nothing changes for an operator who never signed in.
- The keyless results are never held back past
  `SEARCH_MCP_AGENT_LANE_WAIT_SECONDS` (6), counted from when the engines were
  asked, so the keyless fan-out's own time is not added to it, nor more than
  `SEARCH_MCP_AGENT_LANE_GRACE_SECONDS` (2) after the keyless engines have
  answered with results. Measured
  2026-09-26: `codex` answered in about 3 s and the pool in 2 to 4 s, so the
  two usually land together; `antigravity` took 10 to 20 s. When the keyless
  engines found nothing, the wait stretches to the search deadline, and the
  rescue pass starts beside it rather than after it. Whichever settles it
  first ends the other: a rescue that brings results back returns the lane to
  its ordinary wait, and a lane answer that makes the rescue unnecessary
  cancels it.
- A lane engine still searching when the wait ends is not cancelled. It
  finishes in the background, and its results are merged into the cached
  result set of that search, so the same call a little later (or
  `cache://search/...`) returns the fused set. `agent_lane.pending` and the
  "Agent lane" note say so. A newer search that rewrites the same row takes
  it over, and a late answer from the older one is dropped rather than
  written over it.
- A result set that lacks a lane engine for a passing reason (still being
  merged in, out of tokens, a timeout or a reset, or benched after those) is
  replayed from the cache
  for an hour at most, like a result set from a thinned pool, so the lane's
  answer is not kept out of that search for a week. The replay says which
  engine it lacks (`agent_lane.missing`). A lasting state (a spent plan, a
  refused sign-in, a benched engine) does not shorten the cache, or every
  search would re-run hourly for as long as it lasts.
- A late lane answer that echoes the query is also the witness the off-topic
  guard lacked: a keyless bucket it could not judge alone is dropped as a
  decoy when the answer is merged in.
- When no keyless engine has answered, the fan-out used to wait for every
  one of them however long it took. A lane answer with results now counts
  as an answer, so stragglers are cut at the search deadline as usual, and
  an early rescue that has come back by then is merged in rather than
  thrown away.
- The lane never queues for a rate-limit token. Past its per-minute limit (10
  for both engines, which protects the account's usage) an engine sits the
  search out and `agent_lane.rate_limited` says so.
- A spent usage window, a revoked sign-in or a refused model benches a lane
  engine at once (none of them clears on the next search, and every attempt
  draws on the plan); a timeout, a reset or a token endpoint that was down
  takes the ordinary two strikes.
  Failures are reported under `agent_lane.errors`, never under `errors`. The keyless pool's breaker, census and rescue decisions
  count only keyless engines, so the pool stays able to answer alone, and a
  keyless engine that found nothing takes no "silent" strike because a lane
  engine did. A lane search never opens a sign-in page.
- The cache key includes the seated lane, so signing in turns a cached
  keyless-only answer into a fresh search instead of replaying it.
- The server's startup warm-up refreshes a signed-in lane engine's token and
  looks its model up, which the first search would otherwise wait for.

`SEARCH_MCP_AGENT_ENGINES=off` turns the lane off. Naming engines in a call
(`engines=["codex"]`, `engines=["duckduckgo"]`) still runs exactly those.

`engines()` prints this tree live from the registry. The tree is derived from
what each engine declares, so it cannot drift from what runs.

Naming engines explicitly (`engines=[...]`) turns the routing off.

Sources that publish a stricter rate limit than our default declare it
themselves. When such a source's bucket is empty it is skipped, because search
is a parallel fan-out and queueing behind one slow source would add that delay
to every other engine's results.

## When an engine is gated (proxy · stand-ins · login)

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
  [CODEX_SEARCH.md](CODEX_SEARCH.md).)

When results are thin, the remedies are, in order: rephrase the query, pass
`category=`, name `engines=["so360","baidu"]` for Chinese content or
`engines=["brave"]` if Chromium is installed, and use a proxy
if the diagnostics show a real wall. An agent should not reach for an API key
as the fix; see
[API_KEYS.md](API_KEYS.md).

The full guide is [docs/PROXY_AND_GATES.md](PROXY_AND_GATES.md).

> `brave` and `baidu` gate headless browsers after a handful of calls (PoW
> CAPTCHAs, "something went wrong" pages, redirect wrappers). Name them only
> when the default pool can't find what you need.

## Sparse-result diagnostics

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
   │  - agent lane, hedged   │  │                        │
   │    rescue race          │  │                        │
   └────┬────────────────────┘  └────────────┬───────────┘
        │                                    │
   ┌────▼─────────────────┐  ┌──────────────▼─────────────┐
   │  engines/            │  │  browser pool              │
   │   duckduckgo.py      │  │   - persistent context     │
   │   bing.py            │  │   - stealth init script    │
   │   yahoojp.py         │  │   - shared cookies         │
   │   anysearch.py       │  │   - semaphore-bounded pages│
   │   yahoo.py  (reserve)│  └────────────────────────────┘
   │   seznam.py (reserve)│
   │   so360.py  (reserve)│
   │   brave.py  (reserve)│
   │   quark/naver(locale)│
   │   codex/antigravity  │
   │        (agent lane)  │
   │   baidu.py     (opt) │
   │   youcom.py (rescue) │
   │   bilibili.py  (opt) │
   │   zhihu.py     (opt) │
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
