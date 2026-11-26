# Proxy, Gates & Network Diagnostics

Why some search engines return zero results from cloud or restricted networks,
how `free-search-mcp` 1.0 handles blocks automatically, and how to configure a
proxy or diagnose Fake-IP VPNs.

## 1. Why an Engine Returns 0 Results

When a web engine returns nothing, the usual cause is upstream **gating**
(CAPTCHA, consent wall, region block, or rate limit), not a bug in
`free-search-mcp`:

- Cloud, VPS, and datacenter IPs frequently receive CAPTCHAs or JS shells from
  search providers.
- `yahoojp` (Yahoo! JAPAN) does not serve requests from EEA or UK IP addresses
  (returns a region notice and is benched automatically).
- Strict-rate-limit APIs (e.g., GDELT, MusicBrainz, Nominatim) skip a search
  when their GCRA token bucket is empty rather than stalling parallel fan-out.

`free-search-mcp` never attempts to solve or bypass CAPTCHAs. Instead, it
combines automatic circuit breaking, hedging reserves, honest API transport,
and optional proxy routing.

> **Inverse gate (APIs that reject browsers)**: Many JSON APIs (e.g. Zenodo,
> academic/government APIs) reject browser TLS fingerprints with `403`. In 1.0,
> ~85% of traffic uses the Tier-0 `plain` transport (`reqwest` + `rustls(ring)`
> with honest UA `free-search-mcp/1.0`), reserving Chrome TLS impersonation
> (`wreq`) strictly for HTML SERP scrapers that require it.

## 2. Outbound Proxy (`SEARCH_MCP_PROXY` & `SEARCH_MCP_NO_PROXY`)

Routing through a residential or non-blocked exit IP resolves datacenter IP
gating. Supported schemes: `http://`, `https://`, `socks5://`, `socks5h://`
(with optional `user:pass@`).

Set proxy settings in the process environment, `~/.config/search-mcp/.env`, or
`~/.config/search-mcp/config.json` (`0600`) — **never** in a working-directory
`./.env`, where security-sensitive `[SEC]` variables are ignored:

```dotenv
SEARCH_MCP_PROXY=socks5h://user:pass@proxy.example:1080
# Optional: route only named sources through the proxy (blank = all sources)
SEARCH_MCP_PROXY_ENGINES=bing yahoojp
# Optional: comma/space-separated host suffixes that bypass the proxy
SEARCH_MCP_NO_PROXY=localhost,.internal,.example.edu
```

Proxy credentials are redacted from `Debug` logs and `free-search-mcp config`.

## 3. Fake-IP VPNs (Clash / Surge / sing-box TUN) & SSRF Guard

The 4-layer SSRF guard validates every URL before connecting (and runs layers
1–3 offline even on cache hits):

1. Scheme allowlist (`http://`, `https://` only)
2. Authority / IDNA / `inet_aton` IPv4 canonicalization
3. DNS-free blocks (loopback, RFC1918, link-local, CGNAT `100.64.0.0/10`,
   IPv4-mapped IPv6, `.local`/`.internal`/`localhost`/cloud metadata)
4. Resolve-and-check (`hickory-resolver`) with per-hop DNS pinning

Under `SEARCH_MCP_SSRF_RESOLVE_ADDRESSES=auto` (default):
- Layer 4 is skipped automatically when an outbound `SEARCH_MCP_PROXY` handles
  resolution.
- On Fake-IP VPNs (Clash/Surge/sing-box TUN mapping domains into
  `198.18.0.0/15`, `28.0.0.0/8`, `fc00::/7`, `2001:2::/48`), canary probes
  (`example.com`, `a.root-servers.net`) detect the TUN automatically and excuse
  only those tunnel ranges — loopback, RFC1918, and link-local remain blocked.

Test your local network and Fake-IP TUN detection directly:

```bash
free-search-mcp doctor --probe duckduckgo --fetch https://example.com
```

Leave `SEARCH_MCP_ALLOW_PRIVATE_HOSTS=false`; turning it on disables private
and metadata IP protection.

## 4. Circuit Breaker, Hedging Reserves & Off-Topic Guard

- **Persisted breaker (`health_state` in `search-mcp.sqlite`)**: Benches a
  walled or off-topic engine immediately, an erroring engine after 2 failures,
  and a silent zero-result engine after 3. Cooldown starts at 10 minutes and
  doubles up to 60 minutes; one success clears it. Explicit `sources: [...]`
  still run benched engines.
- **Hedging reserves**: Default pool is `["duckduckgo", "bing", "yahoojp",
  "seznam"]`. If `< 2` engines answer by `1.8 s` or primaries fail/wall, the
  kernel dispatches reserves (`["yahoo", "youcom", "mwmbl"]`) at `1.8 s` and
  `+800 ms` (`max_hedges = 2`).
- **CJK-aware off-topic coherence guard**: Drops decoy SERP buckets (such as
  Bing returning HTTP 200 results matching only the first word) when coherence
  drops below `0.3` and another engine proves query terms are echoed (`≥ 0.5`).
  Off-topic results are not IP blocks, so a proxy does not change them.

## 5. Diagnostics in `notes:`

Search responses include compact `notes:` diagnostics for benched engines,
rate-limited skips, off-topic drops, undated recency results, and missing
optional credentials. Run `free-search-mcp doctor` or `free-search-mcp probe`
to verify live engine status from the terminal.
