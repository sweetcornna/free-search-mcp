# Proxy & Gates

Why some search engines return zero results, and how to fix it.

## 1. Why some engines return 0 results

Most "missing" results are not bugs in search-mcp — they are the upstream
provider refusing to serve results to our request. This is called **gating**.

Common cases:

- **Google / Bing** serve a **CAPTCHA** ("unusual traffic", reCAPTCHA, "are
  you a robot") instead of a results page when the request comes from a
  datacenter / cloud IP address. Residential IPs are usually fine; the IPs of
  VPS / CI / cloud hosts are widely flagged.
- **Google / YouTube / Bing** sometimes serve a **consent** wall ("before you
  continue", `consent.google.com`) in certain regions.
- **Zhihu** and other sites serve a **login** wall instead of search results.

search-mcp does **not** try to solve or bypass CAPTCHAs. Defeating a
provider's anti-bot challenge would violate their Terms of Service, so a gated
engine simply yields no results — and tells you *why* (see Gate Diagnostics
below). The supported, ToS-friendly fixes are a **proxy** and the **SearXNG
fallback**, described next.

## 2. Proxy — the real fix for IP gating

The most reliable way to stop datacenter-IP gating is to route outbound
requests through a proxy on a non-flagged (e.g. residential) IP.

The proxy is fully **opt-in**: with nothing configured, behaviour is exactly as
before — no request is proxied. Once set, the proxy applies consistently to the
**HTTP engines**, the **headless browser**, and the **page fetcher**, so every
outbound path uses the same exit IP.

### Setting the proxy

Two equivalent ways (the environment variable wins when both are present):

- **Admin UI** — open the bilingual **"Network / Proxy / 网络 / 代理"** card and
  fill in **Proxy URL / 代理 URL**.
  This saves to the config file and hot-reloads on a running server; no restart
  needed. The value is treated as a secret (masked in the UI, never logged).
- **Environment variable** — set `SEARCH_MCP_PROXY`.

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

By default a configured proxy applies to **all** engines plus the browser and
the fetcher. Proxies are often slower than a direct connection, so you can keep
the fast default engines direct and route **only** the engines that actually
get gated through the proxy.

- **Admin UI** — fill in **"Proxy only these engines / 仅代理这些引擎"** on the
  Network / Proxy / 网络 / 代理 card.
- **Environment variable** — set `SEARCH_MCP_PROXY_ENGINES`.

The value is a list of engine names separated by spaces or commas, for example:

```
google bing zhihu
```

Notes on scoping:

- A blank scope means **all engines** are proxied.
- Engine names are matched case-insensitively.
- **The browser is always proxied with the unscoped proxy** whenever a proxy is
  set. Browser traffic is global and not split per engine, so an engine scope
  affects the HTTP engines but the browser still uses the proxy as soon as one
  is configured.

## 3. SearXNG fallback

Some SERP engines have a built-in safety net. When **google**, **serpsearch**,
or **bing** are gated, search-mcp automatically falls back to the working
**searx** meta-search to recover results for that query.

These recovered results are attributed to **"searx"** in the provenance /
engine field — so if you asked for Google but see results credited to `searx`,
that engine was gated and the fallback supplied the answers instead.

## 4. Gate diagnostics

When an engine returns nothing because it was gated, the response reports it
rather than silently dropping the engine. The diagnostics include a **`gated`**
map from engine name to the reason it was blocked:

- **`captcha`** — a CAPTCHA / anti-bot interstitial was served (e.g. Google
  "unusual traffic", reCAPTCHA/hCaptcha). The usual fix is a proxy.
- **`consent`** — a cookie/consent wall was served (e.g. `consent.google.com`,
  "before you continue").
- **`login`** — a login wall was served instead of results (e.g. Zhihu's
  "请登录" / "登录知乎"). The fix is to log in once (see below).

This turns a confusing empty result set into an actionable explanation: a
`captcha` reason points you at the proxy, a `login` reason points you at the
login flow.

## 5. Zhihu login

Zhihu requires an authenticated session to return search results — it serves a
login wall to anonymous requests (surfaced as a `login` gate). To unblock it,
log in once so the session cookies are saved and reused:

- Run the CLI command:

  ```
  search-mcp-login zhihu
  ```

  (or click the **"Login"** button in the admin UI).
- A browser window opens. Log in to Zhihu normally.
- The session cookies are persisted, after which Zhihu search works.

This is a one-time step (until the cookies expire). It opens a real browser
window, so it **requires a desktop session** — it cannot be completed on a
headless server. On a headless host, run the login on a machine with a display
and carry the saved cookies over, or route Zhihu through a proxy that reaches a
working session.

## 6. Startpage transient errors

Startpage is browser-driven and occasionally hits a transient navigation error
during page load. These are not gates and usually succeed on a second attempt,
so search-mcp **retries the navigation once** before giving up. No
configuration is required; this happens automatically.
