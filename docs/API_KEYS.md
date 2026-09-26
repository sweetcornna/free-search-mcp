# API keys: optional, manual

You can skip this page. free-search-mcp is a no-API-key server: nothing in its
default, reserve, rescue, fresh, locale or category engine pools uses a key,
and `tests/test_no_key_positioning.py` enforces that.

This page is for the operator who already has an account with a search API and
wants to point this server at it by hand. Two rules hold throughout:

- Opt-in engines never run on their own. `brave_api`, `serper`, `tavily`,
  `google_cse` and `github_code` run only when a call names them
  (`engines=["serper"]`) and you have set your own key. Named without one, the
  engine returns an error that says the search itself is fine and what to use
  instead.
- An agent never asks a user for a key. Setting one is something the operator
  of the machine does, outside any conversation.

`codex` is opt-in on the same terms but runs on the operator's ChatGPT
sign-in instead of a key (`search-mcp-login codex`). It is covered in
[CODEX_SEARCH.md](CODEX_SEARCH.md). `antigravity` does the same on a Google
Antigravity sign-in, against Google's terms; read
[ANTIGRAVITY_SEARCH.md](ANTIGRAVITY_SEARCH.md) first.

Three keyless engines (`anysearch`, `stackexchange`, `semanticscholar`) accept
an optional credential that changes only their limits. A GitHub token does the
same for `github` (from 10 to 30 requests/min) and also enables `github_code`,
because GitHub rejects anonymous code search.

One setting with "key" in its name is outside this page.
`SEARCH_MCP_AGENT_API_KEY` belongs to the optional answer agent and is the
credential of a language-model endpoint you chose. It is never sent to a search
engine, the agent is off unless you turn it on, and a local endpoint such as
Ollama has no key to set. [DELEGATION.md](DELEGATION.md) covers it.

## Two ways to set a key

### (a) The local settings page

```
uv run search-mcp-admin
```

For a plugin or uvx install, run `uvx --from free-search-mcp search-mcp-admin`
instead. Then open http://127.0.0.1:8765 in your browser, paste your keys into
the relevant provider fields, and click **Save**. A running server picks up the
new key on its next request, so no restart is needed.

The page is bilingual (中英双语): labels, badges, buttons, free-tier notes and
the "How to get a key / 如何获取密钥" steps appear in English and Chinese. The
first card is **Network / Proxy / 网络 / 代理**, the setting most people need,
and the provider cards follow it.

### (b) Environment variables / `.env`

Set the `SEARCH_MCP_<FIELD>` environment variable for each field you want to
configure. The variable names are:

| Provider | Environment variable(s) |
| --- | --- |
| Brave Search API | `SEARCH_MCP_BRAVE_API_KEY` |
| Serper (Google) | `SEARCH_MCP_SERPER_API_KEY` |
| Tavily (AI search) | `SEARCH_MCP_TAVILY_API_KEY` |
| Google Custom Search | `SEARCH_MCP_GOOGLE_CSE_API_KEY` and `SEARCH_MCP_GOOGLE_CSE_CX` |
| AnySearch (optional key) | `SEARCH_MCP_ANYSEARCH_API_KEY` |
| Semantic Scholar (optional key) | `SEARCH_MCP_SEMANTICSCHOLAR_API_KEY` |
| GitHub (optional token) | `SEARCH_MCP_GITHUB_TOKEN` |
| Stack Exchange (optional key) | `SEARCH_MCP_STACKEXCHANGE_KEY` |

Environment variables always take precedence over values saved in the file, so
existing 12-factor and container deployments keep working unchanged.

### Where keys are stored

Keys saved on the local settings page are written to:

```
~/.config/search-mcp/config.json
```

The file is written atomically with `0600` (owner read/write only) permissions,
and the page binds to localhost only.

## Brave Search API

A search API from Brave. Free tier: 2,000 queries/month.

How to get a key:

1. Open https://brave.com/search/api/ and click 'Get started'.
2. Sign up or log in, and verify your email.
3. Subscribe to the free 'Data for Search' plan (a card may be required, but the free tier isn't charged).
4. In the dashboard, open API Keys and copy your subscription token.

- Sign up: https://brave.com/search/api/
- Docs: https://api-dashboard.search.brave.com/app/documentation
- Environment variable: `SEARCH_MCP_BRAVE_API_KEY`

Use it:

```python
search("...", engines=["brave_api"])
```

## Serper (Google)

Google search results via the Serper API. Free tier: 2,500 free queries
(one-time).

How to get a key:

1. Open https://serper.dev and sign up (Google login works).
2. You land on the dashboard with 2,500 free credits.
3. Copy the API key shown under 'API Key'.

- Sign up: https://serper.dev
- Docs: https://serper.dev/playground
- Environment variable: `SEARCH_MCP_SERPER_API_KEY`

Use it:

```python
search("...", engines=["serper"])
```

## Tavily (AI search)

An AI-oriented search API. Free tier: 1,000 credits/month.

How to get a key:

1. Open https://app.tavily.com and sign up.
2. On the dashboard, find the 'API Keys' section.
3. Copy your key (it starts with 'tvly-').

- Sign up: https://app.tavily.com
- Docs: https://docs.tavily.com
- Environment variable: `SEARCH_MCP_TAVILY_API_KEY`

Use it:

```python
search("...", engines=["tavily"])
```

## Google Custom Search

Google's Programmable Search Engine (Custom Search JSON API). Free tier: 100
queries/day.

This provider needs two values, the API key and the Search engine ID (cx). Set
both before using the engine.

How to get a key:

1. Create an API key at https://console.cloud.google.com/apis/credentials by choosing 'Create credentials', then 'API key'.
2. Enable the 'Custom Search API' for that project: https://console.cloud.google.com/apis/library/customsearch.googleapis.com.
3. Create a search engine at https://programmablesearchengine.google.com/ and set it to 'Search the entire web'.
4. Copy the 'Search engine ID' (cx) from its control panel. Set both the API key and the cx.

- Sign up: https://programmablesearchengine.google.com/
- Docs: https://developers.google.com/custom-search/v1/overview
- Environment variables: `SEARCH_MCP_GOOGLE_CSE_API_KEY` and `SEARCH_MCP_GOOGLE_CSE_CX`

Use it:

```python
search("...", engines=["google_cse"])
```

## AnySearch (optional key)

A JSON REST search aggregator. A key is optional. Free tier: works keyless; a
key raises the rate limit/quota.

How to get a key:

1. AnySearch works anonymously with no key (lower limits).
2. For higher limits, sign up at https://anysearch.com and open Console, then API Keys.
3. Create a key and set it on the settings page or in the environment variable below.

- Sign up: https://anysearch.com/console/api-keys
- Docs: https://www.anysearch.com/docs
- Environment variable: `SEARCH_MCP_ANYSEARCH_API_KEY`

Use it (no key required):

```python
search("...", engines=["anysearch"])
```

## Semantic Scholar (optional key)

Scholarly search with the most metadata of the keyless literature sources:
abstracts, citation counts, influential-citation counts and direct open-access
PDF links.

Free tier: anonymous requests share one saturated pool and are answered with
HTTP 429 in practice; a free key gives 1 request/second.

Every unauthenticated request made while building this engine came back 429.
Without a key the engine therefore stays out of automatic `category="paper"`
routing, where it would spend a slot on a guaranteed empty result. Naming it in
`engines=["semanticscholar"]` still runs it.

How to get a key:

1. Request one at https://www.semanticscholar.org/product/api#api-key-form
2. Approval is by email and takes a few days.
3. Set the key on the settings page or in the environment variable below; `category="paper"` will then include the engine.

- Sign up: https://www.semanticscholar.org/product/api#api-key-form
- Docs: https://api.semanticscholar.org/api-docs/graph
- Environment variable: `SEARCH_MCP_SEMANTICSCHOLAR_API_KEY`

## GitHub (optional token)

Free tier: the `github` engine searches repositories and issues with no token,
at 10 req/min. A token raises that to 30 req/min and unlocks the `github_code`
engine, because GitHub's code-search API rejects anonymous requests.

How to get a token:

1. Create one at https://github.com/settings/tokens
2. Searching public repositories needs no scopes.
3. Set the token on the settings page or in the environment variable below.

- Sign up: https://github.com/settings/tokens
- Docs: https://docs.github.com/rest/search
- Environment variable: `SEARCH_MCP_GITHUB_TOKEN`

## Stack Exchange (optional key)

Free tier: 300 requests/day per IP keyless; an app key raises the quota.

How to get a key:

1. Register an app at https://stackapps.com/apps/oauth/register
2. Copy only the key value. The client secret is not needed.
3. Set the key on the settings page or in the environment variable below.

- Sign up: https://stackapps.com/apps/oauth/register
- Docs: https://api.stackexchange.com/docs
- Environment variable: `SEARCH_MCP_STACKEXCHANGE_KEY`

## At a glance

| Provider | Free tier | Engine it enables or lifts |
| --- | --- | --- |
| Brave Search API | 2,000 queries/month | `brave_api` (opt-in) |
| Serper (Google) | 2,500 queries (one-time) | `serper` (opt-in) |
| Tavily (AI search) | 1,000 credits/month | `tavily` (opt-in) |
| Google Custom Search | 100 queries/day | `google_cse` (opt-in; needs API key + cx) |
| GitHub | Keyless 10/min; token 30/min | lifts `github`; enables `github_code` (opt-in) |
| AnySearch | Keyless; key raises limits | lifts `anysearch` (already in the default pool) |
| Semantic Scholar | Anonymous pool is 429 in practice | makes `semanticscholar` usable and adds it to `category="paper"` routing |
| Stack Exchange | 300/day keyless | lifts `stackexchange` |
