# API Keys & Optional Credentials (Manual Only)

You can skip this page. `free-search-mcp` 1.0 is a **no-API-key** server: every
default web pool, reserve pool, locale pool, academic search pool, and standard
vertical route works keyless.

Two rules always hold:

1. **Keyed sources never run automatically**: Opt-in search APIs (`brave_api`,
   `serper`, `serpbase`, `tavily`, `anysearch`, `github_code`) and keyed
   catalog manifests run only when explicitly requested (or when you choose to
   supply a key for higher-rate scholarly search pools).
2. **Agents never ask for a key**: Setting a key is an operator choice made
   outside any conversation.

> **1.0.0 changes**: The legacy `google_cse`, `brave` HTML, and `zhihu`
> engines plus the `search-mcp-admin` web UI were removed in 1.0.0. Optional
> sign-in engines (`codex` and opt-in `antigravity`) remain available via
> `free-search-mcp login` (see [CODEX_SEARCH.md](CODEX_SEARCH.md) and
> [ANTIGRAVITY_SEARCH.md](ANTIGRAVITY_SEARCH.md)). Configure optional keys via
> environment variables, `~/.config/search-mcp/.env`, or
> `~/.config/search-mcp/config.json` (`0600`), and verify with
> `free-search-mcp config` and `free-search-mcp sources`.

---

## Where to Set Optional Credentials

All credential variables (`*_API_KEY`, `*_TOKEN`, `*_KEY`, `*_SECRET`,
`*_USERNAME`, `*_CLIENT_ID`) are **security-sensitive (`[SEC]`)** and are
**ignored** if placed in a working-directory `./.env`. Set them in one of:

1. **Process environment variables** (highest precedence)
2. **`~/.config/search-mcp/.env`** (override directory with `SEARCH_MCP_CONFIG_DIR`)
3. **`~/.config/search-mcp/config.json`** (`chmod 600`; supports either a flat
   JSON object or `{"secrets": { ... }}`)

Example `~/.config/search-mcp/.env`:

```dotenv
# Optional polite-pool email (not a secret — enables Unpaywall OA full text
# and faster Crossref/OpenAlex/NCBI pools)
SEARCH_MCP_CONTACT_EMAIL=you@example.com

# Optional scholarly rate/pool upgrades
SEARCH_MCP_SEMANTICSCHOLAR_API_KEY=
SEARCH_MCP_OPENALEX_API_KEY=
SEARCH_MCP_CORE_API_KEY=
SEARCH_MCP_NCBI_API_KEY=
```

Verify that your server sees the expected keys (values are always redacted):

```bash
free-search-mcp config
free-search-mcp sources
```

---

## Polite-Pool Email (Recommended for Heavy Academic Use, Not an API Key)

| Variable | Effect |
|---|---|
| `SEARCH_MCP_CONTACT_EMAIL` | Sends an honest polite-pool contact header to Crossref, OpenAlex, and NCBI, and enables **Unpaywall** (step 7 of the 11-step `scholar.fulltext` cascade; skipped cleanly when unset). |

---

## Optional Academic & Specialist Credentials

| Source | Environment Variable(s) | Free Tier / Effect |
|---|---|---|
| **Semantic Scholar** (`semanticscholar`) | `SEARCH_MCP_SEMANTICSCHOLAR_API_KEY` | Single-ID `scholar.work` works keyless; a free key unlocks `scholar.search` pooling and avoids anonymous 429 benching. |
| **OpenAlex** (`openalex`) | `SEARCH_MCP_OPENALEX_API_KEY` | Single-ID `$0` lookups work keyless; an API key enables OpenAlex search endpoints in `scholar.search`. |
| **CORE v3** (`core_ac`) | `SEARCH_MCP_CORE_API_KEY` | Full-text step 9 works keyless; a key unlocks CORE search pooling. |
| **NCBI / PubMed** (`pubmed`) | `SEARCH_MCP_NCBI_API_KEY` | Works keyless at 3 req/s; a free NCBI key raises the limit to 10 req/s. |
| **NASA ADS** (`nasa_ads`) | `SEARCH_MCP_ADS_API_KEY` | Unlocks NASA Astrophysics Data System search (`academic.physics`). |
| **OpenAQ v3** (`openaq`) | `SEARCH_MCP_OPENAQ_API_KEY` | Unlocks OpenAQ v3 air-quality station queries. |
| **GitHub** (`github`, `github_code`) | `SEARCH_MCP_GITHUB_TOKEN` | Raises keyless `github` (repos/issues) from 10 to 30 req/min and unlocks `github_code`. |
| **Stack Exchange** (`stackexchange`) | `SEARCH_MCP_STACKEXCHANGE_KEY` | Raises keyless 300 req/day quota. |

Some cataloged declarative manifests support OAuth2 `client_credentials` pairs
(e.g., `epo_ops` using `SEARCH_MCP_EPO_OPS_KEY` + `SEARCH_MCP_EPO_OPS_SECRET`,
or `legifrance_open` using `SEARCH_MCP_LEGIFRANCE_CLIENT_ID` +
`SEARCH_MCP_LEGIFRANCE_CLIENT_SECRET`). Tokens are cached strictly in process
memory and never written to disk or logs. Run `search({ query: "...", scope: "catalog" })`
or `catalog.describe(id)` to see the exact env var(s) for any cataloged source.

---

## Opt-In Web Search APIs

These engines never run unless you explicitly pass them in `sources: [...]`
(and set their key):

| Engine | Provider | Environment Variable |
|---|---|---|
| `brave_api` | [Brave Search API](https://brave.com/search/api/) | `SEARCH_MCP_BRAVE_API_KEY` |
| `serper` | [Serper](https://serper.dev) | `SEARCH_MCP_SERPER_API_KEY` |
| `serpbase` | [SerpBase](https://serpbase.dev) | `SEARCH_MCP_SERPBASE_API_KEY` |
| `tavily` | [Tavily](https://app.tavily.com) | `SEARCH_MCP_TAVILY_API_KEY` |
| `anysearch` | [AnySearch](https://www.anysearch.com/) | `SEARCH_MCP_ANYSEARCH_API_KEY` |

Example explicit call:

```json
{"query": "rust async trait ergonomics", "sources": ["serper", "duckduckgo"]}
```

If an opt-in engine is named without its key configured, the server returns an
actionable notice pointing back to keyless sources so an agent can continue
without asking you for credentials.
