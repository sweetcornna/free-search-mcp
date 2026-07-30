# Changelog

All notable changes to this project are documented here. The format is loosely
based on [Keep a Changelog](https://keepachangelog.com/), and the project follows
semantic versioning.

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
