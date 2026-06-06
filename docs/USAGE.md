# Usage guide

A practical reference for the `free-search-mcp` tools, engine selection, and
filters. For install/deploy see the [Quick start](#quick-start) below or the
[README](../README.md).

---

## Quick start

```bash
# one-line setup for Claude Code
curl -LsSf https://raw.githubusercontent.com/sweetcornna/free-search-mcp/main/scripts/install.sh | bash -s -- --client claude-code

# one-line setup for Codex
curl -LsSf https://raw.githubusercontent.com/sweetcornna/free-search-mcp/main/scripts/install.sh | bash -s -- --client codex

# local checkout / install only
./scripts/install.sh --client none
```

For Codex, Cursor, Cline, Continue, Zed, and generic agent operating rules, see
[AGENT_USAGE.md](AGENT_USAGE.md).

Wire into **Claude Code**: this repo ships a `.mcp.json`, so running `claude`
inside the project auto-detects the `search` server. To register it globally:

```bash
claude mcp add search -s user -- uv --directory /absolute/path/to/free-search-mcp run search-mcp
```

Wire into **Codex**:

```bash
codex mcp add search -- uv --directory /absolute/path/to/free-search-mcp run search-mcp
codex mcp list
```

Wire into **Claude Desktop** — add to `claude_desktop_config.json`:

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

**Docker** (containerized, stdio):

```bash
docker compose build
docker compose run --rm search-mcp
```

---

## Tools

| Tool | What it does |
|---|---|
| `search(query, engines?, max_results?, ...filters)` | Parallel multi-engine search, RRF-merged + deduped, optional `lead_snippet`. |
| `research(question, depth?, ...filters)` | search + fetch top N + return a Markdown brief in one call. |
| `compare(question, urls=[2..5])` | Concurrent fetch of 2–5 URLs, side-by-side excerpts. |
| `fetch(url, render?, ...)` | Fetch one page → reader-mode Markdown (trafilatura). |
| `fetch_batch(urls, ...)` | Concurrent multi-URL fetch. |
| `read_doc(source, start?, length?, ...)` | Parse PDF / DOCX / HTML / TXT / MD with pagination. |
| `extract_structured(url, ...)` | JSON-LD / OpenGraph / Twitter cards / microdata. |
| `cache_search(query, limit?, ...)` | FTS5 search across previously fetched pages. |
| `engines()` | List engine names accepted by `engines=`. |

All tools default to `format="markdown"`; pass `format="json"` for structured
output.

---

## Engines

`search` / `research` accept an `engines=[...]` list. Omit it to use the fast
default pool. Every engine here is **keyless** (no API key, no account).

**Default pool** (all-HTTP, no browser, ~2× faster):
`duckduckgo`, `mojeek`, `googlenews`.

**Opt-in extras:**

| Engine | Source | Notes |
|---|---|---|
| `startpage` | Startpage | browser-rendered, ~5–10s |
| `brave` `bing` `baidu` | resp. engines | intermittently challenge headless clients |
| `searx` | public SearXNG instances | meta-search; public instances often slow |
| `google` | Google web SERP scrape | HTTP→browser fallback; Google **CAPTCHAs datacenter/headless IPs**, so expect gating off residential networks |
| `serpsearch` | alias of `google` | identical behavior (all real SERP APIs need a key) |
| `anysearch` | [AnySearch](https://github.com/anysearch-ai/anysearch-mcp-server) REST API | anonymous tier, IP rate-limited; one call returns fused/re-ranked results |
| `bilibili` | Bilibili (哔哩哔哩) JSON API | keyless video search (synthetic `buvid3` cookie); **video results only** |
| `zhihu` | Zhihu (知乎) search page | **best-effort**, browser-rendered; Zhihu hard-gates bots so a login wall / empty result is common and honest |

Enable globally via `SEARCH_MCP_DEFAULT_ENGINES` (JSON list) in `.env`.

**Keyed (API-key) engines** — dormant until a key is configured:

| Engine | Provider | Needs | Free tier |
|---|---|---|---|
| `brave_api` | Brave Search API | `brave_api_key` | 2,000/mo |
| `serper` | Serper (Google) | `serper_api_key` | 2,500 |
| `tavily` | Tavily (AI search) | `tavily_api_key` | 1,000/mo |
| `google_cse` | Google Custom Search | `google_cse_api_key` + `google_cse_cx` | 100/day |

Add keys the simple way:

```bash
uv run search-mcp-admin     # http://127.0.0.1:8765 → paste keys → Save (applies live)
```

or set `SEARCH_MCP_<FIELD>` env vars (e.g. `SEARCH_MCP_SERPER_API_KEY`). An
unconfigured keyed engine returns a clear "not configured" hint instead of
failing silently. Step-by-step key acquisition: **[API_KEYS.md](API_KEYS.md)**.

### Examples

```text
# English web search, default pool
search("reciprocal rank fusion")

# One-call aggregator (anonymous AnySearch)
search("vector database benchmarks", engines=["anysearch"])

# Chinese video search on Bilibili
search("python 教程", engines=["bilibili"])

# Mix CJK verticals + general web
search("transformer 架构", engines=["bilibili", "zhihu", "duckduckgo"])

# Google SERP scrape (works best from a residential IP)
search("site:python.org asyncio", engines=["google"])
```

> **Real-tested status (June 2026):** `bilibili` and `anysearch` return live
> results out of the box. `google`/`serpsearch` work only when the source IP
> isn't CAPTCHA-gated by Google. `zhihu` frequently hits a login wall and
> returns empty — that's the honest no-key ceiling, by design.

**Gated engines** (Google/Bing CAPTCHA, Zhihu login) have three escape hatches:
a **proxy** (`SEARCH_MCP_PROXY` / admin "Network / Proxy"), an automatic
**SearXNG fallback** for `google`/`serpsearch`/`bing`, and a one-time
**`search-mcp-login zhihu`**. The response reports `gated_engines` + `gated_hint`
when an engine was gated. See **[PROXY_AND_GATES.md](PROXY_AND_GATES.md)**.

---

## Filters (search / research)

| Param | Values | Effect |
|---|---|---|
| `freshness` | `day` / `week` / `month` / `year` | only results from the last N |
| `include_domains` | `["python.org"]` | restrict to these domains |
| `exclude_domains` | `["pinterest.com"]` | remove these |
| `category` | `news` / `pdf` / `github` / `paper` / `forum` / `blog` | content-type shortcut |
| `include_text` | `"async"` | substring required in title/snippet |
| `exclude_text` | `"beginner"` | substring forbidden |
| `max_age_hours` | `24` | override the 7-day cache TTL on this call |

```text
research("LLM eval frameworks", depth=3, freshness="month", category="paper")
search("kubernetes operators", include_domains=["github.com"], category="github")
```

When filters drop results so aggressively that ≤3 remain, the response includes
`filter_diagnostics` telling you which knob to relax.

---

## Configuration

Copy `.env.example` → `.env` and edit. Every knob is an env var prefixed with
`SEARCH_MCP_` (see `.env.example` for the annotated full list). Common ones:

| Var | Default | Meaning |
|---|---|---|
| `SEARCH_MCP_DEFAULT_ENGINES` | `["duckduckgo","mojeek","googlenews"]` | engine pool (JSON list) |
| `SEARCH_MCP_FETCH_STRATEGY` | `auto` | `auto` / `http` / `browser` |
| `SEARCH_MCP_SAFESEARCH` | `moderate` | `strict` / `moderate` / `off` |
| `SEARCH_MCP_REGION` | `us-en` | `cc-lang` token |
| `SEARCH_MCP_CACHE_TTL_SECONDS` | `604800` | 7 days |

---

## Testing

```bash
# offline (no network) — default
uv run pytest -q

# live network tests (hit the real engines), gated behind an env var
SEARCH_MCP_TEST_NETWORK=1 uv run pytest tests/test_bilibili.py tests/test_anysearch.py -q
```
