# Proxy & Gates

Why some search engines return zero results, and how to fix it.

## 1. Why some engines return 0 results

When an engine returns nothing, the usual cause is the upstream provider
refusing to serve results to our request. This is called **gating**, and it is
not a bug in search-mcp.

Common cases:

- Google, Bing and Mojeek serve a CAPTCHA ("unusual traffic", reCAPTCHA, "are
  you a robot") or a JavaScript-only shell instead of a results page, most
  reliably when the request comes from a datacenter or cloud IP address. The
  IPs of VPS, CI and cloud hosts are widely flagged.
- Google, YouTube and Bing sometimes serve a consent wall ("before you
  continue", `consent.google.com`) in certain regions.
- Zhihu and other sites serve a login wall instead of search results.

search-mcp does not try to solve or bypass CAPTCHAs, because defeating a
provider's anti-bot challenge would violate its Terms of Service. A gated
engine yields no results, and the response says why (§5). The two supported
answers are a proxy (§2) and the automatic stand-ins (§3). An API key is not
one of them: nothing the server selects on its own uses a key, and an agent
should never ask a user for one.

### Non-gate reasons an engine can come back empty

Before reaching for a proxy, check whether one of these applies:

- The engine was skipped by its rate limit. Some sources publish a stricter
  limit than our default (GDELT asks for roughly one request every few
  seconds). When such an engine's token bucket is empty, the search skips it
  instead of waiting, because search runs as a parallel fan-out and waiting
  would add that delay to every other engine's results. The skip is reported
  in the run's diagnostics under `rate_limited`. Search again in a moment and
  the engine participates.
- The engine was benched. An engine that failed recently is not asked again
  for 10 to 60 minutes (§3). It appears under `benched_engines`, not
  `gated_engines`.
- Its answer was off-topic and discarded (§4). That is not a block, and a proxy
  does not change it.
- An opt-in engine was named without its key. `brave_api`, `serper`, `tavily`,
  `google_cse` and `github_code` run only on the operator's own key. Named
  without one, they raise an error, because an empty list would be
  indistinguishable from "nothing matched". The error says the search itself
  is fine, names the keyless alternative (`github` covers repositories and
  issues without a token), and tells an agent not to ask the user for a key.
  These engines are never selected automatically.
- The engine does not cover the query. Vertical sources index only their own
  domain: asking `arxiv` about a restaurant returns nothing, which is correct
  behaviour. Use `category=` and let the aggregator pick the sources instead
  of naming engines by hand.

### The inverse gate: APIs that reject browsers

Most scrapers need to look like Chrome to get past a bot wall. A few JSON APIs
do the opposite and reject clients that present a browser TLS/header
fingerprint. Zenodo answers `403` to one. Those engines opt out of
impersonation and send an honest, contactable `User-Agent`. If you add an
engine for a JSON API and get a `403` that a plain `curl` does not get, this is
the likely cause.

## 2. Proxy: the real fix for IP gating

The most reliable way to stop datacenter-IP gating is to route outbound
requests through a proxy on a non-flagged (e.g. residential) IP.

The proxy is opt-in. With nothing configured, no request is proxied. Once set,
it applies to the HTTP engines, the headless browser and the page fetcher, so
every outbound path uses the same exit IP.

### Setting the proxy

There are two equivalent ways, and the environment variable wins when both are
present:

- On the local settings page (`search-mcp-admin`), the bilingual
  **Network / Proxy / 网络 / 代理** card is the first one. Fill in
  **Proxy URL / 代理 URL**. The value is saved to the config file, and a running
  server reloads it without a restart. It is treated as a secret: masked in the
  UI and never logged.
- As an environment variable, set `SEARCH_MCP_PROXY`.

### Proxy URL format

```
http://host:port
https://host:port
socks5://host:port
http://user:pass@host:port
socks5://user:pass@host:port
```

Supported schemes are `http://`, `https://`, and `socks5://`. Credentials are
optional and embedded as `user:pass@` before the host. Because the URL may
carry credentials, it is handled as a secret and is never written to logs.

### Scoping the proxy to specific engines

By default a configured proxy applies to all engines plus the browser and the
fetcher. Proxies are often slower than a direct connection, so you can keep the
fast default engines direct and route only the engines that get gated through
the proxy.

- On the local settings page, fill in
  **Proxy only these engines / 仅代理这些引擎** on the
  Network / Proxy / 网络 / 代理 card.
- As an environment variable, set `SEARCH_MCP_PROXY_ENGINES`.

The value is a list of engine names separated by spaces or commas, for example:

```
google bing zhihu
```

Notes on scoping:

- A blank scope means all engines are proxied.
- Engine names are matched case-insensitively.
- **The browser is always proxied with the unscoped proxy** whenever a proxy is
  set. Browser traffic is global and is not split per engine, so an engine
  scope affects the HTTP engines, while the browser uses the proxy as soon as
  one is configured.

## 3. Stand-ins: the health-aware pool and the rescue pass

The server keeps a small record of which engines are working, so a gated engine
does not cost every search its full timeout.

### The circuit breaker

The breaker *benches* an engine, meaning the server stops asking it, after any
of these:

- one wall (`captcha`, `consent`, `login`, a JavaScript shell) or one
  off-topic answer (§4);
- 2 errors in a row;
- 3 silent zero-result answers in a row.

The bench lasts 10 minutes and doubles on each repeat, up to 60. One success
clears it. Naming an engine in `engines=[...]` always runs it, benched or not,
because the breaker governs only the pool the server picks for itself. Tune or
disable it with `SEARCH_MCP_HEALTH_ENABLED`,
`SEARCH_MCP_ENGINE_COOLDOWN_SECONDS` and
`SEARCH_MCP_ENGINE_COOLDOWN_MAX_SECONDS`.

An open bench is also written to `<cache_dir>/engine_health.json`, so a new
process starts with it. Measured on 2026-09-21, Mojeek took 2.6 to 6.2 s to
serve its captcha page, and a search waits for its slowest engine, so a fresh
process paid that on its first search: 8.5 s against 4.0 s for the next
process, which read the file. A long-lived server paid it once a session; the
one-shot `search-mcp ask` command and the child servers the answer agent starts
paid it every run. The file is advisory. Delete it, or lose it, and the cost is
one failed attempt, as before.

### Reserves

While fewer than `SEARCH_MCP_MIN_HEALTHY_ENGINES` (3) general engines are
healthy, reserves are seated in order: `so360`, `brave`, `searx`
(`SEARCH_MCP_RESERVE_ENGINES`). A reserve that needs Chromium is skipped while
Chromium is known to be missing. An answer that still rests on too few indexes
is served, but it is cached for at most an hour instead of the usual week, so
the pool's recovery shows up.

### The rescue pass

If a search still comes back empty, or nearly empty while engines were gated or
erroring, one bounded recovery attempt runs through the reserves, then `searx`,
then `bing`, skipping whatever already ran or is benched. Recovered results are
attributed to the engine that produced them. The response reports this in
`rescued_via` and in a `fallback` entry on each gated engine
(`was captcha-gated → served via so360`).

Measured on 2026-09-21: `mojeek` is captcha-walled on every request, so in
practice it is benched after one attempt and re-probed every 10 to 60 minutes.
`google` and `serpsearch` hit a JavaScript wall, and `startpage` and `sogou`
returned nothing. None of them is a recovery path, so do not name
`engines=["google"]` because results were thin.

## 4. Off-topic answers: not a gate

Bing has a failure that looks like success: HTTP 200, ten well-formed results,
all about the first word of the query. "rust ownership borrowing" returned the
Steam page for the game Rust; "postgres explain analyze" returned the
postgresql.org home page. Rank fusion then interleaved those into real answers.

The first fix is in the request. Bing is asked with a warmed cookie jar and the
request shape a browser uses, which is what makes it answer on topic. If a
decoy page still comes back, the engine re-mints its session and retries once.

The second is a guard in the aggregator. For each *web* engine's bucket it
measures coherence: the share of results that mention any query term beyond the
first. A bucket is dropped whole when its coherence is below 0.3 and another
engine's bucket (coherence ≥ 0.5) shows that the query's words do get echoed.
Without such a witness nothing is dropped, because for some queries no engine
echoes the words, and that is no evidence against any of them. A single
unconfirmed suspect triggers the rescue pass as a second opinion.

Specialist engines (anything routed by `category=`) and single-site engines
(`wikipedia`, `bilibili`, `zhihu`, `openlibrary`) are exempt: SEC EDGAR answers
"NVDA risk factors" with filing titles that contain neither word, and every one
of them is right.

A dropped bucket is reported as `gated_engines[<engine>].reason == "off_topic"`
and the engine is benched. **An off-topic answer is not a block, so a proxy
does not help**: the network is fine, and the engine answered a different
question. Turn the guard off with `SEARCH_MCP_COHERENCE_GUARD_ENABLED=false`.

## 5. Diagnostics in the response

When an engine returns nothing because it was gated, the response reports it.
`gated_engines` maps each engine to `{"reason", "fallback"}`, and `gated_hint`
says the same in one sentence. The reasons are:

- `captcha`: a CAPTCHA or anti-bot interstitial was served (e.g. Google
  "unusual traffic", reCAPTCHA/hCaptcha). The usual fix is a proxy.
- `consent`: a cookie or consent wall was served (e.g. `consent.google.com`,
  "before you continue").
- `javascript`: a page that only renders with JavaScript was served in place of
  results.
- `login`: a login wall was served instead of results (e.g. Zhihu's "请登录" /
  "登录知乎"). The fix is to log in once (see below).
- `off_topic`: the engine answered, but about the wrong thing (§4). No proxy
  advice is given, because none applies.
- `browser_unavailable`: the engine needs a browser render and Chromium is not
  installed. The hint carries the install command.
- `no_live_instance`: `searx` only. None of the public SearXNG instances
  answered. Pin a good one with `SEARCH_MCP_SEARX_INSTANCES`.

`benched_engines` lists the engines that were *not asked* this time, each with
its reason, `retry_in_seconds` and the reserve standing in for it.
`benched_hint` (Markdown: "Benched engines") says the same in a sentence.

Each reason tells you what to do next: `captcha` points to the proxy, `login`
points to the login flow, and `off_topic` means the network is fine and should
be left alone.

## 6. Zhihu login

Zhihu requires an authenticated session to return search results. Anonymous
requests get a login wall, which is reported as a `login` gate. To unblock it,
log in once so the session cookies are saved and reused:

- Run the CLI command:

  ```
  search-mcp-login zhihu
  ```

  (or click the **Login / 登录** button on the local settings page).
- A browser window opens. Log in to Zhihu normally.
- The session cookies are persisted, after which Zhihu search works.

This is a one-time step, until the cookies expire. It opens a real browser
window, so it requires a desktop session and cannot be completed on a headless
server. On a headless host, run the login on a machine with a display and carry
the saved cookies over, or route Zhihu through a proxy that reaches a working
session.

## 7. Startpage transient errors

Startpage is browser-driven and occasionally hits a transient navigation error
during page load. These errors are not gates and usually succeed on a second
attempt, so search-mcp retries the navigation once before giving up. This needs
no configuration.
