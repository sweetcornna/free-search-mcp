# Usage Guide

`free-search-mcp` 1.0 exposes exactly **three MCP tools** (`search`, `exec`,
and `ask`) covering 500+ keyless web, academic, and specialist sources,
optional answer-agent synthesis and signed-in `codex`/`antigravity` agent lane,
document parsing, and local FTS5 caching. For install steps see
[INSTALL.md](INSTALL.md), [CODEX_SEARCH.md](CODEX_SEARCH.md), and
[ANTIGRAVITY_SEARCH.md](ANTIGRAVITY_SEARCH.md).

---

## Tool 1: `ask` (Quick One-Call Lookup)

One call, no code — designed for fast factual lookups (versions, dates, prices,
docs, recent events) without pulling full pages into context. Always registered.

### Parameters

| Parameter | Type | Default | Meaning |
|---|---|---|---|
| `question` | `string` *(required)* | — | Natural-language question to answer or gather dated passages for |
| `freshness` | `"day" \| "week" \| "month" \| "year"` | *(none)* | Recency hint |
| `lang` | `string` | *(auto)* | Optional BCP-47 locale hint |
| `domains` | `string[]` | *(none)* | `"example.com"` includes, `"-example.com"` excludes |
| `mode` | `"auto" \| "fast" \| "agent"` | `"auto"` | `"fast"` = no LLM (direct-answer sources first, else web search + parallel page read in ~8 s); `"agent"` = configured answer agent synthesizes a sourced answer; `"auto"` = `agent` when configured, else `fast` |
| `max_tokens` | `integer` | *(default)* | Output token budget |

Also reachable via:
- `await web.ask(question, opts)` inside `exec`
- CLI: `free-search-mcp ask "latest stable tokio version"`
- Streamable HTTP: `POST /v1/ask` and `GET /v1/ask?q=...` (behind the same `SEARCH_MCP_HTTP_TOKEN` / host guards)

See [DELEGATION.md](DELEGATION.md) for configuring answer-agent backends (`api`, `claude-code`, `codex`).

---

## Tool 2: `search`

Plain-parameter discovery across live verticals, the local FTS5 page cache, and
the zero-network catalog of 500+ sources & Host APIs.

### Parameters

| Parameter | Type | Default | Meaning |
|---|---|---|---|
| `query` | `string` *(required)* | — | Keywords, DOI/arXiv/CVE, FTS5 query (`scope="cache"`), or capability keywords (`scope="catalog"`) |
| `scope` | `string` | `"web"` | Vertical scope, sub-scope, synonym, `"cache"`, or `"catalog"` |
| `sources` | `string[]` | *(auto)* | Explicit source IDs from `catalog`; bypasses default pool & scope routing |
| `freshness` | `"day" \| "week" \| "month" \| "year"` | *(none)* | Recency hint; `"day"`/`"week"` also join `googlenews` |
| `domains` | `string[]` | *(none)* | `"example.com"` includes, `"-example.com"` excludes |
| `lang` | `string` | *(auto)* | Optional BCP-47 locale hint (auto-detected from query script when omitted) |
| `limit` | `integer` (`1..50`) | `10` | Maximum merged results returned |
| `max_age_hours` | `number >= 0` | *(TTL)* | Maximum cache age in hours; `0` forces a live fetch and writes the fresh result back to cache |

### Scopes & Synonyms

| Canonical `scope` | Exclusive? | Accepted Synonyms | Routes To |
|---|---|---|---|
| `web` | No | `general`, `internet` | Default pool (`duckduckgo`, `bing`, `yahoojp`, `seznam`) + reserves (`yahoo`, `youcom`, `mwmbl`) + locale (`zh`: `so360`/`quark`/`baidu`, `ko`: `naver`/`daum`) + direct-fact claimants |
| `academic` | Yes | `paper`, `papers`, `scholar`, `literature`, `论文`, `学术` | Direct dispatch to `ScholarApi::search` (keyless pool: `arxiv`, `crossref`, `openaire`, `europepmc`, unified `ScholarlyWork` cards) |
| `academic.*` | Yes | `paper.cs`, `ai`, `ml`, `health`, `medicine`, `oa`, `trials`, `physics`, `grants`, `theses`, `books`, … | Sub-scopes: `.index`, `.preprint`, `.biomed`, `.cs`, `.math`, `.physics`, `.openaccess`, `.trial`, `.humanities`, `.grants`, `.theses`, `.books` |
| `news` | No | `news.world`, `press` | `googlenews`, `gdelt`, and news feeds |
| `code` | No | `github`, `software`, `packages`, `software.python`, `software.node`, `software.rust`, `software.registry`, `software.lifecycle`, `software.app` | `github`, `github_releases`, `pypi`, `npm`, `crates`, `registries`, `endoflife`, `appstore` (direct-fact claimants also run here!) |
| `docs` | No | `docs.web`, `docs.rfc`, `documentation`, `standards` | `mdn`, `ietf`, specification catalogs |
| `dataset` | Yes | `data`, `datasets`, `dataset.repository`, `dataset.ml`, `dataset.gov`, `数据集` | `dryad`, `dataverse`, `zenodo`, `figshare`, `huggingface`, `dataeuropa`, CKAN/Socrata/OpenDataSoft portals |
| `finance` | No | `finance.filings`, `finance.market`, `finance.macro`, `finance.fx`, `finance.entity`, `finance.crypto` | `sec_edgar`, `cninfo`, `yahoofinance`, `worldbank`, `imf`, `frankfurter`, `cfets`, `gleif`, `coingecko`, SDMX macro feeds |
| `security` | No | `security.cve`, `security.package`, `security.exploited`, `cve`, `vuln` | `nvd`, `osv`, `cisakev`, `epss`, `ghsa`, `mitre_attack` |
| `gov` / `gov.law` / `gov.patents` | No | `gov.us`, `gov.uk`, `gov.eu`, `government`, `law`, `legal`, `patents`, `法律`, `专利` | `federalregister`, `govuk`, `courtlistener`, `eurlex_sparql`, `jp_egov_law`, `bundestag_dip`, `uspto_patentsview`, `wipo_patentscope`, etc. |
| `media` | Yes | `image`, `images`, `video`, `audio`, `podcast` | `openverse`, `wikimedia`, `bilibili`, `musicbrainz`, `tvmaze`, `jikan`, `anilist`, `archive_org` |
| `commerce` | Yes | `shop`, `store`, `shopping`, `products`, `prices`, `商品`, `电商` | Storefront / Shopify JSON catalog instances |
| `forum` | No | `social`, `community`, `qa` | `stackexchange`, `hackernews`, `lobsters`, `devto`, `bluesky`, Discourse/Fediverse instances |
| `reference` / `reference.geo` | No | `weather`, `calendar`, `stats`, `wiki`, `geo`, `maps`, `places` | `wikidata`, `wikipedia`, `rdap`, `openmeteo`, `wdi`, `holidays`, `worldclock`, `nominatim`, `overpass_osm` |
| `cache` | Yes (0 net) | `local_cache` | SQLite FTS5 search over pages already fetched by this server |
| `catalog` | Yes (0 net) | `sdk`, `api`, `tools`, `engines`, `sources`, `目录` | BM25 + fuzzy lookup across all 500+ sources, 18 protocol adapters, and `exec` Host APIs |

### Direct-Fact Auto-Routing

On `scope="web"` and `scope="code"`, 14 specialist claimant sources inspect the
query offline and automatically join with a `×5.0` RRF boost when the query
asks a direct registry question:

```json
{"query": "tokio crate latest stable version", "scope": "code"}
{"query": "CVE-2024-3094"}
{"query": "100 usd to cny"}
{"query": "上海明天天气"}
{"query": "现在东京几点"}
{"query": "2026年中国放假安排"}
```

---

## Tool 3: `exec`

Runs sandboxed ES2020 JavaScript inside QuickJS-NG (zero filesystem, env,
process, or raw socket access; TypeScript syntax auto-stripped on retry).
Accepts an `async () => { ... }` arrow, statements with `return`, or a single
expression.

### Parameters

| Parameter | Type | Default | Meaning |
|---|---|---|---|
| `code` | `string` *(required)* | — | JS async arrow, statements with `return`, or expression |
| `max_tokens` | `integer` (`500..20000`) | `6000` | Output budget (CJK-aware). Overflow saves raw JSON to `cache.get("exec:<id>")` |
| `timeout_s` | `integer` (`1..120`) | `30` | Wall-clock timeout in seconds (emits progress every 10 s; Codex users should set `tool_timeout_sec = 150`) |

### Cookbook Examples

#### 1. Search and read primary pages in one turn (`web.research` / `web.fetchBatch`)

```js
web.research("Rust 2024 edition breaking changes", {
  depth: 3,
  domains: ["doc.rust-lang.org", "blog.rust-lang.org"],
})
```

Or search, filter, and batch-fetch with `Promise.all`:

```js
async () => {
  const res = await web.search("Model Context Protocol 2026-07-28 spec", {
    domains: ["modelcontextprotocol.io"],
    limit: 5,
  });
  const pages = await web.fetchBatch(res.results.slice(0, 3).map((r) => r.url));
  return pages.map((p) => ({
    title: p.title,
    url: p.url,
    date: p.publishedDate,
    matches: util.grep(p.content, /streamable|session|header/i, 1).slice(0, 6),
  }));
}
```

#### 2. Compare URLs and extract structured metadata

```js
async () => {
  const [cmp, meta] = await Promise.all([
    web.compare("pricing and rate limits", [urlA, urlB]),
    web.structured(urlA),
  ]);
  return { cmp, jsonLd: meta.jsonLd, opengraph: meta.opengraph };
}
```

#### 3. Read PDFs, Office documents, EPUBs, and archives (`doc.outline` / `doc.read`)

```js
async () => {
  const outline = await doc.outline("https://arxiv.org/pdf/1706.03762.pdf");
  const slice = await doc.read("https://arxiv.org/pdf/1706.03762.pdf", {
    start: 0,
    length: 12000,
  });
  return { pages: outline.pages, sections: outline.sections, excerpt: slice.content };
}
```

*(Local file paths require opting in via `SEARCH_MCP_DOCUMENT_ROOT`. Reading a
local file taints that `exec` call and blocks subsequent network egress.)*

#### 4. End-to-end academic research (`scholar.*`)

```js
async () => {
  const id = "arXiv:1706.03762";
  const [work, integrity, graph, ft, bibtex] = await Promise.all([
    scholar.work(id),
    scholar.integrity(id),
    scholar.graph(id, { direction: "both", limit: 10 }),
    scholar.fulltext(id, { preferStructured: true, sections: ["Abstract", "Conclusion"] }),
    scholar.cite(id, "bibtex"),
  ]);
  return { work, integrity, refs: graph.references.length, cites: graph.citations.length, ft, bibtex };
}
```

> **Retraction Watch initial sync (~70 MB)**: The first scholarly call lazily
> downloads Crossref's ~70 MB CC0 Retraction Watch CSV, indexes 6 columns into
> SQLite (~8–12 MB), deletes the raw CSV, and refreshes via `ETag` every 7 days.
> Set `SEARCH_MCP_OFFLINE_SYNC=false` to disable.

Discoverable scholarly entity helpers:
- `await scholar.author("Geoffrey Hinton")`
- `await scholar.org("ETH Zurich")`
- `await scholar.venue("NeurIPS")`
- `await scholar.grants("CRISPR", { limit: 10 })`

#### 5. Discover and call 500+ specialist sources & 18 protocols (`catalog.*` + `source.*`)

```js
async () => {
  const hits = await catalog.search("clinical trials cancer", { limit: 5 });
  const detail = await catalog.describe("clinicaltrials");
  const out = await source.call("clinicaltrials.search", {
    query: "KRAS G12C",
    limit: 5,
  });
  return { hits: hits.map((h) => h.id), params: detail.params, items: out.items };
}
```

Call any ad-hoc protocol endpoint directly (OAI-PMH, CKAN, Socrata,
OpenDataSoft, SPARQL, MediaWiki, Discourse, WordPress, RSS/Atom, SRU, etc.):

```js
source.protocol("ckan", "https://catalog.data.gov/api/3", "search", {
  query: "electric vehicle charging",
  limit: 5,
})
```

#### 6. Raw public HTTP requests (`web.request`)

`HttpResponse` exposes `.body` (decoded string) and `.json` (parsed when
`Content-Type` is JSON — there is **no** `.text` property):

```js
async () => {
  const r = await web.request({ url: "https://api.github.com/repos/rust-lang/rust/releases/latest" });
  const data = r.json ?? JSON.parse(r.body);
  return { tag: data.tag_name, published: data.published_at };
}
```

#### 7. Working with `exec:<id>` overflow and session KV (`cache.*`)

When an `exec` result exceeds `max_tokens`, the server saves the full raw JSON
under `exec:<id>` (24 h TTL) and prints a nonced handle footer. Query slices of
it in the next turn with zero extra network calls:

```js
async () => {
  const full = await cache.get("exec:a1b2c3d4");
  return full.items.slice(0, 5);
}
```

Store scratch state across turns with `await cache.put("my_key", data, { ttlHours: 12 })`
(pass `{ shared: true }` to share across stateless HTTP requests).

---

## CLI Commands & Diagnostics

```bash
free-search-mcp --help                     # flags: --transport stdio|streamable-http --host --port --path
free-search-mcp ask "latest rust release"  # quick sourced lookup from the terminal
free-search-mcp login codex                # optional ChatGPT plan search + agent lane
free-search-mcp login antigravity          # opt-in Google Search (read ToS warning first)
free-search-mcp login status               # inspect stored OAuth sign-ins
free-search-mcp login logout codex         # remove stored OAuth tokens
free-search-mcp config                     # inspect resolved config (secrets redacted)
free-search-mcp doctor                     # check TLS, SSRF/Fake-IP TUN, cache, and probes
free-search-mcp sources                    # list all registered sources and availability
free-search-mcp probe                      # run live health probes
free-search-mcp eval-replay                # replay offline ranking benchmarks
```

The `search-mcp` binary alias accepts the exact same flags and subcommands.
