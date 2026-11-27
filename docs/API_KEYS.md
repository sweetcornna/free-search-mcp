# API Keys & Optional Credentials (Manual Only)

You can skip optional credential setup to use the default keyless search pool.
This describes the integration's configuration, not a guarantee that every hosted
provider will accept anonymous requests, remain available, or keep its current limits.

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

## Trusted Opt-In Search Instances

`searxng` and `degoog` are registered only when their endpoint is configured.
Set `SEARCH_MCP_SEARXNG_BASE_URL` or `SEARCH_MCP_DEGOOG_BASE_URL` in the process
environment or `<config_dir>/.env`. Both are security-sensitive: CWD `./.env`
is ignored, and `config.json` does not load these endpoint settings. Process
values take precedence over trusted dotenv values; blank values are treated as unset.

Use only an instance you operate or trust: it receives your search queries.
Supply the instance root, not its `/search` route; Degoog also accepts a root
ending in `/api`. URLs must be HTTP(S) without userinfo, query, or fragment.
Optional `SEARCH_MCP_SEARXNG_TOKEN` and `SEARCH_MCP_DEGOOG_API_KEY` credentials
come from your instance operator and are read through `Config::api_key`.
Credentials require HTTPS and are sent as Bearer tokens. Redirects are disabled
and SSRF checks still apply; do not disable those checks merely to try a public instance.
SearXNG must allow JSON search responses. No public instance availability or
anonymous access is promised.

Restart after configuration, then explicitly select `sources: ["searxng"]` or
`sources: ["degoog"]`. A credential alone does not enable an instance.

## Obtaining Search Provider Credentials

Verification date: **2026-11-27**. The official setup pages below are undated;
these are setup pointers, not verified pricing, quota, or availability guarantees.
Create credentials yourself in the provider's site, store them in trusted
configuration (never CWD dotenv), restart, and explicitly select the source.
Do not paste credentials into chat or commit them.

| Source | Official acquisition route | Configuration variable |
|---|---|---|
| `brave_api` | Follow **Get API key** on [Brave Search API](https://brave.com/search/api/) to the [registration dashboard](https://api-dashboard.search.brave.com/register), then obtain a subscription credential. | `SEARCH_MCP_BRAVE_API_KEY` |
| `serper` | Start at [Serper](https://serper.dev) and use its account/dashboard flow; the fetched public page did not expose exact key-management steps, so those remain unverified. | `SEARCH_MCP_SERPER_API_KEY` |
| `serpbase` | Use [SerpBase](https://serpbase.dev) and its **Get API Key** console link; follow the provider's account setup. | `SEARCH_MCP_SERPBASE_API_KEY` |
| `tavily` | Follow **Get API key** from [Tavily documentation](https://docs.tavily.com/) to the [dashboard](https://app.tavily.com), then obtain your key. | `SEARCH_MCP_TAVILY_API_KEY` |
| `anysearch` | Start at the [official provider site](https://www.anysearch.com/); exact authenticated key-creation steps were not verified. | `SEARCH_MCP_ANYSEARCH_API_KEY` |
| `exa_search` | The [official quickstart](https://docs.exa.ai/reference/quickstart) links to the [Exa API-key dashboard](https://dashboard.exa.ai/api-keys); sign in and generate a key. | `SEARCH_MCP_EXA_API_KEY` |
| `firecrawl` | Follow the [introduction](https://docs.firecrawl.dev/introduction), create/sign in to an account, then obtain a key from [API keys](https://www.firecrawl.dev/app/api-keys). | `SEARCH_MCP_FIRECRAWL_API_KEY` |
| `jina_search` | Use **Get your API key** on [Jina Reader](https://jina.ai/reader), or the [API dashboard](https://jina.ai/api-dashboard), to obtain/manage a key. | `SEARCH_MCP_JINA_API_KEY` |

The Exa, Firecrawl, and Jina search manifests require keys in this integration.
Anonymous Reader or scrape access does not establish anonymous search access.
Account creation, authenticated dashboards, credential validity, live search
requests, current plans, and quotas were not tested. The search MCP connection
was unavailable; official pages were inspected through direct web fetching instead.
