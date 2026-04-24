# free-search-mcp

[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Python](https://img.shields.io/badge/python-3.11%2B-blue.svg)](pyproject.toml)
[![MCP](https://img.shields.io/badge/MCP-1.2%2B-purple.svg)](https://modelcontextprotocol.io/)

A **local-first, no-API-key** Model Context Protocol server that gives any
LLM (Claude, GPT, local Ollama, …) the ability to search the web, fetch and
clean up pages, and read documents — without you signing up for a single
search API.

It bundles together the best ideas from a handful of open-source MCPs into
one Python package, and adds the LLM-ergonomics and reliability work they
were each missing.

```text
research("how does reciprocal rank fusion work", depth=3)
   ↓
# Research brief: how does reciprocal rank fusion work
_engines: duckduckgo, mojeek, startpage · sources: 3 · ~3,400 tokens_

## Sources
- [1] Reciprocal rank fusion | Elasticsearch Reference — <https://…>
- [2] Hybrid Search Scoring (RRF) | Microsoft Learn — <https://…>
- [3] RRF explained in 4 mins — Medium — <https://…>

## Documents
…full Markdown bodies of each page, ready for the LLM to read…
```

One tool call. Three sources. No API key. No `OPENAI_API_KEY`-but-for-search
shakedown.

---

## Why this exists

Existing search MCPs each do one thing well, but you usually want all of it:

| | Multi-engine | No API key | Smart fallback | PDF/DOCX | FTS5 cache | Filters | Trafilatura | LLM-tuned |
|---|---|---|---|---|---|---|---|---|
| `nickclyde/duckduckgo-mcp-server` | ✗ | ✓ | ✗ | ✗ | ✗ | ✗ | ✗ | ~ |
| `mrkrsl/web-search-mcp` | ✓ | ✓ | ✓ | ✗ | ✗ | ✗ | ✗ | ~ |
| `Aas-ee/open-webSearch` | ✓ | ✓ | ~ | ✗ | ✗ | ✗ | ✗ | ~ |
| `VincentKaufmann/noapi-google-search-mcp` | ✗ | ✓ | ✓ | ✓ | ✓ | ✗ | ✗ | ~ |
| **free-search-mcp** | **✓** | **✓** | **✓** | **✓** | **✓** | **✓** | **✓** | **✓** |

"LLM-tuned" here means: Markdown-first output, token estimates, smart
truncation at paragraph boundaries, "Best for / Not for / Returns / Common
mistakes" docstrings the model uses to pick the right tool, actionable
error hints, MCP prompts and resource templates, and a one-shot
`research()` that collapses search→fetch→fetch→fetch into a single turn.

"Trafilatura" means we extract main content using
[trafilatura](https://github.com/adbar/trafilatura) — winner of the
Bevendorff 2023 ROUGE benchmark (~0.85 vs ~0.55 for naive boilerplate
stripping). Each fetched page also returns `author`, `published_date`, and
`sitename` for free.

"Filters" means search/research accept `freshness`, `include_domains`,
`exclude_domains`, `category` (`news`/`pdf`/`github`/`paper`/`forum`/`blog`),
`include_text`, `exclude_text`.

---

## Tools

| Tool | Description |
|---|---|
| `search(query, engines?, max_results?, use_cache?, max_age_hours?, freshness?, include_domains?, exclude_domains?, category?, include_text?, exclude_text?, format?)` | Parallel multi-engine search merged via Reciprocal Rank Fusion |
| `research(question, depth?, engines?, fetch?, use_cache?, max_age_hours?, freshness?, include_domains?, exclude_domains?, category?, include_text?, exclude_text?, format?)` | One-shot: search + fetch top N + return Markdown brief |
| `fetch(url, render?, force_refresh?, max_age_hours?, format?)` | Fetch a page, return reader-mode Markdown (trafilatura-extracted, with author/date/sitename) |
| `fetch_batch(urls, render?, format?)` | Concurrent multi-URL fetch |
| `read_doc(source, start?, length?, format?)` | Parse PDF / DOCX / HTML / TXT / MD with pagination |
| `cache_search(query, limit?, format?)` | FTS5 search across previously fetched pages |
| `engines()` | List engine names available to `search` |

Plus **2 MCP prompts** (`Research thoroughly`, `Fact-check claim`) and a
**resource template** (`cache://page/{url}`) for dragging cached pages back
into context without re-fetching.

### Filters (search / research)

| Param | Values | Effect |
|---|---|---|
| `freshness` | `day` / `week` / `month` / `year` | Only results from the last N |
| `include_domains` | `["python.org", "djangoproject.com"]` | Restrict to these domains |
| `exclude_domains` | `["pinterest.com"]` | Remove these |
| `category` | `news` / `pdf` / `github` / `paper` / `forum` / `blog` | Content-type shortcut (paper = arxiv/acm/ieee/…, forum = reddit/HN/SE, etc.) |
| `include_text` | `"async"` | Substring required in title/snippet |
| `exclude_text` | `"beginner"` | Substring forbidden |
| `max_age_hours` | `24` | Override the 7-day default cache TTL on this call |

All tools default to `format="markdown"` — readable, ~40% fewer tokens than
JSON, with provenance and a token-budget header. Pass `format="json"` for
structured access.

### Tool annotations

Every tool ships correct `readOnlyHint`, `idempotentHint`, and
`openWorldHint` annotations so MCP clients can label them and gate
elevated actions.

### Engines

Default set (all reliable, **no captchas** during repeated calls):
`duckduckgo`, `mojeek`, `startpage`.

Opt-in (intermittent challenges to headless clients):
`brave`, `bing`, `baidu`.

> Brave/Bing/Baidu all gate headless browsers after a handful of calls (PoW
> CAPTCHAs, "something went wrong" pages, redirect wrappers). Pass
> `engines=["brave"]` etc. only when the defaults can't find what you need.

---

## Install

```bash
git clone https://github.com/ymylive/free-search-mcp.git
cd free-search-mcp
uv sync
uv run playwright install chromium
```

Run as a stand-alone server (stdio transport):

```bash
uv run search-mcp
```

Run live tests (hits the real web — set the env var):

```bash
SEARCH_MCP_TEST_NETWORK=1 uv run pytest -v
```

Offline tests run by default and don't touch the network.

---

## Wire into Claude Desktop

Add this to `~/Library/Application Support/Claude/claude_desktop_config.json`
(macOS) or the equivalent on your platform:

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

Restart Claude Desktop. The seven tools above will appear in the tool
drawer.

### Wire into other clients

The server speaks plain MCP over stdio. Anything that supports MCP works:

- Claude Code (`claude mcp add search uv --directory /…/free-search-mcp run search-mcp`)
- Cursor / Continue / Cline (use the JSON snippet above)
- Custom Python / TypeScript clients via the official MCP SDK

---

## Configuration

All settings can be overridden by environment variables prefixed with
`SEARCH_MCP_`:

| Var | Default | Meaning |
|---|---|---|
| `SEARCH_MCP_DEFAULT_ENGINES` | `["duckduckgo","mojeek","startpage"]` | JSON list |
| `SEARCH_MCP_MAX_RESULTS_PER_ENGINE` | `10` | |
| `SEARCH_MCP_RATE_LIMIT_PER_MINUTE` | `30` | per engine |
| `SEARCH_MCP_FETCH_RATE_LIMIT_PER_MINUTE` | `20` | shared `fetch` bucket |
| `SEARCH_MCP_CACHE_DIR` | `~/.cache/search-mcp` | |
| `SEARCH_MCP_CACHE_TTL_SECONDS` | `604800` | 7 days |
| `SEARCH_MCP_FETCH_STRATEGY` | `auto` | `auto` / `http` / `browser` |
| `SEARCH_MCP_BROWSER_HEADLESS` | `true` | |
| `SEARCH_MCP_BROWSER_POOL_SIZE` | `2` | concurrent pages |
| `SEARCH_MCP_MAX_CONTENT_CHARS` | `50000` | per result truncation |

---

## Architecture

```
   ┌─────────────────────────────────────────────────────┐
   │  FastMCP server (stdio)                             │
   │  tools: search / research / fetch / fetch_batch /   │
   │         read_doc / cache_search / engines           │
   └────────────┬────────────────────────────────────────┘
                │
   ┌────────────▼────────────┐  ┌────────────────────────┐
   │  aggregator             │  │  fetcher               │
   │  - parallel engines     │  │  - httpx fast path     │
   │  - reciprocal rank      │  │  - playwright fallback │
   │    fusion               │  │  - markdownify         │
   │  - search cache (FTS5)  │  │  - page cache (FTS5)   │
   └────┬────────────────────┘  └────────────┬───────────┘
        │                                    │
   ┌────▼─────────────────┐  ┌──────────────▼─────────────┐
   │  engines/            │  │  browser pool              │
   │   duckduckgo.py      │  │   - persistent context     │
   │   mojeek.py          │  │   - stealth init script    │
   │   startpage.py       │  │   - shared cookies         │
   │   brave.py     (opt) │  │   - semaphore-bounded pages│
   │   bing.py      (opt) │  └────────────────────────────┘
   │   baidu.py     (opt) │
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

The base class handles transport (httpx → Playwright fallback), rate
limiting, and the case where HTTP returns a captcha shell instead of
results (auto-retries via the browser).

---

## Credits

This project stands on the shoulders of:

- [`mrkrsl/web-search-mcp`](https://github.com/mrkrsl/web-search-mcp) —
  smart httpx-then-Playwright fetch strategy, multi-engine fallback chain
- [`Aas-ee/open-webSearch`](https://github.com/Aas-ee/open-webSearch) —
  multi-engine breadth (Bing/DDG/Baidu/Brave/Startpage)
- [`VincentKaufmann/noapi-google-search-mcp`](https://github.com/VincentKaufmann/noapi-google-search-mcp) —
  anti-detection patterns (`navigator.webdriver`, UA, cookies), SQLite
  FTS5 cache idea, multi-format `read_document`
- [`nickclyde/duckduckgo-mcp-server`](https://github.com/nickclyde/duckduckgo-mcp-server) —
  per-engine rate limiting, LLM-friendly content cleanup
- [Mojeek](https://www.mojeek.com/) — independent search index that
  doesn't gate on User-Agent
- [Model Context Protocol](https://modelcontextprotocol.io/) and the
  [official Python SDK](https://github.com/modelcontextprotocol/python-sdk)

---

## License

MIT — see [LICENSE](LICENSE).
