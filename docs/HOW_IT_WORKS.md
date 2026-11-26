# How `free-search-mcp` 1.0 Works

`free-search-mcp` 1.0 is a Rust workspace compiled into a single binary
(`free-search-mcp`, alias `search-mcp`). This document explains the three-tool
MCP surface (`search`, `exec`, `ask`), the QuickJS-NG sandbox, the three-layer
source model + agent lane, search ranking & hedging, the academic subsystem,
networking/SSRF, document isolation, and SQLite storage.

---

## 1. Three-Tool MCP Surface (`fsm-surface` + `fsm-server`)

Instead of exposing a dozen single-purpose MCP tools (~35 KB of schemas), 1.0
always exposes exactly **3 tools** (`search`, `exec`, and `ask`), **0 prompts**,
**0 resource templates**, and **0 completions** via `rmcp 3.11`:

- `tools/list` compact JSON: **≤ 6,800 B** (three tools).
- `ServerInfo.instructions`: **≤ 450 B**.
- Serves all protocol eras (`2024-11-05` through `2026-07-28`) from one
  process, stamping `_meta["io.modelcontextprotocol/serverInfo"]` and cache
  hints (`ttlMs = 3600000`, `cacheScope = "public"`) on modern eras.
- Tools do not emit `outputSchema` or `structuredContent`.
- `ask({ question, freshness?, lang?, domains?, mode?, max_tokens? })` provides
  the one-call quick path: `"fast"` (no LLM: direct-answer sources first, else
  web search + parallel page read in ~8 s returning dated passages), `"agent"`
  (answer agent backends `api` / `claude-code` / `codex`), and `"auto"` (uses
  `"agent"` when configured, else `"fast"`). Also exposed as `web.ask` inside
  `exec`, CLI `free-search-mcp ask`, and HTTP `POST/GET /v1/ask` behind the
  same token/host guards.

---

## 2. Sandboxed JS Runtime (`fsm-script`)

`exec({ code, max_tokens?, timeout_s? })` runs each call on a fresh OS thread
with a fresh QuickJS-NG (`rquickjs 0.14`) ES2020 `AsyncRuntime`:

- **Zero ambient capability**: No filesystem, environment variables, process
  spawning, dynamic `import()`, or raw sockets. Credentials stay strictly
  host-side.
- **TypeScript tolerance**: Accepts async arrows, statement blocks with
  `return`/`await`, or bare expressions; on `SyntaxError`, retries once after
  `swc_ts_fast_strip` (~7.5 µs, preserving line numbers).
- **Resource bounds**:
  - Wall-clock: default `30 s`, ceiling `120 s` (QuickJS interrupt handler +
    Tokio `CancellationToken`; emits `notifications/progress` every `10 s`).
  - Heap / stack: `96 MiB` / `1 MiB` (max `128 MiB` / `2 MiB`, `rust-alloc` off).
  - Host calls / concurrency: `100` calls/exec (max `300`), `12` concurrent
    host calls/exec, `4` global concurrent `exec` runs.
  - Output budget: default `6,000` tokens (CJK-aware: `ceil(ascii/4) +
    ceil(non_ascii/1.5)`). Structural water-filling JSON shrinker keeps valid
    JSON; overflow (up to 2 MiB raw JSON) is saved to SQLite `exec_kv` under an
    8-char random handle `exec:<id>` retrievable via `cache.get("exec:<id>")`.
- **4 Markdown Boundary Guardrails**:
  1. Host return values carry a prelude-private non-enumerable closure `Symbol`
     tag (`RenderKind`), unforgeable by user scripts.
  2. Own enumerable keys must be a subset of the Rust serde camelCase field
     whitelist; adding custom properties falls back to compact JSON so no data
     is silently dropped.
  3. Arrays render as Markdown only when every element passes tag validation.
  4. Overflow always stores raw structured JSON (never rendered Markdown).
- **Anti-spoofing delimiter nonce**: Every `exec` generates a 4-char random
  base36 nonce (`[k7m9]`) embedded in console/truncation/overflow delimiters,
  escaping any matching pattern inside untrusted fetched text.
- **Unified Local-Doc Egress Taint Rule**: Every `HostFn` declares `egress: bool`.
  Once an `exec` reads a local file under `SEARCH_MCP_DOCUMENT_ROOT` via
  `doc.read` / `doc.outline`, `HostServices` centrally refuses all subsequent
  `egress = true` calls (`web.search`, `web.request`, `source.call`,
  `source.protocol`, `scholar.*`, `web.download`, etc.) for the rest of that
  `exec` (permitting only `GET` `web.fetch`/`fetchBatch` to hosts already
  fetched before taint).

---

## 3. Three-Layer Source Architecture (500+ Sources)

1. **42 Native Rust Engines** (3 isolated crates):
   - `fsm-engines-serp` (13): `duckduckgo`, `bing`, `yahoojp`, `yahoo`,
     `seznam`, `so360`, `quark`, `naver`, `daum` (new), `baidu`, `googlenews`,
     `youcom`, `bilibili`.
   - `fsm-engines-api` (15): `github`, `github_code`, `github_releases`,
     `registries`, `sec_edgar`, `yahoofinance`, `cninfo`, `worldclock`,
     `openmeteo`, `rdap`, `wikidata`, `brave_api`, `serper`, `serpbase`,
     `tavily`.
   - `fsm-engines-claims` (14 direct-fact claimants): `imf`, `holidays`,
     `frankfurter`, `cfets`, `coingecko`, `wdi`, `gleif`, `endoflife`,
     `cisakev`, `osv`, `appstore`, `pypi`, `npm`, `crates`.
2. **18 Generic Protocol Adapters × Curated Instances** (`fsm-protocols-a` +
   `fsm-protocols-b` + `crates/manifests/instances/`):
   - Academic/Repository (9): `oai_pmh`, `dspace7`, `inveniordm`, `dataverse`,
     `eprints`, `sru`, `mediawiki`, plus repository feeds/adapters.
   - Data/Web (9): `ckan`, `socrata`, `opendatasoft`, `sparql`, `opensearch`,
     `discourse`, `wordpress`, `rss_atom`, `activitypub_fediverse`,
     `shopify_json`, `sitemap`.
   - Callable both via curated instance IDs (`source.call`) and ad-hoc URLs
     (`source.protocol(kind, baseUrl, op, params)`).
3. **Declarative TOML + `jaq` API Manifests** (`fsm-manifests` + `fsm-sources`):
   - Validated at build time (strict schema + `jaq 3.1` filter syntax) and
     embedded via `postcard`; runtime compiles `jaq` filters lazily on first
     use (`< 1 ms` cold start impact).
   - Supports static keys, OAuth2 `client_credentials` (in-memory cache only),
     per-op auth overrides, single pre-flight steps, JSON/XML/HTML/CSV/NDJSON
     extraction, GraphQL/SPARQL bodies, and mirror `fallback_urls`.
4. **Catalog Search (`fsm-catalog`)**: Okapi BM25 (`bm25 2.3`, EN stem + CJK
   bigrams + EN/ZH synonyms + ID prefix boost) with `rapidfuzz` fallback
   (`< 0.2 ms`, zero network).

---

## 4. Search Kernel (`fsm-search`)

- **Pools**: Default `["duckduckgo", "bing", "yahoojp", "seznam"]`; reserves
  `["yahoo", "youcom", "mwmbl"]`; locale (`zh` → `so360`/`quark`/`baidu`,
  `ko` → `naver`/`daum`, `ja` elevates `yahoojp`); `freshness=day|week` adds
  `googlenews`.
- **Direct-Fact Claimants**: `Source::claims()` runs on both `scope="web"` and
  `scope="code"` (`×5.0` RRF weight).
- **Quorum, Grace & Hedging**: Per-engine timeout `5 s` (hard search ceiling
  `8 s`). Once `≥ 2` healthy engines return and deduped hits `≥ limit`, a
  `400 ms` grace window closes and cancels stragglers without health penalty.
  If `< 2` engines answer by `1.8 s` (or primaries fail/wall), reserves hedge
  at `1.8 s` and `+800 ms` (`max_hedges = 2`).
- **Weighted RRF, Dedup & CJK Coherence**: `sum(w / (60.0 + rank))` (`1.0`
  base, `2.0` native scope, `5.0` claimant), canonical host dedup (unwrapping
  Yahoo `r.*` and `rd.*` redirects) + `rapidfuzz::token_set_ratio >= 92` with
  numeric mismatch guard. The coherence guard (`COHERENCE_MIN=0.3`,
  `WITNESS_MIN=0.5`) matches CJK bigrams to avoid false positives on CJK web
  engines while still catching Bing decoy buckets.
- **Health & Rate Limits**: Persisted SQLite circuit breaker (`health_state`),
  `governor` GCRA rate limiter keyed by `host_bucket`, and `moka` single-flight
  dedup.
- **Optional Agent Lane & Answer-in-Search**: When the operator has signed in
  to `codex` (or opted in to `antigravity`), the agent lane dispatches beside
  the keyless pool (`SEARCH_MCP_AGENT_ENGINES=auto` seats signed-in `codex`
  only; `SEARCH_MCP_AGENT_LANE_WAIT_SECONDS=6`,
  `SEARCH_MCP_AGENT_LANE_GRACE_SECONDS=2`; late results merge into the cache in
  the background). When `SEARCH_MCP_AGENT_BACKEND` is configured and
  `SEARCH_MCP_AGENT_IN_SEARCH=true`, the answer agent also runs alongside
  `search` to present a concise sourced answer above the link list.

---

## 5. Scholar Subsystem (`fsm-scholar`, `fsm-scholar-graph`, `fsm-scholar-fulltext`)

- **Direct academic dispatch**: `search({ scope: "academic*" })` routes directly
  to `ScholarApi::search` (keyless default pool: `arxiv`, `crossref`,
  `openaire` v3, `europepmc`; sub-scopes add `dblp`, `openreview`, `pubmed`,
  `clinicaltrials`, `biorxiv`, `zbmath`, `inspire`, `doaj`, `datacite`,
  `zenodo`). `CORE v3` stays out of the keyless search pool; S2 and OpenAlex
  top-N single-ID enrichment is capped at concurrency 4 / 800 ms and benches
  until UTC midnight on 429.
- **Identity & Merge**: Resolves DOI, arXiv (`arXiv:1706.03762` ⇄ `1706.03762`
  ⇄ `10.48550/arXiv.1706.03762`), PMID/PMCID, OpenAlex, S2, DBLP, OpenAIRE
  using 2-stage Union-Find dedup and structured field precedence.
- **11-Step Lawful Full Text (`scholar.fulltext`)**: Structured-first cascade:
  (1) PMC `idconv` → (2) Europe PMC JATS XML → (3) PMC BioC JSON → (4)
  bioRxiv/medRxiv JATS → (5) arXiv HTML5 (`ar5iv` fallback) → (6) PLOS/Springer
  OA JATS → (7) Unpaywall *(skipped when `SEARCH_MCP_CONTACT_EMAIL` is unset)*
  → (8) OpenAlex `$0` / S2 OA PDF → (9) CORE v3 → (10) DOAJ / Crossref TDM /
  DataCite / HAL / Zenodo → (11) Wayback CDX. Never touches shadow libraries.
- **Integrity & Retraction Watch Lazy Sync**: Combines local `retractions`
  table + Crossref `update-to` + OpenAlex `is_retracted`/`fwci` + `scite`
  tallies + DOAJ vetting. On first scholar call (unless
  `SEARCH_MCP_OFFLINE_SYNC=false`), acquires an `fd-lock`, streams Crossref's
  ~70 MB CC0 CSV, writes 6 indexed columns into SQLite (~8–12 MB), deletes the
  raw CSV, and refreshes via `ETag` every 7 days.
- **Offline Citations & Entities**: `hayagriva 0.10` formats BibTeX, CSL-JSON,
  APA, IEEE, Chicago, and Vancouver offline; `author`/`org`/`venue`/`grants`
  aggregate ORCID, ROR, DBLP, ICORE, NIH RePORTER, NSF, CORDIS, and UKRI.

---

## 6. Networking, SSRF, Extraction, Documents & Storage

- **Dual HTTP stack (`fsm-net`)**:
  - Tier 0 (`plain`, ~85% traffic): `reqwest 0.13` + `rustls(ring)` +
    `webpki-roots`, honest UA `free-search-mcp/1.0`, no cookies.
  - Tier 1 (`chrome`/`edge_preflight`): `wreq 0.16.5` + `wreq-util` with Chrome
    TLS/H2 fingerprint and compression, per-egress manual cookie jar solely for
    `bing`/`yahoo`/`baidu`, pre-read `Content-Encoding` and `Content-Length`
    guards, ≤ 5 manual redirects with per-hop SSRF checks.
- **4-Layer SSRF Guard**: (1) `http`/`https` allowlist, (2) authority/IDNA +
  `inet_aton` canonicalization, (3) DNS-free private/loopback/link-local/CGNAT/
  metadata blocks (also run offline on cache hits!), (4) `hickory-resolver` DNS
  pinning with automatic proxy skip and Clash/Surge/sing-box Fake-IP TUN canary
  detection.
- **Extraction (`fsm-extract`)**: Google News batchexecute URL resolver,
  `chardetng` + `encoding_rs` decoding, `lol_html` + `trafilatura` (fallback
  `dom_smoothie`) + `htmd`, plus `dom_query` structured metadata extraction.
- **Documents & Isolated PDF Worker (`fsm-docs`)**: PDFs run via `pdf_oxide`
  inside `current_exe() __pdf-worker` with piped stdio, cleared env (`PATH`,
  `TMPDIR`, `LANG` only), semaphore cap (2 workers, > 5 s queue wait returns
  `BudgetExceeded`), 15 s timeout, and 200 ms RSS monitor (kills on > 512 MiB
  or > 256 MiB/s growth; falls back to `pdftotext` if installed). Office/EPUB/
  CSV/archives run with zip-bomb caps (≤ 2,000 members, expanded bytes ≤ cap).
- **Storage (`fsm-store`)**: Bundled SQLite (`rusqlite 0.43`, WAL mode, zstd
  compression) at `~/.cache/search-mcp/search-mcp.sqlite` with FTS5 `pages_fts`,
  `search_cache`, `api_cache`, `scholar_works`, `retractions`, `sync_state`,
  `exec_kv` (64 MiB LRU), `health_state`, and `downloads`. Never opens or
  modifies legacy v0.18.x `cache.sqlite`.
