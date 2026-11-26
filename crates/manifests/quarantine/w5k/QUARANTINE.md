# Quarantined Sources — Wave `w5k`

- **`usgs_sciencebase`** (`usgs_sciencebase.toml`, moved from `sources/w5k/usgs_sciencebase.toml`):
  - **Date**: 2026-11-25
  - **Reason**: Upstream `https://www.sciencebase.gov/catalog/items` and `/catalog/item/{id}` consistently return a Cloudflare JavaScript anti-bot challenge (`<title>Just a moment...</title>`) on both the initial full probe and the polite re-probe across both ops (`search` and `item`). Automated clients must not bypass anti-bot verification challenges.
  - **Re-entry**: Re-test `https://www.sciencebase.gov/catalog/items?q=earthquake&max=3&format=json` with generic `User-Agent: free-search-mcp/1.0`; if USGS ScienceBase restores keyless open-access API access without Cloudflare challenge gating, move `usgs_sciencebase.toml` back to `sources/w5k/` and re-record fixtures.
