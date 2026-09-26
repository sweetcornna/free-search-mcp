# Changelog

All notable changes to this project are documented here. The format is loosely
based on [Keep a Changelog](https://keepachangelog.com/), and the project follows
semantic versioning.

## [0.13.1] - 2026-09-26

Documentation only; the server is the same as 0.13.0. This release exists so
that the package page shows the new README.

### Changed

- The README is a quarter of its former length and says how to set up the two
  sign-in engines, `codex` and `antigravity`: what each needs and spends, the
  Antigravity warning, signing in, naming the engine, the settings, servers,
  SSH and Docker, signing out, and the errors people meet with what to do.
- The longer sections moved unchanged into `docs/HOW_IT_WORKS.md`,
  `docs/INSTALL.md`, `docs/DELEGATION.md` and `docs/CONFIGURATION.md`. The
  settings table now lists the `SEARCH_MCP_ANTIGRAVITY_*` settings as well.

## [0.13.0] - 2026-09-26

Two opt-in engines that search on an account sign-in instead of an API key.
`codex` runs OpenAI's own web search on a ChatGPT plan, a use OpenAI allows
from third-party tools. `antigravity` runs Google Search through a Gemini model
on the sign-in of Google's Antigravity IDE, a use Google's terms forbid, with
account suspension as the stated consequence: read docs/ANTIGRAVITY_SEARCH.md
before signing in. Neither is in any default pool or route, so a search
reaches one only when a call names it, and both follow their backend's model
catalogue so a new model is used as soon as it is listed.

### Added

- An opt-in `codex` engine that runs OpenAI's own web search, the one Codex
  uses, on the operator's ChatGPT plan. It needs no API key and no API
  billing; each search counts against the plan's Codex usage. OpenAI supports
  signing in with ChatGPT from third-party tools. The engine follows the rules
  the key engines follow: it is in no pool, reserve or route, and named before
  a sign-in it returns the usual "not configured" error, which now also tells
  the agent not to ask for a sign-in. It calls the backend the way the current
  Codex CLI does (openai/codex, read on 2026-09-26). The first choice is
  `alpha/search`, whose structured results supply every title, URL and
  snippet. On a deployment without that endpoint it uses `/responses` with the
  hosted `web_search` tool, and keeps only URLs the search itself cited. An
  expired token is refreshed once and the search retried. A spent usage window
  is reported with its reset time, and a model the plan lacks names
  `SEARCH_MCP_CODEX_MODEL`. Neither counts against the engine in the breaker.
- `search-mcp-login codex` signs in the way `codex login` does: OAuth with
  PKCE, returning to `127.0.0.1:1455`, or 1457 when 1455 is taken. Over SSH,
  paste the address the browser lands on into the terminal. `--use-codex-cli`
  links the Codex CLI's `auth.json` read-only instead, and never refreshes or
  rewrites it. `search-mcp-login status` and `search-mcp-login logout codex`
  show and forget the sign-in. Tokens live in
  `<config_dir>/oauth/codex.json` (`0600`). OpenAI rotates refresh tokens, so
  refreshes are serialised per process and, where `fcntl` exists, across
  processes.
- No sign-in step is needed either: the first time `codex` is named with no
  sign-in stored, the server opens the ChatGPT sign-in page in the local
  browser and finishes the search once it is approved. One search waits up to
  45 s (`SEARCH_MCP_CODEX_SIGNIN_WAIT_SECONDS`), and the page stays answerable
  for ten minutes for the next one. It happens only over stdio and where a
  desktop browser can be started, and at most one unanswered page per server
  run. The browser is launched with its output detached, because over stdio
  stdout is the MCP connection. `SEARCH_MCP_CODEX_AUTO_SIGNIN=false` turns it
  off. A search that stopped waiting for that approval is not cached, so the
  same search repeated after the approval reaches `codex`. The settings page's
  Test button never starts a sign-in; it says the engine is not signed in.
- A callback with the wrong `state` is refused and the sign-in keeps waiting,
  as in the Codex CLI, so a web page cannot abort a sign-in in progress. The
  browser tab says "Signed in" only once the tokens are stored, and shows the
  error when the exchange fails.
- The settings page has a Codex card with Sign in, Test and Sign out.
- New settings: `SEARCH_MCP_CODEX_MODEL` (`latest`),
  `SEARCH_MCP_CODEX_TIMEOUT`, `SEARCH_MCP_CODEX_REASONING_EFFORT`,
  `SEARCH_MCP_CODEX_BASE_URL`, `SEARCH_MCP_CODEX_AUTO_SIGNIN`,
  `SEARCH_MCP_CODEX_SIGNIN_WAIT_SECONDS`. The guide, with a Chinese summary, is
  docs/CODEX_SEARCH.md.
- An opt-in `antigravity` engine that runs Google Search through a Gemini
  model on the sign-in of Google's Antigravity IDE, at the operator's own
  risk: Google's Antigravity terms forbid using that sign-in from third-party
  tools and Google has suspended accounts for it. The backend licenses only
  requests that identify as Antigravity, so the engine sends Antigravity's user
  agent. It never opens a sign-in by itself; `search-mcp-login antigravity`
  and the settings page card say what it risks before the sign-in starts. It
  calls `v1internal:generateContent` with the `googleSearch` tool, and every
  result URL comes from the reply's grounding, resolved from Google's redirect
  to the page. A reply without search results is asked once more and then
  yields nothing, because some models answer from memory. The daily sandbox
  host is tried before production, which refused every search with 429 for a
  free-tier account. Antigravity's OAuth client is not in the package: the
  sign-in reads it from the Antigravity install on the machine, recognised by
  a fingerprint, or from `SEARCH_MCP_ANTIGRAVITY_CLIENT_ID` and
  `SEARCH_MCP_ANTIGRAVITY_CLIENT_SECRET`, and stores it with the tokens. New
  settings: `SEARCH_MCP_ANTIGRAVITY_MODEL` (`latest`),
  `SEARCH_MCP_ANTIGRAVITY_TIMEOUT`, `SEARCH_MCP_ANTIGRAVITY_BASE_URLS`,
  `SEARCH_MCP_ANTIGRAVITY_VERSION`, `SEARCH_MCP_ANTIGRAVITY_CLIENT_ID`,
  `SEARCH_MCP_ANTIGRAVITY_CLIENT_SECRET`. The guide is
  docs/ANTIGRAVITY_SEARCH.md.
- Both sign-in engines follow their model catalogue by default
  (`SEARCH_MCP_CODEX_MODEL=latest`, `SEARCH_MCP_ANTIGRAVITY_MODEL=latest`),
  looked up at most every six hours, so a new model is used as soon as the
  backend lists it. `codex` takes the newest generation's lightest model that
  can search and is not being retired; `antigravity` takes the flash model
  Antigravity offers, and asks Antigravity's own web-search model when that
  one answers without searching. Each account has its own lookup, and a
  model the service refuses is looked up again on the next search. A model
  name pins it, as before.
- The sign-in listener takes both `127.0.0.1` and `::1` when the registered
  redirect says `localhost`, because a browser may try IPv6 first.

### Changed

- The `engines` tree's closing line says the opt-in extras run on the
  operator's own API key *or account sign-in*, and asks the agent not to
  request either.

## [0.12.0] - 2026-09-22

Three of the four default engines had stopped contributing, and no structural
test could see it. Bing answered every multi-word query with ten well-formed
results about the first word only (the Steam page for "rust ownership
borrowing"). Mojeek sat behind a captcha on every request. Google News put
redirect links that cannot be deduplicated into ordinary searches. Rank fusion
interleaved all of that with DuckDuckGo's real answers at positions 3, 6 and 9.

The errors that would have helped were also lost. 0.11.0 declared
`mcp[cli]>=2.0.0`, so every `uvx` install ran SDK 2.2 while CI tested the
locked 2.0.0. Since SDK 2.1 the message of any exception other than `ToolError`
is replaced, so an actionable error such as "at most 20 URLs" reached the
client as `Error executing tool fetch_batch`.

This release fixes both problems. It also adds the signals an agent needs to
judge how old and how reliable a result is, makes keyless operation a tested
property, and makes the plugin the main way to install.

Upgrading: the search cache key changed, so the first search after the upgrade
is cold. You do not need to delete anything. Output changed in three places
that a program parsing it would notice, all listed under Changed.

### Fixed

- Bing returns results about the whole query again. Measured on 2026-09-21
  with one fresh query per variant, because Bing caches per query: the 0.11.0
  request (`?q=…&count=10`) scored 0 of 10 on topic, a warmed cookie jar plus
  `form=QBRE` without `count=` scored 9 to 10 of 10, and adding `count=10` back
  produced decoys again. The engine now mints one cookie jar per proxy egress
  with a 30-minute lifetime. Minting is single-flight, a failed warm-up is
  remembered for 60 s, and it never raises. The engine never sends `count=`,
  pages with `first=11` when more than ten results are requested, and discards
  the jar and retries once when a response still looks like a decoy.
- Error messages reach the client. Every tool is registered through a boundary
  that converts the exceptions a caller can act on into `ToolError` with the
  original text: `ValueError`, `OSError`, the new `FetchError`, size caps, a
  missing browser and `httpx` errors. Any other exception is logged and
  reported as `internal error (<Type>)`, described as a server bug, without its
  message, because the message may contain paths or response bodies.
- The SDK under test is the SDK users run. The requirement is now
  `mcp[cli]>=2.2.0,<2.3`. `uvx` ignores lockfiles, which makes the declared
  range the only constraint in production, and 2.1.0 was a minor release that
  changed handler semantics. CI gained a daily `sdk-canary` job with two legs.
  One resolves the newest SDK inside the range and is a hard gate that the
  release reuses. The other ignores the upper bound and may fail, which shows
  in advance what the next bump will meet. Its first run found two things a
  fresh install already met: trafilatura 2.2 keeps no heading from a page
  with a single paragraph, and curl_cffi 0.16 removed the helper a test used
  to check impersonation profiles. The tests now hold on both the locked and
  the newest versions.
- Markdown is no longer delivered inside JSON. The SDK derived an output schema
  of `{"result": <str | dict>}` for the dual-format tools and sent the markdown
  twice, as a text block and as `structuredContent={"result": "…"}`. Clients
  that prefer structured content, Claude Code among them, showed the model the
  second copy, a single JSON string with every newline escaped. That cancelled
  the token saving the markdown default exists for.
- The same page is merged when engines disagree about its address. Rank fusion
  is keyed on an identity that ignores scheme, `www.`, default port and
  fragment, so the `http://` and `https://` copies of an arXiv page count as
  one result with two votes. An `https` sighting upgrades the URL that is
  printed. The key itself is never emitted, because emitting it regressed two
  ranking cases. Title de-duplication used to treat a number that appears in
  only one of two titles as a version mismatch, which kept the `abs/` and
  `pdf/` copies of one paper apart. It now compares numbers only when both
  titles have them.
- Google News results are publisher URLs. The first `max_results` links are
  resolved four at a time within four seconds in total. Resolution stops after
  three consecutive failures and keeps the original link when it fails. As a
  result `include_domains`, category filters and cross-engine merging see the
  real host in place of `news.google.com/rss/articles/…`.
- `cache://page/{url}` finds pages whose URL contains an escape. The handler
  unquoted a value the SDK had already decoded, so every non-ASCII Wikipedia
  link was a permanent miss.
- `paper_graph` resolves arXiv identifiers. OpenAlex answers 404 for DataCite's
  `10.48550/arXiv.*` DOIs, so those now go through the arXiv API for the title
  and then an exact-title lookup, and `notes` says the fallback was used. A
  bare id such as `1706.03762` or `hep-th/9901001` is accepted when it is the
  whole input. Before, it was searched as a title.
- `extract_structured` no longer returns the whole article. JSON-LD
  `articleBody`, `text` and `description` values longer than 500 characters are
  clipped, with their original length recorded under `trimmed`. RDFa nodes that
  carry only a layout `role` are dropped. Dates, authors, prices and event
  fields are unchanged.
- `read_doc` reads API responses that have no file extension. A URL such as
  `https://pypi.org/pypi/uv/json` was rejected as an unsupported format because
  only the path was consulted for structured text. `application/json`,
  `application/xml`, `application/yaml` and the `+json` and `+xml` suffixes are
  now recognised from the content type, so a 4.8M-character JSON response can
  be read in pages with `start` and `length`.
- arXiv queries match more than the exact phrase. The query is now
  `all:"<phrase>" OR (all:w1 AND all:w2 …)` with a small stopword list and at
  most eight terms. Quoted and one-word queries pass through unchanged.
- `engines(group=…)` rejects a mistyped group and lists the valid ones. It used
  to return an empty tree.
- `cp .env.example .env` works. The example file has comments on the same line
  as values, and the project's `.env` loader read them as part of the value, so
  a verbatim copy failed validation at import with four errors. The loader now
  ends an unquoted value at a `#` that follows whitespace, which leaves URL
  fragments and passwords containing `#` intact. The example file also keeps
  its comments on their own lines now.
- Browser errors arrive without Playwright's call log. A failed navigation
  reaches the model through the `errors` map of a search and through the
  message of a failed `fetch`, and both now stop at the first line, for example
  `Page.goto: net::ERR_CONNECTION_CLOSED at <url>`.
- The test suite removes provider keys from the environment, so a developer
  who has `SEARCH_MCP_SERPER_API_KEY` exported runs the same product as CI.
- A live test run (`SEARCH_MCP_TEST_NETWORK=1`) no longer sends a fake key to
  Serper. A test of the `.env` loader left `SEARCH_MCP_SERPER_API_KEY=from-dotenv`
  in `os.environ` for the rest of the process, which was invisible offline.
  Every test now gets its `SEARCH_MCP_*` environment restored afterwards. The
  live tests for Mojeek, Zhihu, Sogou and Serper also skip with a stated reason
  when the site shows a detected wall, cannot be reached, or rejects the key.
  An empty answer with no wall detected still fails.

### Added

- Twenty-four direct-fact sources, all keyless JSON APIs, under eight new
  groups and three new finance sub-groups: `software` (`endoflife`,
  `github_releases`, `pypi`, `npm`, `crates`, `registries` for Maven Central,
  RubyGems, the Go proxy, Homebrew, Docker Hub, Packagist and NuGet,
  `appstore`), `security` (`nvd`, `osv`, `cisakev`), `reference` (`wikidata`,
  `rdap`), `weather` (`openmeteo`), `docs` (`mdn`, `ietf`), `gov`
  (`federalregister`, `govuk`), `stats` (`wdi`, World Bank indicators),
  `calendar` (`holidays`, `worldclock`), `finance.fx` (`frankfurter`, with
  the open ExchangeRate-API endpoint for currencies the ECB lacks, and
  `cfets` for the PBOC's RMB central parity), `finance.entity` (`gleif`) and
  `finance.crypto` (`coingecko`). They return the record itself, dated by its
  publisher: the current release of a package with its upload date, the
  support end date of a Python or Ubuntu cycle, a CVE with its CVSS score and
  whether CISA lists it as exploited, the advisories touching a package with
  the fixed version, a Wikidata item's population or inception with the date
  Wikidata attaches, a country's GDP or population with the revision date,
  the ECB rate between two currencies, today's forecast for a named place,
  this year's holidays with China's make-up working days, the time in a
  named city from the server clock, a domain's expiry from its registry, an
  RFC with its standards level. Package names, places, countries,
  currencies, coins and entities are read from the words of the question in
  English or Chinese. All stay out of the default pool; the finance
  sub-groups are registered after the existing three so bare `finance` is
  unchanged.
- Claims routing: each record source declares offline whether it can answer
  a question (`Engine.claims`), and a search with no category and no engines
  seats the first three that say yes next to the default pool
  (`SEARCH_MCP_CLAIM_ENGINE_LIMIT`, `SEARCH_MCP_AUTO_ROUTE_ENABLED`). An
  agent no longer needs the category tree to reach a registry:
  "CVE-2024-3094", "100 usd to cny", "上海明天天气", "latest fastapi version"
  and "现在东京几点" each reach their source unasked. The claimants are listed
  as `auto_routed` and named in the markdown. Measured 2026-09-22 on eleven
  such questions: the record led in every one, in 2.0 to 7.2 s.
- A search deadline (`SEARCH_MCP_SEARCH_DEADLINE_SECONDS`, 10). Once at least
  one engine has answered with results, engines still running ten seconds
  after the fan-out started are cancelled and reported as
  `timed_out_engines`; their slot counts as an error, so the breaker benches
  a source that keeps timing out. Measured 2026-09-21: the pool answers in
  2 to 4 s and the tail was one browser-rendered engine at 15 s or more.
  When nothing has answered by the deadline the search keeps waiting.
- A `direct_answer` engine flag and a per-query `answers_directly(query)`
  hook. When it claimed the question or its category was requested, the
  first result of such an engine counts five times a general engine's in
  the rank fusion, the rule being that one looked-up record outranks the
  four-engine default pool agreeing on a page about it. `nvd`, `cisakev` and
  `ietf` apply it only for a CVE id or an RFC number. A record merged with a
  web sighting of the same page keeps the record's title. Measured 2026-09-21 on eleven category probes: with the ordinary
  doubling the PyPI record ranked below three snippets of the PyPI project
  page and Open-Meteo's numbers ranked third; with the weight both lead. A
  record that won the top rank is also taken as the `Lead:` line without the
  usual test that the snippet echoes the question, since "10000 JPY = 425.70
  CNY" answers "1万日元等于多少人民币" without sharing a word with it.
- Category cache caps for the record categories, applied whatever the query
  said about freshness: `weather` and `finance.fx` answers expire after one
  hour, `software` and `security` after six. A cached forecast or exchange
  rate served for the default seven days would be wrong while looking exact.
- An off-topic guard runs before rank fusion (`coherence.py`). A bucket's
  coherence is the share of its results that mention any query term beyond the
  first. Healthy engines measure 0.5 to 1.0 and decoys 0.00 to 0.20, so the
  threshold is 0.3. A suspect bucket is dropped only when another bucket with
  coherence of at least 0.5 shows that the query's words do get echoed. A lone
  suspect triggers a rescue search for a second opinion, and two suspects that
  agree are both kept. Short queries, thin buckets and specialist sources are
  never judged. A dropped engine is reported as `off_topic` with a hint that a
  proxy will not help. `SEARCH_MCP_COHERENCE_GUARD_ENABLED=false` turns the
  guard off.
- A circuit breaker and a reserve bench (`health.py`). A captcha, consent wall,
  JS wall, login wall or off-topic verdict benches an engine immediately. Two
  consecutive errors do the same, as do three empty results in a row while
  other engines answered. The cooldown is 10 minutes, doubles up to an hour,
  and is cleared by one success. The reserves `so360`, `brave` and `searx` are
  used in that order, and only while fewer than three general engines are
  healthy. The cache is keyed on the nominal pool so that it does not change
  with engine health, a result from a degraded pool is replayed for at most an
  hour, and the output lists `benched_engines` with the reason, the retry time
  and the substitute. A missing local browser, a rate-limit skip and a missing
  key never count against an engine. An open bench is written to
  `<cache_dir>/engine_health.json` and read by the next process: Mojeek took
  2.6 to 6.2 s to serve its captcha page, and every fresh process was paying
  that on its first search (8.5 s, against 4.0 s once the file existed).
- Snippet dates are checked against today. A snippet carries every date the
  page mentions, and the FastAPI page on PyPI announced a conference "on
  October 28, 2026", which came out as the page's publication date and put it
  ahead of pages dated this week. A date after tomorrow is skipped and the
  next date in the text is tried.
- Signals for judging a result. Search output carries `retrieved_at`,
  `cache_age_seconds` and `dated_results` ("4 of 10"). Each result has a
  `date_source` (`structured`, `snippet` or `none`) and a `source_type` (paper,
  code, forum, news, government or academic), which describes the kind of site
  and says nothing about quality. A `freshness_note` appears when `freshness=`
  was requested and half the results have no verifiable date, and a
  `usage_note` says that snippets locate sources while details come from the
  page. `fetch` reports the publication date, or says none was found, along
  with the age of a cached copy. `research` keeps each source's date and type
  and adds a `date_note` when sources are undated or more than a year apart.
- Cache lifetimes follow the question. `freshness="day"` results are served
  from cache for an hour, `"week"` for six hours and `"month"` for a day.
  `category="news"` results are cached for six hours at most.
- A `verified-research` skill in the plugin. `claude plugin details` projects
  about 210 tokens for its description in every session and about 2.1k when it
  is invoked. The workflow is: fix today's
  date, use `search` only to locate URLs, read the primary page, check the
  publication date, cache age and edition, corroborate with an independent
  source, then report with dated citations and a "Could not verify" list. A
  shorter form of the same rules is in the server `instructions`, the tool
  docstrings and the four prompts, for clients that never see the skill. 0.11.0
  advertised "no skills, no always-on prompt tokens", and this reverses that.
  The failure it addresses is an answer assembled from snippets of last year's
  page, which no change inside a tool can prevent.
- A `quick-search` agent in the plugin, addressed in Claude Code as
  `free-search:quick-search`. Every page fetched to check one date used to stay
  in the main conversation for the rest of the session. The agent takes one
  question, makes one `research` call and at most two more reading calls in its
  own context, and replies with one to three sentences, up to five dated source
  lines and a "Not verified" line. It is sized to return fast: `haiku`, at most
  6 turns, four tools (`research`, `search`, `fetch`, `read_doc`), no skill
  preload and no `CLAUDE.md`. It has no shell, no file tools, no `download` and
  no other MCP server, and it does not follow instructions that appear on a
  fetched page. A first draft preloaded the verification skill and ran on
  `sonnet` for up to 30 turns. One delegated lookup took it 38 s and $0.22, and
  one took this agent 25 s and $0.09 (different questions, so only the scale
  is comparable). `claude plugin details` projects about 130 tokens
  for its description in every session and about 560 each time it is spawned,
  which puts the whole plugin at about 340 always-on tokens.
- The same agent for other hosts. `search-mcp agent-file claude-code|codex|prompt`
  prints it as a Claude Code agent file, as a Codex custom-agent TOML file
  (`plugins/free-search/codex/agents/quick_search.toml` is that output) or as
  bare instructions. A fifth MCP prompt, `quick_search`, returns the
  instructions plus a question to any MCP client. All of them come from one
  string, `agent.HOST_AGENT_PROMPT`, and the manifest test fails when a
  committed file differs from it. Measured with Codex CLI 0.154: `codex exec`
  exposed no parameter for choosing an agent by name, so the TOML file could
  not be selected there, while a generic subagent given the prompt text used
  the search tools and answered in 87 s.
- An optional `ask` tool. It is off by default and unregistered while off, so a
  default install still lists 11 tools. With `SEARCH_MCP_AGENT_BACKEND` set to
  `api`, `claude-code` or `codex`, `ask(question)` has the server run one
  `research` call, hand the pages to a model and return a few sentences with
  dated sources, the model used and the time taken. `api` speaks the OpenAI
  chat-completions dialect or the Anthropic messages dialect over httpx with no
  new dependency, and a local endpoint such as Ollama needs no key.
  `claude-code` runs one `claude -p` with built-in tools off and extended
  thinking off, which took the same evidence from 13.7 s to 5.2 s. `codex` runs
  one `codex exec` in a read-only sandbox, in an empty directory, with Codex's
  own web search disabled. The model may spend `SEARCH_MCP_AGENT_MAX_STEPS`
  (default 1) further calls on `search`, `fetch` or `read_doc` and has no other
  tool. The child server it is given has local file reads switched off and
  never receives the model endpoint's key, and every field of a page that a
  site controls (title, snippet, URL, error text) is sealed before it reaches
  the model, so a page cannot forge a second page or a server note.
  Neither CLI can forbid tool use on the last turn, and `haiku` spent its
  turns on tools until the run ended in `error_max_turns`, so the child server
  enforces the limit itself through `SEARCH_MCP_TOOL_CALL_BUDGET`. When the
  model fails or the 90 s deadline passes, `ask` returns the pages as a
  `research` brief with the reason. The pages read before the model is called
  share an 8 s budget (`SEARCH_MCP_AGENT_READ_SECONDS`), and a page that misses
  it is listed as not read while its snippet is still used: one Baidu Baike
  page had taken 25.7 s to fail while the two pages holding the answer arrived
  in 0.5 s and 2.1 s. The deadline kills the CLI's whole process
  group, because killing only the parent left its children holding the output
  pipe and the wait did not return until they exited. Model part, measured
  2026-09-21 when the first pages were enough: 6.4 s on a local Ollama
  `qwen3:1.7b`, 4.8 s on `claude-code` with `haiku`, 13 s on `codex`. Search is
  keyless in every mode. The only credential involved belongs to the model
  endpoint the operator chose.
- `search-mcp ask "question"` runs the same thing as a one-shot command, and
  `search_mcp.agent.ask(question, answer_with=my_model)` lets a Python service
  supply its own model with no setting at all.
- `SEARCH_MCP_TOOLS`, an allow-list of tool names. An embedder that needs
  `search` and `fetch` registers those two and nothing else. A bare `claude -p`
  measured 9.3k input tokens with the full server attached against 0.7k with
  none, nearly all of it tool definitions.
- An MCP Registry entry (`server.json`, `io.github.sweetcornna/free-search-mcp`)
  and a Claude Desktop bundle (`mcpb/`, MCPB 0.4 with the `uv` runtime). The
  bundle contains no server code and pins this release. Both offer three
  optional settings (proxy, region, cache directory) and have no key field. The
  release workflow attaches the `.mcpb` as a third asset and publishes to the
  registry from a job whose only credential is an OIDC token.
- Protocol features. A truncated `fetch` carries a resource link to
  `cache://page/…`, which is the only way to reach the rest of an HTML page. A
  JSON `search` carries one to `cache://search/…`. Links are sent only to
  clients on protocol 2025-06-18 or later. There are completions for prompt
  arguments and for both `cache://` templates, a server description and icon,
  and a cache hint for `server/discover`.
- `tests/test_mcp_boundary.py` calls all eleven tools through a real client in
  both protocol eras, without network, and asserts the full text of every
  anticipated error. `scripts/smoke_mcp.py` does the same over stdio against
  the live web. `tests/test_live_canary.py` asserts coherence on a query seeded
  with the date, so that Bing's per-query cache cannot make a broken engine
  look healthy.
- Ranking evals check their own input.
  `evals/ranking/fixtures/decoy_and_dupes.json` is a dated slice of the
  2026-08-29 capture and is replayed in CI. `replay.py --guard` exits non-zero
  if the guard would drop a bucket that the capture does not mark as a decoy.

### Changed

- The default pool is `duckduckgo`, `bing`, `anysearch`, `mojeek`. `googlenews`
  joins only for `freshness="day"` or `"week"`. `so360` joins queries written
  in Chinese, where it was measured returning the official site first.
  `payload["engines"]` lists the engines that were queried. `docker-compose.yml`
  moved from its own three-engine list to the same four.
- No tool advertises an `outputSchema` (ten did). `format="markdown"` returns
  one text block and no structured content. `format="json"` returns the JSON as
  text plus the object itself as structured content. The `{"result": …}`
  envelope is gone for objects. `fetch_batch` and `cache_search` return arrays
  and keep it, because structured content must be an object.
- Markdown headers changed. `_(from cache)_` is now
  `_(cached 3 days ago · retrieved …)_`, each result gains a kind and date
  crumb, and search output ends with the usage note.
- The search cache key is versioned (`v: 2`) and includes region, safe-search
  and accept-language. Rows written by earlier versions may hold up to seven
  days of decoy results, and they are never read again.
- Keyless operation is tested. `tests/test_no_key_positioning.py` fails if the
  default pool, the reserves, the rescue list, a locale route or any category
  routes to an engine that needs a key. The five opt-in engines stay available
  by name. Asking for one without a key now returns an error that says the
  engine did not run and the search itself is fine, names the keyless engines
  to use, and tells the model not to ask the user for a key. `engines` lists
  the opt-in engines on one closing line, outside the tree, and its JSON gains
  `opt_in` and `not_auto_routed`.
- Documentation leads with the plugin. The install order is plugin, Claude
  Desktop bundle, MCP Registry, then `uvx`, source and Docker. The API-key
  material moved to an optional section at the end of Configuration.
- The version now lives in seven files. `tests/test_plugin_manifest.py` and the
  release workflow check all of them, and `docs/RELEASING.md` lists them.
- `evals/ranking/buckets.json` is untracked, as its `.gitignore` entry always
  intended. The native-category weight of 2.0 was measured again on a capture
  without decoys: three cases improved and none regressed.

## [0.11.0] - 2026-08-30

`image` and `dataset` no longer replace the general pool with a single
specialist source: this release adds redundancy to both exclusive categories,
while `paper.math` adds the missing mathematics index without changing the
existing paper spread. It also carries the Claude Code plugin, which makes
this the first version installable without a `claude mcp add` line.

### Added

- **Install as a Claude Code plugin.** `/plugin marketplace add
  sweetcornna/free-search-mcp` followed by
  `/plugin install free-search@free-search-mcp` registers the same `search`
  stdio server with no `claude mcp add` line and no checkout. The marketplace
  is `.claude-plugin/marketplace.json` in this repo; the plugin is
  `plugins/free-search` and declares exactly one MCP server and nothing else —
  no skills, no hooks, no always-on prompt tokens. Its `.mcp.json` pins
  `free-search-mcp==<plugin version>`, so an installed plugin runs the package
  it advertises, and `/plugin update free-search` is what moves a user to a
  newer server.
- **`docs/RELEASING.md`.** The release process was only ever encoded in
  `.github/workflows/release.yml`; it is now written down, including the plugin
  step it gained here — the four files a version lives in, why the pin has to
  move in the same commit that bumps `pyproject.toml`, and how to verify PyPI,
  the GitHub Release, and a real plugin install afterwards.
  `tests/test_plugin_manifest.py` fails on any drift between the pin, the
  plugin manifest and `pyproject.toml`, and the release workflow re-checks both
  against the pushed tag before it builds anything.
- **Seven new keyless sources bring the registry from 42 to 49 engines.**
  `dryad`, `dataverse`, `figshare`, `huggingface`, and `dataeuropa` add
  repository, machine-learning, and EU public-sector dataset coverage;
  `wikimedia` adds Wikimedia Commons image search; and `zbmath` adds a
  mathematics literature index. None enters the default pool: each is reached
  through `category=`, so ordinary web searches do not pay for the specialist
  fan-out.
- **Four new `Category` tokens expose the new branches.**
  `dataset.repository`, `dataset.ml`, `dataset.gov`, and `paper.math` let a
  caller narrow to a known kind of dataset or paper; `zenodo` now also declares
  `dataset.repository`, putting all four repository sources in the same branch.
- **Rejected candidates now have an explicit reason to stay out.** The source
  probe rejected `data.gov` (the legacy CKAN endpoint returned 404 and its
  replacement requires an API key), Eurostat, OECD, and UN UNdata (no working
  keyless free-text search), IETF Datatracker (its accepted `search` parameter
  is silently ignored), and Reddit (anonymous search returns 403 and current
  API access requires OAuth). OSF Preprints and HAL are technically capable
  adapters, but operator robots policy — not adapter capability — kept them
  out.

### Changed

- **Bare dataset routing now spends its three slots across three different
  sub-groups.** `category="dataset"` selects `dryad`, `huggingface`, and
  `dataeuropa` — one repository, one ML, and one government source — instead of
  three overlapping repositories. The full `dataset.repository` branch is
  `dryad`, `dataverse`, `zenodo`, and `figshare`, while `dataset.ml` and
  `dataset.gov` narrow to `huggingface` and `dataeuropa` respectively.
- **The existing paper spread is deliberately preserved.**
  `category="paper"` still selects `arxiv`, `openalex`, and `europepmc`; the
  mathematics index is reached explicitly with `category="paper.math"`, so
  `zbmath` does not displace one of the three existing corpora.

### Fixed

- **Exclusive image and dataset searches no longer have a single point of
  failure.** Before this release, the general web pool was intentionally
  replaced by exactly one specialist — `openverse` for `image` and `zenodo` for
  `dataset`. An outage, rate limit, or missed hit therefore left no specialist
  fallback and made source unavailability indistinguishable from no matching
  item. `image` now has `openverse` and `wikimedia`, while `dataset` has five
  sources across the repository, ML, and government sub-groups.

## [0.10.0] - 2026-08-29

Finance and scholarly literature get real sources instead of hostname filters,
`category` becomes a two-level tree the agent can navigate, and a batch of
accuracy bugs that quietly corrupted results are fixed.

### Added

- **Ten new keyless sources.** Finance: `sec_edgar` (US filings full text),
  `cninfo` (A-share / HK announcements), `yahoofinance` (ticker resolution and
  market news), `worldbank` (Documents & Reports), `imf` (DataMapper series,
  WEO forecasts included). Scholarly: `europepmc` (40M life-science records
  including preprints), `dblp` (computer science), `doaj` (open access),
  `clinicaltrials` (registered human trials), and `semanticscholar` behind an
  optional key. Before this, a finance query had no specialist source at all
  and fell through to the general web pool.
- **`category` is now a two-level tree.** A bare group widens, a dotted
  sub-group narrows: `paper.index`, `paper.preprint`, `paper.biomed`,
  `paper.cs`, `paper.openaccess`, `paper.trial`, `finance.filings`,
  `finance.market`, `finance.macro`, `news.world`. Every token appears in the
  tool schema's enum, so an agent discovers the whole tree without a second
  call. Preprint search runs through Europe PMC's `SRC:"PPR"` clause because
  bioRxiv's own API cannot keyword-search at all.
- **`paper_graph` — the eleventh tool.** Takes a DOI, an OpenAlex ID or an
  exact title and returns what the paper cites, what cites it (ordered by how
  much the field cited those in turn, so a five-year-old paper leads to the
  current state of the art), and any Crossref retraction, correction or
  expression of concern. A full walk costs four HTTP requests regardless of
  `limit`, because every neighbour's metadata is selected in the call that
  lists it.
- **`cache_search` returned "no cached pages match" for a cache full of
  matches.** `put_page` used `INSERT OR REPLACE`, which resolves the url
  conflict by deleting the old row and inserting a new one with a NEW rowid —
  and SQLite fires DELETE triggers on that path only when `recursive_triggers`
  is on, which it is not by default. So every re-fetch orphaned the old
  rowid's postings in `pages_fts`. External-content FTS5 reads column values
  back from `pages` by rowid, so the first orphan made every query raise
  `fts5: missing row N from content table`, which the malformed-query handler
  swallowed into an empty result. `put_page` now upserts (keeping the rowid),
  existing cache files are rebuilt once on open, and a desynced index is
  repaired on read instead of being reported as "nothing cached".
- **Rate-limited engines are now reported.** `aggregator` wrote
  `diagnostics["rate_limited"]` and nothing in the codebase ever read it, so a
  `gdelt` search skipped for its 6/min bucket looked identical to one that
  genuinely found nothing.

### Changed

- **`category=` now changes the ORDER of results, not just which engines run.**
  A specialist is usually the only source returning a given document, so its
  hit scored 1/61 under plain RRF while three general engines agreeing on a
  blog post about the topic scored 3/61 and won — `category="finance.filings"`
  put NVIDIA's actual 10-K fourth, behind commentary about it, and put A-share
  announcements seventh behind Wikipedia. Engines that natively index the
  requested category now count double in the fusion. Measured on 14 queries
  against real engine output, scoring the one result a knowledgeable person
  would call correct:

  | | hit@1 | hit@3 | MRR |
  |---|---|---|---|
  | before | 6/14 | 9/14 | 0.605 |
  | after | 8/14 | 13/14 | 0.747 |

  Six queries improved and none regressed. Two other candidate changes were
  measured on the same set and dropped for lack of evidence: retuning RRF's
  damping constant moved MRR by under 0.01 at any value between 5 and 60, and
  a lexical query/title overlap bonus made every configuration worse
  (0.747 -> 0.645).
- **`category="paper"` round-robins across its sub-groups before the engine
  limit truncates.** `SEARCH_MCP_CATEGORY_ENGINE_LIMIT` (default 3) used to
  hand all three slots to `arxiv`, `openalex` and `crossref` — and the last two
  are both DOI indexes covering the same corpus. The three slots now buy three
  different corpora, which also revives `pubmed`: it was the fourth engine in a
  three-slot list, i.e. dead code that four separate documents advertised.
- **`engines()` returns the source tree, derived from the registry.** The
  hand-maintained buckets it replaces had drifted — they promoted `pubmed`
  where it could never run and never mentioned `openverse` or `zenodo`. It
  takes an optional `group=` and honours `format="json"` like every other tool.
- **`Engine` gained a one-line `description`**, rendered by `engines()`, so
  picking a source no longer means guessing from its name.

### Fixed

- **`bing`'s redirect unwrapping (shipped in 0.9.2) no longer rewrites URLs it
  should leave alone.** It located the payload by searching the whole URL for
  `u=`, but `u` is an ordinary parameter name that other sites use for their own
  purposes, so a non-Bing link whose `u` value happened to base64-decode into
  something starting with `http` was silently replaced by it. Unwrapping is now
  gated on the URL actually being a `bing.com/ck/a` redirect, and the decoded
  target must be an absolute `http://` or `https://` URL rather than merely
  starting with those four characters.
- **`brave` returned every result twice.** Its comma-joined selector matches
  the same `div` through both branches and selectolax does not deduplicate, so
  two real results parsed as four. Half of `max_results` was spent on
  duplicates, and RRF scored each one twice, giving `brave` roughly double
  weight in the merge. `mojeek`, `bing`, `baidu` and `searx` had the same
  missing `seen` set with a smaller blast radius.
- **`read_doc` ignored the charset and mangled every non-UTF-8 page.** Five
  call sites hard-coded `decode("utf-8", errors="replace")` while the fetcher
  had already retrieved the content type. GBK, Big5 and Shift-JIS documents
  came back as replacement characters. They now go through the same
  header-then-`<meta>` decoder the rest of the package uses.
- **Crossref dates were wrong in two ways, and trusted anyway.** An internal
  `null` in `date-parts` (`[[2024, null, 5]]`) was filtered out rather than
  treated as a stop, promoting the day into the month slot; and a
  year-only record was padded to `YYYY-01-01` and marked confident, so
  `freshness="month"` dropped a paper actually published in December. Dates now
  truncate at the first gap, and year-only precision is no longer confident.
- **`sogou` and `so360` ignored `site:` / `-site:` / `filetype:`.** They were
  the only two web engines that never called `augment_query_with_operators`, so
  they returned unconstrained results that the post-filter then discarded
  wholesale — `engines=["so360"], include_domains=["python.org"]` reliably
  returned nothing.
- **Merging two copies of a URL could throw away the trustworthy date.** The
  representative was picked by snippet length, so a structured source with an
  exact publication date lost to an HTML scrape with a longer teaser, and the
  date was then dropped as empty. The merge is now field-wise: best date, and
  longest snippet, from whichever copy has it.
- **A cache hit erased the provenance of the search it replayed.** The first
  query reported "duckduckgo was gated, `searx` rescued it"; the same query
  inside the 7-day TTL reported an ordinary search. `gated_engines`,
  `gated_hint` and `rescued_via` now travel with the cached rows.
- **`google` could return a relative path as a result URL.** `_unwrap` only
  understood the `q=` wrapper; Google's other form puts the target in `url=`
  and leaves `q` empty, which `parse_qs` discards. The unwrapped value was also
  never made absolute, so `fetch` could not open it and host-based filtering
  silently dropped it.
- **`fetch(max_age_hours=…)` never hit the cache for Google News links.** The
  pre-check used the raw URL while the cache is keyed on the publisher URL the
  fetcher resolves it to, so every call re-fetched.
- **`sec_edgar` answered a ticker query with unrelated issuers' filings.**
  EDGAR's relevance ranking is dominated by structured-product pricing
  supplements, so "NVDA risk factors" led with four 497Ks from ProShares and
  Investment Managers Series Trust that merely mention the ticker. A query that
  writes a ticker as a ticker is now scoped with `entityName=`, which returns
  NVIDIA's own 10-K and 10-Qs. The match is case-sensitive — lower-cased, the
  ticker space is full of ordinary words (`IT`, `ALL`, `ON`, `NOW`, `GO`) — and
  an empty scoped search is re-run unscoped, so a wrong guess costs a round
  trip rather than the answer.
- **The search header credited engines that produced nothing.**
  `payload["engines"]` is the REQUEST — the list the cache key is built from —
  and when a requested engine fails, the rescue pass substitutes another. The
  header printed the request, so `search(engines=["serper"])` with no key
  configured announced `engines: serper` above ten results that every
  per-result byline correctly attributed to `bing`. The header now names the
  engines that actually contributed and lists the rest separately.
- **A dictionary entry passed as a scholarly source.** `_PAPER_HOSTS` lists
  publisher domains and the matcher accepts any subdomain, which is what keeps
  the list short — but the big presses also run dictionaries and bookshops on
  the same domain. `category="paper"` on "what is reciprocal rank fusion"
  dropped 54 of 56 raw results and kept Cambridge Dictionary's entry for the
  WORD "reciprocal" as its single source. Known non-scholarly siblings are now
  excluded before the allowlist is consulted.
- **`category="image"` and `"dataset"` could be rescued into the web pool.**
  `settings.rescue_engines` IS the general web pool, and the whole reason those
  two categories REPLACE that pool is that a web engine cannot return an image
  file or a dataset record. A sparse Openverse run therefore came back as HTML
  pages that `fetch(inline=True)` cannot render, under a header saying the
  search succeeded. Exclusive categories no longer rescue.
- **`max_results` was unvalidated.** `n = max_results or default` turned an
  explicit `0` into ten results with no sign anything had been ignored, and
  four-figure values were passed to every engine in the fan-out. It is now
  clamped to 1-50, with `None` still meaning "use the configured default".
- **`paper_graph` resolved a title to whatever OpenAlex ranked first.**
  "BERT pre-training of deep bidirectional transformers" returned BEiT's
  citation graph under the heading the caller typed — the actual BERT paper is
  not in OpenAlex's top five for that query at all, and a caller checking a
  citation has no way to catch the substitution. A title now has to clear a
  similarity gate that accepts a genuine prefix ("reciprocal rank fusion
  outperforms condorcet") and rejects a neighbour; when nothing clears it, the
  near misses are listed so the caller can retry precisely. DOI and OpenAlex-ID
  lookups are exact and ungated.
- **Blank arguments blamed the wrong thing.** `read_doc("")` answered "Local
  file reads are disabled; set SEARCH_MCP_DOCUMENT_ROOT", sending the caller to
  configure a sandbox they did not need; `cache_search("")` told them to
  populate a cache that may already be full; `fetch_batch([])` returned an
  empty string, which reads as "every fetch failed silently". Each now names
  the argument that was wrong.
- **IMF indicator matching preferred a code that is also a word.** Three of the
  132 codes are ordinary words — `GDP` is Capital Flows' *nominal* GDP — so
  "real gdp growth" matched the code shortcut and beat the indicator literally
  called "Real GDP growth". The shortcut now requires the code typed as a code,
  scoring is case-insensitive, and a label that merely contains the query no
  longer ties with one that matches it.
- **cninfo dated every announcement one day early.** Its epochs are exact
  midnights in Beijing time, so rendering them as UTC landed at 16:00 the
  previous day — visible in the payload itself, where the filing date in
  `adjunctUrl` disagreed with `announcementTime` on every hit.
- **`render_fetch` ran the body into the metadata line.** One newline where
  `render_doc` uses two, on every default-format `fetch` and `fetch_batch`.
- Smaller: `openalex` now sends `select=` (a 10-result page measured 174 KB
  against 56 KB), `arxiv` uses HTTPS, `pubmed` no longer computes its
  publication date twice, `zenodo` no longer recompiles a regex per call, and
  `research` passes through the diagnostics fields it had been dropping.

## [0.9.2] - 2026-08-06

### Fixed

- **Bing results are now publisher URLs instead of `bing.com/ck/a` tracking
  blobs.** `BingEngine.parse` returned the raw `href`, and on the www4 SERP
  essentially every organic href is wrapped in a click-tracking redirect
  (`/ck/a?…&u=a1<base64url>&ntb=1`). Half of a default four-engine run therefore
  came back as opaque `bing.com` links. This was not merely cosmetic: the blob
  carries a per-impression hash, so it is unique on every search, which defeats
  both the URL key the RRF merge fuses on and the `_dedup_by_title` pass behind
  it. A page found by Bing *and* by DuckDuckGo scored as two separate results
  and both were emitted — so a `max_results=10` run spent slots on duplicates of
  pages it had already returned (observed: "Defining schemas | Zod" at rank 1
  from DuckDuckGo and again at rank 8 as a Bing blob). The wrapper also left the
  caller unable to tell what a result even was without fetching it, which is the
  opposite of what a snippet-bearing search result is for. The `u` payload is
  now base64url-decoded to the real target; anything that is not a decodable
  wrapper passes through untouched, since a working redirect link still beats
  dropping the result. Present since the engine was added.

## [0.9.1] - 2026-07-31

### Fixed

- **`cache_search` no longer returns the cache's internal title sentinel.**
  Metadata is packed into the title column behind `\x01META\x01` so the schema
  could stay put, but the tool served those rows verbatim — the page title
  arrived as `\x01META\x01{"title": ...}\x01` in both markdown and json. Titles
  are now unpacked at the tool boundary, and the `author`, `date` and
  `sitename` already stored alongside them are returned instead of discarded.
  Rows written before metadata capture hold a plain title and are unaffected.
  Present since 0.4.2.

## [0.9.0] - 2026-07-31

Download by default, and finish every release on GitHub as well as PyPI.

### Changed

- **`download` now works with zero configuration.** Files go to
  `${SEARCH_MCP_CACHE_DIR}/downloads` (`~/.cache/search-mcp/downloads` locally
  and `/data/downloads` in Docker), still expire after 24 hours by default, and
  retain the existing filename, path-containment, response-size and SSRF guards.
  Set `SEARCH_MCP_DOWNLOAD_ENABLED=false` for an explicit opt-out, or
  `SEARCH_MCP_DOWNLOAD_DIR` to override only the destination.
- **Download policy is operator-owned instead of process-global client state.**
  The old elicitation answer applied to every caller sharing one HTTP process.
  Removing that session flag means one caller can no longer silently authorize
  downloads for another; disabled calls are rejected before any network request.
- **Release tags now produce a recoverable GitHub Release as well as PyPI
  artifacts.** The workflow validates tag, project, lockfile and changelog
  versions, builds once, stages the wheel and sdist on a draft Release, publishes
  those same files to PyPI, then exposes the GitHub Release as Latest.

### Fixed

- **A blank `SEARCH_MCP_DOWNLOAD_DIR` no longer means the current working
  directory.** Empty and whitespace-only values now select the dynamic default
  sandbox instead of being parsed as `Path(".")`.
- **The effective download limit is documented accurately.** Remote bodies are
  bounded by the smaller of `SEARCH_MCP_MAX_RESPONSE_BYTES` and
  `SEARCH_MCP_DOWNLOAD_MAX_MB`; the defaults therefore allow 25,000,000 bytes
  (about 23.8 MiB), not the full 100 MiB disk-layer cap.
- **`SEARCH_MCP_DOWNLOAD_TTL_HOURS=0` no longer claims files expire in `0h`.**
  The documented "keep forever" value now reports that TTL cleanup is disabled.
  Both that setting and `SEARCH_MCP_DOWNLOAD_MAX_MB` also reject negative
  values instead of accepting a configuration that refuses every file.
- **A missing or unreadable download directory no longer blocks startup.** TTL
  cleanup logs and skips the scan rather than raising out of server start, and
  it now runs before the network fetch as documented.
- **`~` in `SEARCH_MCP_CACHE_DIR` is expanded once, in settings.** The cache
  database and the derived download sandbox can no longer resolve to different
  roots.

## [0.8.0] - 2026-07-30

Search in your own language, and keep the SSRF guard switched on.

Two themes, both found by asking why a Chinese-language news search returned
nothing. The filters were discarding correct results, and the diagnostics that
should have said so were the one thing never shown.

### Fixed

- **`category="news"` no longer discards the non-English web.** The category
  filter matched results against a hand-written tuple of 33 Anglosphere
  outlets, so a Chinese news search dropped 17 of 17 hits — `news.sina.com.cn`
  included. The list now covers major outlets in Chinese, Japanese, Korean,
  French, German, Spanish, Portuguese, Russian, Hebrew and more, and also
  accepts the `news.<domain>` naming convention, because no hand-maintained
  tuple will ever hold every news site on earth.
- **Category-native engines are no longer filtered out by their own
  category.** `categories` is documented as a *routing* signal, but results
  were then re-checked against the hostname allowlist anyway. Crossref returned
  8 papers for `category="paper"` and all 8 were dropped for being on
  `doi.org`; OpenAlex kept 1 of 8; GDELT — which exists to index news in 100+
  languages — had every non-Western outlet discarded. An engine that natively
  indexes the requested category is now trusted for it. Domain, text, freshness
  and `category="pdf"` checks still apply.
- **Filter diagnostics are shown when there are no results at all.** The
  aggregator computed "filters dropped 17 of 17 raw results (kept 0), most by
  category=news" and the Markdown renderer returned before ever printing it.
  Callers saw only the silent-engine note and went hunting for an IP block that
  wasn't happening. `research` dropped the same diagnostics, plus `errors`, on
  the floor entirely.
- **Google News is asked in the query's language.** The edition was pinned to
  `hl=en-US&gl=US&ceid=US:en` while every other region-aware engine reads
  `SEARCH_MCP_REGION`. That endpoint is edition-scoped, so a Chinese query got
  an **empty** feed — 0 items where the Simplified Chinese edition had 35. The
  edition now follows `SEARCH_MCP_REGION`, and falls back to the query's
  writing system when the configured one cannot serve it: 23 scripts, from
  Cyrillic and Arabic to Tamil and Georgian. Measured gains include Thai
  12→100, Hebrew 31→100, Arabic 49→100, Bengali 7→100. Latin-script queries
  are deliberately left alone — the US edition already serves them at full
  volume, and script alone cannot tell German from English.
- **A rate-limited source is no longer reported as a silent IP block.** The
  keyless-JSON never-raise rule turned GDELT's HTTP 429 into an empty list, and
  the aggregator advised configuring a proxy for what was a documented
  6-requests-per-minute limit. Refusals are now reported with their status
  code, separately from genuinely silent engines.
- **Mojeek's CAPTCHA is detected.** It serves an ALTCHA proof-of-work wall that
  shares no markup with the Google or DuckDuckGo walls, so a captcha-blocked
  Mojeek — one of the four *default* engines — read as an unexplained empty.
  Google's JavaScript-redirect interstitial is likewise classified now instead
  of looking like "this query found nothing".

### Security

- **The SSRF guard was bypassable via the browser fallback.** `fetch_page`
  caught the guard's rejection as if it were a transport error and handed the
  same URL to the Chromium render, which ran no check at all — so any blocked
  target was reachable by being unreachable over plain HTTP first. On a cloud
  instance `http://169.254.169.254/` returned instance credentials that way.
  A refusal is no longer a failure to fall back from, and the browser path is
  independently guarded (`render="browser"` skips the HTTP branch entirely).
- **A cache hit bypassed the guard.** The page cache was read before any check,
  so anything fetched while the guard was permissive stayed retrievable
  afterwards and tightening the setting had no effect on it. Cache reads now
  run the DNS-free layers first.
- **Alibaba Cloud's metadata endpoint (`100.100.100.200`) is blocked.** It sits
  in CGNAT space, which `ipaddress` reports as ordinary public address space.
- **Internal hostnames are refused by name** — `localhost`, `*.internal`,
  `*.local`, `*.corp`, `*.lan`, `metadata.google.internal`, `instance-data` and
  friends — with no DNS lookup, so the check holds on setups where resolution
  says nothing useful.

### Changed

- **The SSRF guard's resolve-every-address layer now runs only when this
  process's resolver decides what gets connected to.** It is skipped behind an
  outbound proxy (the proxy resolves and connects) and on a fake-IP VPN in TUN
  mode, where every hostname is mapped into a range like `198.18.0.0/15` and
  the answer is a handle, not a destination. Both setups previously refused
  *every* fetch, whose real-world outcome is not "one request blocked" but
  "operator sets `allow_private_hosts=true`" and gives up loopback and metadata
  protection too. The DNS-free layers run in every mode. Fake-IP detection uses
  canary hostnames that are public by definition and can only ever stand down
  for tunnel ranges — never loopback, link-local or RFC1918.
  New `SEARCH_MCP_SSRF_RESOLVE_ADDRESSES` = `auto` (default) | `always` |
  `never`.
- **Google is asked as Chrome, Bing as Edge.** Engines can now declare their
  own TLS/header fingerprint. Measured caveat: Google answered its JS
  interstitial under every profile tried, so its gate is behavioural rather
  than a fingerprint check — this is about presenting a coherent identity, not
  about unblocking it.
- The offline test suite is hermetic with respect to *configuration*, not just
  DNS. A personal `SEARCH_MCP_ALLOW_PRIVATE_HOSTS=true` disarmed the guard
  under test and failed 26 SSRF cases on that machine while passing everywhere
  else; a suite whose result depends on who runs it cannot review a change to
  the thing it covers.

## [0.7.0] - 2026-07-30

Search and fetch anything, not just web pages: images and binaries are
described (and optionally shown to vision models), six more document formats
parse, and files can be downloaded — with permission, and not forever.

### Added

- **`fetch` handles non-text resources.** Images, video, audio, fonts and
  opaque binaries return a description — media type, byte size, image
  dimensions, sha256 — instead of being decoded as text into a screen of
  U+FFFD. `inline=True` returns the image itself as MCP `ImageContent` for a
  vision-capable model. Bytes are withheld by default because a 1MB image
  costs well over a thousand tokens, and the description usually settles
  whether it's worth spending them.
- **`download` tool** — saves a file to disk. **Off by default.** With no
  `SEARCH_MCP_DOWNLOAD_DIR` configured it asks for permission first (MCP
  elicitation, which the SDK carries across both protocol eras) and the answer
  applies to that session only. Declining writes nothing. A client that can't
  be prompted gets an actionable error rather than a silent refusal.
  Downloads are **ephemeral**: anything older than
  `SEARCH_MCP_DOWNLOAD_TTL_HOURS` (default 24) is deleted before the next
  download and at startup.
- **`read_doc` formats**: xlsx, pptx, epub, csv/tsv, source code and config
  files, zip/tar archives — see 0.6.0.
- **`image` and `dataset` categories**, served by `openverse` (CC-licensed
  images, direct file URLs that work with `fetch(inline=True)`) and `zenodo`.
  Unlike the other categories these **replace** the default web pool rather
  than augmenting it: a web engine cannot return an image file or a dataset
  record, so mixing it in only crowds out the source that can.

### Fixed

- **JSON engines now ask for JSON.** The shared curl_cffi session impersonates
  Chrome, so it advertised `Accept: text/html`; Openverse (Django REST
  Framework) answered 200 with its browsable **HTML** API, which failed to
  parse and looked exactly like "no results". Openverse additionally requests
  `format=json` in the query string, because header negotiation does not
  survive the impersonation reliably.
- **Zenodo is no longer blocked.** It answers 403 to clients presenting a
  browser TLS/header fingerprint on its API — the opposite of what every
  scraper needs. Engines can now opt out of impersonation and send an honest,
  contactable User-Agent instead.
- **Downloads no longer block the event loop.** Writing up to
  `SEARCH_MCP_DOWNLOAD_MAX_MB` and sweeping the directory both run on a worker
  thread; a large save would otherwise stall every in-flight request.

### Changed

- `fetch` opts out of structured output. It can return page text, a JSON
  payload, or an actual image, and no single JSON Schema covers an
  `ImageContent` block — while from 2026-07-28 the SDK *validates* returns
  against the derived schema, so deriving one would reject every inline image
  at call time.
- `readOnlyHint` is now accurate per tool rather than blanket-true: `download`
  is the one tool that writes, and clients use that hint to decide whether a
  call needs confirmation.

### Security

- Downloaded filenames are treated as untrusted input: collapsed to a single
  path component, filtered to a conservative character set, length-capped,
  content-hash-prefixed so two resources claiming the same name cannot
  overwrite each other, and the final path is re-checked against the download
  root before writing.
- Archives are listed, never extracted, and a listing that expands more than
  100x is flagged.

## [0.6.0] - 2026-07-30

Thirteen new keyless sources, and `category=` now routes to the ones that
actually index the category instead of filtering general web results by
hostname. Plus six new document formats for `read_doc`.

### Added

- **Category routing.** Passing `category=` without `engines=` pulls in the
  sources that natively index it, capped by
  `SEARCH_MCP_CATEGORY_ENGINE_LIMIT` (default 3). `category="paper"` now
  queries arXiv/OpenAlex/Crossref; previously it ran the same four general web
  engines and discarded every result whose hostname wasn't on a whitelist.
  Engines declare what they cover via `Engine.categories`, which replaces the
  hardcoded "if news, add googlenews" branch.
- **Academic sources** (`paper`): `arxiv`, `openalex`, `crossref`, `pubmed`.
  All keyless, all with structured publication dates, so freshness filtering
  can drop stale results instead of guessing from snippet text.
- **Code and developer discussion**: `github` (repositories + issues/PRs,
  keyless), `stackexchange`, `hackernews`, and `github_code` (keyed — GitHub
  returns 401 to anonymous code search).
- **Reference and news**: `wikipedia` (language follows `SEARCH_MCP_REGION`),
  `openlibrary`, `gdelt` (worldwide news in 100+ languages).
- **Chinese indexes**: `sogou`, `so360`.
- **`read_doc` formats**: xlsx (one Markdown table per sheet), pptx (per slide,
  including speaker notes), epub, csv/tsv, source code and config files (fenced
  with their language), and zip/tar archives.
- **Per-engine rate limits.** An engine can declare `rate_limit_per_minute` and
  `rate_limit_max_wait`; when its bucket is empty the aggregator **skips** it
  and records the fact, rather than making a parallel search wait. GDELT
  publishes a one-request-per-few-seconds rule, and queueing on it would have
  added that delay to every other engine's results.
- `SEARCH_MCP_CONTACT_EMAIL` — optional, routes OpenAlex/Crossref/NCBI calls
  into their faster identified-caller pools.

### Fixed

- **Archives are listed, never extracted**, and a listing that expands more
  than 100x is flagged as such — decompressing untrusted archive members is
  how zip bombs win.
- **`_detect_format` matches the URL path, not the raw URL.** A query string
  routinely ends in something extension-shaped
  (`.../data.csv?token=abc.png`), which classified the document by the wrong
  one.
- **A keyed engine's "missing key" error is no longer swallowed.** The new
  `EngineKeyError` escapes the keyless never-raise boundary, so an
  unconfigured engine says so instead of reporting "no results" —
  indistinguishable from "nothing matched".
- **Unconfigured keyed engines stay out of category routing.** `github_code`
  without a token used to be auto-added to every `category="github"` search,
  guaranteeing an error alongside the results. Naming it explicitly still
  raises, which is the point.
- CSV/table cells escape `|` so a cell cannot forge extra columns, and code
  fences widen past any backtick run in the file so a Markdown fence inside a
  source file cannot break out.

### Notes

- New sources are **not** in the default pool. Ordinary web searches pay
  nothing for them; they arrive via `category=` or an explicit `engines=`.
- `sogou` returns Sogou redirect URLs (`/link?url=...`) rather than target
  URLs — the blob is only resolvable by following it. `fetch` handles that
  fine, but host-based `category` filtering will discard them. `baidu` and
  `so360` return direct URLs.

## [0.5.0] - 2026-07-30

Migrates to MCP protocol revision **2026-07-28** on SDK v2, and adds an
optional HTTP transport. Existing stdio clients need no changes: one server
instance serves both protocol eras, and the tool surface is byte-identical.

### Added

- **`streamable-http` transport.** `search-mcp --transport streamable-http
  [--host --port --path]`, or the matching `SEARCH_MCP_TRANSPORT` /
  `SEARCH_MCP_HTTP_*` env vars; CLI beats env beats default. `stdio` remains
  the default, so nothing changes unless you ask for it. The 2026-07-28
  revision removed protocol-level sessions and `Mcp-Session-Id`, so the HTTP
  endpoint is stateless and needs no session affinity across replicas.
- **DNS-rebinding protection on the HTTP transport**, always on. The SDK
  leaves this *off* when no settings are supplied, which would let any web
  page a user visits drive the server through their browser; the allowed
  `Host`/`Origin` set is now built explicitly from the bind address, plus
  anything in `SEARCH_MCP_HTTP_ALLOWED_ORIGINS`.
- **Cache hints on list results** (SEP-2549). `tools/list`, `prompts/list`,
  `resources/list` and `resources/templates/list` advertise
  `ttlMs=3600000, cacheScope=public` — all four are fixed for the life of the
  process, so clients can stop re-listing. `resources/read` is 60s/private.
- **Server identity.** `serverInfo` now carries a title, version, project URL,
  and instructions describing when to reach for which tool; previously the
  server reported a bare name and an empty version string.
- Tests covering both protocol eras end-to-end, tool/prompt/resource wire
  shapes, transport selection, and the origin guard.

### Changed

- **Requires `mcp>=2.0.0`.** v2 is the first release that speaks 2026-07-28,
  and it renamed `FastMCP` to `MCPServer` with no compatibility alias, so
  there is no version that satisfies both. Pulls in `httpx2` and `mcp-types`
  transitively; `httpx2` imports as `httpx2` and does not collide with the
  `httpx` this project already uses.
- **Tool titles moved to the real `Tool.title` field.** They previously lived
  only in `ToolAnnotations.title`, which the spec defines as an untrusted
  display hint rather than the tool's name.
- Synchronous tool and prompt handlers now run on worker threads (SDK v2
  behavior). `engines()` and the four prompts are unaffected — none touch the
  event loop or thread-local state.
- `_safe_progress` no longer enumerates SDK exception types. A dropped
  progress ping is logged at debug and never fails the call it belongs to.

### Fixed

- **A missing cached resource now reports `-32602` (invalid params) instead of
  `-32603` (internal error).** `cache://page/...` and `cache://search/...`
  raised a bare `ValueError` on a miss, which the SDK could only classify as
  "the server broke" — telling clients to retry something that will never
  succeed. They raise `ResourceNotFoundError` now, matching the code the
  2026-07-28 revision assigns to resource-not-found.

### Notes

- Roots, Sampling and protocol-level Logging are deprecated as of 2026-07-28.
  This server never used any of them; its stderr logging is already the
  recommended replacement.

## [0.4.3] - 2026-07-30

Single-line dependency hotfix. No code changes.

### Fixed

- **Fresh installs were broken.** The `mcp[cli]>=1.2.0` pin had no upper bound,
  so any new resolve (`uvx free-search-mcp`, `pip install free-search-mcp`)
  picked up MCP Python SDK 2.0.0, released 2026-07-28. That release deletes
  `mcp.server.fastmcp` entirely — `FastMCP` is gone with no compatibility
  alias — and `server.py` imports `FastMCP` from exactly there, so the server
  died at import with `ModuleNotFoundError`. Pinned to `>=1.2.0,<2` to restore
  installability. The cap comes off in 0.5.0, which migrates to the v2
  `MCPServer` API and the 2026-07-28 protocol revision.

## [0.4.2] - 2026-07-26

Hardening + hygiene pass: packaging correctness, stricter lint, lifecycle
cleanup, and de-duplicated engine plumbing. No tool-facing behavior changes
except a documented cap on `fetch_batch`.

### Fixed

- **sdist no longer sweeps the whole working tree.** An explicit
  `[tool.hatch.build.targets.sdist]` include list (plus a `.gitignore` entry
  for the local `freesearch-promo/` video project) shrinks the sdist from
  ~209 MB — which PyPI would reject — to ~300 KB.
- `__version__` now reads the installed distribution's version via
  `importlib.metadata` instead of a hardcoded string that had gone stale at
  `0.2.0`.
- `starlette` and `uvicorn` are declared as direct dependencies (admin UI
  imports them directly; previously they arrived only transitively via
  `mcp[cli]`). Dropped the never-imported `playwright-stealth`.
- `run()` now closes the SQLite cache on shutdown (clean WAL checkpoint);
  `cache.close()` no longer swallows the caller's own cancellation; a failing
  `playwright.stop()` can no longer abort browser-pool shutdown midway.
- Rescue-path closures bind their loop variables explicitly (`B023`) — safe
  today, but one refactor away from every candidate using the last engine's
  binding.

### Changed

- **Ruff ruleset tightened** (`E,F,W,I,B,UP,ASYNC,SIM,C4,RET`, line length
  100) and the whole tree brought clean under it; CI now enforces it.
- **Engine tail contract unified.** The post-filter + diagnostics + rank
  stamping every keyed engine mirrored by hand (~22 lines × 6 engines) moved
  into `Engine.finalize_results`. The `chrome131` impersonation constant is
  now imported from `httpfetch.IMPERSONATE` everywhere instead of being
  redeclared in 8 modules.
- `fetch_batch` documents and enforces a 20-URL cap, and `fetch_many` bounds
  live coroutines with a semaphore (8) — the rate limiter throttled requests
  per minute but not simultaneous sockets.
- SQLite cache uses `synchronous=NORMAL` + `temp_store=MEMORY` under WAL —
  drops one fsync per cached write; an abrupt exit can lose the last few
  cache writes but never corrupts the file.

### Added

- **Golden-HTML `parse()` tests for `duckduckgo`, `mojeek`, `bing`** — three
  of the four default engines previously had no markup-drift coverage, which
  is this project's most likely silent failure mode.
- Short-TTL (30 s) memo of successful SSRF DNS validations — a `research()`
  call reading N pages from one host no longer pays a DNS round trip per page
  and per redirect hop.
- `_dedup_by_title` precomputes per-item host/digit keys instead of re-parsing
  every kept URL for every candidate.
- `.env.example` / README now document `SEARCH_MCP_SEARX_INSTANCES`,
  `SEARCH_MCP_USER_AGENT`, and `SEARCH_MCP_ADMIN_NO_BROWSER`.

## [0.4.1] - 2026-07-08

First release actually on PyPI.

### Changed

- **Distribution renamed to `free-search-mcp`** — the name `search-mcp` turned
  out to be taken on PyPI. The import package (`search_mcp`), all three
  console scripts, and every env var (`SEARCH_MCP_*`) are unchanged; a new
  `free-search-mcp` console alias makes the bare `uvx free-search-mcp` start
  the server without `--from`.
- Release workflow publishes with a repo-secret API token (`PYPI_API_TOKEN`)
  instead of Trusted Publishing — pushing a `v*` tag is the entire release
  process.

## [0.4.0] - 2026-07-08

Keyless-search reliability + one-command deploy. Focus: no result should be
silently lost, and `uvx free-search-mcp` should give any agent working search with
zero setup.

### Added

**Keyless search reliability:**
- **Aggregation-level rescue.** When a fresh search comes back empty — or
  nearly empty with demonstrably unhealthy engines (errors, CAPTCHA gates, or
  a silent zero) — the aggregator runs one bounded recovery pass via
  `SEARCH_MCP_RESCUE_ENGINES` (default `searx` → `bing`, capped at
  `SEARCH_MCP_RESCUE_TIMEOUT`, default 10s). Rescue results merge via RRF
  with honest attribution and surface as `rescued_via`. A healthy sparse
  result never triggers it, so the normal path pays nothing. This uniformly
  protects a gated DuckDuckGo (previously unprotected) and replaces the
  per-engine searx fallbacks inside `google`/`bing`.
- **Bounded retry on the keyless HTTP path.** One retry for connection errors
  (0.4–0.8s jittered) and 429/5xx (honoring `Retry-After` up to 3s); timeouts
  are never retried. Happy path unchanged.
- **Silent-block visibility.** An engine returning 0 results with no error
  and no detected gate (the Mojeek-IP-block failure mode) now surfaces as
  `empty_engines` + an actionable hint when results are sparse.
- **`bing` joins the default pool** (`duckduckgo`, `mojeek`, `googlenews`,
  `bing`) — its www4 edge answers over plain HTTP in ~0.3s, and with the
  per-engine searx race gone a gated bing costs one fast attempt.
- **Cache eviction.** Expired rows are now actually deleted, and the cache
  file is capped at `SEARCH_MCP_CACHE_MAX_MB` (default 512, 0 disables) by
  dropping the oldest pages + one VACUUM — opportunistic (init + every 200
  writes), no background tasks.

**Deploy:**
- **PyPI packaging + `uvx free-search-mcp`.** Full metadata (urls, classifiers,
  PEP 639 license), verified end-to-end: `claude mcp add search -- uvx
  search-mcp` gives an agent working keyless search with zero config.
- **GitHub Actions.** `ci.yml` (ruff + offline pytest on Python
  3.11/3.12/3.13) and `release.yml` (tag-triggered build + PyPI publish via
  Trusted Publishing/OIDC — no token stored in the repo).
- **Config-dir `.env`.** Settings also load from `~/.config/search-mcp/.env`
  so uvx installs launched from any directory stay configurable. Precedence:
  real env > `./.env` > config-dir `.env`.
- **Admin UI opens the browser automatically** (`SEARCH_MCP_ADMIN_NO_BROWSER=1`
  to suppress).

### Fixed

- **Gate diagnostics finally render.** `gated_engines`/`gated_hint` were
  attached to the payload but never rendered in markdown mode (the default),
  so callers never saw WHY an engine returned nothing. Gate/silent/rescue
  hints now render in both the results and no-results branches.
- **Missing Chromium degrades gracefully.** A never-downloaded browser now
  raises one actionable error carrying the exact install command
  (`uvx --from free-search-mcp playwright install chromium`), memoized instead of
  re-starting the Playwright driver per attempt; engines record an honest
  `browser_unavailable` gate instead of stack traces. HTTP-only search and
  fetching keep working without Chromium.
- **SSRF DNS lookups no longer block the event loop.** The guard resolves via
  `loop.getaddrinfo` on all async paths (initial URL + every redirect hop).
- **`install.sh` installs Chromium's OS deps on Linux** (`--with-deps`, with
  a plain-install fallback) — browser-rendered engines no longer crash on a
  clean Linux host.

### Changed

- The triplicated redirect-following GET loop (fetcher/documents/structured)
  is consolidated into `httpfetch.py` — one SSRF-checked, size-capped loop
  with curl_cffi and httpx flavors.
- Dependencies: dropped unused `tenacity`; declared direct `w3lib`.
- README hero GIF is now committed and referenced by absolute URL (renders on
  PyPI and fresh clones).

### Known debt

- `documents._source_chars_consumed` mirrors `formatting.smart_truncate`
  boundary logic (fragile coupling; behavior-neutral refactor pending).
- Page metadata rides in the cache `title` column behind a sentinel rather
  than a schema change (deliberate, back-compat).

## [0.3.0] - 2026-06-16

No-API-key usability audit + fixes. Focus: the default keyless path
(duckduckgo + mojeek + googlenews) and the opt-in keyless engines.

### Fixed

**Result quality (default keyless path):**
- **GoogleNews URLs are now readable.** `news.google.com/.../articles/CBM…` links
  resolved to an empty JS shell over both HTTP and a headless browser, so
  `fetch`/`research` returned zero content for every news result. They are now
  decoded to the real publisher URL via Google's `batchexecute` RPC (memoised,
  best-effort). News `research` went from empty shells to full publisher text.
- **DuckDuckGo no longer double-counts every result.** The `div.result,
  div.web-result` selector matched each organic row twice (rows carry both
  classes), doubling DDG's weight in the RRF merge and skewing ranking. Now
  selects `div.result` once with a URL-dedup guard.
- **Title-dedup keeps version/year/quantity variants.** "Python 3.13 released" vs
  "3.12", "best … 2026" vs "2025" scored ≥92 and the second was silently
  dropped; a digit-token guard now keeps them as distinct results.
- **Lead snippet** attributes GoogleNews items to the real outlet (from the
  "(Reuters)" suffix) instead of "news.google.com".

**Fetch / document path:**
- **PDF/DOCX URLs are parsed, not garbled.** `fetch`/`research` on a binary
  document URL returned `U+FFFD` garbage; they now route to the document parser.
- **Charset is honored.** Non-UTF-8 pages (GBK/Big5/Shift-JIS — common for
  baidu/zhihu hits) were decoded as UTF-8 (mojibake); now decoded per the
  Content-Type/`<meta>` charset.
- **read_doc degrades to HTML** when a `.pdf`/`.docx` URL actually serves an
  HTML login wall / soft-404 instead of crashing.
- Short non-HTML responses no longer trigger a needless browser render that
  mislabels them `text/html`.

**Engines:**
- **Bing is HTTP-first** (~0.3s) instead of always browser-rendered (~15s), with
  the browser kept only as a gate fallback.
- **SearXNG instance list refreshed** (the old five were all dead), now
  operator-overridable via `SEARCH_MCP_SEARX_INSTANCES`. This also re-arms the
  google/bing keyless fallbacks that depend on it.
- **Baidu** returns the real destination (`mu` attribute) instead of the opaque
  `baidu.com/link?url=` redirector; **AnySearch** snippets are capped instead of
  dumping the full page body; **Bilibili** upgrades `http://` watch URLs to https.

**Honesty / diagnostics:**
- A gated DuckDuckGo anomaly/CAPTCHA page (HTTP 202) is now detected, so the #1
  default engine reports an honest hint instead of a silent empty.
- SearXNG records a `no_live_instance` gate reason when every instance is dead.
- Tool docstrings corrected: `engines()` no longer lists a stale 7-engine subset;
  `category="github"` documents all forges; `freshness` is described as
  best-effort; `category="news"` documents its whitelist; a keyless-recovery hint
  was added. Token estimator widened to cover CJK punctuation/Extension-A.

**Lower-priority robustness:**
- **Bad/expired API keys raise an actionable error** (HTTP 401/403/422/429)
  instead of a silent empty, for brave_api/serper/tavily/google_cse. Transient
  5xx still degrades to empty.
- **Freshness no longer over-drops.** An absolute date scraped from snippet text
  ("…founded in 2009…") is treated as display-only; only relative "N ago" phrases
  and structured RSS/API dates are trusted to drop a result under a freshness
  filter.
- `read_doc`/`extract_structured` raise a clean `UnsafeURLError` for an invalid
  port instead of leaking a bare `ValueError`.
- `httpx[socks]` is now a dependency, so the documented `socks5://` proxy works
  for `extract_structured`/`read_doc`; `extract_structured` also honors page
  charset. SSRF-guard docstring corrected to describe the real per-hop check.
- `use_cache` docstring now notes the cache key includes all active filters.

## [0.2.0] - 2026-06-01

A large feature release: 9 new search engines (keyless + keyed), a local admin
backend for configuration, and a full set of fixes for provider-gated engines
(proxy, SearXNG fallback, gate diagnostics, Zhihu login).

### Added

**Keyless engines** (no API key, opt-in via `engines=[...]`):
- `google` — Google web SERP scraper (HTTP + browser fallback).
- `serpsearch` — alias of `google` (keyless Google SERP).
- `anysearch` — AnySearch unified-search REST API (anonymous tier; optional key
  lifts limits).
- `bilibili` — Bilibili (哔哩哔哩) video search via the public JSON API.
- `zhihu` — Zhihu (知乎) search, browser-rendered (best-effort; see login below).

**Keyed engines** (dormant until a key is configured):
- `brave_api` (Brave Search API), `serper` (Serper/Google), `tavily` (Tavily AI
  search), `google_cse` (Google Custom Search — needs API key + cx).

**Admin backend & configuration:**
- `search-mcp-admin` — a localhost-only web UI to enter API keys and a proxy,
  with a per-provider "how to get a key" guide, masked inputs, live Save (no
  restart), Test, and Clear. Secrets are stored at `~/.config/search-mcp/config.json`
  (`0600`) and never echoed back to the page.
- `keystore` module: hot-reloaded JSON config with `SEARCH_MCP_*` env override.
- `.env` keys are loaded at server/admin startup.

**Gated-engine fixes (proxy · fallback · diagnostics · login):**
- Optional **proxy** support (`SEARCH_MCP_PROXY` / admin "Network / Proxy" card)
  applied to HTTP engines, the browser pool, and remote fetch/document/structured
  calls. Scope with `SEARCH_MCP_PROXY_ENGINES`. (`http`/`https`/`socks5`.)
- **SearXNG auto-fallback**: `google`/`serpsearch`/`bing` transparently recover
  via the working `searx` meta-search when CAPTCHA-gated; results attributed to
  `searx`.
- **Gate diagnostics**: responses include `gated_engines` + `gated_hint`
  (`captcha`/`consent`/`login`).
- `search-mcp-login` — one-time interactive Zhihu login; cookies persist so
  later headless searches work.
- Transient browser navigation errors are retried once.

**Deploy & docs:**
- One-click `scripts/install.sh`, `Dockerfile` + `docker-compose.yml`,
  project-scoped `.mcp.json`, annotated `.env.example`.
- New docs: `docs/USAGE.md`, `docs/API_KEYS.md`, `docs/PROXY_AND_GATES.md`.

### Fixed
- `anysearch` response mapping (results are nested under `data.results`).

### Notes
- Keyless defaults (`duckduckgo`, `mojeek`, `googlenews`) are unchanged; all new
  engines are opt-in to preserve the fast default-pool latency.
- We deliberately do not attempt to defeat provider CAPTCHAs (ToS); the proxy and
  fallback are the supported ways around datacenter-IP gating.

## [0.1.0]

- Initial release: multi-engine keyless search, smart fetch (httpx → Playwright),
  document reading, FTS5 cache, filters, and LLM-tuned Markdown output.

[0.2.0]: https://github.com/sweetcornna/free-search-mcp/releases/tag/v0.2.0
