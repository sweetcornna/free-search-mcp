# Releasing `free-search-mcp` 1.0

A release of `free-search-mcp` ships one version across **six distribution
surfaces**, and all of them must agree on the exact same version string:

| Surface | Artifact |
|---|---|
| **Cargo workspace** | `Cargo.toml` (`[workspace.package].version`) + `Cargo.lock` |
| **PyPI (`maturin` `bindings = "bin"`)** | Platform binary wheels (`darwin-arm64`, `darwin-x86_64`, `linux-x86_64`, `linux-aarch64`) + sdist for `uvx free-search-mcp` |
| **GitHub Release** | Standalone binaries, wheels, sdist, and binary `.mcpb` bundles |
| **Tri-Client Plugin (`plugins/free-search/`)** | Claude Code (`.claude-plugin/plugin.json`, `.mcp.json`), Codex & Agent Plugins v1 (`plugin.json`, `mcp.json`), and Antigravity (`mcp_config.json`) |
| **Claude Desktop Bundle (`mcpb/`)** | Binary `.mcpb` (`"type": "binary"`) packed per platform with `@anthropic-ai/mcpb` |
| **MCP Registry (`server.json`)** | `io.github.sweetcornna/free-search-mcp` entry published via GitHub OIDC |

## Files That Carry the Version Number

| File | Field(s) |
|---|---|
| `Cargo.toml` | `[workspace.package].version` |
| `pyproject.toml` | `[project].version` |
| `plugins/free-search/.claude-plugin/plugin.json` | `version` |
| `plugins/free-search/.mcp.json` | `mcpServers.search.args` (`["free-search-mcp==X.Y.Z"]`) |
| `plugins/free-search/plugin.json` | `version` |
| `plugins/free-search/mcp.json` | `mcpServers.search.args` (`["free-search-mcp==X.Y.Z"]`) |
| `plugins/free-search/mcp_config.json` | `mcpServers.search.args` (`["free-search-mcp==X.Y.Z"]`) |
| `server.json` | `version` and `packages[0].version` |
| `mcpb/manifest.json` | `version` |
| `CHANGELOG.md` | `## [X.Y.Z] - YYYY-MM-DD` heading |

Two automated gates enforce lockstep sync:

1. `cargo test -p fsm-server --test plugin_manifests --locked --offline` runs in
   CI on every PR and verifies version sync, tri-client fields, server key
   `"search"`, zero legacy tool references, and MCP Registry ownership marker.
2. `.github/workflows/release.yml` re-checks every file against the pushed
   annotated tag `vX.Y.Z` before building or publishing.

## Pre-Release Checklist

1. Bump the version across all files in the table above and update `Cargo.lock`
   (`cargo metadata --offline`).
2. Add a `## [X.Y.Z] - YYYY-MM-DD` section to `CHANGELOG.md` (its body becomes
   the GitHub Release notes).
3. Run the full offline verification suite:
   ```bash
   cargo test --workspace --locked --offline
   cargo clippy --workspace --all-targets --locked --offline -- -D warnings
   cargo deny check --disable-fetch bans licenses sources
   cargo test -p fsm-server --test plugin_manifests --locked --offline
   ```
4. Validate tri-client plugin manifests locally when CLIs are installed:
   ```bash
   claude plugin validate . --strict
   claude plugin validate ./plugins/free-search --strict
   agy plugin validate ./plugins/free-search --strict
   ```
5. Smoke-build a local wheel with pinned `maturin==1.16.0` (no publish):
   ```bash
   maturin build --release --locked --offline --out target/wheels
   uvx --from target/wheels/free_search_mcp-*.whl free-search-mcp --help
   ```
6. Run the live stdio smoke check against the built binary:
   ```bash
   uv run --with mcp python scripts/smoke_mcp.py --bin target/release/free-search-mcp
   ```
7. Only after explicit user/maintainer approval: commit to `main`, push, and
   push an annotated tag `git tag -a vX.Y.Z -m "short title" && git push origin vX.Y.Z`.

## MCP Registry Ownership Marker

PyPI builds the package description from `README.md` and freezes it per release.
Line 2 of `README.md` MUST keep:

```html
<!-- mcp-name: io.github.sweetcornna/free-search-mcp -->
```

Both `crates/server/tests/plugin_manifests.rs` and `release.yml` fail if this
marker is missing.

## Notes

- The PyPI distribution name is `free-search-mcp` (not `search-mcp`).
- Local binary installs provide both `free-search-mcp` and a `search-mcp`
  symlink alias.
- Tool versions are pinned in `release.yml`: `maturin==1.16.0`,
  `@anthropic-ai/mcpb@2.1.2`, and `mcp-publisher v1.8.1`.
