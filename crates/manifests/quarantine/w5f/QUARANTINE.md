# Quarantined Sources — Wave `w5f`

- **`data_qld_au`** (`data_qld_au.toml`, protocol `ckan`, extracted from `instances/w5f/ckan.toml`):
  - **Date**: 2026-11-25
  - **Reason**: Upstream `https://www.data.qld.gov.au/api/3/action/package_search` consistently returns `HTTP 405` with an AWS WAF `"Human Verification"` anti-bot CAPTCHA interstitial on both the initial full probe and the polite re-probe. Automated clients must not bypass CAPTCHA/anti-bot verification challenges.
  - **Re-entry**: Re-test `https://www.data.qld.gov.au/api/3/action/package_search?q=water&rows=2` with generic `User-Agent: free-search-mcp/1.0`; if Queensland Open Data restores keyless open-access API access without anti-bot gating, merge the `[[instance]]` entry back into `instances/w5f/ckan.toml`.
